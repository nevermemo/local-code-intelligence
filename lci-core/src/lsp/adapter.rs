//! The shared contract every optional language-server integration
//! implements. Rust (rust-analyzer) and C# (csharp-ls) are always
//! constructed; disabled/unconfigured is expressed by `enabled()` returning
//! `false` and `transport_config`/`initialize_params` returning `None`, not
//! by omitting the adapter.

use crate::lsp::transport::{JsonRpcClient, RpcError, TransportConfig};
use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde_json::Value;
use std::{path::Path, time::Duration};

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
    async fn before_position_request(
        &self,
        _rpc: &JsonRpcClient,
        _file: &Path,
        _relative: &str,
    ) -> Result<()> {
        Ok(())
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

/// Checks a configured executable with a bounded `--version` invocation.
pub async fn probe_subprocess_version(resolved: &Path, timeout: Duration) -> Result<()> {
    let mut command = tokio::process::Command::new(resolved);
    command.arg("--version");
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, command.output())
        .await
        .with_context(|| format!("timed out running {} --version", resolved.display()))?
        .with_context(|| format!("start {}", resolved.display()))?;
    ensure!(
        output.status.success(),
        "subprocess {} --version failed",
        resolved.display()
    );
    Ok(())
}
