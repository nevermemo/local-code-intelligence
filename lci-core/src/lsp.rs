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
    sync::Arc,
    time::Instant,
};
use tokio::sync::{Mutex, watch};

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
struct Session {
    start: watch::Receiver<Option<StartOutcome>>,
    /// Flips to `true` when priming has settled, whether it succeeded or
    /// not. Never flips if the start failed.
    primed: watch::Receiver<bool>,
}

impl Session {
    fn begin(adapter: Arc<dyn LspAdapter>, workspace: Workspace) -> Arc<Self> {
        let (sender, start) = watch::channel(None);
        let (primed_sender, primed) = watch::channel(false);
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
        Arc::new(Self { start, primed })
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
pub struct Manager {
    adapters: Vec<Arc<dyn LspAdapter>>,
    sessions: Mutex<HashMap<SessionKey, Arc<Session>>>,
}

impl Manager {
    pub fn new(config: &Config) -> Self {
        let timeout = std::time::Duration::from_secs(config.lsp_timeout_seconds);
        let adapters: Vec<Arc<dyn LspAdapter>> = vec![
            Arc::new(RustServer::new(config)),
            Arc::new(CSharpServer::new(&config.csharp, timeout)),
            Arc::new(TypeScriptServer::new(&config.typescript, timeout)),
            Arc::new(PythonServer::new(&config.python, timeout)),
            Arc::new(GoServer::new(&config.go, timeout)),
            Arc::new(JavaServer::new(config, timeout)),
            Arc::new(ClangdServer::new(&config.clangd, timeout)),
        ];
        Self {
            adapters,
            sessions: Mutex::new(HashMap::new()),
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
    /// for later ones).
    async fn session(&self, adapter: &Arc<dyn LspAdapter>, workspace: &Workspace) -> Arc<Session> {
        let key = Self::session_key(adapter, workspace);
        let mut sessions = self.sessions.lock().await;
        if let Some(existing) = sessions.get(&key)
            && !existing.failed()
        {
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
        Ok((session, client))
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
