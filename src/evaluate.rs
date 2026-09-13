//! Portable retrieval evaluation definitions, execution, metrics, and reports.
//!
//! Public items are re-exported from private child modules so the existing
//! `crate::evaluate::*` API remains stable. Evaluation observes retrieval; it
//! never changes production ranking.

mod definition;
mod metrics;
mod runner;
mod types;

pub use definition::{load_definition, validate_definition};
pub use metrics::{aggregate, pattern_matches, query_metrics, validate_pattern};
pub use runner::{
    WorkspaceMapping, has_failures, parse_workspace_mapping, parse_workspace_mappings,
    run_evaluation,
};
pub use types::{
    AggregateReport, EvalQuery, EvalTimings, EvalWorkspace, EvaluationFile, IndexLifecycleAction,
    QueryMetrics, QueryReport, RerankerState, ResultEvidence, SNIPPET_BUDGET, bounded_snippet,
};

#[cfg(test)]
mod tests;
