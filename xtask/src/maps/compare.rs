//! `cargo xtask maps compare <run_dir_A> <run_dir_B>`: a paired comparison of
//! two bench runs of one set.
//!
//! A run dir is `<out>/<wave>/<set>` (what `maps bench` writes). Items are
//! paired by id; for each metric the command prints the number of pairs, the
//! two means, the mean of `B - A` with a 95% paired-bootstrap percentile
//! interval (fixed seed, 10000 resamples by default, the algorithm of
//! `lci evaluate compare`), and how many pairs B is above, below or level on.
//! Which side is better depends on the metric (`spurious` and `expansions` are
//! better when lower), so the counts say `B>A` and `B<A`, not wins and losses.
//!
//! The per-item metrics are those `maps bench` scored and wrote to
//! `items.json` ([`super::bench::item_record`]). A run from before that file
//! is scored again from its `run.json` with the same function, which needs
//! `--gold-dir`. An item is left out when either side stopped on the deadline
//! or had reranker or map call failures (read from `raw/<id>.json`,
//! [`super::bench::ItemFlags`]) unless `--keep-failed`; such a run is not a
//! clean measurement. A run dir with no `raw/` (a rescored run) has unknown
//! flags: its items are kept, and the count is printed.

use super::bench::{self, ItemFlags};
use crate::gold::{self, Set};
use anyhow::{Context, Result, bail};
use clap::{Args, ValueEnum};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Fixed seed, so the same two runs print the same interval.
const SEED: u64 = 0x4C43_4933;
const RESAMPLES: usize = 10_000;
/// Percentile bounds of the 95% interval, in permille.
const LOWER_PERMILLE: usize = 25;
const UPPER_PERMILLE: usize = 975;
/// Deltas within this distance of zero are ties.
const TIE_EPSILON: f64 = 1e-12;

#[derive(Args)]
pub struct CompareArgs {
    /// Run dir A, the baseline (`<out>/<wave>/<set>`).
    run_a: PathBuf,
    /// Run dir B; deltas are `B - A`.
    run_b: PathBuf,
    /// Metrics to compare (default: the set's headline metrics, `precision`,
    /// `f0.5` and `expansions`).
    #[arg(long, num_args = 1..)]
    metric: Vec<String>,
    /// Keep items either side stopped on the deadline or had failures on
    /// (default: leave them out).
    #[arg(long)]
    keep_failed: bool,
    /// Gold dir, only to score a run that has no `items.json` again.
    #[arg(long)]
    gold_dir: Option<PathBuf>,
    /// Bootstrap resamples.
    #[arg(long, default_value_t = RESAMPLES)]
    resamples: usize,
}

/// What one run dir holds, reduced to what the comparison reads.
#[derive(Debug, Default)]
pub struct RunData {
    pub set: String,
    /// id -> metric -> value (`None`: undefined for the item).
    pub items: BTreeMap<String, BTreeMap<String, Option<f64>>>,
    /// id -> flags; `None` when the item's raw output is not there.
    pub flags: BTreeMap<String, Option<ItemFlags>>,
}

fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not JSON", path.display()))
}

/// Metric name -> value of one `items.json` record.
fn record_metrics(record: &Value) -> BTreeMap<String, Option<f64>> {
    record
        .as_object()
        .into_iter()
        .flatten()
        .map(|(name, value)| (name.clone(), value.as_f64()))
        .collect()
}

pub fn load_run(dir: &Path, gold_dir: Option<&Path>) -> Result<RunData> {
    let mut data = RunData::default();
    let items_path = dir.join("items.json");
    if items_path.is_file() {
        let doc = read_json(&items_path)?;
        data.set = doc["set"].as_str().unwrap_or_default().to_owned();
        for (id, record) in doc["items"].as_object().into_iter().flatten() {
            data.items.insert(id.clone(), record_metrics(record));
        }
    } else {
        let gold_dir = gold_dir.with_context(|| {
            format!(
                "{} has no items.json (a run from before it): pass --gold-dir to score it again",
                dir.display()
            )
        })?;
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        let set = Set::from_str(name, true)
            .map_err(|_| anyhow::anyhow!("cannot tell the set from the run dir name {name:?}"))?;
        data.set = bench::set_name(set).to_owned();
        let run = read_json(&dir.join("run.json"))?;
        let gold_items = gold::load_gold(&gold_dir.join(format!("{}.json", data.set)))?;
        for item in &gold_items {
            let Some(id) = item["id"].as_str() else {
                continue;
            };
            let Some(answers) = run[id].as_array() else {
                continue;
            };
            let (_, _, record) = bench::item_record(set, item, answers, None)?;
            data.items
                .insert(id.to_owned(), record_metrics(&Value::Object(record)));
        }
    }
    for id in data.items.keys() {
        let raw = dir.join("raw").join(format!("{}.json", bench::slug(id)));
        let flags = std::fs::read_to_string(&raw)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .map(|raw| ItemFlags::from_raw(&raw));
        data.flags.insert(id.clone(), flags);
    }
    Ok(data)
}

/// One metric over the pairs where both sides define it.
#[derive(Debug, Clone, PartialEq)]
pub struct MetricRow {
    pub name: String,
    pub n: usize,
    pub mean_a: f64,
    pub mean_b: f64,
    pub mean_delta: f64,
    /// 95% paired-bootstrap interval of the mean delta (`None` without pairs).
    pub ci: Option<(f64, f64)>,
    pub b_above: usize,
    pub b_below: usize,
    pub ties: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Comparison {
    /// Ids in both runs, before any exclusion.
    pub paired: usize,
    pub only_a: usize,
    pub only_b: usize,
    /// Ids left out because a side was flagged (0 with `--keep-failed`), of
    /// the pairs: flagged on side A, on side B, either.
    pub excluded_a: usize,
    pub excluded_b: usize,
    pub excluded: usize,
    /// Kept pairs with no raw output on a side (flags unknown).
    pub unknown_flags: usize,
    pub rows: Vec<MetricRow>,
}

fn mean(values: &[f64]) -> f64 {
    values.iter().sum::<f64>() / values.len() as f64
}

/// SplitMix64: a tiny deterministic generator for resampling indices.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (multiply-shift).
    fn below(&mut self, bound: usize) -> usize {
        ((u128::from(self.next_u64()) * bound as u128) >> 64) as usize
    }
}

/// 95% percentile interval of the mean of `deltas` under the paired
/// bootstrap: `resamples` resamples of the indices with replacement, each the
/// size of the input, from a SplitMix64 generator seeded with `seed`. `None`
/// for an empty input; a constant input `d` gives `(d, d)`.
pub fn paired_bootstrap_ci(deltas: &[f64], resamples: usize, seed: u64) -> Option<(f64, f64)> {
    let n = deltas.len();
    if n == 0 {
        return None;
    }
    let resamples = resamples.max(1);
    let mut rng = SplitMix64 { state: seed };
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| (0..n).map(|_| deltas[rng.below(n)]).sum::<f64>() / n as f64)
        .collect();
    means.sort_by(f64::total_cmp);
    let low = resamples * LOWER_PERMILLE / 1000;
    let high = (resamples * UPPER_PERMILLE).div_ceil(1000) - 1;
    Some((
        means[low.min(resamples - 1)],
        means[high.min(resamples - 1)],
    ))
}

/// Pairs the items of two runs by id and compares `metrics` over them.
pub fn compare(
    a: &RunData,
    b: &RunData,
    metrics: &[String],
    keep_failed: bool,
    resamples: usize,
    seed: u64,
) -> Comparison {
    let both: Vec<&String> = a
        .items
        .keys()
        .filter(|id| b.items.contains_key(*id))
        .collect();
    let flagged = |run: &RunData, id: &str| {
        run.flags
            .get(id)
            .copied()
            .flatten()
            .is_some_and(|flags| flags.any())
    };
    let mut out = Comparison {
        paired: both.len(),
        only_a: a.items.len() - both.len(),
        only_b: b.items.len() - both.len(),
        excluded_a: 0,
        excluded_b: 0,
        excluded: 0,
        unknown_flags: 0,
        rows: Vec::new(),
    };
    let mut kept: Vec<&String> = Vec::new();
    for id in both {
        let (fa, fb) = (flagged(a, id), flagged(b, id));
        out.excluded_a += usize::from(fa);
        out.excluded_b += usize::from(fb);
        if (fa || fb) && !keep_failed {
            out.excluded += 1;
            continue;
        }
        let known = |run: &RunData| run.flags.get(id.as_str()).copied().flatten().is_some();
        out.unknown_flags += usize::from(!(known(a) && known(b)));
        kept.push(id);
    }
    for name in metrics {
        let pairs: Vec<(f64, f64)> = kept
            .iter()
            .filter_map(|id| {
                let value = |run: &RunData| run.items[id.as_str()].get(name).copied().flatten();
                Some((value(a)?, value(b)?))
            })
            .collect();
        let deltas: Vec<f64> = pairs.iter().map(|(x, y)| y - x).collect();
        let nan = f64::NAN;
        out.rows.push(MetricRow {
            name: name.clone(),
            n: pairs.len(),
            mean_a: if pairs.is_empty() {
                nan
            } else {
                mean(&pairs.iter().map(|p| p.0).collect::<Vec<_>>())
            },
            mean_b: if pairs.is_empty() {
                nan
            } else {
                mean(&pairs.iter().map(|p| p.1).collect::<Vec<_>>())
            },
            mean_delta: if pairs.is_empty() { nan } else { mean(&deltas) },
            ci: paired_bootstrap_ci(&deltas, resamples, seed),
            b_above: deltas.iter().filter(|d| **d > TIE_EPSILON).count(),
            b_below: deltas.iter().filter(|d| **d < -TIE_EPSILON).count(),
            ties: deltas.iter().filter(|d| d.abs() <= TIE_EPSILON).count(),
        });
    }
    out
}

/// The metrics compared when `--metric` is not given.
fn default_metrics(set: &str) -> Vec<String> {
    let headline = Set::from_str(set, true)
        .map(bench::headline)
        .unwrap_or_default();
    headline
        .iter()
        .copied()
        .chain(["precision", "f0.5", "expansions"])
        .map(str::to_owned)
        .collect()
}

pub fn render(
    a: &Path,
    b: &Path,
    set: &str,
    keep_failed: bool,
    resamples: usize,
    c: &Comparison,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "A: {}", a.display());
    let _ = writeln!(out, "B: {}", b.display());
    let _ = writeln!(
        out,
        "set {set}: {} items in both runs, {} only in A, {} only in B",
        c.paired, c.only_a, c.only_b
    );
    if keep_failed {
        let _ = writeln!(
            out,
            "kept every pair (--keep-failed); flagged by deadline or failures: {} on A, {} on B",
            c.excluded_a, c.excluded_b
        );
    } else {
        let _ = writeln!(
            out,
            "left out {} pairs flagged by a deadline stop or failures ({} on A, {} on B); {} pairs used",
            c.excluded,
            c.excluded_a,
            c.excluded_b,
            c.paired - c.excluded
        );
    }
    if c.unknown_flags > 0 {
        let _ = writeln!(
            out,
            "{} used pairs have no raw output on a side: their flags are unknown",
            c.unknown_flags
        );
    }
    let _ = writeln!(
        out,
        "delta = B - A; interval: 95% paired bootstrap, {resamples} resamples, seed {SEED:#x}"
    );
    let _ = writeln!(
        out,
        "metric | n | mean A | mean B | delta | 95% CI | B>A | B<A | tie"
    );
    for row in &c.rows {
        let ci = row.ci.map_or_else(
            || "-".to_owned(),
            |(lo, hi)| format!("[{lo:+.4}, {hi:+.4}]"),
        );
        if row.n == 0 {
            let _ = writeln!(out, "{} | 0 | - | - | - | - | - | - | -", row.name);
        } else {
            let _ = writeln!(
                out,
                "{} | {} | {:.4} | {:.4} | {:+.4} | {ci} | {} | {} | {}",
                row.name,
                row.n,
                row.mean_a,
                row.mean_b,
                row.mean_delta,
                row.b_above,
                row.b_below,
                row.ties
            );
        }
    }
    out
}

pub fn run(args: CompareArgs) -> Result<()> {
    let a = load_run(&args.run_a, args.gold_dir.as_deref())?;
    let b = load_run(&args.run_b, args.gold_dir.as_deref())?;
    if a.set != b.set {
        bail!("the runs are of different sets ({} and {})", a.set, b.set);
    }
    let metrics = if args.metric.is_empty() {
        default_metrics(&a.set)
    } else {
        args.metric.clone()
    };
    let comparison = compare(&a, &b, &metrics, args.keep_failed, args.resamples, SEED);
    print!(
        "{}",
        render(
            &args.run_a,
            &args.run_b,
            &a.set,
            args.keep_failed,
            args.resamples,
            &comparison
        )
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn run_of(items: &[(&str, f64, ItemFlags)]) -> RunData {
        let mut data = RunData {
            set: "flow".into(),
            ..RunData::default()
        };
        for (id, value, flags) in items {
            data.items.insert(
                (*id).into(),
                BTreeMap::from([("path_recall".to_owned(), Some(*value))]),
            );
            data.flags.insert((*id).into(), Some(*flags));
        }
        data
    }

    const CLEAN: ItemFlags = ItemFlags {
        deadline: false,
        rerank_failures: 0,
        map_failures: 0,
    };

    #[test]
    fn the_bootstrap_is_deterministic_and_brackets_the_mean() {
        let deltas = [0.5, 0.25, 0.0, 0.75, 0.5, 0.25, 1.0, 0.0];
        let first = paired_bootstrap_ci(&deltas, 10_000, SEED).unwrap();
        assert_eq!(first, paired_bootstrap_ci(&deltas, 10_000, SEED).unwrap());
        assert_ne!(
            first,
            paired_bootstrap_ci(&deltas, 10_000, SEED + 1).unwrap()
        );
        let m = mean(&deltas);
        assert!(
            first.0 <= m && m <= first.1 && first.0 > 0.0,
            "{first:?} {m}"
        );
        // A constant input has no spread; no input has no interval.
        assert_eq!(paired_bootstrap_ci(&[0.5; 5], 100, SEED), Some((0.5, 0.5)));
        assert_eq!(paired_bootstrap_ci(&[], 100, SEED), None);
    }

    #[test]
    fn items_are_paired_by_id_and_flagged_ones_are_left_out_unless_kept() {
        let deadline = ItemFlags {
            deadline: true,
            ..CLEAN
        };
        let rerank = ItemFlags {
            rerank_failures: 2,
            ..CLEAN
        };
        let a = run_of(&[
            ("i1", 0.0, CLEAN),
            ("i2", 0.5, deadline),
            ("i3", 1.0, CLEAN),
            ("i4", 0.5, CLEAN),
            ("only-a", 1.0, CLEAN),
        ]);
        let b = run_of(&[
            ("i1", 1.0, CLEAN),
            ("i2", 1.0, CLEAN),
            ("i3", 0.5, rerank),
            ("i4", 0.5, CLEAN),
            ("only-b", 1.0, CLEAN),
        ]);
        let metrics = ["path_recall".to_owned(), "missing".to_owned()];
        let strict = compare(&a, &b, &metrics, false, 1000, SEED);
        assert_eq!((strict.paired, strict.only_a, strict.only_b), (4, 1, 1));
        assert_eq!(
            (strict.excluded, strict.excluded_a, strict.excluded_b),
            (2, 1, 1)
        );
        let row = &strict.rows[0];
        // i1 (0 -> 1) and i4 (tie) remain.
        assert_eq!((row.n, row.b_above, row.b_below, row.ties), (2, 1, 0, 1));
        assert_eq!((row.mean_a, row.mean_b, row.mean_delta), (0.25, 0.75, 0.5));
        assert!(row.ci.is_some());
        // A metric nobody has pairs nothing.
        assert_eq!(strict.rows[1].n, 0);

        let kept = compare(&a, &b, &metrics, true, 1000, SEED);
        assert_eq!(kept.excluded, 0);
        let row = &kept.rows[0];
        // i1 up, i2 up, i3 down, i4 tie.
        assert_eq!((row.n, row.b_above, row.b_below, row.ties), (4, 2, 1, 1));
        assert!((row.mean_delta - 0.25).abs() < 1e-12);
    }

    #[test]
    fn a_run_dir_is_read_with_its_flags_from_the_raw_outputs() {
        let root = std::env::temp_dir().join(format!("xtask-compare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("flow");
        std::fs::create_dir_all(dir.join("raw")).unwrap();
        let items = json!({"set": "flow", "items": {
            "flow-1": {"path_recall": 1.0, "tau": null},
            "flow-2": {"path_recall": 0.5},
        }});
        std::fs::write(dir.join("items.json"), items.to_string()).unwrap();
        std::fs::write(
            dir.join("raw").join("flow-1.json"),
            json!({"stop_reason": "deadline", "totals": {}}).to_string(),
        )
        .unwrap();
        // flow-2 has no raw output: its flags are unknown, not clean.
        let data = load_run(&dir, None).unwrap();
        assert_eq!(data.set, "flow");
        assert_eq!(data.items["flow-1"]["tau"], None);
        assert_eq!(data.items["flow-1"]["path_recall"], Some(1.0));
        assert!(data.flags["flow-1"].unwrap().deadline);
        assert_eq!(data.flags["flow-2"], None);
        // Without items.json the gold dir is needed.
        std::fs::remove_file(dir.join("items.json")).unwrap();
        assert!(load_run(&dir, None).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn default_metrics_are_the_headline_ones_plus_the_work() {
        let metrics = default_metrics("flow");
        assert_eq!(metrics[0], "path_recall");
        assert!(metrics.ends_with(&["precision".into(), "f0.5".into(), "expansions".into()]));
    }
}
