//! `where-to` gold: where a change belongs. Three item forms:
//!
//! * `registration`: hit@3 and MRR (rank cut at 10) of the first expected
//!   file, plus decl_hit@3 for answers that carry a line. Preferred,
//!   documentation, definition, related and neutral sites are skipped when
//!   ranking (neither hit nor miss) unless `--strict`; a decoy is a miss.
//! * `exemplar`: set recall and precision over the member set, at file or
//!   member granularity; `accept.neutral_files` are dropped first.
//! * `convention`: set recall and precision over the implementations. A line
//!   inside an implementation's span (or its `equivalents`) matches it; a
//!   path-only answer matches when exactly one implementation lives in that
//!   file; `accept.neutral` spans are dropped first.
//!
//! Precision is matched members over distinct answered entries, so an entry
//! that matches nothing lowers it.

use super::metrics::{f1, hit_at_k, precision, recall, reciprocal_rank};
use super::{
    Answer, Converted, Dropped, ItemScore, Options, arr_of, dedup_keep_order, span_of, str_of,
    strings_of,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

type Span = (String, i64, i64);

fn lines_of(v: &Value) -> Option<(i64, i64)> {
    let l = v.get("lines").and_then(Value::as_array)?;
    Some((l.first()?.as_i64()?, l.get(1)?.as_i64()?))
}

fn site_spans(item: &Value, keys: &[&str]) -> Vec<Span> {
    keys.iter()
        .flat_map(|k| arr_of(&item["accept"], k))
        .filter_map(|s| {
            let (a, b) = lines_of(s)?;
            Some((str_of(s, "path")?.to_string(), a, b))
        })
        .collect()
}

fn neutral_files(item: &Value) -> BTreeSet<String> {
    let a = &item["accept"];
    let mut out = BTreeSet::new();
    for k in [
        "preferred",
        "documentation",
        "neutral_sites",
        "definition_sites",
    ] {
        out.extend(
            arr_of(a, k)
                .iter()
                .filter_map(|s| str_of(s, "path").map(str::to_string)),
        );
    }
    for r in arr_of(a, "related_sites") {
        out.extend(strings_of(r, "files"));
    }
    for f in strings_of(&item["gold"], "expected_files") {
        out.remove(&f);
    }
    out
}

fn neutral_spans(item: &Value) -> Vec<Span> {
    site_spans(
        item,
        &[
            "preferred",
            "documentation",
            "neutral_sites",
            "definition_sites",
        ],
    )
}

fn registration_rank(item: &Value, answers: &[Answer], strict: bool) -> Option<usize> {
    let expected: BTreeSet<String> = strings_of(&item["gold"], "expected_files")
        .into_iter()
        .collect();
    let neutral = if strict {
        BTreeSet::new()
    } else {
        neutral_files(item)
    };
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    let mut rank = 0usize;
    for a in answers {
        let Some(p) = a.path.as_deref() else { continue };
        if !seen.insert(p) {
            continue;
        }
        if neutral.contains(p) {
            continue;
        }
        rank += 1;
        if expected.contains(p) {
            return Some(rank);
        }
        if rank >= 10 {
            break;
        }
    }
    None
}

fn decl_hit(item: &Value, answers: &[Answer], k: usize, strict: bool) -> bool {
    let spans: Vec<Span> = arr_of(&item["gold"], "expected")
        .iter()
        .filter_map(|s| {
            let (a, b) = lines_of(s)?;
            Some((str_of(s, "path")?.to_string(), a, b))
        })
        .collect();
    let neutral = if strict {
        Vec::new()
    } else {
        neutral_spans(item)
    };
    let related: BTreeSet<String> = if strict {
        BTreeSet::new()
    } else {
        let expected: BTreeSet<String> = strings_of(&item["gold"], "expected_files")
            .into_iter()
            .collect();
        arr_of(&item["accept"], "related_sites")
            .iter()
            .flat_map(|r| strings_of(r, "files"))
            .filter(|f| !expected.contains(f))
            .collect()
    };
    let inside = |spans: &[Span], p: &str, l: i64| {
        spans.iter().any(|(q, a, b)| q == p && *a <= l && l <= *b)
    };
    let mut n = 0;
    for a in answers {
        let (Some(p), Some(l)) = (a.path.as_deref(), a.line) else {
            continue;
        };
        if inside(&spans, p, l) {
            return true;
        }
        if inside(&neutral, p, l) || related.contains(p) {
            continue;
        }
        n += 1;
        if n >= k {
            break;
        }
    }
    false
}

/// Recall and precision as the where-to definition has them: an empty
/// answer scores 0 on both.
fn prf(gold: usize, matched: usize, false_positives: usize) -> (f64, f64) {
    let answered = matched + false_positives;
    if answered == 0 {
        (0.0, 0.0)
    } else {
        (recall(matched, gold), precision(matched, answered))
    }
}

fn exemplar_prf(item: &Value, answers: &[Answer]) -> (f64, f64) {
    let g = &item["gold"];
    let neutral: BTreeSet<String> = arr_of(&item["accept"], "neutral_files")
        .iter()
        .filter_map(|n| str_of(n, "path").map(str::to_string))
        .collect();
    let answers: Vec<&Answer> = answers
        .iter()
        .filter(|a| a.path.as_ref().is_none_or(|p| !neutral.contains(p)))
        .collect();
    let members = arr_of(g, "members");
    if str_of(g, "granularity") == Some("file") {
        let gold: BTreeSet<&str> = members.iter().filter_map(|m| str_of(m, "path")).collect();
        let got: BTreeSet<&str> = answers.iter().filter_map(|a| a.path.as_deref()).collect();
        let matched = gold.intersection(&got).count();
        return prf(gold.len(), matched, got.difference(&gold).count());
    }
    let by_name: BTreeMap<String, &str> = members
        .iter()
        .filter_map(|m| str_of(m, "name").map(|n| (n.to_lowercase(), n)))
        .collect();
    let by_loc: BTreeMap<(&str, i64), &str> = members
        .iter()
        .filter_map(|m| {
            Some((
                (str_of(m, "path")?, m.get("line")?.as_i64()?),
                str_of(m, "name")?,
            ))
        })
        .collect();
    let mut matched: BTreeSet<&str> = BTreeSet::new();
    let mut false_positives: BTreeSet<(String, Option<i64>, String)> = BTreeSet::new();
    for a in answers {
        let by_n = a.name.as_ref().and_then(|n| by_name.get(&n.to_lowercase()));
        let by_l = match (a.path.as_deref(), a.line) {
            (Some(p), Some(l)) => by_loc.get(&(p, l)),
            _ => None,
        };
        if let Some(name) = by_n.or(by_l) {
            matched.insert(name);
        } else {
            false_positives.insert((
                a.path.clone().unwrap_or_default(),
                a.line,
                a.name.clone().unwrap_or_default().to_lowercase(),
            ));
        }
    }
    let gold: BTreeSet<&str> = by_name.values().copied().collect();
    prf(gold.len(), matched.len(), false_positives.len())
}

fn convention_prf(item: &Value, answers: &[Answer]) -> (f64, f64) {
    let g = &item["gold"];
    let impls = arr_of(g, "impls");
    let prim: Vec<(String, i64, i64, usize)> = impls
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            let (a, b) = span_of(m)?;
            Some((str_of(m, "path")?.to_string(), a, b, i))
        })
        .collect();
    let eq: Vec<(String, i64, i64, usize)> = arr_of(g, "equivalents")
        .iter()
        .filter_map(|e| {
            let (a, b) = span_of(e)?;
            let of = &e["equivalent_of"];
            let idx = impls.iter().position(|m| {
                str_of(m, "path") == str_of(of, "path") && m.get("line") == of.get("line")
            })?;
            Some((str_of(e, "path")?.to_string(), a, b, idx))
        })
        .collect();
    let neutral: Vec<(String, i64, i64)> = arr_of(&item["accept"], "neutral")
        .iter()
        .filter_map(|n| {
            let (a, b) = span_of(n)?;
            Some((str_of(n, "path")?.to_string(), a, b))
        })
        .collect();
    let mut by_path: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    for (p, _, _, i) in &prim {
        by_path.entry(p.as_str()).or_default().push(*i);
    }
    let mut matched: BTreeSet<usize> = BTreeSet::new();
    let mut false_positives: BTreeSet<(String, Option<i64>)> = BTreeSet::new();
    for a in answers {
        let Some(p) = a.path.as_deref() else { continue };
        let key = match a.line {
            Some(l) => {
                let hit = prim
                    .iter()
                    .chain(eq.iter())
                    .find(|(q, s, e, _)| q == p && *s <= l && l <= *e)
                    .map(|t| t.3);
                if hit.is_none() && neutral.iter().any(|(q, s, e)| q == p && *s <= l && l <= *e) {
                    continue;
                }
                hit
            }
            None => {
                if by_path.get(p).is_some_and(|v| v.len() == 1) {
                    Some(by_path[p][0])
                } else if neutral.iter().any(|(q, _, _)| q == p) {
                    continue;
                } else {
                    None
                }
            }
        };
        match key {
            Some(k) => {
                matched.insert(k);
            }
            None => {
                false_positives.insert((p.to_string(), a.line));
            }
        }
    }
    prf(impls.len(), matched.len(), false_positives.len())
}

pub fn score_item(item: &Value, answers: &[Answer], opts: &Options) -> ItemScore {
    let form = str_of(&item["gold"], "form").unwrap_or("unknown");
    let mut metrics: Vec<(&'static str, Option<f64>)> = Vec::new();
    match form {
        "registration" => {
            let rank = registration_rank(item, answers, opts.strict);
            metrics.push(("reg_hit@3", Some(hit_at_k(rank, 3))));
            metrics.push(("reg_mrr", Some(reciprocal_rank(rank, 10))));
            metrics.push((
                "reg_decl_hit@3",
                Some(if decl_hit(item, answers, 3, opts.strict) {
                    1.0
                } else {
                    0.0
                }),
            ));
        }
        other => {
            let (r, p) = if other == "exemplar" {
                exemplar_prf(item, answers)
            } else {
                convention_prf(item, answers)
            };
            metrics.push(("set_recall", Some(r)));
            metrics.push(("set_precision", Some(p)));
            metrics.push(("set_f1", Some(f1(p, r))));
        }
    }
    let stratum = str_of(item, "stratum").unwrap_or_default();
    let mut parts = stratum.split('/');
    let _ = parts.next();
    let leak = parts.next().unwrap_or("n/a").to_string();
    ItemScore {
        metrics,
        dims: vec![("form", form.to_string()), ("leak", leak)],
    }
}

pub fn oracle(item: &Value) -> Vec<Value> {
    let g = &item["gold"];
    match str_of(g, "form") {
        Some("registration") => arr_of(g, "expected")
            .iter()
            .filter_map(|s| Some(json!({"path": s["path"], "line": lines_of(s)?.0})))
            .collect(),
        Some("exemplar") => arr_of(g, "members")
            .iter()
            .map(|m| json!({"path": m["path"], "line": m["line"], "name": m["name"]}))
            .collect(),
        _ => arr_of(g, "impls")
            .iter()
            .map(|m| json!({"path": m["path"], "line": m["line"]}))
            .collect(),
    }
}

pub fn to_query(item: &Value) -> Result<Converted, Dropped> {
    let g = &item["gold"];
    let files: Vec<String> = match str_of(g, "form") {
        Some("registration") => strings_of(g, "expected_files"),
        Some("exemplar") => arr_of(g, "members")
            .iter()
            .filter_map(|m| str_of(m, "path").map(str::to_string))
            .collect(),
        _ => arr_of(g, "impls")
            .iter()
            .filter_map(|m| str_of(m, "path").map(str::to_string))
            .collect(),
    };
    let expected = dedup_keep_order(files);
    if expected.is_empty() {
        return Err(Dropped("no gold files"));
    }
    let disfavored = dedup_keep_order(
        arr_of(&item["accept"], "decoys")
            .iter()
            .filter_map(|d| str_of(d, "path").map(str::to_string))
            .filter(|p| !expected.contains(p)),
    );
    let stratum = str_of(item, "stratum").unwrap_or_default();
    let tags = vec![format!("stratum:{stratum}")];
    Ok(Converted {
        expected,
        preferred: Vec::new(),
        disfavored,
        tags,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metric(s: &ItemScore, name: &str) -> f64 {
        s.metrics
            .iter()
            .find(|(m, _)| *m == name)
            .unwrap()
            .1
            .unwrap()
    }

    fn lenient() -> Options {
        Options { strict: false }
    }

    fn ans(s: &str) -> Answer {
        Answer::parse_shorthand(s)
    }

    fn registration() -> Value {
        json!({
            "id": "where-to-t1", "stratum": "registration/clean/single-file",
            "accept": {
                "decoys": [{"path": "decoy.rs"}],
                "preferred": [{"path": "tests/enum.rs", "lines": [1, 9]}],
                "related_sites": [{"files": ["cfg.rs"]}]
            },
            "gold": {
                "form": "registration", "expected_files": ["reg.rs"],
                "expected": [{"path": "reg.rs", "lines": [10, 20]}]
            }
        })
    }

    #[test]
    fn registration_ranks_skip_neutral_files() {
        let item = registration();
        let s = score_item(
            &item,
            &[
                ans("tests/enum.rs"),
                ans("cfg.rs"),
                ans("decoy.rs"),
                ans("reg.rs"),
            ],
            &lenient(),
        );
        assert_eq!(metric(&s, "reg_hit@3"), 1.0);
        assert_eq!(metric(&s, "reg_mrr"), 0.5);
        let strict = score_item(
            &item,
            &[
                ans("tests/enum.rs"),
                ans("cfg.rs"),
                ans("decoy.rs"),
                ans("reg.rs"),
            ],
            &Options { strict: true },
        );
        assert_eq!(metric(&strict, "reg_hit@3"), 0.0);
        assert_eq!(metric(&strict, "reg_mrr"), 0.25);
    }

    #[test]
    fn decl_hit_needs_a_line_inside_an_expected_span() {
        let item = registration();
        assert_eq!(
            metric(
                &score_item(&item, &[ans("reg.rs:15")], &lenient()),
                "reg_decl_hit@3"
            ),
            1.0
        );
        assert_eq!(
            metric(
                &score_item(&item, &[ans("reg.rs:99")], &lenient()),
                "reg_decl_hit@3"
            ),
            0.0
        );
        // a neutral-site answer does not use up one of the three slots
        let fill = [
            ans("tests/enum.rs:2"),
            ans("x.rs:1"),
            ans("y.rs:1"),
            ans("reg.rs:12"),
        ];
        assert_eq!(
            metric(&score_item(&item, &fill, &lenient()), "reg_decl_hit@3"),
            1.0
        );
    }

    #[test]
    fn exemplar_precision_counts_unmatched_entries() {
        let item = json!({
            "id": "where-to-t2", "stratum": "exemplar/clean",
            "accept": {"neutral_files": [{"path": "list.rs"}]},
            "gold": {"form": "exemplar", "granularity": "member",
                "members": [{"name": "Alpha", "path": "a.rs", "line": 1}, {"name": "Beta", "path": "b.rs", "line": 2}]}
        });
        let answers = [
            Answer {
                name: Some("alpha".into()),
                ..Default::default()
            },
            ans("b.rs:2"),
            ans("bogus.rs:7"),
            ans("list.rs"),
        ];
        let s = score_item(&item, &answers, &lenient());
        assert_eq!(metric(&s, "set_recall"), 1.0);
        assert!((metric(&s, "set_precision") - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(
            metric(&score_item(&item, &[], &lenient()), "set_precision"),
            0.0
        );
        let by_file = json!({
            "id": "where-to-t3", "stratum": "exemplar/clean", "accept": {},
            "gold": {"form": "exemplar", "granularity": "file",
                "members": [{"path": "a.rs"}, {"path": "b.rs"}]}
        });
        let s = score_item(&by_file, &[ans("a.rs"), ans("z.rs")], &lenient());
        assert_eq!(metric(&s, "set_recall"), 0.5);
        assert_eq!(metric(&s, "set_precision"), 0.5);
    }

    #[test]
    fn convention_matches_spans_equivalents_and_single_file_paths() {
        let item = json!({
            "id": "where-to-t4", "stratum": "convention/clean",
            "accept": {"neutral": [{"path": "base.rs", "span": [1, 5]}]},
            "gold": {"form": "convention",
                "impls": [{"path": "a.rs", "line": 10, "span": [10, 14]}, {"path": "b.rs", "line": 20, "span": [20, 24]}],
                "equivalents": [{"path": "b.rs", "span": [50, 52], "equivalent_of": {"path": "b.rs", "line": 20}}]}
        });
        let full = score_item(
            &item,
            &[ans("a.rs:12"), ans("b.rs:51"), ans("base.rs:3")],
            &lenient(),
        );
        assert_eq!(metric(&full, "set_recall"), 1.0);
        assert_eq!(metric(&full, "set_precision"), 1.0);
        let by_path = score_item(&item, &[ans("a.rs"), ans("other.rs")], &lenient());
        assert_eq!(metric(&by_path, "set_recall"), 0.5);
        assert_eq!(metric(&by_path, "set_precision"), 0.5);
    }

    #[test]
    fn oracle_is_perfect_and_converts_to_files() {
        let item = registration();
        let run: Vec<Answer> = oracle(&item)
            .iter()
            .map(|v| Answer::from_value(v).unwrap())
            .collect();
        let s = score_item(&item, &run, &lenient());
        assert_eq!(metric(&s, "reg_hit@3"), 1.0);
        assert_eq!(metric(&s, "reg_decl_hit@3"), 1.0);
        let q = to_query(&item).ok().unwrap();
        assert_eq!(q.expected, vec!["reg.rs"]);
        assert_eq!(q.disfavored, vec!["decoy.rs"]);
    }
}
