//! The shared contract every optional language-server integration
//! implements. Rust (rust-analyzer) and C# (csharp-ls) are always
//! constructed; disabled/unconfigured is expressed by `enabled()` returning
//! `false` and `transport_config`/`initialize_params` returning `None`, not
//! by omitting the adapter.

use crate::lsp::transport::{JsonRpcClient, RpcError, TransportConfig};
use anyhow::{Context, Result, anyhow, ensure};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock, PoisonError},
    time::{Duration, SystemTime},
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
    /// `textDocument/didOpen` when it differs from the text this session sent
    /// last (read just before the request, up to [`MAX_DID_OPEN_BYTES`]), so an
    /// unchanged file is not sent again, and the server takes that text even
    /// when the document is already open. Only then may the manager's outline cache
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

/// The texts one session has sent in `textDocument/didOpen`, by document URI,
/// as the content hash of each. A new session starts empty, so a restarted
/// server is sent its files again.
#[derive(Debug, Default)]
pub struct SentTexts(Mutex<HashMap<String, String>>);

impl SentTexts {
    /// Records `hash` as the text sent for `uri`. `true` when it differs from
    /// what was recorded (or nothing was), that is, when the text must be sent.
    pub fn changed(&self, uri: &str, hash: &str) -> bool {
        let mut sent = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if sent.get(uri).is_some_and(|known| known == hash) {
            return false;
        }
        sent.insert(uri.to_owned(), hash.to_owned());
        true
    }

    /// Forgets `uri`, so the next [`Self::changed`] answers `true` (a send
    /// that failed).
    fn forget(&self, uri: &str) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(uri);
    }
}

/// Sends `file`'s current disk text in `textDocument/didOpen` (version 1) unless
/// this session already sent that exact text, and answers the document URI
/// when it sent. `None`: the text is unchanged since the last send, or the file
/// is above [`MAX_DID_OPEN_BYTES`] and is not sent (see
/// [`read_for_did_open`]).
pub async fn open_if_changed(
    rpc: &JsonRpcClient,
    sent: &SentTexts,
    file: &Path,
    relative: &str,
    server: &str,
    language_id: &str,
) -> Result<Option<String>> {
    let Some(content) = read_for_did_open(file, relative, server).await? else {
        return Ok(None);
    };
    let uri = reqwest::Url::from_file_path(file)
        .map_err(|_| anyhow!("cannot convert file path to URI"))?
        .to_string();
    if !sent.changed(&uri, &crate::workspace::hash(&content)) {
        return Ok(None);
    }
    let notified = rpc
        .notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id,
                    "version": 1,
                    "text": content
                }
            }),
        )
        .await;
    if let Err(error) = notified {
        sent.forget(&uri);
        return Err(error);
    }
    Ok(Some(uri))
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
        command
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
    fn sent_texts_report_a_change_once_per_new_hash() {
        let sent = SentTexts::default();
        assert!(sent.changed("file:///a", "h1"), "first text");
        assert!(!sent.changed("file:///a", "h1"), "same text again");
        assert!(
            sent.changed("file:///b", "h1"),
            "another uri is its own entry"
        );
        assert!(sent.changed("file:///a", "h2"), "a new text");
        assert!(!sent.changed("file:///a", "h2"));
        assert!(
            sent.changed("file:///a", "h1"),
            "back to the old text is a change"
        );
        sent.forget("file:///a");
        assert!(
            sent.changed("file:///a", "h1"),
            "a forgotten uri is sent again"
        );
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
