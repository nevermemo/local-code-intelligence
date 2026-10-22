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
use crate::lsp::adapter::{LspAdapter, MAX_DIAGNOSTICS_WAIT, SentTexts, open_if_changed};
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use anyhow::Result;
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

    /// The project configuration the server reads (see the trait).
    fn stamp_manifests(&self) -> &'static [&'static str] {
        &[
            "tsconfig.json",
            "jsconfig.json",
            "package.json",
            "tsconfig.*.json",
        ]
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
                    "synchronization": {},
                    // typescript-language-server publishes diagnostics only to
                    // a client that declares it takes them; the hook waits for
                    // the publication of a file it opened (measured against a
                    // real server: none without this, one with it).
                    "publishDiagnostics": {}
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
        // wrapper; a file the hook opens is waited for by its diagnostics instead.
        Box::new(|_| false)
    }

    // typescript-language-server refuses a second didOpen for a document it
    // already has open ("Can't open already open document"). The session
    // sends a changed text as a didChange with the whole text instead (the
    // server advertises change sync), so it holds the current text whatever
    // it does with a repeated didOpen.
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
        if let Some(opened) = open_if_changed(
            rpc,
            sent,
            file,
            relative,
            "typescript-language-server",
            language_id_for(relative),
        )
        .await?
        {
            // tsserver processes a newly opened file (parsing it and
            // resolving its imports into the project graph) asynchronously;
            // a definition/references request issued immediately after
            // didOpen can race ahead of that and see an empty result,
            // especially for a freshly spawned, one-shot-CLI-invocation
            // process with no warm project state to reuse. It publishes
            // diagnostics for the file once it has analysed it: wait for
            // that, bounded by the configured request timeout, at most 3 s. Only a newly
            // sent text needs it.
            opened
                .wait_for_diagnostics(rpc, self.timeout.min(MAX_DIAGNOSTICS_WAIT))
                .await;
        }
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
    use crate::lsp::adapter::contract;

    fn server(path: Option<&str>, args: &[&str], disabled: bool) -> TypeScriptServer {
        let config = TypeScriptLspConfig {
            path: path.map(str::to_owned),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            disabled,
        };
        TypeScriptServer::new(&config, Duration::from_secs(60))
    }

    #[test]
    fn the_typescript_adapter_follows_the_adapter_contract() {
        let usable = server(Some("typescript-language-server"), &[], false);
        assert_eq!(
            usable.languages(),
            &["typescript", "tsx", "javascript", "jsx"]
        );
        contract::assert_enabled_only_with_a_usable_path(
            |path, disabled| server(path, &[], disabled),
            "typescript-language-server",
        );
        // --stdio always comes before the configured args.
        let transport = server(
            Some("typescript-language-server"),
            &["--log-level", "4"],
            false,
        )
        .transport_config(Path::new("/tmp"))
        .unwrap();
        assert_eq!(transport.args, vec!["--stdio", "--log-level", "4"]);

        assert_eq!(language_id_for("src/a.ts"), "typescript");
        assert_eq!(language_id_for("src/a.tsx"), "typescriptreact");
        assert_eq!(language_id_for("src/a.jsx"), "javascriptreact");
        assert_eq!(language_id_for("src/a.js"), "javascript");
        contract::assert_initialize_params_name_this_process(
            &usable,
            tempfile::tempdir().unwrap().path(),
        );
        // Without this capability the server never publishes, and the wait
        // after a `didOpen` would only ever end at its cap.
        let params = usable.initialize_params(tempfile::tempdir().unwrap().path());
        assert!(params.unwrap()["capabilities"]["textDocument"]["publishDiagnostics"].is_object());
    }
}
