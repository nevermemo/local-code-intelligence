//! Application core: workspace indexing, navigation, search, and readiness.
//!
//! Public items are re-exported from private child modules so the existing
//! `crate::app::*` API remains stable.

mod compact;
mod indexing;
mod navigation;
pub(crate) mod readiness;
pub mod recipes;
mod search;
pub mod traverse;

pub use compact::{COMPACT_MAX_LINE_CHARS, compact_view};
pub use indexing::{
    IndexAction, IndexReport, IndexedFile, IndexedFilesReport, Status, WatchReport,
};
pub use navigation::{NavigationReport, ReleaseReport};
pub use readiness::{ReadinessComponent, ReadinessDegraded, ServiceStatus};
pub use search::{
    CANDIDATE_DUMP_ENV, CandidateTrace, RerankOutcome, SearchIndexLifecycle, SearchOptions,
    SearchReport, Timings,
};

use crate::{config::Config, lsp, models::Models, store::Store};
use anyhow::Result;
use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Instant};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

/// Per-workspace coordination: the existing read/write lock plus the minimum
/// state needed to share the outcome of an index job and
/// to throttle `on-search` staleness checks.
#[derive(Default)]
struct WorkspaceCoordination {
    lock: RwLock<()>,
    /// Formatted cause of the most recent failed index job.
    /// Consulted only by a caller that holds the write lock and finds no
    /// committed snapshot; cleared when a new flight starts.
    flight_error: Mutex<Option<String>>,
    /// Wall-clock time of the last `on-search` filesystem staleness check,
    /// so repeated searches within `stale_check_interval_seconds` skip the
    /// scan and reuse the current snapshot.
    last_stale_check: Mutex<Option<Instant>>,
    /// When the last repair-only refresh of a vectorless snapshot ended still
    /// vectorless (or failed), so a search does not run another one inside
    /// `EMBEDDER_DOWN_WINDOW` while the embedder keeps failing (an HTTP 5xx
    /// does not trip the connect breaker).
    last_failed_repair: Mutex<Option<Instant>>,
}

pub struct App {
    pub config: Config,
    /// `config.run_profile().hash`, computed once: the configuration does not
    /// change after [`Self::open`]. Every result the service returns carries
    /// it (see [`stamp`]).
    profile_hash: String,
    store: Store,
    models: Models,
    workspace_locks: Mutex<HashMap<String, Arc<WorkspaceCoordination>>>,
    // Bound concurrent model traffic from different indexing clients.
    index_lock: Mutex<()>,
    watchers: Mutex<HashMap<String, (crate::workspace::Workspace, CancellationToken)>>,
    analyzer: lsp::Manager,
    /// File every search appends its candidate dump line to (see
    /// [`CANDIDATE_DUMP_ENV`]); `None` writes nothing.
    candidate_dump: Option<PathBuf>,
}

impl App {
    pub async fn open(mut config: Config) -> Result<Self> {
        config.validate()?;
        std::fs::create_dir_all(&config.data_dir)?;
        config.data_dir = dunce::canonicalize(&config.data_dir)?;
        let store = Store::open(&config.data_dir.join("lancedb")).await?;
        let models = Models::new(&config)?;
        let analyzer = lsp::Manager::new(&config);
        let profile_hash = config.run_profile().hash;
        Ok(Self {
            config,
            profile_hash,
            store,
            models,
            workspace_locks: Mutex::new(HashMap::new()),
            index_lock: Mutex::new(()),
            watchers: Mutex::new(HashMap::new()),
            analyzer,
            candidate_dump: search::candidate_dump_from_env(),
        })
    }

    /// Replaces the candidate dump destination that [`Self::open`] took from
    /// the `LCI_CANDIDATE_DUMP` environment variable: `Some(file)` appends
    /// one line per search to `file`, `None` turns the dump off. For callers
    /// (tests, embedders) that must not depend on, or change, the process
    /// environment.
    #[must_use]
    pub fn with_candidate_dump(mut self, path: Option<PathBuf>) -> Self {
        self.candidate_dump = path;
        self
    }

    /// The run-profile hash of this process's configuration (64 lowercase hex
    /// characters), the value of the `profile_hash` key of every result.
    pub fn profile_hash(&self) -> &str {
        &self.profile_hash
    }

    async fn lock(&self, id: &str) -> Arc<WorkspaceCoordination> {
        self.workspace_locks
            .lock()
            .await
            .entry(id.into())
            .or_default()
            .clone()
    }
}

/// Adds `"profile_hash": hash` to a result: only when `value` is an object and
/// has no such key yet, so a result that already carries one (the compact
/// search result writes its own, last) is left as it is. The one place MCP,
/// REST and the CLI stamp results; errors are never passed through it.
pub fn stamp(value: &mut serde_json::Value, hash: &str) {
    if let Some(object) = value.as_object_mut() {
        object
            .entry("profile_hash")
            .or_insert_with(|| serde_json::Value::String(hash.to_owned()));
    }
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// File in the data directory whose OS lock serializes index commits
/// across processes.
const INDEX_LOCK_FILE: &str = "index.lock";

/// How often a wait for another process's data-directory index lock tries
/// the lock again.
const INDEX_LOCK_RETRY: std::time::Duration = std::time::Duration::from_millis(100);

/// Cross-process counterpart of `App::index_lock`: takes an exclusive OS
/// file lock on `<data_dir>/index.lock` (waiting until any other holder
/// releases it) and returns the handle that holds it; dropping the handle
/// releases the lock. Held for a whole index pass, so two processes sharing
/// a data directory -- e.g. a running server and a CLI `index` or
/// `evaluate` -- serialize their snapshot commits instead of interleaving
/// LanceDB table overwrites and manifest writes. Reads never take it.
///
/// The wait tries the lock every [`INDEX_LOCK_RETRY`] on this task instead of
/// blocking in `lock()` on a blocking thread: dropping the future ends the
/// wait. A blocking wait could not be cancelled, and the runtime's shutdown
/// waits for blocking threads, so a `serve` stopped by a signal while a
/// watcher's reindex waited here stayed alive until the other process let
/// go of the lock.
async fn lock_data_dir_for_indexing(data_dir: &std::path::Path) -> Result<std::fs::File> {
    use anyhow::Context;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data_dir.join(INDEX_LOCK_FILE))
        .context("open data-directory index lock")?;
    let mut announced = false;
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) => {
                if !announced {
                    tracing::info!(
                        data_dir = %data_dir.display(),
                        "another process is committing an index in this data directory; waiting for it"
                    );
                    announced = true;
                }
                tokio::time::sleep(INDEX_LOCK_RETRY).await;
            }
            Err(std::fs::TryLockError::Error(error)) => {
                return Err(error).context("acquire data-directory index lock");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{INDEX_LOCK_FILE, lock_data_dir_for_indexing};
    use std::{fs::OpenOptions, time::Duration};

    #[tokio::test]
    async fn data_dir_index_lock_waits_for_another_holder() {
        let temp = tempfile::tempdir().unwrap();
        let holder = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(temp.path().join(INDEX_LOCK_FILE))
            .unwrap();
        holder.lock().unwrap();

        let data_dir = temp.path().to_path_buf();
        let mut waiter = tokio::spawn(async move { lock_data_dir_for_indexing(&data_dir).await });
        assert!(
            tokio::time::timeout(Duration::from_millis(500), &mut waiter)
                .await
                .is_err(),
            "lock_data_dir_for_indexing returned while another holder held the lock"
        );

        drop(holder);
        let acquired = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("lock_data_dir_for_indexing must return promptly after the lock is released")
            .unwrap()
            .unwrap();
        let contender = OpenOptions::new()
            .write(true)
            .open(temp.path().join(INDEX_LOCK_FILE))
            .unwrap();
        assert!(
            matches!(contender.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "the returned handle must keep holding the lock"
        );
        drop(acquired);
    }
}
