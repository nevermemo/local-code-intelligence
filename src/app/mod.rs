//! Application core: workspace indexing, navigation, search, and readiness.
//!
//! Public items are re-exported from private child modules so the existing
//! `crate::app::*` API remains stable.

mod indexing;
mod navigation;
mod readiness;
mod search;

pub use indexing::{IndexAction, IndexReport, Status, WatchReport};
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
