//! Optional typescript-go (`tsgo`, the native TypeScript compiler's language
//! server) adapter: an alternative to typescript-language-server for the same
//! four ECMAScript-family languages (`.ts`, `.tsx`, `.js`, `.jsx`). It is its
//! own adapter, not a flavour of the TypeScript one: the provider name is the
//! session key, the `analyzers` key, the readiness component and the
//! acceptance provider, all of which are per server.
//!
//! At most one of the two TypeScript servers is enabled (`Config::validate`
//! refuses both), and navigation picks the first enabled adapter of a
//! language, so which one answers is the configuration's choice. tsgo is
//! unconfigured on every install; the default stays typescript-language-server.
//!
//! The document hook is the TypeScript adapter's: `didOpen` (or a whole-text
//! `didChange` for a changed text) through [`open_if_changed`], then a wait for
//! the file's diagnostics bounded by [`MAX_DIAGNOSTICS_WAIT`]. Whether tsgo
//! publishes diagnostics after a `didOpen` is a fact of the real server that
//! the live step records; a server that never does only pays the cap once per
//! newly sent file (the wait is fail-open).

use crate::config::TsgoLspConfig;
use crate::lsp::adapter::{LspAdapter, MAX_DIAGNOSTICS_WAIT, SentTexts, open_if_changed};
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use crate::lsp::typescript::{LANGUAGES, MANIFESTS, language_id_for};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

/// The provider name: `Location.provider`, the `analyzers` key and the session
/// key.
const PROVIDER: &str = "typescript-go";

/// A narrow, client-neutral representation of the optional typescript-go
/// language server. It carries configuration and can produce the transport and
/// initialization payloads without spawning a process.
#[derive(Debug, Clone)]
pub struct TsgoServer {
    config: TsgoLspConfig,
    timeout: Duration,
}

impl TsgoServer {
    /// Build a typescript-go server representation from the shared
    /// configuration. The shared `lsp_timeout_seconds` applies here too.
    pub fn new(config: &TsgoLspConfig, timeout: Duration) -> Self {
        Self {
            config: config.clone(),
            timeout,
        }
    }
}

#[async_trait]
impl LspAdapter for TsgoServer {
    fn provider(&self) -> &'static str {
        PROVIDER
    }

    fn languages(&self) -> &'static [&'static str] {
        LANGUAGES
    }

    /// The project configuration the server reads (see the trait): the same
    /// files as typescript-language-server.
    fn stamp_manifests(&self) -> &'static [&'static str] {
        MANIFESTS
    }

    fn enabled(&self) -> bool {
        self.config.enabled()
    }

    fn configured_command(&self) -> Option<String> {
        self.config.path.clone()
    }

    fn readiness_component_name(&self) -> &'static str {
        "typescript_go_available"
    }

    fn transport_config(&self, root: &Path) -> Option<TransportConfig> {
        let executable = self.config.path.as_deref()?;
        if executable.trim().is_empty() {
            return None;
        }
        // `--lsp --stdio` is not user-configurable: this application only
        // ever speaks LSP over stdio, and tsgo is a compiler CLI that serves
        // LSP only in this mode. `config.args` are appended after it.
        let mut args = vec!["--lsp".to_string(), "--stdio".to_string()];
        args.extend(self.config.args.clone());
        Some(TransportConfig {
            executable: executable.to_string(),
            args,
            working_dir: root.to_path_buf(),
            timeout: self.timeout,
            label: PROVIDER.into(),
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
                    // Declared so a server that publishes diagnostics only to
                    // a client that takes them does: the hook waits for the
                    // publication of a file it opened.
                    "publishDiagnostics": {}
                },
                "workspace": { "symbol": {}, "workspaceFolders": true }
            },
            "workspaceFolders": [{ "uri": root_uri, "name": name }],
            "initializationOptions": {}
        }))
    }

    fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        // No notification is known to signal "project loaded"; a file the
        // hook opens is waited for by its diagnostics instead.
        Box::new(|_| false)
    }

    // The session sends a changed text as a didChange with the whole text
    // when the server advertises change sync, and a didOpen otherwise, so it
    // holds the current text whatever the server does with a repeated didOpen.
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
            PROVIDER,
            language_id_for(relative),
        )
        .await?
        {
            // A request issued right after a `didOpen` can race ahead of the
            // server's analysis of the file and see an empty result; the
            // publication of the file's diagnostics marks it analysed. The
            // wait is bounded by the request timeout and at most 3 s, only a
            // newly sent text needs it, and a server that never publishes
            // just reaches the cap.
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
    use crate::lsp::typescript::TypeScriptServer;

    fn server(path: Option<&str>, args: &[&str], disabled: bool) -> TsgoServer {
        let config = TsgoLspConfig {
            path: path.map(str::to_owned),
            args: args.iter().map(|arg| (*arg).to_owned()).collect(),
            disabled,
        };
        TsgoServer::new(&config, Duration::from_secs(60))
    }

    #[test]
    fn the_typescript_go_adapter_follows_the_adapter_contract() {
        let usable = server(Some("tsgo"), &[], false);
        assert_eq!(usable.provider(), "typescript-go");
        assert_eq!(usable.readiness_component_name(), "typescript_go_available");
        contract::assert_enabled_only_with_a_usable_path(
            |path, disabled| server(path, &[], disabled),
            "typescript-go",
        );
        // --lsp --stdio always come before the configured args.
        let transport = server(Some("tsgo"), &["--cwd", "x"], false)
            .transport_config(Path::new("/tmp"))
            .unwrap();
        assert_eq!(transport.args, vec!["--lsp", "--stdio", "--cwd", "x"]);
        assert_eq!(transport.label, "typescript-go");
        assert!(
            server(Some("  "), &[], false)
                .transport_config(Path::new("/tmp"))
                .is_none()
        );
        assert!(
            server(None, &[], false)
                .transport_config(Path::new("/tmp"))
                .is_none()
        );
        // The same languages and stamp manifests as typescript-language-server.
        let sibling = TypeScriptServer::new(
            &crate::config::TypeScriptLspConfig::default(),
            Duration::from_secs(60),
        );
        assert_eq!(usable.languages(), sibling.languages());
        assert_eq!(
            usable.languages(),
            &["typescript", "tsx", "javascript", "jsx"]
        );
        assert_eq!(usable.stamp_manifests(), sibling.stamp_manifests());
        // A document-opening server that joins the search channel, as its sibling.
        assert!(usable.opens_documents());
        assert!(usable.joins_search_channel());
        contract::assert_initialize_params_name_this_process(
            &usable,
            tempfile::tempdir().unwrap().path(),
        );
        // The diagnostics wait after a `didOpen` is kept (it is dropped only if
        // the live install check shows tsgo never publishes), so the client
        // capability is declared.
        let params = usable.initialize_params(tempfile::tempdir().unwrap().path());
        assert!(params.unwrap()["capabilities"]["textDocument"]["publishDiagnostics"].is_object());
    }
}
