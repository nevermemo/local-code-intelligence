use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Controls when a search request may trigger an automatic incremental
/// refresh of a workspace's index. `on-search` is the default for coding
/// agents: freshness is checked lazily on the first retrieval that needs it,
/// throttled by `stale_check_interval_seconds`. `watch` defers refresh to the
/// background watcher started by `watch_workspace` instead of checking on
/// every search, which suits long-running editor sessions. `manual` never
/// refreshes automatically; callers must call `index_workspace` explicitly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IndexFreshness {
    Manual,
    #[default]
    OnSearch,
    Watch,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct IndexConfig {
    pub freshness: IndexFreshness,
    /// Minimum interval between filesystem staleness checks for a workspace
    /// under `on-search` freshness, so repeated searches in the same
    /// unchanged workspace reuse the current snapshot instead of rescanning.
    pub stale_check_interval_seconds: u64,
    /// How long a search waits for a concurrently running refresh job before
    /// falling back to the previous snapshot.
    pub wait_for_existing_job_seconds: u64,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            freshness: IndexFreshness::OnSearch,
            stale_check_interval_seconds: 10,
            wait_for_existing_job_seconds: 120,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub embedding_url: String,
    pub embedding_model: String,
    pub reranker_url: String,
    pub reranker_model: String,
    pub data_dir: PathBuf,
    pub default_top_k: usize,
    pub embedding_batch_size: usize,
    pub embedding_timeout_seconds: u64,
    pub reranker_timeout_seconds: u64,
    pub readiness_timeout_seconds: u64,
    pub ripgrep_path: String,
    pub semantic_candidate_count: usize,
    pub lexical_candidate_count: usize,
    pub rerank_candidate_count: usize,
    pub rrf_k: f32,
    pub watch_poll_milliseconds: u64,
    pub watch_debounce_milliseconds: u64,
    pub rust_analyzer_path: String,
    pub lsp_timeout_seconds: u64,
    pub lsp_candidate_count: usize,
    /// Optional C# language-server (csharp-ls) settings. Absent or empty
    /// configuration keeps C# tooling disabled; syntax retrieval is unaffected.
    pub csharp: CSharpLspConfig,
    /// Optional TypeScript/JavaScript language-server
    /// (typescript-language-server) settings. Absent or empty configuration
    /// keeps TS/JS navigation disabled; syntax retrieval is unaffected.
    pub typescript: TypeScriptLspConfig,
    /// Optional Python language-server (pyright) settings. Absent or empty
    /// configuration keeps Python navigation disabled; syntax retrieval is
    /// unaffected.
    pub python: PythonLspConfig,
    /// Optional Go language-server (gopls) settings. Absent or empty
    /// configuration keeps Go navigation disabled; syntax retrieval is
    /// unaffected.
    pub go: GoLspConfig,
    pub index: IndexConfig,
}

/// Optional C# language-server (csharp-ls) configuration.
///
/// C# tooling is optional and fail-open: an absent or empty `[csharp]`
/// section keeps the server disabled without affecting syntax retrieval.
/// The shared `lsp_timeout_seconds` and `lsp_candidate_count` settings apply
/// to the C# server as well.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CSharpLspConfig {
    /// Path to the standalone csharp-ls executable. `None` (the default)
    /// keeps C# tooling disabled. An empty or whitespace-only value is
    /// rejected by validation rather than silently counting as enabled.
    pub path: Option<String>,
    /// Optional arguments passed to the csharp-ls executable.
    pub args: Vec<String>,
    /// Explicitly disable the C# server even when a path is configured.
    pub disabled: bool,
}

impl CSharpLspConfig {
    /// Whether the C# language server is enabled.
    ///
    /// Enabled only when not explicitly disabled and a nonempty executable
    /// path is configured.
    pub fn enabled(&self) -> bool {
        !self.disabled
            && self
                .path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty())
    }
}

/// Optional TypeScript/JavaScript language-server
/// (typescript-language-server) configuration.
///
/// TS/JS navigation is optional and fail-open: an absent or empty
/// `[typescript]` section keeps the server disabled without affecting
/// syntax retrieval. The shared `lsp_timeout_seconds` and
/// `lsp_candidate_count` settings apply to this server as well. One
/// configured server navigates `.ts`, `.tsx`, `.js`, and `.jsx` files.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TypeScriptLspConfig {
    /// Path to the standalone typescript-language-server executable. `None`
    /// (the default) keeps TS/JS navigation disabled. An empty or
    /// whitespace-only value is rejected by validation rather than silently
    /// counting as enabled.
    pub path: Option<String>,
    /// Additional arguments passed to typescript-language-server, appended
    /// after the `--stdio` flag this application always supplies.
    pub args: Vec<String>,
    /// Explicitly disable the TS/JS server even when a path is configured.
    pub disabled: bool,
}

impl TypeScriptLspConfig {
    /// Whether the TypeScript/JavaScript language server is enabled.
    ///
    /// Enabled only when not explicitly disabled and a nonempty executable
    /// path is configured.
    pub fn enabled(&self) -> bool {
        !self.disabled
            && self
                .path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty())
    }
}

/// Optional Python language-server (pyright) configuration.
///
/// Python navigation is optional and fail-open: an absent or empty
/// `[python]` section keeps the server disabled without affecting syntax
/// retrieval. The shared `lsp_timeout_seconds` and `lsp_candidate_count`
/// settings apply to this server as well.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PythonLspConfig {
    /// Path to the standalone pyright-langserver executable. `None` (the
    /// default) keeps Python navigation disabled. An empty or
    /// whitespace-only value is rejected by validation rather than silently
    /// counting as enabled.
    pub path: Option<String>,
    /// Additional arguments passed to pyright-langserver, appended after the
    /// `--stdio` flag this application always supplies.
    pub args: Vec<String>,
    /// Explicitly disable the Python server even when a path is configured.
    pub disabled: bool,
}

impl PythonLspConfig {
    /// Whether the Python language server is enabled.
    ///
    /// Enabled only when not explicitly disabled and a nonempty executable
    /// path is configured.
    pub fn enabled(&self) -> bool {
        !self.disabled
            && self
                .path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty())
    }
}

/// Optional Go language-server (gopls) configuration.
///
/// Go navigation is optional and fail-open: an absent or empty `[go]`
/// section keeps the server disabled without affecting syntax retrieval.
/// The shared `lsp_timeout_seconds` and `lsp_candidate_count` settings apply
/// to this server as well. Unlike csharp-ls/typescript-language-server/
/// pyright, gopls needs no forced leading transport flag: stdio is its
/// default communication mode with no `-mode`/`--stdio`-equivalent flag
/// required (confirmed against `gopls help serve`, where `-mode` is
/// documented as "no effect").
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GoLspConfig {
    /// Path to the standalone gopls executable. `None` (the default) keeps
    /// Go navigation disabled. An empty or whitespace-only value is
    /// rejected by validation rather than silently counting as enabled.
    pub path: Option<String>,
    /// Additional arguments passed to gopls.
    pub args: Vec<String>,
    /// Explicitly disable the Go server even when a path is configured.
    pub disabled: bool,
}

impl GoLspConfig {
    /// Whether the Go language server is enabled.
    ///
    /// Enabled only when not explicitly disabled and a nonempty executable
    /// path is configured.
    pub fn enabled(&self) -> bool {
        !self.disabled
            && self
                .path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty())
    }
}

impl Default for Config {
    fn default() -> Self {
        // Per-OS local data directory: %LOCALAPPDATA% on Windows,
        // ~/Library/Application Support on macOS, $XDG_DATA_HOME or
        // ~/.local/share on Linux. Falls back to an empty relative path when
        // none of those resolve (rare: no home directory available), which
        // `validate()` rejects via its `data_dir.is_absolute()` check.
        let base = dirs::data_local_dir().unwrap_or_default();
        Self {
            embedding_url: "http://localhost:8766/v1".into(),
            embedding_model: "qwen3-embedding-4b".into(),
            reranker_url: "http://localhost:8767/rerank".into(),
            reranker_model: "qwen3-reranker-4b".into(),
            data_dir: base.join("local-code-intelligence"),
            default_top_k: 8,
            embedding_batch_size: 8,
            embedding_timeout_seconds: 120,
            reranker_timeout_seconds: 120,
            readiness_timeout_seconds: 5,
            ripgrep_path: "rg".into(),
            semantic_candidate_count: 40,
            lexical_candidate_count: 40,
            rerank_candidate_count: 24,
            rrf_k: 60.0,
            watch_poll_milliseconds: 2000,
            watch_debounce_milliseconds: 750,
            rust_analyzer_path: "rust-analyzer".into(),
            lsp_timeout_seconds: 60,
            lsp_candidate_count: 40,
            csharp: CSharpLspConfig::default(),
            typescript: TypeScriptLspConfig::default(),
            python: PythonLspConfig::default(),
            go: GoLspConfig::default(),
            index: IndexConfig::default(),
        }
    }
}

impl Config {
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let config: Self = match path {
            Some(p) => toml::from_str(&std::fs::read_to_string(p).context("read configuration")?)?,
            None => Self::default(),
        };
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.data_dir.is_absolute(),
            "data_dir must be absolute; set it explicitly if the platform's local data directory is unavailable"
        );
        ensure!(
            (1..=200).contains(&self.semantic_candidate_count)
                && (1..=200).contains(&self.lexical_candidate_count)
                && (1..=200).contains(&self.rerank_candidate_count)
                && (1..=200).contains(&self.lsp_candidate_count),
            "candidate counts must be 1..=200"
        );
        ensure!(
            self.rrf_k.is_finite() && self.rrf_k > 0.0,
            "rrf_k must be positive"
        );
        ensure!(
            self.watch_poll_milliseconds > 0 && self.watch_debounce_milliseconds > 0,
            "watch intervals must be positive"
        );
        ensure!(
            !self.ripgrep_path.trim().is_empty(),
            "ripgrep_path must be nonempty"
        );
        ensure!(
            !self.rust_analyzer_path.trim().is_empty() && self.lsp_timeout_seconds > 0,
            "rust-analyzer path must be nonempty and LSP timeout must be positive"
        );
        // An explicitly configured but empty C#/TypeScript/Python path is
        // malformed: it must be rejected rather than silently counting as
        // enabled.
        if !self.csharp.disabled
            && let Some(path) = &self.csharp.path
        {
            ensure!(
                !path.trim().is_empty(),
                "csharp.path must be nonempty when set; omit it or set csharp.disabled = true to keep C# tooling disabled"
            );
        }
        if !self.typescript.disabled
            && let Some(path) = &self.typescript.path
        {
            ensure!(
                !path.trim().is_empty(),
                "typescript.path must be nonempty when set; omit it or set typescript.disabled = true to keep TypeScript/JavaScript navigation disabled"
            );
        }
        if !self.python.disabled
            && let Some(path) = &self.python.path
        {
            ensure!(
                !path.trim().is_empty(),
                "python.path must be nonempty when set; omit it or set python.disabled = true to keep Python navigation disabled"
            );
        }
        if !self.go.disabled
            && let Some(path) = &self.go.path
        {
            ensure!(
                !path.trim().is_empty(),
                "go.path must be nonempty when set; omit it or set go.disabled = true to keep Go navigation disabled"
            );
        }
        ensure!(
            (1..=40).contains(&self.default_top_k),
            "default_top_k must be 1..=40"
        );
        ensure!(
            (1..=64).contains(&self.embedding_batch_size),
            "embedding_batch_size must be 1..=64"
        );
        ensure!(
            self.embedding_timeout_seconds > 0 && self.reranker_timeout_seconds > 0,
            "timeouts must be positive"
        );
        ensure!(
            (1..=30).contains(&self.readiness_timeout_seconds),
            "readiness_timeout_seconds must be 1..=30"
        );
        for value in [&self.embedding_url, &self.reranker_url] {
            let url = reqwest::Url::parse(value)?;
            ensure!(
                matches!(url.scheme(), "http" | "https"),
                "model URL must use HTTP(S)"
            );
        }
        ensure!(
            !self.embedding_model.is_empty() && !self.reranker_model.is_empty(),
            "model names must be nonempty"
        );
        Ok(())
    }

    pub fn embedding_identity(&self) -> String {
        format!(
            "{}|{}|{}",
            self.embedding_url.trim_end_matches('/'),
            self.embedding_model,
            crate::chunk::DOCUMENT_FORMAT_VERSION
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal absolute data_dir so `validate` passes in tests.
    /// Uses a TOML literal (single-quoted) string so Windows backslashes in
    /// the temp path are not interpreted as escape sequences.
    fn base_toml() -> String {
        format!("data_dir = '{}'\n", std::env::temp_dir().display())
    }

    #[test]
    fn default_config_loads_and_csharp_is_disabled() {
        let config = Config::default();
        config.validate().unwrap();
        assert!(!config.csharp.enabled());
        assert!(config.csharp.path.is_none());
        assert!(config.csharp.args.is_empty());
        assert!(!config.csharp.disabled);
    }

    #[test]
    fn old_config_shape_still_loads() {
        // A config containing only the pre-C# LSP fields must still load and
        // keep C# tooling disabled (backward compatibility).
        let toml_text = format!(
            "{}rust_analyzer_path = \"rust-analyzer\"\nlsp_timeout_seconds = 60\nlsp_candidate_count = 40\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        assert_eq!(config.rust_analyzer_path, "rust-analyzer");
        assert_eq!(config.lsp_timeout_seconds, 60);
        assert_eq!(config.lsp_candidate_count, 40);
        assert!(!config.csharp.enabled());
    }

    #[test]
    fn csharp_section_enables_server() {
        let toml_text = format!(
            "{}[csharp]\npath = \"csharp-ls\"\nargs = [\"--stdio\"]\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        assert!(config.csharp.enabled());
        assert_eq!(config.csharp.path.as_deref(), Some("csharp-ls"));
        assert_eq!(config.csharp.args, vec!["--stdio".to_string()]);
    }

    #[test]
    fn csharp_disabled_flag_overrides_path() {
        let toml_text = format!(
            "{}[csharp]\npath = \"csharp-ls\"\ndisabled = true\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        assert!(!config.csharp.enabled());
    }

    #[test]
    fn empty_csharp_path_is_rejected() {
        // An explicitly configured but empty path is malformed and must be
        // rejected rather than silently counting as enabled.
        let toml_text = format!("{}[csharp]\npath = \"   \"\n", base_toml());
        let config: Config = toml::from_str(&toml_text).unwrap();
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("csharp.path"));
    }

    #[test]
    fn unknown_csharp_field_is_rejected() {
        // `deny_unknown_fields` keeps the C# section narrow.
        let toml_text = format!(
            "{}[csharp]\npath = \"csharp-ls\"\nnot_a_field = 1\n",
            base_toml()
        );
        assert!(toml::from_str::<Config>(&toml_text).is_err());
    }

    #[test]
    fn default_go_is_disabled() {
        let config = Config::default();
        config.validate().unwrap();
        assert!(!config.go.enabled());
        assert!(config.go.path.is_none());
    }

    #[test]
    fn go_section_enables_server() {
        let toml_text = format!("{}[go]\npath = \"gopls\"\n", base_toml());
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        assert!(config.go.enabled());
        assert_eq!(config.go.path.as_deref(), Some("gopls"));
    }

    #[test]
    fn go_disabled_flag_overrides_path() {
        let toml_text = format!("{}[go]\npath = \"gopls\"\ndisabled = true\n", base_toml());
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        assert!(!config.go.enabled());
    }

    #[test]
    fn empty_go_path_is_rejected() {
        let toml_text = format!("{}[go]\npath = \"   \"\n", base_toml());
        let config: Config = toml::from_str(&toml_text).unwrap();
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("go.path"));
    }
}
