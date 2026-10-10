use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::filter::{EffectiveFilterReport, FilterRequest};

use super::metrics::query_metrics;
/// A portable evaluation file: the workspaces it references plus the
/// queries to run against them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvaluationFile {
    /// Workspaces referenced by key.
    #[serde(default)]
    pub workspaces: Vec<EvalWorkspace>,
    /// Queries to evaluate.
    #[serde(default)]
    pub queries: Vec<EvalQuery>,
}

/// A workspace referenced by an evaluation file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalWorkspace {
    /// Stable key used by [`EvalQuery::workspace`].
    pub key: String,
    /// Required workspaces must be present; when an optional workspace is
    /// missing, its queries are skipped rather than failing the run.
    /// Defaults to `true` when omitted in a definition file.
    #[serde(default = "default_required")]
    pub required: bool,
}

const fn default_required() -> bool {
    true
}

/// A single evaluation query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvalQuery {
    /// Query text sent to search.
    pub query: String,
    /// Key of the workspace this query runs against.
    pub workspace: String,
    /// Result window size; `None` uses the service default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
    /// Optional retrieval filters, shared with the service filter request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<FilterRequest>,
    /// Repository-relative path patterns that count as a hit. When several
    /// patterns match the same result, the result still counts as one hit.
    #[serde(default)]
    pub expected_paths: Vec<String>,
    /// Path patterns that should appear in the results; each matching
    /// result counts once toward [`QueryMetrics::preferred_count`]
    /// regardless of how many patterns match it.
    #[serde(default)]
    pub preferred_paths: Vec<String>,
    /// Path patterns that should not appear before the first expected hit.
    #[serde(default)]
    pub disfavored_paths: Vec<String>,
    /// Source-role names (lowercase wire names) expected on the first
    /// expected hit. Empty means no role expectation.
    #[serde(default)]
    pub expected_roles: Vec<String>,
    /// A snippet that must appear in the first expected hit's bounded
    /// snippet; `None` means no snippet expectation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_snippet: Option<String>,
}

// ---------------------------------------------------------------------------
// Bounded result evidence
// ---------------------------------------------------------------------------

/// Maximum characters retained in a bounded evidence snippet.
pub const SNIPPET_BUDGET: usize = 400;

/// Bounded evidence for one returned result: just enough to evaluate a
/// query without persisting full chunk text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResultEvidence {
    /// One-based rank in the returned result list.
    pub rank: usize,
    /// Repository-relative file path.
    pub relative_file_path: String,
    /// Language identifier.
    pub language: String,
    /// Source-role name (lowercase wire name).
    pub source_role: String,
    /// One-based inclusive start line of the chunk.
    pub start_line: u32,
    /// One-based inclusive end line of the chunk.
    pub end_line: u32,
    /// Bounded snippet of the chunk code (see [`bounded_snippet`]).
    pub snippet: String,
    /// Reranker score when the report was reranked.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reranker_score: Option<f32>,
}

/// Truncate code to the bounded snippet budget, marking truncation with a
/// trailing ellipsis. Short input is returned unchanged.
pub fn bounded_snippet(code: &str) -> String {
    if code.len() <= SNIPPET_BUDGET {
        return code.to_owned();
    }
    let mut end = SNIPPET_BUDGET;
    while !code.is_char_boundary(end) {
        end -= 1;
    }
    let mut out: String = code[..end].to_owned();
    out.push('…');
    out
}

// ---------------------------------------------------------------------------
// Reported search state
// ---------------------------------------------------------------------------

/// Reranker state observed for a query.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RerankerState {
    /// No results were returned, so no scores could be inspected.
    NoResults,
    /// The report was not reranked.
    NotReranked,
    /// The report was reranked and every evidence row carries a score.
    Reranked,
    /// The report was reranked but some evidence rows lack scores.
    PartiallyScored,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct EvalTimings {
    pub query_embedding_ms: f64,
    pub lancedb_search_ms: f64,
    pub lexical_search_ms: f64,
    pub lsp_search_ms: f64,
    pub fusion_ms: f64,
    pub reranking_ms: f64,
    pub total_ms: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexLifecycleAction {
    Reused,
    Created,
    WaitedForExistingJob,
    RefreshedIncrementally,
}

// ---------------------------------------------------------------------------
// Per-query metrics and report
// ---------------------------------------------------------------------------

/// Pure metrics computed for one query from bounded evidence.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryMetrics {
    /// Whether any expected path matched any result. Duplicate pattern
    /// matches of the same result count as one hit.
    pub hit: bool,
    /// Whether the query succeeded: it has an expected-path hit and any
    /// expected-role and required-snippet expectations are satisfied.
    pub success: bool,
    /// Human-readable outcome reason for the query.
    pub reason: String,
    /// hit@1: the first result matches an expected path.
    pub hit_at_1: bool,
    /// hit@3: an expected path appears within the first three results, or
    /// within the whole window when it is smaller.
    pub hit_at_3: bool,
    /// hit@8-or-window: an expected path appears within the first eight
    /// results, or within the whole window when it is smaller.
    pub hit_at_8_or_window: bool,
    /// Effective window used for the windowed metrics: `min(8, results)`.
    pub window: usize,
    /// One-based rank of the first expected match, if any.
    pub first_expected_rank: Option<usize>,
    /// `1.0 / first_expected_rank`, or `0.0` when there is no hit.
    pub reciprocal_rank: f64,
    /// Number of results matching a preferred path pattern; each result
    /// counts once regardless of how many patterns match it.
    pub preferred_count: usize,
    /// Number of disfavored matches ranked before the first expected hit.
    /// When there is no expected hit, the whole evidence is counted.
    pub disfavored_before_first_expected: usize,
    /// Source-role distribution over the evidence, role name to count.
    #[serde(default)]
    pub role_distribution: BTreeMap<String, usize>,
    /// Reranker state for this query.
    pub reranker: RerankerState,
    /// Whether the first expected hit's role is among the expected roles.
    /// `None` when there is no expected hit; empty expectations are
    /// satisfied vacuously.
    pub expected_roles_satisfied: Option<bool>,
    /// Whether any returned chunk contains the required snippet. A missing
    /// expectation is satisfied vacuously.
    pub required_snippet_found: Option<bool>,
    /// Graded nDCG over the first ten results: a result matching an
    /// expected pattern has gain 2, one matching only a preferred pattern
    /// has gain 1, anything else 0, with a `log2(rank + 1)` discount. The
    /// ideal ranking holds one gain-2 slot per distinct expected pattern
    /// followed by one gain-1 slot per distinct preferred pattern that no
    /// expected pattern already covers. Each gold pattern earns its gain at
    /// most once, at the first result that matches it (a repeated chunk of
    /// the same file earns nothing; when several unsatisfied patterns match,
    /// the most specific one is credited), so the value stays in `[0, 1]`.
    /// `0.0` when there is no gold or no gold result in the window.
    ///
    /// Bounded by the returned result window: the report holds at most
    /// `top_k` results (the checked-in definitions use 8), so with
    /// `top_k < 10` this is effectively nDCG@`top_k`. Set `top_k >= 10` to
    /// compare with published @10 numbers.
    #[serde(default)]
    pub ndcg_at_10: f64,
    /// Fraction of distinct expected patterns matched by at least one of the
    /// first three results; `0.0` when there are no expected patterns.
    #[serde(default)]
    pub recall_at_3: f64,
    /// Fraction of distinct expected patterns matched by at least one of the
    /// first eight results (fewer when the window, `top_k`, is smaller);
    /// `0.0` when there are no expected patterns.
    #[serde(default)]
    pub recall_at_8: f64,
    /// `1.0` when an expected pattern matches one of the first five
    /// distinct files (repeated chunks of a file collapse to that file's
    /// best rank), else `0.0`.
    #[serde(default)]
    pub file_acc_at_5: f64,
    /// `1.0` when an expected pattern matches one of the first ten
    /// distinct files, else `0.0`. Bounded by the returned result window
    /// like [`Self::ndcg_at_10`]: with `top_k = 8` it is effectively
    /// accuracy over the first eight results.
    #[serde(default)]
    pub file_acc_at_10: f64,
}

/// Per-query evaluation report: bounded evidence, reported search state,
/// and computed metrics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueryReport {
    /// The query that was evaluated.
    pub query: EvalQuery,
    /// Whether the query was skipped because its optional workspace was
    /// missing. Skipped reports carry no evidence and are excluded from
    /// aggregate denominators.
    pub skipped: bool,
    /// Explicit per-query outcome: `success`, `failure`, or `skipped`.
    pub outcome: String,
    /// Explicit per-query reason: why the query succeeded, failed, or was
    /// skipped.
    pub reason: String,
    /// Bounded evidence for the returned results.
    #[serde(default)]
    pub evidence: Vec<ResultEvidence>,
    /// Whether the search report indicated reranking was applied.
    pub reranked: bool,
    /// Warning carried by the search report, if any (e.g. a degraded
    /// retrieval channel).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warning: Option<String>,
    /// Effective filters as reported by the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<EffectiveFilterReport>,
    /// Per-stage timings as reported by the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timings: Option<EvalTimings>,
    /// Index lifecycle action as reported by the service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_action: Option<IndexLifecycleAction>,
    /// Index lifecycle wait in milliseconds, as reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub index_wait_ms: Option<f64>,
    /// Computed metrics.
    pub metrics: QueryMetrics,
}

impl QueryReport {
    /// Build a report from bounded evidence and reported search state.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        query: EvalQuery,
        evidence: Vec<ResultEvidence>,
        reranked: bool,
        warning: Option<String>,
        filters: Option<EffectiveFilterReport>,
        timings: Option<EvalTimings>,
        index_action: Option<IndexLifecycleAction>,
        index_wait_ms: Option<f64>,
    ) -> Self {
        let metrics = query_metrics(&query, &evidence, reranked);
        let (outcome, reason) = if metrics.success {
            ("success".to_owned(), metrics.reason.clone())
        } else {
            ("failure".to_owned(), metrics.reason.clone())
        };
        Self {
            query,
            skipped: false,
            outcome,
            reason,
            evidence,
            reranked,
            warning,
            filters,
            timings,
            index_action,
            index_wait_ms,
            metrics,
        }
    }

    /// A report for a query skipped because its optional workspace was
    /// missing. Skipped reports carry no evidence and zero metrics, and
    /// are excluded from aggregate denominators.
    pub fn skipped(query: EvalQuery, reason: String) -> Self {
        Self {
            query,
            skipped: true,
            outcome: "skipped".to_owned(),
            reason,
            evidence: Vec::new(),
            reranked: false,
            warning: None,
            filters: None,
            timings: None,
            index_action: None,
            index_wait_ms: None,
            metrics: QueryMetrics {
                hit: false,
                success: false,
                reason: "skipped".to_owned(),
                hit_at_1: false,
                hit_at_3: false,
                hit_at_8_or_window: false,
                window: 0,
                first_expected_rank: None,
                reciprocal_rank: 0.0,
                preferred_count: 0,
                disfavored_before_first_expected: 0,
                role_distribution: BTreeMap::new(),
                reranker: RerankerState::NoResults,
                expected_roles_satisfied: None,
                required_snippet_found: None,
                ndcg_at_10: 0.0,
                recall_at_3: 0.0,
                recall_at_8: 0.0,
                file_acc_at_5: 0.0,
                file_acc_at_10: 0.0,
            },
        }
    }

    pub fn failed(query: EvalQuery, reason: String) -> Self {
        let mut report = Self::skipped(query, reason.clone());
        report.skipped = false;
        report.outcome = "failure".to_owned();
        report.reason = reason.clone();
        report.metrics.reason = reason;
        report
    }
}

// ---------------------------------------------------------------------------
// Aggregate report
// ---------------------------------------------------------------------------

/// Median and nearest-rank 95th percentile of one latency series, in
/// milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LatencyPercentiles {
    /// Median (mean of the two middle values for even sample counts).
    pub p50: f64,
    /// Nearest-rank 95th percentile.
    pub p95: f64,
}

/// [`LatencyPercentiles`] for every [`EvalTimings`] field; each key mirrors
/// the timing field it summarizes.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct StageLatency {
    pub query_embedding_ms: LatencyPercentiles,
    pub lancedb_search_ms: LatencyPercentiles,
    pub lexical_search_ms: LatencyPercentiles,
    pub lsp_search_ms: LatencyPercentiles,
    pub fusion_ms: LatencyPercentiles,
    pub reranking_ms: LatencyPercentiles,
    pub total_ms: LatencyPercentiles,
}

/// Aggregate metrics across all query reports.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AggregateReport {
    /// Total queries declared.
    pub query_count: usize,
    /// Queries actually executed (`query_count - skipped_count`).
    pub executed_count: usize,
    /// Queries skipped because their optional workspace was missing.
    pub skipped_count: usize,
    /// Executed queries that succeeded.
    pub success_count: usize,
    /// Executed queries that failed.
    pub failure_count: usize,
    /// Evaluated queries with at least one expected hit.
    pub hit_queries: usize,
    /// Hit rate over evaluated queries; `None` when none were evaluated.
    pub hit_rate: Option<f64>,
    /// hit@1 rate over evaluated queries.
    pub hit_at_1_rate: Option<f64>,
    /// hit@3 rate over evaluated queries.
    pub hit_at_3_rate: Option<f64>,
    /// hit@8-or-window rate over evaluated queries.
    pub hit_at_8_or_window_rate: Option<f64>,
    /// Mean reciprocal rank over evaluated queries; `None` when none were
    /// evaluated.
    pub mrr: Option<f64>,
    /// Mean [`QueryMetrics::ndcg_at_10`] over evaluated queries (bounded by
    /// each query's result window, see there); `None` when none were
    /// evaluated.
    #[serde(default)]
    pub mean_ndcg_at_10: Option<f64>,
    /// Mean [`QueryMetrics::recall_at_3`] over evaluated queries.
    #[serde(default)]
    pub mean_recall_at_3: Option<f64>,
    /// Mean [`QueryMetrics::recall_at_8`] over evaluated queries.
    #[serde(default)]
    pub mean_recall_at_8: Option<f64>,
    /// Mean [`QueryMetrics::file_acc_at_5`] over evaluated queries.
    #[serde(default)]
    pub mean_file_acc_at_5: Option<f64>,
    /// Mean [`QueryMetrics::file_acc_at_10`] over evaluated queries.
    #[serde(default)]
    pub mean_file_acc_at_10: Option<f64>,
    /// Median total latency in milliseconds over evaluated queries with
    /// timings; `None` when there are no samples.
    pub median_latency_ms: Option<f64>,
    /// 95th percentile (nearest-rank) total latency in milliseconds;
    /// `None` when there are no samples.
    pub p95_latency_ms: Option<f64>,
    /// Per-stage p50/p95 latency over evaluated queries with timings, using
    /// the same median and nearest-rank percentile as the total-latency
    /// fields; `None` when there are no samples.
    #[serde(default)]
    pub stage_latency: Option<StageLatency>,
    /// Evaluated queries whose report carried a warning (degraded
    /// channel), i.e. fell back.
    pub fallback_queries: usize,
    /// Fallback rate over evaluated queries; `None` when none were
    /// evaluated.
    pub fallback_rate: Option<f64>,
    /// Evaluated queries whose index was created.
    pub index_created: usize,
    /// Evaluated queries whose index was reused.
    pub index_reused: usize,
    /// Evaluated queries that waited for an existing index job.
    pub index_waited: usize,
    /// Evaluated queries whose stale index was refreshed incrementally.
    #[serde(default)]
    pub index_refreshed: usize,
    /// Combined source-role distribution over all evaluated evidence.
    #[serde(default)]
    pub role_distribution: BTreeMap<String, usize>,
    /// The per-query reports.
    #[serde(default)]
    pub queries: Vec<QueryReport>,
}
