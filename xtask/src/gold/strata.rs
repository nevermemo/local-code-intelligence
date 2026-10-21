//! Cross-set gold strata (K6a-1): every gold item gets one of three guards,
//! `core`, `grep_easy` or `agent_trap`, so the six sets can be read on one
//! axis and the ceilings of later slices can read a grep-easy column against
//! `core` without a join.
//!
//! * `grep_easy`: the question already names the gold (a leaking stem, a
//!   backticked name that is a gold declaration, a path, the trait itself), so
//!   a plain grep finds it. A score there says little about retrieval.
//! * `agent_trap`: the item carries a known trap (many definitions of the
//!   seed, a distant test, a decoy a walk can follow, a leaking stem that
//!   names a decoy path, an unanchored "why" question). An absent test is
//!   not a trap for an impact question: `impact_no_tests` is a witness that
//!   leaves the guard alone. Flow has no grep-easy rule.
//! * `core`: everything else. An item no rule reaches is `core` with the
//!   witness `by_elimination`, counted apart so the number of items the rules
//!   do not reach stays visible (decision d457, B). An item carrying only a
//!   note witness (`impact_no_tests`) is core too, but not by elimination.
//!
//! [`classify`] is pure: it takes the set and the gold item and nothing else
//! (no run, no tree, no LCI output), so a classification cannot depend on how
//! any system answered. When both a trap rule and a grep-easy rule fire the
//! item is `agent_trap` (the more specific claim); the witness lists every
//! rule that fired. The guard is a counting key for reports, never a router.
//!
//! The rule table is pinned to the vocabulary found in the private gold files
//! (impact band `unique`/`some`/`common`, where-to leak part `clean` or a
//! `*-leak` tag, history `H1`..`H3b`). `cargo xtask gold strata` reports the
//! counts, so a vocabulary that drifts shows up as items falling to
//! `by_elimination`.

use super::{Set, arr_of, str_of};
use anyhow::Result;
use lci_maps::exact_history::{HistoryQuery, backtick_idents, classify as classify_history};
use lci_maps::impact::code_names;
use serde_json::{Value, json};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Guard {
    Core,
    GrepEasy,
    AgentTrap,
}

impl Guard {
    pub fn as_str(self) -> &'static str {
        match self {
            Guard::Core => "core",
            Guard::GrepEasy => "grep_easy",
            Guard::AgentTrap => "agent_trap",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stratum {
    pub guard: Guard,
    /// Path length in hops where the set states one (flow `lenN`, impact
    /// `degA-B` lower bound).
    pub hops: Option<u32>,
    /// The rule ids that fired; never gold text.
    pub witness: Vec<&'static str>,
    /// Whether any grep-easy rule fired (the guard may still be `agent_trap`).
    pub easy: bool,
    /// Whether any agent-trap rule fired.
    pub trap: bool,
}

/// Witness of an item no rule reaches.
pub const BY_ELIMINATION: &str = "by_elimination";

/// Rule ids, grouped by the guard they argue for.
mod rule {
    pub const IMPACT_NOT_PRIMARY: &str = "impact_not_primary";
    pub const IMPACT_NAMED_UNIQUE: &str = "impact_named_unique";
    pub const WHERE_TO_STEM_LEAK: &str = "where_to_stem_leak";
    pub const TRAITS_Q1_NAMES_TRAIT: &str = "traits_q1_names_trait";
    pub const HISTORY_H2_PATH: &str = "history_h2_path";
    pub const HISTORY_BODY_NAMES_IT: &str = "history_body_names_it";
    pub const INSTRUCTIONS_DOCS_PATH: &str = "instructions_docs_path";

    /// Witness only: an absent test traps nothing for an R1-closure question,
    /// so it never moves the guard.
    pub const IMPACT_NO_TESTS: &str = "impact_no_tests";

    pub const IMPACT_COMMON_BAND: &str = "impact_common_band";
    pub const IMPACT_TESTS_FAR: &str = "impact_tests_far";
    pub const WHERE_TO_LEAK_DECOY: &str = "where_to_leak_decoy";
    pub const FLOW_DECOY: &str = "flow_decoy";
    pub const HISTORY_H3B: &str = "history_h3b";
}

fn question(item: &Value) -> &str {
    str_of(item, "question").unwrap_or_default()
}

/// A token that looks like a file path: a `/` or a known extension.
fn path_like(token: &str) -> bool {
    const EXT: [&str; 8] = [".rs", ".toml", ".md", ".json", ".py", ".ts", ".js", ".yml"];
    let t = token.trim_matches(|c: char| "`(),;:'\"?.".contains(c));
    !t.is_empty()
        && !t.starts_with('-')
        && t.chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-".contains(c))
        && (t.contains('/') || EXT.iter().any(|e| t.ends_with(e)))
        && t.chars().any(|c| c.is_ascii_alphabetic())
}

fn has_path_token(text: &str) -> bool {
    text.split_whitespace().any(path_like)
}

fn parts(item: &Value) -> Vec<&str> {
    str_of(item, "stratum")
        .unwrap_or_default()
        .split('/')
        .collect()
}

/// `degA-B` -> A; `lenN` -> N.
fn number_after(part: &str, prefix: &str) -> Option<u32> {
    let rest = part.strip_prefix(prefix)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

fn last_segment(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

#[derive(Default)]
struct Fired {
    easy: Vec<&'static str>,
    trap: Vec<&'static str>,
    /// Witnesses that name a property of the item but argue for no guard.
    note: Vec<&'static str>,
}

fn impact(item: &Value, f: &mut Fired) -> Option<u32> {
    let p = parts(item);
    let primary = item
        .get("primary")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if !primary {
        f.easy.push(rule::IMPACT_NOT_PRIMARY);
    }
    let band = item
        .get("ambiguity")
        .and_then(|a| str_of(a, "band"))
        .unwrap_or_default();
    if matches!(band, "unique" | "none") {
        let (names, _) = code_names(question(item));
        let named = arr_of(&item["gold"]["r1"], "decls").iter().any(|d| {
            let gold = str_of(d, "bare")
                .or_else(|| str_of(d, "name").map(last_segment))
                .unwrap_or_default();
            !gold.is_empty()
                && names
                    .iter()
                    .filter_map(|chain| chain.last())
                    .any(|n| n.eq_ignore_ascii_case(gold))
        });
        if named {
            f.easy.push(rule::IMPACT_NAMED_UNIQUE);
        }
    }
    if matches!(band, "common" | "many") {
        f.trap.push(rule::IMPACT_COMMON_BAND);
    }
    let coverage = item
        .get("seed")
        .and_then(|s| str_of(s, "test_coverage"))
        .or_else(|| p.get(2).copied())
        .unwrap_or_default();
    match coverage {
        "tests-far" => f.trap.push(rule::IMPACT_TESTS_FAR),
        "no-tests" => f.note.push(rule::IMPACT_NO_TESTS),
        _ => {}
    }
    p.get(1).and_then(|d| number_after(d, "deg"))
}

fn flow(item: &Value, f: &mut Fired) -> Option<u32> {
    if !arr_of(&item["accept"], "ignorable_nodes").is_empty() {
        f.trap.push(rule::FLOW_DECOY);
    }
    parts(item).get(1).and_then(|l| number_after(l, "len"))
}

fn where_to(item: &Value, f: &mut Fired) {
    let p = parts(item);
    let leak = p.get(1).copied().unwrap_or("n/a");
    let leaks = !matches!(leak, "clean" | "n/a" | "none");
    if leaks {
        f.easy.push(rule::WHERE_TO_STEM_LEAK);
    }
    let form = str_of(&item["gold"], "form").unwrap_or_default();
    // A trap only when the question holds the stem of a decoy path: the stem
    // leak then points at a non-member.
    if leaks && matches!(form, "exemplar" | "convention") {
        let words = question_words(item);
        if arr_of(&item["accept"], "decoys")
            .iter()
            .filter_map(path_of)
            .any(|p| holds_stem(&words, p))
        {
            f.trap.push(rule::WHERE_TO_LEAK_DECOY);
        }
    }
}

fn history(item: &Value, f: &mut Fired) {
    let family = parts(item).first().copied().unwrap_or_default();
    let q = question(item);
    if family == "H2"
        && matches!(
            classify_history(q),
            HistoryQuery::LastModified { path: Some(_), .. }
        )
    {
        f.easy.push(rule::HISTORY_H2_PATH);
    }
    if item
        .get("meta")
        .and_then(|m| m.get("body_names_it"))
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        f.easy.push(rule::HISTORY_BODY_NAMES_IT);
    }
    if family == "H3b" {
        f.trap.push(rule::HISTORY_H3B);
    }
}

fn instructions(item: &Value, f: &mut Fired) {
    let stratum = str_of(item, "stratum").unwrap_or_default();
    if stratum.ends_with("/docs") && has_path_token(question(item)) {
        f.easy.push(rule::INSTRUCTIONS_DOCS_PATH);
    }
}

fn traits(item: &Value, f: &mut Fired) {
    if str_of(item, "set") == Some("Q1") && !backtick_idents(question(item)).is_empty() {
        f.easy.push(rule::TRAITS_Q1_NAMES_TRAIT);
    }
}

/// The guard of one gold item of `set`. Reads the item alone.
pub fn classify(set: Set, item: &Value) -> Stratum {
    let mut fired = Fired::default();
    let hops = match set {
        Set::Impact => impact(item, &mut fired),
        Set::Flow => flow(item, &mut fired),
        Set::WhereTo => {
            where_to(item, &mut fired);
            None
        }
        Set::History => {
            history(item, &mut fired);
            None
        }
        Set::Instructions => {
            instructions(item, &mut fired);
            None
        }
        Set::Traits => {
            traits(item, &mut fired);
            None
        }
    };
    let guard = if !fired.trap.is_empty() {
        Guard::AgentTrap
    } else if !fired.easy.is_empty() {
        Guard::GrepEasy
    } else {
        Guard::Core
    };
    let (easy, trap) = (!fired.easy.is_empty(), !fired.trap.is_empty());
    let mut witness = fired.trap;
    witness.extend(fired.easy);
    witness.extend(fired.note);
    if witness.is_empty() {
        witness.push(BY_ELIMINATION);
    }
    Stratum {
        guard,
        hops,
        witness,
        easy,
        trap,
    }
}

// ------------------------------------------------------------------ report

fn file_stem(path: &str) -> &str {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    name.rsplit_once('.').map_or(name, |(stem, _)| stem)
}

/// Stems too common to say anything when a question holds them.
const GENERIC_STEMS: [&str; 9] = [
    "mod", "lib", "main", "test", "tests", "index", "readme", "types", "utils",
];

fn path_of(v: &Value) -> Option<&str> {
    match v {
        Value::String(s) => Some(s),
        other => str_of(other, "path").or_else(|| str_of(other, "file")),
    }
}

/// Every file the gold of an item names, by the fields the scorers read.
fn gold_files(item: &Value) -> Vec<&str> {
    fn take<'a>(out: &mut Vec<&'a str>, v: &'a Value, key: &str) {
        out.extend(arr_of(v, key).iter().filter_map(path_of));
    }
    let g = &item["gold"];
    let mut out: Vec<&str> = Vec::new();
    take(&mut out, &g["r1"], "files");
    for key in [
        "files",
        "expected_files",
        "documents",
        "intermediates",
        "readers",
        "fan_out",
    ] {
        take(&mut out, g, key);
    }
    if let Some(f) = path_of(&g["field"]) {
        out.push(f);
    }
    if let Value::Array(entries) = g {
        out.extend(entries.iter().filter_map(|e| path_of(&e["impl"])));
    }
    out
}

/// The lower-cased words of the question (letters, digits, `_`).
fn question_words(item: &Value) -> Vec<String> {
    question(item)
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Whether the stem of `path` is one of `words` (generic and short stems never).
fn holds_stem(words: &[String], path: &str) -> bool {
    let stem = file_stem(path).to_ascii_lowercase();
    stem.len() >= 3 && !GENERIC_STEMS.contains(&stem.as_str()) && words.contains(&stem)
}

/// Whether the question holds the stem of a gold file as a whole word.
fn question_holds_gold_stem(item: &Value) -> bool {
    let words = question_words(item);
    gold_files(item)
        .into_iter()
        .any(|path| holds_stem(&words, path))
}

/// Nearest-rank quantile of a sorted list.
fn quantile(sorted: &[u32], q: f64) -> Option<u32> {
    if sorted.is_empty() {
        return None;
    }
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    Some(sorted[rank - 1])
}

/// The `gold strata` report: counts per guard, the items no rule reaches, the
/// hop quantiles, the stem-leak ladder rung and the per-item classification.
/// It scores nothing and never reads LCI output; the owner decides what to do
/// with the leak list.
pub fn report(set: Set, items: &[Value]) -> Result<Value> {
    let zero = || -> BTreeMap<&'static str, usize> {
        [Guard::Core, Guard::GrepEasy, Guard::AgentTrap]
            .iter()
            .map(|g| (g.as_str(), 0))
            .collect()
    };
    let (mut counts, mut scored) = (zero(), zero());
    let mut by_witness: BTreeMap<&'static str, usize> = BTreeMap::new();
    let (mut by_elimination, mut both) = (0usize, 0usize);
    let mut hops: Vec<u32> = Vec::new();
    let mut leak: Vec<String> = Vec::new();
    let mut by_item = serde_json::Map::new();
    for item in items {
        let id = str_of(item, "id").unwrap_or_default().to_string();
        let s = classify(set, item);
        *counts.entry(s.guard.as_str()).or_default() += 1;
        if super::is_scored(set, item) {
            *scored.entry(s.guard.as_str()).or_default() += 1;
        }
        for w in &s.witness {
            *by_witness.entry(*w).or_default() += 1;
        }
        if s.witness == [BY_ELIMINATION] {
            by_elimination += 1;
        }
        if s.easy && s.trap {
            both += 1;
        }
        hops.extend(s.hops);
        // An item an easy rule (or the stem-leak decoy trap) already names
        // is not listed again.
        let named = s.easy || s.witness.contains(&rule::WHERE_TO_LEAK_DECOY);
        if !named && question_holds_gold_stem(item) {
            leak.push(id.clone());
        }
        by_item.insert(
            id,
            json!({"guard": s.guard.as_str(), "hops": s.hops, "witness": s.witness}),
        );
    }
    hops.sort_unstable();
    Ok(json!({
        "set": set.name(),
        "items": items.len(),
        "counts": counts,
        "scored": scored,
        "by_witness": by_witness,
        "both": both,
        "by_elimination": by_elimination,
        "hops": {"p50": quantile(&hops, 0.5), "max": hops.last()},
        "leak": {"n": leak.len(), "ids": leak},
        "by_item": by_item,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETS: [Set; 6] = [
        Set::Impact,
        Set::Flow,
        Set::WhereTo,
        Set::History,
        Set::Instructions,
        Set::Traits,
    ];

    fn impact_item(band: &str, stratum: &str, primary: bool, question: &str) -> Value {
        json!({
            "id": "i", "primary": primary, "stratum": stratum, "question": question,
            "ambiguity": {"band": band},
            "gold": {"r1": {"decls": [{"path": "a.rs", "name": "m::alpha", "bare": "alpha"}],
                            "files": ["src/alpha.rs"]}}
        })
    }

    fn witness(set: Set, item: &Value) -> (Guard, Vec<&'static str>) {
        let s = classify(set, item);
        (s.guard, s.witness)
    }

    #[test]
    fn the_classifier_reads_the_item_alone() {
        let base = impact_item(
            "unique",
            "sym-fn/deg2-4/tests-near",
            true,
            "What uses `alpha`?",
        );
        let mut padded = base.clone();
        padded["results"] = json!([{"path": "a.rs"}]);
        padded["run"] = json!({"x": 1});
        padded["answers"] = json!(["a.rs:1"]);
        assert_eq!(classify(Set::Impact, &base), classify(Set::Impact, &padded));
        // the question moves the guard only through the named rule
        let other = impact_item("unique", "sym-fn/deg2-4/tests-near", true, "What uses it?");
        assert_eq!(witness(Set::Impact, &base).1, [rule::IMPACT_NAMED_UNIQUE]);
        assert_eq!(witness(Set::Impact, &other).1, [BY_ELIMINATION]);
        // every set, a bare item and an item of unrelated keys: never a panic
        for set in SETS {
            for item in [
                json!({"id": "x"}),
                json!({"id": "y", "gold": []}),
                padded.clone(),
            ] {
                let _ = classify(set, &item);
            }
        }
    }

    #[test]
    fn each_guard_has_one_item_that_fires_it_and_one_that_does_not() {
        // impact
        let n = "sym-fn/deg5-10/tests-near";
        let q = "What uses `alpha`?";
        assert_eq!(
            witness(Set::Impact, &impact_item("unique", n, false, "Which?")),
            (Guard::GrepEasy, vec![rule::IMPACT_NOT_PRIMARY])
        );
        assert_eq!(
            witness(Set::Impact, &impact_item("unique", n, true, q)),
            (Guard::GrepEasy, vec![rule::IMPACT_NAMED_UNIQUE])
        );
        assert_eq!(
            witness(Set::Impact, &impact_item("some", n, true, q)).0,
            Guard::Core
        );
        assert_eq!(
            witness(Set::Impact, &impact_item("common", n, true, "Which?")),
            (Guard::AgentTrap, vec![rule::IMPACT_COMMON_BAND])
        );
        let far = impact_item("some", "sym-fn/deg2-4/tests-far", true, "Which?");
        assert_eq!(
            witness(Set::Impact, &far),
            (Guard::AgentTrap, vec![rule::IMPACT_TESTS_FAR])
        );
        // no tests at all traps nothing: a note, core, and not "by elimination"
        let none = impact_item("some", "sym-fn/deg2-4/no-tests", true, "Which?");
        assert_eq!(
            witness(Set::Impact, &none),
            (Guard::Core, vec![rule::IMPACT_NO_TESTS])
        );
        // `many` is an alias of `common`; an unknown band argues for nothing
        assert_eq!(
            witness(Set::Impact, &impact_item("many", n, true, "Which?")).0,
            Guard::AgentTrap
        );
        assert_eq!(
            witness(Set::Impact, &impact_item("several", n, true, "Which?")),
            (Guard::Core, vec![BY_ELIMINATION])
        );
        // both a trap and an easy rule fire: the trap wins, both are named
        let both = impact_item("common", n, false, "Which?");
        assert_eq!(
            witness(Set::Impact, &both),
            (
                Guard::AgentTrap,
                vec![rule::IMPACT_COMMON_BAND, rule::IMPACT_NOT_PRIMARY]
            )
        );
        assert_eq!(classify(Set::Impact, &far).hops, Some(2));
        let big = impact_item("some", "sym-type/deg11-40/tests-near", true, "Which?");
        assert_eq!(classify(Set::Impact, &big).hops, Some(11));

        // flow
        let mut flow = json!({"id": "f", "stratum": "named/len4", "accept": {}});
        assert_eq!(
            witness(Set::Flow, &flow),
            (Guard::Core, vec![BY_ELIMINATION])
        );
        assert_eq!(classify(Set::Flow, &flow).hops, Some(4));
        flow["accept"]["ignorable_nodes"] = json!([{"name": "H::helper"}]);
        assert_eq!(
            witness(Set::Flow, &flow),
            (Guard::AgentTrap, vec![rule::FLOW_DECOY])
        );

        // where-to
        // the decoy `src/decoy_one.rs` is named by the question when `decoys`
        let wt = |stratum: &str, form: &str, decoys: bool| {
            json!({"id": "w", "stratum": stratum, "gold": {"form": form},
                   "question": if decoys { "Which handlers are like decoy_one?" } else { "Which handlers?" },
                   "accept": {"decoys": if decoys { json!(["src/decoy_one.rs"]) } else { json!([]) }}})
        };
        assert_eq!(
            witness(
                Set::WhereTo,
                &wt("registration/clean/single-file", "registration", false)
            ),
            (Guard::Core, vec![BY_ELIMINATION])
        );
        assert_eq!(
            witness(
                Set::WhereTo,
                &wt("registration/stem-leak/single-file", "registration", true)
            ),
            (Guard::GrepEasy, vec![rule::WHERE_TO_STEM_LEAK])
        );
        assert_eq!(
            witness(Set::WhereTo, &wt("exemplar/stem-leak", "exemplar", true)),
            (
                Guard::AgentTrap,
                vec![rule::WHERE_TO_LEAK_DECOY, rule::WHERE_TO_STEM_LEAK]
            )
        );

        // decoys present, but the question names no decoy stem: easy, not a trap
        let mut quiet = wt("exemplar/stem-leak", "exemplar", true);
        quiet["question"] = json!("Which handlers are there?");
        assert_eq!(
            witness(Set::WhereTo, &quiet),
            (Guard::GrepEasy, vec![rule::WHERE_TO_STEM_LEAK])
        );
        // the leak part `none` (or a missing one, read as `n/a`) is not a leak
        for stratum in ["exemplar/none", "exemplar"] {
            assert_eq!(
                witness(Set::WhereTo, &wt(stratum, "exemplar", true)),
                (Guard::Core, vec![BY_ELIMINATION]),
                "{stratum}"
            );
        }

        // history
        let h = |stratum: &str, question: &str| json!({"id": "h", "stratum": stratum, "question": question});
        assert_eq!(
            witness(
                Set::History,
                &h(
                    "H2/fn/changes2",
                    "What is the most recent commit that modified `run` in src/a.rs?"
                )
            ),
            (Guard::GrepEasy, vec![rule::HISTORY_H2_PATH])
        );
        assert_eq!(
            witness(
                Set::History,
                &h(
                    "H2/fn/changes2",
                    "What is the most recent commit that modified `run`?"
                )
            ),
            (Guard::Core, vec![BY_ELIMINATION])
        );
        assert_eq!(
            witness(Set::History, &h("H3b", "Why was it changed?")),
            (Guard::AgentTrap, vec![rule::HISTORY_H3B])
        );
        let mut body = h("H3a", "Why?");
        body["meta"] = json!({"body_names_it": true});
        assert_eq!(
            witness(Set::History, &body),
            (Guard::GrepEasy, vec![rule::HISTORY_BODY_NAMES_IT])
        );
        body["meta"] = json!({"body_names_it": false});
        assert_eq!(
            witness(Set::History, &body),
            (Guard::Core, vec![BY_ELIMINATION])
        );
        // the rule is family-independent: an H1 item fires it too
        let mut h1 = h("H1/tercile1/fn", "Which commit first created `run`?");
        h1["meta"] = json!({"body_names_it": true});
        assert_eq!(
            witness(Set::History, &h1),
            (Guard::GrepEasy, vec![rule::HISTORY_BODY_NAMES_IT])
        );

        // instructions
        let ins = |stratum: &str, question: &str| json!({"id": "n", "stratum": stratum, "question": question});
        assert_eq!(
            witness(
                Set::Instructions,
                &ins("I1/docs", "Which rules apply to docs/guide.md?")
            ),
            (Guard::GrepEasy, vec![rule::INSTRUCTIONS_DOCS_PATH])
        );
        assert_eq!(
            witness(
                Set::Instructions,
                &ins("I1/docs", "Which rules apply to the guide?")
            ),
            (Guard::Core, vec![BY_ELIMINATION])
        );
        assert_eq!(
            witness(
                Set::Instructions,
                &ins("I1/xtask", "Which rules apply to docs/guide.md?")
            ),
            (Guard::Core, vec![BY_ELIMINATION])
        );

        // traits
        let tr = |set: &str, question: &str| json!({"id": "t", "set": set, "question": question});
        assert_eq!(
            witness(Set::Traits, &tr("Q1", "Which types implement `a::Trait`?")),
            (Guard::GrepEasy, vec![rule::TRAITS_Q1_NAMES_TRAIT])
        );
        assert_eq!(
            witness(Set::Traits, &tr("Q1", "Which types implement the trait?")).0,
            Guard::Core
        );
        assert_eq!(
            witness(
                Set::Traits,
                &tr("Q2", "Where is `Trait::m` implemented for `T`?")
            )
            .0,
            Guard::Core
        );
    }

    #[test]
    fn the_leak_count_names_items_whose_question_holds_the_gold_stem() {
        let item = |id: &str, question: &str| {
            json!({"id": id, "stratum": "registration/clean/single-file", "question": question,
                   "gold": {"form": "registration", "expected_files": ["src/widget_table.rs"]},
                   "accept": {}})
        };
        let items = vec![
            item("w-hit", "Where is the widget_table registered?"),
            item("w-miss", "Where is the gadget registered?"),
            item("w-part", "Where is the widget registered?"),
        ];
        let r = report(Set::WhereTo, &items).unwrap();
        assert_eq!(r["leak"]["n"], 1);
        assert_eq!(r["leak"]["ids"], json!(["w-hit"]));
        assert_eq!(r["items"], 3);
        assert_eq!(r["counts"]["core"], 3);
        assert_eq!(r["by_elimination"], 3);
        // an item the rules already call grep-easy is not listed again
        let mut easy = item("w-easy", "Where is the widget_table registered?");
        easy["stratum"] = json!("registration/stem-leak/single-file");
        let r = report(Set::WhereTo, &[easy]).unwrap();
        assert_eq!(r["leak"]["n"], 0);
        assert_eq!(r["counts"]["grep_easy"], 1);
        // nor is a trap whose stem leak is already witnessed
        let trap = json!({"id": "w-trap", "stratum": "exemplar/stem-leak",
            "question": "Which handlers are like decoy_one and widget_table?",
            "gold": {"form": "exemplar", "files": ["src/widget_table.rs"]},
            "accept": {"decoys": ["src/decoy_one.rs"]}});
        let r = report(Set::WhereTo, &[trap]).unwrap();
        assert_eq!(r["counts"]["agent_trap"], 1);
        assert_eq!(r["leak"]["n"], 0);
    }

    #[test]
    fn the_report_explains_its_counts() {
        let n = "sym-fn/deg5-10/tests-near";
        let mut tie = impact_item("common", n, true, "What uses `alpha`?");
        tie["id"] = json!("tie");
        let mut tie_np = impact_item("common", n, false, "Which?");
        tie_np["id"] = json!("tie-not-primary");
        let mut easy_np = impact_item("some", n, false, "Which?");
        easy_np["id"] = json!("easy-not-primary");
        let mut no_tests = impact_item("some", "sym-fn/deg2-4/no-tests", true, "Which?");
        no_tests["id"] = json!("no-tests");
        let mut bare = impact_item("some", n, true, "Which?");
        bare["id"] = json!("bare");
        let r = report(Set::Impact, &[tie, tie_np, easy_np, no_tests, bare]).unwrap();
        // all items: two traps (common band), one easy (not primary), two core
        assert_eq!(
            r["counts"],
            json!({"agent_trap": 2, "core": 2, "grep_easy": 1})
        );
        // scored items only (primary): the not-primary ones leave
        assert_eq!(
            r["scored"],
            json!({"agent_trap": 1, "core": 2, "grep_easy": 0})
        );
        // the tie cases: a trap and an easy rule both fired (`tie-not-primary`)
        assert_eq!(r["both"], 1);
        // core by elimination counts only the item no rule reached
        assert_eq!(r["by_elimination"], 1);
        assert_eq!(r["by_witness"][rule::IMPACT_COMMON_BAND], 2);
        assert_eq!(r["by_witness"][rule::IMPACT_NOT_PRIMARY], 2);
        assert_eq!(r["by_witness"][rule::IMPACT_NO_TESTS], 1);
        assert_eq!(r["by_witness"][BY_ELIMINATION], 1);
        assert_eq!(r["by_item"]["no-tests"]["guard"], "core");
    }
}
