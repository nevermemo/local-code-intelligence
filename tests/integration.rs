include!("support/mod.rs");

#[path = "cases/config.rs"]
mod config;
#[path = "cases/evaluation.rs"]
mod evaluation;
#[path = "cases/indexing.rs"]
mod indexing;
#[path = "cases/mcp.rs"]
mod mcp;
#[path = "cases/navigation.rs"]
mod navigation;
#[path = "cases/readiness.rs"]
mod readiness;
#[path = "cases/recipes.rs"]
mod recipes;
// Calls `App::record_through_maps`, which exists only with this feature.
#[cfg(feature = "map-equivalence")]
#[path = "cases/run_records.rs"]
mod run_records;
#[path = "cases/watching.rs"]
mod watching;
