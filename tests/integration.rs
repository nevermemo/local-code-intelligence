include!("support/mod.rs");

#[path = "cases/child_stdin.rs"]
mod child_stdin;
#[path = "cases/config.rs"]
mod config;
#[path = "cases/evaluation.rs"]
mod evaluation;
#[path = "cases/freshness.rs"]
mod freshness;
#[path = "cases/indexing.rs"]
mod indexing;
#[path = "cases/lsp_breaker.rs"]
mod lsp_breaker;
#[path = "cases/lsp_concurrent_open.rs"]
mod lsp_concurrent_open;
#[path = "cases/lsp_diagnostics_wait.rs"]
mod lsp_diagnostics_wait;
#[path = "cases/markdown_lsp.rs"]
mod markdown_lsp;
#[path = "cases/mcp.rs"]
mod mcp;
#[path = "cases/navigation.rs"]
mod navigation;
#[path = "cases/navigation_barrier.rs"]
mod navigation_barrier;
#[path = "cases/readiness.rs"]
mod readiness;
#[path = "cases/recipes.rs"]
mod recipes;
// Calls `App::record_through_maps`, which exists only with this feature.
#[cfg(feature = "map-equivalence")]
#[path = "cases/run_records.rs"]
mod run_records;
#[path = "cases/traverse_matrix.rs"]
mod traverse_matrix;
#[path = "cases/watching.rs"]
mod watching;
