//! Optional Markdown language-server (marksman) adapter. marksman reads
//! headings and links: `textDocument/documentSymbol` is the heading outline,
//! `textDocument/definition` at a link is its target document or heading,
//! `textDocument/references` at a heading is the links that point at it
//! (across files), and `workspace/symbol` lists the headings of every
//! document under the root. Every method already exists on the manager; the
//! adapter only names the server and the prose language identifier
//! (`markdown`, which has no Tree-sitter grammar).
//!
//! marksman's stdio mode is the `server` subcommand (`marksman server`); the
//! adapter forces it before the configured args, like pyright's `--stdio`. The
//! `--version` probe works as it is.
//!
//! marksman reads a workspace only below a `.git` directory or a
//! `.marksman.toml` (an empty file is enough); without either it answers for
//! opened files alone and a link to another document resolves to nothing. LCI
//! writes neither, as it writes no `go.mod`. Its `workspace/symbol` names a
//! heading `H<level>: <text>`.
//!
//! The session is never shared across calls ([`stamp_manifests`] is empty and
//! [`watches_workspace`] is `false`): the file sync forwards no prose edits,
//! so a kept session could go stale, and marksman starts in well under a
//! second. And the server does not join the search's LSP channel
//! ([`joins_search_channel`]): its symbols are headings, not code names, and
//! with prose indexed every search would otherwise start it.
//!
//! [`stamp_manifests`]: LspAdapter::stamp_manifests
//! [`watches_workspace`]: LspAdapter::watches_workspace
//! [`joins_search_channel`]: LspAdapter::joins_search_channel

use crate::config::MarkdownLspConfig;
use crate::lsp::adapter::{
    InitializeVariant, LspAdapter, SentTexts, initialize_params_for, open_if_changed,
};
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::{path::Path, time::Duration};

pub const LANGUAGE_IDENTIFIER: &str = "markdown";

/// A narrow, client-neutral representation of the optional Markdown language
/// server. It carries configuration and can produce the transport and
/// initialization payloads without spawning a process.
#[derive(Debug, Clone)]
pub struct MarkdownServer {
    config: MarkdownLspConfig,
    timeout: Duration,
}

impl MarkdownServer {
    /// Build a Markdown server representation from the shared configuration.
    /// The shared `lsp_timeout_seconds` applies here too.
    pub fn new(config: &MarkdownLspConfig, timeout: Duration) -> Self {
        Self {
            config: config.clone(),
            timeout,
        }
    }
}

#[async_trait]
impl LspAdapter for MarkdownServer {
    fn provider(&self) -> &'static str {
        "marksman"
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
        "markdown_language_server_available"
    }

    fn transport_config(&self, root: &Path) -> Option<TransportConfig> {
        let executable = self.config.path.as_deref()?;
        if executable.trim().is_empty() {
            return None;
        }
        // `server` (marksman's stdio mode) always comes before the
        // configured args and is not user-configurable.
        let mut args = vec!["server".to_string()];
        args.extend(self.config.args.clone());
        Some(TransportConfig {
            executable: executable.to_string(),
            args,
            working_dir: root.to_path_buf(),
            timeout: self.timeout,
            label: "marksman".into(),
        })
    }

    fn initialize_params(&self, root: &Path) -> Option<Value> {
        if !self.enabled() {
            return None;
        }
        initialize_params_for(root, InitializeVariant::DocumentSymbol)
    }

    fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        // marksman sends no status notification; readiness relies on the
        // bounded request timeout, same as gopls and pyright.
        Box::new(|_| false)
    }

    // The hook sends the current disk text whenever it differs from what this
    // session sent, and marksman re-reads an open document from the didOpen text, so the manager's
    // outline cache may key an answer on the bytes on disk.
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
        // No head start: marksman answers from the opened text at once, so
        // the opened-document gate is dropped as soon as the open is sent.
        open_if_changed(rpc, sent, file, relative, "marksman", LANGUAGE_IDENTIFIER)
            .await
            .map(|_| ())
    }

    fn joins_search_channel(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lsp::adapter::contract;
    use serde_json::json;

    fn server(path: Option<&str>, args: &[&str], disabled: bool) -> MarkdownServer {
        let config = MarkdownLspConfig {
            path: path.map(str::to_owned),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            disabled,
        };
        MarkdownServer::new(&config, Duration::from_secs(60))
    }

    #[test]
    fn the_markdown_initialize_params_are_pinned() {
        let (root, uri) = contract::pin_root();
        let params = server(Some("marksman"), &[], false).initialize_params(root);
        assert_eq!(
            params,
            Some(json!({
                "processId": std::process::id(),
                "rootUri": uri,
                "capabilities": {
                    "textDocument": {
                        "definition": {},
                        "references": {},
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true },
                        "synchronization": {}
                    },
                    "workspace": {
                        "symbol": {},
                        "workspaceFolders": true
                    }
                },
                "workspaceFolders": [{ "uri": uri, "name": "proj" }],
                "initializationOptions": {}
            }))
        );
    }

    #[test]
    fn the_markdown_adapter_follows_the_adapter_contract() {
        let usable = server(Some("marksman"), &[], false);
        assert_eq!(usable.languages(), &["markdown"]);
        assert_eq!(usable.provider(), "marksman");
        contract::assert_enabled_only_with_a_usable_path(
            |path, disabled| server(path, &[], disabled),
            "marksman",
        );
        contract::assert_initialize_params_name_this_process(
            &usable,
            tempfile::tempdir().unwrap().path(),
        );
    }

    #[test]
    fn the_server_subcommand_comes_before_the_configured_args() {
        let transport = server(Some("marksman"), &["--verbose"], false)
            .transport_config(Path::new("/tmp"))
            .unwrap();
        assert_eq!(transport.args, vec!["server", "--verbose"]);
        let plain = server(Some("marksman"), &[], false)
            .transport_config(Path::new("/tmp"))
            .unwrap();
        assert_eq!(plain.args, vec!["server"]);
    }

    #[test]
    fn the_session_is_never_shared_and_the_server_stays_out_of_the_search_channel() {
        let adapter = server(Some("marksman"), &[], false);
        for synced in [false, true] {
            assert!(!adapter.keeps_session(synced), "synced = {synced}");
        }
        assert!(adapter.stamp_manifests().is_empty());
        assert!(!adapter.watches_workspace());
        assert!(!adapter.joins_search_channel());
        assert!(adapter.opens_documents());
    }
}
