//! `[run_profile.lsp] sync = "watch"`: a language server's view of the tree
//! kept current while it runs.
//!
//! A server walks the workspace when it starts and, unless something tells it,
//! answers from that walk for as long as it lives. A [`WorkspaceSync`] watches
//! one workspace root (`notify`: ReadDirectoryChangesW, inotify or FSEvents,
//! recursive; a polling watcher at `watch_poll_milliseconds` when that cannot
//! start) and hands every source-file change to a [`Sink`] as a batch, which
//! the manager sends to each server of the workspace as one
//! `workspace/didChangeWatchedFiles`. It is independent of index watching
//! (`App::watch`): a workspace can be index-watched, synced, both or neither.
//!
//! - Events: the watcher's thread queues them and wakes the forwarding task,
//!   which takes everything queued (a drain) and turns it into one batch. A
//!   path is kept when [`crate::language::for_path`] knows its language, it
//!   is not prose and it is not under `.git`. Each kept path is in a batch
//!   once, typed by the disk as it is at the drain: absent is
//!   [`ChangeKind::Deleted`], present and known to the table below is
//!   [`ChangeKind::Changed`], otherwise [`ChangeKind::Created`]. There is no
//!   debounce timer: a write that yields two events gives one batch or two,
//!   and a repeated `Changed` is harmless.
//! - The table: `(mtime, length)` per source file, filled by one walk
//!   ([`crate::chunk::source_files`], the index's rules) when the sync starts
//!   and updated by every forwarded change. The walk runs again only after
//!   the watcher reports an overflow (`need_rescan`) or an error; the
//!   difference between the table and that walk is the next batch.
//!   The polling fallback compares modification times in whole seconds: a
//!   second write within the same second is sent only with a later change.
//!
//! Known limits:
//! - Windows: `notify` 8.2 reports no overflow, and when ReadDirectoryChangesW
//!   fails with anything but "aborted" or "access denied" (a 16 KiB buffer
//!   overflow during a large checkout gives ERROR_NOTIFY_ENUM_DIR) it logs and
//!   stops watching that directory without telling the handler: no rescan
//!   happens and the sync goes silent until the workspace's sessions stop
//!   (roadmap N102).
//! - Single events are not filtered by `.gitignore` (only the walk is): a
//!   source file written under an ignored directory (`target/**/*.rs`, say)
//!   is sent, as `Created` when the table does not know it.
//! - A renamed or moved directory arrives as the directory's own path, which
//!   is no source file, so the files inside it are not sent.
//! - [`WorkspaceSync::drain_now`] is the same drain, called by the manager
//!   before every request: what the watcher has already reported reaches the
//!   servers before the request does. A drain never waits on a walk (walks
//!   run on a blocking thread outside the drain lock) and never on the OS.
//!
//! Prose files are left out until K12c. Dynamic registration of watched
//! globs is not advertised (K12a).

use crate::{chunk, language};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    future::Future,
    path::{Component, Path, PathBuf},
    pin::Pin,
    sync::{
        Arc, Mutex, OnceLock, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime},
};
use tokio::sync::{Notify, watch};

/// The protocol's `FileChangeType`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ChangeKind {
    Created,
    Changed,
    Deleted,
}

impl ChangeKind {
    /// The protocol's number: 1, 2 or 3.
    pub const fn code(self) -> u8 {
        match self {
            Self::Created => 1,
            Self::Changed => 2,
            Self::Deleted => 3,
        }
    }
}

/// One file of a batch, by its path relative to the root (with `/`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub relative: String,
    pub kind: ChangeKind,
}

/// What the forwarding task hands a batch to; the batch is never empty and
/// names each path once, in path order. The sync waits for the returned
/// future before the next drain starts.
pub type Sink =
    Arc<dyn Fn(Vec<FileChange>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Which watcher produces the events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Backend {
    /// The platform's change notifications (`notify::RecommendedWatcher`).
    Notify,
    /// A polling watcher, after the platform's watcher failed to start.
    Poll,
}

/// The counters of one [`WorkspaceSync`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SyncStatus {
    /// `None` while the watcher is still being opened.
    pub backend: Option<Backend>,
    /// Events the watcher reported, kept or not.
    pub events: u64,
    /// Batches handed to the sink (the sync's generation).
    pub batches: u64,
    /// File changes in those batches.
    pub forwarded: u64,
    /// Walks of the tree: the one at start plus one per rescan.
    pub walks: u64,
    /// Walks after an overflow or a watcher error.
    pub rescans: u64,
}

/// `(mtime, length)` of a file; `None` for an absent one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Stamp {
    modified: Option<SystemTime>,
    len: u64,
}

fn stamp_of(path: &Path) -> Option<Stamp> {
    let metadata = std::fs::metadata(path).ok()?;
    metadata.is_file().then(|| Stamp {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

/// Relative path to stamp. `None` marks a file an event found deleted, so that
/// the walk at start, which may have listed it a moment before, does not bring
/// it back.
type Table = HashMap<String, Option<Stamp>>;

/// A function that opens the watcher of a root, sending to the handler.
type Open = fn(Handler, &Path, Duration) -> notify::Result<Box<dyn Watcher + Send>>;

fn open_notify(
    handler: Handler,
    root: &Path,
    _: Duration,
) -> notify::Result<Box<dyn Watcher + Send>> {
    let mut watcher = notify::RecommendedWatcher::new(handler, notify::Config::default())?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(Box::new(watcher))
}

fn open_poll(
    handler: Handler,
    root: &Path,
    interval: Duration,
) -> notify::Result<Box<dyn Watcher + Send>> {
    let mut watcher = notify::PollWatcher::new(
        handler,
        notify::Config::default().with_poll_interval(interval),
    )?;
    watcher.watch(root, RecursiveMode::Recursive)?;
    Ok(Box::new(watcher))
}

/// The watcher's side: queues an event and wakes the forwarding task. Holds
/// the shared state weakly, so a watcher thread that outlives the sync for a
/// moment keeps nothing alive.
struct Handler(Weak<Shared>);

impl notify::EventHandler for Handler {
    fn handle_event(&mut self, event: notify::Result<Event>) {
        if let Some(shared) = self.0.upgrade() {
            shared.push(event);
        }
    }
}

struct Shared {
    root: PathBuf,
    sink: Sink,
    backend: OnceLock<Backend>,
    /// The running watcher; dropped when the sync stops.
    watcher: Mutex<Option<Box<dyn Watcher + Send>>>,
    queue: Mutex<Vec<notify::Result<Event>>>,
    /// Woken by every queued event and by a drain that found a rescan.
    pending: Notify,
    rescan: AtomicBool,
    /// Held for a whole drain, the sink's sends included, so drains (and the
    /// batches they send) never interleave. Never held during a walk.
    drain: tokio::sync::Mutex<()>,
    table: Mutex<Table>,
    events: AtomicU64,
    batches: AtomicU64,
    forwarded: AtomicU64,
    walks: AtomicU64,
    rescans: AtomicU64,
    /// Bumped after every walk and every batch, for the waits.
    progress: watch::Sender<u64>,
}

impl Shared {
    fn push(&self, event: notify::Result<Event>) {
        // Queued before counted: a count that grew means the event waits
        // for the next drain.
        self.queue.lock().unwrap().push(event);
        self.events.fetch_add(1, Ordering::Relaxed);
        self.pending.notify_one();
    }

    fn bump(&self) {
        self.progress.send_modify(|count| *count += 1);
    }

    /// The table key of an event's path, or `None` when the path is not one
    /// the sync forwards (see the module comment).
    fn relative(&self, path: &Path) -> Option<String> {
        let relative = path.strip_prefix(&self.root).ok()?;
        let mut parts = Vec::new();
        for component in relative.components() {
            let Component::Normal(part) = component else {
                return None;
            };
            let part = part.to_str()?;
            if part == ".git" {
                return None;
            }
            parts.push(part);
        }
        let adapter = language::for_path(relative)?;
        (!parts.is_empty() && !adapter.is_prose()).then(|| parts.join("/"))
    }

    /// Takes the queued events: the paths they name that the sync forwards,
    /// and whether one of them asks for a rescan.
    fn take_events(&self) -> (BTreeSet<String>, bool) {
        let events = std::mem::take(&mut *self.queue.lock().unwrap());
        let mut paths = BTreeSet::new();
        let mut rescan = false;
        for event in events {
            match event {
                Ok(event) => {
                    rescan |= event.need_rescan();
                    if matches!(event.kind, EventKind::Access(_)) {
                        continue;
                    }
                    paths.extend(event.paths.iter().filter_map(|path| self.relative(path)));
                }
                Err(error) => {
                    tracing::warn!(
                        root = %self.root.display(),
                        error = %error,
                        "file watcher error; the workspace will be walked again"
                    );
                    rescan = true;
                }
            }
        }
        (paths, rescan)
    }

    /// One drain: the queued events (and, after a rescan, the difference
    /// between the table and `listing`) become one batch for the sink, then
    /// the table takes what was sent.
    async fn drain(&self, listing: Option<HashMap<String, Stamp>>) {
        let _turn = self.drain.lock().await;
        let (paths, rescan) = self.take_events();
        if rescan {
            self.rescan.store(true, Ordering::Release);
            self.pending.notify_one();
        }
        let mut batch: BTreeMap<String, ChangeKind> = BTreeMap::new();
        let mut rows = Vec::new();
        {
            let table = self.table.lock().unwrap();
            if let Some(listing) = &listing {
                for (relative, stamp) in listing {
                    match table.get(relative) {
                        Some(Some(known)) if known == stamp => {}
                        Some(Some(_)) => {
                            batch.insert(relative.clone(), ChangeKind::Changed);
                        }
                        _ => {
                            batch.insert(relative.clone(), ChangeKind::Created);
                        }
                    }
                }
                for (relative, row) in table.iter() {
                    if row.is_some()
                        && !listing.contains_key(relative)
                        && stamp_of(&self.root.join(relative)).is_none()
                    {
                        batch.insert(relative.clone(), ChangeKind::Deleted);
                    }
                }
            }
            for relative in paths {
                let now = stamp_of(&self.root.join(&relative));
                let kind = match (now, table.get(&relative)) {
                    (None, _) => ChangeKind::Deleted,
                    (Some(_), Some(Some(_))) => ChangeKind::Changed,
                    (Some(_), _) => ChangeKind::Created,
                };
                batch.insert(relative.clone(), kind);
                rows.push((relative, now));
            }
        }
        let sent = !batch.is_empty();
        if sent {
            let changes: Vec<FileChange> = batch
                .into_iter()
                .map(|(relative, kind)| FileChange { relative, kind })
                .collect();
            let count = changes.len() as u64;
            (self.sink)(changes).await;
            self.forwarded.fetch_add(count, Ordering::Relaxed);
            self.batches.fetch_add(1, Ordering::Relaxed);
        }
        {
            let mut table = self.table.lock().unwrap();
            if let Some(listing) = listing {
                *table = listing
                    .into_iter()
                    .map(|(relative, stamp)| (relative, Some(stamp)))
                    .collect();
            }
            for (relative, now) in rows {
                table.insert(relative, now);
            }
        }
        if sent {
            self.bump();
        }
    }

    /// Whether each of `relatives` was last forwarded as it is on disk now
    /// (an absent file as absent).
    fn is_synced(&self, relatives: &[&str]) -> bool {
        let table = self.table.lock().unwrap();
        relatives.iter().all(|relative| {
            table.get(*relative).copied().flatten() == stamp_of(&self.root.join(relative))
        })
    }

    fn status(&self) -> SyncStatus {
        SyncStatus {
            backend: self.backend.get().copied(),
            events: self.events.load(Ordering::Relaxed),
            batches: self.batches.load(Ordering::Relaxed),
            forwarded: self.forwarded.load(Ordering::Relaxed),
            walks: self.walks.load(Ordering::Relaxed),
            rescans: self.rescans.load(Ordering::Relaxed),
        }
    }
}

/// The source files under `root` with their stamps, on a blocking thread;
/// `None` (logged) when the walk fails.
async fn walk(root: &Path) -> Option<HashMap<String, Stamp>> {
    let root = root.to_owned();
    let listed = tokio::task::spawn_blocking(move || chunk::source_files(&root, false)).await;
    match listed {
        Ok(Ok(listed)) => Some(
            listed
                .into_iter()
                .map(|file| {
                    (
                        file.relative,
                        Stamp {
                            modified: file.modified,
                            len: file.len,
                        },
                    )
                })
                .collect(),
        ),
        Ok(Err(error)) => {
            tracing::warn!(error = %format!("{error:#}"), "workspace walk for language-server sync failed");
            None
        }
        Err(_) => None,
    }
}

/// Opens a watcher with `open` on a blocking thread: inotify walks the whole
/// tree to add its watches and the polling watcher scans it, so neither may
/// run under a lock or on a runtime worker. `None` when it failed (logged
/// by the caller) or the task was cancelled.
async fn open_blocking(
    shared: &Arc<Shared>,
    open: Open,
    interval: Duration,
) -> Option<notify::Result<Box<dyn Watcher + Send>>> {
    let handler = Handler(Arc::downgrade(shared));
    let root = shared.root.clone();
    tokio::task::spawn_blocking(move || open(handler, &root, interval))
        .await
        .ok()
}

/// The forwarding task: opens the platform watcher (the polling one when
/// that fails), walks once, then drains whenever events are queued, walking
/// again first when a rescan was asked for.
async fn run(shared: Arc<Shared>, open: Open, poll_interval: Duration) {
    let Some(opened) = open_blocking(&shared, open, poll_interval).await else {
        return;
    };
    let opened = match opened {
        Ok(watcher) => {
            let _ = shared.backend.set(Backend::Notify);
            Ok(watcher)
        }
        Err(error) => {
            tracing::warn!(
                root = %shared.root.display(),
                error = %error,
                poll_ms = poll_interval.as_millis() as u64,
                "the file watcher did not start; polling for language-server sync instead"
            );
            let _ = shared.backend.set(Backend::Poll);
            let Some(polled) = open_blocking(&shared, open_poll, poll_interval).await else {
                return;
            };
            polled
        }
    };
    match opened {
        Ok(watcher) => *shared.watcher.lock().unwrap() = Some(watcher),
        Err(error) => tracing::warn!(
            root = %shared.root.display(),
            error = %error,
            "the polling file watcher did not start either; language servers will not be told about edits"
        ),
    }
    if let Some(listing) = walk(&shared.root).await {
        let mut table = shared.table.lock().unwrap();
        for (relative, stamp) in listing {
            table.entry(relative).or_insert(Some(stamp));
        }
    }
    shared.walks.fetch_add(1, Ordering::Relaxed);
    shared.bump();
    loop {
        if shared.rescan.swap(false, Ordering::AcqRel) {
            let listing = walk(&shared.root).await;
            shared.walks.fetch_add(1, Ordering::Relaxed);
            shared.rescans.fetch_add(1, Ordering::Relaxed);
            shared.drain(listing).await;
            shared.bump();
        } else {
            shared.drain(None).await;
        }
        if !shared.rescan.load(Ordering::Acquire) {
            shared.pending.notified().await;
        }
    }
}

/// The sync of one workspace. Dropping it stops the watcher and the
/// forwarding task.
pub struct WorkspaceSync {
    shared: Arc<Shared>,
    task: tokio::task::JoinHandle<()>,
}

impl WorkspaceSync {
    /// Starts watching `root` (an existing directory) and forwarding to
    /// `sink`. Returns at once: the watcher is opened (inotify walks the tree
    /// to do so), with the polling fallback (`poll_interval`), and the first
    /// walk runs, on blocking threads of the forwarding task. Needs a Tokio
    /// runtime.
    pub fn start(root: PathBuf, poll_interval: Duration, sink: Sink) -> Self {
        Self::start_with(root, poll_interval, sink, open_notify)
    }

    fn start_with(root: PathBuf, poll_interval: Duration, sink: Sink, open: Open) -> Self {
        let shared = Arc::new(Shared {
            root,
            sink,
            backend: OnceLock::new(),
            watcher: Mutex::new(None),
            queue: Mutex::new(Vec::new()),
            pending: Notify::new(),
            rescan: AtomicBool::new(false),
            drain: tokio::sync::Mutex::new(()),
            table: Mutex::new(Table::new()),
            events: AtomicU64::new(0),
            batches: AtomicU64::new(0),
            forwarded: AtomicU64::new(0),
            walks: AtomicU64::new(0),
            rescans: AtomicU64::new(0),
            progress: watch::channel(0).0,
        });
        let task = tokio::spawn(run(Arc::clone(&shared), open, poll_interval));
        Self { shared, task }
    }

    /// Sends what the watcher has already reported, as one batch, before
    /// returning. Waits for a drain in progress (and its sends), never for a
    /// walk or the OS.
    pub async fn drain_now(&self) {
        self.shared.drain(None).await;
    }

    pub fn status(&self) -> SyncStatus {
        self.shared.status()
    }

    /// Waits, at most `timeout`, until each of `relatives` (paths relative
    /// to the root, with `/`) was last forwarded as it is on disk now: its
    /// `(mtime, length)`, or its absence. A path the sync never saw counts
    /// as absent. The first walk must be over too, so an empty list waits for
    /// that alone (a file written after it is a change of a known file).
    /// Returns whether that happened in time. For callers that wrote the
    /// files themselves and must know the servers were told.
    pub async fn synced(&self, relatives: &[&str], timeout: Duration) -> bool {
        let mut progress = self.shared.progress.subscribe();
        tokio::time::timeout(timeout, async {
            while self.shared.walks.load(Ordering::Relaxed) == 0
                || !self.shared.is_synced(relatives)
            {
                if progress.changed().await.is_err() {
                    std::future::pending::<()>().await;
                }
            }
        })
        .await
        .is_ok()
    }
}

impl Drop for WorkspaceSync {
    fn drop(&mut self) {
        self.task.abort();
        self.shared.watcher.lock().unwrap().take();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, Flag, ModifyKind, RemoveKind};
    use tokio::sync::mpsc;

    const WAIT: Duration = Duration::from_secs(30);

    fn write(root: &Path, relative: &str, text: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    /// A sink that sends every batch down a channel.
    fn channel_sink() -> (Sink, mpsc::UnboundedReceiver<Vec<FileChange>>) {
        let (sender, receiver) = mpsc::unbounded_channel();
        let sink: Sink = Arc::new(move |batch| {
            let _ = sender.send(batch);
            Box::pin(async {})
        });
        (sink, receiver)
    }

    /// A watcher that reports nothing: the tests feed the events.
    fn open_silent(_: Handler, _: &Path, _: Duration) -> notify::Result<Box<dyn Watcher + Send>> {
        Ok(Box::new(notify::NullWatcher))
    }

    fn open_failing(_: Handler, _: &Path, _: Duration) -> notify::Result<Box<dyn Watcher + Send>> {
        Err(notify::Error::generic("no platform watcher here"))
    }

    fn event(kind: EventKind, root: &Path, relative: &str) -> notify::Result<Event> {
        Ok(Event::new(kind).add_path(root.join(relative)))
    }

    impl WorkspaceSync {
        fn inject(&self, event: notify::Result<Event>) {
            self.shared.push(event);
        }

        async fn wait_until(&self, done: impl Fn(&SyncStatus) -> bool) {
            let mut progress = self.shared.progress.subscribe();
            tokio::time::timeout(WAIT, async {
                while !done(&self.status()) {
                    progress.changed().await.unwrap();
                }
            })
            .await
            .unwrap_or_else(|_| panic!("timed out at {:?}", self.status()));
        }
    }

    fn kinds(batch: &[FileChange]) -> Vec<(&str, ChangeKind)> {
        batch
            .iter()
            .map(|change| (change.relative.as_str(), change.kind))
            .collect()
    }

    #[tokio::test]
    async fn a_rescan_event_walks_once_and_batches_the_diff() {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        write(&root, "src/edited.rs", "fn a() {}\n");
        write(&root, "src/kept.rs", "fn k() {}\n");
        write(&root, "src/removed.rs", "fn r() {}\n");
        let (sink, mut batches) = channel_sink();
        let sync = WorkspaceSync::start_with(root.clone(), WAIT, sink, open_silent);
        sync.wait_until(|status| status.walks == 1).await;
        // Nothing reported, so nothing sent, and no second walk.
        assert_eq!(sync.status().batches, 0);

        write(&root, "src/edited.rs", "fn a() {}\nfn b() {}\n");
        write(&root, "src/added.rs", "fn n() {}\n");
        std::fs::remove_file(root.join("src/removed.rs")).unwrap();
        sync.inject(Ok(Event::new(EventKind::Other).set_flag(Flag::Rescan)));
        sync.wait_until(|status| status.batches == 1).await;
        let batch = batches.recv().await.unwrap();
        assert_eq!(
            kinds(&batch),
            [
                ("src/added.rs", ChangeKind::Created),
                ("src/edited.rs", ChangeKind::Changed),
                ("src/removed.rs", ChangeKind::Deleted),
            ]
        );
        let status = sync.status();
        assert_eq!((status.walks, status.rescans), (2, 1), "{status:?}");
        assert!(batches.try_recv().is_err(), "one batch");
    }

    #[tokio::test]
    async fn a_batch_names_each_path_once_typed_by_the_disk() {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        write(&root, "src/known.rs", "fn k() {}\n");
        let (sink, mut batches) = channel_sink();
        let sync = WorkspaceSync::start_with(root.clone(), WAIT, sink, open_silent);
        sync.wait_until(|status| status.walks == 1).await;

        // A new file reported three times, a known one changed twice, and a
        // file created and removed again before the drain.
        write(&root, "src/new.rs", "fn n() {}\n");
        write(&root, "src/known.rs", "fn k() {}\nfn l() {}\n");
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Any),
            EventKind::Modify(ModifyKind::Any),
        ] {
            sync.inject(event(kind, &root, "src/new.rs"));
        }
        sync.inject(event(
            EventKind::Modify(ModifyKind::Any),
            &root,
            "src/known.rs",
        ));
        sync.inject(event(
            EventKind::Modify(ModifyKind::Any),
            &root,
            "src/known.rs",
        ));
        sync.inject(event(
            EventKind::Create(CreateKind::File),
            &root,
            "src/gone.rs",
        ));
        sync.inject(event(
            EventKind::Remove(RemoveKind::File),
            &root,
            "src/gone.rs",
        ));
        sync.drain_now().await;
        let batch = batches.recv().await.unwrap();
        assert_eq!(
            kinds(&batch),
            [
                ("src/gone.rs", ChangeKind::Deleted),
                ("src/known.rs", ChangeKind::Changed),
                ("src/new.rs", ChangeKind::Created),
            ]
        );
        assert!(
            sync.synced(&["src/new.rs", "src/known.rs", "src/gone.rs"], WAIT)
                .await
        );
        // The new file is known now: its next change is `Changed`.
        write(&root, "src/new.rs", "fn n() {}\nfn m() {}\n");
        assert!(!sync.shared.is_synced(&["src/new.rs"]));
        sync.inject(event(
            EventKind::Modify(ModifyKind::Any),
            &root,
            "src/new.rs",
        ));
        sync.drain_now().await;
        let batch = batches.recv().await.unwrap();
        assert_eq!(kinds(&batch), [("src/new.rs", ChangeKind::Changed)]);
        assert_eq!(sync.status().events, 8);
        assert_eq!(sync.status().walks, 1);
    }

    #[tokio::test]
    async fn non_source_prose_and_git_paths_are_dropped() {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        let (sink, mut batches) = channel_sink();
        let sync = WorkspaceSync::start_with(root.clone(), WAIT, sink, open_silent);
        sync.wait_until(|status| status.walks == 1).await;
        for relative in [
            "notes.txt",
            "README.md",
            ".git/index",
            ".git/hooks/pre-commit.rs",
            "src",
        ] {
            sync.inject(event(EventKind::Modify(ModifyKind::Any), &root, relative));
        }
        sync.inject(Ok(Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(std::env::temp_dir().join("elsewhere.rs"))));
        sync.inject(event(
            EventKind::Access(notify::event::AccessKind::Any),
            &root,
            "src/read.rs",
        ));
        sync.drain_now().await;
        assert_eq!(sync.status().batches, 0, "{:?}", batches.try_recv());
        // The control: a source file in the same drain is sent.
        write(&root, "src/lib.rs", "fn f() {}\n");
        sync.inject(event(
            EventKind::Modify(ModifyKind::Any),
            &root,
            ".git/x.rs",
        ));
        sync.inject(event(
            EventKind::Modify(ModifyKind::Any),
            &root,
            "src/lib.rs",
        ));
        sync.drain_now().await;
        let batch = batches.recv().await.unwrap();
        assert_eq!(kinds(&batch), [("src/lib.rs", ChangeKind::Created)]);
    }

    #[tokio::test]
    async fn the_poll_backend_is_used_when_notify_fails() {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        write(&root, "src/lib.rs", "fn f() {}\n");
        let (sink, mut batches) = channel_sink();
        let sync =
            WorkspaceSync::start_with(root.clone(), Duration::from_millis(50), sink, open_failing);
        sync.wait_until(|status| status.walks == 1).await;
        assert_eq!(sync.status().backend, Some(Backend::Poll));
        // The polling watcher reports an edit by itself. It compares
        // modification times in whole seconds, so the edit gets one that is
        // certainly later than the one the watcher first saw.
        write(&root, "src/lib.rs", "fn f() {}\nfn g() {}\n");
        std::fs::File::options()
            .write(true)
            .open(root.join("src/lib.rs"))
            .unwrap()
            .set_modified(SystemTime::now() + Duration::from_secs(3600))
            .unwrap();
        assert!(
            sync.synced(&["src/lib.rs"], WAIT).await,
            "{:?}",
            sync.status()
        );
        let batch = batches.recv().await.unwrap();
        assert!(
            kinds(&batch).contains(&("src/lib.rs", ChangeKind::Changed)),
            "{batch:?}"
        );
    }

    #[tokio::test]
    async fn the_platform_watcher_reports_an_edit() {
        let temp = tempfile::tempdir().unwrap();
        let root = dunce::canonicalize(temp.path()).unwrap();
        write(&root, "src/lib.rs", "fn f() {}\n");
        let (sink, mut batches) = channel_sink();
        let sync = WorkspaceSync::start(root.clone(), WAIT, sink);
        sync.wait_until(|status| status.walks == 1).await;
        assert_eq!(sync.status().backend, Some(Backend::Notify));
        write(&root, "src/lib.rs", "fn f() {}\nfn g() {}\n");
        assert!(
            sync.synced(&["src/lib.rs"], WAIT).await,
            "{:?}",
            sync.status()
        );
        let batch = batches.recv().await.unwrap();
        assert_eq!(kinds(&batch), [("src/lib.rs", ChangeKind::Changed)]);
        assert_eq!((sync.status().walks, sync.status().rescans), (1, 0));
    }
}
