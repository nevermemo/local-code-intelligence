//! Hybrid retrieval: index freshness, semantic/lexical/LSP fan-out,
//! Reciprocal Rank Fusion, and reranking.

use super::{App, IndexAction, WorkspaceCoordination, ms};
use crate::{
    config::IndexFreshness,
    filter::{EffectiveFilter, EffectiveFilterReport, FilterRequest},
    lexical,
    lsp::adapter::LspAdapter,
    store::{Hit, Snapshot, Store},
    workspace::Workspace,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Instant,
};

#[derive(Debug, Default, Serialize)]
pub struct Timings {
    pub query_embedding_ms: f64,
    pub lancedb_search_ms: f64,
    pub lexical_search_ms: f64,
    pub lsp_search_ms: f64,
    pub fusion_ms: f64,
    pub reranking_ms: f64,
    pub total_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct SearchIndexLifecycle {
    pub action: IndexAction,
    pub wait_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct SearchReport {
    pub workspace: Workspace,
    pub query: String,
    pub candidate_count: usize,
    pub reranked: bool,
    pub warning: Option<String>,
    pub timings: Timings,
    pub results: Vec<Hit>,
    pub index: SearchIndexLifecycle,
    pub filters: EffectiveFilterReport,
}

fn hit_matches(filters: &EffectiveFilter, hit: &Hit) -> bool {
    filters.matches(
        &hit.chunk.language,
        &hit.chunk.relative_file_path,
        hit.source_role,
    )
}

/// Outcome of resolving which index snapshot a search should read: the
/// (re-checked) snapshot, the lifecycle action to report, and the read
/// guard that must stay held for the rest of the search so the snapshot
/// cannot be replaced out from under it.
struct IndexResolution<'a> {
    _read_guard: tokio::sync::RwLockReadGuard<'a, ()>,
    snapshot: Option<Snapshot>,
    action: IndexAction,
    wait_ms: f64,
    warning: Option<String>,
}

impl App {
    pub async fn search(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
    ) -> Result<SearchReport> {
        self.search_with_filters(path, query, top_k, FilterRequest::default())
            .await
    }

    pub async fn search_with_filters(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
        filter_request: FilterRequest,
    ) -> Result<SearchReport> {
        let total = Instant::now();
        ensure!(!query.trim().is_empty(), "query must not be empty");
        ensure!(query.len() <= 16000, "query exceeds 16000 bytes");
        let k = top_k.unwrap_or(self.config.default_top_k);
        ensure!((1..=40).contains(&k), "top_k must be 1..=40");
        let filtering_requested = filter_request.languages.is_some()
            || filter_request.include_paths.is_some()
            || filter_request.exclude_paths.is_some()
            || filter_request.source_roles.is_some();
        let filters = EffectiveFilter::from_request(&filter_request)?;
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let coordination = self.lock(&workspace.id).await;
        let initial_snapshot = self.store.snapshot(&workspace).await?;
        let resolution = self
            .resolve_index_for_search(&workspace, &coordination, initial_snapshot)
            .await?;
        let snapshot = resolution
            .snapshot
            .context("workspace is not indexed; call index_workspace first")?;
        let mut report = SearchReport {
            workspace,
            query: query.into(),
            candidate_count: 0,
            reranked: false,
            warning: resolution.warning,
            timings: Timings::default(),
            results: vec![],
            index: SearchIndexLifecycle {
                action: resolution.action,
                wait_ms: resolution.wait_ms,
            },
            filters: filters.report(),
        };
        if snapshot.table.count_rows(None).await? == 0 {
            report.timings.total_ms = ms(total);
            return Ok(report);
        }
        let mut all_chunks = Store::chunks(&snapshot, &report.workspace).await?;
        all_chunks.retain(|hit| hit_matches(&filters, hit));
        let indexed_languages: HashSet<&str> = all_chunks
            .iter()
            .map(|hit| hit.chunk.language.as_str())
            .collect();
        let mut applicable_adapters: HashMap<&'static str, Arc<dyn LspAdapter>> = HashMap::new();
        for language in indexed_languages {
            for adapter in self.analyzer.adapters_for_language(language) {
                if adapter.enabled() {
                    applicable_adapters
                        .entry(adapter.provider())
                        .or_insert_with(|| Arc::clone(adapter));
                }
            }
        }
        let (query_result, lexical_result, lsp_result) = tokio::join!(
            async {
                let timer = Instant::now();
                (self.models.query(query).await, ms(timer))
            },
            async {
                let timer = Instant::now();
                (
                    lexical::search(&self.config.ripgrep_path, &report.workspace.path, query).await,
                    ms(timer),
                )
            },
            async {
                let timer = Instant::now();
                if applicable_adapters.is_empty() {
                    return ((Vec::new(), Vec::new()), ms(timer));
                }
                let mut found = Vec::new();
                let mut failures = Vec::new();
                let mut symbol_queries = Vec::new();
                for term in lexical::terms(query) {
                    if term.ends_with('s') && term.len() > 4 {
                        symbol_queries.push(format!("{}#", &term[..term.len() - 1]));
                    }
                    symbol_queries.push(format!("{term}#"));
                }
                symbol_queries.dedup();
                for term in symbol_queries.into_iter().take(8) {
                    for adapter in applicable_adapters.values() {
                        match self
                            .analyzer
                            .symbols(adapter, &report.workspace, &term)
                            .await
                        {
                            Ok(mut locations) => found.append(&mut locations),
                            Err(error) => {
                                failures.push(format!("{}: {error:#}", adapter.provider()))
                            }
                        }
                    }
                }
                ((found, failures), ms(timer))
            },
        );
        let (query_vector, query_ms) = query_result;
        let (lexical_matches, lexical_ms) = lexical_result;
        let ((lsp_locations, lsp_failures), lsp_ms) = lsp_result;
        report.timings.query_embedding_ms = query_ms;
        report.timings.lexical_search_ms = lexical_ms;
        report.timings.lsp_search_ms = lsp_ms;
        if !lsp_failures.is_empty() {
            let message = format!(
                "Optional LSP provider unavailable: {}",
                lsp_failures.join("; ")
            );
            report.warning = Some(
                report
                    .warning
                    .take()
                    .map_or(message.clone(), |w| format!("{w}; {message}")),
            );
        }
        let semantic_timer = Instant::now();
        let semantic = match query_vector {
            Ok(vector) => match Store::search(
                &snapshot,
                &report.workspace,
                &vector,
                self.config.semantic_candidate_count,
                &filters,
            )
            .await
            {
                Ok(hits) => hits,
                Err(error) => {
                    let message = format!("Semantic retrieval unavailable: {error:#}");
                    report.warning = Some(
                        report
                            .warning
                            .take()
                            .map_or(message.clone(), |w| format!("{w}; {message}")),
                    );
                    vec![]
                }
            },
            Err(error) => {
                let message =
                    format!("Query embedding unavailable; lexical results used: {error:#}");
                report.warning = Some(
                    report
                        .warning
                        .take()
                        .map_or(message.clone(), |w| format!("{w}; {message}")),
                );
                vec![]
            }
        };
        report.timings.lancedb_search_ms = ms(semantic_timer);
        let mut lexical_hits = Vec::new();
        match lexical_matches {
            Ok(matches) => {
                lexical_hits = Self::map_lexical_matches(
                    &all_chunks,
                    matches,
                    self.config.lexical_candidate_count,
                );
            }
            Err(error) => {
                let message = format!("Lexical retrieval unavailable: {error:#}");
                report.warning = Some(
                    report
                        .warning
                        .take()
                        .map_or(message.clone(), |w| format!("{w}; {message}")),
                );
            }
        }
        let lsp_hits =
            Self::map_lsp_locations(&all_chunks, lsp_locations, self.config.lsp_candidate_count);
        if semantic.is_empty() && lexical_hits.is_empty() && lsp_hits.is_empty() {
            if filtering_requested {
                report.timings.total_ms = ms(total);
                return Ok(report);
            }
            ensure!(
                false,
                "semantic, lexical, and LSP retrieval all failed or returned no candidates"
            );
        }
        let fusion_timer = Instant::now();
        let mut hits = self.fuse_hits(semantic, lexical_hits, lsp_hits);
        hits.retain(|hit| hit_matches(&filters, hit));
        hits.sort_by(|a, b| {
            b.fusion_score
                .total_cmp(&a.fusion_score)
                .then_with(|| a.chunk.relative_file_path.cmp(&b.chunk.relative_file_path))
                .then(a.chunk.start_line.cmp(&b.chunk.start_line))
        });
        hits.truncate(self.config.rerank_candidate_count);
        report.timings.fusion_ms = ms(fusion_timer);
        report.candidate_count = hits.len();
        if !hits.is_empty() {
            let timer = Instant::now();
            self.rerank_into_report(query, hits, k, &mut report).await;
            report.timings.reranking_ms = ms(timer);
        }
        report.results.retain(|hit| hit_matches(&filters, hit));
        report.timings.total_ms = ms(total);
        tracing::info!(
            query_embedding_ms = report.timings.query_embedding_ms,
            lancedb_search_ms = report.timings.lancedb_search_ms,
            lexical_search_ms = report.timings.lexical_search_ms,
            fusion_ms = report.timings.fusion_ms,
            reranking_ms = report.timings.reranking_ms,
            total_ms = report.timings.total_ms,
            reranked = report.reranked,
            "retrieval complete"
        );
        Ok(report)
    }

    /// Resolves the index snapshot a search should read: creates or waits
    /// for a first-search index when the workspace has never been indexed,
    /// otherwise applies the configured freshness policy to an existing
    /// snapshot. Returns a read guard that must be held for the remainder
    /// of the search so the snapshot cannot be replaced concurrently.
    async fn resolve_index_for_search<'a>(
        &self,
        workspace: &Workspace,
        coordination: &'a WorkspaceCoordination,
        initial_snapshot: Option<Snapshot>,
    ) -> Result<IndexResolution<'a>> {
        if initial_snapshot.is_none() {
            let flight = Instant::now();
            let mut created = false;
            let mut action = IndexAction::Reused;
            match coordination.lock.try_write() {
                Ok(_write) => {
                    // Re-check under the lock: a concurrent creator may have
                    // committed between the initial check and lock acquisition.
                    if self.store.snapshot(workspace).await?.is_none() {
                        // We are the creator; clear any stale flight error.
                        *coordination.flight_error.lock().await = None;
                        match self.index_locked(workspace.clone()).await {
                            Ok(_) => created = true,
                            Err(error) => {
                                *coordination.flight_error.lock().await =
                                    Some(format!("{error:#}"));
                                return Err(error);
                            }
                        }
                    } else {
                        action = IndexAction::WaitedForExistingJob;
                    }
                }
                Err(_) => {
                    // A creator or explicit index holds the write lock; wait.
                    let _write = coordination.lock.write().await;
                    if self.store.snapshot(workspace).await?.is_some() {
                        action = IndexAction::WaitedForExistingJob;
                    } else {
                        // The holder committed nothing. If an automatic flight
                        // failed, share its cause instead of starting a
                        // duplicate within the same failed flight.
                        let shared = coordination.flight_error.lock().await.clone();
                        if let Some(cause) = shared {
                            return Err(anyhow::anyhow!(
                                "indexing failed while waiting for the in-progress index job: {cause}"
                            ));
                        }
                        // The holder was not an automatic flight (e.g. an
                        // explicit index that failed); we now hold the write
                        // lock exclusively, so index ourselves.
                        *coordination.flight_error.lock().await = None;
                        match self.index_locked(workspace.clone()).await {
                            Ok(_) => created = true,
                            Err(error) => {
                                *coordination.flight_error.lock().await =
                                    Some(format!("{error:#}"));
                                return Err(error);
                            }
                        }
                    }
                }
            }
            if created {
                action = IndexAction::Created;
            }
            let wait_ms = ms(flight);
            let _read_guard = coordination.lock.read().await;
            let snapshot = self.store.snapshot(workspace).await?;
            Ok(IndexResolution {
                _read_guard,
                snapshot,
                action,
                wait_ms,
                warning: None,
            })
        } else {
            let flight = Instant::now();
            let identity_changed = initial_snapshot
                .as_ref()
                .is_some_and(|s| s.identity != self.config.embedding_identity());
            let (action, warning) = match self.config.index.freshness {
                IndexFreshness::OnSearch => {
                    self.refresh_stale_snapshot(workspace, coordination, identity_changed)
                        .await?
                }
                IndexFreshness::Manual | IndexFreshness::Watch => {
                    ensure!(
                        !identity_changed,
                        "embedding configuration changed; reindex workspace"
                    );
                    (IndexAction::Reused, None)
                }
            };
            let wait_ms = ms(flight);
            let _read_guard = coordination.lock.read().await;
            let snapshot = self.store.snapshot(workspace).await?;
            Ok(IndexResolution {
                _read_guard,
                snapshot,
                action,
                wait_ms,
                warning,
            })
        }
    }

    /// Maps ripgrep lexical matches onto the chunks that contain them,
    /// ranked by how many matches fall within each chunk.
    fn map_lexical_matches(
        all_chunks: &[Hit],
        matches: Vec<lexical::LexicalMatch>,
        candidate_count: usize,
    ) -> Vec<Hit> {
        let mut counts = HashMap::<usize, u32>::new();
        for matched in matches {
            let matched_path = matched.relative_path.replace('\\', "/").to_lowercase();
            if let Some((index, _)) = all_chunks.iter().enumerate().find(|(_, h)| {
                let relative = h.chunk.relative_file_path.to_lowercase();
                (matched_path == relative || matched_path.ends_with(&format!("/{relative}")))
                    && h.chunk.start_line <= matched.line
                    && h.chunk.end_line >= matched.line
            }) {
                *counts.entry(index).or_default() += 1;
            }
        }
        let mut ranked: Vec<_> = counts.into_iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        let mut lexical_hits = Vec::new();
        for (rank, (index, count)) in ranked.into_iter().take(candidate_count).enumerate() {
            let mut hit = all_chunks[index].clone();
            hit.lexical_rank = Some(rank + 1);
            hit.lexical_match_count = count;
            hit.retrieval_channels.push("lexical".into());
            lexical_hits.push(hit);
        }
        lexical_hits
    }

    /// Maps LSP `workspace/symbol` locations onto the same-language chunks
    /// that contain them, preserving first-seen order.
    fn map_lsp_locations(
        all_chunks: &[Hit],
        lsp_locations: Vec<crate::lsp::Location>,
        candidate_count: usize,
    ) -> Vec<Hit> {
        let mut indexes = Vec::new();
        for location in lsp_locations {
            let Some(relative) = location.relative_file_path else {
                continue;
            };
            if let Some((index, _)) = all_chunks.iter().enumerate().find(|(_, hit)| {
                location.language.as_deref() == Some(hit.chunk.language.as_str())
                    && hit.chunk.relative_file_path.eq_ignore_ascii_case(&relative)
                    && hit.chunk.start_line <= location.start_line
                    && hit.chunk.end_line >= location.start_line
            }) && !indexes.contains(&index)
            {
                indexes.push(index);
            }
        }
        let mut lsp_hits = Vec::new();
        for (rank, index) in indexes.into_iter().take(candidate_count).enumerate() {
            let mut hit = all_chunks[index].clone();
            hit.lsp_rank = Some(rank + 1);
            hit.retrieval_channels.push("lsp".into());
            lsp_hits.push(hit);
        }
        lsp_hits
    }

    /// Combines the three retrieval channels with Reciprocal Rank Fusion,
    /// keyed by file path, start line, and content hash so the same chunk
    /// reached through multiple channels contributes once with a summed
    /// score.
    fn fuse_hits(
        &self,
        semantic: Vec<Hit>,
        lexical_hits: Vec<Hit>,
        lsp_hits: Vec<Hit>,
    ) -> Vec<Hit> {
        let rrf_k = self.config.rrf_k;
        let mut fused = HashMap::<(String, u32, String), Hit>::new();
        for mut hit in semantic {
            let rank = hit.semantic_rank.unwrap();
            hit.fusion_score += 1.0 / (rrf_k + rank as f32);
            fused.insert(
                (
                    hit.chunk.relative_file_path.clone(),
                    hit.chunk.start_line,
                    hit.chunk.content_hash.clone(),
                ),
                hit,
            );
        }
        for hit in lexical_hits {
            let key = (
                hit.chunk.relative_file_path.clone(),
                hit.chunk.start_line,
                hit.chunk.content_hash.clone(),
            );
            let lexical_rank = hit.lexical_rank.unwrap();
            let contribution = 1.0 / (rrf_k + lexical_rank as f32);
            if let Some(existing) = fused.get_mut(&key) {
                existing.lexical_rank = hit.lexical_rank;
                existing.lexical_match_count = hit.lexical_match_count;
                existing.fusion_score += contribution;
                existing.retrieval_channels.push("lexical".into());
            } else {
                let mut hit = hit;
                hit.fusion_score = contribution;
                fused.insert(key, hit);
            }
        }
        for hit in lsp_hits {
            let key = (
                hit.chunk.relative_file_path.clone(),
                hit.chunk.start_line,
                hit.chunk.content_hash.clone(),
            );
            let lsp_rank = hit.lsp_rank.unwrap();
            let contribution = 1.0 / (rrf_k + lsp_rank as f32);
            if let Some(existing) = fused.get_mut(&key) {
                existing.lsp_rank = hit.lsp_rank;
                existing.fusion_score += contribution;
                existing.retrieval_channels.push("lsp".into());
            } else {
                let mut hit = hit;
                hit.fusion_score = contribution;
                fused.insert(key, hit);
            }
        }
        for hit in fused.values_mut() {
            hit.fusion_score += hit.source_role_prior;
        }
        fused.into_values().collect()
    }

    /// Reranks fused candidates and writes the outcome (reranked results, or
    /// a fail-open fallback to fusion-ranked results with a warning) into
    /// `report`.
    async fn rerank_into_report(
        &self,
        query: &str,
        hits: Vec<Hit>,
        k: usize,
        report: &mut SearchReport,
    ) {
        let documents: Vec<_> = hits
            .iter()
            .map(|h| format!("{}\n{}", h.chunk.relative_file_path, h.chunk.code))
            .collect();
        match self.models.rerank(query, &documents).await {
            Ok(ranks) => {
                report.reranked = true;
                report.results = ranks
                    .into_iter()
                    .take(k)
                    .map(|(i, score)| {
                        let mut h = hits[i].clone();
                        h.reranker_score = Some(score);
                        h
                    })
                    .collect();
            }
            Err(error) => {
                tracing::warn!(%error, "reranker failed; returning semantic results");
                report.warning = Some(format!(
                    "Reranker unavailable or invalid response; semantic results returned: {error:#}"
                ));
                report.results = hits.into_iter().take(k).collect();
            }
        }
    }
}
