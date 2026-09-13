use std::collections::BTreeMap;

use anyhow::Result;

use crate::filter::GlobPattern;

use super::types::{
    AggregateReport, EvalQuery, IndexLifecycleAction, QueryMetrics, QueryReport, RerankerState,
    ResultEvidence,
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
    }
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

    let mrr = if executed_count == 0 {
        None
    } else {
        Some(
            executed
                .iter()
                .map(|report| report.metrics.reciprocal_rank)
                .sum::<f64>()
                / executed_count as f64,
        )
    };

    let latencies: Vec<f64> = executed
        .iter()
        .filter_map(|report| report.timings.map(|timings| timings.total_ms))
        .collect();
    let median_latency_ms = if latencies.is_empty() {
        None
    } else {
        Some(median(&latencies))
    };
    let p95_latency_ms = if latencies.is_empty() {
        None
    } else {
        Some(percentile(&latencies, 0.95))
    };

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
        median_latency_ms,
        p95_latency_ms,
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
        role_distribution,
        queries: reports.to_vec(),
    }
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
