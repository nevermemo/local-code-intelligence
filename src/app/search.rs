//! Hybrid retrieval: index freshness, semantic/lexical/LSP fan-out,
//! Reciprocal Rank Fusion, and reranking. Each channel and the reranker can
//! be switched off for a search with the `[channels]` configuration.

use super::{App, IndexAction, WorkspaceCoordination, ms};
use crate::{
    config::IndexFreshness,
    filter::{EffectiveFilter, EffectiveFilterReport, FilterRequest},
    lexical,
    lsp::adapter::LspAdapter,
    store::{Hit, Snapshot, Store},
    workspace::Workspace,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet, hash_map::Entry},
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
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
    /// A channel failed or the search fell back (see the retrieval
    /// contracts); counted as a fallback by the evaluation harness.
    pub warning: Option<String>,
    /// Informational remarks about work skipped on purpose where nothing
    /// failed, for example a language server that is still starting and was
    /// not waited for (`lsp_search_budget_ms`). Absent from the JSON when
    /// empty, and never counted as a fallback.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub timings: Timings,
    pub results: Vec<Hit>,
    pub index: SearchIndexLifecycle,
    pub filters: EffectiveFilterReport,
}

/// The longest prefix of `text` that is at most `max_bytes` long and ends on
/// a UTF-8 character boundary.
fn head_within(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn hit_matches(filters: &EffectiveFilter, hit: &Hit) -> bool {
    filters.matches(
        &hit.chunk.language,
        &hit.chunk.relative_file_path,
        hit.source_role,
    )
}

/// What the language-server channel of one search produced.
#[derive(Default)]
struct LspChannel {
    locations: Vec<crate::lsp::Location>,
    /// One `provider: cause` entry for each provider that failed.
    failures: Vec<String>,
    /// Providers that did not answer within `lsp_search_budget_ms`.
    over_budget: Vec<&'static str>,
    elapsed_ms: f64,
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
            notes: Vec::new(),
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
        // `[channels]` switches. A deliberately disabled channel is skipped
        // outright: no work, a zero timing, and no "unavailable" warning.
        let channels = self.config.channels;
        let mut applicable_adapters: HashMap<&'static str, Arc<dyn LspAdapter>> = HashMap::new();
        if channels.lsp {
            let indexed_languages: HashSet<&str> = all_chunks
                .iter()
                .map(|hit| hit.chunk.language.as_str())
                .collect();
            for language in indexed_languages {
                for adapter in self.analyzer.adapters_for_language(language) {
                    if adapter.enabled() {
                        applicable_adapters
                            .entry(adapter.provider())
                            .or_insert_with(|| Arc::clone(adapter));
                    }
                }
            }
        }
        let lsp_budget = Duration::from_millis(self.config.lsp_search_budget_ms);
        let (query_result, lexical_result, lsp_result) = tokio::join!(
            async {
                if !channels.semantic {
                    return None;
                }
                let timer = Instant::now();
                Some((self.models.query(query).await, ms(timer)))
            },
            async {
                if !channels.lexical {
                    return None;
                }
                let timer = Instant::now();
                Some((
                    lexical::search(&self.config.ripgrep_path, &report.workspace.path, query).await,
                    ms(timer),
                ))
            },
            async {
                if !channels.lsp {
                    return LspChannel::default();
                }
                let timer = Instant::now();
                if applicable_adapters.is_empty() {
                    return LspChannel {
                        elapsed_ms: ms(timer),
                        ..LspChannel::default()
                    };
                }
                let mut symbol_queries = Vec::new();
                for term in lexical::terms(query) {
                    if term.ends_with('s') && term.len() > 4 {
                        symbol_queries.push(format!("{}#", &term[..term.len() - 1]));
                    }
                    symbol_queries.push(format!("{term}#"));
                }
                symbol_queries.dedup();
                symbol_queries.truncate(8);
                // The providers run side by side, each under the same wall
                // clock budget, so one that is still starting cannot starve
                // the others. Past the budget the provider's future is
                // dropped, which only stops this search waiting: its session
                // lives in the manager and keeps starting and warming for the
                // next search. Answers gathered before the budget ran out are
                // kept.
                let mut providers: Vec<&Arc<dyn LspAdapter>> =
                    applicable_adapters.values().collect();
                providers.sort_by_key(|adapter| adapter.provider());
                let (workspace, queries) = (&report.workspace, &symbol_queries);
                let answers =
                    futures::future::join_all(providers.into_iter().map(|adapter| async move {
                        let gathered = std::sync::Mutex::new(Vec::new());
                        let answer = tokio::time::timeout(
                            lsp_budget,
                            self.provider_symbols(adapter, workspace, queries, &gathered),
                        )
                        .await;
                        let gathered = gathered.into_inner().unwrap_or_else(|p| p.into_inner());
                        (adapter.provider(), gathered, answer)
                    }))
                    .await;
                let mut channel = LspChannel::default();
                let mut per_provider = Vec::new();
                for (provider, gathered, answer) in answers {
                    per_provider.push(gathered);
                    match answer {
                        Ok(failure) => channel.failures.extend(failure),
                        Err(_) => {
                            tracing::debug!(
                                provider,
                                budget_ms = self.config.lsp_search_budget_ms,
                                "language server did not answer within the search budget"
                            );
                            channel.over_budget.push(provider);
                        }
                    }
                }
                // Merge query by query across providers, as one sequential
                // pass would: first-seen order decides the LSP rank.
                for index in 0..symbol_queries.len() {
                    for locations in &mut per_provider {
                        if let Some(found) = locations.get_mut(index) {
                            channel.locations.append(found);
                        }
                    }
                }
                channel.elapsed_ms = ms(timer);
                channel
            },
        );
        let lsp_locations = lsp_result.locations;
        let lsp_failures = lsp_result.failures;
        report.timings.lsp_search_ms = lsp_result.elapsed_ms;
        // The LSP channel "ran" when it was enabled, at least one provider
        // applied to the indexed languages, and not every provider failed. A
        // provider past its budget did not fail (its server is only still
        // starting), so a search with that channel alone is a clean miss with
        // a note, not an error.
        let mut lexical_or_lsp_ran = channels.lsp
            && !applicable_adapters.is_empty()
            && lsp_failures.len() < applicable_adapters.len();
        if !lsp_result.over_budget.is_empty() {
            // Not a warning: nothing failed, the server is just not ready
            // yet. A warning would also count the search as a fallback.
            report.notes.push(format!(
                "LSP still starting or slow; symbol results from {} are missing or incomplete \
                 for this search (lsp_search_budget_ms = {}). The server keeps starting in the \
                 background.",
                lsp_result.over_budget.join(", "),
                self.config.lsp_search_budget_ms
            ));
        }
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
        let semantic = match query_result {
            // Disabled: the query was never embedded and the index is not
            // searched; both timings stay zero.
            None => vec![],
            Some((query_vector, query_ms)) => {
                report.timings.query_embedding_ms = query_ms;
                let semantic_timer = Instant::now();
                let hits = match query_vector {
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
                        let message = format!(
                            "Query embedding unavailable; results from the other enabled channels used: {error:#}"
                        );
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
                hits
            }
        };
        let mut lexical_hits = Vec::new();
        if let Some((lexical_matches, lexical_ms)) = lexical_result {
            report.timings.lexical_search_ms = lexical_ms;
            match lexical_matches {
                Ok(lexical_search) => {
                    lexical_or_lsp_ran = true;
                    lexical_hits = Self::map_lexical_matches(
                        &all_chunks,
                        lexical_search,
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
        }
        let lsp_hits =
            Self::map_lsp_locations(&all_chunks, lsp_locations, self.config.lsp_candidate_count);
        if semantic.is_empty() && lexical_hits.is_empty() && lsp_hits.is_empty() {
            // With the semantic channel off, an enabled lexical or LSP
            // channel that ran without failing and matched nothing is an
            // ordinary answer (a literal or symbol search with no hit). If
            // every enabled channel failed (or none could run) that stays an
            // error. With the semantic channel on, an empty result means it
            // failed, because it nearly always returns candidates.
            if filtering_requested || (!channels.semantic && lexical_or_lsp_ran) {
                report.timings.total_ms = ms(total);
                return Ok(report);
            }
            let detail: Vec<&str> = report
                .warning
                .iter()
                .chain(report.notes.iter())
                .map(String::as_str)
                .collect();
            if detail.is_empty() {
                bail!("every enabled retrieval channel failed or returned no candidates");
            }
            bail!(
                "every enabled retrieval channel failed or returned no candidates ({})",
                detail.join("; ")
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
            if channels.reranker {
                let timer = Instant::now();
                self.rerank_into_report(query, hits, k, &mut report).await;
                report.timings.reranking_ms = ms(timer);
            } else {
                // Deliberately disabled: fusion order, `reranked = false`, no
                // request, no warning, and a zero reranking time.
                report.results = hits.into_iter().take(k).collect();
            }
        }
        report.results.retain(|hit| hit_matches(&filters, hit));
        report.timings.total_ms = ms(total);
        tracing::info!(
            query_embedding_ms = report.timings.query_embedding_ms,
            lancedb_search_ms = report.timings.lancedb_search_ms,
            lexical_search_ms = report.timings.lexical_search_ms,
            lsp_search_ms = report.timings.lsp_search_ms,
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

    /// Maps credited ripgrep lines onto the chunks that contain them and
    /// ranks the chunks by BM25 over IDF-weighted term evidence (see
    /// `lexical::score_chunks`), not by matching-line count. Document
    /// frequencies are measured over indexed files only: a term's `df` is the
    /// number of indexed files with a credited line, relative to the number of
    /// indexed files in `all_chunks`, so files ripgrep sees but the index
    /// does not cannot skew them. Chunk length is its line count, averaged
    /// over `all_chunks`. Chunks that match only terms too common to score
    /// are not candidates.
    ///
    /// A reported path selects its candidate chunks once (every chunk whose
    /// file path equals it, or is a `/`-delimited suffix of it, compared
    /// case-insensitively); each line then takes the first candidate, in
    /// `all_chunks` order, whose line range holds it.
    fn map_lexical_matches(
        all_chunks: &[Hit],
        search: lexical::LexicalSearch,
        candidate_count: usize,
    ) -> Vec<Hit> {
        struct IndexedFile {
            lowercase_path: String,
            /// `/` + `lowercase_path`, for suffix matches.
            path_suffix: String,
            chunks: Vec<usize>,
        }
        let mut file_ids = HashMap::<&str, usize>::new();
        let mut files: Vec<IndexedFile> = Vec::new();
        let mut chunk_files = Vec::with_capacity(all_chunks.len());
        for (index, hit) in all_chunks.iter().enumerate() {
            let path = hit.chunk.relative_file_path.as_str();
            let next = files.len();
            let id = *file_ids.entry(path).or_insert(next);
            if id == next {
                let lowercase_path = path.to_lowercase();
                files.push(IndexedFile {
                    path_suffix: format!("/{lowercase_path}"),
                    lowercase_path,
                    chunks: Vec::new(),
                });
            }
            files[id].chunks.push(index);
            chunk_files.push(id);
        }
        let total_files = files.len();
        let mut candidates = HashMap::<String, Vec<usize>>::new();
        let mut credited = Vec::with_capacity(search.matches.len());
        for matched in search.matches {
            let (line, term_mask) = (matched.line, matched.term_mask);
            let chunks = match candidates.entry(matched.relative_path) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let reported = entry.key().replace('\\', "/").to_lowercase();
                    let mut chunks: Vec<usize> = files
                        .iter()
                        .filter(|file| {
                            reported == file.lowercase_path || reported.ends_with(&file.path_suffix)
                        })
                        .flat_map(|file| file.chunks.iter().copied())
                        .collect();
                    chunks.sort_unstable();
                    entry.insert(chunks)
                }
            };
            if let Some(&index) = chunks.iter().find(|&&index| {
                let chunk = &all_chunks[index].chunk;
                chunk.start_line <= line && chunk.end_line >= line
            }) {
                credited.push(lexical::CreditedLine {
                    file: chunk_files[index],
                    chunk: index,
                    term_mask,
                });
            }
        }
        let chunk_lengths: Vec<u32> = all_chunks
            .iter()
            .map(|hit| hit.chunk.end_line.saturating_sub(hit.chunk.start_line) + 1)
            .collect();
        let mut ranked =
            lexical::score_chunks(search.terms.len(), total_files, &chunk_lengths, &credited);
        ranked.sort_by(|a, b| {
            let (a_chunk, b_chunk) = (&all_chunks[a.chunk].chunk, &all_chunks[b.chunk].chunk);
            b.score
                .total_cmp(&a.score)
                .then_with(|| a_chunk.relative_file_path.cmp(&b_chunk.relative_file_path))
                .then(a_chunk.start_line.cmp(&b_chunk.start_line))
        });
        let mut lexical_hits = Vec::new();
        for (rank, scored) in ranked.into_iter().take(candidate_count).enumerate() {
            let mut hit = all_chunks[scored.chunk].clone();
            hit.lexical_rank = Some(rank + 1);
            hit.lexical_score = Some(scored.score as f32);
            hit.lexical_match_count = scored.credited_lines;
            hit.retrieval_channels.push("lexical".into());
            lexical_hits.push(hit);
        }
        lexical_hits
    }

    /// One provider's `workspace/symbol` answers, one list per symbol query
    /// in query order, pushed onto `gathered` as they arrive so that the
    /// caller keeps them even when it drops this future at the search budget.
    /// A provider that failed (after its own fresh-process retry) is skipped
    /// for the remaining queries of this search, so a missing or broken
    /// server costs one failure, not one per symbol query; the answers
    /// gathered before the failure are kept. Returns that failure, if any.
    ///
    /// Cancel-safe: dropping this future must (and does) leave the
    /// provider's session untouched.
    async fn provider_symbols(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        symbol_queries: &[String],
        gathered: &std::sync::Mutex<Vec<Vec<crate::lsp::Location>>>,
    ) -> Option<String> {
        for symbol_query in symbol_queries {
            match self
                .analyzer
                .symbols(adapter, workspace, symbol_query)
                .await
            {
                Ok(locations) => gathered
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(locations),
                Err(error) => return Some(format!("{}: {error:#}", adapter.provider())),
            }
        }
        None
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
                existing.lexical_score = hit.lexical_score;
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
        let max_bytes = self.config.rerank_document_max_bytes;
        let documents: Vec<_> = hits
            .iter()
            .map(|h| {
                format!(
                    "{}\n{}",
                    h.chunk.relative_file_path,
                    head_within(&h.chunk.code, max_bytes)
                )
            })
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
                tracing::warn!(%error, "reranker failed; returning fusion-ranked results");
                // Append: earlier warnings (a refresh fallback, an
                // unavailable channel) must survive a reranker failure.
                let message = format!(
                    "Reranker unavailable or invalid response; fusion-ranked results returned: {error:#}"
                );
                report.warning = Some(
                    report
                        .warning
                        .take()
                        .map_or(message.clone(), |w| format!("{w}; {message}")),
                );
                report.results = hits.into_iter().take(k).collect();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::head_within;

    #[test]
    fn head_within_keeps_short_text_and_cuts_on_a_char_boundary() {
        assert_eq!(head_within("fn main() {}", 64), "fn main() {}");
        assert_eq!(head_within("abcdef", 3), "abc");
        // "é" is two bytes; a cut inside it backs off to the boundary.
        assert_eq!(head_within("caé", 3), "ca");
        assert_eq!(head_within("caé", 4), "caé");
    }
}
