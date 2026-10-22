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
use crate::lsp::adapter::{LspAdapter, SentTexts, open_if_changed};
use crate::lsp::transport::{JsonRpcClient, TransportConfig};
use crate::workspace;
use anyhow::{Context, Result};
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
    /// packagings use, otherwise the highest-versioned
    /// `org.eclipse.equinox.launcher_<version>.jar`. `read_dir` order is
    /// unspecified, so "the first one found" could swap between two installed
    /// versions from run to run; the highest version (see
    /// [`launcher_version_key`]) is the one a jdtls upgrade in place leaves
    /// current, and the file name breaks any remaining tie.
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
            .filter(|path| path.is_file())
            .filter_map(|path| {
                let key = launcher_version_key(path.file_name()?.to_str()?)?;
                Some((key, path))
            })
            .max_by(|(a_key, a_path), (b_key, b_path)| {
                a_key.cmp(b_key).then_with(|| a_path.cmp(b_path))
            })
            .map(|(_, path)| path)
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

    /// The configured jdtls installation directory, made absolute. A relative
    /// `java.path` is taken from the process's working directory (as Windows
    /// resolves a relative executable path), and has to be made
    /// absolute here: the launcher jar and the configuration directory are
    /// handed to `java`, which runs in the workspace directory, so a relative
    /// path would find them in one place and name them from another. No
    /// filesystem access and no symlink resolution. `None` when nothing (or
    /// only whitespace) is configured.
    fn install_dir(&self) -> Option<PathBuf> {
        let configured = self.config.path.as_deref()?;
        if configured.trim().is_empty() {
            return None;
        }
        let path = PathBuf::from(configured);
        Some(std::path::absolute(&path).unwrap_or(path))
    }
}

/// Sort key of an `org.eclipse.equinox.launcher_<version>.jar` file name, or
/// `None` for any other name. The version is OSGi's `major.minor.micro.
/// qualifier` (for example `1.6.900.v20240613-2009`): the leading numeric
/// components compare as numbers (so `1.10` is above `1.9`), then the rest
/// compares as text, as OSGi compares a qualifier.
fn launcher_version_key(file_name: &str) -> Option<(Vec<u64>, String)> {
    let version = file_name
        .strip_prefix("org.eclipse.equinox.launcher_")?
        .strip_suffix(".jar")?;
    let mut parts = version.split('.').peekable();
    let mut numbers = Vec::new();
    while let Some(number) = parts.peek().and_then(|part| part.parse::<u64>().ok()) {
        numbers.push(number);
        parts.next();
    }
    Some((numbers, parts.collect::<Vec<_>>().join(".")))
}

/// Java options that take their value as the next argument when written in the
/// two-argument form (`--add-opens java.base/java.util=ALL-UNNAMED`).
const JVM_OPTIONS_WITH_VALUE: &[&str] = &[
    "--add-exports",
    "--add-modules",
    "--add-opens",
    "--add-reads",
    "--enable-native-access",
    "--finalization",
    "--limit-modules",
    "--module-path",
    "--patch-module",
    "--sun-misc-unsafe-memory-access",
    "--upgrade-module-path",
];

/// Prefixes of the options `java` takes before `-jar` (the JVM's own: `-X...`
/// and `-XX:...`, `-D...`, agents, the long assertion flags, the `--add-...`
/// module flags, and `@argfile`, which the launcher expands in place). The
/// short assertion flags are in [`JVM_SHORT_OPTIONS`], not here: as prefixes
/// they would take jdtls's own `-data`.
const JVM_OPTION_PREFIXES: &[&str] = &[
    "-X",
    "-D",
    "-javaagent:",
    "-agentlib:",
    "-agentpath:",
    "@",
    "-enableassertions",
    "-disableassertions",
    "-enablesystemassertions",
    "-disablesystemassertions",
    "-verbose",
    "-server",
    "-client",
    "--add-",
    "--enable-native-access",
    "--enable-preview",
    "--finalization",
    "--limit-modules",
    "--module-path",
    "--patch-module",
    "--sun-misc-unsafe-memory-access",
    "--upgrade-module-path",
    "--illegal-access",
];

/// The short assertion flags, which are JVM options only exactly or with a
/// `:<package or class>` suffix (`-ea`, `-da:com.example...`).
const JVM_SHORT_OPTIONS: &[&str] = &["-ea", "-da", "-esa", "-dsa"];

/// Whether `arg` is an option for the JVM rather than for jdtls.
fn is_jvm_option(arg: &str) -> bool {
    JVM_OPTION_PREFIXES
        .iter()
        .any(|prefix| arg.starts_with(prefix))
        || JVM_SHORT_OPTIONS.iter().any(|option| {
            arg.strip_prefix(option)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
        })
}

/// Splits the configured `java.args` into the options for the JVM, which must
/// come before `-jar` (after it, `java` hands them to the application, and the
/// JVM never sees them), and the arguments for jdtls itself, which follow
/// `-data <dir>`. An argument is a JVM option when [`is_jvm_option`] says so, together with the next argument for a
/// [`JVM_OPTIONS_WITH_VALUE`] option written in the two-argument form; every
/// other argument is a jdtls argument. Each list keeps the configured order.
fn split_java_args(args: &[String]) -> (Vec<String>, Vec<String>) {
    let (mut jvm, mut application) = (Vec::new(), Vec::new());
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        if is_jvm_option(arg) {
            jvm.push(arg.clone());
            if JVM_OPTIONS_WITH_VALUE.contains(&arg.as_str())
                && let Some(value) = iter.next()
            {
                jvm.push(value.clone());
            }
        } else {
            application.push(arg.clone());
        }
    }
    (jvm, application)
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

    async fn probe_readiness(&self, timeout: Duration) -> Result<()> {
        let Some(jdtls_dir) = self.install_dir() else {
            return Ok(());
        };
        let launcher = tokio::time::timeout(
            timeout,
            tokio::task::spawn_blocking(move || Self::find_launcher_jar(&jdtls_dir)),
        )
        .await
        .context("timed out finding jdtls launcher jar")??;
        launcher.context(
            "jdtls launcher jar not found in the configured installation's plugins directory",
        )?;
        Ok(())
    }

    fn transport_config(&self, root: &Path) -> Option<TransportConfig> {
        let jdtls_dir = self.install_dir()?;
        let launcher_jar = Self::find_launcher_jar(&jdtls_dir)?;
        let config_dir = Self::shared_config_dir(&jdtls_dir);
        let (jvm_args, application_args) = split_java_args(&self.config.args);
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
        ];
        // The configured JVM options go last among the JVM's own, so that
        // they win over the defaults above (the JVM takes the last of two
        // `-Xms`), and still before `-jar`.
        args.extend(jvm_args);
        args.extend([
            "-jar".to_string(),
            launcher_jar.to_string_lossy().into_owned(),
            "-data".to_string(),
            data_dir.to_string_lossy().into_owned(),
        ]);
        args.extend(application_args);

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

    fn busy_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        Box::new(|msg: &Value| {
            msg.get("method").and_then(Value::as_str) == Some("language/status")
                && msg["params"]["type"].as_str() == Some("Starting")
        })
    }

    async fn after_initialized(&self, rpc: &JsonRpcClient) -> Result<()> {
        rpc.notify("workspace/didChangeConfiguration", json!({"settings": {}}))
            .await
    }

    // The hook sends the current disk text whenever it differs from what
    // this session sent, and jdtls sets the working copy's buffer to the didOpen text when it
    // differs, also for a unit that is already open.
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
        open_if_changed(rpc, sent, file, relative, "jdtls", "java")
            .await
            .map(|_| ())
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
    fn the_java_adapter_follows_the_adapter_contract() {
        // it reports java
        {
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

        // it is enabled only with a usable path
        {
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

        // it can be represented without a process
        {
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

    #[test]
    fn the_jdtls_transport() {
        // a missing launcher jar yields no transport
        {
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

        // java is spawned directly with the launcher and data flags
        {
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
            // A JVM option is only a JVM option before `-jar`.
            let jar = transport.args.iter().position(|a| a == "-jar").unwrap();
            let heap = transport.args.iter().position(|a| a == "-Xmx2G").unwrap();
            assert!(heap < jar, "{:?}", transport.args);
        }

        // the same workspace gets a stable data dir
        {
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
    }

    #[test]
    fn java_args_are_split_into_jvm_options_and_jdtls_arguments() {
        let args = |list: &[&str]| list.iter().map(|a| a.to_string()).collect::<Vec<_>>();
        let (jvm, application) = split_java_args(&args(&[
            "-Xmx2G",
            "-Dfoo=bar",
            "--add-opens",
            "java.base/java.io=ALL-UNNAMED",
            "-configuration",
            "/tmp/cfg",
            "--add-exports=java.base/sun.nio.ch=ALL-UNNAMED",
            "-javaagent:/tmp/agent.jar",
            "-consoleLog",
            "-data",
            "/tmp/other",
            "-debug",
            "-clean",
            "-ea",
            "-da:com.example...",
            "-esa",
            "-enableassertions:pkg",
            "--enable-native-access",
            "ALL-UNNAMED",
            "--sun-misc-unsafe-memory-access=allow",
            "--finalization=disabled",
            "@/tmp/jvm.args",
        ]));
        assert_eq!(
            jvm,
            args(&[
                "-Xmx2G",
                "-Dfoo=bar",
                "--add-opens",
                "java.base/java.io=ALL-UNNAMED",
                "--add-exports=java.base/sun.nio.ch=ALL-UNNAMED",
                "-javaagent:/tmp/agent.jar",
                "-ea",
                "-da:com.example...",
                "-esa",
                "-enableassertions:pkg",
                "--enable-native-access",
                "ALL-UNNAMED",
                "--sun-misc-unsafe-memory-access=allow",
                "--finalization=disabled",
                "@/tmp/jvm.args",
            ])
        );
        // `-data`, `-debug` and `-clean` are jdtls (Eclipse launcher) arguments;
        // `-data` starts with the letters of `-da`.
        assert_eq!(
            application,
            args(&[
                "-configuration",
                "/tmp/cfg",
                "-consoleLog",
                "-data",
                "/tmp/other",
                "-debug",
                "-clean"
            ])
        );

        // The command line is: JVM options, `-jar <launcher> -data <dir>`,
        // then the jdtls arguments, each group in configured order.
        let jdtls = fake_jdtls_install();
        let config = config_with_java(JavaLspConfig {
            path: Some(jdtls.path().to_string_lossy().into_owned()),
            args: args(&["-configuration", "/tmp/cfg", "-Xmx2G"]),
            disabled: false,
        });
        let server = JavaServer::new(&config, Duration::from_secs(30));
        let workspace = tempfile::tempdir().unwrap();
        let line = server.transport_config(workspace.path()).unwrap().args;
        let at = |needle: &str| line.iter().position(|a| a == needle).unwrap();
        assert!(at("-Xmx2G") < at("-jar"), "{line:?}");
        assert!(at("-data") < at("-configuration"), "{line:?}");
        assert_eq!(
            line[line.len() - 2..],
            args(&["-configuration", "/tmp/cfg"])
        );
    }

    #[test]
    fn the_launcher_jar_is_the_highest_version_whatever_the_directory_order() {
        let install = |names: &[&str]| {
            let dir = tempfile::tempdir().unwrap();
            let plugins = dir.path().join("plugins");
            std::fs::create_dir_all(&plugins).unwrap();
            for name in names {
                std::fs::write(plugins.join(name), b"").unwrap();
            }
            dir
        };
        let jar = |dir: &tempfile::TempDir| {
            JavaServer::find_launcher_jar(dir.path())
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        };
        // Names sorted by text put 1.6 first, which is also what a directory
        // listing in name order (NTFS) hands out first.
        let dir = install(&[
            "org.eclipse.equinox.launcher_1.6.900.v20240613-2009.jar",
            "org.eclipse.equinox.launcher_1.9.0.v20250101-1000.jar",
            "org.eclipse.equinox.launcher.source_1.9.0.jar",
            "org.eclipse.equinox.launcher.win32_1.2.0.jar",
        ]);
        assert_eq!(
            jar(&dir).as_deref(),
            Some("org.eclipse.equinox.launcher_1.9.0.v20250101-1000.jar")
        );
        // The version compares as numbers, not as text: 1.10 is above 1.9.
        let dir = install(&[
            "org.eclipse.equinox.launcher_1.9.0.v20250101-1000.jar",
            "org.eclipse.equinox.launcher_1.10.100.v20240101-0900.jar",
            "org.eclipse.equinox.launcher_1.6.900.v20240613-2009.jar",
        ]);
        assert_eq!(
            jar(&dir).as_deref(),
            Some("org.eclipse.equinox.launcher_1.10.100.v20240101-0900.jar")
        );
        // The unversioned jar still wins when it is there.
        let dir = install(&[
            "org.eclipse.equinox.launcher.jar",
            "org.eclipse.equinox.launcher_1.9.0.v20250101-1000.jar",
        ]);
        assert_eq!(
            jar(&dir).as_deref(),
            Some("org.eclipse.equinox.launcher.jar")
        );
    }

    #[test]
    fn a_relative_jdtls_path_reaches_java_as_an_absolute_one() {
        // The install directory is relative to the process's working
        // directory; `java` runs in the workspace, so what it is handed must
        // not depend on that.
        let jdtls = fake_jdtls_install();
        let cwd = std::env::current_dir().unwrap();
        let Some(relative) = pathdiff_relative(jdtls.path(), &cwd) else {
            // No relative path exists (a different drive from the test's
            // working directory): nothing to check here.
            return;
        };
        let config = config_with_java(JavaLspConfig {
            path: Some(relative.to_string_lossy().into_owned()),
            args: vec![],
            disabled: false,
        });
        let server = JavaServer::new(&config, Duration::from_secs(30));
        let workspace = tempfile::tempdir().unwrap();
        let transport = server.transport_config(workspace.path()).unwrap();
        let jar = transport
            .args
            .iter()
            .position(|a| a == "-jar")
            .map(|i| PathBuf::from(&transport.args[i + 1]))
            .unwrap();
        assert!(jar.is_absolute(), "{jar:?}");
        assert!(jar.is_file(), "{jar:?}");
        let area = transport
            .args
            .iter()
            .find_map(|a| a.strip_prefix("-Dosgi.sharedConfiguration.area="))
            .unwrap();
        assert!(Path::new(area).is_absolute(), "{area}");
    }

    /// `target` as a path relative to `base`, both absolute; `None` when they
    /// share no root (another drive on Windows).
    fn pathdiff_relative(target: &Path, base: &Path) -> Option<PathBuf> {
        let target = target.canonicalize().ok()?;
        let base = base.canonicalize().ok()?;
        let (mut t, mut b) = (target.components().peekable(), base.components().peekable());
        if t.peek() != b.peek() {
            return None;
        }
        while t.peek().is_some() && t.peek() == b.peek() {
            t.next();
            b.next();
        }
        let mut relative = PathBuf::new();
        for _ in b {
            relative.push("..");
        }
        relative.extend(t);
        Some(relative)
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
    fn the_busy_predicate_matches_the_starting_status_only() {
        let config = config_with_java(JavaLspConfig::default());
        let server = JavaServer::new(&config, Duration::from_secs(30));
        let predicate = server.busy_predicate();
        assert!(predicate(&json!({
            "method": "language/status",
            "params": {"type": "Starting", "message": "Init..."}
        })));
        assert!(!predicate(&json!({
            "method": "language/status",
            "params": {"type": "Started", "message": "Ready"}
        })));
        assert!(!predicate(&json!({
            "method": "textDocument/publishDiagnostics",
            "params": {"type": "Starting"}
        })));
    }
}
