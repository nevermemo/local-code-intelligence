use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;

use crate::filter::GlobPattern;

use super::types::{
    AggregateReport, EvalQuery, EvalTimings, IndexLifecycleAction, LatencyPercentiles,
    QueryMetrics, QueryReport, RerankerState, ResultEvidence, StageLatency, TagAggregate,
};
// ---------------------------------------------------------------------------
// Pure metrics
// ---------------------------------------------------------------------------

/// Validate a repository-relative path pattern.
///
/// Patterns must be non-empty, use `/` separators, and stay inside the
/// repository: no backslashes, no absolute or drive/UNC paths, no `.` or
/// `..` components, and no leading `/`. Glob metacharacters `*`, `?`, and
/// `[...]` are allowed.
pub fn validate_pattern(pattern: &str) -> Result<()> {
    GlobPattern::compile(pattern)
        .map(|_| ())
        .map_err(Into::into)
}

/// Whether a repository-relative glob pattern matches a result path.
///
/// The pattern is split on `/` into components and matched against the
/// path components:
/// - `*` matches any run of characters within a single component
///   (it never crosses `/`),
/// - `?` matches exactly one character within a component,
/// - `[...]` matches one character from the set (with `!` negation and
///   `-` ranges),
/// - a component of `**` matches any run of whole components (zero or
///   more),
/// - otherwise the component counts must line up exactly.
///
/// Empty patterns match nothing.
pub fn pattern_matches(pattern: &str, path: &str) -> bool {
    GlobPattern::compile(pattern).is_ok_and(|compiled| compiled.matches(path))
}

/// Compute per-query metrics from bounded evidence.
///
/// Ranks are one-based. When several expected patterns match the same
/// result, the result counts as one hit.
pub fn query_metrics(
    query: &EvalQuery,
    evidence: &[ResultEvidence],
    reranked: bool,
) -> QueryMetrics {
    let first_expected_rank = evidence
        .iter()
        .position(|row| {
            query
                .expected_paths
                .iter()
                .any(|pattern| pattern_matches(pattern, &row.relative_file_path))
        })
        .map(|index| index + 1);

    let gold = GoldPatterns::new(query);

    let hit = first_expected_rank.is_some();
    let hit_at_1 = first_expected_rank == Some(1);
    let hit_at_3 = first_expected_rank.is_some_and(|rank| rank <= 3);
    let hit_at_8_or_window = first_expected_rank.is_some_and(|rank| rank <= 8);

    let preferred_count = evidence
        .iter()
        .filter(|row| {
            query
                .preferred_paths
                .iter()
                .any(|pattern| pattern_matches(pattern, &row.relative_file_path))
        })
        .count();

    let disfavored_match = |row: &ResultEvidence| {
        query
            .disfavored_paths
            .iter()
            .any(|pattern| pattern_matches(pattern, &row.relative_file_path))
    };
    let disfavored_before_first_expected = match first_expected_rank {
        Some(rank) => evidence
            .iter()
            .take(rank - 1)
            .filter(|row| disfavored_match(row))
            .count(),
        None => evidence.iter().filter(|row| disfavored_match(row)).count(),
    };

    let mut role_distribution = BTreeMap::new();
    for row in evidence {
        *role_distribution
            .entry(row.source_role.clone())
            .or_insert(0) += 1;
    }

    let expected_roles_satisfied = match first_expected_rank {
        Some(rank) => {
            let row = &evidence[rank - 1];
            let roles_ok = if query.expected_roles.is_empty() {
                true
            } else {
                query
                    .expected_roles
                    .iter()
                    .any(|role| role == &row.source_role)
            };
            Some(roles_ok)
        }
        None => None,
    };
    let required_snippet_found = query.required_snippet.as_ref().map(|snippet| {
        evidence
            .iter()
            .any(|row| row.snippet.contains(snippet.as_str()))
    });

    let (success, reason) = match first_expected_rank {
        Some(rank) => {
            let roles_ok = expected_roles_satisfied.unwrap_or(true);
            let snippet_ok = required_snippet_found.unwrap_or(true);
            if roles_ok && snippet_ok {
                (
                    true,
                    format!("expected hit at rank {rank}; all expectations satisfied"),
                )
            } else {
                let mut failures = Vec::new();
                if !roles_ok {
                    failures.push("expected role not present on the first expected hit");
                }
                if !snippet_ok {
                    failures.push("required snippet not found in returned chunks");
                }
                (
                    false,
                    format!("expected hit at rank {rank}, but {}", failures.join("; ")),
                )
            }
        }
        None => (
            false,
            if evidence.is_empty() {
                "no results returned; no expected-path hit".to_owned()
            } else {
                format!("no expected-path hit among {} results", evidence.len())
            },
        ),
    };

    QueryMetrics {
        hit,
        success,
        reason,
        hit_at_1,
        hit_at_3,
        hit_at_8_or_window,
        window: evidence.len().min(8),
        first_expected_rank,
        reciprocal_rank: first_expected_rank.map_or(0.0, |rank| 1.0 / rank as f64),
        preferred_count,
        disfavored_before_first_expected,
        role_distribution,
        reranker: reranker_state(reranked, evidence),
        expected_roles_satisfied,
        required_snippet_found,
        ndcg_at_10: ndcg_at_10(&gold, evidence),
        recall_at_3: recall_at_k(&gold, evidence, 3),
        recall_at_8: recall_at_k(&gold, evidence, 8),
        file_acc_at_5: file_acc_at_k(&gold, evidence, 5),
        file_acc_at_10: file_acc_at_k(&gold, evidence, 10),
    }
}

/// Number of leading results that count toward nDCG.
const NDCG_CUTOFF: usize = 10;
/// Gain of a result that matches an expected path pattern.
const EXPECTED_GAIN: f64 = 2.0;
/// Gain of a result that matches only a preferred path pattern.
const PREFERRED_GAIN: f64 = 1.0;

/// Specificity of a path pattern; smaller sorts first. Literal paths (no
/// wildcard component) come first, then patterns with fewer wildcard
/// components, then fewer `**` components, then more literal characters.
fn specificity(pattern: &str) -> (usize, usize, Reverse<usize>) {
    let mut wildcard_components = 0;
    let mut recursive_components = 0;
    for component in pattern.split('/') {
        if component == "**" {
            wildcard_components += 1;
            recursive_components += 1;
        } else if component.contains(['*', '?', '[']) {
            wildcard_components += 1;
        }
    }
    let literal_chars = pattern
        .chars()
        .filter(|ch| !matches!(ch, '*' | '?' | '[' | ']'))
        .count();
    (
        wildcard_components,
        recursive_components,
        Reverse(literal_chars),
    )
}

/// Distinct patterns, most specific first. The order depends only on the
/// pattern set, never on how the definition happened to list them.
fn distinct_by_specificity(patterns: &[String]) -> Vec<&str> {
    let mut distinct: Vec<&str> = patterns
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    distinct.sort_by(|a, b| specificity(a).cmp(&specificity(b)).then_with(|| a.cmp(b)));
    distinct
}

/// The distinct gold patterns of one query, prepared once and shared by the
/// rank-aware metrics.
struct GoldPatterns<'a> {
    /// Distinct expected patterns, most specific first.
    expected: Vec<&'a str>,
    /// Distinct preferred patterns, most specific first, without any pattern
    /// that an expected pattern already covers: one that equals an expected
    /// pattern or that an expected pattern matches as a path (preferred
    /// `src/slang/mod.rs` under expected `src/slang/**`). Such a result is
    /// always an expected hit, so its preferred slot could never be earned.
    preferred: Vec<&'a str>,
}

impl<'a> GoldPatterns<'a> {
    fn new(query: &'a EvalQuery) -> Self {
        let expected = distinct_by_specificity(&query.expected_paths);
        let preferred = distinct_by_specificity(&query.preferred_paths)
            .into_iter()
            .filter(|preferred| {
                !expected
                    .iter()
                    .any(|expected| expected == preferred || pattern_matches(expected, preferred))
            })
            .collect();
        Self {
            expected,
            preferred,
        }
    }
}

/// Discounted gain of `gain` at a one-based `rank`.
fn discounted(gain: f64, rank: usize) -> f64 {
    gain / (rank as f64 + 1.0).log2()
}

/// Outcome of matching one result path against a set of gold patterns.
enum Claim {
    /// No pattern matches the path.
    Unmatched,
    /// The path matched a pattern that no earlier result had satisfied.
    Claimed,
    /// Every pattern that matches was already satisfied by an earlier result.
    Duplicate,
}

/// Match `path` against `patterns` (most specific first), marking the first
/// still-unclaimed matching pattern, hence the most specific one, as
/// satisfied.
fn claim_pattern(patterns: &[&str], claimed: &mut [bool], path: &str) -> Claim {
    let mut matched = false;
    for (index, pattern) in patterns.iter().enumerate() {
        if pattern_matches(pattern, path) {
            matched = true;
            if !claimed[index] {
                claimed[index] = true;
                return Claim::Claimed;
            }
        }
    }
    if matched {
        Claim::Duplicate
    } else {
        Claim::Unmatched
    }
}

/// Graded nDCG@10 (see [`QueryMetrics::ndcg_at_10`]).
///
/// Each distinct expected pattern is one ideal gain-2 slot and each distinct
/// preferred pattern not covered by an expected pattern (see
/// [`GoldPatterns`]) one ideal gain-1 slot. A result earns the gain of the
/// most specific unsatisfied gold pattern it matches; a result that only
/// re-matches already satisfied patterns, such as a second chunk of the same
/// file, earns nothing, which keeps the value at or below 1.
fn ndcg_at_10(gold: &GoldPatterns<'_>, evidence: &[ResultEvidence]) -> f64 {
    let GoldPatterns {
        expected,
        preferred,
    } = gold;

    // Expected slots come first, so the ideal gains are already descending.
    let ideal: f64 = std::iter::repeat_n(EXPECTED_GAIN, expected.len())
        .chain(std::iter::repeat_n(PREFERRED_GAIN, preferred.len()))
        .take(NDCG_CUTOFF)
        .enumerate()
        .map(|(index, gain)| discounted(gain, index + 1))
        .sum();
    if ideal <= 0.0 {
        return 0.0;
    }

    let mut expected_claimed = vec![false; expected.len()];
    let mut preferred_claimed = vec![false; preferred.len()];
    let dcg: f64 = evidence
        .iter()
        .take(NDCG_CUTOFF)
        .enumerate()
        .map(|(index, row)| {
            let path = row.relative_file_path.as_str();
            let gain = match claim_pattern(expected, &mut expected_claimed, path) {
                Claim::Claimed => EXPECTED_GAIN,
                Claim::Duplicate => 0.0,
                Claim::Unmatched => match claim_pattern(preferred, &mut preferred_claimed, path) {
                    Claim::Claimed => PREFERRED_GAIN,
                    Claim::Unmatched | Claim::Duplicate => 0.0,
                },
            };
            discounted(gain, index + 1)
        })
        .sum();
    dcg / ideal
}

/// Fraction of distinct expected patterns matched by at least one of the
/// first `k` results; `0.0` when there are no expected patterns.
fn recall_at_k(gold: &GoldPatterns<'_>, evidence: &[ResultEvidence], k: usize) -> f64 {
    let expected = &gold.expected;
    if expected.is_empty() {
        return 0.0;
    }
    let found = expected
        .iter()
        .filter(|pattern| {
            evidence
                .iter()
                .take(k)
                .any(|row| pattern_matches(pattern, &row.relative_file_path))
        })
        .count();
    found as f64 / expected.len() as f64
}

/// `1.0` when an expected pattern matches one of the first `k` distinct
/// files, else `0.0`. A file's rank is the rank of its best-ranked chunk, so
/// repeated chunks of one file occupy a single position.
fn file_acc_at_k(gold: &GoldPatterns<'_>, evidence: &[ResultEvidence], k: usize) -> f64 {
    let mut seen = BTreeSet::new();
    let hit = evidence
        .iter()
        .map(|row| row.relative_file_path.as_str())
        .filter(|path| seen.insert(*path))
        .take(k)
        .any(|path| {
            gold.expected
                .iter()
                .any(|pattern| pattern_matches(pattern, path))
        });
    f64::from(hit)
}

/// Derive the reranker state from the reranked flag and evidence scores.
fn reranker_state(reranked: bool, evidence: &[ResultEvidence]) -> RerankerState {
    if evidence.is_empty() {
        return RerankerState::NoResults;
    }
    if !reranked {
        return RerankerState::NotReranked;
    }
    if evidence.iter().all(|row| row.reranker_score.is_some()) {
        RerankerState::Reranked
    } else {
        RerankerState::PartiallyScored
    }
}

/// Aggregate per-query reports into aggregate metrics.
///
/// Skipped queries are counted but excluded from every rate, the MRR, and
/// the latency statistics.
pub fn aggregate(reports: &[QueryReport]) -> AggregateReport {
    let query_count = reports.len();
    let skipped_count = reports.iter().filter(|report| report.skipped).count();
    let executed: Vec<&QueryReport> = reports.iter().filter(|report| !report.skipped).collect();
    let executed_count = executed.len();

    let success_count = executed
        .iter()
        .filter(|report| report.metrics.success)
        .count();
    let failure_count = executed_count - success_count;
    let hit_queries = executed.iter().filter(|report| report.metrics.hit).count();
    let hit_at_1 = executed
        .iter()
        .filter(|report| report.metrics.hit_at_1)
        .count();
    let hit_at_3 = executed
        .iter()
        .filter(|report| report.metrics.hit_at_3)
        .count();
    let hit_at_8_or_window = executed
        .iter()
        .filter(|report| report.metrics.hit_at_8_or_window)
        .count();
    let fallback_queries = executed
        .iter()
        .filter(|report| report.warning.is_some())
        .count();

    let mrr = mean_metric(&executed, |metrics| metrics.reciprocal_rank);

    let timings: Vec<EvalTimings> = executed
        .iter()
        .filter_map(|report| report.timings)
        .collect();
    // The total-latency fields are the `total_ms` stage, computed once.
    let stages = stage_latency(&timings);
    let median_latency_ms = stages.map(|stages| stages.total_ms.p50);
    let p95_latency_ms = stages.map(|stages| stages.total_ms.p95);

    let mut role_distribution = BTreeMap::new();
    for report in &executed {
        for (role, count) in &report.metrics.role_distribution {
            *role_distribution.entry(role.clone()).or_insert(0) += count;
        }
    }

    AggregateReport {
        query_count,
        executed_count,
        skipped_count,
        success_count,
        failure_count,
        hit_queries,
        hit_rate: rate(hit_queries, executed_count),
        hit_at_1_rate: rate(hit_at_1, executed_count),
        hit_at_3_rate: rate(hit_at_3, executed_count),
        hit_at_8_or_window_rate: rate(hit_at_8_or_window, executed_count),
        mrr,
        mean_ndcg_at_10: mean_metric(&executed, |metrics| metrics.ndcg_at_10),
        mean_recall_at_3: mean_metric(&executed, |metrics| metrics.recall_at_3),
        mean_recall_at_8: mean_metric(&executed, |metrics| metrics.recall_at_8),
        mean_file_acc_at_5: mean_metric(&executed, |metrics| metrics.file_acc_at_5),
        mean_file_acc_at_10: mean_metric(&executed, |metrics| metrics.file_acc_at_10),
        median_latency_ms,
        p95_latency_ms,
        stage_latency: stages,
        fallback_queries,
        fallback_rate: rate(fallback_queries, executed_count),
        index_created: executed
            .iter()
            .filter(|report| matches!(report.index_action, Some(IndexLifecycleAction::Created)))
            .count(),
        index_reused: executed
            .iter()
            .filter(|report| matches!(report.index_action, Some(IndexLifecycleAction::Reused)))
            .count(),
        index_waited: executed
            .iter()
            .filter(|report| {
                matches!(
                    report.index_action,
                    Some(IndexLifecycleAction::WaitedForExistingJob)
                )
            })
            .count(),
        index_refreshed: executed
            .iter()
            .filter(|report| {
                matches!(
                    report.index_action,
                    Some(IndexLifecycleAction::RefreshedIncrementally)
                )
            })
            .count(),
        role_distribution,
        by_tag: tag_aggregates(&executed),
        queries: reports.to_vec(),
    }
}

/// Group the executed reports by query tag and summarize each group. A
/// query repeating a tag counts once for it, and a query with several tags
/// counts once in each group.
fn tag_aggregates(executed: &[&QueryReport]) -> BTreeMap<String, TagAggregate> {
    let mut groups: BTreeMap<&str, Vec<&QueryReport>> = BTreeMap::new();
    for report in executed {
        let distinct: BTreeSet<&str> = report.query.tags.iter().map(String::as_str).collect();
        for tag in distinct {
            groups.entry(tag).or_default().push(report);
        }
    }
    groups
        .into_iter()
        .map(|(tag, reports)| {
            let count = reports.len() as f64;
            let share = |hit: fn(&QueryMetrics) -> bool| {
                reports.iter().filter(|report| hit(&report.metrics)).count() as f64 / count
            };
            let mean = |value: fn(&QueryMetrics) -> f64| {
                reports
                    .iter()
                    .map(|report| value(&report.metrics))
                    .sum::<f64>()
                    / count
            };
            let aggregate = TagAggregate {
                executed_count: reports.len(),
                hit_at_1_rate: share(|metrics| metrics.hit_at_1),
                hit_at_3_rate: share(|metrics| metrics.hit_at_3),
                mrr: mean(|metrics| metrics.reciprocal_rank),
                mean_ndcg_at_10: mean(|metrics| metrics.ndcg_at_10),
            };
            (tag.to_owned(), aggregate)
        })
        .collect()
}

/// Mean of a per-query metric over the executed reports; `None` when there
/// are none.
fn mean_metric(executed: &[&QueryReport], value: impl Fn(&QueryMetrics) -> f64) -> Option<f64> {
    (!executed.is_empty()).then(|| {
        executed
            .iter()
            .map(|report| value(&report.metrics))
            .sum::<f64>()
            / executed.len() as f64
    })
}

/// Median and nearest-rank p95 of a non-empty series.
fn latency_percentiles(values: impl Iterator<Item = f64>) -> LatencyPercentiles {
    let values: Vec<f64> = values.collect();
    LatencyPercentiles {
        p50: median(&values),
        p95: percentile(&values, 0.95),
    }
}

/// Per-stage latency percentiles over the given timings; `None` when empty.
fn stage_latency(timings: &[EvalTimings]) -> Option<StageLatency> {
    if timings.is_empty() {
        return None;
    }
    let stage = |select: fn(&EvalTimings) -> f64| latency_percentiles(timings.iter().map(select));
    Some(StageLatency {
        query_embedding_ms: stage(|timings| timings.query_embedding_ms),
        lancedb_search_ms: stage(|timings| timings.lancedb_search_ms),
        lexical_search_ms: stage(|timings| timings.lexical_search_ms),
        lsp_search_ms: stage(|timings| timings.lsp_search_ms),
        fusion_ms: stage(|timings| timings.fusion_ms),
        reranking_ms: stage(|timings| timings.reranking_ms),
        total_ms: stage(|timings| timings.total_ms),
    })
}

/// `numerator / denominator` as a rate in `[0, 1]`; `None` when the
/// denominator is zero.
fn rate(numerator: usize, denominator: usize) -> Option<f64> {
    (denominator > 0).then_some(numerator as f64 / denominator as f64)
}

/// Median of a non-empty slice: the middle value for odd lengths, the mean
/// of the two middle values for even lengths. The slice need not be sorted.
fn median(values: &[f64]) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    debug_assert!(n > 0, "median of an empty slice");
    if n % 2 == 1 {
        sorted[n / 2]
    } else {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0
    }
}

/// Nearest-rank percentile of a non-empty slice: the value at rank
/// `ceil(p * n)` (one-based) after sorting.
fn percentile(values: &[f64], p: f64) -> f64 {
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = sorted.len();
    debug_assert!(n > 0, "percentile of an empty slice");
    let rank = (p * n as f64).ceil() as usize;
    sorted[(rank - 1).min(n - 1)]
}
