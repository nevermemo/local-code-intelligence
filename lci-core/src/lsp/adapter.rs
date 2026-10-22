//! The shared contract every optional language-server integration
//! implements. Rust (rust-analyzer) and C# (csharp-ls) are always
//! constructed; disabled/unconfigured is expressed by `enabled()` returning
//! `false` and `transport_config`/`initialize_params` returning `None`, not
//! by omitting the adapter.

use crate::lsp::{
    ChangeSync,
    transport::{JsonRpcClient, RpcError, TransportConfig},
};
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant, SystemTime},
};

#[async_trait]
pub trait LspAdapter: Send + Sync {
    /// Human-readable provider name used in `Location.provider`, warnings,
    /// and readiness details (e.g. "rust-analyzer", "csharp-ls").
    fn provider(&self) -> &'static str;

    /// Every Tree-sitter language identifier this server can navigate (e.g.
    /// `["typescript", "tsx", "javascript", "jsx"]` for one TS/JS server).
    fn languages(&self) -> &'static [&'static str];

    /// Whether this server is configured and not explicitly disabled.
    fn enabled(&self) -> bool;

    /// The configured command or installation path, for readiness details.
    /// `None` when nothing is configured (as opposed to disabled).
    fn configured_command(&self) -> Option<String>;

    /// The `service_status`/`/ready` readiness component name for this
    /// adapter (e.g. `"rust_analyzer_available"`).
    fn readiness_component_name(&self) -> &'static str;

    /// Probe availability without starting an LSP session. Called only when
    /// enabled; adapters with an installation path override the version probe.
    async fn probe_readiness(&self, timeout: Duration) -> Result<()> {
        if let Some(command) = self.configured_command() {
            probe_subprocess_version(Path::new(&command), timeout).await?;
        }
        Ok(())
    }

    /// Build the transport for spawning this server in `root`. `None` when
    /// disabled or unconfigured; never spawns a process itself.
    fn transport_config(&self, root: &Path) -> Option<TransportConfig>;

    /// Child-process environment overrides; other variables remain inherited.
    fn environment(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Build the LSP `initialize` params for `root`. `None` when disabled.
    fn initialize_params(&self, root: &Path) -> Option<Value>;

    /// Predicate the transport evaluates against every notification to
    /// decide when the server is ready (e.g. rust-analyzer's
    /// `experimental/serverStatus`). Servers with no such signal return a
    /// predicate that never matches; readiness then relies on normal
    /// request timeouts.
    fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync>;

    /// Predicate the transport evaluates against every notification the
    /// readiness predicate did not match, to learn that the server is busy
    /// again after it was ready (rust-analyzer reports `quiescent: false`
    /// while it reloads the crate graph). It feeds the readiness report only:
    /// it never clears the ready latch and changes no request or answer.
    /// Default: never matches.
    fn busy_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        Box::new(|_| false)
    }

    /// Whether `error`, a JSON-RPC error response, is how this server says it
    /// is still loading, so that the same request is likely to succeed on the
    /// same server a moment later. `ready` is whether the readiness predicate
    /// (see [`ready_predicate`](Self::ready_predicate)) has matched yet.
    ///
    /// The manager already repeats a request for the error codes the protocol
    /// defines as transient (`ContentModified`, `ServerCancelled`,
    /// `RequestFailed`) on every server; this is for an error that is
    /// transient only for one server, or only until it is ready. Default:
    /// none.
    fn is_loading_error(&self, _error: &RpcError, _ready: bool) -> bool {
        false
    }

    /// Run right after the `initialized` notification, before any request.
    /// Default no-op; pyright uses this to send an empty
    /// `workspace/didChangeConfiguration`.
    async fn after_initialized(&self, _rpc: &JsonRpcClient) -> Result<()> {
        Ok(())
    }

    /// Run before a `textDocument/definition` or `textDocument/references`
    /// request on a specific file. Default no-op; the TypeScript/JavaScript
    /// adapter uses this to send `textDocument/didOpen`, since tsserver only
    /// knows about files it has been told are open.
    ///
    /// `sent` is the session's table of the texts already sent (see
    /// [`SentTexts`]); a hook that opens documents goes through
    /// [`open_if_changed`], so a file whose text did not change since this
    /// session sent it is not opened again.
    async fn before_position_request(
        &self,
        _rpc: &JsonRpcClient,
        _sent: &SentTexts,
        _file: &Path,
        _relative: &str,
    ) -> Result<()> {
        Ok(())
    }

    /// Whether the server holds the file's current disk text at every
    /// position request: [`Self::before_position_request`] sends it in a
    /// `textDocument/didOpen` (or `textDocument/didChange` for a text that
    /// changed, see [`open_if_changed`]) when it differs from the text this
    /// session sent last (read just before the request, up to
    /// [`MAX_DID_OPEN_BYTES`]), so an unchanged file is not sent again, and the
    /// server takes that text even when the document is already open. Only then may the manager's outline cache
    /// (`[run_profile.lsp] outline_cache`) keep an answer under the hash of
    /// the bytes on disk. Default `false`: a server that is told nothing
    /// (csharp-ls), or that watches the disk itself and can lag an edit
    /// (rust-analyzer), could give the old outline for the new bytes.
    fn opens_documents(&self) -> bool {
        false
    }

    /// Whether the server learns of an edit to any file of its workspace on
    /// its own, without the client resending text or sending
    /// `workspace/didChangeWatchedFiles`: it runs its own file watcher. Only
    /// such a server may keep one session across traverse calls under the
    /// default `[run_profile.lsp] sessions = "shared"` without `sync =
    /// "watch"`, and then only with a [`Self::stamp_manifests`] list
    /// ([`Self::keeps_session`]); every other server is started per call. (A
    /// kept session also passes the tree barrier of each walk,
    /// `Manager::tree_barrier`, so a reused server is never staler than under
    /// `per_call` for a change the stamp covers, whatever the watcher does.) A
    /// server that is resent only the requested file
    /// ([`Self::opens_documents`]) does not qualify: an edit
    /// to another file stays unseen. Default `false`; an override names its
    /// capability evidence.
    fn watches_workspace(&self) -> bool {
        false
    }

    /// The project configuration file names that change what the server sees
    /// without changing a source file (rust-analyzer: its crate graph and its
    /// settings; the TypeScript server: `tsconfig.json`). A name, or a name
    /// with one `*` (`tsconfig.*.json`). The tree stamp (`lsp::stamp`) covers
    /// each match among the children of every directory that holds one of the
    /// server's indexed files, and of that directory's ancestors. Default
    /// none, and a server with none is never shared (see
    /// [`Self::keeps_session`]): an edit to its configuration would go
    /// unseen by the barrier.
    fn stamp_manifests(&self) -> &'static [&'static str] {
        &[]
    }

    /// Whether a traverse call keeps this server's session on the service's
    /// manager under `sessions = "shared"`: the server declares the
    /// configuration files its tree stamp covers
    /// ([`Self::stamp_manifests`], non-empty) and it sees an edit on its own
    /// ([`Self::watches_workspace`]) or `synced` (`sync = "watch"` tells it).
    /// Any other server starts per call.
    fn keeps_session(&self, synced: bool) -> bool {
        !self.stamp_manifests().is_empty() && (synced || self.watches_workspace())
    }

    /// Whether the search's LSP channel may start this server for a query
    /// when a chunk of one of its languages is among the candidates. The
    /// channel asks every applicable server for symbols named like the query;
    /// a server whose symbols are not code names (marksman: headings) opts
    /// out with `false`, so an indexed prose file does not start it for every
    /// search (the lsp.symbols map picks its providers the same way and skips
    /// it too). Navigation (`definition`, `references`, `search_symbols`) is
    /// unaffected. Default `true`.
    fn joins_search_channel(&self) -> bool {
        true
    }
}

/// The largest file a `before_position_request` hook reads and sends whole in
/// `textDocument/didOpen`. While it is sent the text is held about three
/// times over (the read, the JSON value the transport clones, and the frame),
/// so a huge generated file would cost several times its size in memory.
/// Above the cap the hook skips the `didOpen` and the request goes out
/// anyway; a server that needs the file open answers it empty or with an
/// error, and navigation fails open on either. A constant until indexing has
/// a per-file size cap to share (roadmap N18 part f).
pub const MAX_DID_OPEN_BYTES: u64 = 16 * 1024 * 1024;

/// The most documents one server session holds open at a time. Opening a
/// new file past it first closes the least recently used one that has no
/// open in flight (`textDocument/didClose`), so a long-lived session over a
/// large tree does not keep every file it ever navigated in the server's
/// memory. A size, not a behaviour: a closed file is sent again (as a new
/// `didOpen`, version 1) when it is next navigated, and no answer changes.
pub const MAX_OPEN_DOCUMENTS: usize = 64;

/// The longest a hook waits for the server's `publishDiagnostics` after a
/// `didOpen` (the wait is also bounded by the configured request timeout, and
/// is the smaller of the two): the worst case of the fixed head start it
/// replaced. To be lifted once a real-server run shows
/// `diagnostics_wait_timeouts == 0` for the servers that wait (K11d-2).
pub const MAX_DIAGNOSTICS_WAIT: Duration = Duration::from_secs(3);

/// Reads `file` for a `textDocument/didOpen` to `server`, or `None` when it is
/// larger than [`MAX_DID_OPEN_BYTES`]. The read itself stops one byte past
/// the cap, so no separate size check is needed (and a file that grows
/// while it is read cannot get past it).
pub async fn read_for_did_open(
    file: &Path,
    relative: &str,
    server: &str,
) -> Result<Option<String>> {
    use tokio::io::AsyncReadExt;
    let context = || format!("read {relative} to open in {server}");
    let mut bytes = Vec::new();
    tokio::fs::File::open(file)
        .await
        .with_context(context)?
        .take(MAX_DID_OPEN_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .with_context(context)?;
    if bytes.len() as u64 > MAX_DID_OPEN_BYTES {
        tracing::warn!(
            server,
            file = relative,
            limit_bytes = MAX_DID_OPEN_BYTES,
            "file too large to open in the language server; sending the request without it"
        );
        return Ok(None);
    }
    String::from_utf8(bytes).map(Some).with_context(context)
}

/// The texts one session has sent in `textDocument/didOpen` or
/// `textDocument/didChange`, by document URI, as the content hash and the
/// `version` of each. A text is recorded only once its send is on the pipe, so
/// a failed or abandoned send is sent again. A new session starts empty, so a
/// restarted server is sent its files again.
///
/// It also holds one gate per URI. [`open_if_changed`] takes it before the
/// comparison and an [`OpenedDocument`] keeps it until the hook that sent the
/// text is done (its diagnostics wait included), so a concurrent request for the
/// same file waits for an open in flight instead of skipping it and reaching
/// the server before the text or before the server has taken it in. The table
/// holds at most [`MAX_OPEN_DOCUMENTS`] files: a new file past that evicts the
/// least recently used one that has no open in flight with a `didClose`, and
/// the evicted file's gate goes with its entry.
#[derive(Debug, Default)]
pub struct SentTexts {
    texts: Arc<Mutex<HashMap<String, SentText>>>,
    gates: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Held while a file new to the table is given its place (the eviction
    /// that makes room, then its `didOpen`), so concurrent new files cannot
    /// each see room and together overfill the table.
    open_set: tokio::sync::Mutex<()>,
    /// The files closed to make room and not opened since, so the open of one
    /// of them is told from a first open ([`SentText::reopened`]).
    closed: Arc<Mutex<HashSet<String>>>,
    /// What the server advertised for a changed document: `didChange` with the
    /// whole text under `Full` and `Incremental`, a second `didOpen` under
    /// `None` (the default, and what a server without the capability is sent).
    sync: ChangeSync,
}

/// What one send of a document carried, as the session recorded it.
#[derive(Debug, Clone)]
struct SentText {
    hash: String,
    version: i64,
    /// The `diagnostics_before` reading of the send, until a hook has
    /// finished waiting for the server's publication for it
    /// ([`OpenedDocument::wait_for_diagnostics`] clears it). While it is set, the
    /// next caller of [`open_if_changed`] for the uri gets the wait handed to
    /// it, so a hook cancelled after its write (or one that never waits, like
    /// jdtls: it stays set, each later call of that hook for the file gets an
    /// [`OpenedDocument`] it ignores, which costs nothing) does not leave the
    /// text unwaited for.
    pending: Option<u64>,
    /// Whether the send is the open of a file the session had closed to make
    /// room: its diagnostics wait then takes only a publication that states a
    /// version ([`JsonRpcClient::wait_diagnostics_reopened`]).
    reopened: bool,
    /// When a caller of [`open_if_changed`] last asked for the uri, whether it
    /// sent or found the text current: the order of eviction.
    last_used: Instant,
}

/// A document this hook just sent in `textDocument/didOpen` or
/// `textDocument/didChange`, or one whose send an earlier hook did not finish
/// waiting for. Holding it keeps the URI's gate, so a hook that waits after
/// the send (for the server's diagnostics, see
/// [`Self::wait_for_diagnostics`]) keeps the waiting inside the value, and
/// drops it when the wait is over.
#[derive(Debug)]
pub struct OpenedDocument {
    /// The document URI.
    pub uri: String,
    /// How many `publishDiagnostics` the server had sent for the URI when the
    /// `didOpen` was written, read just before the write: a publication
    /// counted past it answers this open.
    diagnostics_before: u64,
    /// The `version` the `didOpen` carried: 1 for the first text of the
    /// document in the session, then one more for each text sent after it, so
    /// a publication that names an older version is not for this text.
    version: i64,
    /// Whether this is the open of a file the session closed before (see
    /// [`SentText::reopened`]).
    reopened: bool,
    texts: Arc<Mutex<HashMap<String, SentText>>>,
    _gate: tokio::sync::OwnedMutexGuard<()>,
}

impl OpenedDocument {
    /// Waits for the server's `publishDiagnostics` for this document after the
    /// send, the event that says the server has taken the text in, for at
    /// most `cap` ([`JsonRpcClient::wait_diagnostics`]). `false` when the cap
    /// passed first. A best-effort signal, not proof that the server holds the
    /// current text (see [`crate::lsp::transport::Diagnostics`]). The caller's
    /// own deadline cancels the future, and then the wait is left pending for
    /// the next caller of the uri; a wait that ended, either way, is settled
    /// and not repeated for the same text.
    pub async fn wait_for_diagnostics(&self, rpc: &JsonRpcClient, cap: Duration) -> bool {
        let answered = if self.reopened {
            // A server that publishes on `didClose` sends that publication
            // without a version, possibly after this `didOpen` was written.
            rpc.wait_diagnostics_reopened(&self.uri, self.diagnostics_before, self.version, cap)
                .await
        } else {
            rpc.wait_diagnostics(&self.uri, self.diagnostics_before, self.version, cap)
                .await
        };
        if let Some(sent) = self
            .texts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(&self.uri)
            .filter(|sent| sent.version == self.version)
        {
            sent.pending = None;
        }
        answered
    }
}

impl SentTexts {
    /// An empty table for a server that advertised `sync` for a changed
    /// document (the default table is [`ChangeSync::None`]).
    pub fn with_sync(sync: ChangeSync) -> Self {
        Self {
            sync,
            ..Self::default()
        }
    }

    /// Waits for any open of `uri` in flight, then holds the URI's gate. It
    /// costs nothing while no hook is between its `didOpen` and its release.
    async fn gate(&self, uri: &str) -> tokio::sync::OwnedMutexGuard<()> {
        let gate = Arc::clone(
            self.gates
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(uri.to_owned())
                .or_default(),
        );
        gate.lock_owned().await
    }

    /// How many callers of [`open_if_changed`] wait behind an open of `uri`
    /// in flight, that is, behind a live [`OpenedDocument`]. Counted from the
    /// gate's owners, so a caller that is about to wait counts too. For logs
    /// and for tests that must know a request is parked before they let the
    /// open finish.
    pub fn waiting(&self, uri: &str) -> usize {
        self.gates
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(uri)
            // The map's own handle and the holder's guard are not waiters.
            .map_or(0, |gate| Arc::strong_count(gate).saturating_sub(2))
    }

    /// Whether `hash` is the text this session last sent for `uri`.
    fn is_current(&self, uri: &str, hash: &str) -> bool {
        self.texts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(uri)
            .is_some_and(|sent| sent.hash == hash)
    }

    /// The `version` the next send of `uri` carries.
    fn next_version(&self, uri: &str) -> i64 {
        self.texts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(uri)
            .map_or(1, |sent| sent.version + 1)
    }

    /// The `diagnostics_before` reading and the `version` of the last send of
    /// `uri` when no hook has finished waiting for the server's publication
    /// for it.
    fn pending_wait(&self, uri: &str) -> Option<(u64, i64, bool)> {
        let texts = self.texts.lock().unwrap_or_else(PoisonError::into_inner);
        let sent = texts.get(uri)?;
        sent.pending
            .map(|before| (before, sent.version, sent.reopened))
    }

    /// Whether the session closed `uri` to make room and has not opened it
    /// since.
    fn was_closed(&self, uri: &str) -> bool {
        self.closed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(uri)
    }

    /// Marks `uri` as just asked for, if the session has sent it.
    fn touch(&self, uri: &str) {
        if let Some(sent) = self
            .texts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(uri)
        {
            sent.last_used = Instant::now();
        }
    }

    /// How many files the table holds.
    fn len(&self) -> usize {
        self.texts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// The least recently used file in the table whose gate is free, with the
    /// gate taken, so no open of it can start while it is closed. `None` when
    /// every gate is held (opens in flight on all of them).
    fn eviction_candidate(&self) -> Option<(String, tokio::sync::OwnedMutexGuard<()>)> {
        let mut by_age: Vec<(Instant, String)> = self
            .texts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .map(|(uri, sent)| (sent.last_used, uri.clone()))
            .collect();
        by_age.sort();
        by_age.into_iter().find_map(|(_, uri)| {
            let gate = Arc::clone(
                self.gates
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .entry(uri.clone())
                    .or_default(),
            );
            gate.try_lock_owned().ok().map(|guard| (uri, guard))
        })
    }

    /// Drops the gate of an evicted `uri` from the map, unless a caller has
    /// since started waiting on it (then that caller needs the very same gate
    /// to stay exclusive with whoever opens the file next). `guard` is the
    /// gate's own guard, held across the check: the map's handle and the
    /// guard's are the two when nobody else has a handle.
    fn forget_gate(&self, uri: &str, guard: tokio::sync::OwnedMutexGuard<()>) {
        let mut gates = self.gates.lock().unwrap_or_else(PoisonError::into_inner);
        if gates
            .get(uri)
            .is_some_and(|gate| Arc::strong_count(gate) == 2)
        {
            gates.remove(uri);
        }
        drop(gates);
        drop(guard);
    }

    /// Makes room for a file new to the table: while it holds
    /// [`MAX_OPEN_DOCUMENTS`] files, closes the least recently used one that
    /// has no open in flight (`textDocument/didClose`, written before this
    /// returns, so it precedes the caller's `didOpen` on the pipe) and forgets
    /// it. Stops, leaving the table over the bound until the next new file,
    /// when every file has an open in flight or a close could not be written.
    /// The returned guard keeps other new files out until the caller's own
    /// send has been recorded.
    async fn make_room(&self, rpc: &JsonRpcClient) -> tokio::sync::MutexGuard<'_, ()> {
        let room = self.open_set.lock().await;
        while self.len() >= MAX_OPEN_DOCUMENTS {
            let Some((uri, gate)) = self.eviction_candidate() else {
                tracing::debug!(
                    open = self.len(),
                    "every open document has an open in flight; none is closed"
                );
                break;
            };
            let (texts, closed) = (Arc::clone(&self.texts), Arc::clone(&self.closed));
            let evicted = uri.clone();
            let closed = rpc
                .notify_holding(
                    "textDocument/didClose",
                    json!({"textDocument": {"uri": uri}}),
                    gate,
                    move || {
                        texts
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .remove(&evicted);
                        closed
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(evicted);
                    },
                )
                .await;
            match closed {
                Ok(gate) => self.forget_gate(&uri, gate),
                Err(error) => {
                    // The entry stays, so the next new file closes it again.
                    tracing::debug!(%error, uri, "could not close an open document");
                    break;
                }
            }
        }
        room
    }

    /// What records `hash` and `version` as sent for `uri` (with the wait for
    /// its publication still pending), to run once the send is written (it
    /// outlives the caller, see [`JsonRpcClient::notify_holding`]).
    fn recorder(
        &self,
        uri: &str,
        hash: String,
        version: i64,
        diagnostics_before: u64,
    ) -> impl FnOnce() + Send + 'static {
        let reopened = version == 1 && self.was_closed(uri);
        let (texts, closed, uri) = (
            Arc::clone(&self.texts),
            Arc::clone(&self.closed),
            uri.to_owned(),
        );
        move || {
            closed
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .remove(&uri);
            texts.lock().unwrap_or_else(PoisonError::into_inner).insert(
                uri,
                SentText {
                    hash,
                    version,
                    pending: Some(diagnostics_before),
                    reopened,
                    last_used: Instant::now(),
                },
            );
        }
    }
}

/// Sends `file`'s current disk text unless this session already sent that
/// exact text, and answers the document when it sent (or when an earlier
/// send's wait for the server's diagnostics was never finished, see
/// [`SentText::pending`]). The first text of a file goes in
/// `textDocument/didOpen`; a changed one goes in `textDocument/didChange`
/// with the whole text when the server advertised change sync (`Full` or
/// `Incremental`: LSP 3.17 reads a change entry without a `range` as the whole
/// text under either kind), and in a second `didOpen` with the next `version`
/// when it did not (pyright replaces the text on a repeated open;
/// typescript-language-server refuses it, which is why it needs the change).
/// The text is recorded as sent when the write has finished, not before,
/// and the URI's gate is held until then even when the caller is cancelled
/// mid-write (the write runs on a task that owns the gate), so a later caller
/// never sees the text as sent while its send is still to be written. `None`:
/// the text is unchanged since the last send and its wait is settled, or the
/// file is above [`MAX_DID_OPEN_BYTES`] and is not sent (see
/// [`read_for_did_open`]).
///
/// The call first waits for any open of the same URI in flight on this
/// session, so a request that finds the text already sent returns only after
/// the `didOpen` has been written (the write is awaited to the end) and after
/// the sending hook let go of its [`OpenedDocument`], diagnostics wait included.
/// Hold the returned value across a wait that must also hold other requests
/// for the file back; drop it when the wait is over. The diagnostics count the
/// wait needs is read here, after the gate is taken and just before the
/// `didOpen` is written, so a publication cannot slip in between the reading
/// and the write.
pub async fn open_if_changed(
    rpc: &JsonRpcClient,
    sent: &SentTexts,
    file: &Path,
    relative: &str,
    server: &str,
    language_id: &str,
) -> Result<Option<OpenedDocument>> {
    let uri = reqwest::Url::from_file_path(file)
        .map_err(|_| anyhow!("cannot convert file path to URI"))?
        .to_string();
    let gate = sent.gate(&uri).await;
    sent.touch(&uri);
    let Some(content) = read_for_did_open(file, relative, server).await? else {
        return Ok(None);
    };
    let hash = crate::workspace::hash(&content);
    if sent.is_current(&uri, &hash) {
        // Nothing to send; a wait an earlier caller left unfinished (it was
        // cancelled after its write, or its hook never waits) is this one's.
        return Ok(sent
            .pending_wait(&uri)
            .map(|(diagnostics_before, version, reopened)| OpenedDocument {
                uri,
                diagnostics_before,
                version,
                reopened,
                texts: Arc::clone(&sent.texts),
                _gate: gate,
            }));
    }
    let version = sent.next_version(&uri);
    // A file new to the table takes its place in the open set first: when the
    // table is full, the least recently used file is closed (its `didClose` is
    // written before this file's `didOpen`).
    let _place = if version == 1 {
        Some(sent.make_room(rpc).await)
    } else {
        None
    };
    // The file was closed to make room earlier: a server that publishes on
    // `didClose` may still owe that (versionless) publication.
    let reopened = version == 1 && sent.was_closed(&uri);
    let diagnostics_before = rpc.diagnostics_seen(&uri);
    let (method, params) = if version > 1 && sent.sync != ChangeSync::None {
        (
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri, "version": version},
                "contentChanges": [{"text": content}]
            }),
        )
    } else {
        (
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": version,
                    "text": content
                }
            }),
        )
    };
    let gate = rpc
        .notify_holding(
            method,
            params,
            gate,
            sent.recorder(&uri, hash, version, diagnostics_before),
        )
        .await?;
    Ok(Some(OpenedDocument {
        uri,
        diagnostics_before,
        version,
        reopened,
        texts: Arc::clone(&sent.texts),
        _gate: gate,
    }))
}

/// What identifies a binary for the process-wide probe memo. The invocation is
/// always `--version`, so the arguments are not part of the key. The path is
/// the resolved string as given, not canonicalised: another spelling of the
/// same file (case or a relative form on Windows) is probed again, and on a
/// platform without lookup a bare name that matches an entry of the working
/// directory is memoised against that entry's metadata.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct ProbeKey {
    /// The executable after `resolve_executable`.
    path: PathBuf,
    modified: Option<SystemTime>,
    len: u64,
}

/// The binaries whose `--version` has succeeded in this process.
static PROBED: OnceLock<Mutex<HashSet<ProbeKey>>> = OnceLock::new();

/// Answers `Ok` for a binary that has already passed a probe in this process,
/// else awaits `run` and remembers `key` only when it succeeds. A failure is
/// never remembered. Concurrent first callers may both run.
pub(crate) async fn probe_memoised(
    key: ProbeKey,
    run: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    let probed = PROBED.get_or_init(Default::default);
    let known = |probed: &Mutex<HashSet<ProbeKey>>| {
        probed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .contains(&key)
    };
    if known(probed) {
        return Ok(());
    }
    run.await?;
    probed
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(key);
    Ok(())
}

tokio::task_local! {
    /// Set by [`fresh_probes`]: the probes of the scope bypass the memo.
    static FRESH: bool;
}

/// Runs `work` with every [`probe_subprocess_version`] inside it answering from
/// a real `--version` run, never from the process-wide memo, and without
/// recording it. For a caller that reports the binary's state as it is now, as
/// `/ready` does.
pub async fn fresh_probes<F: std::future::Future>(work: F) -> F::Output {
    FRESH.scope(true, work).await
}

/// Answers `run` through the process-wide memo for the binary at `resolved`,
/// or runs it outright under [`fresh_probes`] or when the file cannot be read.
pub(crate) async fn probe_through(
    resolved: &std::ffi::OsStr,
    run: impl std::future::Future<Output = Result<()>>,
) -> Result<()> {
    let fresh = FRESH.try_with(|fresh| *fresh).unwrap_or(false);
    let key = if fresh {
        None
    } else {
        std::fs::metadata(resolved).ok().map(|metadata| ProbeKey {
            path: PathBuf::from(resolved),
            modified: metadata.modified().ok(),
            len: metadata.len(),
        })
    };
    match key {
        Some(key) => probe_memoised(key, run).await,
        None => run.await,
    }
}

/// Checks a configured executable with a bounded `--version` invocation. A
/// binary that has passed it once in this process (same resolved path, same
/// modification time and size) is not run again, whichever manager asks, so a
/// test process or a service probes one binary once, not once per manager. A
/// binary whose file cannot be read is probed every time, and so is every
/// binary inside [`fresh_probes`].
pub async fn probe_subprocess_version(executable: &Path, timeout: Duration) -> Result<()> {
    let resolved = crate::lsp::transport::resolve_executable(&executable.to_string_lossy());
    let run = async {
        let mut command = tokio::process::Command::new(&resolved);
        command.arg("--version");
        // tokio's `output()` pipes only stdout and stderr, so without this the
        // child inherits our stdin: under the MCP stdio transport that is the
        // client's JSON-RPC stream (N107).
        command
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let output = tokio::time::timeout(timeout, command.output())
            .await
            .with_context(|| format!("timed out running {} --version", executable.display()))?
            .with_context(|| format!("start {}", executable.display()))?;
        ensure!(
            output.status.success(),
            "subprocess {} --version failed",
            executable.display()
        );
        Ok(())
    };
    probe_through(&resolved, run).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn sent_texts_are_current_only_once_recorded() {
        let sent = SentTexts::default();
        assert!(!sent.is_current("file:///a", "h1"), "nothing recorded yet");
        assert_eq!(sent.next_version("file:///a"), 1);
        sent.recorder("file:///a", "h1".into(), 1, 0)();
        assert!(sent.is_current("file:///a", "h1"), "same text again");
        assert!(
            !sent.is_current("file:///b", "h1"),
            "another uri is its own entry"
        );
        assert_eq!(sent.next_version("file:///a"), 2);
        assert!(!sent.is_current("file:///a", "h2"), "a new text");
        sent.recorder("file:///a", "h2".into(), 2, 0)();
        assert!(sent.is_current("file:///a", "h2"));
        assert!(
            !sent.is_current("file:///a", "h1"),
            "back to the old text is a change"
        );
        assert_eq!(sent.next_version("file:///a"), 3);
    }

    #[tokio::test]
    async fn the_least_recently_used_entry_with_a_free_gate_is_the_eviction_candidate() {
        let sent = SentTexts::default();
        // Back-dated, so that a touch (now) is newer than every staged entry.
        let base = Instant::now()
            .checked_sub(Duration::from_secs(60))
            .expect("the clock has run for a minute");
        for (index, uri) in ["file:///a", "file:///b", "file:///c"]
            .into_iter()
            .enumerate()
        {
            // Staged ages: `a` is the oldest, then `b`, then `c`.
            sent.texts.lock().unwrap().insert(
                uri.to_owned(),
                SentText {
                    hash: "h".into(),
                    version: 1,
                    pending: None,
                    reopened: false,
                    last_used: base + Duration::from_secs(index as u64),
                },
            );
        }
        assert_eq!(sent.len(), 3);

        // The oldest has an open in flight: its gate is held, so the next
        // oldest is the candidate.
        let held = sent.gate("file:///a").await;
        let (uri, _guard) = sent.eviction_candidate().expect("a free gate exists");
        assert_eq!(uri, "file:///b");
        drop(_guard);
        drop(held);

        // None held: the oldest.
        let (uri, _guard) = sent.eviction_candidate().expect("a free gate exists");
        assert_eq!(uri, "file:///a");
        drop(_guard);

        // A touch moves a file to the back of the order.
        sent.touch("file:///a");
        let (uri, _guard) = sent.eviction_candidate().expect("a free gate exists");
        assert_eq!(uri, "file:///b");
        drop(_guard);

        // Every gate held: no candidate.
        let all = [
            sent.gate("file:///a").await,
            sent.gate("file:///b").await,
            sent.gate("file:///c").await,
        ];
        assert!(sent.eviction_candidate().is_none());
        drop(all);
    }

    #[tokio::test]
    async fn a_binary_is_probed_once_per_process_until_its_identity_changes() {
        let key = |modified: u64| ProbeKey {
            path: PathBuf::from("a-binary-only-this-test-names"),
            modified: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(modified)),
            len: 7,
        };
        let runs = AtomicUsize::new(0);
        let run = |outcome: Result<()>| {
            runs.fetch_add(1, Ordering::SeqCst);
            std::future::ready(outcome)
        };
        // A failure runs and is not remembered.
        assert!(
            probe_memoised(key(1), async { run(Err(anyhow::anyhow!("no"))).await })
                .await
                .is_err()
        );
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        // The next caller runs again, succeeds, and is remembered.
        probe_memoised(key(1), async { run(Ok(())).await })
            .await
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        // The same identity is answered from memory.
        probe_memoised(key(1), async { run(Ok(())).await })
            .await
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        // A binary that changed on disk is probed again.
        probe_memoised(key(2), async { run(Ok(())).await })
            .await
            .unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }

    /// `/ready` reports a binary as it is now: inside `fresh_probes` the run
    /// happens even after a memoised success, and it records nothing. The file
    /// is this test binary, which only this test probes through the memo.
    #[tokio::test]
    async fn fresh_probes_run_again_after_a_memoised_success() {
        let exe = std::env::current_exe().unwrap().into_os_string();
        let runs = AtomicUsize::new(0);
        let probe = || {
            probe_through(&exe, async {
                runs.fetch_add(1, Ordering::SeqCst);
                Ok(())
            })
        };
        probe().await.unwrap();
        probe().await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 1, "the second is memoised");
        fresh_probes(probe()).await.unwrap();
        fresh_probes(probe()).await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 3, "each fresh probe runs");
        // Outside the scope the memo answers again.
        probe().await.unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 3);
    }
}

/// Checks the unit tests of the optional adapters share.
#[cfg(test)]
pub(crate) mod contract {
    use super::LspAdapter;
    use std::path::Path;

    /// An adapter is enabled only with a usable path that is not disabled; a
    /// missing or blank path is never silently enabled and yields no
    /// transport. `make` builds the adapter from a path and a disabled flag.
    pub(crate) fn assert_enabled_only_with_a_usable_path<A: LspAdapter>(
        make: impl Fn(Option<&str>, bool) -> A,
        usable_path: &str,
    ) {
        assert!(make(Some(usable_path), false).enabled(), "a usable path");
        for (name, path) in [("missing", None), ("blank", Some("   "))] {
            let adapter = make(path, false);
            assert!(!adapter.enabled(), "{name} path");
            assert!(
                adapter.transport_config(Path::new("/tmp")).is_none(),
                "{name} path"
            );
        }
        assert!(
            !make(Some(usable_path), true).enabled(),
            "explicitly disabled even with a path"
        );
    }

    /// The initialize payload can be built without a process.
    pub(crate) fn assert_initialize_params_name_this_process(
        adapter: &impl LspAdapter,
        root: &Path,
    ) {
        let params = adapter.initialize_params(root).unwrap();
        assert_eq!(params["processId"], std::process::id());
        assert!(params["rootUri"].as_str().is_some());
    }
}
