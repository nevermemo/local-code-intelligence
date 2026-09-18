//! Optional TypeScript/JavaScript language-server
//! (typescript-language-server, wrapping tsserver) adapter. One configured
//! server instance navigates all four Tree-sitter-indexed ECMAScript-family
//! languages: `.ts`, `.tsx`, `.js`, `.jsx`.
//!
//! tsserver only knows about a file once it has been told the file is open
//! (`textDocument/didOpen`); this application never writes into a user's
//! real repository (no auto-generated `tsconfig.json`), so `workspace/
//! symbol` search is limited to whatever project tsserver has inferred from
//! files opened so far — a documented product limitation, not something
//! this adapter engineers around. `find_definition`/`find_references` open
//! their target file first via [`LspAdapter::before_position_request`], so
//! those two navigation tools work reliably regardless of a `tsconfig.json`
//! being present.

use crate::config::TypeScriptLspConfig;
use crate::lsp::adapter::LspAdapter;
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

/// Every Tree-sitter language identifier one typescript-language-server
/// instance navigates.
pub const LANGUAGES: &[&str] = &["typescript", "tsx", "javascript", "jsx"];

/// A narrow, client-neutral representation of the optional TypeScript/
/// JavaScript language server. It carries configuration and can produce the
/// transport and initialization payloads without spawning a process.
#[derive(Debug, Clone)]
pub struct TypeScriptServer {
    config: TypeScriptLspConfig,
    timeout: Duration,
}

impl TypeScriptServer {
    /// Build a TypeScript/JavaScript server representation from the shared
    /// configuration. The shared `lsp_timeout_seconds` applies here too.
    pub fn new(config: &TypeScriptLspConfig, timeout: Duration) -> Self {
        Self {
            config: config.clone(),
            timeout,
        }
    }
}

#[async_trait]
impl LspAdapter for TypeScriptServer {
    fn provider(&self) -> &'static str {
        "typescript-language-server"
    }

    fn languages(&self) -> &'static [&'static str] {
        LANGUAGES
    }

    fn enabled(&self) -> bool {
        self.config.enabled()
    }

    fn configured_command(&self) -> Option<String> {
        self.config.path.clone()
    }

    fn readiness_component_name(&self) -> &'static str {
        "typescript_language_server_available"
    }

    fn transport_config(&self, root: &Path) -> Option<TransportConfig> {
        let executable = self.config.path.as_deref()?;
        if executable.trim().is_empty() {
            return None;
        }
        // `--stdio` is not user-configurable: this application only ever
        // speaks LSP over stdio, so omitting it would leave the server
        // waiting on a different transport. `config.args` are additional,
        // appended after it.
        let mut args = vec!["--stdio".to_string()];
        args.extend(self.config.args.clone());
        Some(TransportConfig {
            executable: executable.to_string(),
            args,
            working_dir: root.to_path_buf(),
            timeout: self.timeout,
            label: "typescript-language-server".into(),
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
                "workspace": { "symbol": {}, "workspaceFolders": true }
            },
            "workspaceFolders": [{ "uri": root_uri, "name": name }],
            "initializationOptions": {}
        }))
    }

    fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        // tsserver has no notification this client hooks as a reliable
        // "project loaded" signal over the typescript-language-server LSP
        // wrapper; after_initialized gives it a bounded head start instead.
        Box::new(|_| false)
    }

    async fn before_position_request(
        &self,
        rpc: &JsonRpcClient,
        file: &Path,
        relative: &str,
    ) -> Result<()> {
        let content = tokio::fs::read_to_string(file)
            .await
            .with_context(|| format!("read {relative} to open in typescript-language-server"))?;
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        rpc.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language_id_for(relative),
                    "version": 1,
                    "text": content
                }
            }),
        )
        .await?;
        // tsserver processes a newly opened file (parsing it and resolving
        // its imports into the project graph) asynchronously; a definition/
        // references request issued immediately after didOpen can race
        // ahead of that and see an empty result, especially for a freshly
        // spawned, one-shot-CLI-invocation process with no warm project
        // state to reuse. Give it a bounded, best-effort head start, the
        // same kind of fallback rust-analyzer's adapter uses.
        let head_start = self.timeout.min(Duration::from_secs(3));
        tokio::time::sleep(head_start).await;
        Ok(())
    }
}

/// The LSP `languageId` tsserver expects for a file, derived from its
/// extension (matching the Tree-sitter language identifiers this adapter
/// covers, translated to the LSP-standard `*react` ids for JSX/TSX).
fn language_id_for(relative: &str) -> &'static str {
    match Path::new(relative)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("ts") => "typescript",
        Some("tsx") => "typescriptreact",
        Some("jsx") => "javascriptreact",
        _ => "javascript",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_reports_all_four_ecmascript_languages() {
        let config = TypeScriptLspConfig {
            path: Some("typescript-language-server".into()),
            args: vec![],
            disabled: false,
        };
        let server = TypeScriptServer::new(&config, Duration::from_secs(60));
        assert_eq!(
            server.languages(),
            &["typescript", "tsx", "javascript", "jsx"]
        );
        assert!(server.enabled());
    }

    #[test]
    fn empty_or_missing_path_is_not_enabled() {
        let missing = TypeScriptLspConfig::default();
        assert!(!TypeScriptServer::new(&missing, Duration::from_secs(60)).enabled());
        assert!(
            TypeScriptServer::new(&missing, Duration::from_secs(60))
                .transport_config(Path::new("/tmp"))
                .is_none()
        );

        let empty = TypeScriptLspConfig {
            path: Some("   ".into()),
            args: vec![],
            disabled: false,
        };
        assert!(!TypeScriptServer::new(&empty, Duration::from_secs(60)).enabled());

        let disabled = TypeScriptLspConfig {
            path: Some("typescript-language-server".into()),
            args: vec![],
            disabled: true,
        };
        assert!(!TypeScriptServer::new(&disabled, Duration::from_secs(60)).enabled());
    }

    #[test]
    fn transport_always_includes_stdio_before_configured_args() {
        let config = TypeScriptLspConfig {
            path: Some("typescript-language-server".into()),
            args: vec!["--log-level".into(), "4".into()],
            disabled: false,
        };
        let server = TypeScriptServer::new(&config, Duration::from_secs(30));
        let transport = server.transport_config(Path::new("/tmp")).unwrap();
        assert_eq!(transport.args, vec!["--stdio", "--log-level", "4"]);
    }

    #[test]
    fn server_can_be_represented_without_a_process() {
        let config = TypeScriptLspConfig {
            path: Some("typescript-language-server".into()),
            args: vec![],
            disabled: false,
        };
        let server = TypeScriptServer::new(&config, Duration::from_secs(30));
        let temp = tempfile::tempdir().unwrap();
        let params = server.initialize_params(temp.path()).unwrap();
        assert_eq!(params["processId"], std::process::id());
        assert!(params["rootUri"].as_str().is_some());
    }

    #[test]
    fn language_id_matches_extension() {
        assert_eq!(language_id_for("src/a.ts"), "typescript");
        assert_eq!(language_id_for("src/a.tsx"), "typescriptreact");
        assert_eq!(language_id_for("src/a.jsx"), "javascriptreact");
        assert_eq!(language_id_for("src/a.js"), "javascript");
    }
}
