//! `history` gold: "which commit, when, why". Primary metric is MRR on the
//! commit hash: H1 and H2 have one gold commit, H3 accepts the first hit
//! among `accept.mrr_commits`. Secondary: hit@1/2/3/5/10 and touched-file
//! recall against `gold.files` (with `accept.alt_files` as the same file
//! under its other name) when the answers carry `files`.
//!
//! Commit hashes match by prefix, at least seven hex digits on both sides.
//! Answers without a commit are not ranked.

use super::metrics::{hit_at_k, recall, reciprocal_rank};
use super::{
    Answer, BaselineKind, Converted, Dropped, ItemScore, dedup_keep_order, str_of, strings_of,
};
use anyhow::Result;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn is_hex(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn commit_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.to_ascii_lowercase(), b.to_ascii_lowercase());
    is_hex(&a)
        && is_hex(&b)
        && a.len() >= 7
        && b.len() >= 7
        && (a.starts_with(&b) || b.starts_with(&a))
}

/// Accepted commits for the item: `accept.mrr_commits` (H3) or `gold.commit`.
fn accepted(item: &Value) -> Vec<String> {
    let multi = strings_of(&item["accept"], "mrr_commits");
    if !multi.is_empty() {
        return multi;
    }
    str_of(&item["gold"], "commit")
        .map(|c| vec![c.to_string()])
        .unwrap_or_default()
}

fn family(item: &Value) -> String {
    str_of(item, "stratum")
        .unwrap_or_default()
        .split('/')
        .next()
        .unwrap_or("unknown")
        .to_string()
}

pub fn score_item(item: &Value, answers: &[Answer]) -> ItemScore {
    let accepted = accepted(item);
    let mut ranked: Vec<&str> = Vec::new();
    for a in answers {
        if let Some(c) = a.commit.as_deref()
            && !ranked.iter().any(|r| r.eq_ignore_ascii_case(c))
        {
            ranked.push(c);
        }
    }
    let first_rank = ranked
        .iter()
        .position(|c| accepted.iter().any(|g| commit_eq(c, g)))
        .map(|i| i + 1);

    let answered_files: BTreeSet<&str> = answers
        .iter()
        .flat_map(|a| a.files.iter().map(String::as_str))
        .collect();
    let file_recall = (!answered_files.is_empty()).then(|| {
        let alt = strings_of(&item["accept"], "alt_files");
        let gold = strings_of(&item["gold"], "files");
        let alt_hit = alt.iter().any(|f| answered_files.contains(f.as_str()));
        let found = gold
            .iter()
            .filter(|f| answered_files.contains(f.as_str()) || alt_hit)
            .count();
        recall(found, gold.len())
    });

    ItemScore {
        metrics: vec![
            ("mrr", Some(reciprocal_rank(first_rank, usize::MAX))),
            ("hit@1", Some(hit_at_k(first_rank, 1))),
            ("hit@2", Some(hit_at_k(first_rank, 2))),
            ("hit@3", Some(hit_at_k(first_rank, 3))),
            ("hit@5", Some(hit_at_k(first_rank, 5))),
            ("hit@10", Some(hit_at_k(first_rank, 10))),
            ("file_recall", file_recall),
        ],
        dims: vec![("family", family(item))],
    }
}

pub fn oracle(item: &Value) -> Vec<Value> {
    accepted(item)
        .into_iter()
        .take(1)
        .map(|c| json!({"commit": c, "files": strings_of(&item["gold"], "files")}))
        .collect()
}

/// Best constant commit per family (the commit accepted for the most items;
/// ties go to the commit seen first, like a stable most-common count).
fn best_constant(items: &[&Value]) -> Option<String> {
    let mut order: Vec<String> = Vec::new();
    let mut count: BTreeMap<String, usize> = BTreeMap::new();
    for item in items {
        for c in accepted(item) {
            if !count.contains_key(&c) {
                order.push(c.clone());
            }
            *count.entry(c).or_default() += 1;
        }
    }
    let mut best: Option<(&String, usize)> = None;
    for c in &order {
        let n = count[c];
        if best.is_none_or(|(_, bn)| n > bn) {
            best = Some((c, n));
        }
    }
    best.map(|(c, _)| c.clone())
}

fn item_family_key(item: &Value) -> &'static str {
    let stratum = str_of(item, "stratum").unwrap_or_default();
    if stratum.starts_with("H1") {
        "H1"
    } else if stratum.starts_with("H2") {
        "H2"
    } else {
        "H3"
    }
}

pub fn baseline(items: &[Value], kind: BaselineKind) -> Result<(Value, String)> {
    let mut constants: BTreeMap<&'static str, Option<String>> = BTreeMap::new();
    for fam in ["H1", "H2", "H3"] {
        let members: Vec<&Value> = items.iter().filter(|i| item_family_key(i) == fam).collect();
        constants.insert(fam, best_constant(&members));
    }
    let mut run = serde_json::Map::new();
    for item in items {
        let id = str_of(item, "id").unwrap_or_default().to_string();
        let fam = item_family_key(item);
        let answers: Vec<Value> = if fam == "H2" && kind == BaselineKind::Shipped {
            // Reconstructed from `accept.file_log_rank`: the gold commit sits at
            // that rank of `git log <as_of> -- <path>`; the other entries are
            // placeholders that match no commit.
            let rank = item["accept"]["file_log_rank"].as_u64();
            let slots = rank.unwrap_or(10) as usize;
            let mut list: Vec<Value> = (1..slots)
                .map(|i| json!({"commit": format!("file-log-placeholder-{i}")}))
                .collect();
            if rank.is_some() {
                list.extend(
                    accepted(item)
                        .into_iter()
                        .take(1)
                        .map(|c| json!({"commit": c})),
                );
            } else {
                list.push(json!({"commit": "file-log-placeholder-miss"}));
            }
            list
        } else {
            constants[fam]
                .iter()
                .map(|c| json!({"commit": c}))
                .collect()
        };
        run.insert(id, Value::Array(answers));
    }
    let note = match kind {
        BaselineKind::Constant => {
            "history: one constant commit per family (H1, H2, H3a/H3b share H3's)".to_string()
        }
        _ => "history: H1 and H3 = best constant commit; H2 = the file-log shortcut rebuilt from \
              accept.file_log_rank (placeholders around the gold commit). The H1 'declaring file was \
              created by the gold commit' shortcut needs git and cannot be rebuilt from the gold file"
            .to_string(),
    };
    Ok((Value::Object(run), note))
}

pub fn to_query(item: &Value) -> Result<Converted, Dropped> {
    let mut files = strings_of(&item["gold"], "files");
    files.extend(strings_of(&item["accept"], "alt_files"));
    let expected = dedup_keep_order(files);
    if expected.is_empty() {
        return Err(Dropped("no gold files"));
    }
    Ok(Converted {
        expected,
        preferred: Vec::new(),
        disfavored: Vec::new(),
        tags: vec![format!("family:{}", family(item))],
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const C1: &str = "1111111aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const C2: &str = "2222222bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C3: &str = "3333333ccccccccccccccccccccccccccccccccc";

    fn item(stratum: &str, commits: &[&str]) -> Value {
        let mut it = json!({
            "id": format!("history-{}-t", stratum.to_lowercase()), "stratum": stratum,
            "gold": {"commit": commits[0], "files": ["a.rs"]},
            "accept": {"alt_files": ["moved/a.rs"]}
        });
        if stratum.starts_with("H3") {
            it["accept"]["mrr_commits"] = json!(commits);
        }
        it
    }

    fn run(commits: &[&str]) -> Vec<Answer> {
        commits.iter().map(|c| Answer::parse_shorthand(c)).collect()
    }

    fn metric(s: &ItemScore, name: &str) -> Option<f64> {
        s.metrics.iter().find(|(m, _)| *m == name).unwrap().1
    }

    #[test]
    fn mrr_uses_the_rank_of_the_first_accepted_commit() {
        let it = item("H1/tercile1/fn", &[C1]);
        assert_eq!(metric(&score_item(&it, &run(&[C1])), "mrr"), Some(1.0));
        assert_eq!(
            metric(&score_item(&it, &run(&[C2, C3, C1])), "mrr").map(|x| (x * 1000.0).round()),
            Some(333.0)
        );
        assert_eq!(metric(&score_item(&it, &run(&[C2])), "mrr"), Some(0.0));
        // a seven-digit prefix is enough
        assert_eq!(
            metric(&score_item(&it, &run(&["1111111"])), "mrr"),
            Some(1.0)
        );
        // H3 accepts any listed commit
        let h3 = item("H3a", &[C1, C2]);
        assert_eq!(metric(&score_item(&h3, &run(&[C3, C2])), "mrr"), Some(0.5));
    }

    #[test]
    fn hit_at_k_follows_the_commit_rank() {
        let it = item("H2/fn/changes2", &[C1]);
        let s = score_item(&it, &run(&[C2, C3, C1]));
        assert_eq!(metric(&s, "hit@1"), Some(0.0));
        assert_eq!(metric(&s, "hit@3"), Some(1.0));
        assert_eq!(metric(&s, "hit@10"), Some(1.0));
    }

    #[test]
    fn file_recall_accepts_the_alternate_path_and_needs_files() {
        let it = item("H1/tercile1/fn", &[C1]);
        assert_eq!(metric(&score_item(&it, &run(&[C1])), "file_recall"), None);
        let with = |files: &[&str]| {
            vec![Answer {
                commit: Some(C1.into()),
                files: files.iter().map(|f| f.to_string()).collect(),
                ..Default::default()
            }]
        };
        assert_eq!(
            metric(&score_item(&it, &with(&["a.rs"])), "file_recall"),
            Some(1.0)
        );
        assert_eq!(
            metric(&score_item(&it, &with(&["moved/a.rs"])), "file_recall"),
            Some(1.0)
        );
        assert_eq!(
            metric(&score_item(&it, &with(&["z.rs"])), "file_recall"),
            Some(0.0)
        );
    }

    #[test]
    fn baseline_rebuilds_the_file_log_rank_and_constants() {
        let mut h2 = item("H2/fn/changes2", &[C1]);
        h2["accept"]["file_log_rank"] = json!(2);
        let items = vec![
            item("H1/tercile1/fn", &[C2]),
            h2.clone(),
            item("H3a", &[C3, C2]),
        ];
        let (shipped, _) = baseline(&items, BaselineKind::Shipped).unwrap();
        let h2_id = h2["id"].as_str().unwrap();
        assert_eq!(shipped[h2_id].as_array().unwrap().len(), 2);
        let report = super::super::score_set(
            super::super::Set::History,
            &items,
            &super::super::parse_run(&shipped.to_string()).unwrap(),
            false,
            false,
        )
        .unwrap();
        assert_eq!(report["strata"]["family"]["H2"]["metrics"]["hit@3"], 1.0);
        assert_eq!(report["strata"]["family"]["H2"]["metrics"]["hit@1"], 0.0);
        let (constant, _) = baseline(&items, BaselineKind::Constant).unwrap();
        assert_eq!(constant[h2_id].as_array().unwrap().len(), 1);
    }

    #[test]
    fn oracle_scores_one_and_converts_with_alt_files() {
        let it = item("H1/tercile1/fn", &[C1]);
        let answers: Vec<Answer> = oracle(&it)
            .iter()
            .map(|v| Answer::from_value(v).unwrap())
            .collect();
        let s = score_item(&it, &answers);
        assert_eq!(metric(&s, "mrr"), Some(1.0));
        assert_eq!(metric(&s, "file_recall"), Some(1.0));
        assert_eq!(
            to_query(&it).ok().unwrap().expected,
            vec!["a.rs", "moved/a.rs"]
        );
    }
}
