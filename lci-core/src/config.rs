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

/// Query-time switches for the retrieval channels and the reranker, so a
/// search can run as a controlled ablation (for example the grep-only
/// baseline: `semantic = false`, `lsp = false`, `reranker = false`). They are
/// production configuration, not evaluation data.
///
/// A disabled channel is skipped entirely: it contributes no candidates,
/// does no work (`semantic = false` never embeds the query), reports a zero
/// timing, and raises no "unavailable" warning, because turning it off is a
/// choice rather than a failure. A disabled reranker returns the fusion order
/// with `reranked = false`, sends no request, and warns about nothing.
///
/// The switches affect searches only. Indexing always embeds and stores every
/// chunk, so the index stays valid when a switch is flipped back on, and the
/// readiness report (`/ready`, `service_status`) still probes the embedding
/// service even with `semantic = false`, because indexing needs it. At least
/// one of `semantic`, `lexical`, and `lsp` must stay enabled; validation
/// rejects a configuration that disables all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChannelsConfig {
    /// Embed the query and search the vector index.
    pub semantic: bool,
    /// Search with ripgrep.
    pub lexical: bool,
    /// Ask the enabled language servers for workspace symbols.
    pub lsp: bool,
    /// Rerank the fused shortlist with the reranking service.
    pub reranker: bool,
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            semantic: true,
            lexical: true,
            lsp: true,
            reranker: true,
        }
    }
}

/// Per-channel weights for Reciprocal Rank Fusion.
///
/// Each channel contributes `weight / (rrf_k + rank)` for a chunk it returned,
/// and a chunk several channels returned sums those contributions. The
/// source-role prior is added afterwards and is never scaled by a weight.
/// `semantic_weight` defaults to `2.0` and the other two to `1.0`; setting all
/// three to `1.0` (`FusionConfig::UNIT`) is plain unweighted RRF. `rrf_k` stays
/// a top-level key.
///
/// A weight must be finite and greater than zero. Turning a channel off is the
/// job of `[channels]` (which also skips the channel's work); a zero weight
/// would only hide it from the score while still paying for it, so validation
/// rejects it.
///
/// The weights are visible only through `fusion_score`; channel ranks, scores,
/// and the reranker's input are unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FusionConfig {
    /// Weight of the semantic (vector) channel's reciprocal-rank contribution.
    pub semantic_weight: f32,
    /// Weight of the lexical (ripgrep) channel's reciprocal-rank contribution.
    pub lexical_weight: f32,
    /// Weight of the language-server channel's reciprocal-rank contribution.
    pub lsp_weight: f32,
}

impl FusionConfig {
    /// Plain unweighted RRF: every channel weighs `1.0`.
    pub const UNIT: Self = Self {
        semantic_weight: 1.0,
        lexical_weight: 1.0,
        lsp_weight: 1.0,
    };
}

impl Default for FusionConfig {
    /// `semantic_weight = 2.0` (roadmap F2a: fusion-only mined-set MRR +0.059,
    /// CI [+0.012, +0.118], held-out halves agree, core ranks unchanged,
    /// reranked confirm n.s.); lexical and LSP stay at `1.0`.
    fn default() -> Self {
        Self {
            semantic_weight: 2.0,
            ..Self::UNIT
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
    /// Upper bound, in bytes, on the code sent to the reranker per
    /// candidate (the path is always sent in full). Keeps every
    /// query/document pair inside the reranker's context window -- an
    /// oversized chunk otherwise makes the whole rerank request fail -- and
    /// bounds reranking cost. Chunks are cut at a UTF-8 boundary, keeping
    /// their head (signature, doc comment, opening body). Indexing, lexical
    /// search, and returned results always use the full chunk.
    pub rerank_document_max_bytes: usize,
    pub rrf_k: f32,
    pub watch_poll_milliseconds: u64,
    pub watch_debounce_milliseconds: u64,
    pub rust_analyzer_path: String,
    pub lsp_timeout_seconds: u64,
    pub lsp_candidate_count: usize,
    /// Most milliseconds one search waits for the language-server channel.
    /// A server that is still starting (rust-analyzer needs ~10 s to index
    /// a large workspace) or slow to answer costs a search at most this long;
    /// the search then returns without symbol candidates and notes it, and the
    /// server keeps starting in the background for later searches. Separate
    /// from `lsp_timeout_seconds`, which bounds one request to a hung server.
    pub lsp_search_budget_ms: u64,
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
    /// Optional Java language-server (jdtls) settings. Absent or empty
    /// configuration keeps Java navigation disabled; syntax retrieval is
    /// unaffected.
    pub java: JavaLspConfig,
    /// Optional C/C++ language-server (clangd) settings. Absent or empty
    /// configuration keeps C/C++ navigation disabled; syntax retrieval is
    /// unaffected.
    pub clangd: ClangdLspConfig,
    pub index: IndexConfig,
    /// Query-time channel and reranker switches; all enabled by default.
    pub channels: ChannelsConfig,
    /// Per-channel Reciprocal Rank Fusion weights; `semantic_weight = 2.0`,
    /// `lexical_weight = lsp_weight = 1.0` by default (all `1.0` is plain RRF).
    pub fusion: FusionConfig,
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

/// Optional Java language-server (jdtls / Eclipse JDT Language Server)
/// configuration.
///
/// Java navigation is optional and fail-open: an absent or empty `[java]`
/// section keeps the server disabled without affecting syntax retrieval.
/// The shared `lsp_timeout_seconds` and `lsp_candidate_count` settings
/// apply to this server as well.
///
/// `path` is the jdtls *installation directory* (e.g. the extracted
/// `jdt-language-server-*.tar.gz`), not an executable -- unlike every other
/// adapter here, jdtls has no single native binary to spawn directly.
/// `JavaServer` builds the real `java -jar <equinox launcher> -data <dir>`
/// invocation itself (globbing `<path>/plugins/org.eclipse.equinox.
/// launcher_*.jar` and selecting `<path>/config_win`/`config_mac`/
/// `config_linux`), spawning `java` as a direct child.
///
/// This was **not** the first design: jdtls's own official cross-platform
/// launcher is a Python script (`<path>/bin/jdtls`) that does the same
/// jar/config-dir resolution, and reusing it was tried first. On Windows
/// that produces a `local-code-intelligence.exe -> python.exe -> java.exe`
/// process chain, and killing the direct child (`python.exe`, via
/// `kill_on_drop`) does not cascade to the `java.exe` grandchild -- Windows
/// only cascades process termination through a Job Object, which plain
/// `std`/`tokio` process spawning does not set up. The orphaned `java.exe`
/// then keeps its inherited stdout pipe handle open, so the next read on
/// that pipe blocks forever with neither data nor EOF, hanging the whole
/// calling process indefinitely -- confirmed by reproducing the hang, then
/// confirming a direct `java` invocation (no Python hop) exits cleanly
/// within seconds of being killed, with zero orphaned processes. Spawning
/// `java` directly, like every other adapter here, avoids the multi-hop
/// process tree entirely rather than trying to fix Windows job-object
/// semantics.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JavaLspConfig {
    /// Path to the jdtls installation directory. `None` (the default) keeps
    /// Java navigation disabled. An empty or whitespace-only value is
    /// rejected by validation rather than silently counting as enabled.
    pub path: Option<String>,
    /// Additional JVM/jdtls arguments, appended after the `-data <dir>`
    /// flag this application always supplies.
    pub args: Vec<String>,
    /// Explicitly disable the Java server even when a path is configured.
    pub disabled: bool,
}

impl JavaLspConfig {
    /// Whether the Java language server is enabled.
    ///
    /// Enabled only when not explicitly disabled and a nonempty interpreter
    /// path is configured.
    pub fn enabled(&self) -> bool {
        !self.disabled
            && self
                .path
                .as_deref()
                .is_some_and(|path| !path.trim().is_empty())
    }
}

/// Optional C/C++ language-server (clangd) configuration.
///
/// One configured clangd instance navigates both Tree-sitter-indexed
/// languages `c` and `cpp` (`.c`, `.cpp`, `.hpp`), the same one-server-many-
/// languages shape `TypeScriptLspConfig` uses for its four ECMAScript-family
/// languages. C/C++ navigation is optional and fail-open: an absent or empty
/// `[clangd]` section keeps the server disabled without affecting syntax
/// retrieval. The shared `lsp_timeout_seconds` and `lsp_candidate_count`
/// settings apply to this server as well.
///
/// Like gopls, clangd needs no forced leading transport flag -- stdio is its
/// default communication mode (confirmed against a real installed clangd,
/// not assumed). It also needs no `compile_commands.json` for LCI's simple
/// per-file navigation use case: confirmed live against a small standalone
/// fixture with no build system present, clangd's fallback compilation
/// database is sufficient for `workspace/symbol`/`definition`/`references`
/// to resolve correctly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ClangdLspConfig {
    /// Path to the standalone clangd executable. `None` (the default) keeps
    /// C/C++ navigation disabled. An empty or whitespace-only value is
    /// rejected by validation rather than silently counting as enabled.
    pub path: Option<String>,
    /// Additional arguments passed to clangd (e.g. `--compile-commands-dir`
    /// for a workspace with a real build system).
    pub args: Vec<String>,
    /// Explicitly disable the clangd server even when a path is configured.
    pub disabled: bool,
}

impl ClangdLspConfig {
    /// Whether the clangd language server is enabled.
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
            rerank_document_max_bytes: 4096,
            rrf_k: 60.0,
            watch_poll_milliseconds: 2000,
            watch_debounce_milliseconds: 750,
            rust_analyzer_path: "rust-analyzer".into(),
            lsp_timeout_seconds: 60,
            lsp_candidate_count: 40,
            lsp_search_budget_ms: 1500,
            csharp: CSharpLspConfig::default(),
            typescript: TypeScriptLspConfig::default(),
            python: PythonLspConfig::default(),
            go: GoLspConfig::default(),
            java: JavaLspConfig::default(),
            clangd: ClangdLspConfig::default(),
            index: IndexConfig::default(),
            channels: ChannelsConfig::default(),
            fusion: FusionConfig::default(),
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
        for (name, weight) in [
            ("semantic_weight", self.fusion.semantic_weight),
            ("lexical_weight", self.fusion.lexical_weight),
            ("lsp_weight", self.fusion.lsp_weight),
        ] {
            ensure!(
                weight.is_finite() && weight > 0.0,
                "fusion.{name} must be a finite number greater than zero (got {weight}); to turn a channel off use [channels], not a zero weight"
            );
        }
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
        ensure!(
            (100..=60_000).contains(&self.lsp_search_budget_ms),
            "lsp_search_budget_ms must be 100..=60000"
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
        if !self.java.disabled
            && let Some(path) = &self.java.path
        {
            ensure!(
                !path.trim().is_empty(),
                "java.path must be nonempty when set; omit it or set java.disabled = true to keep Java navigation disabled"
            );
        }
        if !self.clangd.disabled
            && let Some(path) = &self.clangd.path
        {
            ensure!(
                !path.trim().is_empty(),
                "clangd.path must be nonempty when set; omit it or set clangd.disabled = true to keep C/C++ navigation disabled"
            );
        }
        ensure!(
            self.channels.semantic || self.channels.lexical || self.channels.lsp,
            "channels: at least one of semantic, lexical, and lsp must be enabled; a search with no retrieval channel can return nothing"
        );
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
        ensure!(
            (256..=1_048_576).contains(&self.rerank_document_max_bytes),
            "rerank_document_max_bytes must be 256..=1048576"
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
    fn lsp_search_budget_defaults_to_one_and_a_half_seconds_and_round_trips() {
        // A file without the key keeps the default.
        let config: Config = toml::from_str(&base_toml()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.lsp_search_budget_ms, 1500);
        assert_eq!(Config::default().lsp_search_budget_ms, 1500);

        let configured: Config =
            toml::from_str(&format!("{}lsp_search_budget_ms = 4000\n", base_toml())).unwrap();
        configured.validate().unwrap();
        assert_eq!(configured.lsp_search_budget_ms, 4000);

        let reloaded: Config = toml::from_str(&toml::to_string(&configured).unwrap()).unwrap();
        reloaded.validate().unwrap();
        assert_eq!(reloaded.lsp_search_budget_ms, 4000);
    }

    #[test]
    fn lsp_search_budget_outside_its_range_is_rejected() {
        for budget in [0, 99, 60_001] {
            let config = Config {
                data_dir: std::env::temp_dir(),
                lsp_search_budget_ms: budget,
                ..Config::default()
            };
            let error = config.validate().unwrap_err();
            assert!(
                error.to_string().contains("lsp_search_budget_ms"),
                "{budget}: {error}"
            );
        }
        for budget in [100, 1500, 60_000] {
            let config = Config {
                data_dir: std::env::temp_dir(),
                lsp_search_budget_ms: budget,
                ..Config::default()
            };
            config.validate().unwrap();
        }
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

    #[test]
    fn default_java_is_disabled() {
        let config = Config::default();
        config.validate().unwrap();
        assert!(!config.java.enabled());
        assert!(config.java.path.is_none());
    }

    #[test]
    fn java_section_enables_server() {
        let toml_text = format!(
            "{}[java]\npath = \"C:/tools/jdtls\"\nargs = ['-Xmx2G']\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        assert!(config.java.enabled());
        assert_eq!(config.java.path.as_deref(), Some("C:/tools/jdtls"));
        assert_eq!(config.java.args, vec!["-Xmx2G".to_string()]);
    }

    #[test]
    fn java_disabled_flag_overrides_path() {
        let toml_text = format!(
            "{}[java]\npath = \"C:/tools/jdtls\"\ndisabled = true\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        assert!(!config.java.enabled());
    }

    #[test]
    fn empty_java_path_is_rejected() {
        let toml_text = format!("{}[java]\npath = \"   \"\n", base_toml());
        let config: Config = toml::from_str(&toml_text).unwrap();
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("java.path"));
    }

    #[test]
    fn default_clangd_is_disabled() {
        let config = Config::default();
        config.validate().unwrap();
        assert!(!config.clangd.enabled());
        assert!(config.clangd.path.is_none());
    }

    #[test]
    fn clangd_section_enables_server() {
        let toml_text = format!("{}[clangd]\npath = \"clangd\"\n", base_toml());
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        assert!(config.clangd.enabled());
        assert_eq!(config.clangd.path.as_deref(), Some("clangd"));
    }

    #[test]
    fn clangd_disabled_flag_overrides_path() {
        let toml_text = format!(
            "{}[clangd]\npath = \"clangd\"\ndisabled = true\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        assert!(!config.clangd.enabled());
    }

    #[test]
    fn empty_clangd_path_is_rejected() {
        let toml_text = format!("{}[clangd]\npath = \"   \"\n", base_toml());
        let config: Config = toml::from_str(&toml_text).unwrap();
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("clangd.path"));
    }

    #[test]
    fn default_channels_are_all_enabled() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(
            config.channels,
            ChannelsConfig {
                semantic: true,
                lexical: true,
                lsp: true,
                reranker: true,
            }
        );
        assert_eq!(ChannelsConfig::default(), config.channels);
    }

    #[test]
    fn config_without_a_channels_section_keeps_every_channel_enabled() {
        // Existing configuration files predate `[channels]` and must load
        // unchanged, with the behavior they always had.
        let config: Config = toml::from_str(&base_toml()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.channels, ChannelsConfig::default());
    }

    #[test]
    fn channels_section_overrides_only_the_named_switches() {
        let toml_text = format!("{}[channels]\nreranker = false\n", base_toml());
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        assert!(config.channels.semantic);
        assert!(config.channels.lexical);
        assert!(config.channels.lsp);
        assert!(!config.channels.reranker);
    }

    #[test]
    fn grep_only_arm_is_a_valid_configuration() {
        let toml_text = format!(
            "{}[channels]\nsemantic = false\nlsp = false\nreranker = false\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.channels,
            ChannelsConfig {
                semantic: false,
                lexical: true,
                lsp: false,
                reranker: false,
            }
        );
    }

    #[test]
    fn channels_round_trip_through_toml() {
        let config = Config {
            data_dir: std::env::temp_dir(),
            channels: ChannelsConfig {
                semantic: false,
                lexical: true,
                lsp: false,
                reranker: false,
            },
            ..Config::default()
        };
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("[channels]"), "{text}");
        let reloaded: Config = toml::from_str(&text).unwrap();
        reloaded.validate().unwrap();
        assert_eq!(reloaded.channels, config.channels);
    }

    #[test]
    fn disabling_every_retrieval_channel_is_rejected() {
        // With semantic, lexical, and lsp all off no search can find
        // anything; the reranker alone is not a retrieval channel.
        let toml_text = format!(
            "{}[channels]\nsemantic = false\nlexical = false\nlsp = false\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        let error = config.validate().unwrap_err();
        assert!(error.to_string().contains("channels"), "{error}");

        // Any single retrieval channel is enough.
        for (semantic, lexical, lsp) in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
        ] {
            let config = Config {
                data_dir: std::env::temp_dir(),
                channels: ChannelsConfig {
                    semantic,
                    lexical,
                    lsp,
                    reranker: false,
                },
                ..Config::default()
            };
            config.validate().unwrap();
        }
    }

    #[test]
    fn unknown_channels_field_is_rejected() {
        // `deny_unknown_fields` catches a misspelled switch instead of
        // silently leaving the channel enabled.
        let toml_text = format!("{}[channels]\nsemantics = false\n", base_toml());
        assert!(toml::from_str::<Config>(&toml_text).is_err());
    }

    #[test]
    fn fusion_defaults_to_semantic_weight_two() {
        let expected = FusionConfig {
            semantic_weight: 2.0,
            lexical_weight: 1.0,
            lsp_weight: 1.0,
        };
        // The default, a file without the section, and an empty section all
        // mean semantic_weight 2.0 with lexical and LSP at 1.0.
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(config.fusion, expected);
        assert_eq!(FusionConfig::default(), expected);
        let absent: Config = toml::from_str(&base_toml()).unwrap();
        absent.validate().unwrap();
        assert_eq!(absent.fusion, expected);
        let empty: Config = toml::from_str(&format!("{}[fusion]\n", base_toml())).unwrap();
        empty.validate().unwrap();
        assert_eq!(empty.fusion, expected);

        // Plain unweighted RRF is one explicit line away, and is not the
        // default.
        assert_ne!(FusionConfig::default(), FusionConfig::UNIT);
        let plain: Config =
            toml::from_str(&format!("{}[fusion]\nsemantic_weight = 1.0\n", base_toml())).unwrap();
        plain.validate().unwrap();
        assert_eq!(plain.fusion, FusionConfig::UNIT);

        // A section overrides only the weights it names; the rest keep their
        // defaults, including the semantic weight of 2.0.
        let partial: Config =
            toml::from_str(&format!("{}[fusion]\nsemantic_weight = 1.5\n", base_toml())).unwrap();
        partial.validate().unwrap();
        assert_eq!(
            partial.fusion,
            FusionConfig {
                semantic_weight: 1.5,
                ..FusionConfig::UNIT
            }
        );
        let lexical_only: Config =
            toml::from_str(&format!("{}[fusion]\nlexical_weight = 3.0\n", base_toml())).unwrap();
        lexical_only.validate().unwrap();
        assert_eq!(
            lexical_only.fusion,
            FusionConfig {
                lexical_weight: 3.0,
                ..expected
            }
        );

        // The weights round-trip through TOML.
        let reloaded: Config = toml::from_str(&toml::to_string(&partial).unwrap()).unwrap();
        reloaded.validate().unwrap();
        assert_eq!(reloaded.fusion, partial.fusion);

        // `deny_unknown_fields` catches a misspelled weight.
        let misspelled = format!("{}[fusion]\nsemantics_weight = 2.0\n", base_toml());
        assert!(toml::from_str::<Config>(&misspelled).is_err());
    }

    #[test]
    fn fusion_rejects_zero_or_nan_weight() {
        for (key, setter) in [
            (
                "semantic_weight",
                (|f: &mut FusionConfig, w: f32| f.semantic_weight = w)
                    as fn(&mut FusionConfig, f32),
            ),
            ("lexical_weight", |f, w| f.lexical_weight = w),
            ("lsp_weight", |f, w| f.lsp_weight = w),
        ] {
            for bad in [0.0, -0.0, -1.0, f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
                let mut config = Config {
                    data_dir: std::env::temp_dir(),
                    ..Config::default()
                };
                setter(&mut config.fusion, bad);
                let error = config.validate().unwrap_err().to_string();
                assert!(
                    error.contains(&format!("fusion.{key}")),
                    "{key}={bad}: {error}"
                );
                assert!(error.contains("[channels]"), "{key}={bad}: {error}");
            }
            // A small positive and a large finite weight are both fine.
            for good in [f32::MIN_POSITIVE, 0.5, 1.0, 2.0, 1000.0] {
                let mut config = Config {
                    data_dir: std::env::temp_dir(),
                    ..Config::default()
                };
                setter(&mut config.fusion, good);
                config.validate().unwrap();
            }
        }

        // The check runs at load time, from the file.
        for value in ["0", "0.0", "-2.0", "nan", "inf"] {
            let toml_text = format!("{}[fusion]\nlexical_weight = {value}\n", base_toml());
            let config: Config = toml::from_str(&toml_text).unwrap();
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("fusion.lexical_weight"), "{value}: {error}");
        }
    }
}
