//! Optional C/C++ language-server (clangd) adapter. One configured clangd
//! instance navigates both Tree-sitter-indexed languages `c` and `cpp`
//! (`.c`, `.h`, `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`, `.hxx`), the same
//! one-server-many-languages shape `TypeScriptServer` uses for its four
//! ECMAScript-family languages.
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
use crate::lsp::adapter::{LspAdapter, SentTexts, open_if_changed};
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use anyhow::Result;
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

    /// The project configuration the server reads (see the trait).
    fn stamp_manifests(&self) -> &'static [&'static str] {
        &[
            "compile_commands.json",
            "compile_flags.txt",
            ".clangd",
            "CMakeLists.txt",
        ]
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

    // The hook sends the current disk text whenever it differs from what
    // this session sent, and clangd's didOpen is `addDocument`, which
    // replaces the contents of a file it already has open.
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
        if open_if_changed(
            rpc,
            sent,
            file,
            relative,
            "clangd",
            language_id_for(relative),
        )
        .await?
        .is_some()
        {
            // Mirrors the Python/TypeScript/Go adapters' bounded head start:
            // clangd parses and indexes a newly opened file asynchronously,
            // so a request issued immediately after didOpen can race ahead
            // and see no definition/references yet. Only a newly sent text
            // needs it.
            let head_start = self.timeout.min(Duration::from_secs(3));
            tokio::time::sleep(head_start).await;
        }
        Ok(())
    }
}

/// The LSP `languageId` clangd expects for a file, derived from its
/// extension (matching the Tree-sitter language identifiers this adapter
/// covers: `.c` files are `c`, `.cpp`/`.cc`/`.cxx`/`.hpp`/`.hh`/`.hxx` files
/// are `cpp`).
///
/// `.h` is the one deliberate exception to "same as the Tree-sitter
/// identifier": LCI indexes `.h` as `c`, but the LSP `languageId` is `cpp`.
/// The extension cannot say which language a header is written in, and clangd
/// itself treats a bare `.h` as C++ because a C header is almost always valid
/// C++ while a C++ header is never valid C. Opening it as C would turn every
/// class or template in a C++ project's `.h` into parse errors for the server.
/// Navigation results are still attributed to the indexed language `c`.
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
    use crate::lsp::adapter::contract;

    fn server(path: Option<&str>, args: &[&str], disabled: bool) -> ClangdServer {
        let config = ClangdLspConfig {
            path: path.map(str::to_owned),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            disabled,
        };
        ClangdServer::new(&config, Duration::from_secs(60))
    }

    #[test]
    fn the_clangd_adapter_follows_the_adapter_contract() {
        let usable = server(Some("clangd"), &[], false);
        assert_eq!(usable.languages(), &["c", "cpp"]);
        contract::assert_enabled_only_with_a_usable_path(
            |path, disabled| server(path, &[], disabled),
            "clangd",
        );
        // Unlike pyright/typescript-language-server, clangd needs no
        // --stdio-equivalent flag forced before configured args: its
        // default communication mode already is stdio.
        let transport = server(Some("clangd"), &["--log=verbose"], false)
            .transport_config(Path::new("/tmp"))
            .unwrap();
        assert_eq!(transport.args, vec!["--log=verbose".to_string()]);

        assert_eq!(language_id_for("src/a.c"), "c");
        assert_eq!(language_id_for("src/a.cpp"), "cpp");
        assert_eq!(language_id_for("src/a.hpp"), "cpp");
        for extension in ["cc", "cxx", "hh", "hxx"] {
            assert_eq!(language_id_for(&format!("src/a.{extension}")), "cpp");
        }
        // `.h` is indexed as `c` but opened for the server as C++ (see the
        // function's documentation).
        assert_eq!(language_id_for("include/a.h"), "cpp");
        contract::assert_initialize_params_name_this_process(
            &usable,
            tempfile::tempdir().unwrap().path(),
        );
    }
}
