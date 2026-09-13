//! Evaluation definitions, execution, and metrics for portable evaluation files.
//!
//! This module defines the Serde shapes for evaluation workspaces
//! (required/optional), queries, bounded result evidence, per-query
//! reports, and aggregate reports; loads and validates TOML evaluation
//! definitions; and executes queries through
//! [`App::search_with_filters`](crate::app::App::search_with_filters).
//! Ranking stays in the service; this module only records bounded
//! evidence and computes metrics from it.
//!
//! Filter requests and effective filter reports use the shared service
//! types ([`FilterRequest`], [`EffectiveFilterReport`]) directly. Result
//! ranks are one-based. Path patterns are repository-relative globs using
//! `/` separators; see [`pattern_matches`] and [`validate_pattern`] for
//! the matching and validation rules.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::app::{App, IndexAction, SearchReport};
use crate::filter::{EffectiveFilterReport, FilterRequest, GlobPattern};

// ---------------------------------------------------------------------------
// Portable evaluation file
// ---------------------------------------------------------------------------

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
    /// Median total latency in milliseconds over evaluated queries with
    /// timings; `None` when there are no samples.
    pub median_latency_ms: Option<f64>,
    /// 95th percentile (nearest-rank) total latency in milliseconds;
    /// `None` when there are no samples.
    pub p95_latency_ms: Option<f64>,
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
    /// Combined source-role distribution over all evaluated evidence.
    #[serde(default)]
    pub role_distribution: BTreeMap<String, usize>,
    /// The per-query reports.
    #[serde(default)]
    pub queries: Vec<QueryReport>,
}

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

// ---------------------------------------------------------------------------
// TOML definition loading and validation
// ---------------------------------------------------------------------------

/// Load and validate a TOML evaluation definition file.
///
/// Validates:
/// - workspace keys and paths are non-empty,
/// - workspace keys are unique,
/// - query workspace keys resolve to a declared workspace,
/// - all path patterns (expected, preferred, disfavored) are valid
///   repository-relative globs,
/// - filter path patterns are valid repository-relative globs.
pub fn load_definition(path: &Path) -> Result<EvaluationFile> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read evaluation definition: {}", path.display()))?;
    let file: EvaluationFile = toml::from_str(&raw)
        .with_context(|| format!("failed to parse TOML: {}", path.display()))?;
    validate_definition(&file)?;
    Ok(file)
}

/// Validate an evaluation definition for structural correctness.
pub fn validate_definition(file: &EvaluationFile) -> Result<()> {
    let mut seen_keys: BTreeSet<&str> = BTreeSet::new();
    for ws in &file.workspaces {
        if ws.key.trim().is_empty() {
            bail!("workspace key must not be empty");
        }
        if !seen_keys.insert(ws.key.as_str()) {
            bail!("duplicate workspace key: {:?}", ws.key);
        }
    }

    // Validate queries.
    let declared_keys: BTreeSet<&str> = file.workspaces.iter().map(|ws| ws.key.as_str()).collect();
    for (i, query) in file.queries.iter().enumerate() {
        if query.query.trim().is_empty() {
            bail!("query at index {i} has empty query text");
        }
        if query.workspace.trim().is_empty() {
            bail!("query at index {i} has empty workspace key");
        }
        if !declared_keys.contains(query.workspace.as_str()) {
            bail!(
                "query at index {i} references undeclared workspace key {:?}",
                query.workspace
            );
        }
        for pattern in &query.expected_paths {
            validate_pattern(pattern)
                .with_context(|| format!("query at index {i}: invalid expected_paths pattern"))?;
        }
        for pattern in &query.preferred_paths {
            validate_pattern(pattern)
                .with_context(|| format!("query at index {i}: invalid preferred_paths pattern"))?;
        }
        for pattern in &query.disfavored_paths {
            validate_pattern(pattern)
                .with_context(|| format!("query at index {i}: invalid disfavored_paths pattern"))?;
        }
        if let Some(filters) = &query.filters {
            if let Some(ref paths) = filters.include_paths {
                for pattern in paths {
                    validate_pattern(pattern).with_context(|| {
                        format!("query at index {i}: invalid include_paths pattern")
                    })?;
                }
            }
            if let Some(ref paths) = filters.exclude_paths {
                for pattern in paths {
                    validate_pattern(pattern).with_context(|| {
                        format!("query at index {i}: invalid exclude_paths pattern")
                    })?;
                }
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Execution
// ---------------------------------------------------------------------------

/// A workspace mapping supplied on the CLI: `key=path`.
#[derive(Debug, Clone)]
pub struct WorkspaceMapping {
    pub key: String,
    pub path: PathBuf,
}

/// Parse a `key=path` CLI argument into a [`WorkspaceMapping`].
pub fn parse_workspace_mapping(arg: &str) -> Result<WorkspaceMapping> {
    let (key, path) = arg
        .split_once('=')
        .ok_or_else(|| anyhow!("workspace mapping must be key=path, got: {arg:?}"))?;
    if key.trim().is_empty() {
        bail!("workspace mapping key must not be empty: {arg:?}");
    }
    if path.trim().is_empty() {
        bail!("workspace mapping path must not be empty: {arg:?}");
    }
    Ok(WorkspaceMapping {
        key: key.to_owned(),
        path: PathBuf::from(path),
    })
}

pub fn parse_workspace_mappings(args: &[String]) -> Result<Vec<WorkspaceMapping>> {
    let mut keys = BTreeSet::new();
    args.iter()
        .map(|arg| {
            let mapping = parse_workspace_mapping(arg)?;
            if !keys.insert(mapping.key.clone()) {
                bail!("duplicate workspace mapping key: {:?}", mapping.key);
            }
            Ok(mapping)
        })
        .collect()
}

/// Build bounded evidence rows from a search report's hits.
fn build_evidence(report: &SearchReport) -> Vec<ResultEvidence> {
    report
        .results
        .iter()
        .enumerate()
        .map(|(index, hit)| {
            let chunk = &hit.chunk;
            ResultEvidence {
                rank: index + 1,
                relative_file_path: chunk.relative_file_path.clone(),
                language: chunk.language.clone(),
                source_role: hit.source_role.as_str().to_owned(),
                start_line: chunk.start_line,
                end_line: chunk.end_line,
                snippet: bounded_snippet(&chunk.code),
                reranker_score: hit.reranker_score,
            }
        })
        .collect()
}

/// Execute all queries in an evaluation definition against the supplied
/// workspace mappings and return the aggregate report.
///
/// Missing required workspaces cause a failure. Missing optional
/// workspaces cause their queries to be skipped with an explicit reason.
pub async fn run_evaluation(
    app: &App,
    file: &EvaluationFile,
    workspaces: &[WorkspaceMapping],
) -> Result<AggregateReport> {
    // Build a key → path lookup from the CLI mappings.
    let mut provided: BTreeMap<&str, &Path> = BTreeMap::new();
    for mapping in workspaces {
        if provided
            .insert(mapping.key.as_str(), mapping.path.as_path())
            .is_some()
        {
            bail!("duplicate workspace mapping key: {:?}", mapping.key);
        }
    }

    let declarations: BTreeMap<&str, bool> = file
        .workspaces
        .iter()
        .map(|workspace| (workspace.key.as_str(), workspace.required))
        .collect();

    let mut reports: Vec<QueryReport> = Vec::with_capacity(file.queries.len());
    for query in &file.queries {
        let workspace_path = match provided.get(query.workspace.as_str()) {
            Some(path) => *path,
            None if !declarations[query.workspace.as_str()] => {
                let reason = format!("optional workspace {:?} not provided", query.workspace);
                reports.push(QueryReport::skipped(query.clone(), reason));
                continue;
            }
            None => {
                reports.push(QueryReport::failed(
                    query.clone(),
                    format!("required workspace {:?} not provided", query.workspace),
                ));
                continue;
            }
        };

        let filter_request = query.filters.clone().unwrap_or_default();
        let report = match app
            .search_with_filters(workspace_path, &query.query, query.top_k, filter_request)
            .await
        {
            Ok(report) => report,
            Err(error) => {
                reports.push(QueryReport::failed(
                    query.clone(),
                    format!("search failed: {error:#}"),
                ));
                continue;
            }
        };

        let evidence = build_evidence(&report);
        reports.push(QueryReport::new(
            query.clone(),
            evidence,
            report.reranked,
            report.warning,
            Some(report.filters),
            Some(EvalTimings {
                query_embedding_ms: report.timings.query_embedding_ms,
                lancedb_search_ms: report.timings.lancedb_search_ms,
                lexical_search_ms: report.timings.lexical_search_ms,
                lsp_search_ms: report.timings.lsp_search_ms,
                fusion_ms: report.timings.fusion_ms,
                reranking_ms: report.timings.reranking_ms,
                total_ms: report.timings.total_ms,
            }),
            Some(match report.index.action {
                IndexAction::Reused => IndexLifecycleAction::Reused,
                IndexAction::Created => IndexLifecycleAction::Created,
                IndexAction::WaitedForExistingJob => IndexLifecycleAction::WaitedForExistingJob,
            }),
            Some(report.index.wait_ms),
        ));
    }

    Ok(aggregate(&reports))
}

pub fn has_failures(report: &AggregateReport) -> bool {
    report.failure_count > 0
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence_row(
        rank: usize,
        path: &str,
        role: &str,
        snippet: &str,
        score: Option<f32>,
    ) -> ResultEvidence {
        ResultEvidence {
            rank,
            relative_file_path: path.to_owned(),
            language: "rust".to_owned(),
            source_role: role.to_owned(),
            start_line: 1,
            end_line: 10,
            snippet: snippet.to_owned(),
            reranker_score: score,
        }
    }

    fn query_with(expected: &[&str]) -> EvalQuery {
        EvalQuery {
            query: "find the metric helper".to_owned(),
            workspace: "ws".to_owned(),
            top_k: None,
            filters: None,
            expected_paths: expected.iter().map(|s| (*s).to_owned()).collect(),
            preferred_paths: Vec::new(),
            disfavored_paths: Vec::new(),
            expected_roles: Vec::new(),
            required_snippet: None,
        }
    }

    fn timings(total_ms: f64) -> EvalTimings {
        EvalTimings {
            query_embedding_ms: 0.0,
            lancedb_search_ms: 0.0,
            lexical_search_ms: 0.0,
            lsp_search_ms: 0.0,
            fusion_ms: 0.0,
            reranking_ms: 0.0,
            total_ms,
        }
    }

    #[test]
    fn pattern_matching_rules() {
        // Exact match.
        assert!(pattern_matches("src/metric.rs", "src/metric.rs"));
        // Glob `*` within a component.
        assert!(pattern_matches("src/metric*.rs", "src/metric.rs"));
        assert!(pattern_matches("src/metric*.rs", "src/metrics.rs"));
        assert!(!pattern_matches("src/metric*.rs", "src/metric.rs.bak"));
        // `?` matches exactly one character.
        assert!(pattern_matches("src/metric?.rs", "src/metric1.rs"));
        assert!(!pattern_matches("src/metric?.rs", "src/metric12.rs"));
        // `**` matches any run of whole components.
        assert!(pattern_matches("src/**/metric.rs", "src/metric.rs"));
        assert!(pattern_matches("src/**/metric.rs", "src/a/b/metric.rs"));
        assert!(!pattern_matches("src/**/metric.rs", "src/metric.rs.bak"));
        // Directory trees use `/**`.
        assert!(pattern_matches("src/metrics/**", "src/metrics/latency.rs"));
        assert!(!pattern_matches(
            "src/metrics/**",
            "src/metricsx/latency.rs"
        ));
        // Component count must line up.
        assert!(!pattern_matches("metric.rs", "src/metric.rs"));
        assert!(!pattern_matches("src/metric.rs", "metric.rs"));
        // Empty patterns match nothing.
        assert!(!pattern_matches("", "src/metric.rs"));
        assert!(!pattern_matches("metric.rs", ""));
        // Character class.
        assert!(pattern_matches("src/metric[0-9].rs", "src/metric1.rs"));
        assert!(!pattern_matches("src/metric[0-9].rs", "src/metrica.rs"));
    }

    #[test]
    fn rank_one_hit_scores_perfectly() {
        let query = query_with(&["src/metric.rs"]);
        let evidence = vec![
            evidence_row(1, "src/metric.rs", "source", "fn metric() {}", Some(0.9)),
            evidence_row(2, "src/other.rs", "source", "fn other() {}", Some(0.5)),
        ];
        let metrics = query_metrics(&query, &evidence, true);
        assert!(metrics.hit);
        assert!(metrics.hit_at_1);
        assert!(metrics.hit_at_3);
        assert!(metrics.hit_at_8_or_window);
        assert_eq!(metrics.window, 2);
        assert_eq!(metrics.first_expected_rank, Some(1));
        assert_eq!(metrics.reciprocal_rank, 1.0);
        assert_eq!(metrics.reranker, RerankerState::Reranked);
    }

    #[test]
    fn no_expected_match_is_a_miss() {
        let mut query = query_with(&["src/missing.rs"]);
        query.disfavored_paths = vec!["src/noise.rs".to_owned()];
        let evidence = vec![
            evidence_row(1, "src/noise.rs", "source", "fn noise() {}", None),
            evidence_row(2, "src/other.rs", "source", "fn other() {}", None),
        ];
        let metrics = query_metrics(&query, &evidence, false);
        assert!(!metrics.hit);
        assert!(!metrics.hit_at_1);
        assert!(!metrics.hit_at_3);
        assert!(!metrics.hit_at_8_or_window);
        assert_eq!(metrics.first_expected_rank, None);
        assert_eq!(metrics.reciprocal_rank, 0.0);
        // No expected hit: the whole evidence is counted for disfavored.
        assert_eq!(metrics.disfavored_before_first_expected, 1);
        assert_eq!(metrics.reranker, RerankerState::NotReranked);
        assert_eq!(metrics.expected_roles_satisfied, None);
        assert_eq!(metrics.required_snippet_found, None);
    }

    #[test]
    fn duplicate_patterns_match_one_hit() {
        let mut query = query_with(&["src/metric.rs", "metric.rs", "src/metrics/"]);
        query.preferred_paths = vec!["metric.rs".to_owned(), "src/metric.rs".to_owned()];
        let evidence = vec![
            evidence_row(1, "src/other.rs", "source", "fn other() {}", None),
            evidence_row(2, "src/metric.rs", "source", "fn metric() {}", None),
            evidence_row(
                3,
                "src/metrics/latency.rs",
                "source",
                "fn latency() {}",
                None,
            ),
        ];
        let metrics = query_metrics(&query, &evidence, false);
        // Three patterns all match rank 2; it is still one hit at rank 2.
        assert_eq!(metrics.first_expected_rank, Some(2));
        assert!(metrics.hit);
        assert!(!metrics.hit_at_1);
        assert!(metrics.hit_at_3);
        assert_eq!(metrics.reciprocal_rank, 0.5);
        // Two preferred patterns match the same row: counted once.
        assert_eq!(metrics.preferred_count, 1);
    }

    #[test]
    fn disfavored_counted_only_before_first_expected() {
        let mut query = query_with(&["src/metric.rs"]);
        query.disfavored_paths = vec!["src/noise.rs".to_owned()];
        let before = vec![
            evidence_row(1, "src/noise.rs", "source", "n", None),
            evidence_row(2, "src/metric.rs", "source", "m", None),
            evidence_row(3, "src/noise.rs", "source", "n", None),
        ];
        let metrics = query_metrics(&query, &before, false);
        assert_eq!(metrics.disfavored_before_first_expected, 1);

        let after = vec![
            evidence_row(1, "src/metric.rs", "source", "m", None),
            evidence_row(2, "src/noise.rs", "source", "n", None),
        ];
        let metrics = query_metrics(&query, &after, false);
        assert_eq!(metrics.disfavored_before_first_expected, 0);
    }

    #[test]
    fn window_bounds_the_windowed_metrics() {
        let query = query_with(&["src/ninth.rs"]);
        let mut evidence: Vec<ResultEvidence> = (1..=10)
            .map(|rank| evidence_row(rank, &format!("src/row{rank}.rs"), "source", "", None))
            .collect();
        evidence[8] = evidence_row(9, "src/ninth.rs", "source", "", None);
        let metrics = query_metrics(&query, &evidence, false);
        // The hit exists but outside the 8-result window.
        assert!(metrics.hit);
        assert!(!metrics.hit_at_1);
        assert!(!metrics.hit_at_3);
        assert!(!metrics.hit_at_8_or_window);
        assert_eq!(metrics.window, 8);
        assert_eq!(metrics.first_expected_rank, Some(9));

        let small = vec![
            evidence_row(1, "src/a.rs", "source", "", None),
            evidence_row(2, "src/b.rs", "source", "", None),
            evidence_row(3, "src/c.rs", "source", "", None),
        ];
        let query = query_with(&["src/c.rs"]);
        let metrics = query_metrics(&query, &small, false);
        // Window smaller than 8: the last row still counts.
        assert!(metrics.hit_at_3);
        assert!(metrics.hit_at_8_or_window);
        assert_eq!(metrics.window, 3);
    }

    #[test]
    fn role_distribution_counts_each_role() {
        let query = query_with(&[]);
        let evidence = vec![
            evidence_row(1, "src/a.rs", "source", "", None),
            evidence_row(2, "tests/a.rs", "test", "", None),
            evidence_row(3, "src/b.rs", "source", "", None),
        ];
        let metrics = query_metrics(&query, &evidence, false);
        assert_eq!(metrics.role_distribution["source"], 2);
        assert_eq!(metrics.role_distribution["test"], 1);
        assert_eq!(metrics.role_distribution.len(), 2);
    }

    #[test]
    fn reranker_state_variants() {
        let query = query_with(&[]);
        assert_eq!(
            query_metrics(&query, &[], false).reranker,
            RerankerState::NoResults
        );
        let unscored = vec![evidence_row(1, "src/a.rs", "source", "", None)];
        assert_eq!(
            query_metrics(&query, &unscored, true).reranker,
            RerankerState::PartiallyScored
        );
        let partial = vec![
            evidence_row(1, "src/a.rs", "source", "", Some(0.9)),
            evidence_row(2, "src/b.rs", "source", "", None),
        ];
        assert_eq!(
            query_metrics(&query, &partial, true).reranker,
            RerankerState::PartiallyScored
        );
        let scored = vec![
            evidence_row(1, "src/a.rs", "source", "", Some(0.9)),
            evidence_row(2, "src/b.rs", "source", "", Some(0.4)),
        ];
        assert_eq!(
            query_metrics(&query, &scored, true).reranker,
            RerankerState::Reranked
        );
        assert_eq!(
            query_metrics(&query, &scored, false).reranker,
            RerankerState::NotReranked
        );
    }

    #[test]
    fn expected_roles_and_required_snippet() {
        let mut query = query_with(&["src/metric.rs"]);
        query.expected_roles = vec!["source".to_owned()];
        query.required_snippet = Some("fn metric".to_owned());
        let hit = vec![evidence_row(
            1,
            "src/metric.rs",
            "source",
            "fn metric() {}",
            None,
        )];
        let metrics = query_metrics(&query, &hit, false);
        assert_eq!(metrics.expected_roles_satisfied, Some(true));
        assert_eq!(metrics.required_snippet_found, Some(true));

        let wrong_role = vec![evidence_row(
            1,
            "src/metric.rs",
            "test",
            "fn metric() {}",
            None,
        )];
        let metrics = query_metrics(&query, &wrong_role, false);
        assert_eq!(metrics.expected_roles_satisfied, Some(false));

        let missing_snippet = vec![evidence_row(
            1,
            "src/metric.rs",
            "source",
            "fn other() {}",
            None,
        )];
        let metrics = query_metrics(&query, &missing_snippet, false);
        assert_eq!(metrics.required_snippet_found, Some(false));

        // An absent role list is satisfied on a hit; no snippet request is unreported.
        let open = query_with(&["src/metric.rs"]);
        let metrics = query_metrics(&open, &hit, false);
        assert_eq!(metrics.expected_roles_satisfied, Some(true));
        assert_eq!(metrics.required_snippet_found, None);
    }

    #[test]
    fn aggregate_zero_samples() {
        let report = aggregate(&[]);
        assert_eq!(report.query_count, 0);
        assert_eq!(report.executed_count, 0);
        assert_eq!(report.skipped_count, 0);
        assert_eq!(report.success_count, 0);
        assert_eq!(report.failure_count, 0);
        assert_eq!(report.hit_rate, None);
        assert_eq!(report.hit_at_1_rate, None);
        assert_eq!(report.hit_at_3_rate, None);
        assert_eq!(report.hit_at_8_or_window_rate, None);
        assert_eq!(report.mrr, None);
        assert_eq!(report.median_latency_ms, None);
        assert_eq!(report.p95_latency_ms, None);
        assert_eq!(report.fallback_rate, None);
        assert_eq!(report.index_created, 0);
        assert_eq!(report.index_reused, 0);
        assert_eq!(report.index_waited, 0);
    }

    #[test]
    fn aggregate_single_sample() {
        let query = query_with(&["src/metric.rs"]);
        let evidence = vec![evidence_row(
            1,
            "src/metric.rs",
            "source",
            "fn metric() {}",
            Some(0.9),
        )];
        let report = QueryReport::new(
            query,
            evidence,
            true,
            None,
            None,
            Some(timings(12.5)),
            Some(IndexLifecycleAction::Reused),
            Some(0.0),
        );
        let aggregate = aggregate(std::slice::from_ref(&report));
        assert_eq!(aggregate.query_count, 1);
        assert_eq!(aggregate.executed_count, 1);
        assert_eq!(aggregate.success_count, 1);
        assert_eq!(aggregate.failure_count, 0);
        assert_eq!(aggregate.hit_queries, 1);
        assert_eq!(aggregate.hit_rate, Some(1.0));
        assert_eq!(aggregate.mrr, Some(1.0));
        assert_eq!(aggregate.median_latency_ms, Some(12.5));
        assert_eq!(aggregate.p95_latency_ms, Some(12.5));
        assert_eq!(aggregate.fallback_queries, 0);
        assert_eq!(aggregate.fallback_rate, Some(0.0));
        assert_eq!(aggregate.index_reused, 1);
    }

    #[test]
    fn aggregate_median_even_and_odd() {
        let reports: Vec<QueryReport> = [10.0, 20.0, 30.0, 40.0]
            .into_iter()
            .map(|total| {
                let query = query_with(&["src/missing.rs"]);
                QueryReport::new(
                    query,
                    Vec::new(),
                    false,
                    None,
                    None,
                    Some(timings(total)),
                    None,
                    None,
                )
            })
            .collect();
        let even_aggregate = aggregate(&reports);
        // Even sample: mean of the two middle values.
        assert_eq!(even_aggregate.median_latency_ms, Some(25.0));
        // Nearest-rank p95 of 4 samples: ceil(0.95 * 4) = 4th value.
        assert_eq!(even_aggregate.p95_latency_ms, Some(40.0));

        let odd: Vec<QueryReport> = [10.0, 20.0, 30.0]
            .into_iter()
            .map(|total| {
                let query = query_with(&["src/missing.rs"]);
                QueryReport::new(
                    query,
                    Vec::new(),
                    false,
                    None,
                    None,
                    Some(timings(total)),
                    None,
                    None,
                )
            })
            .collect();
        let aggregate = aggregate(&odd);
        // Odd sample: the middle value.
        assert_eq!(aggregate.median_latency_ms, Some(20.0));
        // Nearest-rank p95 of 3 samples: ceil(2.85) = 3rd value.
        assert_eq!(aggregate.p95_latency_ms, Some(30.0));
    }

    #[test]
    fn aggregate_p95_nearest_rank_over_twenty() {
        let reports: Vec<QueryReport> = (1..=20)
            .map(|total| {
                let query = query_with(&["src/missing.rs"]);
                QueryReport::new(
                    query,
                    Vec::new(),
                    false,
                    None,
                    None,
                    Some(timings(total as f64)),
                    None,
                    None,
                )
            })
            .collect();
        let aggregate = aggregate(&reports);
        // Nearest-rank p95 of 20 samples: ceil(19) = 19th value.
        assert_eq!(aggregate.p95_latency_ms, Some(19.0));
        // Even sample: mean of the 10th and 11th values.
        assert_eq!(aggregate.median_latency_ms, Some(10.5));
    }

    #[test]
    fn skipped_optional_workspace_excluded_from_denominators() {
        let skipped = QueryReport::skipped(
            query_with(&["src/metric.rs"]),
            "optional workspace not provided".to_owned(),
        );
        let query = query_with(&["src/metric.rs"]);
        let evidence = vec![evidence_row(
            1,
            "src/metric.rs",
            "source",
            "fn metric() {}",
            None,
        )];
        let hit = QueryReport::new(
            query,
            evidence,
            false,
            None,
            None,
            Some(timings(5.0)),
            None,
            None,
        );
        let aggregate = aggregate(&[skipped, hit]);
        assert_eq!(aggregate.query_count, 2);
        assert_eq!(aggregate.skipped_count, 1);
        assert_eq!(aggregate.executed_count, 1);
        assert_eq!(aggregate.success_count, 1);
        assert_eq!(aggregate.failure_count, 0);
        assert_eq!(aggregate.hit_queries, 1);
        assert_eq!(aggregate.hit_rate, Some(1.0));
        assert_eq!(aggregate.mrr, Some(1.0));
        assert_eq!(aggregate.median_latency_ms, Some(5.0));
    }

    #[test]
    fn lifecycle_counts_and_fallback_rate() {
        let make = |action: IndexLifecycleAction, warning: Option<&str>| {
            let query = query_with(&["src/missing.rs"]);
            QueryReport::new(
                query,
                Vec::new(),
                false,
                warning.map(str::to_owned),
                None,
                None,
                Some(action),
                None,
            )
        };
        let reports = vec![
            make(
                IndexLifecycleAction::Created,
                Some("query embedding unavailable"),
            ),
            make(IndexLifecycleAction::Reused, None),
            make(IndexLifecycleAction::WaitedForExistingJob, None),
            make(
                IndexLifecycleAction::Reused,
                Some("query embedding unavailable"),
            ),
        ];
        let aggregate = aggregate(&reports);
        assert_eq!(aggregate.index_created, 1);
        assert_eq!(aggregate.index_reused, 2);
        assert_eq!(aggregate.index_waited, 1);
        assert_eq!(aggregate.fallback_queries, 2);
        assert_eq!(aggregate.fallback_rate, Some(0.5));
        assert_eq!(aggregate.hit_rate, Some(0.0));
    }

    #[test]
    fn evaluation_file_round_trips_through_serde() {
        let file = EvaluationFile {
            workspaces: vec![
                EvalWorkspace {
                    key: "core".to_owned(),
                    required: true,
                },
                EvalWorkspace {
                    key: "web".to_owned(),
                    required: false,
                },
            ],
            queries: vec![EvalQuery {
                query: "find the metric helper".to_owned(),
                workspace: "core".to_owned(),
                top_k: Some(8),
                filters: Some(FilterRequest {
                    languages: Some(vec!["rust".to_owned()]),
                    include_paths: None,
                    exclude_paths: Some(vec!["src/generated/**".to_owned()]),
                    source_roles: Some(vec!["source".to_owned()]),
                }),
                expected_paths: vec!["src/metric.rs".to_owned()],
                preferred_paths: vec![],
                disfavored_paths: vec!["src/noise.rs".to_owned()],
                expected_roles: vec!["source".to_owned()],
                required_snippet: Some("fn metric".to_owned()),
            }],
        };
        let json = serde_json::to_string(&file).unwrap();
        let back: EvaluationFile = serde_json::from_str(&json).unwrap();
        // Compare via JSON since the types no longer derive PartialEq.
        assert_eq!(json, serde_json::to_string(&back).unwrap());

        let query = query_with(&["src/metric.rs"]);
        let evidence = vec![evidence_row(
            1,
            "src/metric.rs",
            "source",
            "fn metric() {}",
            Some(0.9),
        )];
        let report = QueryReport::new(
            query,
            evidence,
            true,
            None,
            None,
            Some(timings(12.5)),
            Some(IndexLifecycleAction::Created),
            Some(3.0),
        );
        let aggregate = aggregate(std::slice::from_ref(&report));
        let json = serde_json::to_string(&aggregate).unwrap();
        let back: AggregateReport = serde_json::from_str(&json).unwrap();
        assert_eq!(json, serde_json::to_string(&back).unwrap());
    }

    #[test]
    fn bounded_snippet_truncates_long_code() {
        let short = "fn metric() {}";
        assert_eq!(bounded_snippet(short), short);

        let long = "let x = 1; ".repeat(100);
        let snippet = bounded_snippet(&long);
        assert!(snippet.ends_with('…'));
        assert!(snippet.chars().count() <= SNIPPET_BUDGET + 1);
        assert!(snippet.starts_with(&long[..100]));
    }
}
