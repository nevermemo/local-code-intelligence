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
    SearchReport, Timings, verify_scans_on_this_thread,
};

use crate::{config::Config, lsp, models::Models, store::Store};
use anyhow::Result;
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock, watch};
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
    /// scan and reuse the current snapshot. A synchronous mutex, held only
    /// for a read or a write of the value, so a drop guard can restore it.
    last_stale_check: std::sync::Mutex<Option<Instant>>,
    /// When the last repair-only refresh of a vectorless snapshot ended still
    /// vectorless (or failed), so a search does not run another one inside
    /// `EMBEDDER_DOWN_WINDOW` while the embedder keeps failing (an HTTP 5xx
    /// does not trip the connect breaker).
    last_failed_repair: Mutex<Option<Instant>>,
}

/// What one warm start (see [`App::warm_start`]) found, per provider: the
/// warm-up time, or why there was none.
pub type WarmOutcome = BTreeMap<&'static str, std::result::Result<Duration, String>>;

/// The receiving end of a warm start: `None` until it has finished.
pub type WarmStart = watch::Receiver<Option<Arc<WarmOutcome>>>;

/// A workspace's warm start: where its outcome arrives and the task that
/// makes it. The task is never taken out of the entry: every
/// [`App::stop_warm_starts`] caller aborts it (idempotent) and awaits the same
/// `ended` signal. A released workspace holds a tombstone, an entry with
/// neither, so that no later touch starts one.
struct WarmEntry {
    start: Option<WarmStart>,
    task: Option<WarmTask>,
}

/// The handles to stop a warm-start task and to wait until it is gone.
struct WarmTask {
    abort: tokio::task::AbortHandle,
    /// Never sent on: the task owns the sender, which is dropped when the task
    /// finishes or is aborted, so `changed()` errors exactly then.
    ended: watch::Receiver<()>,
}

/// The warm starts of the process, behind one lock with `closed`: set at
/// shutdown, after which none begins.
#[derive(Default)]
struct WarmStarts {
    entries: HashMap<String, WarmEntry>,
    closed: bool,
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
    /// The language servers: navigation, the search channel and, under
    /// `[run_profile.lsp] sessions = "shared"` (the default), the traverse
    /// calls share it for the servers that see an edit on their own, or for
    /// every server under `sync = "watch"` (see [`Self::language_servers`]).
    analyzer: Arc<lsp::Manager>,
    /// Whether the first touch of a workspace warms its language servers
    /// (see [`Self::warm_language_servers`]); fixed by the configuration:
    /// `shared` sessions with `[traverse] enabled` and `lsp.live` in the
    /// default set (`[run_profile.lsp] live`, on by default) or named by a
    /// policy.
    warm_on_touch: bool,
    /// The warm start of each workspace id touched so far, one per id.
    warm_starts: std::sync::Mutex<WarmStarts>,
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
        let analyzer = Arc::new(
            lsp::Manager::new(&config)
                .with_indexed_files(Arc::new(ManifestFiles::new(config.data_dir.clone()))),
        );
        let profile_hash = config.run_profile().hash;
        let warm_on_touch = config.run_profile.lsp.sessions == crate::config::LspSessions::Shared
            && config.traverse.enabled
            && (config.run_profile.lsp.live
                || config.traverse.policies.values().any(|policy| {
                    policy
                        .extra_maps
                        .iter()
                        .flatten()
                        .any(|map| map.trim() == lci_maps::lsp_live::LSP_LIVE_ID)
                }));
        Ok(Self {
            config,
            profile_hash,
            store,
            models,
            workspace_locks: Mutex::new(HashMap::new()),
            index_lock: Mutex::new(()),
            watchers: Mutex::new(HashMap::new()),
            analyzer,
            warm_on_touch,
            warm_starts: std::sync::Mutex::new(WarmStarts::default()),
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

    /// The service's language-server manager, for a caller that shares its
    /// sessions (the traverse calls under `[run_profile.lsp] sessions =
    /// "shared"`, for the servers that see an edit on their own or under
    /// `sync = "watch"`). A server started through a clone is stopped like any other
    /// of the service: by the idle sweeper, [`Self::release_language_servers`]
    /// and [`Self::shutdown_language_servers`].
    pub fn language_servers(&self) -> Arc<lsp::Manager> {
        Arc::clone(&self.analyzer)
    }

    /// Starts the language server of every enabled adapter for the workspace
    /// at `path` and waits (at most `timeout`) until each is primed, so the
    /// first search does not pay the start (see [`lsp::Manager::prime`]). The
    /// map holds, per provider, the warm-up time or why there was none. The
    /// warm-up uses the sessions searches use; it never changes an answer.
    /// The sessions it starts are stamped first (the tree barrier every search
    /// and navigation call passes), so the first search keeps them unless the
    /// tree has changed since.
    pub async fn prime_language_servers(
        &self,
        path: &std::path::Path,
        timeout: std::time::Duration,
    ) -> Result<std::collections::BTreeMap<&'static str, Result<std::time::Duration>>> {
        let workspace = crate::workspace::Workspace::resolve(path, &self.config.data_dir)?;
        let enabled: Vec<Arc<dyn crate::lsp::adapter::LspAdapter>> = self
            .analyzer
            .adapters()
            .iter()
            .filter(|adapter| adapter.enabled())
            .cloned()
            .collect();
        let refs: Vec<&Arc<dyn crate::lsp::adapter::LspAdapter>> = enabled.iter().collect();
        self.analyzer.tree_barrier(&workspace, &refs).await;
        Ok(self.analyzer.prime(&workspace, timeout).await)
    }

    /// Whether a traverse call under the profile keeps this adapter's session
    /// in the service's manager: `sessions = "shared"` and
    /// `LspAdapter::keeps_session` (the server lists its tree stamp's
    /// configuration files and sees an edit on its own or `sync = "watch"`
    /// tells it). These sessions pass the tree barrier of a traverse walk
    /// (`lsp::Manager::tree_barrier`); navigation passes the same barrier, once
    /// per call, and records it in the same way. The search channel passes
    /// `Manager::search_barrier`, which records the same stamps except that it
    /// keeps a watching server's session through a changed stamp, without
    /// recording one for it.
    fn shares_session(&self, adapter: &Arc<dyn crate::lsp::adapter::LspAdapter>) -> bool {
        let profile = &self.config.run_profile.lsp;
        profile.sessions == crate::config::LspSessions::Shared
            && adapter.keeps_session(profile.sync == crate::config::LspSync::Watch)
    }

    /// First touch of `workspace`: starts, in the background, the language
    /// servers of every enabled adapter (see [`lsp::Manager::prime`]) so the
    /// first `lsp.live` walk finds them started instead of paying the start.
    /// Returns at once; no request waits for the warm-up. Only when
    /// `[run_profile.lsp] sessions = "shared"` (the default) and `lsp.live` in the
    /// default map set (`[run_profile.lsp] live`, on by default) or a
    /// `[traverse.policies.*]` `extra_maps` names `lsp.live` (under `per_call`
    /// nothing starts), and once per workspace and process. Only the servers a
    /// walk would share start: those that see an edit on their own
    /// (`LspAdapter::watches_workspace`), or every server under
    /// `sync = "watch"`; a walk starts the others per call, so a warm one
    /// would be unused. It is bounded by nothing but its abort
    /// (`stop_warm_starts`): a server that is slow to start keeps starting in
    /// its session and the first walk shares that start. It changes which
    /// request pays for a start, never an answer.
    fn warm_language_servers(
        &self,
        workspace: &crate::workspace::Workspace,
        languages: std::collections::HashSet<String>,
    ) {
        if !self.warm_on_touch {
            return;
        }
        let shared: std::collections::HashSet<&str> = self
            .analyzer
            .adapters()
            .iter()
            .filter(|adapter| self.shares_session(adapter))
            .flat_map(|adapter| adapter.languages().iter().copied())
            .collect();
        let languages: std::collections::HashSet<String> = languages
            .into_iter()
            .filter(|language| shared.contains(language.as_str()))
            .collect();
        // The entry and its task are registered together under the lock, so a
        // release that finds the entry finds the task, and one that found none
        // has left a tombstone this sees.
        let mut guard = self
            .warm_starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let started = &mut *guard;
        if started.closed || started.entries.contains_key(&workspace.id) {
            return;
        }
        let (sender, receiver) = watch::channel(None);
        let analyzer = Arc::clone(&self.analyzer);
        let task_workspace = workspace.clone();
        let (alive, ended) = watch::channel(());
        let task = tokio::spawn(async move {
            let _alive = alive;
            let workspace = task_workspace;
            let mut outcome = WarmOutcome::new();
            // The tree stamp the sessions start under is recorded first (the
            // barrier every shared session passes), so the first walk keeps
            // them unless the tree has changed since.
            let adapters: Vec<Arc<dyn crate::lsp::adapter::LspAdapter>> = analyzer
                .adapters()
                .iter()
                .filter(|adapter| {
                    adapter.enabled()
                        && adapter
                            .languages()
                            .iter()
                            .any(|language| languages.contains(*language))
                })
                .cloned()
                .collect();
            let refs: Vec<&Arc<dyn crate::lsp::adapter::LspAdapter>> = adapters.iter().collect();
            analyzer.tree_barrier(&workspace, &refs).await;
            let primed = analyzer
                .prime_languages(&workspace, None, Some(&languages))
                .await;
            for (provider, result) in primed {
                match &result {
                    Ok(took) => tracing::info!(
                        provider,
                        ms = took.as_millis() as u64,
                        "lsp.live warm start"
                    ),
                    Err(error) => tracing::info!(
                        provider,
                        error = %format!("{error:#}"),
                        "lsp.live warm start failed"
                    ),
                }
                outcome.insert(provider, result.map_err(|error| format!("{error:#}")));
            }
            let _ = sender.send(Some(Arc::new(outcome)));
        });
        started.entries.insert(
            workspace.id.clone(),
            WarmEntry {
                start: Some(receiver),
                task: Some(WarmTask {
                    abort: task.abort_handle(),
                    ended,
                }),
            },
        );
    }

    /// Whether a touch of workspace `id` would begin a warm start (the
    /// languages of its index are only worth reading then).
    fn warm_start_due(&self, id: &str) -> bool {
        let started = self
            .warm_starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.warm_on_touch && !started.closed && !started.entries.contains_key(id)
    }

    /// Aborts the warm-start task of workspace `only` (every workspace's when
    /// `None`) and waits until it is gone, so that no session is created after
    /// the release that follows returns, whoever else is stopping it: every
    /// caller waits for the same end of the task. For one workspace the reservation is
    /// made under the lock before anything is awaited: its entry stays (or a
    /// tombstone takes its place), so a touch racing the release starts
    /// nothing and a released workspace is not warmed again. With `None` (the
    /// service is shutting down) no warm start begins any more.
    async fn stop_warm_starts(&self, only: Option<&str>) {
        let tasks: Vec<_> = {
            let mut guard = self
                .warm_starts
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let started = &mut *guard;
            match only {
                Some(id) => started
                    .entries
                    .entry(id.to_owned())
                    .or_insert(WarmEntry {
                        start: None,
                        task: None,
                    })
                    .task
                    .iter()
                    .map(|task| (task.abort.clone(), task.ended.clone()))
                    .collect(),
                None => {
                    started.closed = true;
                    started
                        .entries
                        .values()
                        .filter_map(|entry| entry.task.as_ref())
                        .map(|task| (task.abort.clone(), task.ended.clone()))
                        .collect()
                }
            }
        };
        for (abort, _) in &tasks {
            abort.abort();
        }
        for (_, mut ended) in tasks {
            // Errors once the task's sender is dropped: finished or aborted.
            while ended.changed().await.is_ok() {}
        }
    }

    /// The warm start of the workspace with id `workspace_id`; `None` when
    /// none began (see [`Self::warm_language_servers`]). For tests.
    pub fn warm_start(&self, workspace_id: &str) -> Option<WarmStart> {
        self.warm_starts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entries
            .get(workspace_id)
            .and_then(|entry| entry.start.clone())
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

/// Cross-process counterpart of `App::index_lock`: takes an exclusive OS
/// file lock on `<data_dir>/index.lock` (waiting until any other holder
/// releases it) and returns the handle that holds it; dropping the handle
/// releases the lock. Held for a whole index pass, so two processes sharing
/// a data directory -- e.g. a running server and a CLI `index` or
/// `evaluate` -- serialize their snapshot commits instead of interleaving
/// LanceDB table overwrites and manifest writes. Reads never take it.
///
/// The wait is the OS's blocking `lock()` on a plain `std::thread` (not the
/// runtime's blocking pool), and the caller awaits a `oneshot`: the lock's
/// release is the event that ends the wait, with no retry timer. Dropping the
/// future ends the caller's wait at once, and the thread, which cannot be
/// interrupted inside `lock()`, unlocks and ends as soon as it gets the lock
/// (its send finds no receiver). A plain thread is not joined by the
/// runtime's shutdown, as a blocking-pool thread is, so a `serve` stopped by a
/// signal while a watcher's reindex waited here exits at once; the OS drops
/// the thread's lock request with the process.
async fn lock_data_dir_for_indexing(data_dir: &std::path::Path) -> Result<std::fs::File> {
    use anyhow::Context;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data_dir.join(INDEX_LOCK_FILE))
        .context("open data-directory index lock")?;
    let data_dir = data_dir.display().to_string();
    lock_file_exclusive(file, move || {
        tracing::info!(
            data_dir = %data_dir,
            "another process is committing an index in this data directory; waiting for it"
        );
    })
    .await
}

/// Takes an exclusive OS lock on `file` on a plain thread and returns the
/// file holding it. `on_wait` runs once, on that thread, when the lock is
/// held by another handle and the thread is about to block.
async fn lock_file_exclusive(
    file: std::fs::File,
    on_wait: impl FnOnce() + Send + 'static,
) -> Result<std::fs::File> {
    use anyhow::Context;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("lci-index-lock".into())
        .spawn(move || {
            let locked = match file.try_lock() {
                Ok(()) => Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    on_wait();
                    file.lock().map(|()| file)
                }
                Err(std::fs::TryLockError::Error(error)) => Err(error),
            };
            // A caller that stopped waiting leaves the file here: dropping it
            // releases a lock that nobody wants.
            let _ = sender.send(locked);
        })
        .context("start the index-lock thread")?;
    receiver
        .await
        .context("the index-lock thread ended without an answer")?
        .context("acquire data-directory index lock")
}

/// The index's file list for the tree barrier of the service's language
/// servers ([`lsp::stamp::IndexedFiles`]): the paths of the workspace
/// manifest, which `index_workspace` and the incremental refresh write. The
/// list is read once per change of the manifest file (its modification time
/// and length) and kept, so a call with an unchanged index pays one stat. A
/// workspace with no manifest, or an empty one, has no list.
pub struct ManifestFiles {
    data_dir: PathBuf,
    kept: std::sync::Mutex<HashMap<String, KeptFiles>>,
}

impl ManifestFiles {
    /// The file list of the manifests under `data_dir`.
    pub fn new(data_dir: PathBuf) -> Self {
        Self {
            data_dir,
            kept: std::sync::Mutex::new(HashMap::new()),
        }
    }
}

/// One workspace's kept list and the manifest stat it was read under.
struct KeptFiles {
    modified: Option<std::time::SystemTime>,
    len: u64,
    files: Arc<Vec<String>>,
}

#[lci_maps::async_trait]
impl lsp::stamp::IndexedFiles for ManifestFiles {
    async fn indexed_files(
        &self,
        workspace: &crate::workspace::Workspace,
    ) -> Option<Arc<Vec<String>>> {
        let path = crate::manifest::manifest_path(&self.data_dir, &workspace.id);
        let metadata = std::fs::metadata(&path).ok()?;
        let (modified, len) = (metadata.modified().ok(), metadata.len());
        if let Some(kept) = self.kept.lock().unwrap().get(&workspace.id)
            && kept.modified == modified
            && kept.len == len
        {
            return Some(Arc::clone(&kept.files));
        }
        let (data_dir, loaded) = (self.data_dir.clone(), workspace.clone());
        let manifest =
            tokio::task::spawn_blocking(move || crate::manifest::try_load(&data_dir, &loaded))
                .await
                .ok()?
                .ok()?;
        if manifest.files.is_empty() {
            return None;
        }
        let mut files: Vec<String> = manifest.files.into_keys().collect();
        files.sort();
        let files = Arc::new(files);
        self.kept.lock().unwrap().insert(
            workspace.id.clone(),
            KeptFiles {
                modified,
                len,
                files: Arc::clone(&files),
            },
        );
        Some(files)
    }
}

#[cfg(test)]
mod tests {
    use super::{INDEX_LOCK_FILE, lock_data_dir_for_indexing, lock_file_exclusive};
    use std::{fs::OpenOptions, time::Duration};

    /// Outer bound of a test's awaits: a hung wait fails the test, it is never
    /// what a passing test depends on.
    const OUTER: Duration = Duration::from_secs(20);

    fn open_lock_file(dir: &std::path::Path) -> std::fs::File {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(dir.join(INDEX_LOCK_FILE))
            .unwrap()
    }

    #[tokio::test]
    async fn the_lock_wait_ends_when_the_holder_releases() {
        let temp = tempfile::tempdir().unwrap();
        let holder = open_lock_file(temp.path());
        holder.lock().unwrap();

        // The waiter tells the test when its thread is about to enter
        // `lock()` (the callback fires just before the call, so the release
        // below may come before or after the thread blocks); either way the
        // wait can only end through the release, since the holder keeps the lock.
        let (blocking, about_to_block) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(lock_file_exclusive(
            open_lock_file(temp.path()),
            move || blocking.send(()).unwrap(),
        ));
        tokio::task::spawn_blocking(move || about_to_block.recv().unwrap())
            .await
            .unwrap();

        drop(holder);
        let acquired = tokio::time::timeout(OUTER, waiter)
            .await
            .expect("the wait did not end after the holder released the lock")
            .unwrap()
            .unwrap();
        let contender = open_lock_file(temp.path());
        assert!(
            matches!(contender.try_lock(), Err(std::fs::TryLockError::WouldBlock)),
            "the returned handle must keep holding the lock"
        );
        drop(acquired);
    }

    #[tokio::test]
    async fn a_free_lock_is_taken_without_waiting() {
        let temp = tempfile::tempdir().unwrap();
        let acquired = tokio::time::timeout(OUTER, lock_data_dir_for_indexing(temp.path()))
            .await
            .expect("a free lock must not wait")
            .unwrap();
        let contender = open_lock_file(temp.path());
        assert!(matches!(
            contender.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(acquired);
    }

    #[tokio::test]
    async fn a_dropped_wait_gives_the_lock_back_when_the_thread_gets_it() {
        let temp = tempfile::tempdir().unwrap();
        let holder = open_lock_file(temp.path());
        holder.lock().unwrap();

        let (blocking, about_to_block) = std::sync::mpsc::channel();
        let waiter = tokio::spawn(lock_file_exclusive(
            open_lock_file(temp.path()),
            move || blocking.send(()).unwrap(),
        ));
        tokio::task::spawn_blocking(move || about_to_block.recv().unwrap())
            .await
            .unwrap();

        // The caller gives up: the future is dropped while the thread blocks.
        waiter.abort();
        let _ = waiter.await;
        drop(holder);

        // The thread now gets the lock, finds no receiver and releases it, so
        // a blocking lock on a fresh handle ends instead of waiting forever.
        let contender = open_lock_file(temp.path());
        let (done, finished) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            contender.lock().unwrap();
            done.send(()).unwrap();
        });
        tokio::task::spawn_blocking(move || finished.recv_timeout(OUTER))
            .await
            .unwrap()
            .expect("the lock stayed held after its caller stopped waiting");
    }
}
