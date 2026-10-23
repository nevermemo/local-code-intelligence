//! Optional Go language-server (gopls) adapter. Unlike csharp-ls/
//! typescript-language-server/pyright, gopls needs no forced leading
//! transport flag: `gopls help serve` documents stdin/stdout JSONRPC2 as its
//! default communication mode, and its `-mode` flag is explicitly documented
//! as "no effect" in current versions -- confirmed against a real installed
//! `gopls`, not assumed from the other adapters' shape. gopls operates in Go
//! module mode and needs a `go.mod` at or above the workspace root to build
//! its package graph; without one it degrades to a much weaker GOPATH-mode
//! fallback (fail-open, same spirit as every other adapter here -- LCI never
//! writes one on the caller's behalf). Like pyright, gopls's background
//! analysis is asynchronous enough that a freshly opened file benefits from
//! an explicit `textDocument/didOpen` and a wait for the server's
//! `publishDiagnostics` for it before a position request. gopls has no
//! `--version` flag (it exits 2); `gopls version` is its readiness probe.

use crate::config::GoLspConfig;
use crate::lsp::adapter::{
    InitializeVariant, LspAdapter, MAX_DIAGNOSTICS_WAIT, ProbeExit, SentTexts,
    initialize_params_for, open_if_changed, probe_subprocess,
};
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

pub const LANGUAGE_IDENTIFIER: &str = "go";

/// A narrow, client-neutral representation of the optional Go language
/// server. It carries configuration and can produce the transport and
/// initialization payloads without spawning a process.
#[derive(Debug, Clone)]
pub struct GoServer {
    config: GoLspConfig,
    timeout: Duration,
}

impl GoServer {
    /// Build a Go server representation from the shared configuration. The
    /// shared `lsp_timeout_seconds` applies here too.
    pub fn new(config: &GoLspConfig, timeout: Duration) -> Self {
        Self {
            config: config.clone(),
            timeout,
        }
    }
}

#[async_trait]
impl LspAdapter for GoServer {
    fn provider(&self) -> &'static str {
        "gopls"
    }

    fn languages(&self) -> &'static [&'static str] {
        &[LANGUAGE_IDENTIFIER]
    }

    /// The project configuration the server reads (see the trait).
    fn stamp_manifests(&self) -> &'static [&'static str] {
        &["go.mod", "go.sum", "go.work"]
    }

    fn enabled(&self) -> bool {
        self.config.enabled()
    }

    fn configured_command(&self) -> Option<String> {
        self.config.path.clone()
    }

    fn readiness_component_name(&self) -> &'static str {
        "go_language_server_available"
    }

    /// `gopls version`, which exits 0 (see the module doc: `gopls --version`
    /// is a flag error and exits 2).
    async fn probe_readiness(&self, timeout: Duration) -> Result<()> {
        if let Some(command) = self.configured_command() {
            probe_subprocess(Path::new(&command), &["version"], ProbeExit::Zero, timeout).await?;
        }
        Ok(())
    }

    fn transport_config(&self, root: &Path) -> Option<TransportConfig> {
        let executable = self.config.path.as_deref()?;
        if executable.trim().is_empty() {
            return None;
        }
        Some(TransportConfig {
            executable: executable.to_string(),
            args: self.config.args.clone(),
            working_dir: root.to_path_buf(),
            timeout: self.timeout,
            label: "gopls".into(),
        })
    }

    fn initialize_params(&self, root: &Path) -> Option<Value> {
        if !self.enabled() {
            return None;
        }
        initialize_params_for(root, InitializeVariant::Base)
    }

    fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        // gopls has no equivalent to rust-analyzer's experimental/
        // serverStatus; readiness relies on the bounded request timeout,
        // same as pyright/csharp-ls.
        Box::new(|_| false)
    }

    async fn after_initialized(&self, rpc: &JsonRpcClient) -> Result<()> {
        rpc.notify("workspace/didChangeConfiguration", json!({"settings": {}}))
            .await
    }

    // The hook sends the current disk text whenever it differs from what
    // this session sent, and gopls overwrites an open file's overlay on a
    // repeated didOpen.
    fn opens_documents(&self) -> bool {
        true
    }

    async fn before_position_request(
        &self,
        rpc: &JsonRpcClient,
        sent: &SentTexts,
        file: &Path,
        relative: &str,
    ) -> Result<()> {
        if let Some(opened) = open_if_changed(rpc, sent, file, relative, "gopls", "go").await? {
            // gopls processes a newly opened file and its package graph
            // asynchronously, so a request issued immediately after didOpen
            // can race ahead and see no definition/references yet. It
            // publishes diagnostics for the file when it has taken it in:
            // wait for that, bounded by the configured request timeout, at most 3 s. Only
            // a newly sent text needs it.
            opened
                .wait_for_diagnostics(rpc, self.timeout.min(MAX_DIAGNOSTICS_WAIT))
                .await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::adapter::contract;

    fn server(path: Option<&str>, args: &[&str], disabled: bool) -> GoServer {
        let config = GoLspConfig {
            path: path.map(str::to_owned),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            disabled,
        };
        GoServer::new(&config, Duration::from_secs(60))
    }

    /// The readiness probe runs `gopls version`: a shim that answers `version`
    /// only passes, and one that answers `--version` only fails.
    #[cfg(windows)]
    #[tokio::test]
    async fn the_go_probe_runs_gopls_version() {
        use crate::lsp::adapter::fresh_probes;
        let dir = tempfile::tempdir().unwrap();
        let probe = |name: &str, body: &str| {
            let shim = dir.path().join(name);
            std::fs::write(&shim, body).unwrap();
            let path = shim.to_string_lossy().into_owned();
            async move {
                fresh_probes(
                    server(Some(&path), &[], false).probe_readiness(Duration::from_secs(30)),
                )
                .await
            }
        };
        probe(
            "version-only.cmd",
            "@if \"%1\"==\"version\" (exit /b 0) else (exit /b 2)\r\n",
        )
        .await
        .unwrap();
        probe(
            "flag-only.cmd",
            "@if \"%1\"==\"--version\" (exit /b 0) else (exit /b 2)\r\n",
        )
        .await
        .unwrap_err();
    }

    #[test]
    fn the_go_initialize_params_are_pinned() {
        let (root, uri) = contract::pin_root();
        let params = server(Some("gopls"), &[], false).initialize_params(root);
        assert_eq!(
            params,
            Some(json!({
                "processId": std::process::id(),
                "rootUri": uri,
                "capabilities": {
                    "textDocument": {
                        "definition": {},
                        "references": {},
                        "synchronization": {}
                    },
                    "workspace": {
                        "symbol": {},
                        "workspaceFolders": true,
                        "configuration": true
                    }
                },
                "workspaceFolders": [{ "uri": uri, "name": "proj" }],
                "initializationOptions": {}
            }))
        );
    }

    #[test]
    fn the_go_adapter_follows_the_adapter_contract() {
        let usable = server(Some("gopls"), &[], false);
        assert_eq!(usable.languages(), &["go"]);
        contract::assert_enabled_only_with_a_usable_path(
            |path, disabled| server(path, &[], disabled),
            "gopls",
        );
        // Unlike pyright/typescript-language-server, gopls needs no
        // --stdio-equivalent flag forced before configured args: its
        // documented default communication mode already is stdio.
        let transport = server(Some("gopls"), &["-rpc.trace"], false)
            .transport_config(Path::new("/tmp"))
            .unwrap();
        assert_eq!(transport.args, vec!["-rpc.trace".to_string()]);
        contract::assert_initialize_params_name_this_process(
            &usable,
            tempfile::tempdir().unwrap().path(),
        );
    }
}
