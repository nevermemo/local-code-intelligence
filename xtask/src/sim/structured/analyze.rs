//! `sim-structured compare`: B against A over the scored rows of finished
//! runs (preregistration sections 6 and 7, harness plan section 5).
//!
//! # Pairing
//!
//! A run is its score rows and its settings (`summary.json`: arm, grep-skip
//! switch, policy, stratum, the agent's config and the scan byte cap) and its
//! `meta.json`. The settings are checked before any row is read: an A run must
//! be arm `a` with grep-skip `none`, a B run arm `b` with `lockfiles`; both
//! must have the policy named by the pair's label, the same stratum, the same
//! config and byte cap, and be different directories. A context run must be
//! arm `r` with the label's policy and the config of the pairs under the label.
//! Rows are filtered by split (`dev`, `heldout` or `all`) and paired by
//! question id within one policy label. A question missing from either side, a
//! repeated id (in one run, or in two pairs of one label), rows that disagree
//! on family, repository, stratum, split or type, or `meta.xtask_sha256` that
//! is missing or differs between A and B is an error: the arms must be the same
//! frozen runner on the same questions. Several entries may share a policy
//! label (one per stratum); their pairs are pooled. The stratum of a question
//! comes from its rows.
//!
//! # Statistics
//!
//! For each metric and group the paired deltas `B - A` are resampled by
//! (repository, family) (a family's questions always move together): 10,000
//! resamples (SplitMix64 from the recorded seed, restarted for every metric and
//! group, so they share their index sequences), the statistic being the mean of
//! `B - A` over the resampled families' questions, percentile interval 2.5 and
//! 97.5 (the sorted values at 0-based indices `n * 25 / 1000` and
//! `ceil(n * 975 / 1000) - 1`, 250 and 9749 for 10,000).
//! The relative change `(sum B - sum A) / sum A` is resampled alongside.
//! `wins` counts questions where B is better (higher for success, hit and
//! reader scores, lower for charged tokens, calls and pops), `losses` where it
//! is worse. Every cell prints its n; a cell under 10 is "too small to read".
//!
//! # Conditions (section 7)
//!
//! C1 (the correctness fixtures) is not computed here. C2 to C6 are computed
//! per policy and per stratum from the rows, then combined: a condition holds
//! only if it holds under every policy (and, for C3, in every repository), fails
//! if it fails anywhere, and is unresolved otherwise.
//!
//! - C2: the stratum's complete-success drops by at most 2 percentage points.
//! - C3: no repository loses more than one question (complete in A, not in B).
//! - C4 (config): mean reader precision over the q3 questions drops by at most
//!   2 points.
//! - C5 (dependency): tokens charged are at least 15% lower with B completing
//!   at least as many questions as A and the paired interval of the token delta
//!   excluding zero, or complete-success is at least 10 points higher with
//!   tokens at most 5% worse and the interval of the success delta excluding
//!   zero. Both policies must satisfy the same branch (that is the direction
//!   agreement). A stratum under 10 questions, or a set of policy labels that
//!   is not exactly {greedy, bfs}, is unresolved. On the config stratum C5 is
//!   the no-harm test: tokens at most 5% worse.
//! - C6: charged calls and charged pops each at most 5% worse.
//!
//! Charged figures are those to completion, or the whole run on a miss (see
//! `score`). R runs only fill a context table.

use super::gold::SETTING_TO_READER;
use super::score::{ScoreRow, TOO_SMALL};
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::Path;

const RESAMPLES: usize = 10_000;
/// Percentile bounds of the 95% interval, in permille.
const LOWER_PERMILLE: usize = 25;
const UPPER_PERMILLE: usize = 975;
/// Deltas within this distance of zero are ties.
const TIE_EPSILON: f64 = 1e-12;
/// Slack for comparing a computed figure with a threshold.
const LIMIT_EPS: f64 = 1e-9;
/// C2: complete-success may drop by this many percentage points.
const MAX_SUCCESS_DROP_PP: f64 = 2.0;
/// C3: questions one repository may lose.
const MAX_LOST_PER_REPO: usize = 1;
/// C4: reader precision may drop by this many points.
const MAX_PRECISION_DROP_PTS: f64 = 2.0;
/// C5: the token saving that counts, as a fraction.
const TOKEN_SAVING: f64 = 0.15;
/// C5: the success gain that counts, in percentage points.
const SUCCESS_GAIN_PP: f64 = 10.0;
/// C5 (config) and C6: how much worse B may be, as a fraction.
const WORSE_LIMIT: f64 = 0.05;
const STRATA: [&str; 2] = ["config", "dependency"];

// --------------------------------------------------------------------- runs

/// One finished run directory, reduced to what the comparison reads.
#[derive(Clone, Debug)]
pub(crate) struct Run {
    pub meta: Value,
    pub rows: Vec<ScoreRow>,
    /// The resolved run directory (two entries naming one directory are one
    /// run). Never written to an output.
    pub dir: String,
    /// `a`, `b` or `r`.
    pub arm: String,
    /// `none`, `lockfiles` or `rg`.
    pub grep_skip: String,
    pub policy: String,
    pub stratum: String,
    /// The agent's `SimConfig` as recorded.
    pub config: Value,
    /// The scan byte cap; none for an `rg` run.
    pub max_scan_bytes: Option<u64>,
}

impl Run {
    fn xtask_hash(&self) -> Option<&str> {
        self.meta.get("xtask_sha256").and_then(Value::as_str)
    }
}

fn read_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", path.display()))
}

/// Reads `<dir>/summary.json` (its settings and `rows`) and `<dir>/meta.json`.
pub(crate) fn load_run(dir: &Path) -> Result<Run> {
    let summary = read_json(&dir.join("summary.json"))?;
    let meta = read_json(&dir.join("meta.json"))?;
    let rows = summary
        .get("rows")
        .cloned()
        .with_context(|| format!("{}: summary.json has no `rows`", dir.display()))?;
    let rows: Vec<ScoreRow> = serde_json::from_value(rows)
        .with_context(|| format!("{}: summary.json `rows` are not score rows", dir.display()))?;
    let text = |key: &str| -> Result<String> {
        summary
            .get(key)
            .and_then(Value::as_str)
            .map(str::to_owned)
            .with_context(|| format!("{}: summary.json has no `{key}`", dir.display()))
    };
    let config = summary
        .get("config")
        .filter(|c| c.is_object())
        .cloned()
        .with_context(|| format!("{}: summary.json has no `config`", dir.display()))?;
    let resolved = dir
        .canonicalize()
        .with_context(|| format!("resolve {}", dir.display()))?;
    Ok(Run {
        meta,
        rows,
        dir: resolved.display().to_string(),
        arm: text("arm")?,
        grep_skip: text("grep_skip")?,
        policy: text("policy")?,
        stratum: text("stratum")?,
        config,
        max_scan_bytes: summary
            .pointer("/matcher/max_scan_bytes")
            .and_then(Value::as_u64),
    })
}

// --------------------------------------------------------------- bootstrap

/// SplitMix64: a tiny deterministic generator for resampling indices. Copied
/// from `maps/compare.rs` (same algorithm, same multiply-shift `below`).
struct SplitMix64(u64);

impl SplitMix64 {
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (multiply-shift).
    fn below(&mut self, bound: usize) -> usize {
        ((u128::from(self.next_u64()) * bound as u128) >> 64) as usize
    }
}

/// One family's questions: sums of the A and B values and the count.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Cluster {
    sum_a: f64,
    sum_b: f64,
    n: usize,
}

/// ((repository, family), A value, B value) of one paired question. A family
/// id is only unique within its repository.
type Obs<'a> = ((&'a str, &'a str), f64, f64);

fn clusters_of(obs: &[Obs]) -> Vec<Cluster> {
    let mut by: BTreeMap<(&str, &str), Cluster> = BTreeMap::new();
    for &(family, a, b) in obs {
        let c = by.entry(family).or_insert(Cluster {
            sum_a: 0.0,
            sum_b: 0.0,
            n: 0,
        });
        c.sum_a += a;
        c.sum_b += b;
        c.n += 1;
    }
    by.into_values().collect()
}

/// `resamples` resamples of the families with replacement: for each, the mean
/// delta over the drawn families' questions and the relative change (`None`
/// when the drawn A values sum to zero). `clusters` must not be empty.
fn draws(clusters: &[Cluster], resamples: usize, seed: u64) -> Vec<(f64, Option<f64>)> {
    let k = clusters.len();
    let mut rng = SplitMix64(seed);
    (0..resamples.max(1))
        .map(|_| {
            let (mut a, mut b, mut n) = (0.0_f64, 0.0_f64, 0_usize);
            for _ in 0..k {
                let c = &clusters[rng.below(k)];
                a += c.sum_a;
                b += c.sum_b;
                n += c.n;
            }
            ((b - a) / n as f64, (a != 0.0).then(|| (b - a) / a))
        })
        .collect()
}

/// The 2.5 and 97.5 percentiles of `values` (not empty): the sorted values at
/// 0-based indices `n * 25 / 1000` and `ceil(n * 975 / 1000) - 1` (250 and 9749
/// of 10,000).
fn percentile_interval(mut values: Vec<f64>) -> (f64, f64) {
    let n = values.len();
    values.sort_by(f64::total_cmp);
    let low = n * LOWER_PERMILLE / 1000;
    let high = (n * UPPER_PERMILLE).div_ceil(1000) - 1;
    (values[low.min(n - 1)], values[high.min(n - 1)])
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Intervals {
    /// Interval of the mean delta.
    delta: (f64, f64),
    /// Interval of the relative change; `None` when A's total is zero in any
    /// resample.
    rel: Option<(f64, f64)>,
}

/// The clustered paired bootstrap; `None` for no clusters.
fn intervals(clusters: &[Cluster], resamples: usize, seed: u64) -> Option<Intervals> {
    if clusters.is_empty() {
        return None;
    }
    let drawn = draws(clusters, resamples, seed);
    let delta = percentile_interval(drawn.iter().map(|d| d.0).collect());
    let rels: Vec<f64> = drawn.iter().filter_map(|d| d.1).collect();
    let rel = (rels.len() == drawn.len()).then(|| percentile_interval(rels));
    Some(Intervals { delta, rel })
}

// ------------------------------------------------------------------ pairing

/// One question scored in both arms.
#[derive(Clone, Debug)]
struct Paired {
    policy: String,
    a: ScoreRow,
    b: ScoreRow,
}

fn in_split(row: &ScoreRow, split: &str) -> bool {
    split == "all" || row.split == split
}

fn index_rows<'a>(
    policy: &str,
    side: &str,
    run: &'a Run,
    split: &str,
) -> Result<BTreeMap<&'a str, &'a ScoreRow>> {
    let mut out = BTreeMap::new();
    for row in run.rows.iter().filter(|r| in_split(r, split)) {
        if out.insert(row.question_id.as_str(), row).is_some() {
            bail!(
                "{policy}: question {} appears twice in run {side}",
                row.question_id
            );
        }
    }
    Ok(out)
}

/// The settings of the two runs of the pair `label` must make them comparable:
/// A is the unskipped scan, B the lockfile-skipping scan, both of the policy
/// the label names, on one stratum, with the same config and byte cap, and they
/// are two different run dirs.
fn check_pair(label: &str, a: &Run, b: &Run) -> Result<()> {
    if (a.arm.as_str(), a.grep_skip.as_str()) != ("a", "none") {
        bail!(
            "{label}: run A is arm {} with grep-skip {}; it must be arm a with none",
            a.arm,
            a.grep_skip
        );
    }
    if (b.arm.as_str(), b.grep_skip.as_str()) != ("b", "lockfiles") {
        bail!(
            "{label}: run B is arm {} with grep-skip {}; it must be arm b with lockfiles",
            b.arm,
            b.grep_skip
        );
    }
    if a.dir == b.dir {
        bail!("{label}: runs A and B are the same run dir");
    }
    for (side, run) in [("A", a), ("B", b)] {
        if run.policy != label {
            bail!(
                "{label}: run {side} has the policy {}, not the pair's label",
                run.policy
            );
        }
    }
    if a.stratum != b.stratum {
        bail!(
            "{label}: runs A and B are of different strata ({} vs {})",
            a.stratum,
            b.stratum
        );
    }
    if a.config != b.config || a.max_scan_bytes != b.max_scan_bytes {
        bail!(
            "{label}: runs A and B have different settings (config {} / scan cap {:?} vs {} / {:?}); the budgets of the arms must be equal",
            a.config,
            a.max_scan_bytes,
            b.config,
            b.max_scan_bytes
        );
    }
    Ok(())
}

/// A context run must be an R run of the label's policy with the config of the
/// pair it is shown beside.
fn check_context(label: &str, r: &Run, a: &Run) -> Result<()> {
    if r.arm != "r" {
        bail!("{label}: the context run is arm {}, not r", r.arm);
    }
    if r.policy != label {
        bail!(
            "{label}: the context run has the policy {}, not the label",
            r.policy
        );
    }
    if r.config != a.config {
        bail!(
            "{label}: the context run has a different config ({} vs {}); the budgets must be equal",
            r.config,
            a.config
        );
    }
    Ok(())
}

fn pair_up(policy: &str, a: &Run, b: &Run, split: &str) -> Result<Vec<Paired>> {
    check_pair(policy, a, b)?;
    match (a.xtask_hash(), b.xtask_hash()) {
        (Some(x), Some(y)) if x == y => {}
        (Some(x), Some(y)) => bail!(
            "{policy}: arms A and B ran different runners (xtask sha256 {x} vs {y}); not comparable"
        ),
        _ => bail!("{policy}: meta.json of run A or B has no xtask_sha256"),
    }
    let in_a = index_rows(policy, "A", a, split)?;
    let in_b = index_rows(policy, "B", b, split)?;
    let mut out = Vec::new();
    for (id, ra) in &in_a {
        let Some(rb) = in_b.get(id) else {
            bail!("{policy}: question {id} is in run A but not in run B (missing pair)");
        };
        let same = (&ra.family, &ra.repo, &ra.stratum, &ra.split, &ra.qtype)
            == (&rb.family, &rb.repo, &rb.stratum, &rb.split, &rb.qtype);
        if !same {
            bail!("{policy}: question {id} has different labels in run A and run B");
        }
        out.push(Paired {
            policy: policy.to_owned(),
            a: (*ra).clone(),
            b: (*rb).clone(),
        });
    }
    for id in in_b.keys() {
        if !in_a.contains_key(id) {
            bail!("{policy}: question {id} is in run B but not in run A (missing pair)");
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ metrics

type ValueOf = fn(&ScoreRow) -> Option<f64>;

fn flag(b: bool) -> Option<f64> {
    Some(f64::from(u8::from(b)))
}

fn v_complete(r: &ScoreRow) -> Option<f64> {
    flag(r.score.complete)
}

fn v_tokens(r: &ScoreRow) -> Option<f64> {
    Some(r.score.charged_tokens as f64)
}

fn v_calls(r: &ScoreRow) -> Option<f64> {
    Some(f64::from(r.score.charged_calls))
}

fn v_pops(r: &ScoreRow) -> Option<f64> {
    Some(f64::from(r.score.charged_pops))
}

/// Assignment-span hit@5 is the headline for q1 and q2; for q3 it is not the
/// asked fact and is left out of the comparison.
fn v_hit5(r: &ScoreRow) -> Option<f64> {
    if r.qtype == SETTING_TO_READER {
        None
    } else {
        flag(r.score.hit_at_5)
    }
}

fn v_precision(r: &ScoreRow) -> Option<f64> {
    r.score.reader_precision
}

fn v_recall(r: &ScoreRow) -> Option<f64> {
    r.score.reader_recall
}

fn v_f1(r: &ScoreRow) -> Option<f64> {
    r.score.reader_f1
}

struct MetricDef {
    name: &'static str,
    higher_is_better: bool,
    /// A share in 0..=1, printed as a percentage.
    fraction: bool,
    value: ValueOf,
}

const METRICS: &[MetricDef] = &[
    MetricDef {
        name: "complete",
        higher_is_better: true,
        fraction: true,
        value: v_complete,
    },
    MetricDef {
        name: "charged_tokens",
        higher_is_better: false,
        fraction: false,
        value: v_tokens,
    },
    MetricDef {
        name: "charged_calls",
        higher_is_better: false,
        fraction: false,
        value: v_calls,
    },
    MetricDef {
        name: "charged_pops",
        higher_is_better: false,
        fraction: false,
        value: v_pops,
    },
    MetricDef {
        name: "hit_at_5",
        higher_is_better: true,
        fraction: true,
        value: v_hit5,
    },
    MetricDef {
        name: "reader_precision",
        higher_is_better: true,
        fraction: true,
        value: v_precision,
    },
    MetricDef {
        name: "reader_recall",
        higher_is_better: true,
        fraction: true,
        value: v_recall,
    },
    MetricDef {
        name: "reader_f1",
        higher_is_better: true,
        fraction: true,
        value: v_f1,
    },
];

/// One metric over one group of paired questions.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct MetricRow {
    pub policy: String,
    pub stratum: String,
    /// `all`, `repo=<name>` or `type=<name>`.
    pub group: String,
    pub metric: &'static str,
    /// Paired questions where both sides define the metric.
    pub n: usize,
    pub families: usize,
    pub too_small_to_read: bool,
    /// A share (0..=1) rather than a count.
    pub fraction: bool,
    pub mean_a: f64,
    pub mean_b: f64,
    /// Mean of `B - A`, with its clustered 95% interval.
    pub mean_delta: f64,
    pub ci: Option<(f64, f64)>,
    /// `(sum B - sum A) / sum A`, `None` when A sums to zero.
    pub rel_delta: Option<f64>,
    pub rel_ci: Option<(f64, f64)>,
    /// Questions where B is better, worse, and level.
    pub wins: usize,
    pub losses: usize,
    pub ties: usize,
}

fn observations<'a>(sel: &[&'a Paired], value: ValueOf) -> Vec<Obs<'a>> {
    sel.iter()
        .copied()
        .filter_map(|p| {
            Some((
                (p.a.repo.as_str(), p.a.family.as_str()),
                value(&p.a)?,
                value(&p.b)?,
            ))
        })
        .collect()
}

fn metric_row(
    policy: &str,
    stratum: &str,
    group: &str,
    def: &MetricDef,
    sel: &[&Paired],
    resamples: usize,
    seed: u64,
) -> Option<MetricRow> {
    let obs = observations(sel, def.value);
    if obs.is_empty() {
        return None;
    }
    let n = obs.len();
    let sum_a: f64 = obs.iter().map(|o| o.1).sum();
    let sum_b: f64 = obs.iter().map(|o| o.2).sum();
    let (mut wins, mut losses, mut ties) = (0, 0, 0);
    for &(_, a, b) in &obs {
        let d = b - a;
        if d.abs() <= TIE_EPSILON {
            ties += 1;
        } else if (d > 0.0) == def.higher_is_better {
            wins += 1;
        } else {
            losses += 1;
        }
    }
    let clusters = clusters_of(&obs);
    let found = intervals(&clusters, resamples, seed);
    Some(MetricRow {
        policy: policy.to_owned(),
        stratum: stratum.to_owned(),
        group: group.to_owned(),
        metric: def.name,
        n,
        families: clusters.len(),
        too_small_to_read: n < TOO_SMALL,
        fraction: def.fraction,
        mean_a: sum_a / n as f64,
        mean_b: sum_b / n as f64,
        mean_delta: (sum_b - sum_a) / n as f64,
        ci: found.map(|i| i.delta),
        rel_delta: (sum_a != 0.0).then(|| (sum_b - sum_a) / sum_a),
        rel_ci: found.and_then(|i| i.rel),
        wins,
        losses,
        ties,
    })
}

// -------------------------------------------------------------- conditions

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    Holds,
    Fails,
    Unresolved,
}

impl Verdict {
    fn name(self) -> &'static str {
        match self {
            Verdict::Holds => "holds",
            Verdict::Fails => "fails",
            Verdict::Unresolved => "unresolved",
        }
    }
}

/// One condition under one policy (and one repository for C3).
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConditionCell {
    pub id: &'static str,
    pub stratum: String,
    pub policy: String,
    /// `all`, or the repository for C3.
    pub scope: String,
    pub verdict: Verdict,
    pub n: usize,
    pub too_small_to_read: bool,
    pub detail: String,
}

/// One condition on one stratum, over every policy.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ConditionVerdict {
    pub id: &'static str,
    pub stratum: String,
    pub title: String,
    pub verdict: Verdict,
    pub n: usize,
    pub too_small_to_read: bool,
    pub detail: String,
}

fn title(id: &str, stratum: &str) -> &'static str {
    match (id, stratum) {
        ("C2", _) => "complete-success drops by at most 2 pp",
        ("C3", _) => "no repository loses more than one question",
        ("C4", _) => "reader precision drops by at most 2 points",
        ("C5", "dependency") => {
            "at least 15% fewer tokens at preserved success (or +10 pp success at tokens within +5%), interval excludes 0, both policies"
        }
        ("C5", _) => "tokens at most 5% worse (no-harm test)",
        ("C6", _) => "calls and pops each at most 5% worse",
        _ => "",
    }
}

/// Sums of a metric over a stratum's pairs.
#[derive(Clone, Copy, Debug)]
struct Totals {
    a: f64,
    b: f64,
}

impl Totals {
    /// `(B - A) / A`; `None` when A is zero and B is not.
    fn rel(&self) -> Option<f64> {
        if self.a == 0.0 {
            (self.b == 0.0).then_some(0.0)
        } else {
            Some((self.b - self.a) / self.a)
        }
    }

    /// B is at most `limit` (a fraction) worse than A.
    fn within(&self, limit: f64) -> bool {
        self.rel().is_some_and(|r| r <= limit + LIMIT_EPS)
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct RepoCount {
    n: usize,
    lost: usize,
    gained: usize,
}

/// What the conditions read for one policy on one stratum.
struct Stats {
    n: usize,
    complete_a: usize,
    complete_b: usize,
    tokens: Totals,
    calls: Totals,
    pops: Totals,
    repos: BTreeMap<String, RepoCount>,
    /// q3 questions with a precision on both sides, and the means.
    precision_n: usize,
    precision_a: f64,
    precision_b: f64,
    token_ci: Option<(f64, f64)>,
    success_ci: Option<(f64, f64)>,
}

fn stats_of(sel: &[&Paired], resamples: usize, seed: u64) -> Stats {
    let totals = |value: ValueOf| {
        let obs = observations(sel, value);
        Totals {
            a: obs.iter().map(|o| o.1).sum::<f64>(),
            b: obs.iter().map(|o| o.2).sum::<f64>(),
        }
    };
    let ci = |value: ValueOf| {
        intervals(&clusters_of(&observations(sel, value)), resamples, seed).map(|i| i.delta)
    };
    let mut repos: BTreeMap<String, RepoCount> = BTreeMap::new();
    for p in sel {
        let count = repos.entry(p.a.repo.clone()).or_default();
        count.n += 1;
        count.lost += usize::from(p.a.score.complete && !p.b.score.complete);
        count.gained += usize::from(!p.a.score.complete && p.b.score.complete);
    }
    let precisions: Vec<(f64, f64)> = sel
        .iter()
        .filter(|p| p.a.qtype == SETTING_TO_READER)
        .filter_map(|p| Some((p.a.score.reader_precision?, p.b.score.reader_precision?)))
        .collect();
    let precision_n = precisions.len();
    let mean_of = |pick: fn(&(f64, f64)) -> f64| {
        if precision_n == 0 {
            0.0
        } else {
            precisions.iter().map(pick).sum::<f64>() / precision_n as f64
        }
    };
    Stats {
        n: sel.len(),
        complete_a: sel.iter().filter(|p| p.a.score.complete).count(),
        complete_b: sel.iter().filter(|p| p.b.score.complete).count(),
        tokens: totals(v_tokens),
        calls: totals(v_calls),
        pops: totals(v_pops),
        repos,
        precision_n,
        precision_a: mean_of(|p| p.0),
        precision_b: mean_of(|p| p.1),
        token_ci: ci(v_tokens),
        success_ci: ci(v_complete),
    }
}

fn pct(count: usize, n: usize) -> f64 {
    if n == 0 {
        0.0
    } else {
        count as f64 * 100.0 / n as f64
    }
}

fn fmt_rel(rel: Option<f64>) -> String {
    rel.map_or_else(|| "n/a".to_owned(), |r| format!("{:+.1}%", r * 100.0))
}

fn fmt_ci(ci: Option<(f64, f64)>) -> String {
    ci.map_or_else(|| "n/a".to_owned(), |(l, h)| format!("[{l:.2}, {h:.2}]"))
}

fn cell(
    id: &'static str,
    stratum: &str,
    policy: &str,
    scope: &str,
    n: usize,
    verdict: Verdict,
    detail: String,
) -> ConditionCell {
    ConditionCell {
        id,
        stratum: stratum.to_owned(),
        policy: policy.to_owned(),
        scope: scope.to_owned(),
        verdict,
        n,
        too_small_to_read: n < TOO_SMALL,
        detail,
    }
}

fn holds_if(ok: bool) -> Verdict {
    if ok { Verdict::Holds } else { Verdict::Fails }
}

fn combine(cells: &[&ConditionCell]) -> Verdict {
    if cells.iter().any(|c| c.verdict == Verdict::Fails) {
        Verdict::Fails
    } else if !cells.is_empty() && cells.iter().all(|c| c.verdict == Verdict::Holds) {
        Verdict::Holds
    } else {
        Verdict::Unresolved
    }
}

/// `greedy holds, bfs fails`, with the scope after the policy when it is not
/// the whole stratum.
fn list(cells: &[&ConditionCell]) -> String {
    cells
        .iter()
        .map(|c| {
            if c.scope == "all" {
                format!("{} {}", c.policy, c.verdict.name())
            } else {
                format!("{}/{} {}", c.policy, c.scope, c.verdict.name())
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// C5 on the dependency stratum: both branches, per policy.
/// Returns (fewer tokens at preserved success, more success within +5%
/// tokens, detail).
fn c5_branches(s: &Stats) -> (bool, bool, String) {
    let saving = s
        .tokens
        .rel()
        .is_some_and(|r| r <= -TOKEN_SAVING + LIMIT_EPS);
    let preserved = s.complete_b >= s.complete_a;
    let token_excludes_zero = s.token_ci.is_some_and(|(_, hi)| hi < 0.0);
    let gain_pp = pct(s.complete_b, s.n) - pct(s.complete_a, s.n);
    let success_excludes_zero = s.success_ci.is_some_and(|(lo, _)| lo > 0.0);
    let fewer = saving && preserved && token_excludes_zero;
    let more = gain_pp >= SUCCESS_GAIN_PP - LIMIT_EPS
        && s.tokens.within(WORSE_LIMIT)
        && success_excludes_zero;
    let detail = format!(
        "tokens {} (interval of the delta {}), complete A {} B {} of {}, success gain {:+.1} pp \
         (interval {}); fewer-tokens branch {}, more-success branch {}",
        fmt_rel(s.tokens.rel()),
        fmt_ci(s.token_ci),
        s.complete_a,
        s.complete_b,
        s.n,
        gain_pp,
        fmt_ci(s.success_ci),
        fewer,
        more,
    );
    (fewer, more, detail)
}

/// The overall C5 verdict on the dependency stratum.
fn c5_dependency(
    n: usize,
    branches: &[(bool, bool)],
    mine: &[&ConditionCell],
) -> (Verdict, String) {
    if n < TOO_SMALL {
        return (
            Verdict::Unresolved,
            format!(
                "the dependency half has {n} questions, under {TOO_SMALL}: it cannot meet this condition"
            ),
        );
    }
    let labels: BTreeSet<&str> = mine.iter().map(|c| c.policy.as_str()).collect();
    if labels != BTreeSet::from(["bfs", "greedy"]) {
        return (
            Verdict::Unresolved,
            format!(
                "the policy labels must be exactly greedy and bfs to check the direction, got {}",
                labels.into_iter().collect::<Vec<_>>().join(", ")
            ),
        );
    }
    let fewer = branches.iter().all(|b| b.0);
    let more = branches.iter().all(|b| b.1);
    if fewer || more {
        let which = if fewer {
            "fewer tokens at preserved success"
        } else {
            "more success at tokens within +5%"
        };
        (
            Verdict::Holds,
            format!("{which} under every policy ({})", list(mine)),
        )
    } else {
        (
            Verdict::Fails,
            format!("no branch holds under every policy ({})", list(mine)),
        )
    }
}

/// Every condition of one stratum: the cells per policy (and repository) and
/// the overall verdicts.
fn evaluate(
    stratum: &str,
    by_policy: &BTreeMap<String, Stats>,
) -> (Vec<ConditionCell>, Vec<ConditionVerdict>) {
    let ids: &[&'static str] = if stratum == "config" {
        &["C2", "C3", "C4", "C5", "C6"]
    } else {
        &["C2", "C3", "C5", "C6"]
    };
    let n = by_policy.values().map(|s| s.n).max().unwrap_or(0);
    let mut cells: Vec<ConditionCell> = Vec::new();
    let mut branches: Vec<(bool, bool)> = Vec::new();
    for (policy, s) in by_policy {
        let empty = s.n == 0;

        // C2
        let (pa, pb) = (pct(s.complete_a, s.n), pct(s.complete_b, s.n));
        let drop = pa - pb;
        let verdict = if empty {
            Verdict::Unresolved
        } else {
            holds_if(drop <= MAX_SUCCESS_DROP_PP + LIMIT_EPS)
        };
        cells.push(cell(
            "C2",
            stratum,
            policy,
            "all",
            s.n,
            verdict,
            format!(
                "complete A {}/{} ({pa:.1}%), B {}/{} ({pb:.1}%), drop {drop:.1} pp (limit {MAX_SUCCESS_DROP_PP:.1})",
                s.complete_a, s.n, s.complete_b, s.n
            ),
        ));

        // C3
        for (repo, count) in &s.repos {
            cells.push(cell(
                "C3",
                stratum,
                policy,
                repo,
                count.n,
                holds_if(count.lost <= MAX_LOST_PER_REPO),
                format!(
                    "lost {} (limit {MAX_LOST_PER_REPO}), gained {}",
                    count.lost, count.gained
                ),
            ));
        }

        // C4 (config): reader precision on the q3 questions.
        if stratum == "config" {
            let drop = (s.precision_a - s.precision_b) * 100.0;
            let verdict = if s.precision_n == 0 {
                Verdict::Unresolved
            } else {
                holds_if(drop <= MAX_PRECISION_DROP_PTS + LIMIT_EPS)
            };
            cells.push(cell(
                "C4",
                stratum,
                policy,
                "all",
                s.precision_n,
                verdict,
                format!(
                    "reader precision A {:.1}%, B {:.1}%, drop {drop:.1} points (limit {MAX_PRECISION_DROP_PTS:.1})",
                    s.precision_a * 100.0,
                    s.precision_b * 100.0
                ),
            ));
        }

        // C5
        if stratum == "dependency" {
            let (fewer, more, detail) = c5_branches(s);
            branches.push((fewer, more));
            let verdict = if n < TOO_SMALL {
                Verdict::Unresolved
            } else {
                holds_if(fewer || more)
            };
            cells.push(cell("C5", stratum, policy, "all", s.n, verdict, detail));
        } else {
            let verdict = if empty {
                Verdict::Unresolved
            } else {
                holds_if(s.tokens.within(WORSE_LIMIT))
            };
            cells.push(cell(
                "C5",
                stratum,
                policy,
                "all",
                s.n,
                verdict,
                format!(
                    "tokens {} (limit {:+.0}%)",
                    fmt_rel(s.tokens.rel()),
                    WORSE_LIMIT * 100.0
                ),
            ));
        }

        // C6
        let ok = s.calls.within(WORSE_LIMIT) && s.pops.within(WORSE_LIMIT);
        cells.push(cell(
            "C6",
            stratum,
            policy,
            "all",
            s.n,
            if empty {
                Verdict::Unresolved
            } else {
                holds_if(ok)
            },
            format!(
                "calls {}, pops {} (limit {:+.0}% each)",
                fmt_rel(s.calls.rel()),
                fmt_rel(s.pops.rel()),
                WORSE_LIMIT * 100.0
            ),
        ));
    }

    let mut verdicts = Vec::new();
    for &id in ids {
        let mine: Vec<&ConditionCell> = cells.iter().filter(|c| c.id == id).collect();
        let (verdict, detail) = if by_policy.is_empty() {
            (
                Verdict::Unresolved,
                "no paired questions in this stratum".to_owned(),
            )
        } else if id == "C5" && stratum == "dependency" {
            c5_dependency(n, &branches, &mine)
        } else {
            (combine(&mine), list(&mine))
        };
        verdicts.push(ConditionVerdict {
            id,
            stratum: stratum.to_owned(),
            title: title(id, stratum).to_owned(),
            verdict,
            n,
            too_small_to_read: n < TOO_SMALL,
            detail,
        });
    }
    (cells, verdicts)
}

// ----------------------------------------------------------------- context

/// Means over the questions of one arm.
#[derive(Clone, Debug, Default, Serialize)]
pub(crate) struct ArmMeans {
    pub n: usize,
    pub success: Option<f64>,
    pub charged_tokens: Option<f64>,
    pub charged_calls: Option<f64>,
    pub charged_pops: Option<f64>,
}

fn arm_means(rows: &[&ScoreRow]) -> ArmMeans {
    let n = rows.len();
    if n == 0 {
        return ArmMeans::default();
    }
    let mean = |value: ValueOf| -> Option<f64> {
        Some(rows.iter().filter_map(|r| value(r)).sum::<f64>() / n as f64)
    };
    ArmMeans {
        n,
        success: mean(v_complete),
        charged_tokens: mean(v_tokens),
        charged_calls: mean(v_calls),
        charged_pops: mean(v_pops),
    }
}

/// A, B and (context only) R over the same questions of one policy and
/// stratum.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct ContextRow {
    pub policy: String,
    pub stratum: String,
    pub a: ArmMeans,
    pub b: ArmMeans,
    /// R on the questions A and B share; `None` when no R run was given for
    /// the policy. Not part of the ship rule.
    pub r: Option<ArmMeans>,
}

// ------------------------------------------------------------------ compare

#[derive(Clone, Debug, Serialize)]
pub(crate) struct Comparison {
    pub split: String,
    pub seed: u64,
    pub resamples: usize,
    /// The xtask hashes of the runs compared (one when the arms are frozen).
    pub runners: Vec<String>,
    pub policies: Vec<String>,
    /// Conditions C2 to C6 per stratum, over every policy.
    pub conditions: Vec<ConditionVerdict>,
    /// The same, per policy (and repository for C3).
    pub cells: Vec<ConditionCell>,
    pub metrics: Vec<MetricRow>,
    pub context: Vec<ContextRow>,
}

fn select<'a>(paired: &'a [Paired], policy: &str, stratum: &str) -> Vec<&'a Paired> {
    paired
        .iter()
        .filter(|p| p.policy == policy && p.a.stratum == stratum)
        .collect()
}

/// B against A: `pairs` are (policy label, A, B); `context` the optional R
/// runs by policy label; `split` is `dev`, `heldout` or `all`.
pub(crate) fn compare(
    pairs: &[(String, Run, Run)],
    context: Option<&[(String, Run)]>,
    split: &str,
    seed: u64,
) -> Result<Comparison> {
    compare_with(pairs, context, split, seed, RESAMPLES)
}

fn compare_with(
    pairs: &[(String, Run, Run)],
    context: Option<&[(String, Run)]>,
    split: &str,
    seed: u64,
    resamples: usize,
) -> Result<Comparison> {
    if !["dev", "heldout", "all"].contains(&split) {
        bail!("the split must be dev, heldout or all, got {split:?}");
    }
    let mut paired: Vec<Paired> = Vec::new();
    let mut runners: BTreeSet<String> = BTreeSet::new();
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
    for (policy, a, b) in pairs {
        for p in pair_up(policy, a, b, split)? {
            // A question scored in two pairs would count twice.
            if !seen.insert((policy.clone(), p.a.repo.clone(), p.a.question_id.clone())) {
                bail!(
                    "{policy}: question {} of {} appears in two pairs",
                    p.a.question_id,
                    p.a.repo
                );
            }
            paired.push(p);
        }
        runners.extend(a.xtask_hash().map(str::to_owned));
    }
    for (label, r) in context.unwrap_or_default() {
        let mut under_label = pairs.iter().filter(|(l, _, _)| l == label).peekable();
        if under_label.peek().is_none() {
            bail!("{label}: a context run has no pair under that label");
        }
        for (_, a, _) in under_label {
            check_context(label, r, a)?;
        }
    }
    if paired.is_empty() {
        bail!("no question of split {split:?} in the runs");
    }
    let policies: BTreeSet<&str> = paired.iter().map(|p| p.policy.as_str()).collect();
    let strata: BTreeSet<&str> = paired
        .iter()
        .map(|p| p.a.stratum.as_str())
        .chain(STRATA)
        .collect();

    let mut metrics = Vec::new();
    let mut context_rows = Vec::new();
    let mut stats: BTreeMap<&str, BTreeMap<String, Stats>> = BTreeMap::new();
    for &policy in &policies {
        for &stratum in &strata {
            let sel = select(&paired, policy, stratum);
            if sel.is_empty() {
                continue;
            }
            let repos: BTreeSet<&str> = sel.iter().map(|p| p.a.repo.as_str()).collect();
            let types: BTreeSet<&str> = sel.iter().map(|p| p.a.qtype.as_str()).collect();
            let mut groups: Vec<(String, Vec<&Paired>)> = vec![("all".to_owned(), sel.clone())];
            for repo in repos {
                let members = sel.iter().copied().filter(|p| p.a.repo == repo).collect();
                groups.push((format!("repo={repo}"), members));
            }
            for qtype in types {
                let members = sel.iter().copied().filter(|p| p.a.qtype == qtype).collect();
                groups.push((format!("type={qtype}"), members));
            }
            for (group, members) in &groups {
                for def in METRICS {
                    metrics.extend(metric_row(
                        policy, stratum, group, def, members, resamples, seed,
                    ));
                }
            }

            let a_rows: Vec<&ScoreRow> = sel.iter().map(|p| &p.a).collect();
            let b_rows: Vec<&ScoreRow> = sel.iter().map(|p| &p.b).collect();
            let r = context.and_then(|runs| {
                let given: Vec<&Run> = runs
                    .iter()
                    .filter(|(label, _)| label == policy)
                    .map(|(_, run)| run)
                    .collect();
                if given.is_empty() {
                    return None;
                }
                let ids: BTreeSet<&str> = sel.iter().map(|p| p.a.question_id.as_str()).collect();
                let rows: Vec<&ScoreRow> = given
                    .iter()
                    .flat_map(|run| run.rows.iter())
                    .filter(|row| ids.contains(row.question_id.as_str()))
                    .collect();
                Some(arm_means(&rows))
            });
            context_rows.push(ContextRow {
                policy: policy.to_owned(),
                stratum: stratum.to_owned(),
                a: arm_means(&a_rows),
                b: arm_means(&b_rows),
                r,
            });
            stats
                .entry(stratum)
                .or_default()
                .insert(policy.to_owned(), stats_of(&sel, resamples, seed));
        }
    }

    let mut cells = Vec::new();
    let mut conditions = Vec::new();
    let none: BTreeMap<String, Stats> = BTreeMap::new();
    for stratum in STRATA {
        let (stratum_cells, verdicts) = evaluate(stratum, stats.get(stratum).unwrap_or(&none));
        cells.extend(stratum_cells);
        conditions.extend(verdicts);
    }
    Ok(Comparison {
        split: split.to_owned(),
        seed,
        resamples: resamples.max(1),
        runners: runners.into_iter().collect(),
        policies: policies.iter().map(|p| (*p).to_owned()).collect(),
        conditions,
        cells,
        metrics,
        context: context_rows,
    })
}

// ---------------------------------------------------------------- markdown

/// `6 (too small to read)` for n under 10.
fn n_label(n: usize) -> String {
    if n < TOO_SMALL {
        format!("{n} (too small to read)")
    } else {
        n.to_string()
    }
}

fn level(m: &MetricRow, v: f64) -> String {
    if m.fraction {
        format!("{:.1}%", v * 100.0)
    } else {
        format!("{v:.1}")
    }
}

fn change(m: &MetricRow, v: f64) -> String {
    if m.fraction {
        format!("{:+.1} pp", v * 100.0)
    } else {
        format!("{v:+.1}")
    }
}

fn change_ci(m: &MetricRow) -> String {
    m.ci.map_or_else(
        || "n/a".to_owned(),
        |(lo, hi)| format!("[{}, {}]", change(m, lo), change(m, hi)),
    )
}

fn pct_opt(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".to_owned(), |v| format!("{:.1}%", v * 100.0))
}

fn num_opt(v: Option<f64>) -> String {
    v.map_or_else(|| "n/a".to_owned(), |v| format!("{v:.1}"))
}

impl Comparison {
    pub(crate) fn to_markdown(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "# Structured grep: B against A\n");
        let _ = writeln!(
            out,
            "Split `{}`; policies {}; bootstrap {} resamples clustered by family, seed {}; \
             runner sha256 {}. Delta is B - A; wins count questions where B is better. \
             C1 (the correctness fixtures) is not computed here. R is context only.\n",
            self.split,
            self.policies.join(", "),
            self.resamples,
            self.seed,
            self.runners.join(", "),
        );

        let _ = writeln!(out, "## Decision conditions (section 7)\n");
        let _ = writeln!(out, "| Condition | Stratum | n | Verdict | Detail |");
        let _ = writeln!(out, "| --- | --- | ---: | --- | --- |");
        for c in &self.conditions {
            let _ = writeln!(
                out,
                "| {}: {} | {} | {} | {} | {} |",
                c.id,
                c.title,
                c.stratum,
                n_label(c.n),
                c.verdict.name(),
                c.detail
            );
        }

        let _ = writeln!(out, "\n## Per policy\n");
        let _ = writeln!(
            out,
            "| Condition | Stratum | Policy | Scope | n | Verdict | Detail |"
        );
        let _ = writeln!(out, "| --- | --- | --- | --- | ---: | --- | --- |");
        for c in &self.cells {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} | {} | {} | {} |",
                c.id,
                c.stratum,
                c.policy,
                c.scope,
                n_label(c.n),
                c.verdict.name(),
                c.detail
            );
        }

        let _ = writeln!(out, "\n## Metrics\n");
        let mut shown: BTreeSet<(&str, &str)> = BTreeSet::new();
        for m in &self.metrics {
            if !shown.insert((m.policy.as_str(), m.stratum.as_str())) {
                continue;
            }
            let _ = writeln!(out, "### {} / {}\n", m.policy, m.stratum);
            let _ = writeln!(
                out,
                "| Group | Metric | n | A | B | B - A | Interval | Relative | Wins | Losses | Ties |"
            );
            let _ = writeln!(
                out,
                "| --- | --- | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: |"
            );
            for row in self
                .metrics
                .iter()
                .filter(|r| r.policy == m.policy && r.stratum == m.stratum)
            {
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    row.group,
                    row.metric,
                    n_label(row.n),
                    level(row, row.mean_a),
                    level(row, row.mean_b),
                    change(row, row.mean_delta),
                    change_ci(row),
                    fmt_rel(row.rel_delta),
                    row.wins,
                    row.losses,
                    row.ties
                );
            }
            let _ = writeln!(out);
        }

        let _ = writeln!(out, "## Context: A, B and R on the same questions\n");
        let _ = writeln!(
            out,
            "| Policy | Stratum | Arm | n | Complete | Tokens | Calls | Pops |"
        );
        let _ = writeln!(
            out,
            "| --- | --- | --- | ---: | ---: | ---: | ---: | ---: |"
        );
        for c in &self.context {
            let arms = [("A", Some(&c.a)), ("B", Some(&c.b)), ("R", c.r.as_ref())];
            for (name, arm) in arms {
                let Some(arm) = arm else {
                    continue;
                };
                let _ = writeln!(
                    out,
                    "| {} | {} | {} | {} | {} | {} | {} | {} |",
                    c.policy,
                    c.stratum,
                    name,
                    n_label(arm.n),
                    pct_opt(arm.success),
                    num_opt(arm.charged_tokens),
                    num_opt(arm.charged_calls),
                    num_opt(arm.charged_pops)
                );
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::score::QuestionScore;
    use super::*;
    use serde_json::json;

    const Q1: &str = "setting_location";
    const Q2: &str = "behaviour_to_setting";
    const Q3: &str = "setting_to_reader";

    fn base_score() -> QuestionScore {
        QuestionScore {
            complete: true,
            charged_tokens: 1000,
            charged_calls: 10,
            charged_pops: 5,
            tool_calls: 25,
            tokens_read: 5000,
            ..QuestionScore::default()
        }
    }

    /// `fams` families of one repo and stratum: three questions each in the
    /// config stratum, one in the dependency stratum.
    fn family_rows(repo: &str, stratum: &str, split: &str, fams: usize) -> Vec<ScoreRow> {
        let types: &[&str] = if stratum == "dependency" {
            &[Q1]
        } else {
            &[Q1, Q2, Q3]
        };
        let mut out = Vec::new();
        for i in 0..fams {
            let family = format!("{repo}.{stratum}.{split}.f{i}");
            for (k, qtype) in types.iter().enumerate() {
                let mut score = base_score();
                if *qtype == Q3 {
                    score.reader_precision = Some(0.5);
                    score.reader_recall = Some(0.5);
                    score.reader_f1 = Some(0.5);
                }
                out.push(ScoreRow {
                    question_id: format!("{family}.q{}", k + 1),
                    family: family.clone(),
                    repo: repo.to_owned(),
                    stratum: stratum.to_owned(),
                    split: split.to_owned(),
                    qtype: (*qtype).to_owned(),
                    score,
                });
            }
        }
        out
    }

    /// Three config repos with `config_fams` held-out families each (36
    /// questions at 4), two dependency repos with `dep_fams` each, and one dev
    /// family that a held-out comparison must leave out.
    fn world(config_fams: usize, dep_fams: usize) -> Vec<ScoreRow> {
        let mut rows = Vec::new();
        for repo in ["lci", "vite", "home-assistant"] {
            rows.extend(family_rows(repo, "config", "heldout", config_fams));
        }
        for repo in ["lci", "vite"] {
            rows.extend(family_rows(repo, "dependency", "heldout", dep_fams));
        }
        rows.extend(family_rows("lci", "config", "dev", 1));
        rows
    }

    fn changed(rows: &[ScoreRow], edit: impl Fn(&mut ScoreRow)) -> Vec<ScoreRow> {
        let mut out = rows.to_vec();
        out.iter_mut().for_each(edit);
        out
    }

    fn config_json() -> Value {
        json!({"max_calls": 25, "max_tokens": 20000, "probe_terms": 8, "read_files": 3})
    }

    /// A run of the given arm, as `run` writes it, in a directory named after
    /// the arm and policy.
    fn run_of(arm: &str, skip: &str, policy: &str, rows: Vec<ScoreRow>) -> Run {
        Run {
            meta: json!({"xtask_sha256": "abc"}),
            rows,
            dir: format!("{arm}-{skip}-{policy}"),
            arm: arm.to_owned(),
            grep_skip: skip.to_owned(),
            policy: policy.to_owned(),
            stratum: "config".to_owned(),
            config: config_json(),
            max_scan_bytes: (arm != "r").then_some(1 << 32),
        }
    }

    /// The A and B runs of one policy label.
    fn pair(policy: &str, a: &[ScoreRow], b: &[ScoreRow]) -> (String, Run, Run) {
        (
            policy.to_owned(),
            run_of("a", "none", policy, a.to_vec()),
            run_of("b", "lockfiles", policy, b.to_vec()),
        )
    }

    /// The same A and B under both policies.
    fn both_policies(a: &[ScoreRow], b: &[ScoreRow]) -> Vec<(String, Run, Run)> {
        ["greedy", "bfs"].iter().map(|p| pair(p, a, b)).collect()
    }

    /// The error of comparing `pairs` (heldout), as text.
    fn refused(pairs: &[(String, Run, Run)]) -> String {
        compare_with(pairs, None, "heldout", 1, 50)
            .unwrap_err()
            .to_string()
    }

    fn cmp(a: &[ScoreRow], b: &[ScoreRow]) -> Comparison {
        compare_with(&both_policies(a, b), None, "heldout", 7, 300).unwrap()
    }

    fn verdict(c: &Comparison, id: &str, stratum: &str) -> Verdict {
        c.conditions
            .iter()
            .find(|v| v.id == id && v.stratum == stratum)
            .unwrap_or_else(|| panic!("no {id} on {stratum}"))
            .verdict
    }

    fn metric<'a>(
        c: &'a Comparison,
        policy: &str,
        stratum: &str,
        group: &str,
        name: &str,
    ) -> &'a MetricRow {
        c.metrics
            .iter()
            .find(|m| {
                m.policy == policy && m.stratum == stratum && m.group == group && m.metric == name
            })
            .unwrap_or_else(|| panic!("no {name} for {policy}/{stratum}/{group}"))
    }

    #[test]
    fn constant_deltas_give_a_degenerate_interval() {
        let obs: Vec<Obs> = vec![
            (("r", "f1"), 1.0, 1.5),
            (("r", "f1"), 1.0, 1.5),
            (("r", "f2"), 1.0, 1.5),
        ];
        let found = intervals(&clusters_of(&obs), 10_000, 20_261_018).unwrap();
        assert_eq!(found.delta, (0.5, 0.5));
        assert_eq!(found.rel, Some((0.5, 0.5)));
        // Without clusters there is nothing to resample.
        assert_eq!(intervals(&[], 100, 1), None);
        // A's total is zero: the delta has an interval, the relative change none.
        let from_zero: Vec<Obs> = vec![(("r", "f"), 0.0, 2.0), (("r", "g"), 0.0, 2.0)];
        let found = intervals(&clusters_of(&from_zero), 100, 1).unwrap();
        assert_eq!(found.delta, (2.0, 2.0));
        assert_eq!(found.rel, None);
    }

    #[test]
    fn resampling_keeps_whole_families() {
        // One family of three questions at +1, one of a single question at -1.
        // Whole families give only means of +1 (both draws the first), -1 (both
        // the second) and 0.5 (one of each); resampling questions would give
        // others, such as 1/3.
        let obs: Vec<Obs> = vec![
            (("r", "big"), 0.0, 1.0),
            (("r", "big"), 0.0, 1.0),
            (("r", "big"), 0.0, 1.0),
            (("r", "small"), 1.0, 0.0),
        ];
        let clusters = clusters_of(&obs);
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].n + clusters[1].n, 4);
        let mut seen: BTreeSet<i64> = BTreeSet::new();
        for (delta, _) in draws(&clusters, 2_000, 99) {
            assert!(
                [1.0, -1.0, 0.5].contains(&delta),
                "a resample mean of {delta} splits a family"
            );
            seen.insert((delta * 2.0) as i64);
        }
        assert_eq!(seen.len(), 3, "all three mixes should occur");
        let found = intervals(&clusters, 2_000, 99).unwrap();
        assert_eq!(found.delta, (-1.0, 1.0));
        // The same seed gives the same interval; another seed may not.
        assert_eq!(found, intervals(&clusters, 2_000, 99).unwrap());
    }

    #[test]
    fn a_missing_pair_or_a_different_runner_is_an_error() {
        let a = world(4, 6);
        let mut short = a.clone();
        let dropped = short.pop().unwrap();
        // The dropped question is a dev one: it only matters for `all`.
        assert!(compare_with(&both_policies(&a, &short), None, "heldout", 1, 50).is_ok());
        let err = compare_with(&both_policies(&a, &short), None, "all", 1, 50)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains(&dropped.question_id) && err.contains("missing pair"),
            "{err}"
        );
        let err = compare_with(&both_policies(&short, &a), None, "all", 1, 50)
            .unwrap_err()
            .to_string();
        assert!(err.contains("missing pair"), "{err}");

        // A held-out question missing from B.
        let b: Vec<ScoreRow> = a.iter().skip(1).cloned().collect();
        assert!(compare_with(&both_policies(&a, &b), None, "heldout", 1, 50).is_err());

        let mut pairs = both_policies(&a, &a);
        pairs[1].2.meta = json!({"xtask_sha256": "other"});
        let err = compare_with(&pairs, None, "heldout", 1, 50)
            .unwrap_err()
            .to_string();
        assert!(err.contains("different runners"), "{err}");
        pairs[1].2.meta = json!({});
        let err = compare_with(&pairs, None, "heldout", 1, 50)
            .unwrap_err()
            .to_string();
        assert!(err.contains("xtask_sha256"), "{err}");

        // Rows that disagree on labels, repeated ids, a bad split.
        let relabelled = changed(&a, |r| r.repo = "other".to_owned());
        assert!(compare_with(&both_policies(&a, &relabelled), None, "heldout", 1, 50).is_err());
        let mut twice = a.clone();
        twice.push(a[0].clone());
        assert!(compare_with(&both_policies(&twice, &a), None, "heldout", 1, 50).is_err());
        assert!(compare_with(&both_policies(&a, &a), None, "bogus", 1, 50).is_err());
        assert!(compare_with(&[], None, "heldout", 1, 50).is_err());
    }

    #[test]
    fn identical_arms_hold_every_condition_that_can_be_met() {
        let a = world(4, 6);
        // Dependency tokens 30% lower in B, everything else equal.
        let b = changed(&a, |r| {
            if r.stratum == "dependency" {
                r.score.charged_tokens = 700;
            }
        });
        let c = cmp(&a, &b);
        // The dev family is left out: 36 config and 12 dependency questions.
        assert_eq!(metric(&c, "greedy", "config", "all", "complete").n, 36);
        assert_eq!(metric(&c, "greedy", "dependency", "all", "complete").n, 12);
        for stratum in ["config", "dependency"] {
            for id in ["C2", "C3", "C5", "C6"] {
                assert_eq!(verdict(&c, id, stratum), Verdict::Holds, "{id} {stratum}");
            }
        }
        assert_eq!(verdict(&c, "C4", "config"), Verdict::Holds);
        // 5 conditions on config, 4 on dependency.
        assert_eq!(c.conditions.len(), 9);
        let tokens = metric(&c, "bfs", "dependency", "all", "charged_tokens");
        assert_eq!((tokens.wins, tokens.losses, tokens.ties), (12, 0, 0));
        assert_eq!(tokens.ci, Some((-300.0, -300.0)));
        assert!((tokens.rel_delta.unwrap() + 0.3).abs() < 1e-9);
        let complete = metric(&c, "greedy", "config", "all", "complete");
        assert_eq!((complete.wins, complete.losses, complete.ties), (0, 0, 36));
        // The per-repo and per-type groups exist, each with its n.
        assert_eq!(metric(&c, "greedy", "config", "repo=lci", "complete").n, 12);
        assert_eq!(
            metric(&c, "greedy", "config", "type=setting_location", "complete").n,
            12
        );
        // hit@5 leaves q3 out; reader scores are q3 only.
        assert_eq!(metric(&c, "greedy", "config", "all", "hit_at_5").n, 24);
        assert_eq!(
            metric(&c, "greedy", "config", "all", "reader_precision").n,
            12
        );
        assert!(
            c.metrics
                .iter()
                .all(|m| m.stratum != "dependency" || !m.metric.starts_with("reader"))
        );
        assert_eq!(c.runners, vec!["abc".to_owned()]);
        assert_eq!(c.policies, vec!["bfs".to_owned(), "greedy".to_owned()]);
    }

    #[test]
    fn the_dev_split_compares_the_dev_questions() {
        let a = world(4, 6);
        let c = compare_with(&both_policies(&a, &a), None, "dev", 1, 50).unwrap();
        assert_eq!(metric(&c, "greedy", "config", "all", "complete").n, 3);
        // No dependency question is dev here: those conditions are unresolved.
        assert_eq!(verdict(&c, "C2", "dependency"), Verdict::Unresolved);
        let all = compare_with(&both_policies(&a, &a), None, "all", 1, 50).unwrap();
        assert_eq!(metric(&all, "greedy", "config", "all", "complete").n, 39);
    }

    #[test]
    fn one_lost_question_breaks_c2_and_two_in_a_repo_break_c3() {
        let a = world(4, 6);
        let lose = |n: usize, repo: &'static str| {
            let left = std::cell::Cell::new(n);
            changed(&a, move |r| {
                if r.repo == repo && r.stratum == "config" && r.split == "heldout" && left.get() > 0
                {
                    left.set(left.get() - 1);
                    r.score.complete = false;
                    r.score.miss_stop = Some("calls".to_owned());
                }
            })
        };
        // One question of 36 is 2.8 pp: more than the 2 allowed.
        let c = cmp(&a, &lose(1, "lci"));
        assert_eq!(verdict(&c, "C2", "config"), Verdict::Fails);
        let cell = c
            .cells
            .iter()
            .find(|x| x.id == "C2" && x.stratum == "config" && x.policy == "greedy")
            .unwrap();
        assert!(
            cell.detail.contains("35/36") && cell.detail.contains("2.8"),
            "{}",
            cell.detail
        );
        // One loss in one repo is within C3.
        assert_eq!(verdict(&c, "C3", "config"), Verdict::Holds);
        // Two in the same repo are not.
        let c = cmp(&a, &lose(2, "lci"));
        assert_eq!(verdict(&c, "C3", "config"), Verdict::Fails);
        let lci = c
            .cells
            .iter()
            .find(|x| x.id == "C3" && x.scope == "lci" && x.policy == "bfs")
            .unwrap();
        assert_eq!((lci.verdict, lci.n), (Verdict::Fails, 12));
        let vite = c
            .cells
            .iter()
            .find(|x| x.id == "C3" && x.scope == "vite")
            .unwrap();
        assert_eq!(vite.verdict, Verdict::Holds);
        // The dependency stratum is untouched.
        assert_eq!(verdict(&c, "C2", "dependency"), Verdict::Holds);
        // A loss shows as a loss in the metric.
        let complete = metric(&c, "greedy", "config", "all", "complete");
        assert_eq!((complete.wins, complete.losses, complete.ties), (0, 2, 34));
    }

    #[test]
    fn reader_precision_cost_and_work_conditions() {
        let a = world(4, 6);
        // Precision 0.5 -> 0.45: five points down.
        let c = cmp(
            &a,
            &changed(&a, |r| {
                if let Some(p) = r.score.reader_precision.as_mut() {
                    *p = 0.45;
                }
            }),
        );
        assert_eq!(verdict(&c, "C4", "config"), Verdict::Fails);
        // 0.5 -> 0.49: one point down, allowed.
        let c = cmp(
            &a,
            &changed(&a, |r| {
                if let Some(p) = r.score.reader_precision.as_mut() {
                    *p = 0.49;
                }
            }),
        );
        assert_eq!(verdict(&c, "C4", "config"), Verdict::Holds);

        // Config tokens +6%: the no-harm test fails; +5% is allowed.
        let c = cmp(&a, &changed(&a, |r| r.score.charged_tokens = 1060));
        assert_eq!(verdict(&c, "C5", "config"), Verdict::Fails);
        let c = cmp(&a, &changed(&a, |r| r.score.charged_tokens = 1050));
        assert_eq!(verdict(&c, "C5", "config"), Verdict::Holds);

        // Calls +10% fail C6; pops from 5 to 6 (+20%) fail it too.
        let c = cmp(&a, &changed(&a, |r| r.score.charged_calls = 11));
        assert_eq!(verdict(&c, "C6", "config"), Verdict::Fails);
        let c = cmp(&a, &changed(&a, |r| r.score.charged_pops = 6));
        assert_eq!(verdict(&c, "C6", "dependency"), Verdict::Fails);
        // Fewer calls and pops hold.
        let c = cmp(
            &a,
            &changed(&a, |r| {
                r.score.charged_calls = 5;
                r.score.charged_pops = 0;
            }),
        );
        assert_eq!(verdict(&c, "C6", "config"), Verdict::Holds);
    }

    #[test]
    fn c5_is_unresolved_when_the_dependency_half_is_under_ten() {
        // Three families per repository: six dependency questions.
        let a = world(4, 3);
        let b = changed(&a, |r| {
            if r.stratum == "dependency" {
                r.score.charged_tokens = 100;
            }
        });
        let c = cmp(&a, &b);
        let dep = c
            .conditions
            .iter()
            .find(|v| v.id == "C5" && v.stratum == "dependency")
            .unwrap();
        assert_eq!(dep.verdict, Verdict::Unresolved);
        assert_eq!(dep.n, 6);
        assert!(dep.too_small_to_read);
        assert!(dep.detail.contains("under 10"), "{}", dep.detail);
        // The other conditions still read the data, flagged as small.
        assert_eq!(verdict(&c, "C2", "dependency"), Verdict::Holds);
        let c2 = c
            .conditions
            .iter()
            .find(|v| v.id == "C2" && v.stratum == "dependency")
            .unwrap();
        assert!(c2.too_small_to_read);
        // Per-repo cells are smaller still.
        let repo_cell = c
            .cells
            .iter()
            .find(|x| x.id == "C3" && x.stratum == "dependency" && x.scope == "vite")
            .unwrap();
        assert_eq!(repo_cell.n, 3);
        assert!(repo_cell.too_small_to_read);
        // The config stratum (36) is not small.
        let config = c
            .conditions
            .iter()
            .find(|v| v.id == "C2" && v.stratum == "config")
            .unwrap();
        assert!(!config.too_small_to_read);

        // The report says so.
        let md = c.to_markdown();
        assert!(md.contains("6 (too small to read)"), "{md}");
        assert!(md.contains("unresolved"), "{md}");
        assert!(!md.contains("36 (too small"), "{md}");
    }

    #[test]
    fn c5_needs_saved_tokens_or_more_success_in_both_policies() {
        let a = world(4, 6);
        let dep = |tokens: u64| {
            changed(&a, move |r| {
                if r.stratum == "dependency" {
                    r.score.charged_tokens = tokens;
                }
            })
        };
        // 14% fewer: short of 15%.
        let c = cmp(&a, &dep(860));
        assert_eq!(verdict(&c, "C5", "dependency"), Verdict::Fails);
        // Exactly 15% fewer holds.
        let c = cmp(&a, &dep(850));
        assert_eq!(verdict(&c, "C5", "dependency"), Verdict::Holds);

        // 30% fewer tokens but one dependency question lost: success not preserved.
        let mut b = dep(700);
        let lost = b
            .iter_mut()
            .find(|r| r.stratum == "dependency" && r.repo == "lci")
            .unwrap();
        lost.score.complete = false;
        let c = cmp(&a, &b);
        assert_eq!(verdict(&c, "C5", "dependency"), Verdict::Fails);
        // The other branch needs +10 pp success: one question of twelve is not.
        assert_eq!(verdict(&c, "C2", "dependency"), Verdict::Fails);

        // A second branch: six more questions complete at unchanged tokens.
        let mut weak = a.clone();
        let mut left = 6;
        for r in weak.iter_mut().filter(|r| r.stratum == "dependency") {
            if left > 0 {
                left -= 1;
                r.score.complete = false;
                r.score.miss_stop = Some("tokens".to_owned());
            }
        }
        let c = cmp(&weak, &a);
        assert_eq!(verdict(&c, "C5", "dependency"), Verdict::Holds);
        let cell = c
            .cells
            .iter()
            .find(|x| x.id == "C5" && x.stratum == "dependency")
            .unwrap();
        assert!(
            cell.detail.contains("more-success branch true"),
            "{}",
            cell.detail
        );

        // The policies must satisfy the same branch: bfs saves tokens, greedy does not.
        let saving = dep(700);
        let pairs = vec![pair("greedy", &a, &a), pair("bfs", &a, &saving)];
        let c = compare_with(&pairs, None, "heldout", 1, 100).unwrap();
        assert_eq!(verdict(&c, "C5", "dependency"), Verdict::Fails);

        // One policy only cannot show agreement.
        let c = compare_with(&pairs[1..], None, "heldout", 1, 100).unwrap();
        assert_eq!(verdict(&c, "C5", "dependency"), Verdict::Unresolved);
        // But the conditions that need one policy still resolve.
        assert_eq!(verdict(&c, "C2", "config"), Verdict::Holds);
    }

    #[test]
    fn a_stratum_with_no_questions_is_unresolved() {
        let config_only: Vec<ScoreRow> = world(4, 6)
            .into_iter()
            .filter(|r| r.stratum == "config")
            .collect();
        let c = cmp(&config_only, &config_only);
        for id in ["C2", "C3", "C5", "C6"] {
            let v = c
                .conditions
                .iter()
                .find(|v| v.id == id && v.stratum == "dependency")
                .unwrap();
            assert_eq!(v.verdict, Verdict::Unresolved);
            assert_eq!(v.n, 0);
        }
        assert_eq!(verdict(&c, "C2", "config"), Verdict::Holds);
    }

    #[test]
    fn wins_losses_and_ties_follow_the_metric_direction() {
        let a = world(4, 6);
        let seen = std::cell::Cell::new(0);
        let b = changed(&a, |r| {
            if r.stratum == "config" && r.split == "heldout" && r.qtype == Q1 {
                let k = seen.get();
                seen.set(k + 1);
                // 12 setting_location questions: 5 cheaper, 2 dearer, 5 equal.
                if k < 5 {
                    r.score.charged_tokens = 900;
                } else if k < 7 {
                    r.score.charged_tokens = 1100;
                }
            }
        });
        let c = cmp(&a, &b);
        let tokens = metric(
            &c,
            "greedy",
            "config",
            "type=setting_location",
            "charged_tokens",
        );
        assert_eq!((tokens.wins, tokens.losses, tokens.ties), (5, 2, 5));
        assert_eq!(tokens.n, 12);
        assert!((tokens.mean_delta - (-500.0 + 200.0) / 12.0).abs() < 1e-9);
        // Success rose nowhere and fell nowhere.
        let complete = metric(&c, "greedy", "config", "type=setting_location", "complete");
        assert_eq!(complete.ties, 12);
    }

    #[test]
    fn r_is_a_context_column_only() {
        let a = world(4, 6);
        let b = a.clone();
        // R completes 3 of 4 config q1 questions it is asked, at higher cost.
        let r_rows = changed(&a, |r| {
            r.score.charged_tokens = 2000;
            if r.qtype == Q1 && r.question_id.ends_with("f0.q1") {
                r.score.complete = false;
            }
        });
        let r_runs = vec![("greedy".to_owned(), run_of("r", "rg", "greedy", r_rows))];
        let c = compare_with(&both_policies(&a, &b), Some(&r_runs), "heldout", 1, 100).unwrap();
        let row = |policy: &str, stratum: &str| {
            c.context
                .iter()
                .find(|x| x.policy == policy && x.stratum == stratum)
                .unwrap()
        };
        let greedy = row("greedy", "config");
        let r = greedy.r.as_ref().unwrap();
        assert_eq!(r.n, 36);
        // f0 of each of three repos loses its q1: 33 of 36.
        assert!((r.success.unwrap() - 33.0 / 36.0).abs() < 1e-12);
        assert_eq!(r.charged_tokens, Some(2000.0));
        assert_eq!(greedy.a.n, 36);
        assert_eq!(greedy.b.success, Some(1.0));
        // No R run for bfs.
        assert!(row("bfs", "config").r.is_none());
        // The verdicts do not move.
        let plain = cmp(&a, &b);
        for (x, y) in c.conditions.iter().zip(&plain.conditions) {
            assert_eq!(x.verdict, y.verdict);
        }
        let md = c.to_markdown();
        assert!(md.contains("| greedy | config | R |"), "{md}");
        assert!(!md.contains("| bfs | config | R |"), "{md}");
    }

    #[test]
    fn the_report_serializes_and_prints() {
        let a = world(4, 6);
        let c = cmp(&a, &a);
        let value = serde_json::to_value(&c).unwrap();
        assert_eq!(value["split"], "heldout");
        assert_eq!(value["seed"], 7);
        assert_eq!(value["conditions"].as_array().unwrap().len(), 9);
        let first = &value["conditions"][0];
        assert!(["holds", "fails", "unresolved"].contains(&first["verdict"].as_str().unwrap()));
        let metric = &value["metrics"][0];
        assert!(metric["ci"].is_array());
        assert!(metric["n"].is_number());
        let md = c.to_markdown();
        assert!(md.starts_with("# Structured grep"));
        assert!(md.contains("## Decision conditions"));
        assert!(md.contains("C5: tokens at most 5% worse"));
    }

    #[test]
    fn a_run_dir_is_read_from_summary_and_meta() {
        let dir =
            std::env::temp_dir().join(format!("lci-sim-structured-run-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let rows = world(1, 1);
        let summary = json!({
            "arm": "b", "grep_skip": "lockfiles", "policy": "bfs", "stratum": "config",
            "config": config_json(), "matcher": {"kind": "live_grep", "max_scan_bytes": 77},
            "rows": rows, "aggregates": {},
        });
        std::fs::write(
            dir.join("summary.json"),
            serde_json::to_string(&summary).unwrap(),
        )
        .unwrap();
        std::fs::write(
            dir.join("meta.json"),
            serde_json::to_string(&json!({"xtask_sha256": "abc"})).unwrap(),
        )
        .unwrap();
        let loaded = load_run(&dir).unwrap();
        assert_eq!(loaded.rows, rows);
        assert_eq!(loaded.xtask_hash(), Some("abc"));
        assert_eq!(
            (
                loaded.arm.as_str(),
                loaded.grep_skip.as_str(),
                loaded.policy.as_str()
            ),
            ("b", "lockfiles", "bfs")
        );
        assert_eq!(loaded.stratum, "config");
        assert_eq!(loaded.config, config_json());
        assert_eq!(loaded.max_scan_bytes, Some(77));
        // Two spellings of one directory are one run dir.
        assert_eq!(load_run(&dir.join(".")).unwrap().dir, loaded.dir);

        // An rg run records no scan cap.
        let mut rg = summary.clone();
        rg["matcher"] = json!({"kind": "rg"});
        std::fs::write(
            dir.join("summary.json"),
            serde_json::to_string(&rg).unwrap(),
        )
        .unwrap();
        assert_eq!(load_run(&dir).unwrap().max_scan_bytes, None);

        // Each setting is required.
        for key in ["arm", "grep_skip", "policy", "stratum", "config"] {
            let mut without = summary.clone();
            without.as_object_mut().unwrap().remove(key);
            std::fs::write(
                dir.join("summary.json"),
                serde_json::to_string(&without).unwrap(),
            )
            .unwrap();
            let err = format!("{:#}", load_run(&dir).unwrap_err());
            assert!(err.contains(&format!("`{key}`")), "{key}: {err}");
        }
        std::fs::write(dir.join("summary.json"), "{}").unwrap();
        assert!(load_run(&dir).is_err());
        std::fs::remove_file(dir.join("meta.json")).unwrap();
        assert!(load_run(&dir).is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_pair_must_be_an_a_run_and_a_b_run_of_one_policy_and_config() {
        let a = world(4, 6);
        let ok = || pair("greedy", &a, &a);
        assert!(refused(&[]).contains("no question"));
        compare_with(&[ok()], None, "heldout", 1, 50).unwrap();

        // The same run dir twice.
        let mut same = ok();
        same.2.dir = same.1.dir.clone();
        assert!(refused(&[same]).contains("same run dir"));

        // A and B swapped, or the wrong grep-skip, or the wrong arm.
        let mut swapped = ok();
        std::mem::swap(&mut swapped.1, &mut swapped.2);
        let err = refused(&[swapped]);
        assert!(
            err.contains("run A is arm b") && err.contains("arm a with none"),
            "{err}"
        );
        let mut skip_a = ok();
        skip_a.1.grep_skip = "lockfiles".to_owned();
        assert!(refused(&[skip_a]).contains("run A is arm a with grep-skip lockfiles"));
        let mut skip_b = ok();
        skip_b.2.grep_skip = "none".to_owned();
        assert!(refused(&[skip_b]).contains("run B is arm b with grep-skip none"));
        let mut rg_b = ok();
        rg_b.2.arm = "r".to_owned();
        rg_b.2.grep_skip = "rg".to_owned();
        assert!(refused(&[rg_b]).contains("run B is arm r"));

        // A run of another policy than the label, on either side.
        let mut other_a = ok();
        other_a.1.policy = "bfs".to_owned();
        let err = refused(&[other_a]);
        assert!(err.contains("run A has the policy bfs"), "{err}");
        let mut other_b = ok();
        other_b.2.policy = "bfs".to_owned();
        assert!(refused(&[other_b]).contains("run B has the policy bfs"));
        // Two pairs that both carry greedy runs cannot make up a second policy.
        let twin = ("bfs".to_owned(), ok().1, ok().2);
        assert!(refused(&[ok(), twin]).contains("not the pair's label"));

        // A different stratum, config, or scan cap.
        let mut stratum = ok();
        stratum.2.stratum = "dependency".to_owned();
        assert!(refused(&[stratum]).contains("different strata"));
        let mut calls = ok();
        calls.2.config["max_calls"] = json!(26);
        assert!(refused(&[calls]).contains("budgets of the arms must be equal"));
        let mut probes = ok();
        probes.1.config["probe_terms"] = json!(6);
        assert!(refused(&[probes]).contains("different settings"));
        let mut cap = ok();
        cap.2.max_scan_bytes = Some(1);
        assert!(refused(&[cap]).contains("different settings"));
    }

    #[test]
    fn a_question_in_two_pairs_of_one_label_is_an_error() {
        let a = world(4, 6);
        let first = pair("greedy", &a, &a);
        let mut second = pair("greedy", &a, &a);
        second.1.dir = "elsewhere-a".to_owned();
        second.2.dir = "elsewhere-b".to_owned();
        let err = refused(&[first.clone(), second]);
        assert!(err.contains("appears in two pairs"), "{err}");
        // Disjoint questions under one label are pooled.
        let config: Vec<ScoreRow> = a
            .iter()
            .filter(|r| r.stratum == "config")
            .cloned()
            .collect();
        let dependency: Vec<ScoreRow> = a
            .iter()
            .filter(|r| r.stratum == "dependency")
            .cloned()
            .collect();
        let mut dep = pair("greedy", &dependency, &dependency);
        dep.1.dir = "dep-a".to_owned();
        dep.2.dir = "dep-b".to_owned();
        dep.1.stratum = "dependency".to_owned();
        dep.2.stratum = "dependency".to_owned();
        let c = compare_with(
            &[pair("greedy", &config, &config), dep],
            None,
            "heldout",
            1,
            50,
        )
        .unwrap();
        assert_eq!(metric(&c, "greedy", "dependency", "all", "complete").n, 12);
        assert_eq!(metric(&c, "greedy", "config", "all", "complete").n, 36);
    }

    #[test]
    fn c5_resolves_only_for_exactly_greedy_and_bfs() {
        let a = world(4, 6);
        let b = changed(&a, |r| {
            if r.stratum == "dependency" {
                r.score.charged_tokens = 700;
            }
        });
        let relabel = |label: &str| pair(label, &a, &b);
        let dep_verdict = |pairs: &[(String, Run, Run)]| {
            let c = compare_with(pairs, None, "heldout", 1, 50).unwrap();
            c.conditions
                .iter()
                .find(|v| v.id == "C5" && v.stratum == "dependency")
                .unwrap()
                .clone()
        };
        let both = dep_verdict(&[relabel("greedy"), relabel("bfs")]);
        assert_eq!(both.verdict, Verdict::Holds);
        // One label.
        let one = dep_verdict(&[relabel("greedy")]);
        assert_eq!(one.verdict, Verdict::Unresolved);
        assert!(
            one.detail.contains("exactly greedy and bfs"),
            "{}",
            one.detail
        );
        // Two labels, but not the registered ones.
        let other = dep_verdict(&[relabel("greedy"), relabel("random")]);
        assert_eq!(other.verdict, Verdict::Unresolved);
        assert!(other.detail.contains("greedy, random"), "{}", other.detail);
        // Three labels.
        let three = dep_verdict(&[relabel("greedy"), relabel("bfs"), relabel("random")]);
        assert_eq!(three.verdict, Verdict::Unresolved);
    }

    #[test]
    fn a_context_run_must_be_an_r_run_with_the_labels_config() {
        let a = world(4, 6);
        let r_run = |arm: &str, policy: &str| run_of(arm, "rg", policy, a.clone());
        let compared = |r: Run, label: &str| {
            let context = vec![(label.to_owned(), r)];
            compare_with(&both_policies(&a, &a), Some(&context), "heldout", 1, 50)
        };
        compared(r_run("r", "greedy"), "greedy").unwrap();
        let err = compared(r_run("a", "greedy"), "greedy")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not r"), "{err}");
        let err = compared(r_run("r", "bfs"), "greedy")
            .unwrap_err()
            .to_string();
        assert!(err.contains("not the label"), "{err}");
        let mut other = r_run("r", "greedy");
        other.config["read_files"] = json!(4);
        let err = compared(other, "greedy").unwrap_err().to_string();
        assert!(err.contains("different config"), "{err}");
        let err = compared(r_run("r", "random"), "random")
            .unwrap_err()
            .to_string();
        assert!(err.contains("no pair under that label"), "{err}");
    }

    #[test]
    fn a_family_id_shared_by_two_repositories_is_two_clusters() {
        let obs: Vec<Obs> = vec![
            (("lci", "f"), 0.0, 1.0),
            (("vite", "f"), 0.0, 1.0),
            (("vite", "f"), 0.0, 1.0),
        ];
        let clusters = clusters_of(&obs);
        assert_eq!(clusters.len(), 2);
        let mut sizes: Vec<usize> = clusters.iter().map(|c| c.n).collect();
        sizes.sort_unstable();
        assert_eq!(sizes, vec![1, 2]);
    }

    #[test]
    fn the_percentile_interval_takes_the_documented_indices() {
        // 1..=1000 in reverse order: sorting is part of the contract.
        let values: Vec<f64> = (1..=1000).rev().map(f64::from).collect();
        let n = values.len();
        let (low, high) = percentile_interval(values);
        assert_eq!(n * LOWER_PERMILLE / 1000, 25);
        assert_eq!((n * UPPER_PERMILLE).div_ceil(1000) - 1, 974);
        // The value at 0-based index i is i + 1.
        assert_eq!((low, high), (26.0, 975.0));
        // 10,000 resamples: indices 250 and 9749.
        let values: Vec<f64> = (0..10_000).map(f64::from).collect();
        assert_eq!(percentile_interval(values), (250.0, 9749.0));
        // A single value is both bounds.
        assert_eq!(percentile_interval(vec![3.0]), (3.0, 3.0));
    }
}
