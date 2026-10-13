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

/// Rule 4's "back to back": both runs started within this many seconds.
const BACK_TO_BACK_SECONDS: i64 = 6 * 3600;

/// Seconds since the epoch of a `YYYY-MM-DDTHH:MM:SSZ` timestamp.
fn epoch_seconds(text: &str) -> Option<i64> {
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    if text.len() != 20 || !text.ends_with('Z') {
        return None;
    }
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    // Days from civil (proleptic Gregorian).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

fn back_to_back(base: &Value, new: &Value) -> bool {
    match (
        base["started_at"].as_str().and_then(epoch_seconds),
        new["started_at"].as_str().and_then(epoch_seconds),
    ) {
        (Some(b), Some(n)) => (n - b).abs() <= BACK_TO_BACK_SECONDS,
        _ => false,
    }
}

/// Per-repository and per-tag mean `rr` of the paired fusion tasks, with n;
/// descriptive only, never a verdict.
fn breakdown(table: &mut String, base: &Value, new: &Value) {
    let mut groups: BTreeMap<String, (usize, f64, f64)> = BTreeMap::new();
    let Some(tasks) = base["tasks"].as_array() else {
        return;
    };
    for task in tasks {
        let id = task["id"].as_str().unwrap_or_default();
        let (b, n) = (arm_row(base, id, "fusion"), arm_row(new, id, "fusion"));
        if !(scored(b) && scored(n)) {
            continue;
        }
        let (rb, rn) = (metric(b.unwrap(), "rr"), metric(n.unwrap(), "rr"));
        let mut keys = vec![format!("repo {}", task["repo"].as_str().unwrap_or("?"))];
        keys.extend(
            task["tags"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|tag| tag.as_str().map(|tag| format!("tag {tag}"))),
        );
        for key in keys {
            let entry = groups.entry(key).or_default();
            entry.0 += 1;
            entry.1 += rb;
            entry.2 += rn;
        }
    }
    if groups.is_empty() {
        return;
    }
    let _ = writeln!(
        table,
        "\n| fusion rr by group (descriptive) | n | Base | New |"
    );
    let _ = writeln!(table, "| --- | --- | --- | --- |");
    for (key, (n, rb, rn)) in &groups {
        let count = *n as f64;
        let _ = writeln!(
            table,
            "| {key} | {n} | {:.4} | {:.4} |",
            rb / count,
            rn / count
        );
    }
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

/// Level B `defs` (the `sym.defs` probe): (hit@5, precision@5, scored
/// names, errored names), over the tasks of `repo` or all; a failed probe
/// without per-name rows counts once. `None` when the results carry no
/// probe.
fn defs_means(results: &Value, repo: Option<&str>) -> Option<(f64, f64, usize, usize)> {
    let tasks = results["tasks"].as_array()?;
    if !tasks
        .iter()
        .any(|task| task["defs"].is_array() || !task["defs_build"].is_null())
    {
        return None;
    }
    let tasks: Vec<&Value> = tasks
        .iter()
        .filter(|task| repo.is_none_or(|repo| task["repo"] == repo))
        .collect();
    let rows: Vec<&Value> = tasks
        .iter()
        .filter_map(|task| task["defs"].as_array())
        .flatten()
        .collect();
    let scored: Vec<&Value> = rows
        .iter()
        .copied()
        .filter(|row| row.get("error").is_none() && row["precision5"].is_number())
        .collect();
    let failed = tasks
        .iter()
        .filter(|task| {
            // Without per-name rows (an older file, or no gold names) a
            // failed probe counts once.
            task["defs"].as_array().is_none_or(Vec::is_empty)
                && task["defs_build"].get("error").is_some()
        })
        .count();
    let errors = rows.len() - scored.len() + failed;
    let n = scored.len();
    if n == 0 {
        return Some((0.0, 0.0, 0, errors));
    }
    let hit = scored.iter().filter(|r| r["hit5"] == true).count() as f64 / n as f64;
    let precision = scored.iter().map(|r| metric(r, "precision5")).sum::<f64>() / n as f64;
    Some((hit, precision, n, errors))
}

/// One side of the `defs` row: its means, or why there are none.
fn fmt_defs(side: Option<(f64, f64, usize, usize)>) -> String {
    side.map_or_else(
        || "unavailable".to_owned(),
        |(hit, precision, n, errors)| {
            if n == 0 {
                format!("no scored names (errors={errors})")
            } else {
                format!("{hit:.3} / {precision:.3} (n={n}, errors={errors})")
            }
        },
    )
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
    let back_to_back = back_to_back(base, new);
    let judge_cost = same_machine && back_to_back;
    let size = (total_size(base), total_size(new));
    if size.0.is_none() || size.1.is_none() {
        notes.push("index size: not comparable (a repository has no size on one side)".to_owned());
    }
    if let (Some(b), Some(n)) = size {
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
            for (label, p) in [("p95", 95.0), ("max", 100.0)] {
                if let (Some(xb), Some(xn)) = (percentile(b, p), percentile(n, p)) {
                    let _ = writeln!(
                        table,
                        "| {} | {xb:.1} | {xn:.1} | {:+.1}% |",
                        name.replace("p50", label),
                        growth(xb, xn) * 100.0
                    );
                }
            }
            // Rule 4 judges update and the fusion arm, on one machine, back
            // to back only.
            if index < 2 && judge_cost && g > COST_GROWTH {
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
    let (defs_base, defs_new) = (defs_means(base, None), defs_means(new, None));
    if defs_base.is_some() || defs_new.is_some() {
        let _ = writeln!(
            table,
            "| defs hit@5 / precision@5 (sym.defs probe) | {} | {} | reported only |",
            fmt_defs(defs_base),
            fmt_defs(defs_new)
        );
        let mut repos: Vec<&str> = [base, new]
            .iter()
            .filter_map(|results| results["tasks"].as_array())
            .flatten()
            .filter_map(|task| task["repo"].as_str())
            .collect();
        repos.sort_unstable();
        repos.dedup();
        for repo in repos {
            let _ = writeln!(
                table,
                "| defs {repo} | {} | {} | reported only |",
                fmt_defs(defs_means(base, Some(repo))),
                fmt_defs(defs_means(new, Some(repo)))
            );
        }
        for (side, results) in [("base", base), ("new", new)] {
            let probe = &results["defs_probe"];
            if probe.is_object() && probe["commit"] != results["build"]["commit"] {
                notes.push(format!(
                    "{side}: the defs probe is not frozen from the build's commit ({} against {})",
                    probe["commit"], results["build"]["commit"]
                ));
            }
        }
    }
    if !same_machine {
        notes.push("machines differ: costs are printed, not judged (rule 4)".to_owned());
    } else if !back_to_back {
        notes.push(format!(
            "runs started more than {} h apart: costs are printed, not judged (rule 4)",
            BACK_TO_BACK_SECONDS / 3600
        ));
    }
    breakdown(&mut table, base, new);
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
            "started_at": "2026-10-12T17:00:00Z",
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
        // Same machine but a day apart: printed, not judged.
        let mut later = new.clone();
        later["started_at"] = json!("2026-10-13T17:00:01Z");
        let verdict = compare(&base, &later, 42, 1000);
        assert!(!verdict.regressions.iter().any(|r| r.starts_with("rule 4")));
        new["machine"] = json!({"cpu": "other"});
        let verdict = compare(&base, &new, 42, 1000);
        assert!(!verdict.regressions.iter().any(|r| r.starts_with("rule 4")));
        assert!(verdict.regressions.iter().any(|r| r.starts_with("rule 3")));
    }

    #[test]
    fn an_improvement_is_claimable_only_without_errors_or_unpaired_tasks() {
        let base = results(&[0.5; 40], "m", 100.0);
        let new = results(&[1.0; 40], "m", 100.0);
        let verdict = compare(&base, &new, 42, 1000);
        assert!(verdict.regressions.is_empty());
        assert!(
            verdict
                .table
                .contains("rule 1 mirrored (fusion): improvement")
        );
        assert!(
            !verdict.table.contains("not claimable"),
            "{}",
            verdict.table
        );
        let mut short = new.clone();
        short["tasks"].as_array_mut().unwrap().pop();
        let mut base_more = base.clone();
        base_more["tasks"][0]["arms"]["fusion"] = json!({"error": "boom"});
        let verdict = compare(&base_more, &short, 42, 1000);
        assert!(verdict.table.contains("not claimable"), "{}", verdict.table);
    }

    #[test]
    fn defs_are_reported_beside_an_unavailable_side_and_never_judged() {
        let base = results(&[0.5; 40], "m", 100.0);
        let mut new = results(&[0.5; 40], "m", 100.0);
        let tasks = new["tasks"].as_array_mut().unwrap();
        tasks[0]["defs"] = json!([
            {"name": "a", "hit5": true, "precision5": 0.5},
            {"name": "b", "hit5": false, "precision5": 0.0},
            {"name": "c", "error": "name missing from the probe report"},
        ]);
        tasks[1]["defs"] = json!([{"name": "d", "hit5": true, "precision5": 1.0}]);
        tasks[2]["defs_build"] = json!({"error": "probe failed"});
        tasks[0]["repo"] = json!("tokio");
        tasks[1]["repo"] = json!("caddy");
        tasks[2]["repo"] = json!("caddy");
        let verdict = compare(&base, &new, 7, 200);
        // Per repository too: a map may cover one language only.
        let repo_row = |repo: &str| {
            verdict
                .table
                .lines()
                .find(|line| line.starts_with(&format!("| defs {repo} ")))
                .unwrap_or_else(|| panic!("no defs {repo} row in\n{}", verdict.table))
                .to_owned()
        };
        assert!(repo_row("tokio").contains("0.500 / 0.250 (n=2, errors=1)"));
        assert!(repo_row("caddy").contains("1.000 / 1.000 (n=1, errors=1)"));
        let row = verdict
            .table
            .lines()
            .find(|line| line.starts_with("| defs "))
            .unwrap_or_else(|| panic!("no defs row in\n{}", verdict.table));
        assert!(row.contains("| unavailable |"), "{row}");
        assert!(row.contains("0.667 / 0.500 (n=3, errors=2)"), "{row}");
        assert!(row.ends_with("| reported only |"), "{row}");
        assert!(verdict.regressions.is_empty(), "{:?}", verdict.regressions);
        // No probe on either side: no row.
        assert!(!compare(&base, &base, 7, 200).table.contains("| defs "));
    }

    #[test]
    fn defs_count_a_failed_probe_once_per_name_and_flag_a_foreign_probe() {
        let base = results(&[0.5; 40], "m", 100.0);
        let mut new = results(&[0.5; 40], "m", 100.0);
        new["build"] = json!({"commit": "b"});
        new["defs_probe"] = json!({"commit": "p"});
        let tasks = new["tasks"].as_array_mut().unwrap();
        // A failed probe on a task with two gold names: two error rows
        // beside the build error, counted as two, not three.
        tasks[0]["repo"] = json!("tokio");
        tasks[0]["defs"] = json!([
            {"name": "a", "error": "defs probe failed: boom"},
            {"name": "b", "error": "defs probe failed: boom"},
        ]);
        tasks[0]["defs_build"] = json!({"error": "boom"});
        // A file from before per-name error rows: the failed task counts once.
        tasks[1]["repo"] = json!("caddy");
        tasks[1]["defs_build"] = json!({"error": "boom"});
        // A failed task with no gold names has no rows: it counts once too.
        tasks[2]["repo"] = json!("black");
        tasks[2]["defs"] = json!([]);
        tasks[2]["defs_build"] = json!({"error": "boom"});
        let verdict = compare(&base, &new, 7, 200);
        let row = |prefix: &str| {
            verdict
                .table
                .lines()
                .find(|line| line.starts_with(prefix))
                .unwrap_or_else(|| panic!("no {prefix} row in\n{}", verdict.table))
                .to_owned()
        };
        assert!(
            row("| defs tokio ").contains("| no scored names (errors=2) |"),
            "{}",
            verdict.table
        );
        assert!(
            row("| defs caddy ").contains("| no scored names (errors=1) |"),
            "{}",
            verdict.table
        );
        assert!(
            row("| defs black ").contains("| no scored names (errors=1) |"),
            "{}",
            verdict.table
        );
        assert!(
            row("| defs hit@5").contains("| no scored names (errors=4) |"),
            "{}",
            verdict.table
        );
        assert!(
            verdict
                .table
                .contains("new: the defs probe is not frozen from the build's commit"),
            "{}",
            verdict.table
        );
        assert!(verdict.regressions.is_empty(), "{:?}", verdict.regressions);
    }

    #[test]
    fn epoch_seconds_reads_the_results_timestamp() {
        assert_eq!(epoch_seconds("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_seconds("2026-10-12T17:00:00Z"), Some(1_791_824_400));
        assert_eq!(epoch_seconds("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(epoch_seconds("not a time"), None);
    }
}
