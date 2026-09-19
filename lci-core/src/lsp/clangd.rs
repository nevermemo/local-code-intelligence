//! Optional C/C++ language-server (clangd) adapter. One configured clangd
//! instance navigates both Tree-sitter-indexed languages `c` and `cpp`
//! (`.c`, `.cpp`, `.hpp`), the same one-server-many-languages shape
//! `TypeScriptServer` uses for its four ECMAScript-family languages.
//!
//! Like gopls, clangd needs no forced leading transport flag: stdio is its
//! default communication mode, with no `--stdio`-equivalent flag required
//! (confirmed against a real installed clangd, not assumed from the other
//! adapters' shape). It also needs no `compile_commands.json` for LCI's
//! per-file navigation use case: confirmed live against a small standalone
//! fixture with no build system present -- `workspace/symbol`,
//! `textDocument/definition`, and `textDocument/references` all resolved
//! correctly through clangd's fallback compilation database. A workspace
//! with a real build system can still point clangd at its own
//! `compile_commands.json` via `[clangd].args`
//! (`--compile-commands-dir=...`); this adapter does not generate one.

use crate::config::ClangdLspConfig;
use crate::lsp::adapter::LspAdapter;
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

/// Every Tree-sitter language identifier one clangd instance navigates.
pub const LANGUAGES: &[&str] = &["c", "cpp"];

/// A narrow, client-neutral representation of the optional C/C++ language
/// server. It carries configuration and can produce the transport and
/// initialization payloads without spawning a process.
#[derive(Debug, Clone)]
pub struct ClangdServer {
    config: ClangdLspConfig,
    timeout: Duration,
}

impl ClangdServer {
    /// Build a clangd server representation from the shared configuration.
    /// The shared `lsp_timeout_seconds` applies here too.
    pub fn new(config: &ClangdLspConfig, timeout: Duration) -> Self {
        Self {
            config: config.clone(),
            timeout,
        }
    }
}

#[async_trait]
impl LspAdapter for ClangdServer {
    fn provider(&self) -> &'static str {
        "clangd"
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
        "clangd_available"
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
            label: "clangd".into(),
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
        // clangd has no equivalent to rust-analyzer's experimental/
        // serverStatus; readiness relies on the bounded request timeout,
        // same as pyright/csharp-ls/gopls/typescript-language-server.
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
            .with_context(|| format!("read {relative} to open in clangd"))?;
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
        // Mirrors the Python/TypeScript/Go adapters' bounded head start:
        // clangd parses and indexes a newly opened file asynchronously, so
        // a request issued immediately after didOpen can race ahead and see
        // no definition/references yet.
        let head_start = self.timeout.min(Duration::from_secs(3));
        tokio::time::sleep(head_start).await;
        Ok(())
    }
}

/// The LSP `languageId` clangd expects for a file, derived from its
/// extension (matching the Tree-sitter language identifiers this adapter
/// covers: `.c` files are `c`, `.cpp`/`.hpp` files are `cpp`).
fn language_id_for(relative: &str) -> &'static str {
    match Path::new(relative)
        .extension()
        .and_then(|extension| extension.to_str())
    {
        Some("c") => "c",
        _ => "cpp",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_reports_c_and_cpp() {
        let config = ClangdLspConfig {
            path: Some("clangd".into()),
            args: vec![],
            disabled: false,
        };
        let server = ClangdServer::new(&config, Duration::from_secs(60));
        assert_eq!(server.languages(), &["c", "cpp"]);
        assert!(server.enabled());
    }

    #[test]
    fn empty_or_missing_path_is_not_enabled() {
        let missing = ClangdLspConfig::default();
        assert!(!ClangdServer::new(&missing, Duration::from_secs(60)).enabled());
        assert!(
            ClangdServer::new(&missing, Duration::from_secs(60))
                .transport_config(Path::new("/tmp"))
                .is_none()
        );

        let empty = ClangdLspConfig {
            path: Some("   ".into()),
            args: vec![],
            disabled: false,
        };
        assert!(!ClangdServer::new(&empty, Duration::from_secs(60)).enabled());

        let disabled = ClangdLspConfig {
            path: Some("clangd".into()),
            args: vec![],
            disabled: true,
        };
        assert!(!ClangdServer::new(&disabled, Duration::from_secs(60)).enabled());
    }

    #[test]
    fn transport_has_no_forced_leading_flag() {
        // Unlike pyright/typescript-language-server, clangd needs no
        // --stdio-equivalent flag forced before configured args: its
        // default communication mode already is stdio.
        let config = ClangdLspConfig {
            path: Some("clangd".into()),
            args: vec!["--log=verbose".into()],
            disabled: false,
        };
        let server = ClangdServer::new(&config, Duration::from_secs(30));
        let transport = server.transport_config(Path::new("/tmp")).unwrap();
        assert_eq!(transport.args, vec!["--log=verbose".to_string()]);
    }

    #[test]
    fn server_can_be_represented_without_a_process() {
        let config = ClangdLspConfig {
            path: Some("clangd".into()),
            args: vec![],
            disabled: false,
        };
        let server = ClangdServer::new(&config, Duration::from_secs(30));
        let temp = tempfile::tempdir().unwrap();
        let params = server.initialize_params(temp.path()).unwrap();
        assert_eq!(params["processId"], std::process::id());
        assert!(params["rootUri"].as_str().is_some());
    }

    #[test]
    fn language_id_matches_extension() {
        assert_eq!(language_id_for("src/a.c"), "c");
        assert_eq!(language_id_for("src/a.cpp"), "cpp");
        assert_eq!(language_id_for("src/a.hpp"), "cpp");
    }
}
