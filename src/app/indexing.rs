//! Workspace indexing, staleness refresh, status, and file-watch lifecycle.

use super::{App, WorkspaceCoordination, ms};
use crate::{
    chunk,
    manifest::{self, Manifest},
    store::Store,
    workspace::Workspace,
};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    collections::{HashMap, HashSet},
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
    pub csharp_analyzer_running: bool,
    pub typescript_analyzer_running: bool,
    pub python_analyzer_running: bool,
    pub indexed_at_unix_seconds: Option<String>,
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
    pub(super) async fn index_locked(&self, workspace: Workspace) -> Result<IndexReport> {
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

    /// Applies the `on-search` freshness policy for a workspace that already
    /// has a committed snapshot. Returns the action taken and, when the
    /// previous snapshot had to be reused instead of refreshed, a warning
    /// describing why. `identity_changed` is precomputed by the caller from
    /// the snapshot already in hand, so an embedding configuration change is
    /// always detected even when the filesystem staleness check is throttled.
    pub(super) async fn refresh_stale_snapshot(
        &self,
        workspace: &Workspace,
        coordination: &WorkspaceCoordination,
        identity_changed: bool,
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
            Some(tokio::task::spawn_blocking(move || chunk::fingerprint(&root)).await??)
        } else {
            None
        };
        let file_stale = current_fingerprint.as_ref().is_some_and(|current| {
            let stored = manifest::load(&self.config.data_dir, workspace);
            stored.fingerprint.is_empty() || stored.fingerprint != *current
        });
        if !file_stale && !identity_changed {
            return Ok((IndexAction::Reused, None));
        }
        match coordination.lock.try_write() {
            Ok(_write) => match self.index_locked(workspace.clone()).await {
                Ok(_) => Ok((IndexAction::RefreshedIncrementally, None)),
                Err(error) => Self::stale_refresh_fallback(identity_changed, error),
            },
            Err(_) => {
                let wait =
                    std::time::Duration::from_secs(self.config.index.wait_for_existing_job_seconds);
                match tokio::time::timeout(wait, coordination.lock.write()).await {
                    Ok(_write) => {
                        // The previous holder may already have refreshed the
                        // snapshot; re-check before refreshing again.
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
                            match self.index_locked(workspace.clone()).await {
                                Ok(_) => Ok((IndexAction::RefreshedIncrementally, None)),
                                Err(error) => Self::stale_refresh_fallback(identity_changed, error),
                            }
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
        let analyzer_running = self.analyzer.running("rust-analyzer", &workspace.id).await;
        let csharp_analyzer_running = self.analyzer.running("csharp-ls", &workspace.id).await;
        let typescript_analyzer_running = self
            .analyzer
            .running("typescript-language-server", &workspace.id)
            .await;
        let python_analyzer_running = self.analyzer.running("pyright", &workspace.id).await;
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
            csharp_analyzer_running,
            typescript_analyzer_running,
            python_analyzer_running,
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
        let manifest = manifest::load(&self.config.data_dir, &workspace);
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
}
