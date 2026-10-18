//! `traits` gold: the questions of the trait-implementation set. Three
//! templates, told apart by the item's `set` field:
//!
//! * `Q1` "Which types implement `path::Trait`?": the gold is a set of
//!   implementations. An answer credits an implementation when it falls on the
//!   impl block's span or on the implementing type's declaration span (either
//!   credits). Budget k = number of gold implementations; `set_precision`,
//!   `set_recall`, `set_f1`.
//! * `Q2` "Where is `Trait::m` implemented for `Type`?": one location. The
//!   first answer on the member's span (a macro impl: the call site) is the
//!   hit; `hit@1`, `hit@10`, `mrr` (rank cut at 10).
//! * `Q3` "What must change if `Trait::m` changes?": the gold is every
//!   implementing member by location. Same set metrics and budget as Q1.
//!
//! Rules shared by the templates:
//!
//! * An answer is a located node (`path` + `line`; the line must lie inside a
//!   span). An answer with no path or no line is returned but matches nothing.
//!   Answers are distinct by path, line, `symbol` and `name`, so several nodes
//!   at one macro call site stay several answers.
//! * One answer credits at most one gold implementation and one implementation
//!   is credited once (a maximum matching), so a macro call site that stands for
//!   N implementations needs N distinct answers to find all of them.
//! * Neither hit nor miss, and not counted against the budget: an answer on the
//!   trait's own declaration (`trait_decl`, which also covers the declarations
//!   of its methods) or on a `neutral` span; an answer on a `cfg_gated` span
//!   (an implementation the gold's build did not compile; counted apart as
//!   `cfg_gated_answers`). A span that is also gold counts as gold.
//! * Per-kind view of the same matching: `recall_explicit`, `recall_blanket`,
//!   `recall_macro`, `recall_derive` (defined where the item has gold of that
//!   kind). Restricted views, each with its own k and with answers on the
//!   removed implementations dropped as neutral: `explicit_*` (explicit gold
//!   only) and `nodom_*` (without the gold at the item's `dominant_site`, the
//!   macro call site with the most Q1 gold in the set; see [`annotate`]).

use super::metrics::{f1, hit_at_k, precision, recall, reciprocal_rank};
use super::{Answer, Converted, Dropped, ItemScore, arr_of, dedup_keep_order, str_of};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

/// Rank cut for `hit@10` and `mrr` (Q2).
const Q2_CUT: usize = 10;

const KINDS: [(&str, &str); 4] = [
    ("explicit", "recall_explicit"),
    ("blanket", "recall_blanket"),
    ("macro", "recall_macro"),
    ("derive", "recall_derive"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Template {
    Q1,
    Q2,
    Q3,
}

impl Template {
    fn of(item: &Value) -> Template {
        match str_of(item, "set") {
            Some("Q2") => Template::Q2,
            Some("Q3") => Template::Q3,
            _ => Template::Q1,
        }
    }
}

/// A file and an inclusive line range.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Span {
    file: String,
    begin: i64,
    end: i64,
}

impl Span {
    fn from_value(v: &Value) -> Option<Span> {
        let begin = v.get("begin_line")?.as_i64()?;
        Some(Span {
            file: norm(str_of(v, "file")?),
            begin,
            end: v.get("end_line").and_then(Value::as_i64).unwrap_or(begin),
        })
    }

    fn holds(&self, path: &str, line: i64) -> bool {
        self.file == path && self.begin <= line && line <= self.end
    }

    fn to_json(&self) -> Value {
        json!({"file": self.file, "begin_line": self.begin, "end_line": self.end})
    }
}

fn norm(path: &str) -> String {
    let p = path.replace('\\', "/");
    p.strip_prefix("./").map(str::to_string).unwrap_or(p)
}

fn spans_of(item: &Value, key: &str) -> Vec<Span> {
    arr_of(item, key)
        .iter()
        .filter_map(Span::from_value)
        .collect()
}

/// One gold implementation (Q1, Q3) or the one gold location (Q2).
struct Entry {
    kind: String,
    /// The spans an answer may fall on to credit this entry.
    credit: Vec<Span>,
    /// The impl span (macro: the call site); the key of the dominant site.
    site: Option<Span>,
}

fn entries_of(item: &Value, template: Template) -> Vec<Entry> {
    arr_of(item, "gold")
        .iter()
        .map(|g| {
            let site = g.get("impl").and_then(Span::from_value);
            let credit: Vec<Span> = match template {
                Template::Q1 => [site.clone(), g.get("type_decl").and_then(Span::from_value)]
                    .into_iter()
                    .flatten()
                    .collect(),
                Template::Q2 | Template::Q3 => g
                    .get("location")
                    .and_then(Span::from_value)
                    .or_else(|| site.clone())
                    .into_iter()
                    .collect(),
            };
            Entry {
                kind: str_of(g, "stratum").unwrap_or("explicit").to_string(),
                credit,
                site,
            }
        })
        .collect()
}

/// One distinct answer, classified against the item.
struct Ans {
    /// Gold entries (indices) whose credit spans the answer falls on.
    hits: Vec<usize>,
    ignored: bool,
    cfg: bool,
}

struct Context {
    entries: Vec<Entry>,
    answers: Vec<Ans>,
    dominant: Option<Span>,
}

fn context(item: &Value, template: Template, answers: &[Answer]) -> Context {
    let entries = entries_of(item, template);
    let mut ignore = spans_of(item, "neutral");
    ignore.extend(item.get("trait_decl").and_then(Span::from_value));
    let cfg = spans_of(item, "cfg_gated");
    let mut seen = BTreeSet::new();
    let mut classified = Vec::new();
    for a in answers {
        let key = (
            a.path.as_deref().map(norm),
            a.line,
            a.symbol.clone(),
            a.name.clone(),
        );
        if !seen.insert(key) {
            continue;
        }
        let place = match (a.path.as_deref(), a.line) {
            (Some(p), Some(l)) => Some((norm(p), l)),
            _ => None,
        };
        let on = |spans: &[Span]| {
            place
                .as_ref()
                .is_some_and(|(p, l)| spans.iter().any(|s| s.holds(p, *l)))
        };
        classified.push(Ans {
            hits: entries
                .iter()
                .enumerate()
                .filter(|(_, e)| on(&e.credit))
                .map(|(i, _)| i)
                .collect(),
            ignored: on(&ignore),
            cfg: on(&cfg),
        });
    }
    Context {
        entries,
        answers: classified,
        dominant: item.get("dominant_site").and_then(Span::from_value),
    }
}

/// The scored answers of one view: `retained` says which gold entries are
/// still gold. Answers on removed entries, ignored spans and cfg-gated spans
/// drop out first; the rest are cut to the budget k = retained entries.
struct Outcome {
    k: usize,
    counted: usize,
    matched: Vec<bool>,
}

impl Outcome {
    fn hits(&self) -> usize {
        self.matched.iter().filter(|m| **m).count()
    }

    fn prf(&self) -> [Option<f64>; 3] {
        if self.k == 0 {
            return [None; 3];
        }
        let (p, r) = (
            precision(self.hits(), self.counted),
            recall(self.hits(), self.k),
        );
        [Some(p), Some(r), Some(f1(p, r))]
    }
}

fn counted_answers<'a>(ctx: &'a Context, retained: &[bool]) -> Vec<&'a Ans> {
    ctx.answers
        .iter()
        .filter(|a| {
            if a.hits.iter().any(|e| retained[*e]) {
                return true;
            }
            a.hits.is_empty() && !a.ignored && !a.cfg
        })
        .collect()
}

fn try_assign(
    a: usize,
    adj: &[Vec<usize>],
    seen: &mut [bool],
    owner: &mut [Option<usize>],
) -> bool {
    for &e in &adj[a] {
        if seen[e] {
            continue;
        }
        seen[e] = true;
        let current = owner[e];
        if current.is_none_or(|b| try_assign(b, adj, seen, owner)) {
            owner[e] = Some(a);
            return true;
        }
    }
    false
}

fn evaluate(ctx: &Context, retained: &[bool]) -> Outcome {
    let k = retained.iter().filter(|r| **r).count();
    let mut counted = counted_answers(ctx, retained);
    counted.truncate(k);
    let adj: Vec<Vec<usize>> = counted
        .iter()
        .map(|a| a.hits.iter().copied().filter(|e| retained[*e]).collect())
        .collect();
    let mut owner = vec![None; retained.len()];
    for a in 0..adj.len() {
        try_assign(a, &adj, &mut vec![false; retained.len()], &mut owner);
    }
    Outcome {
        k,
        counted: counted.len(),
        matched: owner.iter().map(Option::is_some).collect(),
    }
}

fn q2_rank(ctx: &Context) -> Option<usize> {
    let all = vec![true; ctx.entries.len()];
    counted_answers(ctx, &all)
        .iter()
        .position(|a| !a.hits.is_empty())
        .map(|i| i + 1)
}

fn set_metrics(ctx: &Context, metrics: &mut Vec<(&'static str, Option<f64>)>) {
    let all = vec![true; ctx.entries.len()];
    let base = evaluate(ctx, &all);
    let [p, r, f] = base.prf();
    metrics.extend([("set_precision", p), ("set_recall", r), ("set_f1", f)]);
    for (kind, name) in KINDS {
        let of_kind: Vec<usize> = (0..ctx.entries.len())
            .filter(|i| ctx.entries[*i].kind == kind)
            .collect();
        let value = (!of_kind.is_empty()).then(|| {
            let found = of_kind.iter().filter(|i| base.matched[**i]).count();
            recall(found, of_kind.len())
        });
        metrics.push((name, value));
    }
    let explicit: Vec<bool> = ctx.entries.iter().map(|e| e.kind == "explicit").collect();
    let [p, r, f] = evaluate(ctx, &explicit).prf();
    metrics.extend([
        ("explicit_precision", p),
        ("explicit_recall", r),
        ("explicit_f1", f),
    ]);
    let [p, r, f] = match &ctx.dominant {
        Some(site) => {
            let kept: Vec<bool> = ctx
                .entries
                .iter()
                .map(|e| e.site.as_ref() != Some(site))
                .collect();
            evaluate(ctx, &kept).prf()
        }
        None => [None; 3],
    };
    metrics.extend([("nodom_precision", p), ("nodom_recall", r), ("nodom_f1", f)]);
}

pub fn score_item(item: &Value, answers: &[Answer]) -> ItemScore {
    let template = Template::of(item);
    let ctx = context(item, template, answers);
    let mut metrics: Vec<(&'static str, Option<f64>)> = Vec::new();
    let mut dims = vec![(
        "template",
        str_of(item, "set").unwrap_or("unknown").to_string(),
    )];
    if template == Template::Q2 {
        let rank = q2_rank(&ctx);
        metrics.push(("hit@1", Some(hit_at_k(rank, 1))));
        metrics.push(("hit@10", Some(hit_at_k(rank, Q2_CUT))));
        metrics.push(("mrr", Some(reciprocal_rank(rank, Q2_CUT))));
        let kind = str_of(item, "stratum")
            .or_else(|| {
                arr_of(item, "gold")
                    .first()
                    .and_then(|g| str_of(g, "stratum"))
            })
            .unwrap_or("unknown");
        dims.push(("kind", kind.to_string()));
    } else {
        set_metrics(&ctx, &mut metrics);
    }
    let cfg = ctx
        .answers
        .iter()
        .filter(|a| a.hits.is_empty() && a.cfg)
        .count();
    metrics.push(("cfg_gated_answers", Some(cfg as f64)));
    ItemScore { metrics, dims }
}

// ------------------------------------------------------- set-level helpers

/// Gives every Q1 item the set's `dominant_site`: the macro call site (impl
/// span of a `macro` gold entry) that carries the most Q1 gold entries, at
/// least two; the smallest file and lines win a tie. An item that already has
/// the key keeps it. Call it on the whole gold, before any item is dropped, so
/// the site is the same in every view of the set.
pub fn annotate(mut items: Vec<Value>) -> Vec<Value> {
    let mut count: BTreeMap<Span, usize> = BTreeMap::new();
    for item in items.iter().filter(|i| Template::of(i) == Template::Q1) {
        for entry in entries_of(item, Template::Q1) {
            if entry.kind == "macro"
                && let Some(site) = entry.site
            {
                *count.entry(site).or_default() += 1;
            }
        }
    }
    let dominant = count
        .iter()
        .filter(|(_, n)| **n >= 2)
        .max_by(|a, b| a.1.cmp(b.1).then_with(|| b.0.cmp(a.0)))
        .map(|(site, _)| site.to_json());
    if let Some(site) = dominant {
        for item in items.iter_mut().filter(|i| Template::of(i) == Template::Q1) {
            if let Some(map) = item.as_object_mut() {
                map.entry("dominant_site").or_insert_with(|| site.clone());
            }
        }
    }
    items
}

/// Report additions: which optional gold fields the items carry, so a score
/// computed without them says so.
pub fn report_notes(items: &[Value]) -> Value {
    let with = |key: &str| {
        items
            .iter()
            .filter(|i| i.get(key).is_some_and(|v| !v.is_null() && v != &json!([])))
            .count()
    };
    let mut notes = Map::new();
    notes.insert("items_with_trait_decl".into(), json!(with("trait_decl")));
    notes.insert("items_with_cfg_gated".into(), json!(with("cfg_gated")));
    notes.insert(
        "dominant_site".into(),
        items
            .iter()
            .find_map(|i| i.get("dominant_site").cloned())
            .unwrap_or(Value::Null),
    );
    Value::Object(notes)
}

/// Each gold entry answered by its own span, with a distinct `symbol` so that
/// entries at one macro call site stay distinct answers.
pub fn oracle(item: &Value) -> Vec<Value> {
    let template = Template::of(item);
    let id = str_of(item, "id").unwrap_or_default();
    entries_of(item, template)
        .iter()
        .enumerate()
        .filter_map(|(i, e)| {
            let span = e.credit.first()?;
            let symbol = arr_of(item, "gold")
                .get(i)
                .and_then(|g| str_of(g, "impl_id"))
                .map_or_else(|| format!("{id}#{i}"), str::to_string);
            Some(json!({"path": span.file, "line": span.begin, "symbol": symbol}))
        })
        .collect()
}

pub fn to_query(item: &Value) -> Result<Converted, Dropped> {
    let template = Template::of(item);
    let expected = dedup_keep_order(
        entries_of(item, template)
            .into_iter()
            .flat_map(|e| e.credit)
            .map(|s| s.file),
    );
    if expected.is_empty() {
        return Err(Dropped("no gold files"));
    }
    Ok(Converted {
        expected,
        preferred: Vec::new(),
        disfavored: Vec::new(),
        tags: vec![format!(
            "template:{}",
            str_of(item, "set").unwrap_or("unknown")
        )],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(s: &ItemScore, name: &str) -> Option<f64> {
        s.metrics.iter().find(|(m, _)| *m == name).unwrap().1
    }

    fn m(s: &ItemScore, name: &str) -> f64 {
        metric(s, name).unwrap()
    }

    fn ans(s: &str) -> Answer {
        Answer::parse_shorthand(s)
    }

    fn at(path: &str, line: i64, symbol: &str) -> Answer {
        Answer {
            path: Some(path.into()),
            line: Some(line),
            end_line: Some(line),
            symbol: Some(symbol.into()),
            ..Default::default()
        }
    }

    fn span(file: &str, a: i64, b: i64) -> Value {
        json!({"file": file, "begin_line": a, "end_line": b})
    }

    /// Gold: an explicit impl (a.rs 10..20, type at t.rs 1..3), a blanket impl
    /// (b.rs 30..32, no type), a macro impl (m.rs 5..5, type at t.rs 40..44).
    fn q1() -> Value {
        json!({
            "id": "t-Q1-0", "set": "Q1", "trait": "x::Tr",
            "trait_decl": span("tr.rs", 100, 140),
            "gold": [
                {"impl_id": "i0", "stratum": "explicit", "impl": span("a.rs", 10, 20),
                 "type_decl": span("t.rs", 1, 3)},
                {"impl_id": "i1", "stratum": "blanket", "impl": span("b.rs", 30, 32), "type_decl": null},
                {"impl_id": "i2", "stratum": "macro", "impl": span("m.rs", 5, 5),
                 "type_decl": span("t.rs", 40, 44)}
            ]
        })
    }

    #[test]
    fn q1_hits_on_the_impl_span_or_the_type_declaration() {
        let item = q1();
        let s = score_item(&item, &[ans("a.rs:12"), ans("t.rs:41"), ans("b.rs:30")]);
        assert_eq!(m(&s, "set_recall"), 1.0);
        assert_eq!(m(&s, "set_precision"), 1.0);
        assert_eq!(m(&s, "set_f1"), 1.0);
        // The type declaration credits; so does the impl; one answer, one entry.
        let s = score_item(&item, &[ans("a.rs:12"), ans("t.rs:2")]);
        assert!((m(&s, "set_recall") - 1.0 / 3.0).abs() < 1e-9);
        assert!((m(&s, "set_precision") - 0.5).abs() < 1e-9);
    }

    #[test]
    fn q1_misses_lower_precision_and_the_budget_cuts_the_tail() {
        let item = q1();
        let s = score_item(&item, &[ans("a.rs:99"), ans("a.rs:11")]);
        assert_eq!(m(&s, "set_recall"), 1.0 / 3.0);
        assert_eq!(m(&s, "set_precision"), 0.5);
        // k = 3: the fourth answer is outside the budget.
        let s = score_item(
            &item,
            &[ans("x.rs:1"), ans("y.rs:1"), ans("z.rs:1"), ans("a.rs:11")],
        );
        assert_eq!(m(&s, "set_recall"), 0.0);
        assert_eq!(m(&s, "set_precision"), 0.0);
        // a path-only answer is returned and matches nothing
        let s = score_item(&item, &[ans("a.rs")]);
        assert_eq!((m(&s, "set_recall"), m(&s, "set_precision")), (0.0, 0.0));
    }

    #[test]
    fn the_trait_declaration_is_neither_hit_nor_miss() {
        let item = q1();
        let with_decl = score_item(&item, &[ans("tr.rs:100"), ans("a.rs:12"), ans("tr.rs:120")]);
        let without = score_item(&item, &[ans("a.rs:12")]);
        for name in ["set_precision", "set_recall", "set_f1"] {
            assert_eq!(m(&with_decl, name), m(&without, name), "{name}");
        }
        assert_eq!(m(&with_decl, "set_precision"), 1.0);
        // it does not use up the budget either
        let s = score_item(
            &item,
            &[
                ans("tr.rs:100"),
                ans("a.rs:12"),
                ans("b.rs:31"),
                ans("m.rs:5"),
            ],
        );
        assert_eq!(m(&s, "set_recall"), 1.0);
    }

    #[test]
    fn a_macro_call_site_needs_one_answer_per_implementation() {
        let item = json!({
            "id": "t-Q1-1", "set": "Q1", "trait": "x::Tr",
            "gold": [
                {"impl_id": "i0", "stratum": "macro", "impl": span("m.rs", 8, 9), "type_decl": span("t.rs", 1, 2)},
                {"impl_id": "i1", "stratum": "macro", "impl": span("m.rs", 8, 9), "type_decl": span("t.rs", 5, 6)},
                {"impl_id": "i2", "stratum": "explicit", "impl": span("e.rs", 1, 4), "type_decl": null}
            ]
        });
        // two nodes at the call site, told apart by symbol, find both
        let both = score_item(&item, &[at("m.rs", 8, "n1"), at("m.rs", 8, "n2")]);
        assert!((m(&both, "set_recall") - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(m(&both, "set_precision"), 1.0);
        assert_eq!(m(&both, "recall_macro"), 1.0);
        assert_eq!(metric(&both, "recall_explicit"), Some(0.0));
        assert_eq!(metric(&both, "recall_derive"), None);
        // the same node twice is one answer
        let twice = score_item(&item, &[at("m.rs", 8, "n1"), at("m.rs", 8, "n1")]);
        assert!((m(&twice, "set_recall") - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(m(&twice, "set_precision"), 1.0);
        // the call site answer and the type declaration answer may share an entry:
        // matching is maximum, so 1 call-site answer + 1 type answer find both macro impls
        let mixed = score_item(&item, &[at("m.rs", 8, "n1"), ans("t.rs:1")]);
        assert_eq!(m(&mixed, "recall_macro"), 1.0);
    }

    #[test]
    fn per_kind_recall_and_the_explicit_view() {
        let item = q1();
        let s = score_item(&item, &[ans("a.rs:12"), ans("m.rs:5"), ans("nowhere.rs:1")]);
        assert_eq!(metric(&s, "recall_explicit"), Some(1.0));
        assert_eq!(metric(&s, "recall_blanket"), Some(0.0));
        assert_eq!(metric(&s, "recall_macro"), Some(1.0));
        // explicit view: gold = the explicit impl only, so k = 1; the macro hit is
        // dropped as neutral and the budget then cuts the stray answer
        assert_eq!(m(&s, "explicit_recall"), 1.0);
        assert_eq!(m(&s, "explicit_precision"), 1.0);
        let s = score_item(&item, &[ans("m.rs:5"), ans("nowhere.rs:1")]);
        assert_eq!(m(&s, "explicit_recall"), 0.0);
        assert_eq!(m(&s, "explicit_precision"), 0.0);
        assert_eq!(m(&s, "recall_macro"), 1.0);
    }

    #[test]
    fn the_view_without_the_dominant_site_drops_its_gold_and_answers() {
        let mut item = q1();
        item["dominant_site"] = span("m.rs", 5, 5);
        let s = score_item(&item, &[ans("a.rs:12"), ans("b.rs:30"), ans("m.rs:5")]);
        assert_eq!(m(&s, "set_recall"), 1.0);
        assert_eq!(m(&s, "nodom_recall"), 1.0);
        assert_eq!(m(&s, "nodom_precision"), 1.0);
        let s = score_item(&item, &[ans("m.rs:5")]);
        assert!((m(&s, "set_recall") - 1.0 / 3.0).abs() < 1e-9);
        // with only the dominant site found, the view without it has found nothing
        // and returned nothing
        assert_eq!(m(&s, "nodom_recall"), 0.0);
        assert_eq!(m(&s, "nodom_precision"), 0.0);
        // no dominant site: the view is undefined
        let s = score_item(&q1(), &[ans("m.rs:5")]);
        assert_eq!(metric(&s, "nodom_f1"), None);
    }

    #[test]
    fn annotate_picks_the_macro_site_with_the_most_gold() {
        let site = |n: usize, file: &str| -> Vec<Value> {
            (0..n)
                .map(|i| {
                    json!({"impl_id": format!("{file}{i}"), "stratum": "macro",
                           "impl": span(file, 1, 2), "type_decl": null})
                })
                .collect()
        };
        let mut a = site(3, "big.rs");
        a.extend(site(2, "small.rs"));
        let items = vec![
            json!({"id": "a", "set": "Q1", "gold": a}),
            json!({"id": "b", "set": "Q1", "gold": site(1, "one.rs")}),
            json!({"id": "c", "set": "Q2", "gold": []}),
        ];
        let out = annotate(items);
        assert_eq!(out[0]["dominant_site"]["file"], "big.rs");
        assert_eq!(out[1]["dominant_site"]["file"], "big.rs");
        assert!(out[2].get("dominant_site").is_none());
        // nothing is shared by two entries: no dominant site
        let none = annotate(vec![
            json!({"id": "d", "set": "Q1", "gold": site(1, "x.rs")}),
        ]);
        assert!(none[0].get("dominant_site").is_none());
    }

    #[test]
    fn cfg_gated_answers_are_neither_hit_nor_miss_and_counted_apart() {
        let mut item = q1();
        item["cfg_gated"] = json!([span("linux.rs", 1, 50)]);
        let s = score_item(&item, &[ans("linux.rs:9"), ans("a.rs:12")]);
        assert_eq!(m(&s, "set_precision"), 1.0);
        assert!((m(&s, "set_recall") - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(m(&s, "cfg_gated_answers"), 1.0);
        let plain = score_item(&q1(), &[ans("linux.rs:9"), ans("a.rs:12")]);
        assert_eq!(m(&plain, "set_precision"), 0.5);
        assert_eq!(m(&plain, "cfg_gated_answers"), 0.0);
    }

    #[test]
    fn an_empty_answer_scores_zero() {
        let s = score_item(&q1(), &[]);
        assert_eq!(m(&s, "set_precision"), 0.0);
        assert_eq!(m(&s, "set_recall"), 0.0);
        assert_eq!(m(&s, "set_f1"), 0.0);
        assert_eq!(m(&s, "explicit_f1"), 0.0);
        assert_eq!(m(&s, "recall_blanket"), 0.0);
        let q2 = json!({"id": "q2", "set": "Q2", "stratum": "explicit",
            "gold": [{"impl_id": "i", "stratum": "explicit", "location": span("a.rs", 11, 14)}]});
        let s = score_item(&q2, &[]);
        assert_eq!(
            (m(&s, "hit@1"), m(&s, "hit@10"), m(&s, "mrr")),
            (0.0, 0.0, 0.0)
        );
    }

    fn q2() -> Value {
        json!({
            "id": "t-Q2-0", "set": "Q2", "stratum": "macro", "trait": "x::Tr",
            "trait_decl": span("tr.rs", 100, 140),
            "gold": [{"impl_id": "i2", "stratum": "macro", "location": span("m.rs", 5, 5),
                      "impl": span("m.rs", 5, 5)}]
        })
    }

    #[test]
    fn q2_ranks_the_first_answer_on_the_location() {
        let item = q2();
        let s = score_item(&item, &[ans("m.rs:5")]);
        assert_eq!(
            (m(&s, "hit@1"), m(&s, "hit@10"), m(&s, "mrr")),
            (1.0, 1.0, 1.0)
        );
        // the trait declaration's card in front does not push the hit down
        let s = score_item(&item, &[ans("tr.rs:110"), ans("x.rs:1"), ans("m.rs:5")]);
        assert_eq!(
            (m(&s, "hit@1"), m(&s, "hit@10"), m(&s, "mrr")),
            (0.0, 1.0, 0.5)
        );
        // rank 11 is outside the cut
        let mut late: Vec<Answer> = (1..=10).map(|i| ans(&format!("x.rs:{i}"))).collect();
        late.push(ans("m.rs:5"));
        let s = score_item(&item, &late);
        assert_eq!((m(&s, "hit@10"), m(&s, "mrr")), (0.0, 0.0));
        // the stratum is a dimension
        assert!(s.dims.contains(&("kind", "macro".to_string())));
        assert!(s.dims.contains(&("template", "Q2".to_string())));
    }

    #[test]
    fn q3_credits_the_member_location_only() {
        let item = json!({
            "id": "t-Q3-0", "set": "Q3", "trait": "x::Tr",
            "gold": [
                {"impl_id": "i0", "stratum": "explicit", "location": span("a.rs", 11, 14)},
                {"impl_id": "i1", "stratum": "explicit", "location": span("b.rs", 21, 24)}
            ]
        });
        let s = score_item(&item, &[ans("a.rs:12"), ans("a.rs:10")]);
        assert_eq!(m(&s, "set_recall"), 0.5);
        assert_eq!(m(&s, "set_precision"), 0.5);
        assert_eq!(m(&s, "explicit_recall"), 0.5);
    }

    #[test]
    fn the_oracle_scores_one_on_every_primary_metric() {
        for item in [q1(), q2()] {
            let run: Vec<Answer> = oracle(&item)
                .iter()
                .map(|v| Answer::from_value(v).unwrap())
                .collect();
            let s = score_item(&item, &run);
            if Template::of(&item) == Template::Q2 {
                assert_eq!(m(&s, "hit@1"), 1.0);
            } else {
                assert_eq!(m(&s, "set_f1"), 1.0);
                assert_eq!(m(&s, "explicit_f1"), 1.0);
            }
        }
        let q = to_query(&q1()).ok().unwrap();
        assert_eq!(q.expected, vec!["a.rs", "t.rs", "b.rs", "m.rs"]);
    }
}
