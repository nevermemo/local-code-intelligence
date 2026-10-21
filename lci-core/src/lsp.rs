use crate::{
    config::{Config, LspSync},
    language,
    lsp::{
        adapter::LspAdapter,
        clangd::ClangdServer,
        csharp::CSharpServer,
        go::GoServer,
        java::JavaServer,
        python::PythonServer,
        rust::RustServer,
        transport::{InactivityTimeout, JsonRpcClient, RpcError},
        typescript::TypeScriptServer,
    },
    models::ConnectBreaker,
    workspace::Workspace,
};
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, watch};

pub mod adapter;
pub mod clangd;
pub mod csharp;
pub mod go;
pub mod java;
#[cfg(windows)]
mod job;
pub mod python;
pub mod rust;
pub mod stamp;
pub mod sync;
pub mod transport;
pub mod typescript;

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Hash)]
pub struct Location {
    pub file_path: PathBuf,
    pub relative_file_path: Option<String>,
    pub start_line: u32,
    pub start_character: u32,
    pub end_line: u32,
    pub end_character: u32,
    pub name: Option<String>,
    pub kind: Option<u64>,
    pub language: Option<String>,
    pub provider: Option<String>,
}

/// Where a language server's session stands, one word of a
/// [`ServerReadiness`] (`snake_case` on the wire).
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ServerStateName {
    /// The session exists but the server has not finished starting.
    Starting,
    /// Started, and not reporting that it is busy.
    Ready,
    /// Started, and its last status notification said it is working again
    /// (see [`LspAdapter::busy_predicate`]).
    Busy,
    /// The start failed.
    Failed,
    /// It has answered nothing but transient errors for `lsp_stuck_seconds`
    /// (see [`Session::stuck`]).
    Stuck,
}

impl ServerStateName {
    /// The precedence: a failed start, then a start that has not finished,
    /// then a stuck mark, then busy, else ready.
    fn classify(failed: bool, started: bool, stuck: bool, busy: bool) -> Self {
        if failed {
            Self::Failed
        } else if !started {
            Self::Starting
        } else if stuck {
            Self::Stuck
        } else if busy {
            Self::Busy
        } else {
            Self::Ready
        }
    }

    /// The word as the wire and the notes spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Busy => "busy",
            Self::Failed => "failed",
            Self::Stuck => "stuck",
        }
    }
}

/// What one session of a language server reports about itself, for the status
/// report and the notes of a search or a traversal. A report only: reading it
/// changes no request, repeat or answer, and does not count as use of the
/// session (the idle sweeper's clock is not touched).
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct ServerReadiness {
    pub state: ServerStateName,
    /// Priming (see [`PRIMING_QUERY`]) has settled.
    pub primed: bool,
    /// `$/progress` notifications the server has sent.
    pub progress: u64,
    /// Notifications that matched the adapter's ready or busy predicate; 0 for
    /// an adapter that has none (a server that never reports it).
    pub ready_changes: u64,
    /// Milliseconds since the session began.
    pub age_ms: u64,
    /// Milliseconds since the last matching notification; `None` when none came.
    pub changed_ms: Option<u64>,
    /// Milliseconds since a caller last asked for the session.
    pub idle_ms: u64,
}

/// A live JSON-RPC session with one spawned, initialized language server.
struct GenericClient {
    rpc: Arc<JsonRpcClient>,
}

impl GenericClient {
    async fn spawn(adapter: &Arc<dyn LspAdapter>, workspace: &Workspace) -> Result<Arc<Self>> {
        let transport = adapter
            .transport_config(&workspace.path)
            .with_context(|| format!("{} is disabled or has no executable", adapter.provider()))?;
        let rpc = JsonRpcClient::spawn_with_busy(
            &transport,
            &adapter.environment(),
            adapter.ready_predicate(),
            adapter.busy_predicate(),
        )
        .await?;
        let params = adapter
            .initialize_params(&workspace.path)
            .with_context(|| format!("cannot build {} initialization", adapter.provider()))?;
        rpc.request("initialize", params)
            .await
            .with_context(|| format!("initialize {}", adapter.provider()))?;
        rpc.notify("initialized", json!({})).await?;
        adapter.after_initialized(&rpc).await?;
        Ok(Arc::new(Self { rpc }))
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        self.rpc.request(method, params).await
    }
}

/// The query of the priming request sent once a server has started: a single
/// common letter in the same `<term>#` form a search sends.
///
/// The request exists only to make the server build the symbol index its
/// first real `workspace/symbol` would otherwise build, which on a large
/// Rust workspace is 8-30 s of work that a search cannot afford to wait for
/// or to have cancelled (rust-analyzer 1.98 builds it on either query form).
/// It is not an empty string because servers differ in what they do with
/// one: some answer it without touching the index, others return every
/// symbol they have. A single common letter has a small answer on every
/// server yet still exercises the index; the `#` suffix keeps the form that
/// searches use (rust-analyzer reads it as "all symbols", other servers see
/// one more ordinary character).
const PRIMING_QUERY: &str = "e#";

fn workspace_symbol_params(query: &str) -> Value {
    json!({"query": query, "searchScope":"workspace", "searchKind":"allSymbols"})
}

type SessionKey = (String, String);

/// The registry of live sessions. The idle sweeper holds a [`Weak`] to it, so
/// it never keeps the sessions (and their child processes) alive.
type Sessions = Mutex<HashMap<SessionKey, Arc<Session>>>;

/// How long each of the two slow steps of a language server's shutdown (the
/// `shutdown` request, the wait for the process to exit) is given before the
/// server is killed, so releasing a session is bounded by about twice this
/// (the `exit` write adds at most 200 ms, the kill is immediate).
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

/// How long a request waits before each repeat on the same server, after the
/// server answered it with a transient error response (see
/// [`send_with_repeats`]). One entry per repeat, so this is also the most
/// repeats: a short first wait for the usual case (the content changed under
/// the request, and the server is ready again at once), a longer second one for
/// a server that is still busy.
const SAME_SERVER_RETRY_WAITS: [Duration; 2] =
    [Duration::from_millis(500), Duration::from_millis(1500)];

/// Bounds when a repeat on the same server may START, measured from the
/// request's first error response: a repeat is not begun unless its wait fits in
/// what is left of the window, and the time is checked again once the wait is
/// over, because a timer can run late. It does not bound a repeat once it is
/// sent: every request keeps its normal timeout (`lsp_timeout_seconds` of
/// inactivity), so a call can take about this long plus the time its last repeat
/// takes. When no repeat may start, the last error is returned to the caller
/// and the session is kept.
const SAME_SERVER_RETRY_WINDOW: Duration = Duration::from_secs(5);

/// When a request may be repeated on the same server. Production uses
/// [`SAME_SERVER_RETRY_WAITS`] and [`SAME_SERVER_RETRY_WINDOW`]; the tests use
/// shorter values.
#[derive(Clone, Copy)]
struct RepeatPolicy {
    waits: &'static [Duration],
    window: Duration,
}

impl RepeatPolicy {
    const SAME_SERVER: Self = Self {
        waits: &SAME_SERVER_RETRY_WAITS,
        window: SAME_SERVER_RETRY_WINDOW,
    };

    /// The wait before repeat number `repeats` (0 for the first), or `None`
    /// when there is no such repeat or its wait does not fit in what is left of
    /// the window, `since_first_error` after the first error response.
    fn wait_before(&self, repeats: usize, since_first_error: Duration) -> Option<Duration> {
        let wait = *self.waits.get(repeats)?;
        (since_first_error + wait <= self.window).then_some(wait)
    }

    /// Whether a repeat whose wait is over may still start.
    fn may_start(&self, since_first_error: Duration) -> bool {
        since_first_error <= self.window
    }
}

/// Whether a server's error response means "ask again", on any server: the
/// protocol's `ContentModified`, `ServerCancelled` and `RequestFailed`. The
/// adapter adds the errors that are transient only for its own server (see
/// [`LspAdapter::is_loading_error`]).
fn is_transient_code(code: i64) -> bool {
    matches!(
        code,
        RpcError::CONTENT_MODIFIED | RpcError::SERVER_CANCELLED | RpcError::REQUEST_FAILED
    )
}

/// Whether an error response means "ask again" for this server: a code that
/// is transient on any server, or one that is only for this adapter (given
/// whether the server has reported ready). The one rule behind both the repeats
/// of [`send_with_repeats`] and the stuck-session check of [`TransientStreak`].
fn is_transient(adapter: &Arc<dyn LspAdapter>, response: &RpcError, ready: bool) -> bool {
    is_transient_code(response.code) || adapter.is_loading_error(response, ready)
}

/// How long a session has answered nothing but transient errors: the time of
/// the first one since the last answer of any other kind (a result, or an error
/// that is not transient) and since the last time the server reported progress
/// (`$/progress`). A server that is still loading answers transient errors for
/// minutes, and a restarted one starts the load over, so only a long span is
/// evidence of a server that will never be ready; a server that keeps sending
/// progress is working, whatever its answers say. The limit is
/// `lsp_stuck_seconds`. Time, not a count of failed requests: a burst of
/// concurrent searches fails many requests within seconds of a perfectly normal
/// load, and a count would replace the server that is about to be ready.
#[derive(Default)]
struct TransientStreak {
    since: Option<Instant>,
    /// The server's progress count when the span began.
    progress: u64,
}

impl TransientStreak {
    /// The server answered with a result or an error that is not transient.
    fn answered(&mut self) {
        self.since = None;
    }

    /// Records a transient answer received at `now`, when the server had sent
    /// `progress` progress notifications in all. A count that differs from the
    /// one at the start of the span starts the span over at `now`. Returns how
    /// long the session has answered nothing else when that is at least
    /// `limit`, which needs at least two such answers, `limit` apart.
    fn transient(&mut self, now: Instant, progress: u64, limit: Duration) -> Option<Duration> {
        if self.since.is_none() || progress != self.progress {
            self.since = Some(now);
            self.progress = progress;
        }
        let span = now.saturating_duration_since(self.since.unwrap_or(now));
        (span >= limit).then_some(span)
    }
}

/// What one server start produced: the ready client, or the rendered cause of
/// the failure (shared by every caller that waited on the same start).
type StartOutcome = Result<Arc<GenericClient>, String>;

/// One (provider, workspace) language-server session, from the moment its
/// start is requested until it is discarded.
///
/// The start (process spawn, `initialize`, and the adapter's warm-up wait --
/// rust-analyzer waits up to ~10 s for its first indexing pass) runs on a
/// detached task, never inside the future of whichever caller asked first:
///
/// - concurrent callers for the same key share the one start instead of each
///   spawning a server;
/// - a caller that goes away (a search past its latency budget) does not
///   cancel the start, so the server keeps warming for the next search.
///   Dropping a start mid-way would kill the child (`kill_on_drop`) and throw
///   the warm-up away;
/// - the finished client is owned by the session (the watch channel's value),
///   not by any request future, so it lives until the session is discarded or
///   the [`Manager`] is dropped. Dropping the manager drops the last
///   receiver, which ends a still-running start (or priming) and kills its
///   child.
///
/// Once the start succeeds the same task *primes* the server (see
/// [`PRIMING_QUERY`]): one `workspace/symbol` request that makes the server
/// build the symbol index its first real query would otherwise build. Priming
/// is never cancelled by a caller's budget, and only `workspace/symbol`
/// callers wait for it ([`Session::primed`]); definition and references are
/// served as soon as the start succeeds.
///
/// A session that nobody has asked for in a while is released: the idle
/// sweeper (see [`Manager::new`]) and [`Manager::release`] remove it from the
/// registry and [`Session::shutdown`] stops its server. The next caller then
/// starts a fresh session.
struct Session {
    /// Unique within the process: tells this session from the one that
    /// replaces it under the same key (see [`OutlineCache`]).
    id: u64,
    start: watch::Receiver<Option<StartOutcome>>,
    /// Flips to `true` when priming has settled, whether it succeeded or
    /// not. Never flips if the start failed.
    primed: watch::Receiver<bool>,
    provider: &'static str,
    /// The workspace's path, for log lines (the registry key holds its id).
    workspace: PathBuf,
    /// When a caller last asked for this session, for the idle sweeper:
    /// set when the session begins and touched by [`Manager::session`] and
    /// [`Manager::client`], so a search and a navigation call both count. The session's own start and
    /// priming never touch it, and neither does [`Manager::running`], so a
    /// status poll does not keep a server alive.
    last_used: std::sync::Mutex<Instant>,
    /// How long this session has answered nothing but transient errors, see
    /// [`TransientStreak`]. Only requests made through [`Manager::send`] count,
    /// not the session's own priming.
    streak: std::sync::Mutex<TransientStreak>,
    /// `lsp_stuck_seconds`: the span of transient errors after which the session
    /// is marked [`Session::stuck`]. `None` never marks it.
    stuck_after: Option<Duration>,
    /// Set once the server has answered nothing but transient errors for
    /// `stuck_after`, and taken back by the next answer that is a result or an
    /// error that is not transient (a server that answers is not wedged, and
    /// replacing it would throw away the load it has just finished). A stuck
    /// session is replaced by the next caller that asks for its key
    /// ([`Manager::session`]), or at once by the call that noticed
    /// ([`Manager::send`]). A flag and not a discard made on the spot, because a
    /// request is cancelled as often as it finishes (a search past its latency
    /// budget drops the call before the repeats end), and a cancelled call must
    /// still count.
    stuck: AtomicBool,
    /// When the session began, for [`ServerReadiness::age_ms`].
    began: Instant,
}

impl Session {
    fn begin(
        adapter: Arc<dyn LspAdapter>,
        workspace: Workspace,
        stuck_after: Option<Duration>,
        breakers: Option<Arc<LspBreakers>>,
    ) -> Arc<Self> {
        let (sender, start) = watch::channel(None);
        let (primed_sender, primed) = watch::channel(false);
        let provider = adapter.provider();
        let workspace_path = workspace.path.clone();
        tokio::spawn(async move {
            let provider = adapter.provider();
            let started = Instant::now();
            let outcome = tokio::select! {
                outcome = GenericClient::spawn(&adapter, &workspace) => outcome,
                // Every receiver is gone: the manager was dropped.
                () = sender.closed() => return,
            };
            let client = match &outcome {
                Ok(client) => {
                    tracing::debug!(
                        provider,
                        workspace = %workspace.path.display(),
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "language server session ready"
                    );
                    Some(Arc::clone(client))
                }
                Err(error) => {
                    tracing::warn!(
                        provider,
                        workspace = %workspace.path.display(),
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        error = %format!("{error:#}"),
                        "language server failed to start"
                    );
                    None
                }
            };
            // Once per start, whatever the number of callers waiting on it.
            if let Some(breakers) = &breakers {
                breakers.note_start(provider, &outcome);
            }
            sender.send_replace(Some(outcome.map_err(|error| format!("{error:#}"))));
            let Some(client) = client else { return };

            let priming = Instant::now();
            let primed_result = tokio::select! {
                result = client.request("workspace/symbol", workspace_symbol_params(PRIMING_QUERY)) => result,
                () = sender.closed() => return,
            };
            match primed_result {
                Ok(_) => tracing::debug!(
                    provider,
                    workspace = %workspace.path.display(),
                    elapsed_ms = priming.elapsed().as_millis() as u64,
                    "language server primed"
                ),
                Err(error) => tracing::warn!(
                    provider,
                    workspace = %workspace.path.display(),
                    elapsed_ms = priming.elapsed().as_millis() as u64,
                    error = %format!("{error:#}"),
                    "priming workspace/symbol request failed; searches will pay for the index build"
                ),
            }
            primed_sender.send_replace(true);
        });
        static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);
        Arc::new(Self {
            id: NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed),
            start,
            primed,
            provider,
            workspace: workspace_path,
            last_used: std::sync::Mutex::new(Instant::now()),
            streak: std::sync::Mutex::new(TransientStreak::default()),
            stuck_after,
            stuck: AtomicBool::new(false),
            began: Instant::now(),
        })
    }

    /// The session's report. Touches nothing: not `last_used`, not the streak.
    fn readiness(&self) -> ServerReadiness {
        let client = self.ready_client();
        let (busy, progress, changes, changed_ms) = match &client {
            Some(client) => {
                let state = client.rpc.state();
                (
                    state.busy,
                    client.rpc.progress_count(),
                    state.changes,
                    (state.changes > 0).then(|| state.changed_at.elapsed().as_millis() as u64),
                )
            }
            None => (false, 0, 0, None),
        };
        ServerReadiness {
            state: ServerStateName::classify(self.failed(), client.is_some(), self.stuck(), busy),
            primed: *self.primed.borrow(),
            progress,
            ready_changes: changes,
            age_ms: self.began.elapsed().as_millis() as u64,
            changed_ms,
            idle_ms: self.idle_for().as_millis() as u64,
        }
    }

    /// Records the outcome of one request sent to this session's `client`, as
    /// soon as the server has answered it (each repeat of a request is one
    /// answer): a result or an error that is not transient ends the
    /// [`TransientStreak`], a transient error extends it (or starts it over
    /// when the server has reported progress since the streak began) and marks
    /// the session stuck once the streak reaches `stuck_after`. A transport failure changes
    /// nothing here: the caller discards the session itself.
    fn note_answer(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        client: &GenericClient,
        method: &str,
        result: &Result<Value>,
    ) {
        let response = match result {
            Ok(_) => return self.answered(),
            Err(error) => match error.downcast_ref::<RpcError>() {
                Some(response) => response,
                None => return,
            },
        };
        if !is_transient(adapter, response, client.rpc.is_ready()) {
            return self.answered();
        }
        let Some(limit) = self.stuck_after else {
            return;
        };
        let span = self.streak.lock().unwrap().transient(
            Instant::now(),
            client.rpc.progress_count(),
            limit,
        );
        if let Some(span) = span
            && !self.stuck.swap(true, Ordering::Relaxed)
        {
            tracing::warn!(
                provider = self.provider,
                workspace = %self.workspace.display(),
                method,
                code = response.code,
                message = %response.message,
                stuck_s = span.as_secs(),
                "language server has answered nothing but transient errors for too long; its session will be replaced"
            );
        }
    }

    /// The server gave a result or an error that is not transient: the span
    /// ends, and a stuck mark that the span had set is taken back.
    fn answered(&self) {
        self.streak.lock().unwrap().answered();
        self.stuck.store(false, Ordering::Relaxed);
    }

    /// Whether the server has answered nothing but transient errors for
    /// `lsp_stuck_seconds`, and has not answered anything else since.
    fn stuck(&self) -> bool {
        self.stuck.load(Ordering::Relaxed)
    }

    /// Records that a caller just asked for this session.
    fn touch(&self) {
        *self.last_used.lock().unwrap() = Instant::now();
    }

    /// How long ago a caller last asked for this session.
    fn idle_for(&self) -> Duration {
        self.last_used.lock().unwrap().elapsed()
    }

    /// Whether the idle sweeper may release this session, called under the
    /// registry lock. All of these must hold:
    ///
    /// - the server has started and priming has settled: a session that is
    ///   still priming is busy building the index its `workspace/symbol`
    ///   callers wait for, and releasing it would fail them (the retry could
    ///   fail the same way at a small idle timeout);
    /// - no call is using it: the registry holds one reference and every
    ///   call in progress holds another (a call gets its reference under the
    ///   same lock), so this cannot release a server between a caller
    ///   finding it and its request;
    /// - nobody has asked for it for `idle`.
    ///
    /// So a session whose start failed (no ready client) is never released
    /// here: it stays in the registry, and keeps its workspace's sync, until
    /// the next caller replaces it, a release or a shutdown.
    fn idle_and_unused(self: &Arc<Self>, idle: Duration) -> bool {
        self.ready_client().is_some()
            && *self.primed.borrow()
            && Arc::strong_count(self) == 1
            && self.idle_for() >= idle
    }

    /// Stops this session's server, bounded by about `2 * grace`. Returns
    /// `false` only when the session has no server to stop because its start
    /// failed. A session that is still starting counts as released (`true`):
    /// there is nothing to shut down yet, so this just lets go of it. Its
    /// process dies when the last `Arc` to the session drops, and a caller
    /// still waiting in [`Session::client`] holds one, so that caller is
    /// served by the removed session's server (the start is not cancelled)
    /// and the next call starts a fresh session.
    ///
    /// The caller removes the session from the registry first, so no new
    /// request can pick it up. A request already in flight on a started
    /// server fails when the server exits and recovers through the manager's
    /// retry-once path with a fresh session.
    async fn shutdown(self: Arc<Self>, grace: Duration) -> bool {
        let Some(client) = self.ready_client() else {
            return !self.failed();
        };
        let exited = client.rpc.shutdown(grace).await;
        tracing::debug!(
            provider = self.provider,
            workspace = %self.workspace.display(),
            exited_on_its_own = exited,
            "language server shut down"
        );
        true
    }

    /// Waits for the start to finish (not for priming). Cancel-safe:
    /// dropping this future only stops waiting.
    async fn client(&self) -> Result<Arc<GenericClient>> {
        let mut start = self.start.clone();
        let outcome = start
            .wait_for(Option::is_some)
            .await
            .map(|started| started.clone())
            .map_err(|_| anyhow!("language server start ended unexpectedly"))?;
        match outcome {
            Some(Ok(client)) => Ok(client),
            Some(Err(message)) => Err(anyhow!(message)),
            None => unreachable!("wait_for returned before the start finished"),
        }
    }

    /// Waits for priming to settle. Cancel-safe, like [`Self::client`].
    async fn primed(&self) -> Result<()> {
        let mut primed = self.primed.clone();
        primed
            .wait_for(|settled| *settled)
            .await
            .map(|_| ())
            .map_err(|_| anyhow!("language server priming ended unexpectedly"))
    }

    fn ready_client(&self) -> Option<Arc<GenericClient>> {
        match &*self.start.borrow() {
            Some(Ok(client)) => Some(Arc::clone(client)),
            _ => None,
        }
    }

    fn failed(&self) -> bool {
        matches!(&*self.start.borrow(), Some(Err(_)))
    }
}

/// The content hash of a file's text: [`crate::workspace::hash`], the
/// function the manifest and the chunk scan key a file on, so every
/// freshness check that compares text (the outline cache here, a document's
/// open text later) agrees with the index about what "the same bytes" are.
pub fn content_hash(text: &str) -> String {
    crate::workspace::hash(text)
}

/// The most outlines one [`Manager`] keeps (`[run_profile.lsp]
/// outline_cache`); a new outline past it empties the cache first.
pub const MAX_CACHED_OUTLINES: usize = 4096;

/// `(provider, workspace id, workspace-relative path)`.
type OutlineKey = (&'static str, String, String);

/// One kept outline: what the session `session` answered for the text whose
/// [`content_hash`] is `hash`.
struct CachedOutline {
    hash: String,
    session: u64,
    symbols: Arc<Vec<Location>>,
}

/// The `textDocument/documentSymbol` answers of a [`Manager`] under
/// `[run_profile.lsp] outline_cache = true`. An entry answers only for the
/// same file, the same content hash and the same server session: an edited
/// file, or a server that was restarted, released or replaced since (and may
/// see the file differently), is asked again. An empty outline is never kept
/// (a loading server answers `[]`, N8), and at most
/// [`MAX_CACHED_OUTLINES`] are: a new one past that clears the cache.
#[derive(Default)]
struct OutlineCache {
    entries: HashMap<OutlineKey, CachedOutline>,
}

impl OutlineCache {
    fn get(&self, key: &OutlineKey, hash: &str, session: u64) -> Option<Arc<Vec<Location>>> {
        self.entries
            .get(key)
            .filter(|kept| kept.hash == hash && kept.session == session)
            .map(|kept| Arc::clone(&kept.symbols))
    }

    /// Keeps `symbols` unless it is empty; returns whether it was kept.
    fn put(&mut self, key: OutlineKey, hash: String, session: u64, symbols: &[Location]) -> bool {
        if symbols.is_empty() {
            return false;
        }
        if self.entries.len() >= MAX_CACHED_OUTLINES && !self.entries.contains_key(&key) {
            self.entries.clear();
        }
        self.entries.insert(
            key,
            CachedOutline {
                hash,
                session,
                symbols: Arc::new(symbols.to_vec()),
            },
        );
        true
    }

    /// Drops every outline of `workspace_id` kept for a path of `changes`
    /// (whichever provider and session answered it): a synced change may be
    /// one the server now sees differently.
    fn forget(&mut self, workspace_id: &str, changes: &[sync::FileChange]) {
        let paths: HashSet<&str> = changes
            .iter()
            .map(|change| change.relative.as_str())
            .collect();
        self.entries.retain(|(_, id, relative), _| {
            id != workspace_id || !paths.contains(relative.as_str())
        });
    }
}

/// The [`content_hash`] of `file` as an outline key, or `None` when it is not
/// to be cached: unreadable, not UTF-8, or larger than
/// [`adapter::MAX_DID_OPEN_BYTES`] (the read stops one byte past the cap).
async fn outline_hash(file: &Path) -> Option<String> {
    use tokio::io::AsyncReadExt;
    let mut bytes = Vec::new();
    tokio::fs::File::open(file)
        .await
        .ok()?
        .take(adapter::MAX_DID_OPEN_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .ok()?;
    if bytes.len() as u64 > adapter::MAX_DID_OPEN_BYTES {
        return None;
    }
    String::from_utf8(bytes)
        .ok()
        .map(|text| content_hash(&text))
}

tokio::task_local! {
    /// The walk whose requests the running task makes (see [`WalkSessions`]).
    static WALK: Arc<WalkSessions>;
}

/// Which sessions one walk used, and whether it started each or found it
/// running (started by an earlier call, or by an overlapping walk that got to
/// the server first). The barrier cannot say: two walks can both pass it
/// before either starts a server. So the manager records it when a request
/// acquires its session ([`Manager::session`]), for the requests made inside
/// [`WalkSessions::scope`]; the walk reads [`Self::reused`] when it ends and
/// labels the answers of a reused server. Requests made outside a scope (the
/// warm start, navigation, the search channel) are not recorded.
#[derive(Default)]
pub struct WalkSessions {
    seen: std::sync::Mutex<Vec<SessionUse>>,
}

/// One session a walk acquired.
struct SessionUse {
    session: u64,
    provider: &'static str,
    /// Found running, not started by this walk.
    reused: bool,
    /// For a reused session: how long its tree stamp had been unchanged when
    /// the walk acquired it; `None` when no stamp was recorded for it.
    unchanged: Option<Duration>,
}

impl WalkSessions {
    /// Runs `future` as the requests of this walk: the sessions it acquires
    /// are recorded here.
    pub async fn scope<F: std::future::Future>(self: &Arc<Self>, future: F) -> F::Output {
        WALK.scope(Arc::clone(self), future).await
    }

    fn note(
        &self,
        session: u64,
        provider: &'static str,
        reused: bool,
        unchanged: Option<Duration>,
    ) {
        let mut seen = self.seen.lock().unwrap();
        if seen.iter().all(|used| used.session != session) {
            seen.push(SessionUse {
                session,
                provider,
                reused,
                unchanged,
            });
        }
    }

    /// The providers whose answers (some of them) came from a session this
    /// walk found running, each with how long that session's tree stamp had
    /// been unchanged (`None`: no stamp was recorded), in first-use order.
    pub fn reused(&self) -> Vec<(&'static str, Option<Duration>)> {
        let mut found: Vec<(&'static str, Option<Duration>)> = Vec::new();
        for used in self.seen.lock().unwrap().iter().filter(|used| used.reused) {
            if found.iter().all(|(provider, _)| *provider != used.provider) {
                found.push((used.provider, used.unchanged));
            }
        }
        found
    }
}

/// How long a provider whose version probe failed, or whose server could not be
/// started, is left alone under `[run_profile.lsp] breaker = true` (see
/// [`LspBreakers`]).
pub const LSP_DOWN_WINDOW: Duration = Duration::from_secs(30);

/// `[run_profile.lsp] breaker`: one skip window per provider. A failed version
/// probe, or a server start that failed for a reason other than the inactivity
/// timeout (the process could not be spawned, exited, or broke the protocol),
/// opens the provider's window; until it has passed, [`Manager::probe_once`]
/// and a new session start are refused at once with an error that says so, so a
/// dead server costs one probe or start per window instead of one per call. A
/// start that merely timed out (a slow `initialize`) opens nothing, a started
/// server is never refused, and a successful probe or start closes the window.
/// Shared by `Arc` between the managers of one service (see
/// [`Manager::with_breakers`]), so a traversal's per-call manager reads the
/// windows the service's manager opened. Time is read from the clock, with
/// [`Self::reset`] and, in tests, `expire` in place of waiting.
pub struct LspBreakers {
    window: Duration,
    by_provider: std::sync::Mutex<HashMap<&'static str, ConnectBreaker>>,
}

impl LspBreakers {
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            by_provider: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn with<R>(&self, provider: &'static str, act: impl FnOnce(&ConnectBreaker) -> R) -> R {
        let mut by_provider = self
            .by_provider
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        act(by_provider
            .entry(provider)
            .or_insert_with(|| ConnectBreaker::new(self.window)))
    }

    /// Time left until `provider` may be probed or started again, or `None`
    /// when it may be now (no window, or it has passed).
    pub fn remaining(&self, provider: &'static str) -> Option<Duration> {
        self.with(provider, |breaker| breaker.remaining(Instant::now()))
    }

    /// Opens `provider`'s window now.
    pub fn trip(&self, provider: &'static str) {
        self.with(provider, |breaker| breaker.trip(Instant::now()));
    }

    /// Closes `provider`'s window.
    pub fn clear(&self, provider: &'static str) {
        self.with(provider, ConnectBreaker::clear);
    }

    /// Closes every window.
    pub fn reset(&self) {
        self.by_provider
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
    }

    /// Ages `provider`'s window past its end, in place of waiting for it. If
    /// the clock has not run for a full window yet it closes the window.
    #[cfg(test)]
    fn expire(&self, provider: &'static str) {
        self.with(provider, |breaker| {
            match Instant::now().checked_sub(self.window) {
                Some(past) => breaker.trip(past),
                None => breaker.clear(),
            }
        });
    }

    /// The refusal for a provider whose window is open, `None` when it is not.
    fn refusal(&self, provider: &'static str) -> Option<anyhow::Error> {
        self.remaining(provider).map(|left| {
            anyhow!(
                "{provider}: skipped for {} s after a failed probe or start (lsp breaker)",
                left.as_secs() + 1
            )
        })
    }

    /// What the end of a server start does to the window: a started server
    /// closes it, a start that timed out ([`InactivityTimeout`]) leaves it
    /// alone (the server may only be slow), any other failure opens it.
    fn note_start<T>(&self, provider: &'static str, outcome: &Result<T>) {
        match outcome {
            Ok(_) => self.clear(provider),
            Err(error) if error.chain().any(|cause| cause.is::<InactivityTimeout>()) => {}
            Err(_) => self.trip(provider),
        }
    }
}

/// Per-workspace, per-provider language-server sessions. Rust (rust-analyzer)
/// and C# (csharp-ls) are always constructed; TypeScript/JavaScript, Python,
/// Go, and Java join the same registry as optional, individually
/// disableable adapters. A disabled or unconfigured adapter simply never
/// produces a session (`transport_config` returns `None`), which surfaces
/// as a clear error from the caller.
///
/// A session is released, its server shut down cleanly (see
/// [`SHUTDOWN_GRACE`]), when nobody has used it for `lsp_idle_timeout_seconds`
/// (`0` keeps it for the life of the manager), on [`Self::release`], and on
/// [`Self::shutdown_all`]. The next call for that workspace starts a new
/// session transparently. A session whose server has answered nothing but
/// transient errors for `lsp_stuck_seconds` is replaced the same way (see
/// [`Manager::send`]).
///
/// Under `[run_profile.lsp] sync = "watch"` the first session of a workspace
/// starts a [`sync::WorkspaceSync`] for it, which sends every source-file
/// change to the workspace's sessions as `workspace/didChangeWatchedFiles`
/// and drops the changed files' kept outlines; every request first drains
/// what the watcher has reported ([`Self::client`]). The sync stops when the
/// workspace's last session goes: [`Self::release`], [`Self::shutdown_all`]
/// or the idle sweeper. A per-call manager ([`Self::without_sync`]) never
/// syncs.
///
/// Lock order: a sync's drain lock, then the session registry (the sink
/// collects the workspace's sessions under it and sends after letting it
/// go), then `syncs` (a `std` mutex, never held across an await). A request
/// takes the registry and lets it go before it drains, so no path holds the
/// registry while it waits for a drain.
pub struct Manager {
    adapters: Vec<Arc<dyn LspAdapter>>,
    sessions: Arc<Sessions>,
    /// `[run_profile.lsp] sync`, `Off` for a per-call manager.
    sync: LspSync,
    /// `watch_poll_milliseconds`: the interval of the polling watcher a sync
    /// falls back to.
    poll_interval: Duration,
    /// The running syncs by workspace id, one per workspace with a session.
    syncs: Arc<Syncs>,
    /// `lsp_stuck_seconds`: how long a session may answer nothing but transient
    /// errors before it is replaced. `None` never replaces it.
    stuck_after: Option<Duration>,
    /// The providers whose binary has answered a version probe (see
    /// [`Self::probe_once`]).
    probed: std::sync::Mutex<HashSet<&'static str>>,
    /// `readiness_timeout_seconds`: the bound of one version probe in an
    /// unbounded warm-up (see [`Self::prime_languages`]).
    probe_timeout: Duration,
    /// The tree stamp each kept session was last checked against (see
    /// [`Self::tree_barrier`]).
    stamps: std::sync::Mutex<HashMap<SessionKey, (stamp::TreeStamp, Instant)>>,
    /// One lock per workspace id: the barrier of a workspace (stamp, compare,
    /// stop, record) runs one at a time, so a barrier that took its stamp
    /// earlier cannot overwrite a newer record or stop a server a later
    /// walk has just started.
    barriers: std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Where the barrier gets the index's file list (`None`: no sharing).
    indexed: std::sync::Mutex<Option<Arc<dyn stamp::IndexedFiles>>>,
    /// The kept outlines, `Some` only under `[run_profile.lsp]
    /// outline_cache = true` (see [`Self::document_symbols`]).
    outlines: Option<Arc<std::sync::Mutex<OutlineCache>>>,
    /// The skip windows, `Some` only under `[run_profile.lsp] breaker = true`
    /// (see [`LspBreakers`]); a per-call manager borrows the service's
    /// ([`Self::with_breakers`]).
    breakers: Option<Arc<LspBreakers>>,
}

/// The running syncs of a [`Manager`], by workspace id.
type Syncs = std::sync::Mutex<HashMap<String, Arc<sync::WorkspaceSync>>>;

/// Stops the syncs of the workspaces that have no session left in
/// `sessions` (called under the registry lock; a sync's drop only signals
/// its watcher and task, it waits for nothing).
fn prune_syncs(sessions: &HashMap<SessionKey, Arc<Session>>, syncs: &Syncs) {
    syncs
        .lock()
        .unwrap()
        .retain(|id, _| sessions.keys().any(|(_, workspace)| workspace == id));
}

const DID_CHANGE_WATCHED_FILES: &str = "workspace/didChangeWatchedFiles";

/// The sink of a workspace's sync: drops the kept outlines of the changed
/// files, then sends the batch to every session of the workspace. A started
/// server gets it at once; a server still starting gets it on a task of its
/// own once it has started (a failed start drops it: the next server walks
/// the disk anyway).
fn forward_to_sessions(
    registry: Weak<Sessions>,
    outlines: Option<Arc<std::sync::Mutex<OutlineCache>>>,
    workspace: Workspace,
) -> sync::Sink {
    Arc::new(move |batch: Vec<sync::FileChange>| {
        let (registry, outlines, workspace) =
            (registry.clone(), outlines.clone(), workspace.clone());
        Box::pin(async move {
            if let Some(cache) = &outlines {
                cache.lock().unwrap().forget(&workspace.id, &batch);
            }
            let Some(registry) = registry.upgrade() else {
                return;
            };
            let sessions: Vec<Arc<Session>> = registry
                .lock()
                .await
                .iter()
                .filter(|((_, id), _)| *id == workspace.id)
                .map(|(_, session)| Arc::clone(session))
                .collect();
            drop(registry);
            let changes: Vec<Value> = batch
                .iter()
                .filter_map(|change| {
                    let uri =
                        reqwest::Url::from_file_path(workspace.path.join(&change.relative)).ok()?;
                    Some(json!({"uri": uri.to_string(), "type": change.kind.code()}))
                })
                .collect();
            let params = json!({ "changes": changes });
            for session in sessions {
                if let Some(client) = session.ready_client() {
                    if let Err(error) = client
                        .rpc
                        .notify(DID_CHANGE_WATCHED_FILES, params.clone())
                        .await
                    {
                        tracing::debug!(
                            provider = session.provider,
                            workspace = %workspace.path.display(),
                            error = %format!("{error:#}"),
                            "language server was not told about file changes"
                        );
                    }
                } else if !session.failed() {
                    let params = params.clone();
                    tokio::spawn(async move {
                        if let Ok(client) = session.client().await {
                            let _ = client.rpc.notify(DID_CHANGE_WATCHED_FILES, params).await;
                        }
                    });
                }
            }
        })
    })
}

impl Manager {
    /// Builds the registry. When `lsp_idle_timeout_seconds` is positive and a
    /// Tokio runtime is running, this also spawns the idle sweeper; without a
    /// runtime only explicit [`Self::release`] frees a session.
    pub fn new(config: &Config) -> Self {
        let timeout = Duration::from_secs(config.lsp_timeout_seconds);
        let adapters: Vec<Arc<dyn LspAdapter>> = vec![
            Arc::new(RustServer::new(config)),
            Arc::new(CSharpServer::new(&config.csharp, timeout)),
            Arc::new(TypeScriptServer::new(&config.typescript, timeout)),
            Arc::new(PythonServer::new(&config.python, timeout)),
            Arc::new(GoServer::new(&config.go, timeout)),
            Arc::new(JavaServer::new(config, timeout)),
            Arc::new(ClangdServer::new(&config.clangd, timeout)),
        ];
        let sessions = Arc::new(Mutex::new(HashMap::new()));
        let syncs = Arc::new(std::sync::Mutex::new(HashMap::new()));
        if config.lsp_idle_timeout_seconds > 0 {
            spawn_idle_sweeper(
                Arc::downgrade(&sessions),
                Arc::downgrade(&syncs),
                Duration::from_secs(config.lsp_idle_timeout_seconds),
            );
        }
        let stuck_after =
            (config.lsp_stuck_seconds > 0).then(|| Duration::from_secs(config.lsp_stuck_seconds));
        Self {
            adapters,
            sessions,
            sync: config.run_profile.lsp.sync,
            poll_interval: Duration::from_millis(config.watch_poll_milliseconds.max(1)),
            syncs,
            stuck_after,
            probed: std::sync::Mutex::new(HashSet::new()),
            probe_timeout: Duration::from_secs(config.readiness_timeout_seconds),
            stamps: std::sync::Mutex::new(HashMap::new()),
            barriers: std::sync::Mutex::new(HashMap::new()),
            indexed: std::sync::Mutex::new(None),
            outlines: config
                .run_profile
                .lsp
                .outline_cache
                .then(|| Arc::new(std::sync::Mutex::new(OutlineCache::default()))),
            breakers: config
                .run_profile
                .lsp
                .breaker
                .then(|| Arc::new(LspBreakers::new(LSP_DOWN_WINDOW))),
        }
    }

    /// The skip windows of `[run_profile.lsp] breaker` (`None` when it is
    /// off), to hand to the per-call managers of a walk.
    pub fn breakers(&self) -> Option<Arc<LspBreakers>> {
        self.breakers.clone()
    }

    /// This manager reading and opening `breakers` in place of its own windows
    /// (`None`: no windows at all), so the managers of one service share them.
    /// Call before the first request.
    #[must_use]
    pub fn with_breakers(mut self, breakers: Option<Arc<LspBreakers>>) -> Self {
        self.breakers = breakers;
        self
    }

    /// This manager with `source` as the index's file list for the tree
    /// barrier ([`Self::tree_barrier`]). Without one the barrier restarts
    /// every session it is asked about.
    #[must_use]
    pub fn with_indexed_files(self, source: Arc<dyn stamp::IndexedFiles>) -> Self {
        *self.indexed.lock().unwrap() = Some(source);
        self
    }

    /// This manager without `[run_profile.lsp] sync`: a per-call manager
    /// starts its servers on the tree as it is and stops them when the call
    /// ends, so it watches nothing. Call before the first request.
    #[must_use]
    pub fn without_sync(mut self) -> Self {
        self.sync = LspSync::Off;
        self
    }

    /// `[run_profile.lsp] sync` as this manager applies it.
    pub fn sync_mode(&self) -> LspSync {
        self.sync
    }

    /// The counters of the workspace's sync; `None` when it has none (sync
    /// off, or no live session).
    pub fn sync_status(&self, workspace_id: &str) -> Option<sync::SyncStatus> {
        self.sync_of(workspace_id).map(|sync| sync.status())
    }

    /// [`sync::WorkspaceSync::synced`] for the workspace: whether each of
    /// `relatives` reached its servers as it is on disk now, within
    /// `timeout`. `false` at once when the workspace has no sync.
    pub async fn synced(&self, workspace_id: &str, relatives: &[&str], timeout: Duration) -> bool {
        match self.sync_of(workspace_id) {
            Some(sync) => sync.synced(relatives, timeout).await,
            None => false,
        }
    }

    fn sync_of(&self, workspace_id: &str) -> Option<Arc<sync::WorkspaceSync>> {
        self.syncs.lock().unwrap().get(workspace_id).cloned()
    }

    /// Starts the workspace's sync under `sync = "watch"` unless it runs.
    /// Called under the registry lock when a session begins, so a release
    /// cannot slip between the two. It only spawns the sync's task: the
    /// watcher is opened (inotify walks the tree to do so) and the first walk
    /// runs on blocking threads of that task, never under this lock.
    fn ensure_sync(&self, workspace: &Workspace) {
        if self.sync != LspSync::Watch {
            return;
        }
        let mut syncs = self.syncs.lock().unwrap();
        if syncs.contains_key(&workspace.id) {
            return;
        }
        let sink = forward_to_sessions(
            Arc::downgrade(&self.sessions),
            self.outlines.clone(),
            workspace.clone(),
        );
        syncs.insert(
            workspace.id.clone(),
            Arc::new(sync::WorkspaceSync::start(
                workspace.path.clone(),
                self.poll_interval,
                sink,
            )),
        );
    }

    /// [`LspAdapter::probe_readiness`] for the adapter, answered from memory
    /// once the probe has succeeded for its provider: the binary is not asked
    /// again for the life of this manager (a binary removed meanwhile is found
    /// when its session fails). A failed probe is remembered nowhere and is
    /// run again by the next caller. Concurrent first callers each probe.
    /// The default probe also keeps a process-wide memo of its own (see
    /// [`adapter::probe_subprocess_version`]), so a second manager in the same
    /// process does not run a binary again that has answered.
    ///
    /// Under `[run_profile.lsp] breaker = true` a failed probe opens the
    /// provider's window (see [`LspBreakers`]) and a caller that finds it open
    /// gets an error that says so, without a probe; a probe that passes closes
    /// it. A provider whose probe has passed is answered from memory before
    /// the window is read.
    pub async fn probe_once(&self, adapter: &Arc<dyn LspAdapter>, timeout: Duration) -> Result<()> {
        let provider = adapter.provider();
        if self.probed.lock().unwrap().contains(provider) {
            return Ok(());
        }
        if let Some(refusal) = self
            .breakers
            .as_ref()
            .and_then(|breakers| breakers.refusal(provider))
        {
            return Err(refusal);
        }
        let probe = adapter.probe_readiness(timeout).await;
        if let Some(breakers) = &self.breakers {
            match &probe {
                Ok(()) => breakers.clear(provider),
                Err(_) => breakers.trip(provider),
            }
        }
        probe?;
        self.probed.lock().unwrap().insert(provider);
        Ok(())
    }

    pub fn adapters(&self) -> &[Arc<dyn LspAdapter>] {
        &self.adapters
    }

    /// Every adapter that can navigate the given Tree-sitter language
    /// identifier (normally at most one, but the registry allows more).
    pub fn adapters_for_language(&self, language: &str) -> Vec<&Arc<dyn LspAdapter>> {
        self.adapters
            .iter()
            .filter(|adapter| adapter.languages().contains(&language))
            .collect()
    }

    fn session_key(adapter: &Arc<dyn LspAdapter>, workspace: &Workspace) -> SessionKey {
        (adapter.provider().to_string(), workspace.id.clone())
    }

    /// The session for this (provider, workspace), starting it if there is
    /// none. At most one start is ever in flight per key: concurrent callers
    /// get the same session. A session whose start already failed is replaced
    /// (a failure is reported to the callers that waited on it, never cached
    /// for later ones), and so is a stuck one (see [`Session::stuck`]), whose
    /// server is stopped on a task of its own like a discarded session's.
    /// Asking for the session counts as using it, recorded
    /// under the registry lock, so the idle sweeper cannot release it between
    /// this caller finding it and the caller's first request.
    ///
    /// Under `[run_profile.lsp] breaker = true`, a new start is refused with an
    /// error (and no session is begun) while the provider's window is open
    /// (see [`LspBreakers`]); a session that is already there and usable is
    /// returned whatever the window says.
    async fn session(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
    ) -> Result<Arc<Session>> {
        let key = Self::session_key(adapter, workspace);
        let (session, reused) = {
            let mut sessions = self.sessions.lock().await;
            if let Some(existing) = sessions.get(&key)
                && !existing.failed()
                && !existing.stuck()
            {
                existing.touch();
                (Arc::clone(existing), true)
            } else {
                if let Some(refusal) = self
                    .breakers
                    .as_ref()
                    .and_then(|breakers| breakers.refusal(adapter.provider()))
                {
                    return Err(refusal);
                }
                let session = Session::begin(
                    Arc::clone(adapter),
                    workspace.clone(),
                    self.stuck_after,
                    self.breakers.clone(),
                );
                if let Some(replaced) = sessions.insert(key.clone(), Arc::clone(&session))
                    && replaced.stuck()
                {
                    tokio::spawn(replaced.shutdown(SHUTDOWN_GRACE));
                }
                self.ensure_sync(workspace);
                (session, false)
            }
        };
        self.note_walk(&key, session.id, session.provider, reused);
        Ok(session)
    }

    /// Tells the walk this request belongs to, if any, that it used session
    /// `session` and whether it found it running (see [`WalkSessions`]). Every
    /// path that answers from a session, or from something a session
    /// produced, calls this: [`Self::session`] for a request, and
    /// [`Self::live_session_id`] for an outline served from the cache.
    fn note_walk(&self, key: &SessionKey, session: u64, provider: &'static str, reused: bool) {
        let _ = WALK.try_with(|walk| {
            let unchanged = reused
                .then(|| {
                    self.stamps
                        .lock()
                        .unwrap()
                        .get(key)
                        .map(|(_, since)| since.elapsed())
                })
                .flatten();
            walk.note(session, provider, reused, unchanged);
        });
    }

    /// Waits for the session's client, and for priming too when
    /// `after_priming` (only `workspace/symbol` callers: the index priming
    /// builds is exactly what their request needs, and sending it earlier
    /// would race the priming request). The returned session identifies which
    /// session to [`Self::discard`] if the client then fails.
    async fn client(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        after_priming: bool,
    ) -> Result<(Arc<Session>, Arc<GenericClient>)> {
        let session = self.session(adapter, workspace).await?;
        let client = session.client().await?;
        if after_priming {
            session.primed().await?;
        }
        // `sync = "watch"`: the changes the watcher has already reported
        // reach the server before this request does.
        if let Some(sync) = self.sync_of(&workspace.id) {
            sync.drain_now().await;
        }
        // The start and priming can take far longer than the idle timeout
        // (a cold rust-analyzer): count the moment the caller finally has its
        // server, so the sweeper cannot release it just before the request.
        session.touch();
        Ok((session, client))
    }

    /// The tree barrier of the kept sessions: for each of `adapters`, takes
    /// the workspace's tree stamp ([`stamp::TreeStamp`], built from the
    /// index's file list and one directory read per stamped directory, with no
    /// recursive walk, before any server is started or reused for the caller)
    /// and compares it with the one recorded when the adapter's session was
    /// last checked. A different stamp (or a session with no recorded stamp,
    /// whose age is unknown) stops that session, so the caller's request
    /// starts a fresh server that reads the disk as `per_call` would; an equal
    /// stamp keeps it. A first sight with no session records the stamp for the
    /// session that starts next. With no file list (no [`stamp::IndexedFiles`]
    /// source, no index snapshot) or a failed stamp (a stat or directory read
    /// that failed for a reason other than "not found") every session of
    /// `adapters` is restarted and nothing is recorded: no sharing without a
    /// stamp.
    ///
    /// The barriers of one workspace run one at a time (the stamp is taken
    /// under the workspace's barrier lock), so a slower barrier cannot record
    /// an older stamp over a newer one, nor stop a server that a later walk
    /// has just started from a newer tree.
    ///
    /// This makes a kept session never older than the tree as of the check,
    /// for any change inside what a stamp covers (see [`stamp`]: the indexed
    /// files, their directories and one level of child directories, the
    /// ancestors, the server's configuration files). Outside that coverage an
    /// edit that keeps a file's length and modification time is not seen, nor
    /// a new file two or more levels below a stamped directory; the caller
    /// labels the answer of a kept session for that reason
    /// ([`WalkSessions::reused`], recorded where the walk acquires its
    /// session, not here: two walks can both pass their barriers before either
    /// starts a server). An edit during the call is seen by the next
    /// barrier, as under `per_call`. It does not depend on the server's own
    /// file watcher or on `sync`.
    pub async fn tree_barrier(
        &self,
        workspace: &Workspace,
        adapters: &[&Arc<dyn LspAdapter>],
    ) -> Vec<&'static str> {
        let mut restarted = Vec::new();
        if adapters.is_empty() {
            return restarted;
        }
        let lock = Arc::clone(
            self.barriers
                .lock()
                .unwrap()
                .entry(workspace.id.clone())
                .or_default(),
        );
        let _serial = lock.lock().await;
        let source = self.indexed.lock().unwrap().clone();
        let listed = match source {
            Some(source) => source.indexed_files(workspace).await,
            None => None,
        };
        let stamps: Vec<Option<stamp::TreeStamp>> = match listed {
            Some(listed) => {
                let servers: Vec<(Vec<&'static str>, &'static [&'static str])> = adapters
                    .iter()
                    .map(|adapter| (adapter.languages().to_vec(), adapter.stamp_manifests()))
                    .collect();
                let root = workspace.path.clone();
                let stamped = tokio::task::spawn_blocking(move || {
                    stamp::indexed_stamps(&root, &listed, &servers)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|stamped| stamped);
                match stamped {
                    Ok(stamps) => stamps.into_iter().map(Some).collect(),
                    Err(error) => {
                        tracing::warn!(
                            workspace = %workspace.path.display(),
                            error = %format!("{error:#}"),
                            "tree stamp failed: the kept sessions are restarted"
                        );
                        vec![None; adapters.len()]
                    }
                }
            }
            None => vec![None; adapters.len()],
        };
        for (adapter, stamp) in adapters.iter().zip(stamps) {
            let key = Self::session_key(adapter, workspace);
            let same = {
                let recorded = self.stamps.lock().unwrap();
                matches!((recorded.get(&key), stamp), (Some((before, _)), Some(now)) if *before == now)
            };
            if same {
                continue;
            }
            // The only await between the decision and the record is the
            // registry lock, which comes before the session is removed: a
            // caller dropped there has changed nothing, and one that got
            // past it records the stamp without yielding.
            let stopped = self.stop_session(&key).await;
            {
                let mut recorded = self.stamps.lock().unwrap();
                match stamp {
                    Some(stamp) => recorded.insert(key, (stamp, Instant::now())),
                    None => recorded.remove(&key),
                };
            }
            if stopped {
                restarted.push(adapter.provider());
            }
        }
        restarted
    }

    /// Removes the session under `key` and stops its server on a task of its
    /// own (like [`Self::discard`]); whether there was one.
    async fn stop_session(&self, key: &SessionKey) -> bool {
        let removed = {
            let mut sessions = self.sessions.lock().await;
            let removed = sessions.remove(key);
            prune_syncs(&sessions, &self.syncs);
            removed
        };
        match removed {
            Some(session) => {
                tokio::spawn(session.shutdown(SHUTDOWN_GRACE));
                true
            }
            None => false,
        }
    }

    /// Releases every session of this workspace: removes it from the
    /// registry, shuts its server down (the sessions side by side, each
    /// bounded by [`SHUTDOWN_GRACE`] twice over), and returns the providers
    /// that had a server to stop. A session whose start failed is dropped
    /// without being listed. Returns an empty list when the workspace has no
    /// session. The next call for this workspace starts a fresh server.
    ///
    /// Only this manager's sessions are touched: the same workspace under
    /// another `Manager` (another process, say) is unaffected.
    pub async fn release(&self, workspace_id: &str) -> Vec<&'static str> {
        self.stamps
            .lock()
            .unwrap()
            .retain(|(_, id), _| id != workspace_id);
        let removed: Vec<Arc<Session>> = {
            let mut sessions = self.sessions.lock().await;
            let removed = sessions
                .extract_if(|(_, id), _| id == workspace_id)
                .map(|(_, session)| session)
                .collect();
            prune_syncs(&sessions, &self.syncs);
            removed
        };
        let mut released = shut_down(removed).await;
        released.sort_unstable();
        released
    }

    /// Releases every session of every workspace, like [`Self::release`]
    /// for each, and returns how many servers were stopped. For service
    /// shutdown: bounded by about `2 * SHUTDOWN_GRACE` however many
    /// sessions there are.
    pub async fn shutdown_all(&self) -> usize {
        self.stamps.lock().unwrap().clear();
        let removed: Vec<Arc<Session>> = {
            let mut sessions = self.sessions.lock().await;
            let removed = sessions.drain().map(|(_, session)| session).collect();
            prune_syncs(&sessions, &self.syncs);
            removed
        };
        shut_down(removed).await.len()
    }

    /// Forgets a failed session so the next caller starts a fresh server, and
    /// stops the failed session's server on a task of its own: the same
    /// bounded `shutdown`, `exit`, kill as a release (and, on Windows, the end
    /// of the server's whole process tree). The task is detached so the
    /// caller's retry starts its fresh server at once instead of after up to
    /// about 4 s of shutdown; if the process is already gone it ends
    /// immediately.
    ///
    /// Only that exact session is removed: with concurrent callers, a second
    /// one reporting the same failure must not remove the replacement the
    /// first one already started (and does not stop a server twice).
    ///
    /// Without the shutdown the dropped client would be the only thing ending
    /// the server, and that reaches only the direct child.
    async fn discard(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        failed: &Arc<Session>,
    ) {
        let key = Self::session_key(adapter, workspace);
        let removed = {
            let mut sessions = self.sessions.lock().await;
            if sessions
                .get(&key)
                .is_some_and(|current| Arc::ptr_eq(current, failed))
            {
                let removed = sessions.remove(&key);
                // The workspace's last session may have gone with it.
                prune_syncs(&sessions, &self.syncs);
                removed
            } else {
                None
            }
        };
        if let Some(session) = removed {
            tokio::spawn(session.shutdown(SHUTDOWN_GRACE));
        }
    }

    /// A request with no per-file preparation step: spawn-or-reuse a client,
    /// and on a transport failure (the server exited, closed a pipe, stopped
    /// reading, went silent past the timeout, or sent an error response that is
    /// not a well-formed error object) discard the session and retry once with
    /// a fresh client (never a retry loop). A server that answers with an error
    /// response is alive and keeps its session, unless it has answered nothing
    /// else for `lsp_stuck_seconds`: see [`Self::send`].
    ///
    /// `after_priming` is true for `workspace/symbol` only (see
    /// [`Self::client`]).
    ///
    /// Cancel-safe: a caller that stops waiting (a search past its latency
    /// budget) drops only its own wait. The session stays in the registry and
    /// keeps starting or warming for the next caller.
    async fn request(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        method: &str,
        params: Value,
        after_priming: bool,
    ) -> Result<Value> {
        let (session, client) = self.client(adapter, workspace, after_priming).await?;
        match self
            .send(adapter, workspace, &session, &client, method, &params)
            .await
        {
            Ok(value) => Ok(value),
            Err(first) if first.downcast_ref::<RpcError>().is_some() => Err(first),
            Err(first) => {
                Self::warn_restart(adapter, workspace, method, &first);
                self.discard(adapter, workspace, &session).await;
                let (session, client) = self
                    .client(adapter, workspace, after_priming)
                    .await
                    .with_context(|| format!("{} restart after: {first:#}", adapter.provider()))?;
                self.send(adapter, workspace, &session, &client, method, &params)
                    .await
            }
        }
    }

    /// Sends one request on `client` (the client of `session`), repeating it on
    /// the same server while the server keeps answering with a transient error
    /// response (see [`send_with_repeats`] for the rules). The caller tells an
    /// answer from a transport failure by the [`RpcError`] in the error.
    ///
    /// Every answer, each repeat's too, updates the session's
    /// [`TransientStreak`] as it arrives (see [`Session::note_answer`]). When
    /// the server has answered nothing but transient errors for
    /// `lsp_stuck_seconds` it is wedged, not loading: this call, if it is still
    /// waiting, discards the session like a transport failure but without a
    /// retry (it still returns the server's own error), and otherwise the next
    /// caller for the key replaces it ([`Self::session`]). Repeating on a fresh
    /// server would only hand this caller another cold start. A server that
    /// answers with a result after the span has passed is not wedged after all:
    /// the result takes the mark back and the session is kept.
    async fn send(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        session: &Arc<Session>,
        client: &GenericClient,
        method: &str,
        params: &Value,
    ) -> Result<Value> {
        let result = send_with_repeats(
            &RepeatPolicy::SAME_SERVER,
            tokio::time::sleep,
            adapter,
            workspace,
            method,
            move || client.rpc.is_ready(),
            move || async move {
                let result = client.request(method, params.clone()).await;
                session.note_answer(adapter, client, method, &result);
                result
            },
        )
        .await;
        if session.stuck() {
            self.discard(adapter, workspace, session).await;
        }
        result
    }

    /// Same retry contract as [`Self::request`], but runs the adapter's
    /// `before_position_request` hook (e.g. TypeScript's `textDocument/
    /// didOpen`) against the client before the request on each client (the
    /// repeats of [`Self::send`] on one server do not run it again).
    async fn position_request(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        file: &Path,
        relative: &str,
        method: &str,
        params: Value,
    ) -> Result<Value> {
        self.position_request_on(adapter, workspace, file, relative, method, params)
            .await
            .map(|(value, _)| value)
    }

    /// [`Self::position_request`], with the id of the session whose server
    /// gave the answer (the restarted one when the first attempt failed).
    async fn position_request_on(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        file: &Path,
        relative: &str,
        method: &str,
        params: Value,
    ) -> Result<(Value, u64)> {
        let attempt = async {
            let (session, client) = match self.client(adapter, workspace, false).await {
                Ok(started) => started,
                // The start itself failed: nothing to discard, the failed
                // session is replaced by the retry below.
                Err(error) => return Err((None, error)),
            };
            let result = async {
                adapter
                    .before_position_request(&client.rpc, file, relative)
                    .await?;
                self.send(adapter, workspace, &session, &client, method, &params)
                    .await
            }
            .await;
            let id = session.id;
            result
                .map(|value| (value, id))
                .map_err(|error| (Some(session), error))
        };
        match attempt.await {
            Ok(answered) => Ok(answered),
            Err((_, first)) if first.downcast_ref::<RpcError>().is_some() => Err(first),
            Err((session, first)) => {
                Self::warn_restart(adapter, workspace, method, &first);
                if let Some(session) = session {
                    self.discard(adapter, workspace, &session).await;
                }
                let (session, client) = self
                    .client(adapter, workspace, false)
                    .await
                    .with_context(|| format!("{} restart after: {first:#}", adapter.provider()))?;
                adapter
                    .before_position_request(&client.rpc, file, relative)
                    .await?;
                self.send(adapter, workspace, &session, &client, method, &params)
                    .await
                    .map(|value| (value, session.id))
            }
        }
    }

    /// A transport failure throws away a warmed-up server, and the retry pays
    /// the whole start (and warm-up) again, so it is worth a log line: it is
    /// the likeliest cause of a sudden multi-second search. The error is not a
    /// server's error response (those keep the session and are logged by
    /// [`Self::send`]), so it carries no code; its text says what failed.
    fn warn_restart(
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        method: &str,
        error: &anyhow::Error,
    ) {
        tracing::warn!(
            provider = adapter.provider(),
            workspace = %workspace.path.display(),
            method,
            error = %format!("{error:#}"),
            "language server request failed; discarding the session and restarting once"
        );
    }

    pub async fn symbols(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        query: &str,
    ) -> Result<Vec<Location>> {
        let value = self
            .request(
                adapter,
                workspace,
                "workspace/symbol",
                workspace_symbol_params(query),
                true,
            )
            .await?;
        locations_for(value, workspace, adapter.provider())
    }

    pub async fn definition(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        file: &Path,
        relative: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>> {
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        let value = self
            .position_request(
                adapter,
                workspace,
                file,
                relative,
                "textDocument/definition",
                json!({"textDocument":{"uri":uri}, "position":{"line":line,"character":character}}),
            )
            .await?;
        locations_for(value, workspace, adapter.provider())
    }

    /// `textDocument/implementation` at a position: where the member (or
    /// type) named there is implemented. Same contract as [`Self::definition`].
    pub async fn implementation(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        file: &Path,
        relative: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<Location>> {
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        let value = self
            .position_request(
                adapter,
                workspace,
                file,
                relative,
                "textDocument/implementation",
                json!({"textDocument":{"uri":uri}, "position":{"line":line,"character":character}}),
            )
            .await?;
        locations_for(value, workspace, adapter.provider())
    }

    /// `textDocument/prepareCallHierarchy` at a position: the call-hierarchy
    /// items of the symbol named there (none when the position names no
    /// callable). Same contract as [`Self::definition`]. A server that lacks
    /// the capability answers an error, returned as the [`RpcError`].
    pub async fn prepare_call_hierarchy(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        file: &Path,
        relative: &str,
        line: u32,
        character: u32,
    ) -> Result<Vec<HierarchyItem>> {
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        let value = self
            .position_request(
                adapter,
                workspace,
                file,
                relative,
                "textDocument/prepareCallHierarchy",
                json!({"textDocument":{"uri":uri}, "position":{"line":line,"character":character}}),
            )
            .await?;
        hierarchy_items(&value, workspace, adapter.provider())
    }

    /// `callHierarchy/incomingCalls` of an item [`Self::prepare_call_hierarchy`]
    /// gave (its [`HierarchyItem::raw`], sent back verbatim): the callers, each
    /// with the places of its calls (in the caller's file).
    pub async fn incoming_calls(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        item: &Value,
    ) -> Result<Vec<HierarchyCall>> {
        let value = self
            .request(
                adapter,
                workspace,
                "callHierarchy/incomingCalls",
                json!({"item": item}),
                false,
            )
            .await?;
        hierarchy_calls(&value, "from", None, workspace, adapter.provider())
    }

    /// `callHierarchy/outgoingCalls` of an item [`Self::prepare_call_hierarchy`]
    /// gave (its [`HierarchyItem::raw`]): the callees, each with the places of
    /// the calls (in the asked item's file, as the protocol says).
    pub async fn outgoing_calls(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        item: &Value,
    ) -> Result<Vec<HierarchyCall>> {
        let value = self
            .request(
                adapter,
                workspace,
                "callHierarchy/outgoingCalls",
                json!({"item": item}),
                false,
            )
            .await?;
        let uri = item.get("uri").and_then(Value::as_str);
        hierarchy_calls(&value, "to", uri, workspace, adapter.provider())
    }

    // One more parameter than `definition` (`include_declaration`), which is
    // enough to cross clippy's default threshold; a parameter struct would
    // add indirection for one caller (`app.rs`) without real benefit here.
    #[allow(clippy::too_many_arguments)]
    pub async fn references(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        file: &Path,
        relative: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<Vec<Location>> {
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        let value = self
            .position_request(
                adapter,
                workspace,
                file,
                relative,
                "textDocument/references",
                json!({
                    "textDocument":{"uri":uri}, "position":{"line":line,"character":character},
                    "context":{"includeDeclaration":include_declaration}
                }),
            )
            .await?;
        locations_for(value, workspace, adapter.provider())
    }

    /// `textDocument/documentSymbol` for `file`: every symbol the server lists
    /// (nested ones included, whichever of the two response shapes it uses),
    /// as the location of its name.
    ///
    /// Under `[run_profile.lsp] outline_cache = true`, and only for an
    /// adapter whose server is sent the file's current text before every
    /// position request ([`LspAdapter::opens_documents`]), the file is read
    /// and hashed ([`content_hash`]) first, and an outline kept for the same
    /// file, hash and live session is returned without a request (see
    /// [`OutlineCache`]). Otherwise the server is asked, and a non-empty
    /// answer is kept when the session that gave it is still the live one and
    /// the file's hash is still the one read before the request (an edit
    /// during the request keeps nothing): the answer is then about the text
    /// with that hash. A server that is not sent the text (rust-analyzer, which
    /// watches the disk itself and can lag an edit; csharp-ls; the
    /// TypeScript server, which refuses a second `didOpen`) is asked every
    /// time, as without the key.
    pub async fn document_symbols(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        file: &Path,
        relative: &str,
    ) -> Result<Vec<Location>> {
        let cache = match &self.outlines {
            Some(cache) if adapter.opens_documents() => cache,
            _ => {
                return self
                    .ask_document_symbols(adapter, workspace, file, relative)
                    .await
                    .map(|(found, _)| found);
            }
        };
        let Some(hash) = outline_hash(file).await else {
            return self
                .ask_document_symbols(adapter, workspace, file, relative)
                .await
                .map(|(found, _)| found);
        };
        let key = (
            adapter.provider(),
            workspace.id.clone(),
            relative.to_owned(),
        );
        // `sync = "watch"`: a change already reported drops its outlines
        // before they can answer.
        if let Some(sync) = self.sync_of(&workspace.id) {
            sync.drain_now().await;
        }
        if let Some(session) = self.live_session_id(adapter, workspace).await {
            let kept = cache.lock().unwrap().get(&key, &hash, session);
            if let Some(kept) = kept {
                return Ok(kept.as_ref().clone());
            }
        }
        let (found, answered_by) = self
            .ask_document_symbols(adapter, workspace, file, relative)
            .await?;
        if !found.is_empty()
            && self.live_session_id(adapter, workspace).await == Some(answered_by)
            && outline_hash(file).await.as_deref() == Some(hash.as_str())
        {
            cache.lock().unwrap().put(key, hash, answered_by, &found);
        }
        Ok(found)
    }

    /// The id of the started session for this provider and workspace, counted
    /// as a use of it (like [`Self::session`]); `None` when there is none, or
    /// it is still starting, failed or stuck. The walk this call belongs to, if
    /// any, is told it used that running session: a cached outline answers from
    /// what this session produced, so it is a kept-session answer unless the
    /// walk started the session itself (its earlier [`Self::session`] call
    /// recorded that first, and the first record stands).
    async fn live_session_id(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
    ) -> Option<u64> {
        let key = Self::session_key(adapter, workspace);
        let (id, provider) = {
            let sessions = self.sessions.lock().await;
            let session = sessions.get(&key)?;
            if session.stuck() || session.ready_client().is_none() {
                return None;
            }
            session.touch();
            (session.id, session.provider)
        };
        self.note_walk(&key, id, provider, true);
        Some(id)
    }

    /// [`Self::document_symbols`] without the cache: one request, with the id
    /// of the session that answered it.
    async fn ask_document_symbols(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        file: &Path,
        relative: &str,
    ) -> Result<(Vec<Location>, u64)> {
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        let (value, session) = self
            .position_request_on(
                adapter,
                workspace,
                file,
                relative,
                "textDocument/documentSymbol",
                json!({"textDocument":{"uri":uri}}),
            )
            .await?;
        let found = locations_for(
            Value::Array(flatten_document_symbols(&value, &uri)),
            workspace,
            adapter.provider(),
        )?;
        Ok((found, session))
    }

    /// Whether a started, ready session exists for this provider and
    /// workspace. A session still starting (or warming up) is not running
    /// yet.
    pub async fn running(&self, provider: &str, workspace_id: &str) -> bool {
        self.sessions
            .lock()
            .await
            .get(&(provider.to_string(), workspace_id.to_string()))
            .is_some_and(|session| session.ready_client().is_some())
    }

    /// What the session of this provider and workspace reports about itself:
    /// `None` when there is no session (never started, released, or replaced
    /// after a failure). Like [`running`](Self::running) it does not count as
    /// use of the session, so a status poll keeps no server alive.
    pub async fn readiness(&self, provider: &str, workspace_id: &str) -> Option<ServerReadiness> {
        self.sessions
            .lock()
            .await
            .get(&(provider.to_string(), workspace_id.to_string()))
            .map(|session| session.readiness())
    }

    /// Starts the server of every enabled adapter for `workspace` and waits
    /// until each has finished priming, so that a later `workspace/symbol`
    /// request does not pay the start. Returns, per provider, the time the
    /// warm-up took or why it did not happen: a binary that fails its version
    /// probe, a server that fails to start, or one that is not primed within
    /// `timeout` (the whole call is bounded by it; a server still starting
    /// then keeps starting in its session). A disabled adapter is not started
    /// and is absent from the map. Never panics.
    ///
    /// The sessions are the ones searches use, so this changes which request
    /// pays for the start, never an answer.
    pub async fn prime(
        &self,
        workspace: &Workspace,
        timeout: Duration,
    ) -> BTreeMap<&'static str, Result<Duration>> {
        self.prime_languages(workspace, Some(timeout), None).await
    }

    /// [`Self::prime`] for the adapters that serve one of `languages` (the
    /// language identifiers of an index, for example from the store); `None`
    /// primes every enabled adapter. An adapter for a language the workspace
    /// does not hold is neither probed nor started, and is absent from the map.
    ///
    /// `timeout` `Some`: the whole call is bounded by it, as in [`Self::prime`].
    /// `None`: nothing but the caller's own cancellation (dropping or aborting
    /// the future) ends the wait for a server to start and prime, which is for
    /// a background warm-up that has an abort. Each version probe is still
    /// bounded by `readiness_timeout_seconds`, so a binary whose `--version`
    /// never exits is reported and the next adapter goes on.
    pub async fn prime_languages(
        &self,
        workspace: &Workspace,
        timeout: Option<Duration>,
        languages: Option<&HashSet<String>>,
    ) -> BTreeMap<&'static str, Result<Duration>> {
        let deadline = timeout.map(|timeout| tokio::time::Instant::now() + timeout);
        let mut outcomes = BTreeMap::new();
        let mut starting = Vec::new();
        let wanted = |adapter: &&Arc<dyn LspAdapter>| {
            adapter.enabled()
                && languages.is_none_or(|held| {
                    adapter
                        .languages()
                        .iter()
                        .any(|language| held.contains(*language))
                })
        };
        for adapter in self.adapters.iter().filter(wanted) {
            let started = Instant::now();
            // Under a timeout every probe shares the one deadline, so hanging
            // probes cost `timeout` in total, not each.
            let probe = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    tokio::time::timeout(remaining, self.probe_once(adapter, remaining)).await
                }
                None => Ok(self.probe_once(adapter, self.probe_timeout).await),
            };
            match probe {
                Ok(Ok(())) => {
                    // Begin every start before waiting for any, so the servers
                    // warm side by side.
                    match self.session(adapter, workspace).await {
                        Ok(session) => {
                            drop(session);
                            starting.push((adapter, started));
                        }
                        // The provider's window is open (`breaker`).
                        Err(error) => {
                            outcomes.insert(adapter.provider(), Err(error));
                        }
                    }
                }
                Ok(Err(error)) => {
                    outcomes.insert(adapter.provider(), Err(error));
                }
                Err(_) => {
                    outcomes.insert(
                        adapter.provider(),
                        Err(anyhow!(
                            "the version probe did not answer within {:?}",
                            timeout.unwrap_or(self.probe_timeout)
                        )),
                    );
                }
            }
        }
        for (adapter, started) in starting {
            let primed = self.client(adapter, workspace, true);
            let outcome = match deadline {
                Some(deadline) => {
                    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                    match tokio::time::timeout(remaining, primed).await {
                        Ok(Ok(_)) => Ok(started.elapsed()),
                        Ok(Err(error)) => Err(error),
                        Err(_) => Err(anyhow!(
                            "not started and primed within {:?}; it keeps starting in the background",
                            timeout.unwrap_or_default()
                        )),
                    }
                }
                None => primed.await.map(|_| started.elapsed()),
            };
            outcomes.insert(adapter.provider(), outcome);
        }
        outcomes
    }
}

/// Runs `attempt` (one request on one server). A server that answers with a
/// transient error response, one that says "ask again" (see
/// [`is_transient_code`] and [`LspAdapter::is_loading_error`], the latter given
/// `is_ready()`), gets the request again on the same server: after each of
/// `policy.waits`, and only while the repeat can start within `policy.window` of
/// the first error response (decided before the wait, so a wait that cannot fit
/// is not slept, and again after it). `sleep` is `tokio::time::sleep`, a
/// parameter so that a test can make the timer run late.
///
/// Any other error, or a transient one that outlasts the repeats, is returned
/// as it came with the session left alone: a server that answers is not dead,
/// and replacing a loading one with a freshly started (so equally loading) one
/// only costs another cold start. (A server that answers nothing else for
/// `lsp_stuck_seconds` is replaced by [`Manager::send`].) A failure that is not an error response (the
/// process exited, a pipe closed, the timeout) is returned at once, unrepeated:
/// that is the caller's cue to restart the server.
async fn send_with_repeats<A, S>(
    policy: &RepeatPolicy,
    sleep: impl Fn(Duration) -> S,
    adapter: &Arc<dyn LspAdapter>,
    workspace: &Workspace,
    method: &str,
    is_ready: impl Fn() -> bool,
    mut attempt: impl FnMut() -> A,
) -> Result<Value>
where
    A: Future<Output = Result<Value>>,
    S: Future<Output = ()>,
{
    let mut first_error = None;
    let mut repeats = 0;
    loop {
        let error = match attempt().await {
            Ok(value) => return Ok(value),
            Err(error) => error,
        };
        let Some(response) = error.downcast_ref::<RpcError>() else {
            return Err(error);
        };
        let first_error_at: Instant = *first_error.get_or_insert_with(Instant::now);
        let transient = is_transient(adapter, response, is_ready());
        let wait = policy
            .wait_before(repeats, first_error_at.elapsed())
            .filter(|_| transient);
        let Some(wait) = wait else {
            log_final_answer(adapter, workspace, method, response, transient, repeats);
            return Err(error);
        };
        tracing::debug!(
            provider = adapter.provider(),
            workspace = %workspace.path.display(),
            method,
            code = response.code,
            message = %response.message,
            wait_ms = wait.as_millis() as u64,
            "language server answered with a transient error; repeating the request on the same session"
        );
        repeats += 1;
        sleep(wait).await;
        if !policy.may_start(first_error_at.elapsed()) {
            log_final_answer(adapter, workspace, method, response, transient, repeats);
            return Err(error);
        }
    }
}

/// Logs the error response that ends a request's repeats and is returned to
/// the caller: a `warn` when it was transient (the server never got ready in
/// time), otherwise a `debug` (an ordinary answer such as "No Project").
fn log_final_answer(
    adapter: &Arc<dyn LspAdapter>,
    workspace: &Workspace,
    method: &str,
    response: &RpcError,
    transient: bool,
    repeats: usize,
) {
    if transient {
        tracing::warn!(
            provider = adapter.provider(),
            workspace = %workspace.path.display(),
            method,
            code = response.code,
            message = %response.message,
            repeats,
            "language server kept answering with a transient error; returning it and keeping the session"
        );
    } else {
        tracing::debug!(
            provider = adapter.provider(),
            workspace = %workspace.path.display(),
            method,
            code = response.code,
            message = %response.message,
            "language server answered with an error; keeping the session"
        );
    }
}

/// Shuts the servers of already-removed sessions down side by side and
/// returns the providers that had a server to stop. The tasks are detached:
/// dropping this future only stops waiting, so shutdowns finish even when
/// another caller still holds a session or its client.
async fn shut_down(removed: Vec<Arc<Session>>) -> Vec<&'static str> {
    let mut shutdowns = Vec::new();
    for session in removed {
        let provider = session.provider;
        shutdowns.push(tokio::spawn(async move {
            (provider, session.shutdown(SHUTDOWN_GRACE).await)
        }));
    }
    let mut stopped = Vec::new();
    for shutdown in shutdowns {
        if let Ok((provider, true)) = shutdown.await {
            stopped.push(provider);
        }
    }
    stopped
}

/// Spawns the task that releases sessions idle for `idle` or longer. It
/// wakes every quarter of `idle` (at least 1 s, at most 30 s apart), removes
/// the sessions that [`Session::idle_and_unused`] accepts (started, primed,
/// not in use, and nobody has asked for them for `idle`), and shuts them down
/// outside the registry lock. It holds only a [`Weak`] reference to the registry and
/// ends at its next wake-up once the [`Manager`] is dropped. Without a Tokio
/// runtime nothing is spawned.
fn spawn_idle_sweeper(registry: Weak<Sessions>, syncs: Weak<Syncs>, idle: Duration) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        tracing::debug!("no async runtime; idle language servers are released only on request");
        return;
    };
    let tick = (idle / 4).clamp(Duration::from_secs(1), Duration::from_secs(30));
    runtime.spawn(async move {
        loop {
            tokio::time::sleep(tick).await;
            let Some(sessions) = registry.upgrade() else {
                return;
            };
            let expired: Vec<Arc<Session>> = {
                let mut guard = sessions.lock().await;
                let expired = guard
                    .extract_if(|_, session| session.idle_and_unused(idle))
                    .map(|(_, session)| session)
                    .collect();
                // A workspace whose last session went here stops being
                // watched.
                if let Some(syncs) = syncs.upgrade() {
                    prune_syncs(&guard, &syncs);
                }
                expired
            };
            // The shutdowns take seconds: do not keep the registry alive
            // (or locked) meanwhile.
            drop(sessions);
            if expired.is_empty() {
                continue;
            }
            let released: Vec<(&'static str, PathBuf, u64)> = expired
                .iter()
                .map(|session| {
                    (
                        session.provider,
                        session.workspace.clone(),
                        session.idle_for().as_secs(),
                    )
                })
                .collect();
            shut_down(expired).await;
            for (provider, workspace, idle_s) in released {
                tracing::info!(
                    provider,
                    workspace = %workspace.display(),
                    idle_s,
                    "language server released after idle"
                );
            }
        }
    });
}

/// The items of a `textDocument/documentSymbol` response as the flat
/// `SymbolInformation` shape `locations_for` reads: a `DocumentSymbol` has no
/// uri (it is the requested file's): its range starts at the name
/// (`selectionRange`) and ends where the item does (`range`), so the last line
/// of a `Location` is the item's, not the name's. A flat item is unchanged
/// (its range is the item). Children follow their parent.
fn flatten_document_symbols(value: &Value, uri: &str) -> Vec<Value> {
    let mut out = Vec::new();
    for item in value.as_array().into_iter().flatten() {
        if item.get("location").is_some() {
            out.push(item.clone());
            continue;
        }
        let range = match (item.get("selectionRange"), item.get("range")) {
            (Some(name), Some(whole)) => {
                // An item that ends before its name (a malformed answer) ends
                // with the name.
                let end_line = |range: &Value| range["end"]["line"].as_u64();
                let end = if end_line(whole) >= end_line(name) {
                    &whole["end"]
                } else {
                    &name["end"]
                };
                Some(json!({"start": name["start"], "end": end}))
            }
            (Some(range), None) | (None, Some(range)) => Some(range.clone()),
            (None, None) => None,
        };
        if let Some(range) = range {
            out.push(json!({
                "name": item.get("name"), "kind": item.get("kind"),
                "location": {"uri": uri, "range": range}
            }));
        }
        if let Some(children) = item.get("children") {
            out.extend(flatten_document_symbols(children, uri));
        }
    }
    out
}

/// Normalizes an LSP `workspace/symbol`/`textDocument/definition`/
/// `textDocument/references` response into this app's `Location` shape.
/// Each location's `language` is derived from its own file (not fixed to one
/// value), since a single TypeScript-adapter response can legitimately span
/// `.ts`/`.tsx`/`.js`/`.jsx` results.
fn locations_for(value: Value, workspace: &Workspace, provider: &str) -> Result<Vec<Location>> {
    let values = if value.is_null() {
        vec![]
    } else if let Some(items) = value.as_array() {
        items.clone()
    } else {
        vec![value]
    };
    let mut output = Vec::new();
    for item in values {
        let location = item.get("location").unwrap_or(&item);
        let Some(uri) = location
            .get("uri")
            .or_else(|| item.get("targetUri"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let range = location
            .get("range")
            .or_else(|| item.get("range"))
            .or_else(|| item.get("targetSelectionRange"))
            .or_else(|| item.get("targetRange"));
        let Some(range) = range else { continue };
        let Ok(url) = reqwest::Url::parse(uri) else {
            continue;
        };
        let Ok(file_path) = url.to_file_path() else {
            continue;
        };
        let Some(start_line) = range["start"]["line"]
            .as_u64()
            .and_then(|line| u32::try_from(line).ok())
            .and_then(|line| line.checked_add(1))
        else {
            continue;
        };
        let Some(start_character) = range["start"]["character"]
            .as_u64()
            .and_then(|character| u32::try_from(character).ok())
        else {
            continue;
        };
        let Some(end_line) = range["end"]["line"]
            .as_u64()
            .and_then(|line| u32::try_from(line).ok())
            .and_then(|line| line.checked_add(1))
        else {
            continue;
        };
        let Some(end_character) = range["end"]["character"]
            .as_u64()
            .and_then(|character| u32::try_from(character).ok())
        else {
            continue;
        };
        let relative_file_path = file_path
            .strip_prefix(&workspace.path)
            .ok()
            .and_then(Path::to_str)
            .map(crate::path::normalize_path_separators);
        let Some(relative_file_path) = relative_file_path else {
            continue;
        };
        let detected_language = language::for_path(Path::new(&relative_file_path))
            .map(|adapter| adapter.identifier().to_string());
        output.push(Location {
            file_path,
            relative_file_path: Some(relative_file_path),
            start_line,
            start_character,
            end_line,
            end_character,
            name: item.get("name").and_then(Value::as_str).map(str::to_owned),
            kind: item.get("kind").and_then(Value::as_u64),
            language: detected_language,
            provider: Some(provider.to_string()),
        });
    }
    Ok(output)
}

/// A call-hierarchy item: where the callable's name is, and the server's own
/// item.
#[derive(Debug, Clone, PartialEq)]
pub struct HierarchyItem {
    /// The place of the name (the item's `selectionRange`, else its `range`).
    pub location: Location,
    /// The item as the server sent it. Its `data` is opaque by the protocol,
    /// so a follow-up request sends the item back verbatim.
    pub raw: Value,
}

/// One caller (`callHierarchy/incomingCalls`) or callee (`outgoingCalls`) of
/// an item, with the places of the calls.
#[derive(Debug, Clone, PartialEq)]
pub struct HierarchyCall {
    /// The caller (incoming) or the callee (outgoing).
    pub item: HierarchyItem,
    /// Where the calls are: in the caller's file for an incoming call, in the
    /// asked item's file for an outgoing one.
    pub from_ranges: Vec<Location>,
}

/// The items of a call-hierarchy response (a null answer is empty). Each is
/// read as a `SymbolInformation` whose location is the item's
/// `selectionRange`, not its `range` (the whole definition, which
/// `locations_for` would take from the item as it is); an item outside the
/// workspace or with no readable place is left out.
fn hierarchy_items(
    value: &Value,
    workspace: &Workspace,
    provider: &str,
) -> Result<Vec<HierarchyItem>> {
    let items: Vec<&Value> = match value {
        Value::Null => Vec::new(),
        Value::Array(items) => items.iter().collect(),
        item => vec![item],
    };
    let mut out = Vec::new();
    for item in items {
        let range = item.get("selectionRange").or_else(|| item.get("range"));
        let (Some(uri), Some(range)) = (item.get("uri"), range) else {
            continue;
        };
        let symbol = json!({
            "name": item.get("name"), "kind": item.get("kind"),
            "location": {"uri": uri, "range": range}
        });
        if let Some(location) = locations_for(symbol, workspace, provider)?.pop() {
            out.push(HierarchyItem {
                location,
                raw: item.clone(),
            });
        }
    }
    Ok(out)
}

/// The entries of an `incomingCalls` (`key` = `"from"`) or `outgoingCalls`
/// (`key` = `"to"`) response. `ranges_uri` is the file `fromRanges` are in
/// when it is not the entry's item's (an outgoing call's: the asked item's).
fn hierarchy_calls(
    value: &Value,
    key: &str,
    ranges_uri: Option<&str>,
    workspace: &Workspace,
    provider: &str,
) -> Result<Vec<HierarchyCall>> {
    let mut out = Vec::new();
    for entry in value.as_array().into_iter().flatten() {
        let Some(raw) = entry.get(key) else { continue };
        let Some(item) = hierarchy_items(raw, workspace, provider)?.pop() else {
            continue;
        };
        let uri = ranges_uri.or_else(|| raw.get("uri").and_then(Value::as_str));
        let mut from_ranges = Vec::new();
        if let Some(uri) = uri {
            let located = entry
                .get("fromRanges")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|range| json!({"uri": uri, "range": range}))
                .collect::<Vec<_>>();
            from_ranges = locations_for(Value::Array(located), workspace, provider)?;
        }
        out.push(HierarchyCall { item, from_ranges });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::transport::TransportConfig;

    #[test]
    fn normalizes_locations_and_location_links() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            id: "test".into(),
            path: dunce::canonicalize(temp.path()).unwrap(),
        };
        let file = workspace.path.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn item() {}\n").unwrap();
        let uri = reqwest::Url::from_file_path(&file).unwrap().to_string();
        let result = locations_for(
            json!([
                {"name":"item","kind":12,"location":{"uri":uri,"range":{"start":{"line":2,"character":3},"end":{"line":2,"character":7}}}},
                {"targetUri":uri,"targetSelectionRange":{"start":{"line":4,"character":1},"end":{"line":4,"character":5}}}
            ]),
            &workspace,
            "rust-analyzer",
        )
        .unwrap();
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].relative_file_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(result[0].start_line, 3);
        assert_eq!(result[0].language.as_deref(), Some("rust"));
        assert_eq!(result[0].provider.as_deref(), Some("rust-analyzer"));
        assert_eq!(result[1].start_line, 5);
    }

    #[test]
    fn hierarchy_items_take_the_selection_range_and_keep_the_raw_item() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            id: "test".into(),
            path: dunce::canonicalize(temp.path()).unwrap(),
        };
        let file = workspace.path.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "fn item() {}\n").unwrap();
        let uri = reqwest::Url::from_file_path(&file).unwrap().to_string();
        let item = json!({
            "name": "item", "kind": 12, "uri": uri, "data": {"opaque": [1, 2]},
            "range": {"start":{"line":3,"character":0},"end":{"line":9,"character":1}},
            "selectionRange": {"start":{"line":4,"character":8},"end":{"line":4,"character":12}}
        });
        let items = hierarchy_items(&json!([item]), &workspace, "rust-analyzer").unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].location.start_line, 5);
        assert_eq!(items[0].location.start_character, 8);
        assert_eq!(items[0].location.name.as_deref(), Some("item"));
        assert_eq!(items[0].raw, item);
        assert!(
            hierarchy_items(&Value::Null, &workspace, "rust-analyzer")
                .unwrap()
                .is_empty()
        );
        // The ranges of an outgoing call are in the asked item's file.
        let calls = hierarchy_calls(
            &json!([{"to": item, "fromRanges": [
                {"start":{"line":0,"character":3},"end":{"line":0,"character":7}}
            ]}]),
            "to",
            Some(&uri),
            &workspace,
            "rust-analyzer",
        )
        .unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].from_ranges.len(), 1);
        assert_eq!(calls[0].from_ranges[0].start_line, 1);
        assert_eq!(calls[0].from_ranges[0].start_character, 3);
    }

    #[test]
    fn document_symbols_of_both_shapes_become_name_locations() {
        let range = |line: u32| json!({"start":{"line":line,"character":3},"end":{"line":line,"character":7}});
        let hierarchical = json!([{
            "name":"outer","kind":23,"range":range(0),"selectionRange":range(1),
            "children":[{"name":"inner","kind":12,"range":range(2),"selectionRange":range(3)}]
        }]);
        let flat =
            json!([{"name":"item","kind":12,"location":{"uri":"file:///x.rs","range":range(4)}}]);
        let names = |value: &Value| -> Vec<(String, u64)> {
            flatten_document_symbols(value, "file:///x.rs")
                .iter()
                .map(|item| {
                    (
                        item["name"].as_str().unwrap().to_owned(),
                        item["location"]["range"]["start"]["line"].as_u64().unwrap(),
                    )
                })
                .collect()
        };
        assert_eq!(
            names(&hierarchical),
            [("outer".into(), 1), ("inner".into(), 3)]
        );
        assert_eq!(names(&flat), [("item".into(), 4)]);
        assert!(names(&Value::Null).is_empty());
    }

    #[test]
    fn a_nested_document_symbol_starts_at_its_name_and_ends_with_the_item() {
        let at = |line: u32, character: u32| json!({"line": line, "character": character});
        let nested = json!([{
            "name":"outer","kind":23,
            "range":{"start": at(3, 0), "end": at(9, 1)},
            "selectionRange":{"start": at(4, 8), "end": at(4, 13)}
        }]);
        let flat = json!([{
            "name":"item","kind":12,
            "location":{"uri":"file:///x.rs","range":{"start": at(4, 8), "end": at(4, 12)}}
        }]);
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            id: "test".into(),
            path: dunce::canonicalize(temp.path()).unwrap(),
        };
        let uri = reqwest::Url::from_file_path(workspace.path.join("x.rs"))
            .unwrap()
            .to_string();
        let located = |value: &Value| {
            let flattened = flatten_document_symbols(value, &uri);
            // A flat item keeps its own uri; point it at the workspace file.
            let flattened: Vec<Value> = flattened
                .into_iter()
                .map(|mut item| {
                    item["location"]["uri"] = json!(uri);
                    item
                })
                .collect();
            locations_for(Value::Array(flattened), &workspace, "test").unwrap()
        };
        // The name's start (1-based line 5, column 8), the item's end (line 10).
        let nested = located(&nested);
        assert_eq!(
            (
                nested[0].start_line,
                nested[0].start_character,
                nested[0].end_line
            ),
            (5, 8, 10)
        );
        // A flat item is unchanged: its range is the item.
        let flat = located(&flat);
        assert_eq!((flat[0].start_line, flat[0].end_line), (5, 5));
    }

    #[cfg(unix)]
    #[test]
    fn location_preserves_backslash_in_unix_filename() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            id: "test".into(),
            path: dunce::canonicalize(temp.path()).unwrap(),
        };
        let file = workspace.path.join(r"a\b.rs");
        std::fs::write(&file, "fn item() {}\n").unwrap();
        std::fs::create_dir_all(workspace.path.join("a")).unwrap();
        std::fs::write(workspace.path.join("a/b.rs"), "fn sibling() {}\n").unwrap();
        let uri = reqwest::Url::from_file_path(&file).unwrap().to_string();
        let result = locations_for(
            json!([{
                "name":"item",
                "kind":12,
                "location":{
                    "uri":uri,
                    "range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}
                }
            }]),
            &workspace,
            "rust-analyzer",
        )
        .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].relative_file_path.as_deref(), Some(r"a\b.rs"));
    }

    #[test]
    fn a_location_that_cannot_be_represented_is_dropped_alone() {
        // (name, items built from the file's uri, the surviving location's
        // start line and end character)
        type Case<'a> = (&'a str, Box<dyn Fn(&str) -> Value>, u32, u32);
        let cases: [Case; 4] = [
            (
                "a line that does not fit u32 after the 1-based shift, a character that does not fit u32, and an invalid uri",
                Box::new(|uri| {
                    json!([
                        {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}},
                        {"uri":uri,"range":{"start":{"line":4294967295u64,"character":0},"end":{"line":0,"character":1}}},
                        {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":4294967296u64}}},
                        {"uri":"not a uri","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
                    ])
                }),
                1,
                1,
            ),
            (
                "a character that does not fit u32 at either end",
                Box::new(|uri| {
                    json!([
                        {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}},
                        {"uri":uri,"range":{"start":{"line":0,"character":4294967296u64},"end":{"line":0,"character":1}}},
                        {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":4294967296u64}}},
                    ])
                }),
                1,
                1,
            ),
            (
                "a missing or negative position",
                Box::new(|uri| {
                    json!([
                        {"uri":uri,"range":{"start":{"line":2,"character":3},"end":{"line":2,"character":5}}},
                        {"uri":uri,"range":{"start":{"line":-1,"character":0},"end":{"line":0,"character":1}}},
                        {"uri":uri,"range":{"start":{"character":0},"end":{"line":0,"character":1}}},
                    ])
                }),
                3,
                5,
            ),
            (
                "a uri that does not parse",
                Box::new(|uri| {
                    json!([
                        {"uri":"not a uri","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}},
                        {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}},
                    ])
                }),
                1,
                1,
            ),
        ];
        for (name, items, start_line, end_character) in cases {
            let result = locations_in_temp_workspace(items);
            assert_eq!(result.len(), 1, "{name}");
            assert_eq!(
                result[0].relative_file_path.as_deref(),
                Some("src/lib.rs"),
                "{name}"
            );
            assert_eq!(result[0].start_line, start_line, "{name}");
            assert_eq!(result[0].end_character, end_character, "{name}");
        }
    }

    /// `locations_for` over `items`, where each item is built from the
    /// `uri` of a file in a temporary workspace.
    fn locations_in_temp_workspace(items: impl Fn(&str) -> Value) -> Vec<Location> {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            id: "test".into(),
            path: dunce::canonicalize(temp.path()).unwrap(),
        };
        let file = workspace.path.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let uri = reqwest::Url::from_file_path(&file).unwrap().to_string();
        locations_for(items(&uri), &workspace, "rust-analyzer").unwrap()
    }

    #[test]
    fn only_the_protocols_transient_codes_are_repeated_on_any_server() {
        for code in [-32801, -32802, -32803] {
            assert!(is_transient_code(code), "{code}");
        }
        // Internal error (what rust-analyzer's "file not found" uses, which only
        // its adapter treats as transient), invalid params, method not found,
        // RequestCancelled, tsserver's "No Project" code, and a response with
        // no usable code.
        for code in [-32603, -32602, -32601, -32800, 1, 0] {
            assert!(!is_transient_code(code), "{code}");
        }
    }

    /// An adapter with no rules of its own: only the protocol's transient codes
    /// are repeated.
    struct NoRules;

    impl LspAdapter for NoRules {
        fn provider(&self) -> &'static str {
            "test-server"
        }
        fn languages(&self) -> &'static [&'static str] {
            &[]
        }
        fn enabled(&self) -> bool {
            true
        }
        fn configured_command(&self) -> Option<String> {
            None
        }
        fn readiness_component_name(&self) -> &'static str {
            "test_server_available"
        }
        fn transport_config(&self, _root: &std::path::Path) -> Option<TransportConfig> {
            None
        }
        fn initialize_params(&self, _root: &std::path::Path) -> Option<Value> {
            None
        }
        fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
            Box::new(|_| false)
        }
    }

    /// An adapter whose version probe answers from a list (the last answer
    /// repeats) and counts how often it was asked.
    struct Probing {
        answers: Vec<bool>,
        asked: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LspAdapter for Probing {
        fn provider(&self) -> &'static str {
            "probing-server"
        }
        fn languages(&self) -> &'static [&'static str] {
            &[]
        }
        fn enabled(&self) -> bool {
            true
        }
        fn configured_command(&self) -> Option<String> {
            None
        }
        fn readiness_component_name(&self) -> &'static str {
            "probing_server_available"
        }
        async fn probe_readiness(&self, _timeout: Duration) -> Result<()> {
            let n = self.asked.fetch_add(1, Ordering::SeqCst);
            match self.answers[n.min(self.answers.len() - 1)] {
                true => Ok(()),
                false => Err(anyhow!("no such binary")),
            }
        }
        fn transport_config(&self, _root: &std::path::Path) -> Option<TransportConfig> {
            None
        }
        fn initialize_params(&self, _root: &std::path::Path) -> Option<Value> {
            None
        }
        fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
            Box::new(|_| false)
        }
    }

    /// The table of real adapters: only rust-analyzer sees an edit on its own
    /// (it declares no `didChangeWatchedFiles` and runs its own file watcher),
    /// so only it keeps a session across traverse calls under `sync = "off"`
    /// (see `LspAdapter::watches_workspace`).
    #[test]
    fn only_rust_analyzer_watches_its_workspace() {
        let manager = Manager::new(&Config::default());
        let watching: Vec<&str> = manager
            .adapters()
            .iter()
            .filter(|adapter| adapter.watches_workspace())
            .map(|adapter| adapter.provider())
            .collect();
        assert_eq!(watching, ["rust-analyzer"]);
        assert_eq!(
            manager.adapters().len(),
            7,
            "a new adapter must be classified"
        );
    }

    /// Only a server that declares the configuration files its tree stamp
    /// covers may be kept, even under `sync = "watch"`; the others start per
    /// call. A new adapter must be classified here.
    #[test]
    fn only_servers_with_a_stamped_configuration_list_keep_a_session() {
        let manager = Manager::new(&Config::default());
        let keeping = |synced: bool| -> Vec<&str> {
            manager
                .adapters()
                .iter()
                .filter(|adapter| adapter.keeps_session(synced))
                .map(|adapter| adapter.provider())
                .collect()
        };
        assert_eq!(keeping(false), ["rust-analyzer"], "sync = off");
        let mut watched = keeping(true);
        watched.sort_unstable();
        assert_eq!(
            watched,
            [
                "clangd",
                "csharp-ls",
                "gopls",
                "pyright",
                "rust-analyzer",
                "typescript-language-server"
            ],
            "sync = watch: jdtls lists no configuration files"
        );
        for adapter in manager.adapters() {
            if adapter.keeps_session(true) {
                assert!(!adapter.stamp_manifests().is_empty());
            }
        }
    }

    /// An index file list that counts how many stamps are being taken at once.
    struct SlowFiles {
        in_flight: std::sync::atomic::AtomicUsize,
        most: std::sync::atomic::AtomicUsize,
        calls: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl stamp::IndexedFiles for SlowFiles {
        async fn indexed_files(&self, _workspace: &Workspace) -> Option<Arc<Vec<String>>> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.most.fetch_max(now, Ordering::SeqCst);
            self.calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(40)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Some(Arc::new(vec!["src/lib.rs".to_owned()]))
        }
    }

    /// The barriers of one workspace run one at a time: a barrier that took
    /// its stamp before another finished could record an older stamp over the
    /// newer one and stop the server the other had just started.
    #[tokio::test]
    async fn concurrent_barriers_of_one_workspace_run_one_at_a_time() {
        let temp = tempfile::tempdir().unwrap();
        let path = dunce::canonicalize(temp.path()).unwrap();
        let ws = Workspace {
            id: crate::workspace::hash(&path.to_string_lossy()),
            path,
        };
        std::fs::create_dir_all(ws.path.join("src")).unwrap();
        std::fs::write(ws.path.join("src/lib.rs"), "pub fn a() {}\n").unwrap();
        let source = Arc::new(SlowFiles {
            in_flight: std::sync::atomic::AtomicUsize::new(0),
            most: std::sync::atomic::AtomicUsize::new(0),
            calls: std::sync::atomic::AtomicUsize::new(0),
        });
        let manager = Manager::new(&Config::default())
            .with_indexed_files(Arc::clone(&source) as Arc<dyn stamp::IndexedFiles>);
        let rust = Arc::clone(manager.adapters_for_language("rust")[0]);
        let only = [&rust];
        let (a, b, c) = tokio::join!(
            manager.tree_barrier(&ws, &only),
            manager.tree_barrier(&ws, &only),
            manager.tree_barrier(&ws, &only),
        );
        assert_eq!(source.calls.load(Ordering::SeqCst), 3);
        assert_eq!(
            source.most.load(Ordering::SeqCst),
            1,
            "two barriers of one workspace took their stamps at once"
        );
        // No session ran, so nothing was stopped or kept.
        for restarted in [a, b, c] {
            assert!(restarted.is_empty());
        }
    }

    #[tokio::test]
    async fn a_probe_is_run_once_per_provider_until_it_fails() {
        let timeout = Duration::from_secs(1);
        let probing = |answers: &[bool]| {
            let adapter = Arc::new(Probing {
                answers: answers.to_vec(),
                asked: std::sync::atomic::AtomicUsize::new(0),
            });
            (Arc::clone(&adapter) as Arc<dyn LspAdapter>, adapter)
        };
        let manager = Manager::new(&Config::default());
        // Ok, Ok: the second call is answered from memory.
        let (adapter, counts) = probing(&[true]);
        manager.probe_once(&adapter, timeout).await.unwrap();
        manager.probe_once(&adapter, timeout).await.unwrap();
        assert_eq!(counts.asked.load(Ordering::SeqCst), 1);
        // Err, Ok: a failure is not remembered, the next call asks again, and
        // after the Ok no more probes are made.
        let manager = Manager::new(&Config::default());
        let (adapter, counts) = probing(&[false, true]);
        assert!(manager.probe_once(&adapter, timeout).await.is_err());
        manager.probe_once(&adapter, timeout).await.unwrap();
        manager.probe_once(&adapter, timeout).await.unwrap();
        assert_eq!(counts.asked.load(Ordering::SeqCst), 2);
        // Another manager keeps its own memory.
        let other = Manager::new(&Config::default());
        other.probe_once(&adapter, timeout).await.unwrap();
        assert_eq!(counts.asked.load(Ordering::SeqCst), 3);
    }

    /// A window long enough that no test outlives it: a test that needs it to
    /// have passed calls `expire` instead of waiting.
    fn long_window() -> Arc<LspBreakers> {
        Arc::new(LspBreakers::new(Duration::from_secs(3600)))
    }

    #[tokio::test]
    async fn a_failed_probe_opens_the_window_and_the_next_caller_is_refused_without_a_probe() {
        let timeout = Duration::from_secs(1);
        let adapter = Arc::new(Probing {
            answers: vec![false, true],
            asked: std::sync::atomic::AtomicUsize::new(0),
        });
        let asked = || adapter.asked.load(Ordering::SeqCst);
        let dyn_adapter: Arc<dyn LspAdapter> = Arc::clone(&adapter) as Arc<dyn LspAdapter>;
        let breakers = long_window();
        let manager = Manager::new(&Config::default()).with_breakers(Some(Arc::clone(&breakers)));
        // The first probe fails and is run.
        assert!(manager.probe_once(&dyn_adapter, timeout).await.is_err());
        assert_eq!(asked(), 1);
        assert!(breakers.remaining("probing-server").is_some());
        // The next caller is refused by the window, with no probe.
        let refused = manager.probe_once(&dyn_adapter, timeout).await.unwrap_err();
        assert!(
            format!("{refused:#}").contains("lsp breaker"),
            "{refused:#}"
        );
        assert_eq!(asked(), 1);
        // The window has passed: the provider is probed again, and the
        // success is remembered.
        breakers.expire("probing-server");
        manager.probe_once(&dyn_adapter, timeout).await.unwrap();
        assert_eq!(asked(), 2);
        assert!(breakers.remaining("probing-server").is_none());
        manager.probe_once(&dyn_adapter, timeout).await.unwrap();
        assert_eq!(asked(), 2);
    }

    /// An adapter whose version probe passes and whose server cannot start (it
    /// has no executable), counting the starts it was asked for.
    struct Starting {
        starts: std::sync::atomic::AtomicUsize,
    }

    #[async_trait::async_trait]
    impl LspAdapter for Starting {
        fn provider(&self) -> &'static str {
            "starting-server"
        }
        fn languages(&self) -> &'static [&'static str] {
            &[]
        }
        fn enabled(&self) -> bool {
            true
        }
        fn configured_command(&self) -> Option<String> {
            None
        }
        fn readiness_component_name(&self) -> &'static str {
            "starting_server_available"
        }
        async fn probe_readiness(&self, _timeout: Duration) -> Result<()> {
            Ok(())
        }
        fn transport_config(&self, _root: &std::path::Path) -> Option<TransportConfig> {
            self.starts.fetch_add(1, Ordering::SeqCst);
            None
        }
        fn initialize_params(&self, _root: &std::path::Path) -> Option<Value> {
            None
        }
        fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
            Box::new(|_| false)
        }
    }

    #[tokio::test]
    async fn a_failed_start_opens_the_window_and_the_next_request_is_refused_at_once() {
        let stub = Arc::new(Starting {
            starts: std::sync::atomic::AtomicUsize::new(0),
        });
        let starts = || stub.starts.load(Ordering::SeqCst);
        let adapter: Arc<dyn LspAdapter> = Arc::clone(&stub) as Arc<dyn LspAdapter>;
        let workspace = prime_workspace();
        let breakers = long_window();
        let manager = Manager::new(&Config::default()).with_breakers(Some(Arc::clone(&breakers)));
        // The first request starts the server once; the start fails, which
        // opens the window.
        let first = manager
            .symbols(&adapter, &workspace, "x")
            .await
            .unwrap_err();
        let first = format!("{first:#}");
        assert!(first.contains("disabled or has no executable"), "{first}");
        assert!(!first.contains("lsp breaker"), "{first}");
        assert_eq!(starts(), 1);
        assert!(breakers.remaining("starting-server").is_some());
        // The next request is refused at once: no start is attempted.
        let second = manager
            .symbols(&adapter, &workspace, "x")
            .await
            .unwrap_err();
        assert!(format!("{second:#}").contains("lsp breaker"), "{second:#}");
        assert_eq!(starts(), 1);
        // The window has passed: the server is started again.
        breakers.expire("starting-server");
        assert!(manager.symbols(&adapter, &workspace, "x").await.is_err());
        assert_eq!(starts(), 2);
    }

    #[tokio::test]
    async fn without_the_breaker_every_request_starts_the_server_again() {
        let stub = Arc::new(Starting {
            starts: std::sync::atomic::AtomicUsize::new(0),
        });
        let adapter: Arc<dyn LspAdapter> = Arc::clone(&stub) as Arc<dyn LspAdapter>;
        let workspace = prime_workspace();
        let manager = Manager::new(&Config::default());
        assert!(manager.breakers().is_none());
        for _ in 0..2 {
            let error = manager
                .symbols(&adapter, &workspace, "x")
                .await
                .unwrap_err();
            assert!(!format!("{error:#}").contains("lsp breaker"), "{error:#}");
        }
        // Each request starts the server again.
        assert_eq!(stub.starts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_timed_out_start_opens_no_window() {
        let breakers = LspBreakers::new(Duration::from_secs(3600));
        let timed_out: Result<()> = Err(anyhow::Error::new(InactivityTimeout {
            label: "csharp-ls".into(),
            seconds: 5,
        })
        .context("initialize csharp-ls"));
        // A slow start is not a dead one.
        breakers.note_start("csharp-ls", &timed_out);
        assert!(breakers.remaining("csharp-ls").is_none());
        // Any other failure of a start opens the window.
        breakers.note_start("csharp-ls", &Err::<(), _>(anyhow!("start x: no such file")));
        assert!(breakers.remaining("csharp-ls").is_some());
        // Another provider is not touched, and a start that succeeds closes it.
        assert!(breakers.remaining("rust-analyzer").is_none());
        breakers.note_start("csharp-ls", &Ok(()));
        assert!(breakers.remaining("csharp-ls").is_none());
        // A timed-out start leaves an open window as it is.
        breakers.trip("csharp-ls");
        breakers.note_start("csharp-ls", &timed_out);
        assert!(breakers.remaining("csharp-ls").is_some());
        breakers.reset();
        assert!(breakers.remaining("csharp-ls").is_none());
    }

    /// How a [`Primable`]'s version probe behaves.
    #[derive(Clone)]
    enum Probe {
        Passes,
        /// The binary is missing.
        Fails,
        /// The probe never answers.
        Hangs,
        /// The probe says it has started, then waits for a permit of the gate.
        Gated(Gate),
    }

    /// The hands a test keeps on a [`Probe::Gated`] probe.
    #[derive(Clone)]
    struct Gate {
        entered: Arc<tokio::sync::Notify>,
        permits: Arc<tokio::sync::Semaphore>,
        asked: Arc<std::sync::atomic::AtomicUsize>,
    }

    impl Gate {
        fn new() -> Self {
            Self {
                entered: Arc::new(tokio::sync::Notify::new()),
                permits: Arc::new(tokio::sync::Semaphore::new(0)),
                asked: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
            }
        }
    }

    /// An adapter with a name, a switch and a scripted version probe. It has
    /// no transport, so a session started for it fails at once.
    struct Primable {
        provider: &'static str,
        enabled: bool,
        probe: Probe,
    }

    #[async_trait::async_trait]
    impl LspAdapter for Primable {
        fn provider(&self) -> &'static str {
            self.provider
        }
        fn languages(&self) -> &'static [&'static str] {
            &[]
        }
        fn enabled(&self) -> bool {
            self.enabled
        }
        fn configured_command(&self) -> Option<String> {
            None
        }
        fn readiness_component_name(&self) -> &'static str {
            "primable_available"
        }
        async fn probe_readiness(&self, _timeout: Duration) -> Result<()> {
            match &self.probe {
                Probe::Passes => Ok(()),
                Probe::Fails => Err(anyhow!("no such binary: {}", self.provider)),
                Probe::Hangs => std::future::pending().await,
                Probe::Gated(gate) => {
                    gate.asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    gate.entered.notify_one();
                    gate.permits.acquire().await.unwrap().forget();
                    Ok(())
                }
            }
        }
        fn transport_config(&self, _root: &std::path::Path) -> Option<TransportConfig> {
            None
        }
        fn initialize_params(&self, _root: &std::path::Path) -> Option<Value> {
            None
        }
        fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
            Box::new(|_| false)
        }
    }

    fn primable(provider: &'static str, probe: Probe) -> Arc<dyn LspAdapter> {
        Arc::new(Primable {
            provider,
            enabled: true,
            probe,
        })
    }

    fn prime_workspace() -> Workspace {
        Workspace {
            id: "prime".into(),
            path: PathBuf::from("workspace"),
        }
    }

    /// With no timeout a slow version probe is waited for: the call ends when
    /// the probe answers, not when a clock says so.
    #[tokio::test]
    async fn an_unbounded_warm_up_waits_for_a_slow_probe() {
        let gate = Gate::new();
        let mut manager = Manager::new(&Config::default());
        manager.adapters = vec![
            primable("gated", Probe::Gated(gate.clone())),
            primable("no-transport", Probe::Passes),
        ];
        let manager = Arc::new(manager);
        let handle = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move {
                manager
                    .prime_languages(&prime_workspace(), None, None)
                    .await
                    .into_iter()
                    .map(|(provider, outcome)| (provider, format!("{:#}", outcome.unwrap_err())))
                    .collect::<BTreeMap<_, _>>()
            })
        };
        gate.entered.notified().await;
        assert!(!handle.is_finished(), "the probe has not answered yet");
        gate.permits.add_permits(1);
        let outcomes = handle.await.unwrap();
        assert_eq!(
            outcomes.keys().copied().collect::<Vec<_>>(),
            ["gated", "no-transport"]
        );
        for (provider, message) in &outcomes {
            assert!(
                message.contains("disabled or has no executable"),
                "{provider}: {message}"
            );
            assert!(!message.contains("did not answer"), "{provider}: {message}");
        }
        assert_eq!(gate.asked.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// With no timeout a probe that never answers ends only by the caller's
    /// abort. This proves the abort cancels the probe wait and leaves no
    /// session; it does not prove the cleanup of a start already in flight
    /// (the d450 warm-start tests cover that).
    #[tokio::test]
    async fn a_warm_up_with_a_hanging_probe_ends_only_by_abort() {
        let gate = Gate::new();
        let mut manager = Manager::new(&Config::default());
        manager.adapters = vec![primable("gated", Probe::Gated(gate.clone()))];
        let manager = Arc::new(manager);
        let handle = {
            let manager = Arc::clone(&manager);
            tokio::spawn(async move {
                manager
                    .prime_languages(&prime_workspace(), None, None)
                    .await
            })
        };
        gate.entered.notified().await;
        assert!(!handle.is_finished());
        handle.abort();
        assert!(handle.await.unwrap_err().is_cancelled());
        assert!(!manager.running("gated", "prime").await);
    }

    /// Two probes that never answer share the one timeout: the call ends at the
    /// deadline, not after one timeout each. The paused clock makes the end
    /// exact; no session starts, so nothing waits on a blocking thread.
    #[tokio::test(start_paused = true)]
    async fn hanging_probes_share_the_one_timeout_of_a_bounded_prime() {
        let mut manager = Manager::new(&Config::default());
        manager.adapters = vec![
            primable("never-answers", Probe::Hangs),
            primable("never-answers-either", Probe::Hangs),
        ];
        let timeout = Duration::from_millis(400);
        let started = tokio::time::Instant::now();
        let outcomes = manager.prime(&prime_workspace(), timeout).await;
        assert_eq!(started.elapsed(), timeout);
        for provider in ["never-answers", "never-answers-either"] {
            let message = format!("{:#}", outcomes[provider].as_ref().unwrap_err());
            assert!(message.contains("did not answer within 400ms"), "{message}");
        }
    }

    #[tokio::test]
    async fn prime_reports_each_provider_and_a_missing_binary_as_an_error() {
        let mut manager = Manager::new(&Config::default());
        let stub = |provider, enabled, probe| -> Arc<dyn LspAdapter> {
            Arc::new(Primable {
                provider,
                enabled,
                probe,
            })
        };
        manager.adapters = vec![
            stub("no-binary", true, Probe::Fails),
            stub("no-transport", true, Probe::Passes),
            stub("disabled", false, Probe::Fails),
            stub("never-answers", true, Probe::Hangs),
            stub("never-answers-either", true, Probe::Hangs),
        ];
        let workspace = Workspace {
            id: "prime".into(),
            path: PathBuf::from("workspace"),
        };
        let timeout = Duration::from_millis(400);

        // Real time here: the session of `no-transport` fails on a blocking
        // thread, which the paused clock would not wait for. That the probes
        // share one deadline is the next test's, under the paused clock.
        let outcomes = manager.prime(&workspace, timeout).await;

        // Every enabled provider is answered for, a disabled one is not asked.
        assert_eq!(
            outcomes.keys().copied().collect::<Vec<_>>(),
            [
                "never-answers",
                "never-answers-either",
                "no-binary",
                "no-transport"
            ]
        );
        let message = |provider: &str| format!("{:#}", outcomes[provider].as_ref().unwrap_err());
        assert!(
            message("no-binary").contains("no such binary"),
            "{}",
            message("no-binary")
        );
        assert!(
            message("no-transport").contains("disabled or has no executable"),
            "{}",
            message("no-transport")
        );
        assert!(
            message("never-answers").contains("did not answer"),
            "{}",
            message("never-answers")
        );
        // A provider whose probe failed started no session.
        assert!(!manager.running("no-binary", "prime").await);
        assert!(!manager.running("never-answers", "prime").await);

        // Only the adapters of the languages asked for are primed: these
        // stubs serve none, so none is asked, and `None` still asks them all.
        let held = HashSet::from(["rust".to_owned()]);
        assert!(
            manager
                .prime_languages(&workspace, Some(timeout), Some(&held))
                .await
                .is_empty()
        );
    }

    const FAST_WAITS: [Duration; 2] = [Duration::from_millis(10), Duration::from_millis(10)];

    fn fast_policy(window: Duration) -> RepeatPolicy {
        RepeatPolicy {
            waits: &FAST_WAITS,
            window,
        }
    }

    /// Runs `send_with_repeats` against a server that answers every request
    /// with an error response with `code` until attempt number `succeed_on`.
    /// The timer runs `stretch` times slower than asked. Returns the outcome,
    /// how many requests were sent and how many waits were started.
    async fn ask(
        policy: &RepeatPolicy,
        stretch: u32,
        code: i64,
        succeed_on: usize,
    ) -> (Result<Value>, usize, usize) {
        let adapter: Arc<dyn LspAdapter> = Arc::new(NoRules);
        let workspace = Workspace {
            id: "test".into(),
            path: PathBuf::from("workspace"),
        };
        let attempts = std::cell::Cell::new(0usize);
        let waits = std::cell::Cell::new(0usize);
        let result = send_with_repeats(
            policy,
            |wait| {
                waits.set(waits.get() + 1);
                tokio::time::sleep(wait * stretch)
            },
            &adapter,
            &workspace,
            "workspace/symbol",
            || false,
            || {
                attempts.set(attempts.get() + 1);
                let attempt = attempts.get();
                async move {
                    if attempt >= succeed_on {
                        Ok(json!("answer"))
                    } else {
                        Err(anyhow::Error::new(RpcError {
                            code,
                            message: "boom".to_owned(),
                        }))
                    }
                }
            },
        )
        .await;
        (result, attempts.get(), waits.get())
    }

    #[tokio::test]
    async fn same_server_repeats() {
        // a transient error is repeated until the server answers
        {
            let policy = fast_policy(Duration::from_secs(5));
            let (result, attempts, _) = ask(&policy, 1, RpcError::CONTENT_MODIFIED, 3).await;
            assert_eq!(result.unwrap(), json!("answer"));
            assert_eq!(attempts, 3);
        }

        // the repeats stop after the last wait
        {
            let policy = fast_policy(Duration::from_secs(5));
            let (result, attempts, _) = ask(&policy, 1, RpcError::SERVER_CANCELLED, 99).await;
            let error = result.unwrap_err();
            assert_eq!(
                error.downcast_ref::<RpcError>().map(|e| e.code),
                Some(RpcError::SERVER_CANCELLED)
            );
            assert_eq!(attempts, 3, "the request and its two repeats");
        }

        // an error that is not transient is not repeated
        {
            let policy = fast_policy(Duration::from_secs(5));
            let (result, attempts, _) = ask(&policy, 1, -32602, 99).await;
            assert!(result.is_err());
            assert_eq!(attempts, 1);
        }

        // a repeat whose wait does not fit in the window is not waited for
        // The window decides before the wait: a repeat that cannot start inside
        // it is not even waited for.
        {
            const LONG_WAIT: [Duration; 1] = [Duration::from_millis(400)];
            let policy = RepeatPolicy {
                waits: &LONG_WAIT,
                window: Duration::from_millis(50),
            };
            let (result, attempts, waits) = ask(&policy, 1, RpcError::CONTENT_MODIFIED, 2).await;
            assert!(result.is_err());
            assert_eq!(attempts, 1);
            assert_eq!(waits, 0, "the wait must not be started");
        }

        // the window is checked again after the wait
        // The window is checked again after the wait, because a timer can run
        // late: here the 10 ms wait fits when it is decided on, but the timer
        // returns after 60 ms, past the 50 ms window, so the repeat is not sent.
        // Without the second check the repeat would go out (2 requests, not 1).
        {
            let policy = fast_policy(Duration::from_millis(50));
            let (result, attempts, _) = ask(&policy, 6, RpcError::CONTENT_MODIFIED, 99).await;
            assert!(result.is_err());
            assert_eq!(
                attempts, 1,
                "no repeat may start after the window has passed"
            );
        }
    }

    #[test]
    fn the_server_state_word_follows_the_precedence() {
        use ServerStateName::*;
        // (failed, started, stuck, busy)
        assert_eq!(
            ServerStateName::classify(false, false, false, false),
            Starting
        );
        assert_eq!(ServerStateName::classify(false, true, false, false), Ready);
        assert_eq!(ServerStateName::classify(false, true, false, true), Busy);
        assert_eq!(
            ServerStateName::classify(false, true, true, true),
            Stuck,
            "at the limit the session is stuck, busy or not"
        );
        assert_eq!(ServerStateName::classify(true, false, false, false), Failed);
        assert_eq!(
            serde_json::to_value(Stuck).unwrap(),
            json!("stuck"),
            "the wire spelling is the word"
        );
        assert_eq!(Busy.as_str(), "busy");
    }

    const LIMIT: Duration = Duration::from_secs(900);

    #[test]
    fn transient_streaks() {
        // a streak needs two transient answers a limit apart
        {
            let start = Instant::now();
            let mut streak = TransientStreak::default();
            assert_eq!(streak.transient(start, 0, LIMIT), None, "the first answer");
            assert_eq!(
                streak.transient(start + LIMIT - Duration::from_millis(1), 0, LIMIT),
                None,
                "just under the limit"
            );
            assert_eq!(
                streak.transient(start + LIMIT, 0, LIMIT),
                Some(LIMIT),
                "at the limit the session is stuck"
            );
        }

        // an answer ends the streak
        // A server that answers (a result, or an error that is not transient)
        // is not stuck, and the span starts over with its next transient error.
        {
            let start = Instant::now();
            let mut streak = TransientStreak::default();
            assert_eq!(streak.transient(start, 0, LIMIT), None);
            streak.answered();
            let later = start + LIMIT * 2;
            assert_eq!(
                streak.transient(later, 0, LIMIT),
                None,
                "a new streak begins here, not at the first error"
            );
            assert_eq!(streak.transient(later + LIMIT, 0, LIMIT), Some(LIMIT));
        }

        // a transient answer before the streak began is not a span
        // An answer that arrives with an earlier timestamp than the streak's start
        // (clocks of concurrent requests) is no span at all, not a panic.
        {
            let start = Instant::now();
            let mut streak = TransientStreak::default();
            assert_eq!(streak.transient(start + LIMIT, 0, LIMIT), None);
            assert_eq!(streak.transient(start, 0, LIMIT), None);
        }

        // progress restarts the streak
        // Progress means the server is working: a span that straddles a
        // progress notification starts over at the answer that saw it, and only
        // a span in which the count stayed the same can reach the limit.
        {
            let start = Instant::now();
            let mut streak = TransientStreak::default();
            assert_eq!(streak.transient(start, 1, LIMIT), None);
            assert_eq!(
                streak.transient(start + LIMIT, 2, LIMIT),
                None,
                "the count moved: a new span begins here"
            );
            assert_eq!(
                streak.transient(start + LIMIT * 2 - Duration::from_millis(1), 2, LIMIT),
                None,
                "just under the limit since the progress"
            );
            assert_eq!(
                streak.transient(start + LIMIT * 2, 2, LIMIT),
                Some(LIMIT),
                "no more progress for the whole limit: stuck"
            );
        }
    }

    #[test]
    fn the_repeat_waits_fit_inside_the_repeat_window() {
        // The window is measured from the first error response, so the waits
        // alone must not exhaust it: a server that answers at once would
        // otherwise never get its last repeat.
        let waits: Duration = SAME_SERVER_RETRY_WAITS.iter().sum();
        assert!(waits < SAME_SERVER_RETRY_WINDOW, "{waits:?}");
    }

    #[test]
    fn only_the_adapters_that_resend_the_text_count_as_opening_documents() {
        let manager = Manager::new(&Config::default());
        let mut opens: Vec<(&str, bool)> = manager
            .adapters()
            .iter()
            .map(|adapter| (adapter.provider(), adapter.opens_documents()))
            .collect();
        opens.sort_unstable();
        assert_eq!(
            opens,
            [
                ("clangd", true),
                ("csharp-ls", false),
                ("gopls", true),
                ("jdtls", true),
                ("pyright", true),
                ("rust-analyzer", false),
                ("typescript-language-server", false),
            ]
        );
    }

    #[test]
    fn an_outline_is_kept_per_hash_and_session_never_empty_and_bounded() {
        let symbol = |name: &str| Location {
            file_path: PathBuf::from("/w/src/a.cs"),
            relative_file_path: Some("src/a.cs".into()),
            start_line: 1,
            start_character: 0,
            end_line: 1,
            end_character: 1,
            name: Some(name.into()),
            kind: Some(6),
            language: None,
            provider: None,
        };
        let key = |path: &str| ("csharp-ls", "ws".to_owned(), path.to_owned());
        let (old, new) = (content_hash("a"), content_hash("b"));
        assert_eq!(old, crate::workspace::hash("a"), "the index's hash");
        let mut cache = OutlineCache::default();

        assert!(!cache.put(key("src/a.cs"), old.clone(), 1, &[]), "empty");
        assert!(cache.get(&key("src/a.cs"), &old, 1).is_none());

        assert!(cache.put(key("src/a.cs"), old.clone(), 1, &[symbol("Add")]));
        assert_eq!(
            cache.get(&key("src/a.cs"), &old, 1).unwrap()[0]
                .name
                .as_deref(),
            Some("Add")
        );
        assert!(cache.get(&key("src/a.cs"), &new, 1).is_none(), "edited");
        assert!(
            cache.get(&key("src/a.cs"), &old, 2).is_none(),
            "new session"
        );
        assert!(cache.get(&key("src/b.cs"), &old, 1).is_none(), "other file");
        let other_workspace = ("csharp-ls", "other".to_owned(), "src/a.cs".to_owned());
        assert!(cache.get(&other_workspace, &old, 1).is_none());

        // The new text's outline replaces the old one.
        cache.put(key("src/a.cs"), new.clone(), 1, &[symbol("Sub")]);
        assert!(cache.get(&key("src/a.cs"), &old, 1).is_none());
        assert_eq!(
            cache.get(&key("src/a.cs"), &new, 1).unwrap()[0]
                .name
                .as_deref(),
            Some("Sub")
        );

        // Full: replacing a kept file does not clear, a new file does.
        for index in 1..MAX_CACHED_OUTLINES {
            cache.put(key(&format!("f{index}")), old.clone(), 1, &[symbol("x")]);
        }
        assert_eq!(cache.entries.len(), MAX_CACHED_OUTLINES);
        cache.put(key("src/a.cs"), old.clone(), 1, &[symbol("Add")]);
        assert_eq!(cache.entries.len(), MAX_CACHED_OUTLINES);
        cache.put(key("one-more"), old.clone(), 1, &[symbol("y")]);
        assert_eq!(cache.entries.len(), 1);
        assert!(cache.get(&key("one-more"), &old, 1).is_some());
    }
}

#[cfg(test)]
mod sync_tests {
    use super::*;

    /// A manager whose C# server cannot start (its binary is missing), so a
    /// request begins a session that fails at once.
    fn manager(sync: LspSync) -> Manager {
        let mut config = Config::default();
        config.csharp.path = Some("definitely-missing-csharp-ls".into());
        config.lsp_idle_timeout_seconds = 0;
        config.run_profile.lsp.sync = sync;
        Manager::new(&config)
    }

    fn workspace(temp: &tempfile::TempDir, name: &str) -> Workspace {
        let path = temp.path().join(name);
        std::fs::create_dir_all(&path).unwrap();
        let path = dunce::canonicalize(path).unwrap();
        Workspace {
            id: crate::workspace::hash(&path.to_string_lossy()),
            path,
        }
    }

    async fn ask(manager: &Manager, workspace: &Workspace) {
        let csharp = Arc::clone(manager.adapters_for_language("csharp")[0]);
        assert!(manager.symbols(&csharp, workspace, "x").await.is_err());
    }

    #[tokio::test]
    async fn the_sync_stops_with_the_workspaces_last_session() {
        let temp = tempfile::tempdir().unwrap();
        let (one, two) = (workspace(&temp, "one"), workspace(&temp, "two"));
        let manager = manager(LspSync::Watch);
        assert_eq!(manager.sync_mode(), LspSync::Watch);
        assert!(manager.sync_status(&one.id).is_none(), "no session yet");
        ask(&manager, &one).await;
        ask(&manager, &two).await;
        assert!(manager.sync_status(&one.id).is_some());
        assert!(manager.sync_status(&two.id).is_some());
        manager.release(&one.id).await;
        assert!(manager.sync_status(&one.id).is_none(), "released");
        assert!(manager.sync_status(&two.id).is_some(), "another workspace");
        ask(&manager, &one).await;
        assert!(manager.sync_status(&one.id).is_some(), "a new session");
        manager.shutdown_all().await;
        assert!(manager.sync_status(&one.id).is_none());
        assert!(manager.sync_status(&two.id).is_none());
    }

    #[tokio::test]
    async fn no_sync_starts_when_it_is_off_or_the_manager_is_per_call() {
        let temp = tempfile::tempdir().unwrap();
        let one = workspace(&temp, "one");
        let off = manager(LspSync::Off);
        ask(&off, &one).await;
        assert!(off.sync_status(&one.id).is_none());
        assert!(off.syncs.lock().unwrap().is_empty());
        let per_call = manager(LspSync::Watch).without_sync();
        assert_eq!(per_call.sync_mode(), LspSync::Off);
        ask(&per_call, &one).await;
        assert!(per_call.syncs.lock().unwrap().is_empty());
        assert!(!per_call.synced(&one.id, &[], Duration::from_secs(1)).await);
    }

    #[test]
    fn a_synced_change_drops_the_files_kept_outlines_in_its_workspace_only() {
        let symbol = Location {
            file_path: PathBuf::from("a"),
            relative_file_path: Some("src/a.cs".into()),
            start_line: 1,
            start_character: 0,
            end_line: 1,
            end_character: 1,
            name: Some("A".into()),
            kind: None,
            language: None,
            provider: None,
        };
        let mut cache = OutlineCache::default();
        let key = |workspace: &str, path: &str| ("pyright", workspace.to_owned(), path.to_owned());
        for (workspace, path) in [("w", "src/a.py"), ("w", "src/b.py"), ("v", "src/a.py")] {
            cache.put(
                key(workspace, path),
                "h".into(),
                1,
                std::slice::from_ref(&symbol),
            );
        }
        cache.forget(
            "w",
            &[sync::FileChange {
                relative: "src/a.py".into(),
                kind: sync::ChangeKind::Changed,
            }],
        );
        assert!(cache.get(&key("w", "src/a.py"), "h", 1).is_none());
        assert!(cache.get(&key("w", "src/b.py"), "h", 1).is_some());
        assert!(cache.get(&key("v", "src/a.py"), "h", 1).is_some());
    }
}
