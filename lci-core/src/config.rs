use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub mod layers;

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

/// One named set of query-time overrides, `[search_profiles.<name>]`, so a
/// single process can serve the default search and, next to it, a faster or a
/// deeper one chosen per request.
///
/// A profile overrides only the switches below; every key it leaves out keeps
/// the top-level value (`[channels]`, `rerank_candidate_count`). Nothing that
/// changes the index, the embedding identity, fusion, the candidate depths of
/// the channels, the per-file cap, or `rerank_document_max_bytes` can be
/// overridden, so every profile reads the same index and differs only in
/// which channels run and how many fused candidates the reranker orders. A
/// search that names no profile uses the top-level values unchanged.
///
/// Like the top-level values, each profile must leave at least one of
/// `semantic`, `lexical`, and `lsp` enabled and keep `rerank_candidate_count`
/// in `1..=200`; validation checks the resolved settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SearchProfile {
    /// Override of `[channels].semantic`.
    pub semantic: Option<bool>,
    /// Override of `[channels].lexical`.
    pub lexical: Option<bool>,
    /// Override of `[channels].lsp`.
    pub lsp: Option<bool>,
    /// Override of `[channels].reranker`.
    pub reranker: Option<bool>,
    /// Override of the top-level `rerank_candidate_count`.
    pub rerank_candidate_count: Option<usize>,
}

/// The query-time settings one search runs with: the top-level values, or
/// those with a [`SearchProfile`]'s overrides applied. Resolved by
/// [`Config::search_settings`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchSettings {
    /// The profile these settings came from; `None` for the top-level values.
    pub profile: Option<String>,
    /// Which retrieval channels and the reranker run.
    pub channels: ChannelsConfig,
    /// How many fused candidates the reranker orders (and a traced search
    /// marks as shortlisted).
    pub rerank_candidate_count: usize,
}

/// Longest accepted search profile name, in characters.
pub const SEARCH_PROFILE_NAME_MAX_LEN: usize = 32;

/// Whether `name` is usable as a search profile name: 1 to
/// [`SEARCH_PROFILE_NAME_MAX_LEN`] ASCII letters, digits, `_` and `-`,
/// starting with a letter or digit, so it can stand in a URL path segment
/// unescaped. `default` is reserved for the unprofiled search.
fn is_valid_search_profile_name(name: &str) -> bool {
    let mut characters = name.chars();
    characters
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && name.len() <= SEARCH_PROFILE_NAME_MAX_LEN
        && characters.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && name != "default"
}

/// Per-channel weights for Reciprocal Rank Fusion.
///
/// Each channel contributes `weight / (rrf_k + rank)` for a chunk it returned,
/// and a chunk several channels returned sums those contributions. The
/// source-role prior is added afterwards and is never scaled by a weight.
/// All three weights default to `1.0`, which is plain unweighted RRF;
/// `FusionConfig::UNIT` is those weights with no per-file cap. `rrf_k` stays a
/// top-level key.
///
/// A weight must be finite and greater than zero. Turning a channel off is the
/// job of `[channels]` (which also skips the channel's work); a zero weight
/// would only hide it from the score while still paying for it, so validation
/// rejects it.
///
/// The weights are visible only through `fusion_score`; channel ranks, scores,
/// and the reranker's input are unchanged.
///
/// `max_chunks_per_file` is the one non-weight setting: an optional per-file
/// cap on the fused ordering (see the field).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FusionConfig {
    /// Weight of the semantic (vector) channel's reciprocal-rank contribution.
    pub semantic_weight: f32,
    /// Weight of the lexical (ripgrep) channel's reciprocal-rank contribution.
    pub lexical_weight: f32,
    /// Weight of the language-server channel's reciprocal-rank contribution.
    pub lsp_weight: f32,
    /// Most chunks of one file that keep their place in the fused order before
    /// the rest of that file's chunks are demoted behind every other chunk
    /// (`0..=100`, `0` = unlimited; default `2`, roadmap F2b: fusion-only
    /// nDCG@10 +0.049 on the mined set, significant in both halves, and +0.168
    /// on core, significant; reranked mined nDCG@10 +0.046, significant, MRR
    /// -0.023, not significant). It runs after fusion
    /// ordering and before the list is cut to `rerank_candidate_count`, so the
    /// reranker's shortlist and the reranker-off and reranker-failure results
    /// fill with distinct files first, while `top_k` is still filled from the
    /// demoted chunks when there are too few distinct files. Demotion is
    /// stable and never removes a chunk. `fusion_score` is not rescaled, so a
    /// demoted chunk can appear after a chunk with a lower `fusion_score`. The
    /// reranker's own output order is not capped again.
    pub max_chunks_per_file: u32,
}

impl FusionConfig {
    /// Largest accepted `max_chunks_per_file`.
    pub const MAX_CHUNKS_PER_FILE_LIMIT: u32 = 100;

    /// Plain unweighted RRF: every channel weighs `1.0`, and no per-file cap
    /// (`max_chunks_per_file = 0`). This is not the default: the default has
    /// the same weights but caps a file at two chunks.
    pub const UNIT: Self = Self {
        semantic_weight: 1.0,
        lexical_weight: 1.0,
        lsp_weight: 1.0,
        max_chunks_per_file: 0,
    };
}

impl Default for FusionConfig {
    /// Plain RRF weights (all `1.0`) with a two-chunk file cap
    /// (`max_chunks_per_file = 2`, roadmap F2b, see the field). A semantic
    /// weight of `2.0` was the default for a while (roadmap F2a); the flip was
    /// reverted after an exact offline replay with the LSP channel present
    /// found no significant effect, so plain RRF is the simpler default.
    fn default() -> Self {
        Self {
            max_chunks_per_file: 2,
            ..Self::UNIT
        }
    }
}

/// Default for [`Config::port`]: the loopback port the service listens on.
pub const DEFAULT_PORT: u16 = 8768;

/// Ports that belong to other services and that LCI itself never listens on:
/// 8765 is the external generation model, 8766 the embedding service, 8767
/// the reranker. [`Config::validate`] rejects them as [`Config::port`].
pub const RESERVED_PORTS: std::ops::RangeInclusive<u16> = 8765..=8767;

/// Default for [`Config::embedding_query_template`]: the instruction
/// Qwen3-Embedding is trained with, then the query. It is
/// `models::QUERY_INSTRUCTION` followed by `{query}` (a test pins that), so the
/// default query text is byte-identical to the former hard-coded one.
pub const DEFAULT_EMBEDDING_QUERY_TEMPLATE: &str = "Instruct: Given a code search query, retrieve relevant code passages that answer the query\nQuery: {query}";

/// Default for [`Config::embedding_document_template`]: the chunk's code as is.
pub const DEFAULT_EMBEDDING_DOCUMENT_TEMPLATE: &str = "{code}";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub embedding_url: String,
    pub embedding_model: String,
    pub reranker_url: String,
    pub reranker_model: String,
    pub data_dir: PathBuf,
    /// Loopback port `serve` binds (`127.0.0.1:<port>`) and the one the URLs
    /// `lci config` derives name. `serve --port` overrides it. Never one of
    /// [`RESERVED_PORTS`].
    pub port: u16,
    pub default_top_k: usize,
    pub embedding_batch_size: usize,
    /// Text embedded for a search query, with `{query}` replaced by the query
    /// (required). Embedding models differ in the prompt format they expect
    /// (an instruction prefix, a task label), so this is configurable.
    /// Query-side only: changing it never invalidates an existing index.
    pub embedding_query_template: String,
    /// Text embedded for each indexed chunk, with `{code}` replaced by the
    /// chunk's source (required) and `{path}` by its repository-relative file
    /// path (optional). The default embeds the bare code. Changing it
    /// changes `embedding_identity`, so every chunk is re-embedded on the next
    /// index or search. Without `{path}` a vector depends only on the chunk's
    /// code, and identical code is embedded once and reused wherever it
    /// appears. With `{path}` it depends on the file too, so identical code in
    /// different files, or a file moved to a new path, is embedded per path.
    pub embedding_document_template: String,
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
    /// Seconds a language-server session may go unused before its server is
    /// shut down to free its memory (default `1800`, 30 minutes; `0` keeps
    /// every server until the service stops). A server that is still
    /// priming or serving a call is never released. The next search or
    /// navigation call for that workspace starts a fresh server, so a
    /// released server costs one cold start (and re-priming) later. Values
    /// below 60 are meant for tests.
    pub lsp_idle_timeout_seconds: u64,
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
    /// Per-channel Reciprocal Rank Fusion weights; all `1.0` (plain RRF) by
    /// default. Also `max_chunks_per_file` (default `2`; `0` is unlimited).
    pub fusion: FusionConfig,
    /// Named query-time override sets, `[search_profiles.<name>]`, chosen per
    /// request (see [`SearchProfile`]). None by default; a search that names
    /// no profile ignores them all.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub search_profiles: BTreeMap<String, SearchProfile>,
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
            port: DEFAULT_PORT,
            default_top_k: 8,
            embedding_batch_size: 8,
            embedding_query_template: DEFAULT_EMBEDDING_QUERY_TEMPLATE.into(),
            embedding_document_template: DEFAULT_EMBEDDING_DOCUMENT_TEMPLATE.into(),
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
            lsp_idle_timeout_seconds: 1800,
            csharp: CSharpLspConfig::default(),
            typescript: TypeScriptLspConfig::default(),
            python: PythonLspConfig::default(),
            go: GoLspConfig::default(),
            java: JavaLspConfig::default(),
            clangd: ClangdLspConfig::default(),
            index: IndexConfig::default(),
            channels: ChannelsConfig::default(),
            fusion: FusionConfig::default(),
            search_profiles: BTreeMap::new(),
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
            !RESERVED_PORTS.contains(&self.port),
            "port {} is reserved for another service (8765 generation model, 8766 embedding service, 8767 reranker); choose another port",
            self.port
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
            self.fusion.max_chunks_per_file <= FusionConfig::MAX_CHUNKS_PER_FILE_LIMIT,
            "fusion.max_chunks_per_file must be 0..={} (got {}); 0 means unlimited",
            FusionConfig::MAX_CHUNKS_PER_FILE_LIMIT,
            self.fusion.max_chunks_per_file
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
        for (name, profile) in &self.search_profiles {
            ensure!(
                is_valid_search_profile_name(name),
                "search_profiles.{name}: a profile name is 1 to {SEARCH_PROFILE_NAME_MAX_LEN} letters, digits, '_' or '-' starting with a letter or digit, and cannot be \"default\" (the unprofiled search)"
            );
            let resolved = self.resolve_search_profile(name, profile);
            ensure!(
                resolved.channels.semantic || resolved.channels.lexical || resolved.channels.lsp,
                "search_profiles.{name}: at least one of semantic, lexical, and lsp must be enabled once the profile is applied to [channels]; a search with no retrieval channel can return nothing"
            );
            ensure!(
                (1..=200).contains(&resolved.rerank_candidate_count),
                "search_profiles.{name}: rerank_candidate_count must be 1..=200 (got {})",
                resolved.rerank_candidate_count
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
            self.embedding_query_template.contains("{query}"),
            "embedding_query_template must contain the {{query}} placeholder (got {:?})",
            self.embedding_query_template
        );
        ensure!(
            self.embedding_document_template.contains("{code}"),
            "embedding_document_template must contain the {{code}} placeholder (got {:?})",
            self.embedding_document_template
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

    /// The query-time settings a search runs with when it names no profile:
    /// `[channels]` and `rerank_candidate_count` as configured.
    pub fn base_search_settings(&self) -> SearchSettings {
        SearchSettings {
            profile: None,
            channels: self.channels,
            rerank_candidate_count: self.rerank_candidate_count,
        }
    }

    /// The query-time settings for `profile`: the top-level values with that
    /// `[search_profiles.<name>]`'s overrides applied. `None` is the
    /// unprofiled search ([`Self::base_search_settings`]). A name that is not
    /// configured is an error listing the configured names, never a silent
    /// fall back to the defaults.
    pub fn search_settings(&self, profile: Option<&str>) -> Result<SearchSettings> {
        let Some(name) = profile else {
            return Ok(self.base_search_settings());
        };
        let Some(overrides) = self.search_profiles.get(name) else {
            if self.search_profiles.is_empty() {
                bail!("unknown search profile {name:?}; no search profiles are configured");
            }
            let configured: Vec<&str> = self.search_profiles.keys().map(String::as_str).collect();
            bail!(
                "unknown search profile {name:?}; configured: {}",
                configured.join(", ")
            );
        };
        Ok(self.resolve_search_profile(name, overrides))
    }

    /// `overrides` applied over the top-level settings, under `name`.
    fn resolve_search_profile(&self, name: &str, overrides: &SearchProfile) -> SearchSettings {
        SearchSettings {
            profile: Some(name.to_owned()),
            channels: ChannelsConfig {
                semantic: overrides.semantic.unwrap_or(self.channels.semantic),
                lexical: overrides.lexical.unwrap_or(self.channels.lexical),
                lsp: overrides.lsp.unwrap_or(self.channels.lsp),
                reranker: overrides.reranker.unwrap_or(self.channels.reranker),
            },
            rerank_candidate_count: overrides
                .rerank_candidate_count
                .unwrap_or(self.rerank_candidate_count),
        }
    }

    /// What the stored vectors were computed with; an index whose identity
    /// differs from this one is incompatible and is re-embedded. It names the
    /// embedding service, model, and chunk format. A non-default
    /// `embedding_document_template` appends `|doc=<hash>` (a stable 64-bit
    /// FNV-1a of the template, 16 lowercase hex digits), because it changes
    /// the embedded text; the default template adds nothing, so indexes built
    /// before the template existed stay valid. `embedding_query_template` is
    /// not part of it: it only shapes the query, never a stored vector.
    pub fn embedding_identity(&self) -> String {
        let identity = format!(
            "{}|{}|{}",
            self.embedding_url.trim_end_matches('/'),
            self.embedding_model,
            crate::chunk::DOCUMENT_FORMAT_VERSION
        );
        if self.embedding_document_template == DEFAULT_EMBEDDING_DOCUMENT_TEMPLATE {
            identity
        } else {
            format!(
                "{identity}|doc={:016x}",
                fnv1a_64(self.embedding_document_template.as_bytes())
            )
        }
    }
}

/// Fills the `{name}` placeholders of `template` from `values` in one
/// left-to-right pass.
///
/// A `{name}` whose name is a key of `values` is replaced by the paired value.
/// Substituted text is never scanned again, so a value that itself contains
/// `{path}` (source code that mentions the placeholder) stays literal. All
/// other text is copied unchanged: an unknown name, an unclosed `{`, a stray
/// `}`. With nested braces such as `{{code}}` the inner `{code}` is
/// substituted and the outer braces are kept.
pub fn fill_template(template: &str, values: &[(&str, &str)]) -> String {
    let values_len: usize = values.iter().map(|&(_, value)| value.len()).sum();
    let mut out = String::with_capacity(template.len() + values_len);
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        let filled = after.find('}').and_then(|close| {
            let name = &after[..close];
            let (_, value) = values.iter().find(|&&(key, _)| key == name)?;
            Some((close, *value))
        });
        match filled {
            Some((close, value)) => {
                out.push_str(value);
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// 64-bit FNV-1a. Used instead of a `std` hasher because the result is stored
/// in index metadata and must be identical across runs, platforms, and
/// compiler releases.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
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
    fn lsp_idle_timeout_defaults_to_thirty_minutes_allows_zero_and_round_trips() {
        // A file without the key keeps the default.
        let config: Config = toml::from_str(&base_toml()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.lsp_idle_timeout_seconds, 1800);
        assert_eq!(Config::default().lsp_idle_timeout_seconds, 1800);

        // 0 means never release; it is a valid setting, not an error.
        for seconds in [0, 1, 600] {
            let configured: Config = toml::from_str(&format!(
                "{}lsp_idle_timeout_seconds = {seconds}\n",
                base_toml()
            ))
            .unwrap();
            configured.validate().unwrap();
            assert_eq!(configured.lsp_idle_timeout_seconds, seconds);

            let reloaded: Config = toml::from_str(&toml::to_string(&configured).unwrap()).unwrap();
            reloaded.validate().unwrap();
            assert_eq!(reloaded.lsp_idle_timeout_seconds, seconds);
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
    fn port_defaults_to_8768_loads_from_a_file_and_round_trips() {
        assert_eq!(Config::default().port, DEFAULT_PORT);
        assert_eq!(DEFAULT_PORT, 8768);
        // A file without the key keeps the default.
        let absent: Config = toml::from_str(&base_toml()).unwrap();
        absent.validate().unwrap();
        assert_eq!(absent.port, 8768);

        let configured: Config = toml::from_str(&format!("{}port = 9100\n", base_toml())).unwrap();
        configured.validate().unwrap();
        assert_eq!(configured.port, 9100);
        let text = toml::to_string(&configured).unwrap();
        assert!(text.contains("port = 9100"), "{text}");
        let reloaded: Config = toml::from_str(&text).unwrap();
        reloaded.validate().unwrap();
        assert_eq!(reloaded.port, 9100);
    }

    #[test]
    fn the_ports_of_the_other_services_are_rejected_as_the_listening_port() {
        for port in [8765, 8766, 8767] {
            let config = Config {
                data_dir: std::env::temp_dir(),
                port,
                ..Config::default()
            };
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(&port.to_string()), "{error}");
            assert!(error.contains("reserved"), "{error}");
            let from_file: Config =
                toml::from_str(&format!("{}port = {port}\n", base_toml())).unwrap();
            assert!(from_file.validate().is_err(), "{port}");
        }
        for port in [1, 8764, 8768, 8769, 65_535] {
            let config = Config {
                data_dir: std::env::temp_dir(),
                port,
                ..Config::default()
            };
            config.validate().unwrap();
        }
        // The key is a port number, so a value outside u16 does not load.
        assert!(toml::from_str::<Config>(&format!("{}port = 70000\n", base_toml())).is_err());
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

    /// A configuration with `[search_profiles.fast]` and `[search_profiles.deep]`
    /// next to a non-default top-level `rerank_candidate_count`.
    fn profiled_toml() -> String {
        format!(
            "{}rerank_candidate_count = 10\n\
             [search_profiles.fast]\nreranker = false\nrerank_candidate_count = 5\n\
             [search_profiles.deep]\nlsp = false\nrerank_candidate_count = 40\n",
            base_toml()
        )
    }

    #[test]
    fn no_search_profiles_are_configured_by_default_and_the_base_settings_are_the_top_level_ones() {
        let config = Config::default();
        config.validate().unwrap();
        assert!(config.search_profiles.is_empty());
        let absent: Config = toml::from_str(&base_toml()).unwrap();
        absent.validate().unwrap();
        assert!(absent.search_profiles.is_empty());

        let base = SearchSettings {
            profile: None,
            channels: ChannelsConfig::default(),
            rerank_candidate_count: 24,
        };
        assert_eq!(config.search_settings(None).unwrap(), base);
        assert_eq!(config.base_search_settings(), base);
        // No profile is configured, so naming one is an error, not a default.
        let error = config.search_settings(Some("fast")).unwrap_err();
        let error = error.to_string();
        assert!(error.contains("unknown search profile \"fast\""), "{error}");
        assert!(
            error.contains("no search profiles are configured"),
            "{error}"
        );
    }

    #[test]
    fn a_search_profile_overrides_only_the_keys_it_names() {
        let config: Config = toml::from_str(&profiled_toml()).unwrap();
        config.validate().unwrap();

        // The unprofiled search is untouched by the profiles.
        let base = config.search_settings(None).unwrap();
        assert_eq!(base.profile, None);
        assert_eq!(base.channels, ChannelsConfig::default());
        assert_eq!(base.rerank_candidate_count, 10);

        // `fast` turns the reranker off and shrinks the window; the three
        // retrieval channels inherit.
        let fast = config.search_settings(Some("fast")).unwrap();
        assert_eq!(fast.profile.as_deref(), Some("fast"));
        assert_eq!(
            fast.channels,
            ChannelsConfig {
                semantic: true,
                lexical: true,
                lsp: true,
                reranker: false,
            }
        );
        assert_eq!(fast.rerank_candidate_count, 5);

        // `deep` turns the language servers off and widens the window; the
        // reranker inherits.
        let deep = config.search_settings(Some("deep")).unwrap();
        assert_eq!(
            deep.channels,
            ChannelsConfig {
                semantic: true,
                lexical: true,
                lsp: false,
                reranker: true,
            }
        );
        assert_eq!(deep.rerank_candidate_count, 40);

        // An empty profile inherits everything but is still a named profile.
        let empty: Config =
            toml::from_str(&format!("{}[search_profiles.same]\n", base_toml())).unwrap();
        empty.validate().unwrap();
        let same = empty.search_settings(Some("same")).unwrap();
        assert_eq!(same.profile.as_deref(), Some("same"));
        assert_eq!(same.channels, empty.channels);
        assert_eq!(same.rerank_candidate_count, empty.rerank_candidate_count);

        // A profile inherits the top-level `[channels]`, so it can switch a
        // channel the top level turned off back on.
        let inheriting: Config = toml::from_str(&format!(
            "{}[channels]\nlsp = false\nreranker = false\n\
             [search_profiles.back]\nlsp = true\n\
             [search_profiles.plain]\nrerank_candidate_count = 7\n",
            base_toml()
        ))
        .unwrap();
        inheriting.validate().unwrap();
        let back = inheriting.search_settings(Some("back")).unwrap();
        assert!(back.channels.lsp && !back.channels.reranker);
        let plain = inheriting.search_settings(Some("plain")).unwrap();
        assert!(!plain.channels.lsp && !plain.channels.reranker);
        assert_eq!(plain.rerank_candidate_count, 7);
    }

    #[test]
    fn an_unknown_search_profile_is_an_error_that_lists_the_configured_names() {
        let config: Config = toml::from_str(&profiled_toml()).unwrap();
        config.validate().unwrap();
        let error = config.search_settings(Some("nope")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown search profile \"nope\"; configured: deep, fast"
        );
        // `default` is not a profile; neither is an empty or differently
        // cased name.
        for name in ["default", "", "Fast"] {
            let error = config.search_settings(Some(name)).unwrap_err().to_string();
            assert!(error.contains("unknown search profile"), "{name}: {error}");
            assert!(error.contains("configured: deep, fast"), "{name}: {error}");
        }
    }

    #[test]
    fn a_search_profile_with_an_unknown_key_is_rejected() {
        // `deny_unknown_fields` catches a misspelled switch, and keeps a
        // profile from overriding anything but the query-time switches.
        for key in [
            "semantics = false",
            "rerank_document_max_bytes = 1024",
            "semantic_candidate_count = 10",
            "max_chunks_per_file = 1",
            "embedding_model = 'other'",
            "data_dir = '/tmp/other'",
        ] {
            let toml_text = format!("{}[search_profiles.fast]\n{key}\n", base_toml());
            assert!(
                toml::from_str::<Config>(&toml_text).is_err(),
                "{key} was accepted"
            );
        }
    }

    #[test]
    fn search_profile_names_are_restricted_to_url_safe_words_and_not_default() {
        let accepted = [
            "fast",
            "Deep",
            "a",
            "7",
            "grep-only",
            "grep_only",
            "a2-b_3",
            &"x".repeat(SEARCH_PROFILE_NAME_MAX_LEN),
        ];
        for name in accepted {
            let toml_text = format!("{}[search_profiles.{name}]\n", base_toml());
            let config: Config = toml::from_str(&toml_text).unwrap();
            config
                .validate()
                .unwrap_or_else(|error| panic!("{name} rejected: {error}"));
        }
        let rejected = [
            "default".to_owned(),
            "-fast".to_owned(),
            "_fast".to_owned(),
            "fa st".to_owned(),
            "fast/deep".to_owned(),
            "fast.deep".to_owned(),
            "f\u{e9}".to_owned(),
            "x".repeat(SEARCH_PROFILE_NAME_MAX_LEN + 1),
            String::new(),
        ];
        for name in rejected {
            let mut config = Config {
                data_dir: std::env::temp_dir(),
                ..Config::default()
            };
            config
                .search_profiles
                .insert(name.clone(), SearchProfile::default());
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("search_profiles."), "{name:?}: {error}");
            assert!(error.contains("profile name"), "{name:?}: {error}");
        }
    }

    #[test]
    fn a_search_profile_that_leaves_no_retrieval_channel_is_rejected() {
        // Off in the profile.
        let toml_text = format!(
            "{}[search_profiles.empty]\nsemantic = false\nlexical = false\nlsp = false\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("search_profiles.empty"), "{error}");
        assert!(error.contains("semantic, lexical, and lsp"), "{error}");

        // Off at the top level and left alone by the profile.
        let toml_text = format!(
            "{}[channels]\nsemantic = false\nlexical = false\n\
             [search_profiles.blind]\nlsp = false\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("search_profiles.blind"), "{error}");

        // One channel is enough, and the reranker alone is not a channel.
        let toml_text = format!(
            "{}[search_profiles.grep]\nsemantic = false\nlsp = false\nreranker = false\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        let toml_text = format!(
            "{}[search_profiles.reranker_only]\nsemantic = false\nlexical = false\nlsp = false\nreranker = true\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        assert!(config.validate().is_err());
    }

    #[test]
    fn a_search_profile_rerank_candidate_count_must_be_within_one_to_two_hundred() {
        for (count, valid) in [(0, false), (1, true), (200, true), (201, false)] {
            let toml_text = format!(
                "{}[search_profiles.window]\nrerank_candidate_count = {count}\n",
                base_toml()
            );
            let config: Config = toml::from_str(&toml_text).unwrap();
            match config.validate() {
                Ok(()) => assert!(valid, "{count} was accepted"),
                Err(error) => {
                    assert!(!valid, "{count} was rejected: {error}");
                    let error = error.to_string();
                    assert!(error.contains("search_profiles.window"), "{error}");
                    assert!(error.contains("rerank_candidate_count"), "{error}");
                }
            }
        }
        // A negative count does not even parse as a `usize`.
        let toml_text = format!(
            "{}[search_profiles.window]\nrerank_candidate_count = -1\n",
            base_toml()
        );
        assert!(toml::from_str::<Config>(&toml_text).is_err());
    }

    #[test]
    fn search_profiles_round_trip_through_toml_and_stay_out_of_a_profileless_file() {
        let config: Config = toml::from_str(&profiled_toml()).unwrap();
        let text = toml::to_string(&config).unwrap();
        assert!(text.contains("[search_profiles.fast]"), "{text}");
        assert!(text.contains("[search_profiles.deep]"), "{text}");
        let reloaded: Config = toml::from_str(&text).unwrap();
        reloaded.validate().unwrap();
        assert_eq!(reloaded.search_profiles, config.search_profiles);
        assert_eq!(
            reloaded.search_profiles["fast"],
            SearchProfile {
                reranker: Some(false),
                rerank_candidate_count: Some(5),
                ..SearchProfile::default()
            }
        );

        // Without profiles the serialized configuration has no such table.
        let plain = toml::to_string(&Config {
            data_dir: std::env::temp_dir(),
            ..Config::default()
        })
        .unwrap();
        assert!(!plain.contains("search_profiles"), "{plain}");
    }

    #[test]
    fn fusion_defaults_to_plain_rrf_weights_and_a_two_chunk_file_cap() {
        let expected = FusionConfig {
            semantic_weight: 1.0,
            lexical_weight: 1.0,
            lsp_weight: 1.0,
            max_chunks_per_file: 2,
        };
        // The default, a file without the section, and an empty section all
        // mean plain RRF weights (every channel at 1.0), and at most two
        // chunks of one file ahead of the other files.
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

        // Plain unweighted RRF with no file cap (`UNIT`) differs from the
        // default only in the cap, so it is one explicit line away. Naming the
        // default weight changes nothing, and weighting the semantic channel
        // 2x (the briefly shipped F2a default) is an explicit opt-in that
        // leaves the cap alone.
        assert_ne!(FusionConfig::default(), FusionConfig::UNIT);
        assert_eq!(
            FusionConfig::default(),
            FusionConfig {
                max_chunks_per_file: 2,
                ..FusionConfig::UNIT
            }
        );
        assert_eq!(FusionConfig::UNIT.max_chunks_per_file, 0);
        let plain: Config = toml::from_str(&format!(
            "{}[fusion]\nmax_chunks_per_file = 0\n",
            base_toml()
        ))
        .unwrap();
        plain.validate().unwrap();
        assert_eq!(plain.fusion, FusionConfig::UNIT);
        let named: Config =
            toml::from_str(&format!("{}[fusion]\nsemantic_weight = 1.0\n", base_toml())).unwrap();
        named.validate().unwrap();
        assert_eq!(named.fusion, expected);
        let semantic_two: Config =
            toml::from_str(&format!("{}[fusion]\nsemantic_weight = 2.0\n", base_toml())).unwrap();
        semantic_two.validate().unwrap();
        assert_eq!(
            semantic_two.fusion,
            FusionConfig {
                semantic_weight: 2.0,
                ..expected
            }
        );

        // A section overrides only the weights it names; the rest keep their
        // defaults, including the cap of two.
        let partial: Config =
            toml::from_str(&format!("{}[fusion]\nsemantic_weight = 1.5\n", base_toml())).unwrap();
        partial.validate().unwrap();
        assert_eq!(
            partial.fusion,
            FusionConfig {
                semantic_weight: 1.5,
                max_chunks_per_file: 2,
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

    #[test]
    fn max_chunks_per_file_defaults_to_two_and_is_range_checked() {
        // Two is the default everywhere the section or key can be absent;
        // `UNIT` (plain RRF) is the uncapped, zero baseline.
        assert_eq!(Config::default().fusion.max_chunks_per_file, 2);
        assert_eq!(FusionConfig::default().max_chunks_per_file, 2);
        assert_eq!(FusionConfig::UNIT.max_chunks_per_file, 0);
        let absent: Config = toml::from_str(&base_toml()).unwrap();
        assert_eq!(absent.fusion.max_chunks_per_file, 2);
        let empty: Config = toml::from_str(&format!("{}[fusion]\n", base_toml())).unwrap();
        assert_eq!(empty.fusion.max_chunks_per_file, 2);
        let weight_only: Config =
            toml::from_str(&format!("{}[fusion]\nlsp_weight = 3.0\n", base_toml())).unwrap();
        assert_eq!(weight_only.fusion.max_chunks_per_file, 2);

        // A value is read from the file, validated, and round-trips; it leaves
        // the weights at their defaults. 0 turns the cap off.
        for value in [0, 1, 3, 100] {
            let capped: Config = toml::from_str(&format!(
                "{}[fusion]\nmax_chunks_per_file = {value}\n",
                base_toml()
            ))
            .unwrap();
            capped.validate().unwrap();
            assert_eq!(
                capped.fusion,
                FusionConfig {
                    max_chunks_per_file: value,
                    ..FusionConfig::default()
                }
            );
            let reloaded: Config = toml::from_str(&toml::to_string(&capped).unwrap()).unwrap();
            reloaded.validate().unwrap();
            assert_eq!(reloaded.fusion, capped.fusion, "{value}");
        }

        // 0 and 100 are the ends of the accepted range.
        for good in [0, 1, 2, 100] {
            let mut config = Config {
                data_dir: std::env::temp_dir(),
                ..Config::default()
            };
            config.fusion.max_chunks_per_file = good;
            config.validate().unwrap();
        }
        for bad in [101, 1000, u32::MAX] {
            let mut config = Config {
                data_dir: std::env::temp_dir(),
                ..Config::default()
            };
            config.fusion.max_chunks_per_file = bad;
            let error = config.validate().unwrap_err().to_string();
            assert!(
                error.contains("fusion.max_chunks_per_file"),
                "{bad}: {error}"
            );
            assert!(error.contains("0..=100"), "{bad}: {error}");
        }

        // The check runs at load time, from the file; a negative or fractional
        // value is not an unsigned integer and fails to parse.
        let too_big = format!("{}[fusion]\nmax_chunks_per_file = 101\n", base_toml());
        let config: Config = toml::from_str(&too_big).unwrap();
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("fusion.max_chunks_per_file")
        );
        for value in ["-1", "1.5", "\"2\""] {
            let text = format!("{}[fusion]\nmax_chunks_per_file = {value}\n", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{value}");
        }
    }

    #[test]
    fn embedding_templates_default_to_the_former_behavior() {
        let config = Config::default();
        config.validate().unwrap();
        assert_eq!(
            config.embedding_query_template,
            DEFAULT_EMBEDDING_QUERY_TEMPLATE
        );
        assert_eq!(config.embedding_document_template, "{code}");

        // A file without the keys keeps the defaults.
        let absent: Config = toml::from_str(&base_toml()).unwrap();
        absent.validate().unwrap();
        assert_eq!(
            absent.embedding_query_template,
            config.embedding_query_template
        );
        assert_eq!(
            absent.embedding_document_template,
            config.embedding_document_template
        );
    }

    #[test]
    fn embedding_templates_load_from_toml_and_round_trip() {
        let text = base_toml()
            + "embedding_query_template = 'task: code retrieval | query: {query}'\n"
            + "embedding_document_template = 'title: {path} | text: {code}'\n";
        let config: Config = toml::from_str(&text).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.embedding_query_template,
            "task: code retrieval | query: {query}"
        );
        assert_eq!(
            config.embedding_document_template,
            "title: {path} | text: {code}"
        );
        let reloaded: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        reloaded.validate().unwrap();
        assert_eq!(
            reloaded.embedding_query_template,
            config.embedding_query_template
        );
        assert_eq!(
            reloaded.embedding_document_template,
            config.embedding_document_template
        );

        // A basic string can carry a newline escape, as the bge-code query
        // format needs.
        let bge = base_toml()
            + "embedding_query_template = \"<instruct>Given a question in text, retrieve relevant code that answers it.\\n<query>{query}\"\n";
        let config: Config = toml::from_str(&bge).unwrap();
        config.validate().unwrap();
        assert_eq!(
            config.embedding_query_template,
            "<instruct>Given a question in text, retrieve relevant code that answers it.\n<query>{query}"
        );
        let reloaded: Config = toml::from_str(&toml::to_string(&config).unwrap()).unwrap();
        assert_eq!(
            reloaded.embedding_query_template,
            config.embedding_query_template
        );
    }

    #[test]
    fn embedding_identity_ignores_the_default_document_template_and_the_query_template() {
        let identity = Config::default().embedding_identity();
        assert_eq!(
            identity,
            format!(
                "http://localhost:8766/v1|qwen3-embedding-4b|{}",
                crate::chunk::DOCUMENT_FORMAT_VERSION
            ),
            "the default identity must stay what it was before the templates existed"
        );
        assert!(!identity.contains("doc="), "{identity}");

        // Naming the default document template explicitly changes nothing.
        let named = Config {
            embedding_document_template: "{code}".into(),
            ..Config::default()
        };
        assert_eq!(named.embedding_identity(), identity);

        // The query template only shapes the query, so it never invalidates
        // a stored vector.
        let query_only = Config {
            embedding_query_template: "task: code retrieval | query: {query}".into(),
            ..Config::default()
        };
        assert_eq!(query_only.embedding_identity(), identity);
    }

    #[test]
    fn a_custom_document_template_changes_the_identity_deterministically() {
        let identity = Config::default().embedding_identity();
        let custom = Config {
            embedding_document_template: "title: {path} | text: {code}".into(),
            ..Config::default()
        };
        let custom_identity = custom.embedding_identity();
        assert_ne!(custom_identity, identity);
        // Pinned: the hash is stored in index metadata, so it must not drift.
        assert_eq!(custom_identity, format!("{identity}|doc=d4e50e703f86cec4"));
        assert_eq!(custom.embedding_identity(), custom_identity);
        assert_eq!(custom.clone().embedding_identity(), custom_identity);

        let other = Config {
            embedding_document_template: "{path}\n{code}".into(),
            ..Config::default()
        };
        let other_identity = other.embedding_identity();
        assert_ne!(other_identity, custom_identity);
        assert!(
            other_identity.starts_with(&format!("{identity}|doc=")),
            "{other_identity}"
        );
        let hash = other_identity.rsplit("doc=").next().unwrap();
        assert_eq!(hash.len(), 16, "{other_identity}");
        assert!(
            hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')),
            "{other_identity}"
        );
    }

    #[test]
    fn fnv1a_64_matches_the_reference_vectors() {
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn embedding_templates_missing_their_placeholder_are_rejected() {
        for bad in ["", "plain text", "{code}", "{ query }", "{{query"] {
            let config = Config {
                data_dir: std::env::temp_dir(),
                embedding_query_template: bad.into(),
                ..Config::default()
            };
            let error = config.validate().unwrap_err().to_string();
            assert!(
                error.contains("embedding_query_template") && error.contains("{query}"),
                "{bad:?}: {error}"
            );
        }
        for bad in ["", "plain text", "{path}", "{ code }", "{{code"] {
            let config = Config {
                data_dir: std::env::temp_dir(),
                embedding_document_template: bad.into(),
                ..Config::default()
            };
            let error = config.validate().unwrap_err().to_string();
            assert!(
                error.contains("embedding_document_template") && error.contains("{code}"),
                "{bad:?}: {error}"
            );
        }

        // Each template needs only its own placeholder.
        for good in ["{query}", "Q: {query}", "{query}{query}"] {
            let config = Config {
                data_dir: std::env::temp_dir(),
                embedding_query_template: good.into(),
                ..Config::default()
            };
            config.validate().unwrap();
        }
        for good in ["{code}", "{path}\n{code}", "title: {path} | text: {code}"] {
            let config = Config {
                data_dir: std::env::temp_dir(),
                embedding_document_template: good.into(),
                ..Config::default()
            };
            config.validate().unwrap();
        }

        // The check runs at load time, from the file.
        let text = base_toml() + "embedding_document_template = 'only text'\n";
        let config: Config = toml::from_str(&text).unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("embedding_document_template"), "{error}");
    }

    #[test]
    fn fill_template_substitutes_every_placeholder_in_one_pass() {
        let values = [("path", "src/lib.rs"), ("code", "let s = \"{path}\";")];
        // Substituted text is never scanned again: the code mentions
        // `{path}`, which stays literal whichever value is filled first.
        let expected = "title: src/lib.rs | text: let s = \"{path}\";";
        assert_eq!(
            fill_template("title: {path} | text: {code}", &values),
            expected
        );
        let reversed = [values[1], values[0]];
        assert_eq!(
            fill_template("title: {path} | text: {code}", &reversed),
            expected
        );

        // A placeholder may repeat, and the template may be multi-line.
        assert_eq!(fill_template("{code}{code}", &[("code", "x")]), "xx");
        let values = [("path", "a.rs"), ("code", "fn a() {}")];
        assert_eq!(
            fill_template("{path}\n{code}\n", &values),
            "a.rs\nfn a() {}\n"
        );

        // Text around the placeholders survives, including non-ASCII text.
        assert_eq!(
            fill_template("\u{3042}{code}\u{3044}", &[("code", "x")]),
            "\u{3042}x\u{3044}"
        );
        assert_eq!(fill_template("plain", &[("code", "x")]), "plain");
        assert_eq!(fill_template("", &[("code", "x")]), "");
    }

    #[test]
    fn fill_template_leaves_unknown_and_unbalanced_braces_alone() {
        let values = [("code", "X")];
        // Unknown names, an empty name, and the wrong case are not
        // placeholders.
        assert_eq!(fill_template("{unknown} {}", &values), "{unknown} {}");
        assert_eq!(fill_template("{Code} {query}", &values), "{Code} {query}");
        // An unclosed `{` and a stray `}` are copied as they are.
        assert_eq!(fill_template("{nope} {code", &values), "{nope} {code");
        assert_eq!(fill_template("a } {code} {", &values), "a } X {");
        // With doubled braces the inner placeholder still fills and the
        // outer braces stay.
        assert_eq!(fill_template("{{code}}", &values), "{X}");
    }
}
