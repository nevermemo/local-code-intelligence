//! The shared contract every optional language-server integration
//! implements. Rust (rust-analyzer) and C# (csharp-ls) are always
//! constructed; disabled/unconfigured is expressed by `enabled()` returning
//! `false` and `transport_config`/`initialize_params` returning `None`, not
//! by omitting the adapter.

use crate::lsp::transport::{JsonRpcClient, RpcError, TransportConfig};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

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

    /// The configured executable/command, for readiness detail messages.
    /// `None` when nothing is configured (as opposed to disabled).
    fn configured_command(&self) -> Option<String>;

    /// The `service_status`/`/ready` readiness component name for this
    /// adapter (e.g. `"rust_analyzer_available"`).
    fn readiness_component_name(&self) -> &'static str;

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
