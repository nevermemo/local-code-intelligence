//! Portable retrieval evaluation definitions, execution, metrics, and reports.
//!
//! Public items are re-exported from private child modules so the existing
//! `crate::evaluate::*` API remains stable. Evaluation observes retrieval; it
//! never changes production ranking. [`compare()`] reads two finished reports
//! and measures the paired difference between them; it needs no services.
//! [`replay()`] scores other retrieval settings from the candidate pool that
//! `evaluate --dump-candidates` recorded, equally offline.

mod compare;
mod definition;
mod metrics;
mod replay;
mod run_record;
mod runner;
mod types;

pub use compare::{
    CompareOptions, ComparisonReport, DEFAULT_RESAMPLES, DEFAULT_SEED, GroupComparison,
    MetricComparison, PairMetrics, PairedQuery, QueryKey, compare, compare_files, load_report,
    paired_bootstrap_ci, report_with_comparison,
};
pub use definition::{load_definition, validate_definition};
pub use metrics::{aggregate, pattern_matches, query_metrics, validate_pattern};
pub use replay::{
    CapStage, CostBasis, CostModel, FALLBACK_INTERCEPT_MS, FALLBACK_TOKENS_PER_SECOND, LinearFit,
    MIN_R_SQUARED, OriginalTimings, RefusedQuery, ReplayArm, ReplayBlock, ReplayCounts,
    ReplayLatency, ReplayOptions, ReplayOriginal, ReplayReport, fit_line, replay, replay_file,
};
pub use run_record::{FuseReplay, RunRecordFile};
pub use runner::{
    RunOptions, WorkspaceMapping, has_failures, parse_workspace_mapping, parse_workspace_mappings,
    run_evaluation, run_evaluation_with_options,
};
pub use types::{
    AggregateReport, CandidateArm, EvalQuery, EvalTimings, EvalWorkspace, EvaluationFile,
    IndexLifecycleAction, LatencyPercentiles, QueryMetrics, QueryReport, ReplayRefusal, RerankMode,
    RerankerState, ResultEvidence, SNIPPET_BUDGET, StageLatency, bounded_snippet,
};

#[cfg(test)]
mod tests;
