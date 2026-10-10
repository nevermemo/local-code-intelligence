//! Portable retrieval evaluation definitions, execution, metrics, and reports.
//!
//! Public items are re-exported from private child modules so the existing
//! `crate::evaluate::*` API remains stable. Evaluation observes retrieval; it
//! never changes production ranking. [`compare()`] reads two finished reports
//! and measures the paired difference between them; it needs no services.

mod compare;
mod definition;
mod metrics;
mod runner;
mod types;

pub use compare::{
    CompareOptions, ComparisonReport, DEFAULT_RESAMPLES, DEFAULT_SEED, GroupComparison,
    MetricComparison, PairMetrics, PairedQuery, QueryKey, compare, compare_files, load_report,
    paired_bootstrap_ci, report_with_comparison,
};
pub use definition::{load_definition, validate_definition};
pub use metrics::{aggregate, pattern_matches, query_metrics, validate_pattern};
pub use runner::{
    WorkspaceMapping, has_failures, parse_workspace_mapping, parse_workspace_mappings,
    run_evaluation,
};
pub use types::{
    AggregateReport, EvalQuery, EvalTimings, EvalWorkspace, EvaluationFile, IndexLifecycleAction,
    LatencyPercentiles, QueryMetrics, QueryReport, RerankerState, ResultEvidence, SNIPPET_BUDGET,
    StageLatency, bounded_snippet,
};

#[cfg(test)]
mod tests;
