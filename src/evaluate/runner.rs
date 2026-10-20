use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow, bail};

use lci_core::evaluation::EvalLane;

use crate::app::{App, IndexAction, SearchOptions, SearchReport};
use crate::store::Hit;

use super::definition::validate_definition;
use super::metrics::aggregate;
use super::types::{
    AggregateReport, CandidateArm, EvalTimings, EvaluationFile, IndexLifecycleAction, QueryReport,
    ResultEvidence, bounded_snippet,
};
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

/// Bounded evidence rows for `hits`, ranked from one.
fn evidence_of(hits: &[Hit]) -> Vec<ResultEvidence> {
    hits.iter()
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

/// The evidence a query is scored on and the evidence of the search's other
/// list (counted in the token proxy, not scored), by the query's lane: the
/// code `results` (the default), the `docs` list, or both in that order.
fn lane_evidence(
    lane: EvalLane,
    report: &SearchReport,
) -> (Vec<ResultEvidence>, Vec<ResultEvidence>) {
    let code = evidence_of(&report.results);
    let docs = report
        .docs
        .as_ref()
        .map_or_else(Vec::new, |docs| evidence_of(&docs.results));
    match lane {
        EvalLane::Code => (code, docs),
        EvalLane::Docs => (docs, code),
        EvalLane::Union => {
            let mut rows = code;
            rows.extend(docs);
            for (index, row) in rows.iter_mut().enumerate() {
                row.rank = index + 1;
            }
            (rows, Vec::new())
        }
    }
}

/// Switches of one evaluation run that never change what it measures.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RunOptions {
    /// Record, for every executed query, the whole fused candidate pool and
    /// the retrieval configuration it came from (`evaluate
    /// --dump-candidates`), so another arm can be scored from the report
    /// without services. Results, metrics, and every other field of the
    /// report are the same with it on or off.
    pub dump_candidates: bool,
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
    run_evaluation_with_options(app, file, workspaces, RunOptions::default()).await
}

/// [`run_evaluation`] with [`RunOptions`].
pub async fn run_evaluation_with_options(
    app: &App,
    file: &EvaluationFile,
    workspaces: &[WorkspaceMapping],
    options: RunOptions,
) -> Result<AggregateReport> {
    validate_definition(file)?;

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
        let search_options = SearchOptions {
            trace_candidates: options.dump_candidates,
        };
        let mut report = match app
            .search_with_options_unverified(
                workspace_path,
                &query.query,
                query.top_k,
                filter_request,
                search_options,
            )
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

        let lane = query.lane();
        let docs_list_len = report.docs.as_ref().map(|docs| docs.results.len());
        let (evidence, unscored) = lane_evidence(lane, &report);
        let candidates = report.candidates.take();
        let rerank_outcome = report.rerank_outcome.take();
        // A query scored on the docs lane also answers for its failures: a
        // docs-lane warning is that query's fallback. A code query does not
        // (the lane is fail-open and the code list is unaffected).
        let docs_warning = report.docs.as_ref().and_then(|docs| docs.warning.clone());
        let warning = match (lane, docs_warning) {
            (EvalLane::Code, _) | (_, None) => report.warning,
            (_, Some(docs)) => Some(match report.warning {
                Some(code) => format!("{code}; {docs}"),
                None => docs,
            }),
        };
        let reranked = match (lane, report.docs.as_ref()) {
            (EvalLane::Docs, Some(docs)) => docs.reranked,
            (EvalLane::Docs, None) => false,
            _ => report.reranked,
        };
        let mut query_report = QueryReport::new(
            query.clone(),
            evidence,
            reranked,
            warning,
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
                IndexAction::RefreshedIncrementally => IndexLifecycleAction::RefreshedIncrementally,
            }),
            Some(report.index.wait_ms),
        );
        if !unscored.is_empty() {
            query_report = query_report.with_unscored_evidence(unscored);
        }
        // The docs list is never longer than `[docs_lane].top_k`, whatever
        // the query asks for; say so when that, not the index, bounded it.
        if let (EvalLane::Docs | EvalLane::Union, Some(docs)) = (lane, docs_list_len) {
            let asked = query.top_k.unwrap_or(app.config.default_top_k);
            let bound = app.config.docs_lane.top_k;
            if docs == bound && asked > bound {
                query_report.notes.push(format!(
                    "docs list bounded by [docs_lane].top_k = {bound} (query top_k {asked}); a docs arm sets docs_lane.top_k >= the query top_k"
                ));
            }
        }
        if let Some(candidates) = candidates {
            query_report.candidates = candidates;
            query_report.arm = Some(CandidateArm::new(
                &app.config,
                query.top_k.unwrap_or(app.config.default_top_k),
            ));
            query_report.rerank_outcome = rerank_outcome;
            // Notes (an LSP channel that hit its budget, say) change what a
            // dumped pool means, so a dump keeps them. Without the flag the
            // report is exactly what it always was and carries none.
            query_report.notes.extend(report.notes);
        }
        reports.push(query_report);
    }

    Ok(aggregate(&reports))
}

pub fn has_failures(report: &AggregateReport) -> bool {
    report.failure_count > 0
}
