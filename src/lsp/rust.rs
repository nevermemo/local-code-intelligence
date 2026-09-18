//! rust-analyzer adapter. Rust navigation has no enable/disable knob (unlike
//! the optional C#/TypeScript/Python servers): `rust_analyzer_path` always
//! has a default value and `Config::validate` requires it nonempty, so this
//! adapter is always "enabled" — the underlying executable may simply not be
//! installed, which readiness/error reporting surfaces at request time.

use crate::config::Config;
use crate::lsp::adapter::LspAdapter;
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

pub const LANGUAGE_IDENTIFIER: &str = "rust";

#[derive(Debug, Clone)]
pub struct RustServer {
    path: String,
    timeout: Duration,
}

impl RustServer {
    pub fn new(config: &Config) -> Self {
        Self {
            path: config.rust_analyzer_path.clone(),
            timeout: Duration::from_secs(config.lsp_timeout_seconds),
        }
    }
}

#[async_trait]
impl LspAdapter for RustServer {
    fn provider(&self) -> &'static str {
        "rust-analyzer"
    }

    fn languages(&self) -> &'static [&'static str] {
        &[LANGUAGE_IDENTIFIER]
    }

    fn enabled(&self) -> bool {
        true
    }

    fn configured_command(&self) -> Option<String> {
        Some(self.path.clone())
    }

    fn readiness_component_name(&self) -> &'static str {
        "rust_analyzer_available"
    }

    fn transport_config(&self, root: &Path) -> Option<TransportConfig> {
        Some(TransportConfig {
            executable: self.path.clone(),
            args: Vec::new(),
            working_dir: root.to_path_buf(),
            timeout: self.timeout,
            label: "rust-analyzer".into(),
        })
    }

    fn initialize_params(&self, root: &Path) -> Option<Value> {
        let root_uri = reqwest::Url::from_directory_path(root).ok()?.to_string();
        Some(json!({
            "processId": std::process::id(), "rootUri": &root_uri,
            "capabilities": {"workspace":{"symbol":{"resolveSupport":{"properties":["location.range"]}}},"experimental":{"serverStatusNotification":true}},
            "workspaceFolders":[{"uri":&root_uri,"name":root.file_name().and_then(|v|v.to_str()).unwrap_or("workspace")}],
            "initializationOptions":{"workspace":{"symbol":{"search":{"scope":"workspace","kind":"all_symbols","limit":256}}}}
        }))
    }

    fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        Box::new(|msg: &Value| {
            msg.get("method").and_then(Value::as_str) == Some("experimental/serverStatus")
                && msg["params"]["quiescent"].as_bool() == Some(true)
        })
    }

    async fn after_initialized(&self, rpc: &JsonRpcClient) -> Result<()> {
        if !rpc.is_ready() {
            // Some rust-analyzer builds do not emit serverStatus even when
            // the capability is advertised. Give crate discovery a bounded
            // head start, then rely on normal request timeouts and restart
            // recovery.
            let readiness_wait = self.timeout.min(Duration::from_secs(10));
            let _ = rpc.wait_ready(readiness_wait).await;
        }
        Ok(())
    }
}
