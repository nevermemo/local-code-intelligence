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
    trusted: bool,
}

impl RustServer {
    pub fn new(config: &Config) -> Self {
        Self {
            path: config.rust_analyzer_path.clone(),
            timeout: Duration::from_secs(config.lsp_timeout_seconds),
            trusted: config.rust_analyzer_trusted,
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

    /// Evidence: the client capabilities in [`Self::initialize_params`]
    /// declare only `workspace.symbol.resolveSupport` and
    /// `experimental.serverStatusNotification`, no
    /// `workspace.didChangeWatchedFiles`, so rust-analyzer cannot rely on the
    /// client for file changes and runs its own watcher (its documented
    /// fallback). Correctness does not rely on that watcher: a kept session
    /// passes the tree barrier of each walk (`Manager::tree_barrier`), so its
    /// lag is not a staleness. Checked
    /// against the real server by the SHARED smoke (decision d454).
    fn watches_workspace(&self) -> bool {
        true
    }

    /// `Cargo.toml` and `Cargo.lock` (the crate graph) and `rust-analyzer.toml`
    /// (its per-project settings).
    fn stamp_manifests(&self) -> &'static [&'static str] {
        &["Cargo.toml", "Cargo.lock", "rust-analyzer.toml"]
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

    fn environment(&self) -> Vec<(String, String)> {
        if self.trusted {
            return Vec::new();
        }
        [
            ("CARGO_NET_OFFLINE", "true"),
            ("RUSTC_WRAPPER", ""),
            ("RUSTC_WORKSPACE_WRAPPER", ""),
            ("CARGO_BUILD_RUSTC_WRAPPER", ""),
            ("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER", ""),
        ]
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
    }

    fn initialize_params(&self, root: &Path) -> Option<Value> {
        let root_uri = reqwest::Url::from_directory_path(root).ok()?.to_string();
        let mut options = json!({
            "workspace":{"symbol":{"search":{"scope":"workspace","kind":"all_symbols","limit":256}}}
        });
        if !self.trusted {
            let environment: serde_json::Map<String, Value> = self
                .environment()
                .into_iter()
                .map(|(key, value)| (key, Value::String(value)))
                .collect();
            options["cargo"] = json!({
                "buildScripts": {"enable": false, "overrideCommand": null},
                "extraEnv": &environment
            });
            options["procMacro"] = json!({"enable": false});
            options["checkOnSave"] = json!(false);
            options["check"] = json!({"overrideCommand": null, "extraEnv": &environment});
        }
        Some(json!({
            "processId": std::process::id(), "rootUri": &root_uri,
            "capabilities": {"workspace":{"symbol":{"resolveSupport":{"properties":["location.range"]}}},"experimental":{"serverStatusNotification":true}},
            "workspaceFolders":[{"uri":&root_uri,"name":root.file_name().and_then(|v|v.to_str()).unwrap_or("workspace")}],
            "initializationOptions": options
        }))
    }

    fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        Box::new(|msg: &Value| {
            msg.get("method").and_then(Value::as_str) == Some("experimental/serverStatus")
                && msg["params"]["quiescent"].as_bool() == Some(true)
        })
    }

    fn busy_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        Box::new(|msg: &Value| {
            msg.get("method").and_then(Value::as_str) == Some("experimental/serverStatus")
                && msg["params"]["quiescent"].as_bool() == Some(false)
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

    #[test]
    fn default_initialization_disables_repository_execution() {
        let root = tempfile::tempdir().unwrap();
        let server = RustServer::new(&Config::default());
        let params = server.initialize_params(root.path()).unwrap();
        let options = &params["initializationOptions"];
        assert_eq!(options["cargo"]["buildScripts"]["enable"], json!(false));
        assert_eq!(options["procMacro"]["enable"], json!(false));
        assert_eq!(options["checkOnSave"], json!(false));
        assert_eq!(
            options["workspace"]["symbol"]["search"],
            json!({"scope":"workspace","kind":"all_symbols","limit":256})
        );
        let environment = json!({
            "CARGO_NET_OFFLINE": "true",
            "RUSTC_WRAPPER": "",
            "RUSTC_WORKSPACE_WRAPPER": "",
            "CARGO_BUILD_RUSTC_WRAPPER": "",
            "CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER": ""
        });
        assert_eq!(
            options,
            &json!({
                "workspace": {"symbol": {"search": {
                    "scope": "workspace", "kind": "all_symbols", "limit": 256
                }}},
                "cargo": {
                    "buildScripts": {"enable": false, "overrideCommand": null},
                    "extraEnv": &environment
                },
                "procMacro": {"enable": false},
                "checkOnSave": false,
                "check": {"overrideCommand": null, "extraEnv": &environment}
            })
        );
        let transport = server.transport_config(root.path()).unwrap();
        let command = transport.command(&server.environment());
        let overrides: serde_json::Map<String, Value> = command
            .as_std()
            .get_envs()
            .map(|(key, value)| {
                (
                    key.to_str().unwrap().to_owned(),
                    json!(value.unwrap().to_str().unwrap()),
                )
            })
            .collect();
        assert_eq!(Value::Object(overrides), environment);
    }

    #[test]
    fn trusted_initialization_restores_server_defaults_and_inherited_environment() {
        let root = tempfile::tempdir().unwrap();
        let config: Config = toml::from_str("rust_analyzer_trusted = true").unwrap();
        let server = RustServer::new(&config);
        let params = server.initialize_params(root.path()).unwrap();
        assert_eq!(
            params["initializationOptions"],
            json!({"workspace":{"symbol":{"search":{
                "scope":"workspace","kind":"all_symbols","limit":256
            }}}})
        );
        assert!(server.environment().is_empty());
        let transport = server.transport_config(root.path()).unwrap();
        assert_eq!(
            transport
                .command(&server.environment())
                .as_std()
                .get_envs()
                .count(),
            0
        );
        assert!(!toml::from_str::<Config>("").unwrap().rust_analyzer_trusted);
    }

    fn error(code: i64, message: &str) -> RpcError {
        RpcError {
            code,
            message: message.to_owned(),
        }
    }

    #[test]
    fn the_busy_predicate_matches_quiescent_false_only() {
        let server = RustServer::new(&Config::default());
        let busy = server.busy_predicate();
        let ready = server.ready_predicate();
        let status = |quiescent: Value| json!({"method": "experimental/serverStatus", "params": {"quiescent": quiescent}});
        assert!(busy(&status(json!(false))));
        assert!(!busy(&status(json!(true))));
        assert!(!busy(&status(Value::Null)));
        assert!(!busy(&json!({
            "method": "textDocument/publishDiagnostics",
            "params": {"quiescent": false}
        })));
        // The two never match the same notification.
        assert!(ready(&status(json!(true))) && !ready(&status(json!(false))));
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
