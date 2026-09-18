//! Optional Python language-server (pyright, via `pyright-langserver`)
//! adapter. pyright discovers workspace files on its own for diagnostics/
//! workspace-wide analysis, but precise `textDocument/definition`/
//! `textDocument/references` queries at a specific position were confirmed
//! (against a real pyright) to need the target file opened first, same as
//! TypeScript's tsserver; this adapter sends `textDocument/didOpen` before
//! each. pyright also expects an (even empty) `workspace/
//! didChangeConfiguration` notification after `initialized`; this adapter
//! always sends one, and this application's shared JSON-RPC transport
//! answers pyright's `workspace/configuration` requests generically (pyright
//! blocks further work until that reply arrives).

use crate::config::PythonLspConfig;
use crate::lsp::adapter::LspAdapter;
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

pub const LANGUAGE_IDENTIFIER: &str = "python";

/// A narrow, client-neutral representation of the optional Python language
/// server. It carries configuration and can produce the transport and
/// initialization payloads without spawning a process.
#[derive(Debug, Clone)]
pub struct PythonServer {
    config: PythonLspConfig,
    timeout: Duration,
}

impl PythonServer {
    /// Build a Python server representation from the shared configuration.
    /// The shared `lsp_timeout_seconds` applies here too.
    pub fn new(config: &PythonLspConfig, timeout: Duration) -> Self {
        Self {
            config: config.clone(),
            timeout,
        }
    }
}

#[async_trait]
impl LspAdapter for PythonServer {
    fn provider(&self) -> &'static str {
        "pyright"
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
        "python_language_server_available"
    }

    fn transport_config(&self, root: &Path) -> Option<TransportConfig> {
        let executable = self.config.path.as_deref()?;
        if executable.trim().is_empty() {
            return None;
        }
        // `--stdio` is not user-configurable, matching the TypeScript
        // adapter: this application only ever speaks LSP over stdio.
        let mut args = vec!["--stdio".to_string()];
        args.extend(self.config.args.clone());
        Some(TransportConfig {
            executable: executable.to_string(),
            args,
            working_dir: root.to_path_buf(),
            timeout: self.timeout,
            label: "pyright".into(),
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
        // pyright signals background-indexing progress via $/progress
        // tokens rather than a fixed notification shape; readiness relies
        // on the bounded request timeout, same as csharp-ls.
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
            .with_context(|| format!("read {relative} to open in pyright"))?;
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        rpc.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "python",
                    "version": 1,
                    "text": content
                }
            }),
        )
        .await?;
        // Mirrors the TypeScript adapter's bounded head start: pyright
        // processes a newly opened file and its workspace scan
        // asynchronously, so a request issued immediately after didOpen can
        // race ahead and see no definition/references yet.
        let head_start = self.timeout.min(Duration::from_secs(3));
        tokio::time::sleep(head_start).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_reports_python() {
        let config = PythonLspConfig {
            path: Some("pyright-langserver".into()),
            args: vec![],
            disabled: false,
        };
        let server = PythonServer::new(&config, Duration::from_secs(60));
        assert_eq!(server.languages(), &["python"]);
        assert!(server.enabled());
    }

    #[test]
    fn empty_or_missing_path_is_not_enabled() {
        let missing = PythonLspConfig::default();
        assert!(!PythonServer::new(&missing, Duration::from_secs(60)).enabled());
        assert!(
            PythonServer::new(&missing, Duration::from_secs(60))
                .transport_config(Path::new("/tmp"))
                .is_none()
        );

        let empty = PythonLspConfig {
            path: Some("   ".into()),
            args: vec![],
            disabled: false,
        };
        assert!(!PythonServer::new(&empty, Duration::from_secs(60)).enabled());

        let disabled = PythonLspConfig {
            path: Some("pyright-langserver".into()),
            args: vec![],
            disabled: true,
        };
        assert!(!PythonServer::new(&disabled, Duration::from_secs(60)).enabled());
    }

    #[test]
    fn transport_always_includes_stdio_before_configured_args() {
        let config = PythonLspConfig {
            path: Some("pyright-langserver".into()),
            args: vec!["--verbose".into()],
            disabled: false,
        };
        let server = PythonServer::new(&config, Duration::from_secs(30));
        let transport = server.transport_config(Path::new("/tmp")).unwrap();
        assert_eq!(transport.args, vec!["--stdio", "--verbose"]);
    }

    #[test]
    fn server_can_be_represented_without_a_process() {
        let config = PythonLspConfig {
            path: Some("pyright-langserver".into()),
            args: vec![],
            disabled: false,
        };
        let server = PythonServer::new(&config, Duration::from_secs(30));
        let temp = tempfile::tempdir().unwrap();
        let params = server.initialize_params(temp.path()).unwrap();
        assert_eq!(params["processId"], std::process::id());
        assert!(params["rootUri"].as_str().is_some());
    }
}
