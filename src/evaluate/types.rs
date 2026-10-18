use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::app::{CandidateTrace, RerankOutcome};
use crate::config::{ChannelsConfig, Config, FusionConfig, Rescorer};
use crate::filter::EffectiveFilterReport;

use super::metrics::query_metrics;
pub use lci_core::evaluation::{EvalQuery, EvalWorkspace, EvaluationFile};

// ---------------------------------------------------------------------------
// Bounded result evidence
// ---------------------------------------------------------------------------

// The snippet bound lives in `store`, beside the retrieval pipeline that
// records it in a candidate dump; evaluation re-exports it.
pub use crate::store::{SNIPPET_BUDGET, bounded_snippet};

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

// ---------------------------------------------------------------------------
// Candidate dump
// ---------------------------------------------------------------------------

/// How the reranker's output is used to order the results.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RerankMode {
    /// The reranker's order replaces the fusion order (production behaviour
    /// whenever `[channels].reranker` is on).
    Replace,
    /// No reranker: the results are the head of the fusion order
    /// (`[channels].reranker = false`).
    Off,
    /// The reranker's order enters as a vote: each shortlisted candidate is
    /// re-scored `fusion_score + vote_weight / (rrf_k + rerank_rank)` and the
    /// shortlist is re-sorted. Replay-only: production has no such mode, so
    /// no dump carries it, but `replay --rerank-mode vote` applies it.
    Vote,
}

impl RerankMode {
    /// The lowercase wire/CLI name of the mode.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Replace => "replace",
            Self::Off => "off",
            Self::Vote => "vote",
        }
    }
}

/// Why `replay` could not score a dumped query under the replayed arm. A
/// refused query is never approximated: it is reported as skipped (so
/// `compare` leaves it out of every pair), counted, and listed under the
/// report's `replay` block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayRefusal {
    /// The query carries no candidate dump (the report was not made with
    /// `evaluate --dump-candidates`, or the query was dumped by nothing).
    NotDumped,
    /// Fusion recomputed at the dumped arm does not reproduce a row's
    /// `fusion_score` bit for bit, or the dump is not in `fused_rank` order.
    FusionMismatch,
    /// The per-file cap recomputed at the dumped arm does not reproduce a
    /// row's `capped_rank` or `shortlisted`.
    ShortlistMismatch,
    /// The replayed arm uses the reranker but the dumped search has no
    /// reranker scores (its outcome was not `scored`).
    RerankNotScored,
    /// The replayed rerank window holds a candidate the dumped search never
    /// sent to the reranker, so it has no score.
    UnscoredWindow,
}

impl ReplayRefusal {
    /// The snake_case name used in the report.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NotDumped => "not_dumped",
            Self::FusionMismatch => "fusion_mismatch",
            Self::ShortlistMismatch => "shortlist_mismatch",
            Self::RerankNotScored => "rerank_not_scored",
            Self::UnscoredWindow => "unscored_window",
        }
    }
}

/// The retrieval configuration one candidate dump was taken under: what a
/// replay must hold equal (or may only narrow) to reproduce the dumped
/// search exactly. Recorded on every dumped query so a report stays
/// self-describing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateArm {
    /// Reciprocal Rank Fusion constant of the dumped fusion.
    pub rrf_k: f32,
    /// Channel weights and the per-file cap (`max_chunks_per_file`; `0` is
    /// unlimited) of the dumped fusion.
    pub fusion: FusionConfig,
    /// Size of the reranker shortlist (the cut after the cap).
    pub rerank_candidate_count: usize,
    /// Bytes of code each shortlisted document carried to the reranker.
    pub rerank_document_max_bytes: usize,
    /// Depth of the semantic channel's candidate list.
    pub semantic_candidate_count: usize,
    /// Depth of the lexical channel's candidate list.
    pub lexical_candidate_count: usize,
    /// Depth of the language-server channel's candidate list.
    pub lsp_candidate_count: usize,
    /// Which channels and whether the reranker were switched on.
    pub channels: ChannelsConfig,
    /// The result window the query asked for: `top_k`, or the configured
    /// default when the query has none.
    pub k: usize,
    /// Embedding endpoint, model, and document format the index was built
    /// with (`Config::embedding_identity`).
    pub embedding_identity: String,
    /// Reranker model name sent with every request.
    pub reranker_model: String,
    /// Non-default service that scored this arm's shortlist.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rescorer: Option<String>,
    /// How the reranker's output ordered the dumped results.
    pub rerank_mode: RerankMode,
}

impl CandidateArm {
    /// The arm `config` produces for a query whose result window is `k`.
    pub fn new(config: &Config, k: usize) -> Self {
        Self {
            rrf_k: config.rrf_k,
            fusion: config.fusion,
            rerank_candidate_count: config.rerank_candidate_count,
            rerank_document_max_bytes: config.rerank_document_max_bytes,
            semantic_candidate_count: config.semantic_candidate_count,
            lexical_candidate_count: config.lexical_candidate_count,
            lsp_candidate_count: config.lsp_candidate_count,
            channels: config.channels,
            k,
            embedding_identity: config.embedding_identity(),
            reranker_model: config.reranker_model.clone(),
            rescorer: match config.rescorer {
                Rescorer::Reranker => None,
                Rescorer::SystemOne => Some("system_one".to_owned()),
            },
            rerank_mode: if config.channels.reranker {
                RerankMode::Replace
            } else {
                RerankMode::Off
            },
        }
    }
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
    /// The whole fused candidate pool of the search, in fused order, with
    /// everything a replay needs (see [`CandidateTrace`]). Only
    /// `evaluate --dump-candidates` fills it; it is empty, and absent from the
    /// JSON, otherwise and for skipped or failed queries.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidates: Vec<CandidateTrace>,
    /// The retrieval configuration `candidates` was dumped under. Present
    /// exactly when the query was dumped, even if its pool is empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arm: Option<CandidateArm>,
    /// What the reranker did for a dumped query: scored, failed, off, or not
    /// run. Present exactly when [`Self::arm`] is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rerank_outcome: Option<RerankOutcome>,
    /// The search report's informational notes, for example "LSP still
    /// starting or slow" when the language-server channel hit its budget and
    /// contributed nothing or less than usual. A replay can use them to
    /// exclude or stratify such queries. Never a fallback. Only
    /// `evaluate --dump-candidates` fills it, together with `arm`; it is
    /// empty, and absent from the JSON, otherwise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Set by `replay` on a query it refused to score (see
    /// [`ReplayRefusal`]); such a query is also `skipped`, with the detail
    /// in `reason`. Absent from every other report.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replay_refusal: Option<ReplayRefusal>,
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
            candidates: Vec::new(),
            arm: None,
            rerank_outcome: None,
            notes: Vec::new(),
            replay_refusal: None,
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
            candidates: Vec::new(),
            arm: None,
            rerank_outcome: None,
            notes: Vec::new(),
            replay_refusal: None,
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

/// Median, nearest-rank 95th percentile, maximum and sample count of one
/// latency series, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LatencyPercentiles {
    /// Median (mean of the two middle values for even sample counts).
    pub p50: f64,
    /// Nearest-rank 95th percentile. Below 20 samples this is the maximum
    /// itself and for 20 to 39 it is the second-highest value, so a single
    /// outlier can hide; read `max` for the worst query.
    pub p95: f64,
    /// Largest sample. Reports written before this field existed load as 0.
    #[serde(default)]
    pub max: f64,
    /// Number of samples (queries with timings). Reports written before this
    /// field existed load as 0.
    #[serde(default)]
    pub n: usize,
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

/// Headline metrics for the executed queries that carry one tag (see
/// [`EvalQuery::tags`]). A query with several tags counts once in each.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TagAggregate {
    /// Executed (non-skipped) queries carrying the tag; always at least one.
    pub executed_count: usize,
    /// hit@1 rate over those queries.
    pub hit_at_1_rate: f64,
    /// hit@3 rate over those queries.
    pub hit_at_3_rate: f64,
    /// Mean reciprocal rank over those queries.
    pub mrr: f64,
    /// Mean [`QueryMetrics::ndcg_at_10`] over those queries (bounded by each
    /// query's result window, see there).
    pub mean_ndcg_at_10: f64,
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
    /// Per-stage p50/p95/max latency and sample count over evaluated queries
    /// with timings, using the same median and nearest-rank percentile as
    /// the total-latency fields; `None` when there are no samples.
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
    /// Per-tag aggregates over executed queries, keyed by tag. Empty (and
    /// omitted from JSON) when no executed query carries a tag; a tag that
    /// only appears on skipped queries has no entry.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub by_tag: BTreeMap<String, TagAggregate>,
    /// The per-query reports.
    #[serde(default)]
    pub queries: Vec<QueryReport>,
}
