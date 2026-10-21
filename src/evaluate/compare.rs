//! Paired comparison of two evaluation reports.
//!
//! [`compare`] pairs the queries of a baseline and a candidate report by
//! `(workspace, query text)` and measures, per metric, how much the
//! candidate moved: the mean paired delta (`candidate - baseline`) with a
//! seeded paired-bootstrap confidence interval, plus win/loss/tie counts on
//! reciprocal rank. It is pure: it reads no files, needs no services, and
//! the same inputs and options always give the same report. Comparison
//! observes two finished runs; it never changes ranking.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::types::{AggregateReport, QueryReport, snippet_tokens};

/// Default bootstrap seed. Fixed so that two runs of `compare` over the same
/// reports print the same intervals.
pub const DEFAULT_SEED: u64 = 0x4C43_4933;

/// Default number of bootstrap resamples.
pub const DEFAULT_RESAMPLES: usize = 10_000;

/// Confidence level of the reported intervals.
const CONFIDENCE: f64 = 0.95;

/// Percentile interval bounds in permille (a 95% interval cuts 2.5% from
/// each tail). Integer arithmetic avoids float rounding at the edges.
const LOWER_PERMILLE: usize = 25;
const UPPER_PERMILLE: usize = 975;

/// Reciprocal-rank deltas within this distance of zero count as ties.
const TIE_EPSILON: f64 = 1e-12;

/// Bootstrap parameters for [`compare`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompareOptions {
    /// Seed of the resampling generator.
    pub seed: u64,
    /// Number of bootstrap resamples (at least one is always used).
    pub resamples: usize,
    /// Keep the pairs in which either side's language server missed
    /// `lsp_search_budget_ms` (`QueryMetrics::lsp_budget_hit`). Off by
    /// default: such a query measured a cold start, not retrieval, so
    /// [`compare`] leaves its pair out and counts it in `budget_hit_*`.
    /// `--keep-failed` on `compare` and `evaluate --baseline`, the flag
    /// name `maps compare` uses for the same choice.
    pub keep_failed: bool,
}

impl Default for CompareOptions {
    fn default() -> Self {
        Self {
            seed: DEFAULT_SEED,
            resamples: DEFAULT_RESAMPLES,
            keep_failed: false,
        }
    }
}

/// Identity of a query across two reports.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct QueryKey {
    /// Workspace key of the query.
    pub workspace: String,
    /// Query text.
    pub query: String,
}

/// The three compared metrics for one query. `hit_at_1` is `1.0` or `0.0`,
/// so its deltas are `-1`, `0`, or `1`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PairMetrics {
    /// Reciprocal rank of the first expected hit; `0.0` for a miss.
    pub reciprocal_rank: f64,
    /// Graded nDCG over the returned window (see `QueryMetrics::ndcg_at_10`).
    pub ndcg_at_10: f64,
    /// `1.0` when the first result matched an expected path.
    pub hit_at_1: f64,
    /// What the agent reads: the token proxy of the snippets of every
    /// returned row of both lists (`QueryMetrics::returned_tokens`). A report
    /// written before the field has it derived from its evidence, so a
    /// baseline of an older build compares on the same measure.
    #[serde(default)]
    pub returned_tokens: f64,
}

/// One query present and executed in both reports.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairedQuery {
    /// Workspace key.
    pub workspace: String,
    /// Query text.
    pub query: String,
    /// Union of the tags the two reports carry for this query, sorted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// Metrics from the baseline report.
    pub baseline: PairMetrics,
    /// Metrics from the candidate report.
    pub candidate: PairMetrics,
    /// `candidate - baseline`, per metric.
    pub delta: PairMetrics,
}

/// One metric compared over a set of pairs.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct MetricComparison {
    /// Mean of the metric over the baseline side of the pairs.
    pub baseline_mean: f64,
    /// Mean of the metric over the candidate side of the pairs.
    pub candidate_mean: f64,
    /// Mean paired delta, `candidate_mean - baseline_mean`.
    pub mean_delta: f64,
    /// Lower bound of the 95% paired-bootstrap percentile interval of the
    /// mean delta.
    pub ci_low: f64,
    /// Upper bound of the same interval.
    pub ci_high: f64,
    /// Whether the interval excludes zero.
    pub significant: bool,
}

/// All compared metrics over one set of pairs (everything, or one tag).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct GroupComparison {
    /// Number of pairs in the group; always at least one.
    pub pairs: usize,
    /// Pairs whose reciprocal rank improved.
    pub wins: usize,
    /// Pairs whose reciprocal rank dropped.
    pub losses: usize,
    /// Pairs whose reciprocal rank did not change.
    pub ties: usize,
    /// Mean reciprocal rank.
    pub mrr: MetricComparison,
    /// Mean nDCG@10.
    pub ndcg_at_10: MetricComparison,
    /// hit@1 rate.
    pub hit_at_1: MetricComparison,
    /// Returned-token proxy per query (see [`PairMetrics::returned_tokens`]);
    /// a lower candidate mean is cheaper to read.
    #[serde(default)]
    pub returned_tokens: MetricComparison,
}

/// Result of [`compare`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ComparisonReport {
    /// Seed the bootstrap used.
    pub seed: u64,
    /// Number of bootstrap resamples used.
    pub resamples: usize,
    /// Confidence level of the intervals.
    pub confidence: f64,
    /// Where the baseline report came from, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_source: Option<String>,
    /// Where the candidate report came from, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub candidate_source: Option<String>,
    /// Number of paired queries (`pairs.len()`).
    pub paired: usize,
    /// Matched queries left out because at least one side was skipped.
    pub skipped_pairs: usize,
    /// Of those, how many were skipped in the baseline report.
    pub skipped_baseline: usize,
    /// Of those, how many were skipped in the candidate report.
    pub skipped_candidate: usize,
    /// Matched, executed queries left out because at least one side had a
    /// language server past its search budget (see
    /// [`CompareOptions::keep_failed`]). Zero when they are kept.
    #[serde(default)]
    pub budget_hit_pairs: usize,
    /// Of those, how many had the hit in the baseline report.
    #[serde(default)]
    pub budget_hit_baseline: usize,
    /// Of those, how many had the hit in the candidate report.
    #[serde(default)]
    pub budget_hit_candidate: usize,
    /// Of the `budget_hit_pairs` left out, how many carried each tag (the tag
    /// union of both sides, as a kept pair takes it). Empty (and omitted from
    /// JSON) when nothing was left out or no left-out pair had a tag, and
    /// always empty with [`CompareOptions::keep_failed`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub budget_hit_by_tag: BTreeMap<String, usize>,
    /// Baseline queries with no counterpart in the candidate report.
    pub unpaired_baseline: usize,
    /// Candidate queries with no counterpart in the baseline report.
    pub unpaired_candidate: usize,
    /// The baseline queries counted in `unpaired_baseline`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpaired_baseline_queries: Vec<QueryKey>,
    /// The candidate queries counted in `unpaired_candidate`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unpaired_candidate_queries: Vec<QueryKey>,
    /// Comparison over all pairs; `None` when there are no pairs.
    pub overall: Option<GroupComparison>,
    /// Comparison per query tag. Empty (and omitted from JSON) when no pair
    /// carries a tag. A pair with several tags counts once in each.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_tag: BTreeMap<String, GroupComparison>,
    /// The per-query pairs, in baseline order.
    pub pairs: Vec<PairedQuery>,
}

/// Read an evaluation report written by `evaluate --output`.
pub fn load_report(path: &Path) -> Result<AggregateReport> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read evaluation report: {}", path.display()))?;
    // A report redirected from PowerShell may start with a byte-order mark.
    serde_json::from_str(raw.trim_start_matches('\u{feff}'))
        .with_context(|| format!("failed to parse evaluation report: {}", path.display()))
}

/// The evaluation report as JSON, with the comparison embedded under a
/// `comparison` key when there is one. The result still loads with
/// [`load_report`], so it can serve as the baseline of a later comparison.
pub fn report_with_comparison(
    report: &AggregateReport,
    comparison: Option<&ComparisonReport>,
) -> Result<serde_json::Value> {
    let mut value = serde_json::to_value(report)?;
    if let Some(comparison) = comparison {
        value["comparison"] = serde_json::to_value(comparison)?;
    }
    Ok(value)
}

/// Read two reports from disk and [`compare`] them, recording the paths as
/// the report sources.
pub fn compare_files(
    baseline: &Path,
    candidate: &Path,
    options: &CompareOptions,
) -> Result<ComparisonReport> {
    let baseline_report = load_report(baseline)?;
    let candidate_report = load_report(candidate)?;
    let mut comparison = compare(&baseline_report, &candidate_report, options);
    comparison.baseline_source = Some(baseline.display().to_string());
    comparison.candidate_source = Some(candidate.display().to_string());
    Ok(comparison)
}

/// Compare a candidate report against a baseline report.
///
/// Queries are paired by `(workspace, query text)`. When a key occurs
/// several times in one report (the same text with different filters, say),
/// its occurrences pair in order. A matched query that was skipped on either
/// side is excluded from the pairs and counted in `skipped_*`; so is one in
/// which either side had a language server past its search budget
/// (`lsp_budget_hit`), counted in `budget_hit_*`, unless
/// [`CompareOptions::keep_failed`]; a query with
/// no counterpart is listed under `unpaired_*`. Failed queries are not
/// skipped: they pair with zero metrics, like any miss.
pub fn compare(
    baseline: &AggregateReport,
    candidate: &AggregateReport,
    options: &CompareOptions,
) -> ComparisonReport {
    let resamples = options.resamples.max(1);

    let mut candidate_queue: BTreeMap<(&str, &str), VecDeque<&QueryReport>> = BTreeMap::new();
    for report in &candidate.queries {
        candidate_queue
            .entry((report.query.workspace.as_str(), report.query.query.as_str()))
            .or_default()
            .push_back(report);
    }

    let mut pairs = Vec::new();
    let mut skipped_pairs = 0;
    let mut skipped_baseline = 0;
    let mut skipped_candidate = 0;
    let mut budget_hit_pairs = 0;
    let mut budget_hit_baseline = 0;
    let mut budget_hit_candidate = 0;
    let mut budget_hit_by_tag: BTreeMap<String, usize> = BTreeMap::new();
    let mut unpaired_baseline_queries = Vec::new();
    for base in &baseline.queries {
        let key = (base.query.workspace.as_str(), base.query.query.as_str());
        let Some(cand) = candidate_queue.get_mut(&key).and_then(VecDeque::pop_front) else {
            unpaired_baseline_queries.push(query_key(base));
            continue;
        };
        if base.skipped || cand.skipped {
            skipped_pairs += 1;
            skipped_baseline += usize::from(base.skipped);
            skipped_candidate += usize::from(cand.skipped);
            continue;
        }
        if !options.keep_failed && (base.metrics.lsp_budget_hit || cand.metrics.lsp_budget_hit) {
            budget_hit_pairs += 1;
            budget_hit_baseline += usize::from(base.metrics.lsp_budget_hit);
            budget_hit_candidate += usize::from(cand.metrics.lsp_budget_hit);
            for tag in tags_of(base, cand) {
                *budget_hit_by_tag.entry(tag.clone()).or_default() += 1;
            }
            continue;
        }
        pairs.push(pair_up(base, cand));
    }
    let unpaired_candidate_queries: Vec<QueryKey> = candidate_queue
        .into_values()
        .flatten()
        .map(query_key)
        .collect();

    let overall = summarize(&pairs.iter().collect::<Vec<_>>(), options.seed, resamples);
    let mut tagged: BTreeMap<&str, Vec<&PairedQuery>> = BTreeMap::new();
    for pair in &pairs {
        for tag in &pair.tags {
            tagged.entry(tag).or_default().push(pair);
        }
    }
    let by_tag = tagged
        .into_iter()
        .filter_map(|(tag, group)| {
            summarize(&group, options.seed, resamples).map(|summary| (tag.to_owned(), summary))
        })
        .collect();

    ComparisonReport {
        seed: options.seed,
        resamples,
        confidence: CONFIDENCE,
        baseline_source: None,
        candidate_source: None,
        paired: pairs.len(),
        skipped_pairs,
        skipped_baseline,
        skipped_candidate,
        budget_hit_pairs,
        budget_hit_baseline,
        budget_hit_candidate,
        budget_hit_by_tag,
        unpaired_baseline: unpaired_baseline_queries.len(),
        unpaired_candidate: unpaired_candidate_queries.len(),
        unpaired_baseline_queries,
        unpaired_candidate_queries,
        overall,
        by_tag,
        pairs,
    }
}

fn query_key(report: &QueryReport) -> QueryKey {
    QueryKey {
        workspace: report.query.workspace.clone(),
        query: report.query.query.clone(),
    }
}

fn pair_metrics(report: &QueryReport) -> PairMetrics {
    PairMetrics {
        reciprocal_rank: report.metrics.reciprocal_rank,
        ndcg_at_10: report.metrics.ndcg_at_10,
        hit_at_1: f64::from(u8::from(report.metrics.hit_at_1)),
        returned_tokens: report
            .metrics
            .returned_tokens
            .unwrap_or_else(|| snippet_tokens(&[&report.evidence, &report.unscored_evidence]))
            as f64,
    }
}

/// The distinct tags of a matched pair: the union of both sides.
fn tags_of<'a>(base: &'a QueryReport, cand: &'a QueryReport) -> BTreeSet<&'a String> {
    base.query.tags.iter().chain(&cand.query.tags).collect()
}

fn pair_up(base: &QueryReport, cand: &QueryReport) -> PairedQuery {
    let baseline = pair_metrics(base);
    let candidate = pair_metrics(cand);
    let tags = tags_of(base, cand);
    PairedQuery {
        workspace: base.query.workspace.clone(),
        query: base.query.query.clone(),
        tags: tags.into_iter().cloned().collect(),
        baseline,
        candidate,
        delta: PairMetrics {
            reciprocal_rank: candidate.reciprocal_rank - baseline.reciprocal_rank,
            ndcg_at_10: candidate.ndcg_at_10 - baseline.ndcg_at_10,
            hit_at_1: candidate.hit_at_1 - baseline.hit_at_1,
            returned_tokens: candidate.returned_tokens - baseline.returned_tokens,
        },
    }
}

/// Summarize a set of pairs; `None` when the set is empty.
fn summarize(pairs: &[&PairedQuery], seed: u64, resamples: usize) -> Option<GroupComparison> {
    if pairs.is_empty() {
        return None;
    }
    let wins = pairs
        .iter()
        .filter(|pair| pair.delta.reciprocal_rank > TIE_EPSILON)
        .count();
    let losses = pairs
        .iter()
        .filter(|pair| pair.delta.reciprocal_rank < -TIE_EPSILON)
        .count();
    let metric = |select: fn(&PairMetrics) -> f64| {
        let baseline: Vec<f64> = pairs.iter().map(|pair| select(&pair.baseline)).collect();
        let candidate: Vec<f64> = pairs.iter().map(|pair| select(&pair.candidate)).collect();
        let deltas: Vec<f64> = pairs.iter().map(|pair| select(&pair.delta)).collect();
        metric_comparison(&baseline, &candidate, &deltas, seed, resamples)
    };
    Some(GroupComparison {
        pairs: pairs.len(),
        wins,
        losses,
        ties: pairs.len() - wins - losses,
        mrr: metric(|metrics| metrics.reciprocal_rank),
        ndcg_at_10: metric(|metrics| metrics.ndcg_at_10),
        hit_at_1: metric(|metrics| metrics.hit_at_1),
        returned_tokens: metric(|metrics| metrics.returned_tokens),
    })
}

fn metric_comparison(
    baseline: &[f64],
    candidate: &[f64],
    deltas: &[f64],
    seed: u64,
    resamples: usize,
) -> MetricComparison {
    let mean = |values: &[f64]| values.iter().sum::<f64>() / values.len() as f64;
    let (ci_low, ci_high) = paired_bootstrap_ci(deltas, resamples, seed).unwrap_or((0.0, 0.0));
    MetricComparison {
        baseline_mean: mean(baseline),
        candidate_mean: mean(candidate),
        mean_delta: mean(deltas),
        ci_low,
        ci_high,
        significant: ci_low > 0.0 || ci_high < 0.0,
    }
}

/// 95% percentile interval of the mean of `deltas` under the paired
/// bootstrap: `resamples` resamples of the indices with replacement, each
/// the size of the input, drawn from a SplitMix64 generator seeded with
/// `seed`. `None` for an empty input. The same inputs always give the same
/// interval; a constant input `d` gives `(d, d)`.
pub fn paired_bootstrap_ci(deltas: &[f64], resamples: usize, seed: u64) -> Option<(f64, f64)> {
    let n = deltas.len();
    if n == 0 {
        return None;
    }
    let resamples = resamples.max(1);
    let mut rng = SplitMix64::new(seed);
    let mut means: Vec<f64> = (0..resamples)
        .map(|_| {
            let sum: f64 = (0..n).map(|_| deltas[rng.below(n)]).sum();
            sum / n as f64
        })
        .collect();
    means.sort_by(f64::total_cmp);
    let low = resamples * LOWER_PERMILLE / 1000;
    let high = (resamples * UPPER_PERMILLE).div_ceil(1000) - 1;
    Some((
        means[low.min(resamples - 1)],
        means[high.min(resamples - 1)],
    ))
}

/// SplitMix64: a tiny deterministic generator, enough for resampling indices
/// reproducibly without a dependency.
struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// A value in `0..bound` (multiply-shift; the bias is below 2^-40 for
    /// any realistic query count).
    fn below(&mut self, bound: usize) -> usize {
        ((u128::from(self.next_u64()) * bound as u128) >> 64) as usize
    }
}

impl ComparisonReport {
    /// A short human-readable summary: per metric the baseline and candidate
    /// means, the delta with its interval, win/loss/tie counts, the unpaired
    /// and skipped counts, and the same per tag.
    pub fn render_summary(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Paired comparison (delta = candidate - baseline)");
        if let Some(source) = &self.baseline_source {
            let _ = writeln!(out, "  baseline:  {source}");
        }
        if let Some(source) = &self.candidate_source {
            let _ = writeln!(out, "  candidate: {source}");
        }
        let _ = writeln!(
            out,
            "  pairs: {} | unpaired: baseline {}, candidate {} | skipped on either side: {} (baseline {}, candidate {}) | LSP budget hit on either side, left out: {} (baseline {}, candidate {})",
            self.paired,
            self.unpaired_baseline,
            self.unpaired_candidate,
            self.skipped_pairs,
            self.skipped_baseline,
            self.skipped_candidate,
            self.budget_hit_pairs,
            self.budget_hit_baseline,
            self.budget_hit_candidate,
        );
        if !self.budget_hit_by_tag.is_empty() {
            let counts: Vec<String> = self
                .budget_hit_by_tag
                .iter()
                .map(|(tag, count)| format!("{tag} {count}"))
                .collect();
            let _ = writeln!(
                out,
                "  LSP budget hit, left out, by tag: {}",
                counts.join(", ")
            );
        }
        let _ = writeln!(
            out,
            "  {:.0}% paired-bootstrap CI, {} resamples, seed {}",
            self.confidence * 100.0,
            self.resamples,
            self.seed,
        );
        match &self.overall {
            Some(group) => render_group(&mut out, "  ", group),
            None => {
                let _ = writeln!(out, "  no comparable pairs, nothing to compare");
            }
        }
        for (tag, group) in &self.by_tag {
            let _ = writeln!(out, "\n  tag {tag}");
            render_group(&mut out, "    ", group);
        }
        out
    }
}

fn render_group(out: &mut String, indent: &str, group: &GroupComparison) {
    let _ = writeln!(
        out,
        "{indent}{:<9}{:>9}{:>11}{:>9}  95% CI",
        "", "baseline", "candidate", "delta",
    );
    for (name, metric) in [
        ("MRR", &group.mrr),
        ("nDCG@10", &group.ndcg_at_10),
        ("hit@1", &group.hit_at_1),
    ] {
        let _ = writeln!(
            out,
            "{indent}{name:<9}{:>9.4}{:>11.4}{:>9}  {:<20}{}",
            metric.baseline_mean,
            metric.candidate_mean,
            signed(metric.mean_delta),
            format!("[{}, {}]", signed(metric.ci_low), signed(metric.ci_high)),
            if metric.significant {
                "significant"
            } else {
                "not significant"
            },
        );
    }
    let tokens = &group.returned_tokens;
    let _ = writeln!(
        out,
        "{indent}{:<9}{:>9.1}{:>11.1}{:>9.1}  (returned tokens per query, snippet proxy; lower is cheaper)",
        "tokens", tokens.baseline_mean, tokens.candidate_mean, tokens.mean_delta,
    );
    let _ = writeln!(
        out,
        "{indent}reciprocal rank W/L/T: {}/{}/{} over {} pairs",
        group.wins, group.losses, group.ties, group.pairs,
    );
}

/// `value` with an explicit sign and four decimals; values that round to
/// zero print as `+0.0000` rather than `-0.0000`.
fn signed(value: f64) -> String {
    let value = if value.abs() < 0.000_05 { 0.0 } else { value };
    format!("{value:+.4}")
}
