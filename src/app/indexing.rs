//! Workspace indexing, staleness refresh, status, and file-watch lifecycle.

use super::{App, WorkspaceCoordination, ms};
use crate::{
    chunk,
    config::{LspRoots, fill_template},
    lsp::roots::{Root, RootReason},
    manifest::{self, Manifest},
    models::{self, Models},
    store::{Store, vector_key},
    workspace::Workspace,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::Arc,
    time::Instant,
};
use tokio_util::sync::CancellationToken;

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
    /// Files with a source extension left out of the index because their
    /// content is another format (a Qt Linguist `.ts` catalog is XML, not
    /// TypeScript). They are not counted in `files` and have no chunks.
    pub skipped_non_source_files: usize,
    /// Files that were not valid UTF-8 and were indexed from a lossy decoding.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lossy_decoded_files: Vec<String>,
    /// Chunks (`path:start_line`) the embedding service rejected as too long,
    /// whose vectors were computed from a shortened prefix instead. The full
    /// chunk is still stored, returned, and lexically searchable.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub truncated_for_embedding: Vec<String>,
    /// The embedding service was unreachable and the index was committed
    /// without vectors (placeholders no search serves); the semantic channel
    /// is unavailable until a later index run embeds the chunks.
    pub vectors_missing: bool,
    pub total_ms: f64,
}

/// Shortest prefix (in bytes) an oversized input is cut down to before
/// giving up: below this, a rejection can't plausibly be about length.
const MIN_EMBED_PREFIX_BYTES: usize = 512;

/// The text embedded for `chunk`: `template` (`embedding_document_template`)
/// with `{code}` replaced by the chunk's source and `{path}` by its
/// repository-relative file path. The default template `{code}` yields the
/// source unchanged. The oversized-input fallback below shortens this
/// formatted text from its end, so a prefix from the template survives.
pub(super) fn document_text(template: &str, chunk: &chunk::Chunk) -> String {
    fill_template(
        template,
        &[
            ("code", chunk.code.as_str()),
            ("path", chunk.relative_file_path.as_str()),
        ],
    )
}

/// Embeds one batch of document texts. When the service rejects the batch's
/// input itself (HTTP 400/413/422 -- typically one chunk longer than the
/// model's context, e.g. a large generated literal the chunker cannot
/// split), the batch is retried one input at a time, and each rejected
/// input is embedded from a progressively halved prefix. One oversized
/// chunk therefore costs a few extra requests instead of failing the whole
/// indexing pass. Returns the vectors plus the batch positions that had to
/// be shortened. Any other error -- or a rejection that persists down to
/// [`MIN_EMBED_PREFIX_BYTES`] -- is returned unchanged.
async fn embed_isolating_oversized(
    models: &Models,
    texts: &[String],
) -> Result<(Vec<Vec<f32>>, Vec<usize>)> {
    match models.embed(texts).await {
        Ok(vectors) => return Ok((vectors, Vec::new())),
        Err(error) if !models::is_input_rejected(&error) => return Err(error),
        Err(_) => {}
    }
    let mut vectors = Vec::with_capacity(texts.len());
    let mut shortened = Vec::new();
    for (position, text) in texts.iter().enumerate() {
        let mut input = text.as_str();
        loop {
            match models.embed(&[input.to_owned()]).await {
                Ok(mut vector) => {
                    vectors.push(vector.remove(0));
                    if input.len() < text.len() {
                        shortened.push(position);
                    }
                    break;
                }
                Err(error)
                    if models::is_input_rejected(&error)
                        && input.len() / 2 >= MIN_EMBED_PREFIX_BYTES =>
                {
                    let mut end = input.len() / 2;
                    while !input.is_char_boundary(end) {
                        end -= 1;
                    }
                    input = &input[..end];
                }
                Err(error) => return Err(error),
            }
        }
    }
    Ok((vectors, shortened))
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub workspace: Workspace,
    pub indexed: bool,
    pub indexing: bool,
    pub chunks: usize,
    pub embedding_dimension: Option<usize>,
    pub compatible: bool,
    /// The snapshot was committed without vectors because the embedding
    /// service was unreachable; semantic retrieval is unavailable on it.
    pub vectors_missing: bool,
    pub stale: bool,
    /// Why staleness could not be determined (e.g. an unreadable file);
    /// `stale` is then reported as `true` for an indexed workspace.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stale_check_error: Option<String>,
    pub watched: bool,
    /// Deprecated, kept for one release: `analyzers_running["rust-analyzer"]`.
    pub analyzer_running: bool,
    /// Deprecated, kept for one release: `analyzers_running["csharp-ls"]`.
    pub csharp_analyzer_running: bool,
    /// Deprecated, kept for one release:
    /// `analyzers_running["typescript-language-server"]`.
    pub typescript_analyzer_running: bool,
    /// Deprecated, kept for one release: `analyzers_running["pyright"]`.
    pub python_analyzer_running: bool,
    /// Deprecated, kept for one release: `analyzers_running["gopls"]`.
    pub go_analyzer_running: bool,
    /// Deprecated, kept for one release: `analyzers_running["jdtls"]`.
    pub java_analyzer_running: bool,
    /// Deprecated, kept for one release: `analyzers_running["clangd"]`.
    pub clangd_analyzer_running: bool,
    /// Whether a started, ready language server is running for this
    /// workspace, keyed by provider name (`rust-analyzer`, `csharp-ls`,
    /// `typescript-language-server`, `pyright`, `gopls`, `jdtls`, `clangd`, `marksman`).
    /// Derived from the language-server registry, so it has one entry for
    /// every registered server, `false` for one that is not running, disabled
    /// or unconfigured.
    pub analyzers_running: BTreeMap<String, bool>,
    /// What each language server's session reports about itself, keyed like
    /// `analyzers_running` (one entry for every registered server): `null`
    /// when this workspace has no session for it, else its `state`
    /// (`starting`, `ready`, `busy`, `failed` or `stuck`), whether it is
    /// `primed`, and counters and ages in milliseconds. A report only: it
    /// does not keep a session alive and changes no answer.
    pub analyzers: BTreeMap<String, Option<crate::lsp::ServerReadiness>>,
    /// One entry per registered language server, sorted by provider: what it
    /// can serve, which of those languages this index holds, whether it is
    /// configured and enabled, whether its binary has answered a version
    /// probe, and its session. A report built from the manager's memory:
    /// reading it runs no probe and starts no server.
    pub servers: Vec<ServerRoster>,
    /// The languages this index holds that no enabled, configured language
    /// server lists (sorted). Empty without a snapshot.
    pub languages_without_server: Vec<String>,
    pub indexed_at_unix_seconds: Option<String>,
}

/// One language server in [`Status::servers`].
#[derive(Debug, Serialize)]
pub struct ServerRoster {
    pub provider: String,
    /// Every Tree-sitter language the server can navigate.
    pub languages: Vec<&'static str>,
    /// The subset of `languages` the index holds (sorted).
    pub languages_in_index: Vec<String>,
    /// A command or installation path is configured.
    pub configured: bool,
    /// Configured and not explicitly disabled.
    pub enabled: bool,
    /// The configured command or installation path.
    pub command: Option<String>,
    /// `probed` once the manager's own version probe has passed (run by
    /// priming and the `lsp.live` warm start; `/ready` probes separately and
    /// does not count), else `not_probed`.
    pub probe: crate::lsp::ProbeState,
    /// Milliseconds left in the server's skip window under
    /// `[run_profile.lsp] breaker = true` (a failed probe or start opened it);
    /// `null` without the breaker or when the window is closed.
    pub breaker_open_ms: Option<u64>,
    /// The directory the server is started at, relative to the workspace
    /// (`.` for the workspace itself). Of a live session, the directory it was
    /// started at; without one, the directory a start would choose now under
    /// `[run_profile.lsp] roots`, worked out only for an enabled, configured
    /// server whose languages the index holds (else `.`).
    pub root: String,
    /// Why `root` is what it is: `roots_workspace` (the key says so),
    /// `manifest:<file>` (the directory of the topmost manifest),
    /// `no_manifest`, `several_roots:<n>` (sibling manifests),
    /// `files_outside_root:<n>`, `no_file_list` or `unreadable` (the last
    /// five start at the workspace).
    pub root_reason: String,
    /// The session's report, as in `analyzers`; `null` without a session.
    pub session: Option<crate::lsp::ServerReadiness>,
}

/// The languages of `in_index` that no enabled, configured adapter lists,
/// sorted.
fn languages_without_server(
    adapters: &[Arc<dyn crate::lsp::adapter::LspAdapter>],
    in_index: &HashSet<String>,
) -> Vec<String> {
    let mut languages: Vec<String> = in_index
        .iter()
        .filter(|language| {
            !adapters.iter().any(|adapter| {
                adapter.enabled()
                    && adapter.configured_command().is_some()
                    && adapter.languages().contains(&language.as_str())
            })
        })
        .cloned()
        .collect();
    languages.sort_unstable();
    languages
}

#[derive(Debug, Serialize)]
pub struct IndexedFile {
    pub relative_file_path: String,
    pub language: String,
    pub chunks: usize,
}

#[derive(Debug, Serialize)]
pub struct IndexedFilesReport {
    pub workspace: Workspace,
    pub files: Vec<IndexedFile>,
}

#[derive(Debug, Serialize)]
pub struct WatchReport {
    pub workspace: Workspace,
    pub watched: bool,
    pub already_in_requested_state: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IndexAction {
    Reused,
    Created,
    WaitedForExistingJob,
    /// The previous snapshot was stale (changed files or an embedding
    /// configuration change) and was incrementally refreshed before search.
    RefreshedIncrementally,
}

impl App {
    /// Embeds every chunk whose vector key `cache` lacks (each key once) and
    /// adds the vectors to `cache`; returns how many it embedded. Chunks the
    /// service accepted only as a shortened prefix are recorded in
    /// `truncated_for_embedding`.
    async fn embed_missing(
        &self,
        workspace: &Workspace,
        chunks: &[chunk::Chunk],
        key_of: &impl Fn(&chunk::Chunk) -> String,
        cache: &mut HashMap<String, Vec<f32>>,
        truncated_for_embedding: &mut Vec<String>,
    ) -> Result<usize> {
        let template = &self.config.embedding_document_template;
        let mut seen = HashSet::new();
        let missing: Vec<_> = chunks
            .iter()
            .filter(|&c| {
                let key = key_of(c);
                !cache.contains_key(&key) && seen.insert(key)
            })
            .collect();
        for (i, batch) in missing.chunks(self.config.embedding_batch_size).enumerate() {
            let texts: Vec<_> = batch.iter().map(|c| document_text(template, c)).collect();
            let (vectors, shortened) = embed_isolating_oversized(&self.models, &texts)
                .await
                .with_context(|| {
                    let first = batch[0];
                    format!(
                        "embedding a batch starting at {}:{}",
                        first.relative_file_path, first.start_line
                    )
                })?;
            for position in shortened {
                let chunk = batch[position];
                let location = format!("{}:{}", chunk.relative_file_path, chunk.start_line);
                tracing::warn!(workspace_id = %workspace.id, chunk = %location, bytes = texts[position].len(), "embedding service rejected the full chunk as too long; embedded a shortened prefix");
                truncated_for_embedding.push(location);
            }
            for (&c, vector) in batch.iter().zip(vectors) {
                cache.insert(key_of(c), vector);
            }
            tracing::info!(workspace_id = %workspace.id, completed = ((i+1)*self.config.embedding_batch_size).min(missing.len()), total = missing.len(), "embedding chunks");
        }
        Ok(missing.len())
    }

    /// The vector width the embedding service returns now, from embedding a
    /// prefix of the first chunk (at most `MIN_EMBED_PREFIX_BYTES`, which the
    /// service accepts whenever it accepts any chunk) with one attempt. `None`
    /// when there is no chunk or the request fails: a failed probe only warns,
    /// and the run keeps the cached vectors as it would without the probe
    /// (fail-open), without paying index-time retries or backoff.
    async fn probe_dimension(
        &self,
        workspace: &Workspace,
        chunks: &[chunk::Chunk],
    ) -> Option<usize> {
        let text = document_text(&self.config.embedding_document_template, chunks.first()?);
        let mut end = text.len().min(MIN_EMBED_PREFIX_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        match self.models.embed_once(&[text[..end].to_owned()]).await {
            Ok(vectors) => vectors.first().map(Vec::len),
            Err(error) => {
                tracing::warn!(workspace_id = %workspace.id, error = %format!("{error:#}"), "could not check the embedding width; reusing the cached vectors");
                None
            }
        }
    }

    pub async fn index(&self, path: &Path) -> Result<IndexReport> {
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let coordination = self.lock(&workspace.id).await;
        let _write = coordination.lock.write().await;
        *coordination.flight_error.lock().await = None;
        let result = self.index_locked(workspace).await;
        if let Err(error) = &result {
            *coordination.flight_error.lock().await = Some(format!("{error:#}"));
        }
        result
    }

    /// The watcher's refresh pass. `unwatch` cancels `cancellation` and returns
    /// at once, so a pass still waiting for the workspace write lock (behind a
    /// long index job) must give up instead of committing after the workspace
    /// was reported unwatched. A pass that already holds the lock runs to its end.
    async fn index_if_stale(
        &self,
        workspace: &Workspace,
        cancellation: &CancellationToken,
    ) -> Result<()> {
        let coordination = self.lock(&workspace.id).await;
        let _write = tokio::select! {
            biased;
            () = cancellation.cancelled() => return Ok(()),
            write = coordination.lock.write() => write,
        };
        // A job ahead of the watcher may already have committed the change.
        // Scan again under the lock: the pre-debounce fingerprint can be old.
        let root = workspace.path.clone();
        let prose = self.config.index.prose;
        let current =
            tokio::task::spawn_blocking(move || chunk::fingerprint_with(&root, prose)).await??;
        let persisted = manifest::try_load(&self.config.data_dir, workspace)?;
        if persisted.fingerprint == current {
            return Ok(());
        }
        *coordination.flight_error.lock().await = None;
        let result = self.index_locked(workspace.clone()).await;
        if let Err(error) = &result {
            *coordination.flight_error.lock().await = Some(format!("{error:#}"));
        }
        result.map(|_| ())
    }

    /// Runs the indexing pipeline for `workspace`, assuming the caller already
    /// holds the workspace write lock. Explicit indexing and automatic
    /// first-search indexing share this implementation so both coordinate
    /// through the same locks.
    pub(super) async fn index_locked(&self, workspace: Workspace) -> Result<IndexReport> {
        let start = Instant::now();
        let _index = self.index_lock.lock().await;
        let _data_dir_lock = super::lock_data_dir_for_indexing(&self.config.data_dir).await?;
        let prior_manifest = manifest::try_load(&self.config.data_dir, &workspace)?;
        let root = workspace.path.clone();
        let prior_files = prior_manifest.files;
        let prose = self.config.index.prose;
        let scan = tokio::task::spawn_blocking(move || {
            chunk::scan_incremental_with(&root, &prior_files, prose)
        })
        .await??;
        let chunks = scan.chunks;
        let previous = self.store.snapshot(&workspace).await?;
        let identity = self.config.embedding_identity();
        let template = &self.config.embedding_document_template;
        // A template that embeds the file path makes a vector depend on the
        // file as well as the code, so the vector cache is keyed by both.
        let uses_path = template.contains("{path}");
        let key_of =
            |c: &chunk::Chunk| vector_key(uses_path, &c.content_hash, &c.relative_file_path);
        let mut cache = match &previous {
            Some(s) if s.identity == identity => Store::cached_vectors(s, uses_path).await?,
            _ => HashMap::new(),
        };
        let mut cached_keys: HashSet<String> = cache.keys().cloned().collect();
        let mut truncated_for_embedding = Vec::new();
        // An embedding failure may commit the chunks without vectors only when
        // there are no real vectors to lose: the cache is empty (a fresh
        // workspace, a vectorless previous snapshot, or one that held no
        // chunks) and the previous snapshot, if any, has this identity (an
        // identity change also leaves the cache empty, but must stay a hard
        // failure). Decided before `embed_missing` fills the cache. Even then
        // only an availability error (service unreachable, timeout, dropped
        // connection, HTTP 5xx) is caught; a configuration or protocol error
        // (HTTP 4xx, a bad response) stays a hard error.
        let may_index_vectorless =
            cache.is_empty() && previous.as_ref().is_none_or(|s| s.identity == identity);
        let mut vectors_missing = false;
        let mut embedded_chunks = match self
            .embed_missing(
                &workspace,
                &chunks,
                &key_of,
                &mut cache,
                &mut truncated_for_embedding,
            )
            .await
        {
            Ok(embedded) => embedded,
            Err(error) if may_index_vectorless && models::is_service_unavailable(&error) => {
                tracing::warn!(workspace_id = %workspace.id, error = %format!("{error:#}"), "the embedding service is unreachable; indexing without vectors");
                cache.clear();
                truncated_for_embedding.clear();
                vectors_missing = true;
                0
            }
            Err(error) => return Err(error),
        };
        // The identity names the URL and model, not the width: a service can
        // start returning another width under both. Cached vectors of another
        // width than the service returns now are embedded again; mixing widths
        // would fail the commit, and an index of the old width fails every
        // search. The width comes from this process's latest embedding
        // response; a probe is sent only when there was none and the cache is
        // not empty (a first index never probes), so an unchanged run in a
        // process that has embedded before still sends no request.
        // Known gap: a width change seen by no request since this process's
        // last one (no search, no changed chunk) is found by the next search,
        // which warns, and the index run after it repairs.
        if !cached_keys.is_empty() {
            let current = match self.models.observed_dimension() {
                Some(dimension) => Some(dimension),
                None => self.probe_dimension(&workspace, &chunks).await,
            };
            if let Some(current) = current
                && cache.values().any(|vector| vector.len() != current)
            {
                tracing::warn!(workspace_id = %workspace.id, dimension = current, "the embedding service returns another vector width than the cached vectors; embedding them again");
                cache.retain(|_, vector| vector.len() == current);
                cached_keys.retain(|key| cache.contains_key(key));
                embedded_chunks += self
                    .embed_missing(
                        &workspace,
                        &chunks,
                        &key_of,
                        &mut cache,
                        &mut truncated_for_embedding,
                    )
                    .await?;
            }
        }
        let reused_chunks = chunks
            .iter()
            .filter(|&c| cached_keys.contains(&key_of(c)))
            .count();
        if vectors_missing {
            drop(previous);
            self.store
                .replace_vectorless(&workspace, &chunks, &identity)
                .await?;
        } else {
            let vectors: Vec<_> = chunks.iter().map(|c| cache[&key_of(c)].clone()).collect();
            let dimension = vectors
                .first()
                .map(Vec::len)
                .or_else(|| previous.as_ref().map(|s| s.dimension))
                .unwrap_or(1);
            drop(previous);
            self.store
                .replace(&workspace, &chunks, &vectors, &identity, dimension)
                .await?;
        }
        let files = scan.files.len();
        for path in &scan.lossy_files {
            tracing::warn!(workspace_id = %workspace.id, file = %path, "file is not valid UTF-8; indexed from a lossy decoding");
        }
        let lossy_decoded_files = scan.lossy_files;
        let skipped_non_source_files = scan.skipped_files.len();
        if let Some(first) = scan.skipped_files.first() {
            tracing::info!(workspace_id = %workspace.id, skipped = skipped_non_source_files, first = %first.relative_path, reason = first.reason.identifier(), "skipped files whose content is not source for their extension");
        }
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
            skipped_non_source_files,
            lossy_decoded_files,
            truncated_for_embedding,
            vectors_missing,
            total_ms: ms(start),
        })
    }

    /// Applies the `on-search` freshness policy for a workspace that already
    /// has a committed snapshot. Returns the action taken and, when the
    /// previous snapshot had to be reused instead of refreshed, a warning
    /// describing why. `identity_changed` is precomputed by the caller from
    /// the snapshot already in hand, so an embedding configuration change is
    /// always detected even when the filesystem staleness check is throttled.
    /// `vectors_missing` (the snapshot in hand was indexed without vectors,
    /// K0a) is a third trigger: when the throttled check is due (which still
    /// runs its fingerprint walk), the index is run again to embed the
    /// chunks. The repair is skipped without re-indexing while the embedder
    /// connect breaker is open (a refused connection within its window) and
    /// for `EMBEDDER_DOWN_WINDOW` after a repair that ended still vectorless
    /// or failed (`last_failed_repair`). A repair that finds the service
    /// still down commits the same vectorless snapshot and reports `Reused`;
    /// the search warns about the missing vectors itself.
    pub(super) async fn refresh_stale_snapshot(
        &self,
        workspace: &Workspace,
        coordination: &WorkspaceCoordination,
        identity_changed: bool,
        vectors_missing: bool,
    ) -> Result<(IndexAction, Option<String>)> {
        let due = {
            let mut last = coordination.last_stale_check.lock().await;
            let interval =
                std::time::Duration::from_secs(self.config.index.stale_check_interval_seconds);
            let due = last.is_none_or(|checked_at| checked_at.elapsed() >= interval);
            if due {
                *last = Some(Instant::now());
            }
            due
        };
        let current_fingerprint = if due {
            let root = workspace.path.clone();
            let prose = self.config.index.prose;
            match tokio::task::spawn_blocking(move || chunk::fingerprint_with(&root, prose)).await?
            {
                Ok(fingerprint) => Some(fingerprint),
                // A file that can't be read (permissions, a file vanishing
                // mid-walk) must not turn ordinary search into a hard
                // failure while a compatible snapshot exists.
                Err(error) if !identity_changed => {
                    return Ok((
                        IndexAction::Reused,
                        Some(format!(
                            "staleness check unavailable; reusing previous snapshot: {error:#}"
                        )),
                    ));
                }
                Err(error) => return Err(error),
            }
        } else {
            None
        };
        let file_stale = current_fingerprint.as_ref().is_some_and(|current| {
            let stored = manifest::load(&self.config.data_dir, workspace);
            stored.fingerprint.is_empty() || stored.fingerprint != *current
        });
        let repair = vectors_missing
            && due
            && self.models.embedder_skipped().is_none()
            && coordination
                .last_failed_repair
                .lock()
                .await
                .is_none_or(|failed| failed.elapsed() >= models::EMBEDDER_DOWN_WINDOW);
        let repair_only = repair && !file_stale && !identity_changed;
        if !file_stale && !identity_changed && !repair {
            return Ok((IndexAction::Reused, None));
        }
        let run = || async {
            match self.index_locked(workspace.clone()).await {
                Ok(report) if repair_only && report.vectors_missing => {
                    *coordination.last_failed_repair.lock().await = Some(Instant::now());
                    Ok((IndexAction::Reused, None))
                }
                Ok(_) => Ok((IndexAction::RefreshedIncrementally, None)),
                Err(error) => {
                    if repair_only {
                        *coordination.last_failed_repair.lock().await = Some(Instant::now());
                    }
                    Self::stale_refresh_fallback(identity_changed, error)
                }
            }
        };
        match coordination.lock.try_write() {
            Ok(_write) => run().await,
            Err(_) => {
                let wait =
                    std::time::Duration::from_secs(self.config.index.wait_for_existing_job_seconds);
                match tokio::time::timeout(wait, coordination.lock.write()).await {
                    Ok(_write) => {
                        // The previous holder may already have refreshed the
                        // snapshot; re-check before refreshing again.
                        // (A vectorless snapshot is not re-checked: the lock
                        // holder just attempted the repair.)
                        let still_stale = self
                            .store
                            .snapshot(workspace)
                            .await?
                            .is_some_and(|s| s.identity != self.config.embedding_identity())
                            || current_fingerprint.is_some_and(|current| {
                                manifest::load(&self.config.data_dir, workspace).fingerprint
                                    != current
                            });
                        if !still_stale {
                            Ok((IndexAction::WaitedForExistingJob, None))
                        } else {
                            run().await
                        }
                    }
                    Err(_elapsed) => {
                        ensure!(
                            !identity_changed,
                            "embedding configuration changed and no compatible index is available; \
                             an index refresh is already in progress"
                        );
                        Ok((
                            IndexAction::Reused,
                            Some(format!(
                                "timed out after {}s waiting for an in-progress index refresh; reusing previous snapshot",
                                wait.as_secs()
                            )),
                        ))
                    }
                }
            }
        }
    }

    /// Shared handling for a failed refresh attempt: an embedding-identity
    /// change leaves no searchable snapshot, so it must be a hard failure;
    /// a plain file-content staleness can safely fall back to the previous
    /// snapshot with a warning.
    fn stale_refresh_fallback(
        identity_changed: bool,
        error: anyhow::Error,
    ) -> Result<(IndexAction, Option<String>)> {
        if identity_changed {
            Err(error.context(
                "embedding configuration changed; automatic reindex failed and the previous \
                 incompatible snapshot cannot be searched",
            ))
        } else {
            Ok((
                IndexAction::Reused,
                Some(format!(
                    "index refresh unavailable; reusing previous snapshot: {error:#}"
                )),
            ))
        }
    }

    pub async fn status(&self, path: &Path) -> Result<Status> {
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let coordination = self.lock(&workspace.id).await;
        let indexing = coordination.lock.try_read().is_err();
        let snapshot = self.store.snapshot(&workspace).await?;
        // Warm the servers once there is an index and no build competes: the
        // decision is made under a read guard of the workspace, held until the
        // task is spawned (spawning awaits nothing), so a build cannot start
        // between the check and the warm start.
        // The languages the index holds, read once for the warm start and the
        // roster below.
        let in_index = match &snapshot {
            Some(snapshot) => Store::languages(snapshot).await.unwrap_or_default(),
            None => HashSet::new(),
        };
        if snapshot.is_some()
            && self.warm_start_due(&workspace.id)
            && let Ok(_building_excluded) = coordination.lock.try_read()
        {
            self.warm_language_servers(&workspace, in_index.clone());
        }
        let chunks = match &snapshot {
            Some(s) => s.table.count_rows(None).await?,
            None => 0,
        };
        // A manifest that cannot be read makes staleness unknown too: the
        // snapshot is reported stale and the status says why.
        let (stored_manifest, manifest_error) =
            match manifest::try_load(&self.config.data_dir, &workspace) {
                Ok(stored) => (stored, None),
                Err(error) => (
                    manifest::Manifest::empty(&workspace),
                    Some(format!("{error:#}")),
                ),
            };
        let root = workspace.path.clone();
        let prose = self.config.index.prose;
        // An unreadable file makes staleness unknown, not the status call a
        // failure: report the snapshot as stale and say why.
        let (stale, stale_check_error) =
            match tokio::task::spawn_blocking(move || chunk::fingerprint_with(&root, prose)).await?
            {
                Ok(current_fingerprint) => (
                    snapshot.is_some()
                        && (stored_manifest.fingerprint.is_empty()
                            || stored_manifest.fingerprint != current_fingerprint),
                    manifest_error,
                ),
                Err(error) => (
                    snapshot.is_some(),
                    Some(match manifest_error {
                        Some(manifest_error) => format!("{manifest_error}; {error:#}"),
                        None => format!("{error:#}"),
                    }),
                ),
            };
        let watched = self.watchers.lock().await.contains_key(&workspace.id);
        let mut analyzers_running = BTreeMap::new();
        let mut analyzers = BTreeMap::new();
        let mut servers = Vec::new();
        for adapter in self.analyzer.adapters() {
            let provider = adapter.provider();
            analyzers_running.insert(
                provider.to_owned(),
                self.analyzer.running(provider, &workspace.id).await,
            );
            let session = self.analyzer.readiness(provider, &workspace.id).await;
            analyzers.insert(provider.to_owned(), session.clone());
            let mut languages_in_index: Vec<String> = adapter
                .languages()
                .iter()
                .filter(|language| in_index.contains(**language))
                .map(|language| (*language).to_owned())
                .collect();
            languages_in_index.sort_unstable();
            let command = adapter.configured_command();
            // A live session reports the root it started at. Without one, the
            // root a start would choose now; under `manifest` that reads the
            // directories of the server's files, so it is worked out only for
            // a server a start could serve here.
            let root = match self.analyzer.session_root(provider, &workspace.id).await {
                Some(root) => root,
                None if self.config.run_profile.lsp.roots == LspRoots::Workspace
                    || (adapter.enabled()
                        && command.is_some()
                        && !languages_in_index.is_empty()) =>
                {
                    self.analyzer.server_root(adapter, &workspace).await
                }
                None => Root::workspace(&workspace.path, RootReason::NoManifest),
            };
            servers.push(ServerRoster {
                provider: provider.to_owned(),
                languages: adapter.languages().to_vec(),
                languages_in_index,
                configured: command.is_some(),
                enabled: adapter.enabled(),
                command,
                probe: self.analyzer.probe_state(provider),
                breaker_open_ms: self
                    .analyzer
                    .breaker_remaining(provider)
                    .map(|left| u64::try_from(left.as_millis()).unwrap_or(u64::MAX)),
                root: root.relative,
                root_reason: root.reason.to_string(),
                session,
            });
        }
        servers.sort_by(|a, b| a.provider.cmp(&b.provider));
        let languages_without_server =
            languages_without_server(self.analyzer.adapters(), &in_index);
        // The deprecated per-server booleans read the same map, so they can
        // never disagree with it.
        let running = |provider: &str| analyzers_running.get(provider).copied().unwrap_or(false);
        let analyzer_running = running("rust-analyzer");
        let csharp_analyzer_running = running("csharp-ls");
        let typescript_analyzer_running = running("typescript-language-server");
        let python_analyzer_running = running("pyright");
        let go_analyzer_running = running("gopls");
        let java_analyzer_running = running("jdtls");
        let clangd_analyzer_running = running("clangd");
        Ok(Status {
            workspace,
            indexed: snapshot.is_some(),
            indexing,
            chunks,
            embedding_dimension: snapshot
                .as_ref()
                .filter(|s| chunks > 0 && !s.vectors_missing)
                .map(|s| s.dimension),
            compatible: snapshot
                .as_ref()
                .is_some_and(|s| s.identity == self.config.embedding_identity()),
            vectors_missing: snapshot.as_ref().is_some_and(|s| s.vectors_missing),
            stale,
            stale_check_error,
            watched,
            analyzer_running,
            csharp_analyzer_running,
            typescript_analyzer_running,
            python_analyzer_running,
            go_analyzer_running,
            java_analyzer_running,
            clangd_analyzer_running,
            analyzers_running,
            analyzers,
            servers,
            languages_without_server,
            indexed_at_unix_seconds: snapshot.map(|s| s.indexed_at),
        })
    }

    /// Lists every file the persistent index currently has cached chunks
    /// for, from the workspace manifest -- the same data `index_workspace`
    /// wrote, not a fresh directory scan, so this reflects what was
    /// actually indexed (respecting gitignore and per-language support)
    /// rather than what's on disk right now.
    pub async fn indexed_files(&self, path: &Path) -> Result<IndexedFilesReport> {
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        ensure!(
            self.store.snapshot(&workspace).await?.is_some(),
            "workspace is not indexed; call index_workspace first"
        );
        let manifest = manifest::try_load(&self.config.data_dir, &workspace)?;
        let mut files: Vec<IndexedFile> = manifest
            .files
            .into_iter()
            .map(|(relative_file_path, cached)| IndexedFile {
                relative_file_path,
                language: cached.language,
                chunks: cached.chunks.len(),
            })
            .collect();
        files.sort_by(|a, b| a.relative_file_path.cmp(&b.relative_file_path));
        Ok(IndexedFilesReport { workspace, files })
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
        watchers.insert(
            workspace.id.clone(),
            (workspace.clone(), cancellation.clone()),
        );
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
                    biased;
                    _ = cancellation.cancelled() => break,
                    _ = interval.tick() => {}
                }
                let root = watched_workspace.path.clone();
                let prose = app.config.index.prose;
                let current = match tokio::task::spawn_blocking(move || {
                    if !root.try_exists()? {
                        return Ok(None);
                    }
                    chunk::fingerprint_with(&root, prose).map(Some)
                })
                .await
                {
                    Ok(Ok(Some(value))) => value,
                    Ok(Ok(None)) => {
                        tracing::warn!(workspace_id = %watched_workspace.id, "watched workspace disappeared; stopping watcher");
                        break;
                    }
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
                    biased;
                    _ = cancellation.cancelled() => break,
                    _ = tokio::time::sleep(debounce) => {}
                }
                if let Err(error) = app.index_if_stale(&watched_workspace, &cancellation).await {
                    tracing::warn!(workspace_id = %watched_workspace.id, error = %format!("{error:#}"), "automatic reindex failed; previous index retained");
                }
            }
            let mut watchers = app.watchers.lock().await;
            // Unwatch cancels under this lock; an old task must not remove a replacement.
            if !cancellation.is_cancelled() {
                watchers.remove(&watched_workspace.id);
                cancellation.cancel();
            }
        });
        Ok(WatchReport {
            workspace,
            watched: true,
            already_in_requested_state: false,
        })
    }

    pub async fn unwatch(&self, path: &Path) -> Result<WatchReport> {
        let mut watchers = self.watchers.lock().await;
        let workspace = match Workspace::resolve(path, &self.config.data_dir) {
            Ok(workspace) => workspace,
            Err(error) => {
                let absolute = std::path::absolute(path)?;
                watchers
                    .values()
                    .find(|(workspace, _)| workspace.path == absolute)
                    .map(|(workspace, _)| workspace.clone())
                    .ok_or(error)?
            }
        };
        let cancellation = watchers.remove(&workspace.id);
        let already_in_requested_state = cancellation.is_none();
        if let Some((_, token)) = cancellation {
            token.cancel();
        }
        Ok(WatchReport {
            workspace,
            watched: false,
            already_in_requested_state,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(path: &str, code: &str) -> chunk::Chunk {
        chunk::Chunk {
            relative_file_path: path.to_owned(),
            language: "rust".to_owned(),
            start_line: 1,
            end_line: 1,
            code: code.to_owned(),
            content_hash: "hash".to_owned(),
        }
    }

    #[test]
    fn languages_without_server_ignores_disabled_and_unconfigured_adapters() {
        let in_index: HashSet<String> = ["csharp", "python", "rust"]
            .into_iter()
            .map(str::to_owned)
            .collect();
        let without = |csharp: crate::config::CSharpLspConfig| {
            let config = crate::config::Config {
                csharp,
                ..crate::config::Config::default()
            };
            let manager = crate::lsp::Manager::new(&config);
            languages_without_server(manager.adapters(), &in_index)
        };
        // Enabled and configured: only python (never configured) has none.
        let configured = crate::config::CSharpLspConfig {
            path: Some("csharp-ls".into()),
            ..Default::default()
        };
        assert_eq!(without(configured.clone()), ["python"]);
        // A path with `disabled = true`: the adapter exists but serves nothing.
        let disabled = crate::config::CSharpLspConfig {
            disabled: true,
            ..configured
        };
        assert_eq!(without(disabled), ["csharp", "python"]);
        // No path at all: the adapter is disabled too, so this case does not
        // pin the `configured_command` guard in `languages_without_server`; the
        // guard is redundant today (every adapter's `enabled()` implies a
        // configured command) and stays as a defence.
        assert_eq!(
            without(crate::config::CSharpLspConfig::default()),
            ["csharp", "python"]
        );
    }

    #[test]
    fn the_default_document_template_embeds_the_code_unchanged() {
        let template = &crate::config::Config::default().embedding_document_template;
        let source = "fn a() {}\n// {path} stays literal\n";
        let text = document_text(template, &sample("src/a.rs", source));
        assert_eq!(text, source);
    }

    #[test]
    fn a_document_template_can_carry_the_file_path() {
        let plain = sample("src/a.rs", "fn a() {}");
        assert_eq!(
            document_text("title: {path} | text: {code}", &plain),
            "title: src/a.rs | text: fn a() {}"
        );
        // Code that mentions a placeholder is never expanded, and the
        // template's own prefix is still in front of it.
        let tricky = sample("src/a.rs", "let p = \"{path}\";");
        assert_eq!(
            document_text("{path}\n{code}", &tricky),
            "src/a.rs\nlet p = \"{path}\";"
        );
    }
}
