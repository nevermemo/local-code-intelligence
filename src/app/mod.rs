//! Application core: workspace indexing, navigation, search, and readiness.
//!
//! Public items are re-exported from private child modules so the existing
//! `crate::app::*` API remains stable.

mod indexing;
mod navigation;
mod readiness;
mod search;

pub use indexing::{
    IndexAction, IndexReport, IndexedFile, IndexedFilesReport, Status, WatchReport,
};
pub use navigation::NavigationReport;
pub use readiness::{ReadinessComponent, ReadinessDegraded, ServiceStatus};
pub use search::{SearchIndexLifecycle, SearchReport, Timings};

use crate::{config::Config, lsp, models::Models, store::Store};
use anyhow::Result;
use std::{collections::HashMap, sync::Arc, time::Instant};
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;

/// Per-workspace coordination: the existing read/write lock plus the minimum
/// state needed to share the outcome of an automatic first-index flight and
/// to throttle `on-search` staleness checks.
#[derive(Default)]
struct WorkspaceCoordination {
    lock: RwLock<()>,
    /// Formatted cause of the most recent failed automatic first-index flight.
    /// Consulted only by a caller that holds the write lock and finds no
    /// committed snapshot; cleared when a new flight starts.
    flight_error: Mutex<Option<String>>,
    /// Wall-clock time of the last `on-search` filesystem staleness check,
    /// so repeated searches within `stale_check_interval_seconds` skip the
    /// scan and reuse the current snapshot.
    last_stale_check: Mutex<Option<Instant>>,
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
}

fn ms(start: Instant) -> f64 {
    start.elapsed().as_secs_f64() * 1000.0
}

/// File in the data directory whose OS lock serializes index commits
/// across processes.
const INDEX_LOCK_FILE: &str = "index.lock";

/// Cross-process counterpart of `App::index_lock`: takes an exclusive OS
/// file lock on `<data_dir>/index.lock` (blocking until any other holder
/// releases it) and returns the handle that holds it; dropping the handle
/// releases the lock. Held for a whole index pass, so two processes sharing
/// a data directory -- e.g. a running server and a CLI `index` or
/// `evaluate` -- serialize their snapshot commits instead of interleaving
/// LanceDB table overwrites and manifest writes. Reads never take it.
fn lock_data_dir_for_indexing(data_dir: &std::path::Path) -> Result<std::fs::File> {
    use anyhow::Context;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data_dir.join(INDEX_LOCK_FILE))
        .context("open data-directory index lock")?;
    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            tracing::info!(
                data_dir = %data_dir.display(),
                "another process is committing an index in this data directory; waiting for it"
            );
            file.lock().context("acquire data-directory index lock")?;
        }
        Err(std::fs::TryLockError::Error(error)) => {
            return Err(error).context("acquire data-directory index lock");
        }
    }
    Ok(file)
}
