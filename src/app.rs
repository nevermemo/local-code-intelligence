use crate::{
    chunk,
    config::Config,
    language, lexical, lsp,
    manifest::{self, Manifest},
    models::Models,
    store::{Hit, Store},
    workspace::Workspace,
};
use anyhow::{Context, Result, anyhow, ensure};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Instant,
};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

/// Per-workspace coordination: the existing read/write lock plus the minimum
/// state needed to share the outcome of an automatic first-index flight.
#[derive(Default)]
struct WorkspaceCoordination {
    lock: RwLock<()>,
    /// Formatted cause of the most recent failed automatic first-index flight.
    /// Consulted only by a caller that holds the write lock and finds no
    /// committed snapshot; cleared when a new flight starts.
    flight_error: Mutex<Option<String>>,
}

pub struct App {
    pub config: Config,
    store: Store,
    models: Models,
    workspace_locks: Mutex<HashMap<String, Arc<WorkspaceCoordination>>>,
    // Bound concurrent model traffic from different indexing clients.
    index_lock: Mutex<()>,
    watchers: Mutex<HashMap<String, CancellationToken>>,
    analyzer: lsp::Manager,
}

#[derive(Debug, Serialize)]
pub struct IndexReport {
    pub workspace: Workspace,
    pub files: usize,
    pub chunks: usize,
    pub embedded_chunks: usize,
    pub reused_chunks: usize,
    pub parsed_files: usize,
    pub unchanged_files: usize,
    pub removed_files: usize,
    pub total_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub workspace: Workspace,
    pub indexed: bool,
    pub indexing: bool,
    pub chunks: usize,
    pub embedding_dimension: Option<usize>,
    pub compatible: bool,
    pub stale: bool,
    pub watched: bool,
    pub analyzer_running: bool,
    pub indexed_at_unix_seconds: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct WatchReport {
    pub workspace: Workspace,
    pub watched: bool,
    pub already_in_requested_state: bool,
}

#[derive(Debug, Serialize)]
pub struct NavigationReport {
    pub workspace: Workspace,
    pub results: Vec<lsp::Location>,
}

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
#[serde(rename_all = "snake_case")]
pub enum IndexAction {
    Reused,
    Created,
    WaitedForExistingJob,
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
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadinessComponent {
    pub name: &'static str,
    pub ready: bool,
    pub required: bool,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ReadinessDegraded {
    pub name: &'static str,
    pub reason: String,
}

#[derive(Debug, Serialize)]
pub struct ServiceStatus {
    pub ready: bool,
    pub can_create_semantic_index: bool,
    pub components: Vec<ReadinessComponent>,
    pub degraded: Vec<ReadinessDegraded>,
}

/// Outcome of a model-listing probe. `NotExposed` (HTTP 404/405) is neutral for
/// an optional component but a failure for a required one.
#[derive(Debug, Clone)]
enum ModelListing {
    Listed,
    NotExposed,
    Failed(String),
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// Per-process monotonic sequence for readiness probe filenames. Combined with
/// the PID and a wall-clock timestamp, concurrent probes stay unique.
static READINESS_PROBE_SEQUENCE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

impl App {
    pub async fn open(mut config: Config) -> Result<Self> {
        config.validate()?;
        std::fs::create_dir_all(&config.data_dir)?;
        config.data_dir = dunce::canonicalize(&config.data_dir)?;
        let store = Store::open(&config.data_dir.join("lancedb")).await?;
        let models = Models::new(&config)?;
        let analyzer = lsp::Manager::new(&config);
        Ok(Self {
            config,
            store,
            models,
            workspace_locks: Mutex::new(HashMap::new()),
            index_lock: Mutex::new(()),
            watchers: Mutex::new(HashMap::new()),
            analyzer,
        })
    }
    async fn lock(&self, id: &str) -> Arc<WorkspaceCoordination> {
        self.workspace_locks
            .lock()
            .await
            .entry(id.into())
            .or_default()
            .clone()
    }
    pub async fn index(&self, path: &Path) -> Result<IndexReport> {
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let coordination = self.lock(&workspace.id).await;
        let _write = coordination.lock.write().await;
        self.index_locked(workspace).await
    }

    /// Runs the indexing pipeline for `workspace`, assuming the caller already
    /// holds the workspace write lock. Explicit indexing and automatic
    /// first-search indexing share this implementation so both coordinate
    /// through the same locks.
    async fn index_locked(&self, workspace: Workspace) -> Result<IndexReport> {
        let start = Instant::now();
        let _index = self.index_lock.lock().await;
        let prior_manifest = manifest::load(&self.config.data_dir, &workspace);
        let root = workspace.path.clone();
        let prior_files = prior_manifest.files;
        let scan =
            tokio::task::spawn_blocking(move || chunk::scan_incremental(&root, &prior_files))
                .await??;
        let chunks = scan.chunks;
        let previous = self.store.snapshot(&workspace).await?;
        let identity = self.config.embedding_identity();
        let mut cache = match &previous {
            Some(s) if s.identity == identity => Store::cached_vectors(s).await?,
            _ => HashMap::new(),
        };
        let reused_chunks = chunks
            .iter()
            .filter(|c| cache.contains_key(&c.content_hash))
            .count();
        let mut seen = HashSet::new();
        let missing: Vec<_> = chunks
            .iter()
            .filter(|c| !cache.contains_key(&c.content_hash) && seen.insert(c.content_hash.clone()))
            .collect();
        let embedded_chunks = missing.len();
        for (i, batch) in missing.chunks(self.config.embedding_batch_size).enumerate() {
            let texts: Vec<_> = batch.iter().map(|c| c.code.clone()).collect();
            let vectors = self.models.embed(&texts).await?;
            for (c, vector) in batch.iter().zip(vectors) {
                cache.insert(c.content_hash.clone(), vector);
            }
            tracing::info!(workspace_id = %workspace.id, completed = ((i+1)*self.config.embedding_batch_size).min(missing.len()), total = missing.len(), "embedding chunks");
        }
        let vectors: Vec<_> = chunks
            .iter()
            .map(|c| cache[&c.content_hash].clone())
            .collect();
        let dimension = vectors
            .first()
            .map(Vec::len)
            .or_else(|| previous.as_ref().map(|s| s.dimension))
            .unwrap_or(1);
        self.store
            .replace(&workspace, &chunks, &vectors, &identity, dimension)
            .await?;
        let files = scan.files.len();
        let next_manifest = Manifest {
            version: manifest::VERSION,
            workspace_id: workspace.id.clone(),
            workspace_path: workspace.path.clone(),
            fingerprint: scan.fingerprint,
            files: scan.files,
        };
        manifest::save(&self.config.data_dir, &next_manifest)?;
        Ok(IndexReport {
            workspace,
            files,
            chunks: chunks.len(),
            embedded_chunks,
            reused_chunks,
            parsed_files: scan.parsed_files,
            unchanged_files: scan.unchanged_files,
            removed_files: scan.removed_files,
            total_ms: ms(start),
        })
    }
    pub async fn status(&self, path: &Path) -> Result<Status> {
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let coordination = self.lock(&workspace.id).await;
        let indexing = coordination.lock.try_read().is_err();
        let snapshot = self.store.snapshot(&workspace).await?;
        let chunks = match &snapshot {
            Some(s) => s.table.count_rows(None).await?,
            None => 0,
        };
        let stored_manifest = manifest::load(&self.config.data_dir, &workspace);
        let root = workspace.path.clone();
        let current_fingerprint =
            tokio::task::spawn_blocking(move || chunk::fingerprint(&root)).await??;
        let stale = snapshot.is_some()
            && (stored_manifest.fingerprint.is_empty()
                || stored_manifest.fingerprint != current_fingerprint);
        let watched = self.watchers.lock().await.contains_key(&workspace.id);
        let analyzer_running = self.analyzer.running(&workspace.id).await;
        Ok(Status {
            workspace,
            indexed: snapshot.is_some(),
            indexing,
            chunks,
            embedding_dimension: snapshot
                .as_ref()
                .filter(|_| chunks > 0)
                .map(|s| s.dimension),
            compatible: snapshot
                .as_ref()
                .is_some_and(|s| s.identity == self.config.embedding_identity()),
            stale,
            watched,
            analyzer_running,
            indexed_at_unix_seconds: snapshot.map(|s| s.indexed_at),
        })
    }

    pub async fn watch(self: &Arc<Self>, path: &Path) -> Result<WatchReport> {
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        ensure!(
            self.store.snapshot(&workspace).await?.is_some(),
            "workspace is not indexed; call index_workspace first"
        );
        let mut watchers = self.watchers.lock().await;
        if watchers.contains_key(&workspace.id) {
            return Ok(WatchReport {
                workspace,
                watched: true,
                already_in_requested_state: true,
            });
        }
        let cancellation = CancellationToken::new();
        watchers.insert(workspace.id.clone(), cancellation.clone());
        drop(watchers);

        let app = Arc::clone(self);
        let watched_workspace = workspace.clone();
        tokio::spawn(async move {
            let poll = std::time::Duration::from_millis(app.config.watch_poll_milliseconds);
            let debounce = std::time::Duration::from_millis(app.config.watch_debounce_milliseconds);
            let mut interval = tokio::time::interval(poll);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let root = watched_workspace.path.clone();
                let current = match tokio::task::spawn_blocking(move || chunk::fingerprint(&root))
                    .await
                {
                    Ok(Ok(value)) => value,
                    Ok(Err(error)) => {
                        tracing::warn!(workspace_id = %watched_workspace.id, error = %error, "watch scan failed");
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!(workspace_id = %watched_workspace.id, error = %error, "watch task failed");
                        continue;
                    }
                };
                let persisted = manifest::load(&app.config.data_dir, &watched_workspace);
                if persisted.fingerprint == current {
                    continue;
                }
                tokio::select! {
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(debounce) => {}
                }
                if let Err(error) = app.index(&watched_workspace.path).await {
                    tracing::warn!(workspace_id = %watched_workspace.id, error = %format!("{error:#}"), "automatic reindex failed; previous index retained");
                }
            }
        });
        Ok(WatchReport {
            workspace,
            watched: true,
            already_in_requested_state: false,
        })
    }

    pub async fn unwatch(&self, path: &Path) -> Result<WatchReport> {
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let cancellation = self.watchers.lock().await.remove(&workspace.id);
        let already_in_requested_state = cancellation.is_none();
        if let Some(token) = cancellation {
            token.cancel();
        }
        Ok(WatchReport {
            workspace,
            watched: false,
            already_in_requested_state,
        })
    }

    pub async fn symbols(&self, path: &Path, query: &str) -> Result<NavigationReport> {
        ensure!(!query.trim().is_empty(), "query must not be empty");
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let results = self.analyzer.symbols(&workspace, query).await?;
        Ok(NavigationReport { workspace, results })
    }

    fn source_path(workspace: &Workspace, relative_file_path: &str) -> Result<std::path::PathBuf> {
        ensure!(
            !relative_file_path.trim().is_empty(),
            "relative_file_path must not be empty"
        );
        let file = dunce::canonicalize(workspace.path.join(relative_file_path))?;
        ensure!(
            file.starts_with(&workspace.path),
            "source file must be inside the workspace"
        );
        ensure!(file.is_file(), "source path must be a file");
        Ok(file)
    }

    fn ensure_rust_navigation(file: &Path) -> Result<()> {
        match language::for_path(file) {
            Some(adapter) if adapter.identifier() == "rust" => Ok(()),
            Some(adapter) => anyhow::bail!(
                "rust-analyzer navigation does not support {} source files",
                adapter.identifier()
            ),
            None => anyhow::bail!("rust-analyzer navigation requires a Rust .rs source path"),
        }
    }

    pub async fn definition(
        &self,
        path: &Path,
        relative_file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<NavigationReport> {
        ensure!(line > 0, "line must be one-based and positive");
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let file = Self::source_path(&workspace, relative_file_path)?;
        Self::ensure_rust_navigation(&file)?;
        let results = self
            .analyzer
            .definition(&workspace, &file, line - 1, character)
            .await?;
        Ok(NavigationReport { workspace, results })
    }

    pub async fn references(
        &self,
        path: &Path,
        relative_file_path: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<NavigationReport> {
        ensure!(line > 0, "line must be one-based and positive");
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let file = Self::source_path(&workspace, relative_file_path)?;
        Self::ensure_rust_navigation(&file)?;
        let results = self
            .analyzer
            .references(&workspace, &file, line - 1, character, include_declaration)
            .await?;
        Ok(NavigationReport { workspace, results })
    }

    pub async fn search(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
    ) -> Result<SearchReport> {
        let total = Instant::now();
        ensure!(!query.trim().is_empty(), "query must not be empty");
        ensure!(query.len() <= 16000, "query exceeds 16000 bytes");
        let k = top_k.unwrap_or(self.config.default_top_k);
        ensure!((1..=40).contains(&k), "top_k must be 1..=40");
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let coordination = self.lock(&workspace.id).await;
        let initial_snapshot = self.store.snapshot(&workspace).await?;
        let _read_guard;
        let snapshot;
        let index_action;
        let index_wait_ms;
        if initial_snapshot.is_none() {
            let flight = Instant::now();
            let mut created = false;
            let mut action = IndexAction::Reused;
            match coordination.lock.try_write() {
                Ok(_write) => {
                    // Re-check under the lock: a concurrent creator may have
                    // committed between the initial check and lock acquisition.
                    if self.store.snapshot(&workspace).await?.is_none() {
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
                    if self.store.snapshot(&workspace).await?.is_some() {
                        action = IndexAction::WaitedForExistingJob;
                    } else {
                        // The holder committed nothing. If an automatic flight
                        // failed, share its cause instead of starting a
                        // duplicate within the same failed flight.
                        let shared = coordination.flight_error.lock().await.clone();
                        if let Some(cause) = shared {
                            return Err(anyhow!(
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
            index_wait_ms = ms(flight);
            index_action = action;
            _read_guard = coordination.lock.read().await;
            snapshot = self.store.snapshot(&workspace).await?;
        } else {
            _read_guard = coordination.lock.read().await;
            snapshot = self.store.snapshot(&workspace).await?;
            index_action = IndexAction::Reused;
            index_wait_ms = 0.0;
        }
        let snapshot = snapshot.context("workspace is not indexed; call index_workspace first")?;
        ensure!(
            snapshot.identity == self.config.embedding_identity(),
            "embedding configuration changed; reindex workspace"
        );
        let mut report = SearchReport {
            workspace,
            query: query.into(),
            candidate_count: 0,
            reranked: false,
            warning: None,
            timings: Timings::default(),
            results: vec![],
            index: SearchIndexLifecycle {
                action: index_action,
                wait_ms: index_wait_ms,
            },
        };
        if snapshot.table.count_rows(None).await? == 0 {
            report.timings.total_ms = ms(total);
            return Ok(report);
        }
        let all_chunks = Store::chunks(&snapshot, &report.workspace).await?;
        let has_rust = all_chunks.iter().any(|hit| hit.chunk.language == "rust");
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
                if !has_rust {
                    return (Ok(Vec::new()), ms(timer));
                }
                let mut found = Vec::new();
                let mut failure = None;
                let mut symbol_queries = Vec::new();
                for term in lexical::terms(query) {
                    if term.ends_with('s') && term.len() > 4 {
                        symbol_queries.push(format!("{}#", &term[..term.len() - 1]));
                    }
                    symbol_queries.push(format!("{term}#"));
                }
                symbol_queries.dedup();
                for term in symbol_queries.into_iter().take(8) {
                    match self.analyzer.symbols(&report.workspace, &term).await {
                        Ok(mut locations) => found.append(&mut locations),
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
                (failure.map_or(Ok(found), Err), ms(timer))
            },
        );
        let (query_vector, query_ms) = query_result;
        let (lexical_matches, lexical_ms) = lexical_result;
        let (lsp_locations, lsp_ms) = lsp_result;
        report.timings.query_embedding_ms = query_ms;
        report.timings.lexical_search_ms = lexical_ms;
        report.timings.lsp_search_ms = lsp_ms;
        let semantic_timer = Instant::now();
        let semantic = match query_vector {
            Ok(vector) => match Store::search(
                &snapshot,
                &report.workspace,
                &vector,
                self.config.semantic_candidate_count,
            )
            .await
            {
                Ok(hits) => hits,
                Err(error) => {
                    report.warning = Some(format!("Semantic retrieval unavailable: {error:#}"));
                    vec![]
                }
            },
            Err(error) => {
                report.warning = Some(format!(
                    "Query embedding unavailable; lexical results used: {error:#}"
                ));
                vec![]
            }
        };
        report.timings.lancedb_search_ms = ms(semantic_timer);
        let mut lexical_hits = Vec::new();
        match lexical_matches {
            Ok(matches) => {
                let mut counts = HashMap::<usize, u32>::new();
                for matched in matches {
                    let matched_path = matched.relative_path.replace('\\', "/").to_lowercase();
                    if let Some((index, _)) = all_chunks.iter().enumerate().find(|(_, h)| {
                        let relative = h.chunk.relative_file_path.to_lowercase();
                        (matched_path == relative
                            || matched_path.ends_with(&format!("/{relative}")))
                            && h.chunk.start_line <= matched.line
                            && h.chunk.end_line >= matched.line
                    }) {
                        *counts.entry(index).or_default() += 1;
                    }
                }
                let mut ranked: Vec<_> = counts.into_iter().collect();
                ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
                for (rank, (index, count)) in ranked
                    .into_iter()
                    .take(self.config.lexical_candidate_count)
                    .enumerate()
                {
                    let mut hit = all_chunks[index].clone();
                    hit.lexical_rank = Some(rank + 1);
                    hit.lexical_match_count = count;
                    hit.retrieval_channels.push("lexical".into());
                    lexical_hits.push(hit);
                }
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
        let mut lsp_hits = Vec::new();
        match lsp_locations {
            Ok(locations) => {
                let mut indexes = Vec::new();
                for location in locations {
                    let Some(relative) = location.relative_file_path else {
                        continue;
                    };
                    if let Some((index, _)) = all_chunks.iter().enumerate().find(|(_, hit)| {
                        hit.chunk.language == "rust"
                            && hit.chunk.relative_file_path.eq_ignore_ascii_case(&relative)
                            && hit.chunk.start_line <= location.start_line
                            && hit.chunk.end_line >= location.start_line
                    }) && !indexes.contains(&index)
                    {
                        indexes.push(index);
                    }
                }
                for (rank, index) in indexes
                    .into_iter()
                    .take(self.config.lsp_candidate_count)
                    .enumerate()
                {
                    let mut hit = all_chunks[index].clone();
                    hit.lsp_rank = Some(rank + 1);
                    hit.retrieval_channels.push("lsp".into());
                    lsp_hits.push(hit);
                }
            }
            Err(error) => {
                let message = format!("LSP retrieval unavailable: {error:#}");
                report.warning = Some(
                    report
                        .warning
                        .take()
                        .map_or(message.clone(), |w| format!("{w}; {message}")),
                );
            }
        }
        ensure!(
            !semantic.is_empty() || !lexical_hits.is_empty() || !lsp_hits.is_empty(),
            "semantic, lexical, and LSP retrieval all failed or returned no candidates"
        );
        let fusion_timer = Instant::now();
        let mut fused = HashMap::<(String, u32, String), Hit>::new();
        for mut hit in semantic {
            let rank = hit.semantic_rank.unwrap();
            hit.fusion_score += 1.0 / (self.config.rrf_k + rank as f32);
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
            let contribution = 1.0 / (self.config.rrf_k + lexical_rank as f32);
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
            let contribution = 1.0 / (self.config.rrf_k + lsp_rank as f32);
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
        let mut hits: Vec<_> = fused.into_values().collect();
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
            report.timings.reranking_ms = ms(timer);
        }
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

    /// Builds the shared, serializable readiness report. Each named component
    /// is probed independently and concurrently; required components gate the
    /// overall `ready` / `can_create_semantic_index` outcome while optional
    /// components only surface as degraded.
    pub async fn service_status(&self) -> ServiceStatus {
        let timeout = std::time::Duration::from_secs(self.config.readiness_timeout_seconds);
        let data_dir = self.config.data_dir.clone();
        let lancedb_path = self.config.data_dir.join("lancedb");
        let embedding_url = self.config.embedding_url.clone();
        let embedding_model = self.config.embedding_model.clone();
        let reranker_url = self.config.reranker_url.clone();
        let reranker_model = self.config.reranker_model.clone();
        let ripgrep_resolved = lexical::resolve_ripgrep_path(&self.config.ripgrep_path);
        let rust_analyzer_path = self.config.rust_analyzer_path.clone();
        let client = self.models.client();

        let (
            data_dir_probe,
            lancedb_probe,
            embedding_reach,
            embedding_models,
            reranker_reach,
            reranker_models,
            ripgrep_probe,
            rust_analyzer_probe,
        ) = tokio::join!(
            Self::probe_data_dir(&data_dir),
            self.store.accessibility_probe(&lancedb_path),
            Self::probe_endpoint_reach(client, &embedding_url, "embedding endpoint", timeout),
            Self::probe_model_listing(
                client,
                &embedding_url,
                &embedding_model,
                "embedding model listing",
                timeout
            ),
            Self::probe_endpoint_reach(client, &reranker_url, "reranker endpoint", timeout),
            Self::probe_model_listing(
                client,
                &reranker_url,
                &reranker_model,
                "reranker model listing",
                timeout
            ),
            Self::probe_subprocess_version(&ripgrep_resolved, timeout),
            Self::probe_subprocess_version(std::path::Path::new(&rust_analyzer_path), timeout),
        );

        let data_dir_ready = data_dir_probe.is_ok();
        let lancedb_ready = lancedb_probe.is_ok();
        let embedding_reach_ready = embedding_reach.is_ok();
        let reranker_reach_ready = reranker_reach.is_ok();
        let ripgrep_ready = ripgrep_probe.is_ok();
        let rust_analyzer_ready = rust_analyzer_probe.is_ok();

        // Required embedding listing: any non-Listed outcome fails readiness.
        let (embedding_models_ready, embedding_models_detail) = match &embedding_models {
            ModelListing::Listed => (true, None),
            ModelListing::NotExposed => (
                false,
                Some("model listing not exposed (HTTP 404/405)".into()),
            ),
            ModelListing::Failed(reason) => (false, Some(reason.clone())),
        };
        // Optional reranker listing: NotExposed is neutral (ready, no degrade);
        // only a Failed outcome degrades (handled by the generic loop below).
        let (reranker_models_ready, reranker_models_detail) = match &reranker_models {
            ModelListing::Listed => (true, None),
            ModelListing::NotExposed => (
                true,
                Some("model listing not exposed (HTTP 404/405)".into()),
            ),
            ModelListing::Failed(reason) => (false, Some(reason.clone())),
        };

        let components = vec![
            ReadinessComponent {
                name: "writable_data_dir",
                ready: data_dir_ready,
                required: true,
                detail: data_dir_probe.err().map(|e| format!("{e:#}")),
            },
            ReadinessComponent {
                name: "lancedb_accessible",
                ready: lancedb_ready,
                required: true,
                detail: lancedb_probe.err().map(|e| format!("{e:#}")),
            },
            ReadinessComponent {
                name: "embedding_endpoint_reachable",
                ready: embedding_reach_ready,
                required: true,
                detail: embedding_reach.err().map(|e| format!("{e:#}")),
            },
            ReadinessComponent {
                name: "embedding_model_listed",
                ready: embedding_models_ready,
                required: true,
                detail: embedding_models_detail,
            },
            ReadinessComponent {
                name: "reranker_endpoint_reachable",
                ready: reranker_reach_ready,
                required: false,
                detail: reranker_reach.err().map(|e| format!("{e:#}")),
            },
            ReadinessComponent {
                name: "reranker_model_listed",
                ready: reranker_models_ready,
                required: false,
                detail: reranker_models_detail,
            },
            ReadinessComponent {
                name: "ripgrep_available",
                ready: ripgrep_ready,
                required: false,
                detail: Some(match ripgrep_probe {
                    Ok(()) => format!("resolved ripgrep at {}", ripgrep_resolved.display()),
                    Err(e) => format!(
                        "ripgrep at {} unavailable: {e:#}",
                        ripgrep_resolved.display()
                    ),
                }),
            },
            ReadinessComponent {
                name: "rust_analyzer_available",
                ready: rust_analyzer_ready,
                required: false,
                detail: Some(match rust_analyzer_probe {
                    Ok(()) => format!("configured rust-analyzer command {}", rust_analyzer_path),
                    Err(e) => format!(
                        "configured rust-analyzer command {} unavailable: {e:#}",
                        rust_analyzer_path
                    ),
                }),
            },
        ];

        // The report requests overall degraded components/reasons, so every
        // unavailable component is listed with its reason, required or not.
        let mut degraded = Vec::new();
        for c in &components {
            if !c.ready {
                degraded.push(ReadinessDegraded {
                    name: c.name,
                    reason: c.detail.clone().unwrap_or_else(|| "unavailable".into()),
                });
            }
        }

        let ready =
            data_dir_ready && lancedb_ready && embedding_reach_ready && embedding_models_ready;
        let can_create_semantic_index = ready;

        ServiceStatus {
            ready,
            can_create_semantic_index,
            components,
            degraded,
        }
    }

    async fn probe_data_dir(data_dir: &Path) -> Result<()> {
        let data_dir = data_dir.to_path_buf();
        tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&data_dir)?;
            // Unique per process, call, and instant so concurrent /ready and
            // service_status probes cannot collide on the same filename.
            let sequence =
                READINESS_PROBE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let probe = data_dir.join(format!(
                ".readiness_probe_{}_{}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
                sequence
            ));
            let result = (|| -> std::io::Result<()> {
                std::fs::write(&probe, b"probe")?;
                std::fs::metadata(&probe)?;
                Ok(())
            })();
            // Always attempt cleanup, even when the write or metadata
            // verification above failed, so a failed probe leaves no residue.
            let _ = std::fs::remove_file(&probe);
            result.with_context(|| format!("write readiness probe in {}", data_dir.display()))
        })
        .await?
    }

    /// Classifies a reqwest transport error by class only. reqwest's `Display`
    /// embeds the request URL, which may contain credentials or query secrets,
    /// so readiness reports must never echo the raw error text.
    fn reqwest_error_class(error: &reqwest::Error) -> &'static str {
        if error.is_timeout() {
            "timeout"
        } else if error.is_connect() {
            "connection failure"
        } else {
            "request failure"
        }
    }

    /// Contacts the configured base URL. Any HTTP response (including 404/405)
    /// proves the endpoint is reachable; only a connection failure or timeout
    /// marks it unreachable. Errors identify the dependency generically via
    /// `label` because configured URLs may contain credentials or query
    /// secrets that must not be echoed into readiness reports.
    async fn probe_endpoint_reach(
        client: &reqwest::Client,
        url: &str,
        label: &str,
        timeout: std::time::Duration,
    ) -> Result<()> {
        let target = url.trim_end_matches('/');
        client
            .get(target)
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| {
                anyhow!("connect to {label}: {}", Self::reqwest_error_class(&error))
            })?;
        Ok(())
    }

    /// Derives the likely OpenAI-compatible model listing endpoint from the
    /// configured URL origin and checks that the configured model is present.
    /// A 404/405 means the listing is not exposed. Errors identify the
    /// dependency generically via `label` (with HTTP status where relevant)
    /// because configured URLs may contain credentials or query secrets that
    /// must not be echoed into readiness reports.
    async fn probe_model_listing(
        client: &reqwest::Client,
        url: &str,
        model: &str,
        label: &str,
        timeout: std::time::Duration,
    ) -> ModelListing {
        let parsed = match reqwest::Url::parse(url) {
            Ok(u) => u,
            Err(e) => return ModelListing::Failed(format!("invalid {label} URL: {e}")),
        };
        let mut models_url = parsed;
        models_url.set_path("/v1/models");
        models_url.set_query(None);
        models_url.set_fragment(None);
        let response = match client.get(models_url).timeout(timeout).send().await {
            Ok(r) => r,
            Err(e) => {
                return ModelListing::Failed(format!(
                    "connect to {label}: {}",
                    Self::reqwest_error_class(&e)
                ));
            }
        };
        let status = response.status();
        if status.as_u16() == 404 || status.as_u16() == 405 {
            return ModelListing::NotExposed;
        }
        if !status.is_success() {
            return ModelListing::Failed(format!("{label} HTTP error: {status}"));
        }
        let body: serde_json::Value = match response.json().await {
            Ok(b) => b,
            Err(e) => return ModelListing::Failed(format!("invalid {label} response: {e:#}")),
        };
        let ids = body
            .get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if ids.iter().any(|id| id == model) {
            ModelListing::Listed
        } else {
            ModelListing::Failed(format!("configured model {model} not present in {label}"))
        }
    }

    async fn probe_subprocess_version(
        resolved: &std::path::Path,
        timeout: std::time::Duration,
    ) -> Result<()> {
        let mut command = tokio::process::Command::new(resolved);
        command.arg("--version");
        command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let output = tokio::time::timeout(timeout, command.output())
            .await
            .with_context(|| format!("timed out running {} --version", resolved.display()))?
            .with_context(|| format!("start {}", resolved.display()))?;
        ensure!(
            output.status.success(),
            "subprocess {} --version failed",
            resolved.display()
        );
        Ok(())
    }
}
