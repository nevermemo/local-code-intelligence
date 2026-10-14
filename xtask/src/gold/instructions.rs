//! `instructions` gold: which rules apply.
//!
//! * `I1` (path-scoped): the answer is a set of document sections. A returned
//!   span counts when it lies in, or is, a gold section of the same document.
//!   Reports section recall, precision and F1, and recall over the
//!   `scope == "path"` sections only (a root-only system already finds the
//!   rest).
//! * `I2` (task-triggered): the answer is a ranked list of units. A returned
//!   span matches a unit when it overlaps it in the same document. Reports
//!   hit@3 on the `primary` and `core` units, required-unit recall (at 5 and
//!   over the whole list), required-unit precision@5 and whether a decoy
//!   document reached the top 3.
//!
//! A path-only answer is the whole document: it can never lie inside a
//! section, but it overlaps every unit of that document.

use super::metrics::{f1, hit_at_k, precision, recall};
use super::{Answer, Converted, Dropped, ItemScore, arr_of, dedup_keep_order, str_of, strings_of};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Span {
    doc: String,
    start: i64,
    end: i64,
}

fn span_of_answer(a: &Answer) -> Option<Span> {
    let doc = a.path.clone()?;
    let (start, end) = match a.line {
        Some(l) => (l, a.end_line.unwrap_or(l)),
        None => (1, i64::MAX),
    };
    Some(Span { doc, start, end })
}

fn spans_of(answers: &[Answer]) -> Vec<Span> {
    let mut seen = BTreeSet::new();
    answers
        .iter()
        .filter_map(span_of_answer)
        .filter(|s| seen.insert((s.doc.clone(), s.start, s.end)))
        .collect()
}

fn gold_span(v: &Value) -> Option<Span> {
    Some(Span {
        doc: str_of(v, "doc")?.to_string(),
        start: v.get("start_line")?.as_i64()?,
        end: v.get("end_line")?.as_i64()?,
    })
}

fn within(a: &Span, g: &Span) -> bool {
    a.doc == g.doc && a.start >= g.start && a.end <= g.end
}

fn overlaps(a: &Span, g: &Span) -> bool {
    a.doc == g.doc && a.start <= g.end && g.start <= a.end
}

fn subtype(item: &Value) -> &str {
    str_of(item, "subtype").unwrap_or("unknown")
}

fn score_i1(item: &Value, answers: &[Answer]) -> Vec<(&'static str, Option<f64>)> {
    let sections: Vec<(Span, bool)> = arr_of(&item["gold"], "sections")
        .iter()
        .filter_map(|s| Some((gold_span(s)?, str_of(s, "scope") == Some("path"))))
        .collect();
    let returned = spans_of(answers);
    let correct = returned
        .iter()
        .filter(|a| sections.iter().any(|(g, _)| within(a, g)))
        .count();
    let found = sections
        .iter()
        .filter(|(g, _)| returned.iter().any(|a| within(a, g)))
        .count();
    let path_scoped: Vec<&(Span, bool)> = sections.iter().filter(|(_, p)| *p).collect();
    let path_found = path_scoped
        .iter()
        .filter(|(g, _)| returned.iter().any(|a| within(a, g)))
        .count();
    let p = precision(correct, returned.len());
    let r = recall(found, sections.len());
    vec![
        ("i1_section_precision", Some(p)),
        ("i1_section_recall", Some(r)),
        ("i1_section_f1", Some(f1(p, r))),
        (
            "i1_path_specific_recall",
            (!path_scoped.is_empty()).then(|| recall(path_found, path_scoped.len())),
        ),
    ]
}

fn units(item: &Value, list: &str) -> Vec<Span> {
    let table = &item["gold"]["units"];
    strings_of(&item["gold"], list)
        .iter()
        .filter_map(|k| table.get(k).and_then(gold_span))
        .collect()
}

fn score_i2(item: &Value, answers: &[Answer]) -> Vec<(&'static str, Option<f64>)> {
    let returned = spans_of(answers);
    let hit_rank = |gold: &[Span]| {
        returned
            .iter()
            .position(|a| gold.iter().any(|g| overlaps(a, g)))
            .map(|i| i + 1)
    };
    let primary = units(item, "primary");
    let core = units(item, "core");
    let required = units(item, "required");
    let covered = |k: usize| {
        let found = required
            .iter()
            .filter(|g| returned.iter().take(k).any(|a| overlaps(a, g)))
            .count();
        recall(found, required.len())
    };
    let top5: Vec<&Span> = returned.iter().take(5).collect();
    let top5_correct = top5
        .iter()
        .filter(|a| required.iter().any(|g| overlaps(a, g)))
        .count();
    let decoys: BTreeSet<String> = strings_of(&item["accept"], "decoys").into_iter().collect();
    let decoy_in_top3 = returned.iter().take(3).any(|a| decoys.contains(&a.doc));
    vec![
        ("i2_hit@3_primary", Some(hit_at_k(hit_rank(&primary), 3))),
        ("i2_hit@3_core", Some(hit_at_k(hit_rank(&core), 3))),
        ("i2_required_recall@5", Some(covered(5))),
        ("i2_required_recall", Some(covered(usize::MAX))),
        (
            "i2_required_precision@5",
            Some(precision(top5_correct, top5.len())),
        ),
        (
            "i2_decoy_in_top3",
            Some(if decoy_in_top3 { 1.0 } else { 0.0 }),
        ),
    ]
}

pub fn score_item(item: &Value, answers: &[Answer]) -> ItemScore {
    let metrics = if subtype(item) == "I1" {
        score_i1(item, answers)
    } else {
        score_i2(item, answers)
    };
    ItemScore {
        metrics,
        dims: vec![
            ("subtype", subtype(item).to_string()),
            (
                "stratum",
                str_of(item, "stratum").unwrap_or("unknown").to_string(),
            ),
        ],
    }
}

fn span_json(s: &Span) -> Value {
    json!({"path": s.doc, "start_line": s.start, "end_line": s.end})
}

pub fn oracle(item: &Value) -> Vec<Value> {
    if subtype(item) == "I1" {
        arr_of(&item["gold"], "sections")
            .iter()
            .filter_map(gold_span)
            .map(|s| span_json(&s))
            .collect()
    } else {
        units(item, "required").iter().map(span_json).collect()
    }
}

/// The root-only system of the gold documentation: it returns every section
/// of the two root rule documents for every path. Rebuilt from the gold, it
/// returns the root sections that occur in some I1 item, so recall matches
/// the real system exactly while precision is an upper bound (sections of the
/// root documents that no item needs are missing from the list). I2 items
/// get no answer: the lexical I2 baseline needs the rule corpus.
pub fn baseline(items: &[Value]) -> Result<(Value, String)> {
    const ROOT: [&str; 2] = ["AGENTS.md", "CLAUDE.md"];
    let mut seen = BTreeSet::new();
    let mut root_sections: Vec<Value> = Vec::new();
    for item in items.iter().filter(|i| subtype(i) == "I1") {
        for s in arr_of(&item["gold"], "sections")
            .iter()
            .filter_map(gold_span)
        {
            if ROOT.contains(&s.doc.as_str()) && seen.insert((s.doc.clone(), s.start, s.end)) {
                root_sections.push(span_json(&s));
            }
        }
    }
    let mut run = serde_json::Map::new();
    for item in items.iter().filter(|i| subtype(i) == "I1") {
        let id = str_of(item, "id").unwrap_or_default().to_string();
        run.insert(id, Value::Array(root_sections.clone()));
    }
    Ok((
        Value::Object(run),
        "instructions: I1 root-only baseline (AGENTS.md and CLAUDE.md sections). Recall is exact; precision is \
         an upper bound. No I2 baseline can be rebuilt from the gold file (the lexical baseline needs the rule \
         corpus), so I2 items are absent from the run; score with --partial to ignore them"
            .to_string(),
    ))
}

pub fn to_query(item: &Value) -> Result<Converted, Dropped> {
    let g = &item["gold"];
    let (expected, preferred) = if subtype(item) == "I1" {
        (dedup_keep_order(strings_of(g, "documents")), Vec::new())
    } else {
        let primary = dedup_keep_order(units(item, "primary").into_iter().map(|s| s.doc));
        let others = dedup_keep_order(
            units(item, "required")
                .into_iter()
                .map(|s| s.doc)
                .filter(|d| !primary.contains(d)),
        );
        (primary, others)
    };
    if expected.is_empty() {
        return Err(Dropped("no gold documents"));
    }
    let disfavored = dedup_keep_order(
        strings_of(&item["accept"], "decoys")
            .into_iter()
            .filter(|d| !expected.contains(d) && !preferred.contains(d)),
    );
    let stratum = str_of(item, "stratum").unwrap_or("unknown");
    Ok(Converted {
        expected,
        preferred,
        disfavored,
        tags: vec![format!("stratum:{stratum}")],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(s: &ItemScore, name: &str) -> Option<f64> {
        s.metrics.iter().find(|(m, _)| *m == name).unwrap().1
    }

    fn ans(s: &str) -> Answer {
        Answer::parse_shorthand(s)
    }

    fn i1() -> Value {
        json!({
            "id": "instructions-I1-t", "subtype": "I1", "stratum": "I1/docs",
            "accept": {"decoys": ["vendor/AGENTS.md"]},
            "gold": {
                "documents": ["AGENTS.md", "docs/rules.md"],
                "sections": [
                    {"doc": "AGENTS.md", "start_line": 1, "end_line": 20, "scope": "everywhere"},
                    {"doc": "docs/rules.md", "start_line": 5, "end_line": 9, "scope": "path"}
                ]
            }
        })
    }

    fn i2() -> Value {
        json!({
            "id": "instructions-I2-t", "subtype": "I2", "stratum": "I2/skill/x",
            "accept": {"decoys": ["vendor/AGENTS.md"]},
            "gold": {
                "units": {
                    "S": {"doc": "skill.md", "start_line": 1, "end_line": 17},
                    "R": {"doc": "ref.md", "start_line": 1, "end_line": 25},
                    "B": {"doc": "AGENTS.md", "start_line": 46, "end_line": 46}
                },
                "primary": ["S"], "core": ["S", "R"], "required": ["S", "R", "B"]
            }
        })
    }

    #[test]
    fn i1_counts_spans_inside_gold_sections() {
        let s = score_item(
            &i1(),
            &[
                ans("AGENTS.md:3-8"),
                ans("docs/rules.md:5-9"),
                ans("vendor/AGENTS.md:1-4"),
            ],
        );
        assert_eq!(metric(&s, "i1_section_recall"), Some(1.0));
        assert!((metric(&s, "i1_section_precision").unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(metric(&s, "i1_path_specific_recall"), Some(1.0));
        // a span that sticks out of the section, and a path-only answer, do not count
        let s = score_item(&i1(), &[ans("AGENTS.md:15-30"), ans("docs/rules.md")]);
        assert_eq!(metric(&s, "i1_section_recall"), Some(0.0));
        assert_eq!(metric(&s, "i1_path_specific_recall"), Some(0.0));
        assert_eq!(
            metric(&score_item(&i1(), &[]), "i1_section_precision"),
            Some(0.0)
        );
    }

    #[test]
    fn i2_hit_at_3_uses_overlap_with_primary_units() {
        let top = [ans("x.md:1-2"), ans("skill.md:10-12"), ans("y.md:1-2")];
        assert_eq!(
            metric(&score_item(&i2(), &top), "i2_hit@3_primary"),
            Some(1.0)
        );
        let late = [
            ans("x.md:1-2"),
            ans("y.md:1-2"),
            ans("z.md:1-2"),
            ans("skill.md:1-17"),
        ];
        assert_eq!(
            metric(&score_item(&i2(), &late), "i2_hit@3_primary"),
            Some(0.0)
        );
        assert_eq!(
            metric(&score_item(&i2(), &late), "i2_hit@3_core"),
            Some(0.0)
        );
    }

    #[test]
    fn i2_required_recall_precision_and_decoys() {
        let run = [
            ans("skill.md:1-17"),
            ans("vendor/AGENTS.md:1-3"),
            ans("AGENTS.md:46"),
            ans("n1.md:1"),
            ans("n2.md:1"),
            ans("ref.md:1-25"),
        ];
        let s = score_item(&i2(), &run);
        assert!((metric(&s, "i2_required_recall@5").unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(metric(&s, "i2_required_recall"), Some(1.0));
        assert!((metric(&s, "i2_required_precision@5").unwrap() - 2.0 / 5.0).abs() < 1e-9);
        assert_eq!(metric(&s, "i2_decoy_in_top3"), Some(1.0));
    }

    #[test]
    fn oracle_scores_one_and_baseline_is_root_only() {
        for item in [i1(), i2()] {
            let run: Vec<Answer> = oracle(&item)
                .iter()
                .map(|v| Answer::from_value(v).unwrap())
                .collect();
            let s = score_item(&item, &run);
            let name = if subtype(&item) == "I1" {
                "i1_section_f1"
            } else {
                "i2_required_recall"
            };
            assert_eq!(metric(&s, name), Some(1.0));
        }
        let (run, _) = baseline(&[i1(), i2()]).unwrap();
        assert_eq!(run.as_object().unwrap().len(), 1);
        assert_eq!(run["instructions-I1-t"].as_array().unwrap().len(), 1);
        let q = to_query(&i2()).ok().unwrap();
        assert_eq!(q.expected, vec!["skill.md"]);
        assert_eq!(q.preferred, vec!["ref.md", "AGENTS.md"]);
        assert_eq!(q.disfavored, vec!["vendor/AGENTS.md"]);
    }
}
