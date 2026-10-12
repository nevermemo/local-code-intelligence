//! `cargo xtask bench compare`: pair two results files and apply the
//! pre-registered rules of `docs/spike/multimap/bench.md`, section 6.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde_json::Value;

use super::run::RESULTS_SCHEMA;
use super::score::{Interval, bootstrap, percentile};

pub const DEFAULT_SEED: u64 = 42;
pub const DEFAULT_RESAMPLES: usize = 10_000;
/// Rule 5: fewer paired tasks than this give no quality verdict.
pub const MIN_PAIRS: usize = 30;
/// Rule 3: total index size may grow by at most this fraction.
const SIZE_GROWTH: f64 = 0.10;
/// Rule 4: p50 cost may grow by at most this fraction (same machine only).
const COST_GROWTH: f64 = 0.25;

pub fn load(path: &Path) -> Result<Value> {
    let value: Value = serde_json::from_str(
        &std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    )
    .with_context(|| format!("parsing {}", path.display()))?;
    ensure!(
        value["schema"] == RESULTS_SCHEMA,
        "{} is not a bench results file",
        path.display()
    );
    Ok(value)
}

/// Score of one task in one arm, or `None` when it errored or is absent.
fn arm_row<'a>(results: &'a Value, id: &str, arm: &str) -> Option<&'a Value> {
    results["tasks"]
        .as_array()?
        .iter()
        .find(|task| task["id"] == id)
        .and_then(|task| task["arms"].get(arm))
}

fn scored(row: Option<&Value>) -> bool {
    row.is_some_and(|row| row.get("error").is_none() && row["rr"].is_number())
}

fn metric(row: &Value, name: &str) -> f64 {
    match &row[name] {
        Value::Bool(b) => f64::from(u8::from(*b)),
        value => value.as_f64().unwrap_or(0.0),
    }
}

fn task_ids(results: &Value) -> Vec<String> {
    results["tasks"]
        .as_array()
        .map(|tasks| {
            tasks
                .iter()
                .filter_map(|task| task["id"].as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn arms(results: &Value) -> Vec<String> {
    results["arms"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

/// One arm's paired comparison.
#[derive(Debug)]
pub struct ArmComparison {
    pub arm: String,
    pub pairs: usize,
    pub base_only: usize,
    pub new_only: usize,
    /// Scored on base, errored on new (rule 2).
    pub new_errors: Vec<String>,
    pub base_errors: usize,
    pub wins: usize,
    pub losses: usize,
    pub ties: usize,
    /// metric -> (base mean, new mean, interval of new minus base).
    pub metrics: BTreeMap<&'static str, (f64, f64, Option<Interval>)>,
    pub identical_files: usize,
}

pub fn compare_arm(
    base: &Value,
    new: &Value,
    arm: &str,
    seed: u64,
    resamples: usize,
) -> ArmComparison {
    let base_ids = task_ids(base);
    let new_ids = task_ids(new);
    let mut out = ArmComparison {
        arm: arm.to_owned(),
        pairs: 0,
        base_only: base_ids.iter().filter(|id| !new_ids.contains(id)).count(),
        new_only: new_ids.iter().filter(|id| !base_ids.contains(id)).count(),
        new_errors: Vec::new(),
        base_errors: 0,
        wins: 0,
        losses: 0,
        ties: 0,
        metrics: BTreeMap::new(),
        identical_files: 0,
    };
    let mut pairs: Vec<(&Value, &Value)> = Vec::new();
    for id in base_ids.iter().filter(|id| new_ids.contains(id)) {
        let (b, n) = (arm_row(base, id, arm), arm_row(new, id, arm));
        match (scored(b), scored(n)) {
            (true, true) => pairs.push((b.unwrap(), n.unwrap())),
            (true, false) => out.new_errors.push(id.clone()),
            (false, _) => out.base_errors += 1,
        }
    }
    out.pairs = pairs.len();
    for (b, n) in &pairs {
        let delta = metric(n, "rr") - metric(b, "rr");
        if delta > 0.0 {
            out.wins += 1;
        } else if delta < 0.0 {
            out.losses += 1;
        } else {
            out.ties += 1;
        }
        if b["files"] == n["files"] {
            out.identical_files += 1;
        }
    }
    for name in ["rr", "hit10", "recall10"] {
        let base_values: Vec<f64> = pairs.iter().map(|(b, _)| metric(b, name)).collect();
        let deltas: Vec<f64> = pairs
            .iter()
            .map(|(b, n)| metric(n, name) - metric(b, name))
            .collect();
        let mean = |values: &[f64]| {
            if values.is_empty() {
                0.0
            } else {
                values.iter().sum::<f64>() / values.len() as f64
            }
        };
        let new_values: Vec<f64> = pairs.iter().map(|(_, n)| metric(n, name)).collect();
        out.metrics.insert(
            name,
            (
                mean(&base_values),
                mean(&new_values),
                bootstrap(&deltas, resamples, seed),
            ),
        );
    }
    out
}

/// Values of `field` (a number) of `section` across tasks, skipping errors.
fn task_values(results: &Value, section: &str, field: &str) -> Vec<f64> {
    results["tasks"]
        .as_array()
        .map(|tasks| {
            tasks
                .iter()
                .filter_map(|task| task.get(section)?.get(field)?.as_f64())
                .collect()
        })
        .unwrap_or_default()
}

fn arm_latencies(results: &Value, arm: &str) -> Vec<f64> {
    results["tasks"]
        .as_array()
        .map(|tasks| {
            tasks
                .iter()
                .filter_map(|task| task["arms"].get(arm)?["total_ms"].as_f64())
                .collect()
        })
        .unwrap_or_default()
}

/// Tasks whose row in `arm` carries a fail-open warning.
fn warnings(results: &Value, arm: &str) -> usize {
    results["tasks"].as_array().map_or(0, |tasks| {
        tasks
            .iter()
            .filter(|task| {
                task["arms"]
                    .get(arm)
                    .is_some_and(|row| row["warning"].is_string())
            })
            .count()
    })
}

fn total_size(results: &Value) -> Option<f64> {
    let repos = results["repos"].as_array()?;
    let sizes: Vec<f64> = repos
        .iter()
        .filter_map(|repo| repo["index"]["size_bytes"].as_f64())
        .collect();
    (!sizes.is_empty() && sizes.len() == repos.len()).then(|| sizes.iter().sum())
}

fn lookup_means(results: &Value) -> Option<(f64, f64, usize)> {
    let rows: Vec<&Value> = results["tasks"]
        .as_array()?
        .iter()
        .filter_map(|task| task["lookup"].as_array())
        .flatten()
        .filter(|row| row.get("error").is_none() && row["precision5"].is_number())
        .collect();
    if rows.is_empty() {
        return None;
    }
    let n = rows.len() as f64;
    let recall = rows.iter().filter(|r| r["hit5"] == true).count() as f64 / n;
    let precision = rows.iter().map(|r| metric(r, "precision5")).sum::<f64>() / n;
    Some((recall, precision, rows.len()))
}

pub struct Verdict {
    pub table: String,
    pub regressions: Vec<String>,
}

fn fmt_interval(interval: Option<Interval>) -> String {
    interval.map_or_else(
        || "n/a".to_owned(),
        |i| format!("{:+.4} [{:+.4}, {:+.4}]", i.mean, i.low, i.high),
    )
}

fn growth(base: f64, new: f64) -> f64 {
    if base > 0.0 { new / base - 1.0 } else { 0.0 }
}

pub fn compare(base: &Value, new: &Value, seed: u64, resamples: usize) -> Verdict {
    let mut table = String::new();
    let mut regressions = Vec::new();
    let mut notes = Vec::new();
    let build = |r: &Value| {
        format!(
            "{} ({})",
            r["build"]["commit"]
                .as_str()
                .map_or("unknown", |c| &c[..c.len().min(12)]),
            r["label"].as_str().unwrap_or("-")
        )
    };
    let _ = writeln!(table, "## Bench comparison\n");
    let _ = writeln!(
        table,
        "Base: {} at {}; new: {} at {}. Bench set {} / {}. Embedder: {} / {}.\n",
        build(base),
        base["started_at"].as_str().unwrap_or("?"),
        build(new),
        new["started_at"].as_str().unwrap_or("?"),
        base["bench_set"]["version"],
        new["bench_set"]["version"],
        base["embedder"]["model"],
        new["embedder"]["model"],
    );
    if base["bench_set"]["version"] != new["bench_set"]["version"] {
        notes.push("bench set versions differ: the pairs are not the same tasks".to_owned());
    }
    if base["embedder"]["model"] != new["embedder"]["model"] {
        notes.push("embedders differ: a quality delta mixes builds and embedders".to_owned());
    }

    let _ = writeln!(
        table,
        "| Arm | Pairs | Errored new / base | Unpaired base / new | Same files | W/L/T (rr) | Metric | Base | New | Delta new-base [95% CI] |"
    );
    let _ = writeln!(
        table,
        "| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |"
    );
    let new_arms = arms(new);
    for arm in arms(base).iter().filter(|a| new_arms.contains(a)) {
        let c = compare_arm(base, new, arm, seed, resamples);
        for (index, name) in ["rr", "hit10", "recall10"].iter().enumerate() {
            let (b, n, interval) = c.metrics[name];
            let lead = if index == 0 {
                format!(
                    "| {} | {} | {} / {} | {} / {} | {} | {}/{}/{} ",
                    c.arm,
                    c.pairs,
                    c.new_errors.len(),
                    c.base_errors,
                    c.base_only,
                    c.new_only,
                    c.identical_files,
                    c.wins,
                    c.losses,
                    c.ties
                )
            } else {
                "| | | | | | ".to_owned()
            };
            let _ = writeln!(
                table,
                "{lead}| {name}{} | {b:.4} | {n:.4} | {} |",
                if index == 0 { " (primary)" } else { "" },
                fmt_interval(interval)
            );
        }
        let (warned_base, warned_new) = (warnings(base, arm), warnings(new, arm));
        if warned_base + warned_new > 0 {
            notes.push(format!(
                "{arm}: degraded-channel warnings on {warned_base} base / {warned_new} new tasks"
            ));
        }
        if !c.new_errors.is_empty() {
            regressions.push(format!(
                "rule 2 ({}): scored on base, errored on new: {}",
                c.arm,
                c.new_errors.join(", ")
            ));
        }
        // Rule 1 judges the fusion arm only; grep is reported, never a gate.
        if c.arm == "fusion" {
            let interval = c.metrics["rr"].2;
            if c.pairs < MIN_PAIRS {
                notes.push(format!(
                    "rule 5 (fusion): {} pairs < {MIN_PAIRS}: insufficient data for a quality verdict",
                    c.pairs
                ));
            } else if let Some(i) = interval {
                if i.high < 0.0 {
                    regressions.push(format!(
                        "rule 1 (fusion): mean rr delta {:+.4}, 95% CI [{:+.4}, {:+.4}] entirely below zero",
                        i.mean, i.low, i.high
                    ));
                } else if i.low > 0.0 {
                    let clean = c.new_errors.is_empty()
                        && c.base_errors == 0
                        && c.base_only == 0
                        && c.new_only == 0;
                    notes.push(format!(
                        "rule 1 mirrored (fusion): improvement {:+.4} [{:+.4}, {:+.4}], W/L/T {}/{}/{}{}",
                        i.mean,
                        i.low,
                        i.high,
                        c.wins,
                        c.losses,
                        c.ties,
                        if clean { "" } else { " -- not claimable: errored or unpaired tasks" }
                    ));
                }
            }
        }
    }

    // Level B.
    let _ = writeln!(table, "\n| Level B | Base | New | Change |");
    let _ = writeln!(table, "| --- | --- | --- | --- |");
    let same_machine = base["machine"] == new["machine"];
    if let (Some(b), Some(n)) = (total_size(base), total_size(new)) {
        let g = growth(b, n);
        let _ = writeln!(
            table,
            "| index size (bytes, all repos) | {b:.0} | {n:.0} | {:+.1}% |",
            g * 100.0
        );
        if g > SIZE_GROWTH {
            regressions.push(format!(
                "rule 3: index size grew {:+.1}% (> 10%)",
                g * 100.0
            ));
        }
    }
    let costs = [
        (
            "update p50 (ms)",
            task_values(base, "update", "ms"),
            task_values(new, "update", "ms"),
        ),
        (
            "fusion total_ms p50",
            arm_latencies(base, "fusion"),
            arm_latencies(new, "fusion"),
        ),
        (
            "grep total_ms p50",
            arm_latencies(base, "grep"),
            arm_latencies(new, "grep"),
        ),
    ];
    for (index, (name, b, n)) in costs.iter().enumerate() {
        if let (Some(pb), Some(pn)) = (percentile(b, 50.0), percentile(n, 50.0)) {
            let g = growth(pb, pn);
            let _ = writeln!(table, "| {name} | {pb:.1} | {pn:.1} | {:+.1}% |", g * 100.0);
            // Rule 4 judges update and the fusion arm, on one machine only.
            if index < 2 && same_machine && g > COST_GROWTH {
                regressions.push(format!(
                    "rule 4: {name} grew {:+.1}% (> 25%, same machine)",
                    g * 100.0
                ));
            }
        }
    }
    if let (Some(b), Some(n)) = (lookup_means(base), lookup_means(new)) {
        let _ = writeln!(
            table,
            "| lookup recall@5 / precision@5 (proxy, n={} / {}) | {:.3} / {:.3} | {:.3} / {:.3} | reported only |",
            b.2, n.2, b.0, b.1, n.0, n.1
        );
    }
    if !same_machine {
        notes.push("machines differ: costs are printed, not judged (rule 4)".to_owned());
    }
    let _ = writeln!(table, "\n### Verdict\n");
    if regressions.is_empty() {
        let _ = writeln!(table, "No regression under the pre-registered rules.");
    } else {
        for line in &regressions {
            let _ = writeln!(table, "- REGRESSION {line}");
        }
    }
    for line in &notes {
        let _ = writeln!(table, "- note: {line}");
    }
    Verdict { table, regressions }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn results(rr: &[f64], machine: &str, size: f64) -> Value {
        let tasks: Vec<Value> = rr
            .iter()
            .enumerate()
            .map(|(i, rr)| {
                json!({
                    "id": format!("t{i}"),
                    "arms": {
                        "fusion": {"files": ["a"], "rr": rr, "hit10": *rr > 0.0, "recall10": rr, "total_ms": 10.0},
                        "grep": {"files": ["a"], "rr": 0.0, "hit10": false, "recall10": 0.0, "total_ms": 5.0},
                    },
                    "update": {"ms": 100.0},
                })
            })
            .collect();
        json!({
            "schema": RESULTS_SCHEMA,
            "bench_set": {"version": "v1"},
            "embedder": {"model": "fake"},
            "machine": {"cpu": machine},
            "arms": ["fusion", "grep"],
            "repos": [{"index": {"size_bytes": size}}],
            "tasks": tasks,
        })
    }

    #[test]
    fn identical_results_are_all_ties_and_no_regression() {
        let base = results(&[1.0; 40], "m", 100.0);
        let verdict = compare(&base, &base, 42, 1000);
        assert!(verdict.regressions.is_empty(), "{}", verdict.table);
        let c = compare_arm(&base, &base, "fusion", 42, 1000);
        assert_eq!((c.pairs, c.ties, c.identical_files), (40, 40, 40));
    }

    #[test]
    fn a_clear_rr_drop_is_a_rule_1_regression_but_grep_never_is() {
        let base = results(&[1.0; 40], "m", 100.0);
        let new = results(&[0.5; 40], "m", 100.0);
        let verdict = compare(&base, &new, 42, 1000);
        assert_eq!(verdict.regressions.len(), 1, "{}", verdict.table);
        assert!(verdict.regressions[0].starts_with("rule 1 (fusion)"));
        // A grep-only drop: base grep rr 1.0, new grep rr 0.0, fusion unchanged.
        let mut grep_base = base.clone();
        for task in grep_base["tasks"].as_array_mut().unwrap() {
            task["arms"]["grep"]["rr"] = json!(1.0);
        }
        assert!(compare(&grep_base, &base, 42, 1000).regressions.is_empty());
    }

    #[test]
    fn too_few_pairs_give_no_quality_verdict() {
        let base = results(&[1.0; 10], "m", 100.0);
        let new = results(&[0.0; 10], "m", 100.0);
        let verdict = compare(&base, &new, 42, 1000);
        assert!(verdict.regressions.is_empty(), "{}", verdict.table);
        assert!(verdict.table.contains("insufficient data"));
    }

    #[test]
    fn a_new_error_is_a_coverage_regression_and_is_not_paired() {
        let base = results(&[1.0; 40], "m", 100.0);
        let mut new = base.clone();
        new["tasks"][3]["arms"]["fusion"] = json!({"error": "boom"});
        let c = compare_arm(&base, &new, "fusion", 42, 1000);
        assert_eq!((c.pairs, c.new_errors.len()), (39, 1));
        let verdict = compare(&base, &new, 42, 1000);
        assert!(verdict.regressions.iter().any(|r| r.starts_with("rule 2")));
    }

    #[test]
    fn size_is_judged_everywhere_and_cost_only_on_one_machine() {
        let base = results(&[1.0; 40], "m", 100.0);
        let mut new = results(&[1.0; 40], "m", 111.0);
        for task in new["tasks"].as_array_mut().unwrap() {
            task["update"]["ms"] = json!(200.0);
        }
        let verdict = compare(&base, &new, 42, 1000);
        assert!(verdict.regressions.iter().any(|r| r.starts_with("rule 3")));
        assert!(verdict.regressions.iter().any(|r| r.starts_with("rule 4")));
        new["machine"] = json!({"cpu": "other"});
        let verdict = compare(&base, &new, 42, 1000);
        assert!(!verdict.regressions.iter().any(|r| r.starts_with("rule 4")));
        assert!(verdict.regressions.iter().any(|r| r.starts_with("rule 3")));
    }
}
