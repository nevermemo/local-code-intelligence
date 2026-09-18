//! Optional Java language-server (jdtls / Eclipse JDT Language Server)
//! adapter. Spawns `java` directly as a single native child process, the
//! same shape as every other adapter here -- see `JavaLspConfig`'s doc
//! comment in `config.rs` for why this replaced an earlier design that
//! reused jdtls's own Python launcher script (a `local-code-intelligence
//! -> python -> java` process chain that hung indefinitely on Windows: the
//! orphaned `java` grandchild survived killing its direct `python` parent
//! and kept its inherited stdout pipe open, so the next pipe read blocked
//! forever with neither data nor EOF).
//!
//! This adapter reimplements the two things jdtls's own launcher does
//! before invoking `java`: finding the version-suffixed Equinox launcher
//! jar under `<jdtls install>/plugins/`, and picking the platform's shared
//! OSGi configuration directory (`config_win`/`config_mac`/`config_linux`).
//! Confirmed against a real installed jdtls snapshot: the resulting `java
//! -jar <launcher> -data <dir>` invocation produces a real `initialize`
//! response, a `language/status` notification sequence ending in
//! `{"type":"Started","message":"Ready"}`, and correct `workspace/symbol`/
//! `textDocument/definition` results -- and, unlike the Python-wrapped
//! version, exits within seconds of being killed with zero orphaned
//! processes.

use crate::config::JavaLspConfig;
use crate::lsp::adapter::LspAdapter;
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use crate::workspace;
use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

pub const LANGUAGE_IDENTIFIER: &str = "java";

/// A narrow, client-neutral representation of the optional Java language
/// server. It carries configuration and can produce the transport and
/// initialization payloads without spawning a process.
#[derive(Debug, Clone)]
pub struct JavaServer {
    config: JavaLspConfig,
    data_dir: PathBuf,
    timeout: Duration,
}

impl JavaServer {
    /// Build a Java server representation from the shared configuration.
    /// Unlike the single-sub-config adapters, this one also needs the top-
    /// level `data_dir` to derive jdtls's per-workspace `-data` directory
    /// (mirroring `RustServer::new`, which likewise takes the whole
    /// `Config`).
    pub fn new(config: &crate::config::Config, timeout: Duration) -> Self {
        Self {
            config: config.java.clone(),
            data_dir: config.data_dir.clone(),
            timeout,
        }
    }

    /// The stable per-workspace jdtls metadata directory: `<data_dir>/jdtls/
    /// <workspace id>`, where the workspace id is the same SHA-256 hash of
    /// the canonicalized workspace path `Workspace::resolve` computes
    /// (recomputed here since `transport_config` only receives the root
    /// path, not a `Workspace`). Stable across restarts so jdtls reuses its
    /// own incremental index instead of cold-starting every time; scoped
    /// under the disposable `data_dir` acceptance fixtures already use, so
    /// acceptance runs never touch or corrupt a real workspace's jdtls
    /// state.
    fn workspace_data_dir(&self, root: &Path) -> Option<PathBuf> {
        let key = root.to_str()?;
        Some(self.data_dir.join("jdtls").join(workspace::hash(key)))
    }

    /// Finds the version-suffixed Equinox launcher jar under
    /// `<jdtls_dir>/plugins/`. Mirrors jdtls.py's `find_equinox_launcher`:
    /// prefers the unversioned `org.eclipse.equinox.launcher.jar` some
    /// packagings use, otherwise the first `org.eclipse.equinox.launcher_*
    /// .jar` found.
    fn find_launcher_jar(jdtls_dir: &Path) -> Option<PathBuf> {
        let plugins_dir = jdtls_dir.join("plugins");
        let unversioned = plugins_dir.join("org.eclipse.equinox.launcher.jar");
        if unversioned.is_file() {
            return Some(unversioned);
        }
        let entries = std::fs::read_dir(&plugins_dir).ok()?;
        entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| {
                        name.starts_with("org.eclipse.equinox.launcher_") && name.ends_with(".jar")
                    })
            })
    }

    /// The platform's shared OSGi configuration directory, matching
    /// jdtls.py's `get_shared_config_path`.
    fn shared_config_dir(jdtls_dir: &Path) -> PathBuf {
        let name = if cfg!(windows) {
            "config_win"
        } else if cfg!(target_os = "macos") {
            "config_mac"
        } else {
            "config_linux"
        };
        jdtls_dir.join(name)
    }
}

#[async_trait]
impl LspAdapter for JavaServer {
    fn provider(&self) -> &'static str {
        "jdtls"
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
        "java_language_server_available"
    }

    fn transport_config(&self, root: &Path) -> Option<TransportConfig> {
        let jdtls_dir = self.config.path.as_deref()?;
        if jdtls_dir.trim().is_empty() {
            return None;
        }
        let jdtls_dir = Path::new(jdtls_dir);
        let launcher_jar = Self::find_launcher_jar(jdtls_dir)?;
        let config_dir = Self::shared_config_dir(jdtls_dir);
        let data_dir = self.workspace_data_dir(root)?;
        let _ = std::fs::create_dir_all(&data_dir);

        let mut args: Vec<String> = vec![
            "-Declipse.application=org.eclipse.jdt.ls.core.id1".into(),
            "-Dosgi.bundles.defaultStartLevel=4".into(),
            "-Declipse.product=org.eclipse.jdt.ls.core.product".into(),
            "-Dosgi.checkConfiguration=true".into(),
            format!(
                "-Dosgi.sharedConfiguration.area={}",
                config_dir.to_string_lossy()
            ),
            "-Dosgi.sharedConfiguration.area.readOnly=true".into(),
            "-Dosgi.configuration.cascaded=true".into(),
            "-Xms1G".into(),
            "--add-modules=ALL-SYSTEM".into(),
            "--add-opens".into(),
            "java.base/java.util=ALL-UNNAMED".into(),
            "--add-opens".into(),
            "java.base/java.lang=ALL-UNNAMED".into(),
            "-jar".into(),
            launcher_jar.to_string_lossy().into_owned(),
            "-data".into(),
            data_dir.to_string_lossy().into_owned(),
        ];
        args.extend(self.config.args.clone());

        Some(TransportConfig {
            executable: "java".to_string(),
            args,
            working_dir: root.to_path_buf(),
            timeout: self.timeout,
            label: "jdtls".into(),
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
        Box::new(|msg: &Value| {
            msg.get("method").and_then(Value::as_str) == Some("language/status")
                && msg["params"]["type"].as_str() == Some("Started")
        })
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
            .with_context(|| format!("read {relative} to open in jdtls"))?;
        let uri = reqwest::Url::from_file_path(file)
            .map_err(|_| anyhow!("cannot convert file path to URI"))?
            .to_string();
        rpc.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": "java",
                    "version": 1,
                    "text": content
                }
            }),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn config_with_java(java: JavaLspConfig) -> Config {
        Config {
            java,
            ..Config::default()
        }
    }

    /// A minimal fake jdtls install directory good enough for
    /// `transport_config` to resolve a launcher jar and config dir without
    /// a real jdtls download.
    fn fake_jdtls_install() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let plugins = dir.path().join("plugins");
        std::fs::create_dir_all(&plugins).unwrap();
        std::fs::write(
            plugins.join("org.eclipse.equinox.launcher_1.8.0.v20260804-1928.jar"),
            b"",
        )
        .unwrap();
        for name in ["config_win", "config_mac", "config_linux"] {
            std::fs::create_dir_all(dir.path().join(name)).unwrap();
        }
        dir
    }

    #[test]
    fn adapter_reports_java() {
        let jdtls = fake_jdtls_install();
        let config = config_with_java(JavaLspConfig {
            path: Some(jdtls.path().to_string_lossy().into_owned()),
            args: vec![],
            disabled: false,
        });
        let server = JavaServer::new(&config, Duration::from_secs(60));
        assert_eq!(server.languages(), &["java"]);
        assert!(server.enabled());
    }

    #[test]
    fn empty_or_missing_path_is_not_enabled() {
        let missing = config_with_java(JavaLspConfig::default());
        assert!(!JavaServer::new(&missing, Duration::from_secs(60)).enabled());
        assert!(
            JavaServer::new(&missing, Duration::from_secs(60))
                .transport_config(Path::new("/tmp"))
                .is_none()
        );

        let empty = config_with_java(JavaLspConfig {
            path: Some("   ".into()),
            args: vec![],
            disabled: false,
        });
        assert!(!JavaServer::new(&empty, Duration::from_secs(60)).enabled());

        let jdtls = fake_jdtls_install();
        let disabled = config_with_java(JavaLspConfig {
            path: Some(jdtls.path().to_string_lossy().into_owned()),
            args: vec![],
            disabled: true,
        });
        assert!(!JavaServer::new(&disabled, Duration::from_secs(60)).enabled());
    }

    #[test]
    fn missing_launcher_jar_yields_no_transport() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("plugins")).unwrap();
        let config = config_with_java(JavaLspConfig {
            path: Some(dir.path().to_string_lossy().into_owned()),
            args: vec![],
            disabled: false,
        });
        let server = JavaServer::new(&config, Duration::from_secs(30));
        let workspace = tempfile::tempdir().unwrap();
        assert!(server.transport_config(workspace.path()).is_none());
    }

    #[test]
    fn transport_spawns_java_directly_with_launcher_and_data_flags() {
        let jdtls = fake_jdtls_install();
        let config = config_with_java(JavaLspConfig {
            path: Some(jdtls.path().to_string_lossy().into_owned()),
            args: vec!["-Xmx2G".into()],
            disabled: false,
        });
        let server = JavaServer::new(&config, Duration::from_secs(30));
        let workspace = tempfile::tempdir().unwrap();
        let transport = server.transport_config(workspace.path()).unwrap();
        assert_eq!(transport.executable, "java");
        assert!(transport.args.iter().any(|a| a == "-jar"));
        assert!(
            transport
                .args
                .iter()
                .any(|a| a.contains("org.eclipse.equinox.launcher"))
        );
        assert!(transport.args.iter().any(|a| a == "-data"));
        assert_eq!(transport.args.last(), Some(&"-Xmx2G".to_string()));
    }

    #[test]
    fn same_workspace_gets_a_stable_data_dir_across_calls() {
        let jdtls = fake_jdtls_install();
        let config = config_with_java(JavaLspConfig {
            path: Some(jdtls.path().to_string_lossy().into_owned()),
            args: vec![],
            disabled: false,
        });
        let server = JavaServer::new(&config, Duration::from_secs(30));
        let workspace = tempfile::tempdir().unwrap();
        let first = server.transport_config(workspace.path()).unwrap();
        let second = server.transport_config(workspace.path()).unwrap();
        let data_index = first.args.iter().position(|a| a == "-data").unwrap() + 1;
        assert_eq!(first.args[data_index], second.args[data_index]);
    }

    #[test]
    fn ready_predicate_matches_only_started_status() {
        let config = config_with_java(JavaLspConfig::default());
        let server = JavaServer::new(&config, Duration::from_secs(30));
        let predicate = server.ready_predicate();
        assert!(predicate(&json!({
            "method": "language/status",
            "params": {"type": "Started", "message": "Ready"}
        })));
        assert!(!predicate(&json!({
            "method": "language/status",
            "params": {"type": "Starting", "message": "Init..."}
        })));
        assert!(!predicate(
            &json!({"method": "textDocument/publishDiagnostics"})
        ));
    }

    #[test]
    fn server_can_be_represented_without_a_process() {
        let jdtls = fake_jdtls_install();
        let config = config_with_java(JavaLspConfig {
            path: Some(jdtls.path().to_string_lossy().into_owned()),
            args: vec![],
            disabled: false,
        });
        let server = JavaServer::new(&config, Duration::from_secs(30));
        let workspace = tempfile::tempdir().unwrap();
        let params = server.initialize_params(workspace.path()).unwrap();
        assert_eq!(params["processId"], std::process::id());
        assert!(params["rootUri"].as_str().is_some());
    }
}
