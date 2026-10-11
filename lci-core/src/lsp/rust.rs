//! rust-analyzer adapter. Rust navigation has no enable/disable knob (unlike
//! the optional C#/TypeScript/Python servers): `rust_analyzer_path` always
//! has a default value and `Config::validate` requires it nonempty, so this
//! adapter is always "enabled" — the underlying executable may simply not be
//! installed, which readiness/error reporting surfaces at request time.

use crate::config::Config;
use crate::lsp::adapter::LspAdapter;
use crate::lsp::transport::{JsonRpcClient, RpcError, TransportConfig};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    path::Path,
    time::{Duration, Instant},
};

pub const LANGUAGE_IDENTIFIER: &str = "rust";

#[derive(Debug, Clone)]
pub struct RustServer {
    path: String,
    timeout: Duration,
}

impl RustServer {
    pub fn new(config: &Config) -> Self {
        Self {
            path: config.rust_analyzer_path.clone(),
            timeout: Duration::from_secs(config.lsp_timeout_seconds),
        }
    }
}

#[async_trait]
impl LspAdapter for RustServer {
    fn provider(&self) -> &'static str {
        "rust-analyzer"
    }

    fn languages(&self) -> &'static [&'static str] {
        &[LANGUAGE_IDENTIFIER]
    }

    fn enabled(&self) -> bool {
        true
    }

    fn configured_command(&self) -> Option<String> {
        Some(self.path.clone())
    }

    fn readiness_component_name(&self) -> &'static str {
        "rust_analyzer_available"
    }

    fn transport_config(&self, root: &Path) -> Option<TransportConfig> {
        Some(TransportConfig {
            executable: self.path.clone(),
            args: Vec::new(),
            working_dir: root.to_path_buf(),
            timeout: self.timeout,
            label: "rust-analyzer".into(),
        })
    }

    fn initialize_params(&self, root: &Path) -> Option<Value> {
        let root_uri = reqwest::Url::from_directory_path(root).ok()?.to_string();
        Some(json!({
            "processId": std::process::id(), "rootUri": &root_uri,
            "capabilities": {"workspace":{"symbol":{"resolveSupport":{"properties":["location.range"]}}},"experimental":{"serverStatusNotification":true}},
            "workspaceFolders":[{"uri":&root_uri,"name":root.file_name().and_then(|v|v.to_str()).unwrap_or("workspace")}],
            "initializationOptions":{"workspace":{"symbol":{"search":{"scope":"workspace","kind":"all_symbols","limit":256}}}}
        }))
    }

    fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        Box::new(|msg: &Value| {
            msg.get("method").and_then(Value::as_str) == Some("experimental/serverStatus")
                && msg["params"]["quiescent"].as_bool() == Some(true)
        })
    }

    fn is_loading_error(&self, error: &RpcError, ready: bool) -> bool {
        // While it loads the workspace, rust-analyzer answers a request for a
        // file it has not seen yet with an internal error, "file not found:
        // <path>" (observed on `textDocument/definition` right after the
        // readiness wait expired). Once it has reported quiescence the same
        // text means the file really is unknown to it.
        !ready && error.code == RpcError::INTERNAL_ERROR && error.message.contains("file not found")
    }

    async fn after_initialized(&self, rpc: &JsonRpcClient) -> Result<()> {
        if !rpc.is_ready() {
            // Some rust-analyzer builds do not emit serverStatus even when
            // the capability is advertised. Give crate discovery a bounded
            // head start, then rely on normal request timeouts and restart
            // recovery.
            //
            // This runs inside the session's detached start, never on a
            // search's own future: a search waits for it only up to its
            // `lsp_search_budget_ms`, and the wait carries on afterwards.
            let readiness_wait = self.timeout.min(Duration::from_secs(10));
            let started = Instant::now();
            if rpc.wait_ready(readiness_wait).await {
                tracing::debug!(
                    waited_ms = started.elapsed().as_millis() as u64,
                    "rust-analyzer reported quiescence"
                );
            } else {
                tracing::warn!(
                    waited_ms = started.elapsed().as_millis() as u64,
                    "rust-analyzer did not report quiescence within the readiness wait; \
                     continuing, workspace symbols may be incomplete until indexing finishes"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(code: i64, message: &str) -> RpcError {
        RpcError {
            code,
            message: message.to_owned(),
        }
    }

    #[test]
    fn file_not_found_is_a_loading_error_only_before_quiescence() {
        let server = RustServer::new(&Config::default());
        let not_found = error(-32603, r"file not found: C:\work\src\lib.rs");
        assert!(server.is_loading_error(&not_found, false));
        assert!(
            !server.is_loading_error(&not_found, true),
            "after quiescence the file is really unknown"
        );
        // Other errors, and the same code with other text, are real failures.
        assert!(!server.is_loading_error(&error(-32603, "internal error"), false));
        assert!(!server.is_loading_error(&error(-32602, "file not found: x"), false));
    }
}
