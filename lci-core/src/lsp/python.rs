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
use crate::lsp::adapter::{LspAdapter, SentTexts, open_if_changed};
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use anyhow::Result;
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

    /// The project configuration the server reads (see the trait).
    fn stamp_manifests(&self) -> &'static [&'static str] {
        &["pyrightconfig.json", "pyproject.toml", "setup.cfg"]
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

    // The hook sends the current disk text whenever it differs from what
    // this session sent, and pyright replaces an open document's text on a
    // repeated didOpen (it logs the redundant open and applies the new text).
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
        if let Some(opened) =
            open_if_changed(rpc, sent, file, relative, "pyright", "python").await?
        {
            // pyright processes a newly opened file and its workspace scan
            // asynchronously, so a request issued immediately after didOpen
            // can race ahead and see no definition/references yet. It
            // publishes diagnostics for the file when it has taken it in:
            // wait for that, bounded by the configured request timeout. Only
            // a newly sent text needs it; a repeat request on an unchanged
            // file pays nothing.
            opened.wait_for_diagnostics(rpc, self.timeout).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::adapter::contract;

    fn server(path: Option<&str>, args: &[&str], disabled: bool) -> PythonServer {
        let config = PythonLspConfig {
            path: path.map(str::to_owned),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            disabled,
        };
        PythonServer::new(&config, Duration::from_secs(60))
    }

    #[test]
    fn the_python_adapter_follows_the_adapter_contract() {
        let usable = server(Some("pyright-langserver"), &[], false);
        assert_eq!(usable.languages(), &["python"]);
        contract::assert_enabled_only_with_a_usable_path(
            |path, disabled| server(path, &[], disabled),
            "pyright-langserver",
        );
        // --stdio always comes before the configured args.
        let transport = server(Some("pyright-langserver"), &["--verbose"], false)
            .transport_config(Path::new("/tmp"))
            .unwrap();
        assert_eq!(transport.args, vec!["--stdio", "--verbose"]);
        contract::assert_initialize_params_name_this_process(
            &usable,
            tempfile::tempdir().unwrap().path(),
        );
    }
}
