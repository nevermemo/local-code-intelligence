use crate::{
    config::Config,
    language,
    lsp::{
        adapter::LspAdapter, clangd::ClangdServer, csharp::CSharpServer, go::GoServer,
        java::JavaServer, python::PythonServer, rust::RustServer, transport::JsonRpcClient,
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
    sync::{Arc, Weak},
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
        let rpc = JsonRpcClient::spawn(&transport, adapter.ready_predicate()).await?;
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
}

impl Session {
    fn begin(adapter: Arc<dyn LspAdapter>, workspace: Workspace) -> Arc<Self> {
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
        })
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
/// session transparently.
pub struct Manager {
    adapters: Vec<Arc<dyn LspAdapter>>,
    sessions: Arc<Sessions>,
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
        Self { adapters, sessions }
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
    /// for later ones). Asking for the session counts as using it, recorded
    /// under the registry lock, so the idle sweeper cannot release it between
    /// this caller finding it and the caller's first request.
    async fn session(&self, adapter: &Arc<dyn LspAdapter>, workspace: &Workspace) -> Arc<Session> {
        let key = Self::session_key(adapter, workspace);
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(&key)
            && !existing.failed()
        {
            existing.touch();
            return Arc::clone(existing);
        }
        let session = Session::begin(Arc::clone(adapter), workspace.clone());
        sessions.insert(key, Arc::clone(&session));
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

    /// Forgets a failed session so the next caller starts a fresh server.
    /// Only that exact session is removed: with concurrent callers, a second
    /// one reporting the same failure must not remove the replacement the
    /// first one already started.
    async fn discard(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        failed: &Arc<Session>,
    ) {
        let key = Self::session_key(adapter, workspace);
        let mut sessions = self.sessions.lock().await;
        if sessions
            .get(&key)
            .is_some_and(|current| Arc::ptr_eq(current, failed))
        {
            sessions.remove(&key);
        }
    }

    /// A request with no per-file preparation step: spawn-or-reuse a client,
    /// and on any failure discard the session and retry once with a fresh
    /// client (never a retry loop).
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
        match client.request(method, params.clone()).await {
            Ok(value) => Ok(value),
            Err(first) => {
                Self::warn_restart(adapter, workspace, method, &first);
                self.discard(adapter, workspace, &session).await;
                let (_, client) = self
                    .client(adapter, workspace, after_priming)
                    .await
                    .with_context(|| format!("{} restart after: {first:#}", adapter.provider()))?;
                client.request(method, params).await
            }
        }
    }

    /// Same retry contract as [`Self::request`], but runs the adapter's
    /// `before_position_request` hook (e.g. TypeScript's `textDocument/
    /// didOpen`) against the same client before every attempt.
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
                client.request(method, params.clone()).await
            }
            .await;
            result.map_err(|error| (Some(session), error))
        };
        match attempt.await {
            Ok(value) => Ok(value),
            Err((session, first)) => {
                Self::warn_restart(adapter, workspace, method, &first);
                if let Some(session) = session {
                    self.discard(adapter, workspace, &session).await;
                }
                let (_, client) = self
                    .client(adapter, workspace, false)
                    .await
                    .with_context(|| format!("{} restart after: {first:#}", adapter.provider()))?;
                adapter
                    .before_position_request(&client.rpc, file, relative)
                    .await?;
                client.request(method, params).await
            }
        }
    }

    /// A request failure throws away a warmed-up server, and the retry pays
    /// the whole start (and warm-up) again, so it is worth a log line: it is
    /// the likeliest cause of a sudden multi-second search.
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
        let url = reqwest::Url::parse(uri)?;
        let Ok(file_path) = url.to_file_path() else {
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
            start_line: range["start"]["line"].as_u64().unwrap_or(0) as u32 + 1,
            start_character: range["start"]["character"].as_u64().unwrap_or(0) as u32,
            end_line: range["end"]["line"].as_u64().unwrap_or(0) as u32 + 1,
            end_character: range["end"]["character"].as_u64().unwrap_or(0) as u32,
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
}
