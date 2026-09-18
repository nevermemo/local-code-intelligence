//! Optional C# language-server (csharp-ls) adapter.
//!
//! C# tooling is optional and fail-open: it never affects syntax retrieval.
//! This module provides the adapter data, enable/disable handling,
//! deterministic project/solution discovery, and the initialization payload
//! for a standalone stdin/stdout `csharp-ls` process. It can represent the
//! server without contacting a real process; spawning is left to the caller.

use crate::config::CSharpLspConfig;
use crate::lsp::adapter::LspAdapter;
use crate::lsp::transport::TransportConfig;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Canonical language identifier for C# source files.
pub const LANGUAGE_IDENTIFIER: &str = "csharp";

/// Recognized C# source extension (without the leading dot).
pub const EXTENSION: &str = "cs";

/// The kind of C# workspace entry point discovered beneath the root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    /// A Visual Studio solution file (`.sln`).
    Solution,
    /// A C# project file (`.csproj`).
    Project,
    /// No solution or project was found; the root itself is used.
    Root,
}

/// A deterministic C# workspace entry point discovered beneath the root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CSharpProject {
    /// Absolute path to the solution, project, or the root itself.
    pub path: PathBuf,
    /// The kind of entry point that was selected.
    pub kind: ProjectKind,
}

/// A narrow, client-neutral representation of the optional C# language
/// server. It carries configuration and can produce the transport and
/// initialization payloads without spawning a process.
#[derive(Debug, Clone)]
pub struct CSharpServer {
    config: CSharpLspConfig,
    timeout: Duration,
}

impl CSharpServer {
    /// Build a C# server representation from the shared configuration.
    ///
    /// The shared `lsp_timeout_seconds` applies to the C# server.
    pub fn new(config: &CSharpLspConfig, timeout: Duration) -> Self {
        Self {
            config: config.clone(),
            timeout,
        }
    }

    /// The canonical language identifier reported by this adapter.
    pub fn language_identifier(&self) -> &'static str {
        LANGUAGE_IDENTIFIER
    }

    /// The recognized source extension (without the leading dot).
    pub fn extension(&self) -> &'static str {
        EXTENSION
    }

    /// Whether the C# server is enabled.
    ///
    /// Enabled only when not explicitly disabled and a nonempty executable
    /// path is configured. An empty or whitespace-only path is never enabled.
    pub fn enabled(&self) -> bool {
        self.config.enabled()
    }

    /// The configured executable path, if any.
    pub fn executable(&self) -> Option<&str> {
        self.config.path.as_deref()
    }

    /// The configured executable arguments.
    pub fn args(&self) -> &[String] {
        &self.config.args
    }

    /// The shared LSP timeout applied to the C# server.
    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Build the transport configuration for spawning `csharp-ls` in the
    /// given workspace. This does not spawn a process.
    ///
    /// Returns `None` when the server is disabled.
    pub fn transport_config(&self, root: &Path) -> Option<TransportConfig> {
        let executable = self.config.path.as_deref()?;
        if executable.trim().is_empty() {
            return None;
        }
        Some(TransportConfig {
            executable: executable.to_string(),
            args: self.config.args.clone(),
            working_dir: root.to_path_buf(),
            timeout: self.timeout,
            label: "csharp-ls".into(),
        })
    }

    /// Build the LSP `initialize` parameters for the canonical indexed
    /// workspace. Project discovery remains deterministic and is exposed as
    /// initialization metadata without replacing the canonical root.
    ///
    /// This does not contact a real process.
    pub fn initialize_params(&self, root: &Path) -> Option<Value> {
        if !self.enabled() {
            return None;
        }
        let project = discover_project(root);
        let root_uri = reqwest::Url::from_directory_path(root).ok()?.to_string();
        let name = project
            .path
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("workspace")
            .to_string();
        Some(json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "capabilities": {
                "workspace": {
                    "symbol": {
                        "resolveSupport": { "properties": ["location.range"] }
                    }
                }
            },
            "workspaceFolders": [{ "uri": root_uri, "name": name }],
            "initializationOptions": {
                "csharpLspProject": project.path.to_string_lossy()
            }
        }))
    }
}

#[async_trait]
impl LspAdapter for CSharpServer {
    fn provider(&self) -> &'static str {
        "csharp-ls"
    }

    fn languages(&self) -> &'static [&'static str] {
        &[LANGUAGE_IDENTIFIER]
    }

    // Calls are qualified with the type name (rather than `self.enabled()`
    // etc.) even though inherent methods would win method resolution anyway
    // — this makes it unambiguous to a reader that these delegate to the
    // inherent methods above, not recurse into the trait method itself.
    fn enabled(&self) -> bool {
        CSharpServer::enabled(self)
    }

    fn configured_command(&self) -> Option<String> {
        self.config.path.clone()
    }

    fn readiness_component_name(&self) -> &'static str {
        "csharp_language_server_available"
    }

    fn transport_config(&self, root: &Path) -> Option<TransportConfig> {
        CSharpServer::transport_config(self, root)
    }

    fn initialize_params(&self, root: &Path) -> Option<Value> {
        CSharpServer::initialize_params(self, root)
    }

    fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        // csharp-ls has no analogous quiescent-server notification; readiness
        // relies purely on the bounded request timeout.
        Box::new(|_| false)
    }
}

/// Directories that are never searched during project/solution discovery.
///
/// These are build output, dependency, and version-control directories that
/// would otherwise add noise or non-determinism to the scan.
const SKIP_DIRS: &[&str] = &[
    "bin",
    "obj",
    "node_modules",
    "target",
    "packages",
    ".git",
    ".vs",
    ".vscode",
];

/// Deterministically discover the C# workspace entry point beneath `root`.
///
/// Precedence (documented and stable):
/// 1. A solution (`.sln`) is preferred over a project (`.csproj`).
/// 2. Among candidates of the same kind, the shallowest path (fewest
///    components below `root`) wins.
/// 3. Ties at the same depth are broken lexicographically by the full path.
/// 4. If no solution or project is found, the root itself is returned.
///
/// The scan is bounded: hidden directories and well-known build/dependency
/// directories are skipped.
pub fn discover_project(root: &Path) -> CSharpProject {
    let mut best_solution: Option<(usize, PathBuf)> = None;
    let mut best_project: Option<(usize, PathBuf)> = None;

    if root.is_dir() {
        walk(root, 0, &mut best_solution, &mut best_project);
    }

    match (best_solution, best_project) {
        (Some((_, path)), _) => CSharpProject {
            path,
            kind: ProjectKind::Solution,
        },
        (None, Some((_, path))) => CSharpProject {
            path,
            kind: ProjectKind::Project,
        },
        (None, None) => CSharpProject {
            path: root.to_path_buf(),
            kind: ProjectKind::Root,
        },
    }
}

/// Recursively collect the shallowest, lexicographically-first `.sln` and
/// `.csproj` beneath `dir`, skipping hidden and well-known build directories.
fn walk(
    dir: &Path,
    depth: usize,
    best_solution: &mut Option<(usize, PathBuf)>,
    best_project: &mut Option<(usize, PathBuf)>,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    // Sort entries by name so traversal order is deterministic.
    let mut entries: Vec<std::fs::DirEntry> = entries.filter_map(|entry| entry.ok()).collect();
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let path = entry.path();
        if path.is_dir() {
            if name.starts_with('.') || SKIP_DIRS.contains(&name) {
                continue;
            }
            walk(&path, depth + 1, best_solution, best_project);
        } else if name.ends_with(".sln") {
            consider(best_solution, depth, path);
        } else if name.ends_with(".csproj") {
            consider(best_project, depth, path);
        }
    }
}

/// Keep the shallowest, then lexicographically-first candidate.
fn consider(slot: &mut Option<(usize, PathBuf)>, depth: usize, path: PathBuf) {
    match slot {
        None => *slot = Some((depth, path)),
        Some((current_depth, current_path)) => {
            if depth < *current_depth || (depth == *current_depth && path < *current_path) {
                *slot = Some((depth, path));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &Path, content: &str) {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    #[test]
    fn adapter_reports_csharp_and_cs() {
        let config = CSharpLspConfig {
            path: Some("csharp-ls".into()),
            args: vec![],
            disabled: false,
        };
        let server = CSharpServer::new(&config, Duration::from_secs(60));
        assert_eq!(server.language_identifier(), "csharp");
        assert_eq!(server.extension(), "cs");
        assert!(server.enabled());
    }

    #[test]
    fn empty_or_missing_path_is_not_enabled() {
        // Missing path.
        let missing = CSharpLspConfig::default();
        assert!(!CSharpServer::new(&missing, Duration::from_secs(60)).enabled());
        assert!(
            CSharpServer::new(&missing, Duration::from_secs(60))
                .transport_config(Path::new("/tmp"))
                .is_none()
        );

        // Empty path is not silently enabled.
        let empty = CSharpLspConfig {
            path: Some("   ".into()),
            args: vec![],
            disabled: false,
        };
        assert!(!CSharpServer::new(&empty, Duration::from_secs(60)).enabled());
        assert!(
            CSharpServer::new(&empty, Duration::from_secs(60))
                .transport_config(Path::new("/tmp"))
                .is_none()
        );

        // Explicitly disabled even with a path.
        let disabled = CSharpLspConfig {
            path: Some("csharp-ls".into()),
            args: vec![],
            disabled: true,
        };
        assert!(!CSharpServer::new(&disabled, Duration::from_secs(60)).enabled());
    }

    #[test]
    fn server_can_be_represented_without_a_process() {
        let config = CSharpLspConfig {
            path: Some("csharp-ls".into()),
            args: vec!["--stdio".into()],
            disabled: false,
        };
        let server = CSharpServer::new(&config, Duration::from_secs(30));
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        let transport = server.transport_config(root).unwrap();
        assert_eq!(transport.executable, "csharp-ls");
        assert_eq!(transport.label, "csharp-ls");
        assert_eq!(server.args(), &["--stdio".to_string()]);

        let params = server.initialize_params(root).unwrap();
        assert_eq!(params["processId"], std::process::id());
        assert!(params["rootUri"].as_str().is_some());
    }

    #[test]
    fn discovery_prefers_solution_over_project() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("App.csproj"), "<Project/>");
        write(&root.join("Solution.sln"), "");
        let project = discover_project(root);
        assert_eq!(project.kind, ProjectKind::Solution);
        assert_eq!(project.path, root.join("Solution.sln"));
    }

    #[test]
    fn discovery_falls_back_to_project_then_root() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("App.csproj"), "<Project/>");
        let project = discover_project(root);
        assert_eq!(project.kind, ProjectKind::Project);
        assert_eq!(project.path, root.join("App.csproj"));

        let empty = tempfile::tempdir().unwrap();
        let rootless = discover_project(empty.path());
        assert_eq!(rootless.kind, ProjectKind::Root);
        assert_eq!(rootless.path, empty.path().to_path_buf());
    }

    #[test]
    fn discovery_is_deterministic_shallowest_then_lexicographic() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        // Two projects at the same depth: lexicographic tie-break.
        write(&root.join("b.csproj"), "<Project/>");
        write(&root.join("a.csproj"), "<Project/>");
        // A deeper project should lose to the shallower one.
        write(&root.join("deep").join("z.csproj"), "<Project/>");
        let project = discover_project(root);
        assert_eq!(project.kind, ProjectKind::Project);
        assert_eq!(project.path, root.join("a.csproj"));
    }

    #[test]
    fn discovery_skips_build_and_hidden_directories() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path();
        write(&root.join("obj").join("gen.csproj"), "<Project/>");
        write(&root.join(".git").join("hidden.csproj"), "<Project/>");
        write(&root.join("real.csproj"), "<Project/>");
        let project = discover_project(root);
        assert_eq!(project.kind, ProjectKind::Project);
        assert_eq!(project.path, root.join("real.csproj"));
    }
}
