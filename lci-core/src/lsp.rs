use crate::{
    config::Config,
    language,
    lsp::{
        adapter::LspAdapter,
        clangd::ClangdServer,
        csharp::CSharpServer,
        go::GoServer,
        java::JavaServer,
        python::PythonServer,
        rust::RustServer,
        transport::{JsonRpcClient, RpcError},
        typescript::TypeScriptServer,
    },
    workspace::Workspace,
};
use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::{
    sync::{Mutex, watch},
    task::JoinSet,
};

pub mod adapter;
pub mod clangd;
pub mod csharp;
pub mod go;
pub mod java;
#[cfg(windows)]
mod job;
pub mod python;
pub mod rust;
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

/// A live JSON-RPC session with one spawned, initialized language server.
struct GenericClient {
    rpc: Arc<JsonRpcClient>,
}

impl GenericClient {
    async fn spawn(adapter: &Arc<dyn LspAdapter>, workspace: &Workspace) -> Result<Arc<Self>> {
        let transport = adapter
            .transport_config(&workspace.path)
            .with_context(|| format!("{} is disabled or has no executable", adapter.provider()))?;
        let rpc = JsonRpcClient::spawn(
            &transport,
            &adapter.environment(),
            adapter.ready_predicate(),
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
}

impl Session {
    fn begin(
        adapter: Arc<dyn LspAdapter>,
        workspace: Workspace,
        stuck_after: Option<Duration>,
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
        Arc::new(Self {
            start,
            primed,
            provider,
            workspace: workspace_path,
            last_used: std::sync::Mutex::new(Instant::now()),
            streak: std::sync::Mutex::new(TransientStreak::default()),
            stuck_after,
            stuck: AtomicBool::new(false),
        })
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
pub struct Manager {
    adapters: Vec<Arc<dyn LspAdapter>>,
    sessions: Arc<Sessions>,
    /// `lsp_stuck_seconds`: how long a session may answer nothing but transient
    /// errors before it is replaced. `None` never replaces it.
    stuck_after: Option<Duration>,
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
        if config.lsp_idle_timeout_seconds > 0 {
            spawn_idle_sweeper(
                Arc::downgrade(&sessions),
                Duration::from_secs(config.lsp_idle_timeout_seconds),
            );
        }
        let stuck_after =
            (config.lsp_stuck_seconds > 0).then(|| Duration::from_secs(config.lsp_stuck_seconds));
        Self {
            adapters,
            sessions,
            stuck_after,
        }
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
    async fn session(&self, adapter: &Arc<dyn LspAdapter>, workspace: &Workspace) -> Arc<Session> {
        let key = Self::session_key(adapter, workspace);
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(&key)
            && !existing.failed()
            && !existing.stuck()
        {
            existing.touch();
            return Arc::clone(existing);
        }
        let session = Session::begin(Arc::clone(adapter), workspace.clone(), self.stuck_after);
        if let Some(replaced) = sessions.insert(key, Arc::clone(&session))
            && replaced.stuck()
        {
            tokio::spawn(replaced.shutdown(SHUTDOWN_GRACE));
        }
        session
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
        let session = self.session(adapter, workspace).await;
        let client = session.client().await?;
        if after_priming {
            session.primed().await?;
        }
        // The start and priming can take far longer than the idle timeout
        // (a cold rust-analyzer): count the moment the caller finally has its
        // server, so the sweeper cannot release it just before the request.
        session.touch();
        Ok((session, client))
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
        let removed: Vec<Arc<Session>> = {
            let mut sessions = self.sessions.lock().await;
            sessions
                .extract_if(|(_, id), _| id == workspace_id)
                .map(|(_, session)| session)
                .collect()
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
        let removed: Vec<Arc<Session>> = {
            let mut sessions = self.sessions.lock().await;
            sessions.drain().map(|(_, session)| session).collect()
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
                sessions.remove(&key)
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
            result.map_err(|error| (Some(session), error))
        };
        match attempt.await {
            Ok(value) => Ok(value),
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
/// returns the providers that had a server to stop. Dropping this future
/// (the caller gave up) aborts the shutdowns, and dropping the clients then
/// kills any server that is left.
async fn shut_down(removed: Vec<Arc<Session>>) -> Vec<&'static str> {
    let mut shutdowns = JoinSet::new();
    for session in removed {
        let provider = session.provider;
        shutdowns.spawn(async move { (provider, session.shutdown(SHUTDOWN_GRACE).await) });
    }
    let mut stopped = Vec::new();
    while let Some(outcome) = shutdowns.join_next().await {
        if let Ok((provider, true)) = outcome {
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
fn spawn_idle_sweeper(registry: Weak<Sessions>, idle: Duration) {
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
            let expired: Vec<Arc<Session>> = sessions
                .lock()
                .await
                .extract_if(|_, session| session.idle_and_unused(idle))
                .map(|(_, session)| session)
                .collect();
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
            .map(|p| p.replace('\\', "/"));
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
    fn skips_locations_with_out_of_range_positions_or_invalid_uris() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = Workspace {
            id: "test".into(),
            path: dunce::canonicalize(temp.path()).unwrap(),
        };
        let file = workspace.path.join("src").join("lib.rs");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let uri = reqwest::Url::from_file_path(&file).unwrap().to_string();
        let result = locations_for(
            json!([
                {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}},
                {"uri":uri,"range":{"start":{"line":4294967295u64,"character":0},"end":{"line":0,"character":1}}},
                {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":4294967296u64}}},
                {"uri":"not a uri","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}}
            ]),
            &workspace,
            "rust-analyzer",
        )
        .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].relative_file_path.as_deref(), Some("src/lib.rs"));
        assert_eq!(result[0].start_line, 1);
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
    fn a_character_that_does_not_fit_u32_drops_only_its_location() {
        let result = locations_in_temp_workspace(|uri| {
            json!([
                {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}},
                {"uri":uri,"range":{"start":{"line":0,"character":4294967296u64},"end":{"line":0,"character":1}}},
                {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":4294967296u64}}},
            ])
        });
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].end_character, 1);
    }

    #[test]
    fn a_missing_or_negative_position_drops_only_its_location() {
        let result = locations_in_temp_workspace(|uri| {
            json!([
                {"uri":uri,"range":{"start":{"line":2,"character":3},"end":{"line":2,"character":5}}},
                {"uri":uri,"range":{"start":{"line":-1,"character":0},"end":{"line":0,"character":1}}},
                {"uri":uri,"range":{"start":{"character":0},"end":{"line":0,"character":1}}},
            ])
        });
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].start_line, 3);
    }

    #[test]
    fn a_uri_that_does_not_parse_drops_only_its_location() {
        let result = locations_in_temp_workspace(|uri| {
            json!([
                {"uri":"not a uri","range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}},
                {"uri":uri,"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}}},
            ])
        });
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].relative_file_path.as_deref(), Some("src/lib.rs"));
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
    async fn a_transient_error_is_repeated_until_the_server_answers() {
        let policy = fast_policy(Duration::from_secs(5));
        let (result, attempts, _) = ask(&policy, 1, RpcError::CONTENT_MODIFIED, 3).await;
        assert_eq!(result.unwrap(), json!("answer"));
        assert_eq!(attempts, 3);
    }

    #[tokio::test]
    async fn the_repeats_stop_after_the_last_wait() {
        let policy = fast_policy(Duration::from_secs(5));
        let (result, attempts, _) = ask(&policy, 1, RpcError::SERVER_CANCELLED, 99).await;
        let error = result.unwrap_err();
        assert_eq!(
            error.downcast_ref::<RpcError>().map(|e| e.code),
            Some(RpcError::SERVER_CANCELLED)
        );
        assert_eq!(attempts, 3, "the request and its two repeats");
    }

    #[tokio::test]
    async fn an_error_that_is_not_transient_is_not_repeated() {
        let policy = fast_policy(Duration::from_secs(5));
        let (result, attempts, _) = ask(&policy, 1, -32602, 99).await;
        assert!(result.is_err());
        assert_eq!(attempts, 1);
    }

    /// The window decides before the wait: a repeat that cannot start inside
    /// it is not even waited for.
    #[tokio::test]
    async fn a_repeat_whose_wait_does_not_fit_in_the_window_is_not_waited_for() {
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

    /// The window is checked again after the wait, because a timer can run
    /// late: here the 10 ms wait fits when it is decided on, but the timer
    /// returns after 60 ms, past the 50 ms window, so the repeat is not sent.
    /// Without the second check the repeat would go out (2 requests, not 1).
    #[tokio::test]
    async fn the_window_is_checked_again_after_the_wait() {
        let policy = fast_policy(Duration::from_millis(50));
        let (result, attempts, _) = ask(&policy, 6, RpcError::CONTENT_MODIFIED, 99).await;
        assert!(result.is_err());
        assert_eq!(
            attempts, 1,
            "no repeat may start after the window has passed"
        );
    }

    const LIMIT: Duration = Duration::from_secs(900);

    #[test]
    fn a_streak_needs_two_transient_answers_a_limit_apart() {
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

    /// A server that answers (a result, or an error that is not transient)
    /// is not stuck, and the span starts over with its next transient error.
    #[test]
    fn an_answer_ends_the_streak() {
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

    /// An answer that arrives with an earlier timestamp than the streak's start
    /// (clocks of concurrent requests) is no span at all, not a panic.
    #[test]
    fn a_transient_answer_before_the_streak_began_is_not_a_span() {
        let start = Instant::now();
        let mut streak = TransientStreak::default();
        assert_eq!(streak.transient(start + LIMIT, 0, LIMIT), None);
        assert_eq!(streak.transient(start, 0, LIMIT), None);
    }

    /// Progress means the server is working: a span that straddles a
    /// progress notification starts over at the answer that saw it, and only
    /// a span in which the count stayed the same can reach the limit.
    #[test]
    fn progress_restarts_the_streak() {
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

    #[test]
    fn the_repeat_waits_fit_inside_the_repeat_window() {
        // The window is measured from the first error response, so the waits
        // alone must not exhaust it: a server that answers at once would
        // otherwise never get its last repeat.
        let waits: Duration = SAME_SERVER_RETRY_WAITS.iter().sum();
        assert!(waits < SAME_SERVER_RETRY_WINDOW, "{waits:?}");
    }
}
