//! `impact` gold: the R1 closure of a symbol or config key. Primary metrics
//! are set precision/recall/F1 and recall@k of `gold.r1.decls`, and the same
//! over `gold.r1.files`, on the `primary: true` items.
//!
//! A returned declaration matches a gold declaration by `symbol`, or by
//! `path` plus a line inside the gold span, or by `path` plus the bare or
//! full name. Declarations in `accept.neutral_decls`, the seed's own
//! declaration and files in `accept.neutral_files` earn neither credit nor
//! penalty; any other returned declaration or file counts as wrong.

use super::metrics::{f1, ndcg_at_k, precision, recall};
use super::{
    Answer, Converted, Dropped, ItemScore, arr_of, dedup_keep_order, span_of, str_of, strings_of,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub fn is_primary(item: &Value) -> bool {
    item.get("primary")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[derive(Debug, Clone)]
struct Decl {
    symbol: String,
    path: String,
    name: String,
    bare: String,
    span: (i64, i64),
}

fn last_segment(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

fn decl_of(v: &Value) -> Option<Decl> {
    let path = str_of(v, "path")?.to_string();
    let name = str_of(v, "name").unwrap_or_default().to_string();
    let line = v.get("line").and_then(Value::as_i64).unwrap_or(0);
    Some(Decl {
        symbol: str_of(v, "symbol").unwrap_or_default().to_string(),
        bare: str_of(v, "bare")
            .unwrap_or_else(|| last_segment(&name))
            .to_string(),
        span: span_of(v).unwrap_or((line, line)),
        path,
        name,
    })
}

/// Test lists wrap the declaration (`decl`, or `enclosing` for lexical ones).
fn test_decl_of(v: &Value) -> Option<Decl> {
    decl_of(v.get("decl").or_else(|| v.get("enclosing")).unwrap_or(v))
}

fn decls(v: &Value, key: &str) -> Vec<Decl> {
    arr_of(v, key).iter().filter_map(decl_of).collect()
}

fn is_decl_entry(a: &Answer) -> bool {
    a.line.is_some() || a.name.is_some() || a.symbol.is_some()
}

fn name_matches(a: &str, d: &Decl) -> bool {
    a.eq_ignore_ascii_case(&d.name)
        || last_segment(a).eq_ignore_ascii_case(&d.bare)
        || a.eq_ignore_ascii_case(&d.bare)
}

fn matches(a: &Answer, d: &Decl) -> bool {
    if let Some(s) = &a.symbol
        && !d.symbol.is_empty()
        && *s == d.symbol
    {
        return true;
    }
    match (&a.path, a.line, &a.name) {
        (Some(p), Some(l), _) => *p == d.path && l >= d.span.0 && l <= d.span.1,
        (Some(p), None, Some(n)) => *p == d.path && name_matches(n, d),
        (None, None, Some(n)) => name_matches(n, d),
        _ => false,
    }
}

fn matched_indices(a: &Answer, gold: &[Decl]) -> Vec<usize> {
    gold.iter()
        .enumerate()
        .filter(|(_, d)| matches(a, d))
        .map(|(i, _)| i)
        .collect()
}

fn path_of_entry(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        other => str_of(other, "path").map(str::to_string),
    }
}

fn dedup_answers(answers: &[Answer]) -> Vec<&Answer> {
    let mut seen = BTreeSet::new();
    answers
        .iter()
        .filter(|a| seen.insert((a.path.clone(), a.line, a.name.clone(), a.symbol.clone())))
        .collect()
}

fn dims(item: &Value) -> Vec<(&'static str, String)> {
    let stratum = str_of(item, "stratum").unwrap_or_default();
    let mut parts = stratum.split('/');
    let kind = parts.next().unwrap_or("unknown").to_string();
    let band = item
        .get("ambiguity")
        .and_then(|a| str_of(a, "band"))
        .unwrap_or("n/a")
        .to_string();
    let coverage = item
        .get("seed")
        .and_then(|s| str_of(s, "test_coverage"))
        .map(str::to_string)
        .or_else(|| stratum.split('/').nth(2).map(str::to_string))
        .unwrap_or_else(|| "n/a".to_string());
    vec![("kind", kind), ("band", band), ("coverage", coverage)]
}

pub fn score_item(item: &Value, answers: &[Answer]) -> ItemScore {
    let g = &item["gold"];
    let gold = decls(&g["r1"], "decls");
    let gold_files: BTreeSet<String> = strings_of(&g["r1"], "files").into_iter().collect();
    let mut neutral = decls(&item["accept"], "neutral_decls");
    if let Some(seed) = item.get("seed").and_then(decl_of) {
        neutral.push(seed);
    }
    let neutral_files: BTreeSet<String> = arr_of(&item["accept"], "neutral_files")
        .iter()
        .filter_map(path_of_entry)
        .filter(|p| !gold_files.contains(p))
        .collect();

    // Non-neutral decl entries in rank order, with the gold they match.
    let mut ranked: Vec<(&Answer, Vec<usize>)> = Vec::new();
    let mut files: Vec<String> = Vec::new();
    for a in dedup_answers(answers) {
        if is_decl_entry(a) {
            let hit = matched_indices(a, &gold);
            if hit.is_empty() && neutral.iter().any(|d| matches(a, d)) {
                continue;
            }
            if let Some(p) = &a.path {
                files.push(p.clone());
            }
            ranked.push((a, hit));
        } else if let Some(p) = &a.path
            && !neutral_files.contains(p)
        {
            files.push(p.clone());
        }
    }
    let files = dedup_keep_order(files);

    let correct = ranked.iter().filter(|(_, h)| !h.is_empty()).count();
    let found: BTreeSet<usize> = ranked.iter().flat_map(|(_, h)| h.iter().copied()).collect();
    let p = precision(correct, ranked.len());
    let r = recall(found.len(), gold.len());

    let recall_at = |k: usize| {
        let seen: BTreeSet<usize> = ranked
            .iter()
            .take(k)
            .flat_map(|(_, h)| h.iter().copied())
            .collect();
        recall(seen.len(), gold.len())
    };
    let mut credited = BTreeSet::new();
    let relevance: Vec<bool> = ranked
        .iter()
        .map(|(_, h)| {
            let fresh = h.iter().any(|i| !credited.contains(i));
            credited.extend(h.iter().copied());
            fresh
        })
        .collect();

    let file_hits = files.iter().filter(|f| gold_files.contains(*f)).count();
    let fp = precision(file_hits, files.len());
    let fr = recall(file_hits, gold_files.len());

    // Secondary: recall of other gold lists by any non-neutral decl entry.
    let covered = |list: &[Decl]| -> Option<f64> {
        if list.is_empty() {
            return None;
        }
        let hit = list
            .iter()
            .filter(|d| ranked.iter().any(|(a, _)| matches(a, d)))
            .count();
        Some(recall(hit, list.len()))
    };
    let tests =
        |key: &str| -> Vec<Decl> { arr_of(g, key).iter().filter_map(test_decl_of).collect() };
    let docs = arr_of(g, "docs");
    let docs_recall = (!docs.is_empty()).then(|| {
        let hit = docs
            .iter()
            .filter(|d| {
                let path = str_of(d, "path");
                let line = d.get("line").and_then(Value::as_i64);
                answers
                    .iter()
                    .any(|a| a.path.as_deref() == path && (a.line.is_none() || a.line == line))
            })
            .count();
        recall(hit, docs.len())
    });

    ItemScore {
        metrics: vec![
            ("decl_precision", Some(p)),
            ("decl_recall", Some(r)),
            ("decl_f1", Some(f1(p, r))),
            ("decl_recall@5", Some(recall_at(5))),
            ("decl_recall@10", Some(recall_at(10))),
            ("decl_recall@20", Some(recall_at(20))),
            ("decl_ndcg@10", Some(ndcg_at_k(&relevance, gold.len(), 10))),
            ("file_precision", Some(fp)),
            ("file_recall", Some(fr)),
            ("file_f1", Some(f1(fp, fr))),
            ("r2_decl_recall", covered(&decls(&g["r2"], "decls"))),
            ("tests_recall", covered(&tests("tests"))),
            ("tests_deep_recall", covered(&tests("tests_deep"))),
            ("tests_lexical_recall", covered(&tests("tests_lexical"))),
            ("docs_recall", docs_recall),
        ],
        dims: dims(item),
    }
}

pub fn oracle(item: &Value) -> Vec<Value> {
    let g = &item["gold"];
    let mut out: Vec<Value> = arr_of(&g["r1"], "decls")
        .iter()
        .map(|d| {
            json!({
                "path": d["path"], "line": d["line"], "name": d["name"], "symbol": d["symbol"],
            })
        })
        .collect();
    out.extend(
        strings_of(&g["r1"], "files")
            .into_iter()
            .map(|p| json!({ "path": p })),
    );
    out
}

/// `grep_baseline` stores metrics only (the returned declarations are not
/// kept), so the baseline is a metrics report, not a run.
pub fn stored_grep_baseline(items: &[Value]) -> Result<(Value, String)> {
    let mut agg = super::Aggregate::new();
    let mut with_field = 0usize;
    for item in items.iter().filter(|i| is_primary(i)) {
        let Some(b) = item.get("grep_baseline") else {
            continue;
        };
        with_field += 1;
        let num = |k: &str| b.get(k).and_then(Value::as_f64);
        agg.add(&ItemScore {
            metrics: vec![
                ("grep_decl_precision", num("decl_precision")),
                ("grep_decl_recall", num("decl_recall")),
                ("grep_decl_f1", num("decl_f1")),
                ("grep_file_precision", num("file_precision")),
                ("grep_file_recall", num("file_recall")),
                ("grep_file_f1", num("file_f1")),
                ("grep_decls_returned", num("decls_returned")),
            ],
            dims: dims(item),
        });
    }
    let (overall, strata) = agg.to_json();
    let report = json!({
        "set": "impact",
        "kind": "stored grep_baseline metrics (not a run file)",
        "scored_items": agg.n(),
        "primary_items_with_grep_baseline": with_field,
        "overall": overall,
        "strata": strata,
    });
    Ok((
        report,
        "impact: grep_baseline stores metrics only, not the returned declarations, so no run file can be built; \
         this report averages the stored per-item metrics in the layout `score` uses"
            .to_string(),
    ))
}

pub fn to_query(item: &Value) -> Result<Converted, Dropped> {
    if !is_primary(item) {
        return Err(Dropped("not a primary item (stem-leak or patch)"));
    }
    let expected = dedup_keep_order(strings_of(&item["gold"]["r1"], "files"));
    if expected.is_empty() {
        return Err(Dropped("no R1 files"));
    }
    let disfavored = dedup_keep_order(
        arr_of(&item["accept"], "decoys")
            .iter()
            .filter_map(|d| str_of(d, "path").map(str::to_string))
            .filter(|p| !expected.contains(p)),
    );
    let tags = dims(item)
        .into_iter()
        .map(|(dim, v)| format!("{dim}:{v}"))
        .collect();
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

    fn item() -> Value {
        json!({
            "id": "impact-t1", "primary": true,
            "stratum": "sym-fn/deg2-4/tests-near",
            "ambiguity": {"band": "some"},
            "seed": {"path": "a.rs", "line": 1, "span": [1, 3], "name": "T::seed", "symbol": "sym:seed"},
            "accept": {
                "neutral_decls": [{"path": "n.rs", "line": 5, "span": [5, 9], "name": "n", "symbol": "sym:n"}],
                "neutral_files": [], "decoys": [{"path": "d.rs"}]
            },
            "gold": {
                "r1": {
                    "decls": [
                        {"path": "a.rs", "line": 10, "span": [10, 20], "name": "m::one", "bare": "one", "symbol": "sym:one"},
                        {"path": "b.rs", "line": 30, "span": [30, 40], "name": "m::two", "bare": "two", "symbol": "sym:two"}
                    ],
                    "files": ["a.rs", "b.rs"]
                },
                "r2": {"decls": [{"path": "c.rs", "line": 1, "span": [1, 2], "name": "three", "symbol": "sym:three"}]},
                "tests": [{"decl": {"path": "t.rs", "line": 3, "span": [3, 8], "name": "t", "symbol": "sym:t"}}]
            }
        })
    }

    fn metric(s: &ItemScore, name: &str) -> Option<f64> {
        s.metrics.iter().find(|(n, _)| *n == name).unwrap().1
    }

    fn ans(s: &str) -> Answer {
        Answer::parse_shorthand(s)
    }

    #[test]
    fn decl_matching_uses_symbol_span_or_name() {
        let s = score_item(
            &item(),
            &[
                ans("a.rs:15"),
                Answer {
                    symbol: Some("sym:two".into()),
                    ..Default::default()
                },
                ans("n.rs:6"),
                ans("zzz.rs:1"),
            ],
        );
        // two correct, one neutral (skipped), one wrong
        assert!((metric(&s, "decl_precision").unwrap() - 2.0 / 3.0).abs() < 1e-9);
        assert_eq!(metric(&s, "decl_recall"), Some(1.0));
    }

    #[test]
    fn f1_recall_at_k_and_ndcg_follow_rank_order() {
        let s = score_item(&item(), &[ans("zzz.rs:1"), ans("a.rs:10"), ans("b.rs:31")]);
        assert_eq!(metric(&s, "decl_recall@5"), Some(1.0));
        assert_eq!(metric(&s, "decl_recall@10"), Some(1.0));
        let s1 = score_item(&item(), &[ans("zzz.rs:1"), ans("a.rs:10")]);
        assert_eq!(metric(&s1, "decl_recall@5"), Some(0.5));
        assert!(metric(&s, "decl_ndcg@10").unwrap() < 1.0);
        let p = 0.5;
        let r = 0.5;
        assert!((metric(&s1, "decl_f1").unwrap() - 2.0 * p * r / (p + r)).abs() < 1e-9);
    }

    #[test]
    fn files_and_secondary_lists_score_separately() {
        let s = score_item(
            &item(),
            &[ans("a.rs"), ans("x.rs"), ans("t.rs:4"), ans("c.rs:2")],
        );
        assert!((metric(&s, "file_precision").unwrap() - 0.25).abs() < 1e-9);
        assert!((metric(&s, "file_recall").unwrap() - 0.5).abs() < 1e-9);
        assert_eq!(metric(&s, "tests_recall"), Some(1.0));
        assert_eq!(metric(&s, "r2_decl_recall"), Some(1.0));
        assert_eq!(metric(&s, "tests_deep_recall"), None);
        let none = score_item(&item(), &[]);
        assert_eq!(metric(&none, "decl_f1"), Some(0.0));
    }

    #[test]
    fn oracle_scores_one_and_only_primary_converts() {
        let it = item();
        let run: Vec<Answer> = oracle(&it)
            .iter()
            .map(|v| Answer::from_value(v).unwrap())
            .collect();
        let s = score_item(&it, &run);
        assert_eq!(metric(&s, "decl_f1"), Some(1.0));
        assert_eq!(metric(&s, "file_f1"), Some(1.0));
        let q = to_query(&it).ok().unwrap();
        assert_eq!(q.expected, vec!["a.rs", "b.rs"]);
        assert_eq!(q.disfavored, vec!["d.rs"]);
        let mut non_primary = it;
        non_primary["primary"] = json!(false);
        assert!(to_query(&non_primary).is_err());
    }

    #[test]
    fn stored_baseline_averages_by_band() {
        let mut it = item();
        it["grep_baseline"] = json!({"decl_f1": 0.5, "decl_precision": 0.5, "decl_recall": 0.5,
            "file_f1": 1.0, "file_precision": 1.0, "file_recall": 1.0, "decls_returned": 4});
        let (report, _) = stored_grep_baseline(&[it]).unwrap();
        assert_eq!(report["scored_items"], 1);
        assert_eq!(
            report["strata"]["band"]["some"]["metrics"]["grep_decl_f1"],
            0.5
        );
    }
}
