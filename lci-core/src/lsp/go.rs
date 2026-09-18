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
//! an explicit `textDocument/didOpen` and a bounded head start before a
//! position request.

use crate::config::GoLspConfig;
use crate::lsp::adapter::LspAdapter;
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use anyhow::{Context, Result, anyhow};
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

    fn enabled(&self) -> bool {
        self.config.enabled()
    }

    fn configured_command(&self) -> Option<String> {
        self.config.path.clone()
    }

    fn readiness_component_name(&self) -> &'static str {
        "go_language_server_available"
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
        let root_uri = reqwest::Url::from_directory_path(root).ok()?.to_string();
        let name = root
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("workspace")
            .to_string();
        Some(json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
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
            "workspaceFolders": [{ "uri": root_uri, "name": name }],
            "initializationOptions": {}
        }))
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

    async fn before_position_request(
        &self,
        rpc: &JsonRpcClient,
        file: &Path,
        relative: &str,
    ) -> Result<()> {
        let content = tokio::fs::read_to_string(file)
            .await
            .with_context(|| format!("read {relative} to open in gopls"))?;
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        rpc.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "go",
                    "version": 1,
                    "text": content
                }
            }),
        )
        .await?;
        // Mirrors the Python/TypeScript adapters' bounded head start: gopls
        // processes a newly opened file and its package graph asynchronously,
        // so a request issued immediately after didOpen can race ahead and
        // see no definition/references yet.
        let head_start = self.timeout.min(Duration::from_secs(3));
        tokio::time::sleep(head_start).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_reports_go() {
        let config = GoLspConfig {
            path: Some("gopls".into()),
            args: vec![],
            disabled: false,
        };
        let server = GoServer::new(&config, Duration::from_secs(60));
        assert_eq!(server.languages(), &["go"]);
        assert!(server.enabled());
    }

    #[test]
    fn empty_or_missing_path_is_not_enabled() {
        let missing = GoLspConfig::default();
        assert!(!GoServer::new(&missing, Duration::from_secs(60)).enabled());
        assert!(
            GoServer::new(&missing, Duration::from_secs(60))
                .transport_config(Path::new("/tmp"))
                .is_none()
        );

        let empty = GoLspConfig {
            path: Some("   ".into()),
            args: vec![],
            disabled: false,
        };
        assert!(!GoServer::new(&empty, Duration::from_secs(60)).enabled());

        let disabled = GoLspConfig {
            path: Some("gopls".into()),
            args: vec![],
            disabled: true,
        };
        assert!(!GoServer::new(&disabled, Duration::from_secs(60)).enabled());
    }

    #[test]
    fn transport_has_no_forced_leading_flag() {
        // Unlike pyright/typescript-language-server, gopls needs no
        // --stdio-equivalent flag forced before configured args: its
        // documented default communication mode already is stdio.
        let config = GoLspConfig {
            path: Some("gopls".into()),
            args: vec!["-rpc.trace".into()],
            disabled: false,
        };
        let server = GoServer::new(&config, Duration::from_secs(30));
        let transport = server.transport_config(Path::new("/tmp")).unwrap();
        assert_eq!(transport.args, vec!["-rpc.trace".to_string()]);
    }

    #[test]
    fn server_can_be_represented_without_a_process() {
        let config = GoLspConfig {
            path: Some("gopls".into()),
            args: vec![],
            disabled: false,
        };
        let server = GoServer::new(&config, Duration::from_secs(30));
        let temp = tempfile::tempdir().unwrap();
        let params = server.initialize_params(temp.path()).unwrap();
        assert_eq!(params["processId"], std::process::id());
        assert!(params["rootUri"].as_str().is_some());
    }
}
