use crate::{endpoint::validate_api_key_env, provider::SystemOneConfig};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

pub mod layers;
pub mod profile;

pub use profile::{
    LspPlaces, LspProfile, LspRoots, LspSessions, LspSync, RunProfile, RunProfileConfig,
    RunProfileSeeds, RunProfileTraverse, WalkGroups, WalkPricing, WalkProfile,
};

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
    /// Index prose files (Markdown). `false` (default): files of prose
    /// adapters are not indexed at all, so an index and every search result
    /// are exactly as before the Markdown adapter and the docs lane never
    /// runs. `true`: they are indexed and the docs lane follows
    /// `[docs_lane]`. Flipping the switch makes the snapshot stale; the next
    /// refresh adds or drops exactly the prose files.
    pub prose: bool,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            freshness: IndexFreshness::OnSearch,
            stale_check_interval_seconds: 10,
            wait_for_existing_job_seconds: 120,
            prose: false,
        }
    }
}

/// Which service rescores the fused shortlist when `[channels].reranker` is
/// on (`rescorer`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Rescorer {
    /// The reranking service (`reranker_url`): one score per document.
    #[default]
    Reranker,
    /// The typed `[system_one]` service (`provider = "system_one_wire"`):
    /// one yes/no question per candidate, answered as a probability.
    SystemOne,
}

impl Rescorer {
    fn is_default(&self) -> bool {
        *self == Self::default()
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
///
/// Four more keys change only how the profile's MCP endpoint (`/mcp/<name>`)
/// presents itself to an agent, never what a search retrieves: `tools` (which
/// MCP tools the endpoint lists), `response` (the shape of the `search_code`
/// result), and, for `response = "compact"`, `compact_max_hit_lines` and
/// `compact_max_response_chars`. REST and the command line always return the
/// full result.
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
    /// Override of `[docs_lane].top_k` (`0..=10`; `0` runs no docs lane).
    pub docs_top_k: Option<usize>,
    /// Override of `[docs_lane].reranker`; `[channels].reranker` and the
    /// profile's `reranker` do not affect the docs lane.
    pub docs_reranker: Option<bool>,
    /// Which MCP tools the profile's endpoint lists and dispatches; absent
    /// means [`McpToolList::Full`].
    pub tools: Option<McpToolList>,
    /// The shape of the `search_code` result on the profile's MCP endpoint;
    /// absent means [`McpResponse::Full`].
    pub response: Option<McpResponse>,
    /// With `response = "compact"`: most lines of code one hit keeps
    /// (`1..=`[`COMPACT_MAX_HIT_LINES_LIMIT`]); absent means
    /// [`DEFAULT_COMPACT_MAX_HIT_LINES`].
    pub compact_max_hit_lines: Option<usize>,
    /// With `response = "compact"`: most serialized characters the whole
    /// `search_code` result may have
    /// ([`COMPACT_MAX_RESPONSE_CHARS_RANGE`]); absent means
    /// [`DEFAULT_COMPACT_MAX_RESPONSE_CHARS`].
    pub compact_max_response_chars: Option<usize>,
}

impl SearchProfile {
    /// How the profile's MCP endpoint presents itself (see
    /// [`Config::mcp_presentation`]); the keys it leaves out give every tool
    /// and the full result.
    pub fn mcp_presentation(&self) -> McpPresentation {
        let defaults = CompactLimits::default();
        McpPresentation {
            tools: self.tools.unwrap_or_default(),
            compact: (self.response == Some(McpResponse::Compact)).then(|| CompactLimits {
                max_hit_lines: self.compact_max_hit_lines.unwrap_or(defaults.max_hit_lines),
                max_response_chars: self
                    .compact_max_response_chars
                    .unwrap_or(defaults.max_response_chars),
            }),
        }
    }
}

/// The MCP tool list an endpoint offers (`[search_profiles.<name>].tools`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpToolList {
    /// Every tool: indexing, watching, status, search, and navigation.
    #[default]
    Full,
    /// Only `search_code`, `search_symbols`, `find_definition`, and
    /// `find_references`; the other tools are neither listed nor callable.
    Search,
}

/// The shape of an MCP `search_code` result (`[search_profiles.<name>].response`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum McpResponse {
    /// The whole search report, as REST and the command line return it.
    #[default]
    Full,
    /// Locators and code only, with a per-hit line cap and a whole-response
    /// ceiling (see [`CompactLimits`]).
    Compact,
}

/// Default for `compact_max_hit_lines`.
pub const DEFAULT_COMPACT_MAX_HIT_LINES: usize = 40;
/// Largest accepted `compact_max_hit_lines`.
pub const COMPACT_MAX_HIT_LINES_LIMIT: usize = 1000;
/// Default for `compact_max_response_chars`: a starting value, not a measured
/// client limit.
pub const DEFAULT_COMPACT_MAX_RESPONSE_CHARS: usize = 16_000;
/// Accepted `compact_max_response_chars`.
pub const COMPACT_MAX_RESPONSE_CHARS_RANGE: std::ops::RangeInclusive<usize> = 1_000..=1_000_000;

/// The limits of a compact `search_code` result, resolved from a profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactLimits {
    /// Most lines of code one hit keeps.
    pub max_hit_lines: usize,
    /// Most serialized characters the whole result may have.
    pub max_response_chars: usize,
}

impl Default for CompactLimits {
    fn default() -> Self {
        Self {
            max_hit_lines: DEFAULT_COMPACT_MAX_HIT_LINES,
            max_response_chars: DEFAULT_COMPACT_MAX_RESPONSE_CHARS,
        }
    }
}

/// How one MCP endpoint presents itself: the tool list and the shape of the
/// `search_code` result. Resolved by [`Config::mcp_presentation`]; the default
/// (`/mcp`, or a profile that sets none of the keys) is every tool and the
/// full result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct McpPresentation {
    /// Which tools the endpoint lists and dispatches.
    pub tools: McpToolList,
    /// `Some` when `search_code` returns the compact result.
    pub compact: Option<CompactLimits>,
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
    /// How many documentation chunks the separate `docs` list holds
    /// (`[docs_lane].top_k`, or the profile's `docs_top_k`); `0` runs no
    /// docs lane. The lane also needs `[docs_lane].enabled`.
    pub docs_top_k: usize,
    /// Whether the docs lane is reranked: `[docs_lane].reranker` (default
    /// off) unless the profile sets `docs_reranker`.
    pub docs_reranker: bool,
}

/// `[docs_lane]`: the separate, small list of documentation chunks
/// (Markdown, any prose extractor) that `search_code` returns beside its code
/// `results` when the request names no `source_roles`. The code list is
/// ranked exactly as if the documentation were not indexed; the docs lane is
/// fed the prose chunks only and fails open (a failure becomes a
/// `docs.warning`, never a failure of the search).
///
/// The lane's reranker is its own switch, off by default (`reranker` here; a
/// search profile may override it with `docs_reranker`); `[channels].reranker`
/// does not affect it. The fusion weights, `rrf_k`, and the per-file cap are
/// the code lane's. A request that names `source_roles`
/// gets one list as before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DocsLaneConfig {
    /// Run the docs lane. `false`: documentation chunks stay out of the
    /// default search altogether (they are still reachable with
    /// `source_roles = ["documentation"]`).
    pub enabled: bool,
    /// Most documentation chunks returned (`0..=10`; `0` runs no lane).
    pub top_k: usize,
    /// Documentation candidates kept after fusion and sent to the reranker
    /// (`1..=200`).
    pub rerank_candidate_count: usize,
    /// Depth of the docs lane's semantic candidate list (`1..=200`).
    pub semantic_candidate_count: usize,
    /// Rerank the docs lane (needs the reranker service). Independent of
    /// `[channels].reranker`; default off.
    pub reranker: bool,
}

impl DocsLaneConfig {
    /// Largest accepted `top_k`.
    pub const MAX_TOP_K: usize = 10;

    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            self.top_k <= Self::MAX_TOP_K,
            "docs_lane.top_k must be 0..={} (got {}); 0 runs no docs lane",
            Self::MAX_TOP_K,
            self.top_k
        );
        ensure!(
            (1..=200).contains(&self.rerank_candidate_count),
            "docs_lane.rerank_candidate_count must be 1..=200 (got {})",
            self.rerank_candidate_count
        );
        ensure!(
            (1..=200).contains(&self.semantic_candidate_count),
            "docs_lane.semantic_candidate_count must be 1..=200 (got {})",
            self.semantic_candidate_count
        );
        Ok(())
    }
}

impl Default for DocsLaneConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            top_k: 2,
            rerank_candidate_count: 12,
            semantic_candidate_count: 20,
            reranker: false,
        }
    }
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

/// The port of the external generation model. LCI never contacts it, so
/// [`Config::validate`] rejects it in [`Config::embedding_url`] and
/// [`Config::reranker_url`], whatever host the URL names.
pub const GENERATION_MODEL_PORT: u16 = 8765;

/// Ports that belong to other services and that LCI itself never listens on:
/// 8765 is the external generation model, 8766 the embedding service, 8767
/// the reranker. [`Config::validate`] rejects them as [`Config::port`].
pub const RESERVED_PORTS: std::ops::RangeInclusive<u16> = GENERATION_MODEL_PORT..=8767;

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
    /// The NAME of the environment variable holding the embedder's API key,
    /// sent as a bearer token; the key itself is never configured. Absent:
    /// no credential is sent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub embedding_api_key_env: Option<String>,
    /// Like `embedding_api_key_env`, for the reranker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reranker_api_key_env: Option<String>,
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
    /// Bound in seconds (1..=30) of each dependency probe of /ready and
    /// service_status, and of the --version probe lsp.live runs once per
    /// language-server manager before a traversal. Default 15 (d450).
    pub readiness_timeout_seconds: u64,
    pub ripgrep_path: String,
    pub semantic_candidate_count: usize,
    pub lexical_candidate_count: usize,
    /// Candidates kept after fusion: the reranker's window, and the most
    /// results a search returns even when `top_k` asks for more (a `top_k`
    /// passed above it adds a note to the response). A search profile may set
    /// its own.
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
    /// Restore rust-analyzer's execution/network defaults. Only enable when
    /// every repository this process accesses and its dependencies are trusted.
    /// Also gates the `probe.compile` traverse map (`cargo check` runs build
    /// scripts and procedural macros).
    pub rust_analyzer_trusted: bool,
    /// Seconds one request to a language server may go without any sign of
    /// life from it before it fails (1..=86400).
    pub lsp_timeout_seconds: u64,
    /// Seconds a language server's start (its `initialize` request) may go
    /// without any sign of life from it before the start fails (default `0`:
    /// the start is bounded by `lsp_timeout_seconds`, as every request is).
    /// Raise it for a server that is silent while it loads a large solution
    /// or project at start (csharp-ls, jdtls) without raising the bound of
    /// every later request. Only `initialize` uses it; requests to a started
    /// server, `initialized` and the adapters' own hooks keep
    /// `lsp_timeout_seconds`. Allowed: `0..=86400`. Per machine: it is not
    /// part of the run-profile hash.
    pub lsp_start_timeout_seconds: u64,
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
    /// released server costs one cold start (and re-priming) later. Allowed:
    /// `0..=86400`. Values below 60 are meant for tests.
    pub lsp_idle_timeout_seconds: u64,
    /// Seconds a language server may answer nothing but transient errors
    /// ("ask again": `ContentModified`, `ServerCancelled`, `RequestFailed`, a
    /// loading server's own errors) before its session is replaced (default
    /// `900`, 15 minutes; `0` never replaces it). The span runs from the
    /// session's first such answer, each repeat's included, also for a call
    /// that a search's latency budget cancelled. Any other answer (a result,
    /// or an error that is not transient, `RequestCancelled` among them) ends
    /// it, and so does a `$/progress` notification from the server: a server
    /// that reports progress is loading. A server that is still loading
    /// legitimately answers them for minutes, and a restarted one starts over,
    /// so this must stay above the longest load you expect: a repository that
    /// needs longer would be restarted every time the span runs out. The
    /// default is not measured. Allowed: `0..=86400`. Values below 60 are
    /// meant for tests.
    pub lsp_stuck_seconds: u64,
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
    /// Optional Markdown language-server (marksman) settings. Absent or empty
    /// configuration keeps prose navigation disabled; the server is reached
    /// only for an indexed prose file (`[index] prose = true`).
    pub markdown: MarkdownLspConfig,
    pub index: IndexConfig,
    /// Query-time channel and reranker switches; all enabled by default.
    pub channels: ChannelsConfig,
    /// Per-channel Reciprocal Rank Fusion weights; all `1.0` (plain RRF) by
    /// default. Also `max_chunks_per_file` (default `2`; `0` is unlimited).
    pub fusion: FusionConfig,
    /// `[docs_lane]`: the separate list of documentation chunks beside the
    /// code results. Left out of the serialized configuration while it equals
    /// the default.
    #[serde(skip_serializing_if = "DocsLaneConfig::is_default")]
    pub docs_lane: DocsLaneConfig,
    /// Named query-time override sets, `[search_profiles.<name>]`, chosen per
    /// request (see [`SearchProfile`]). None by default; a search that names
    /// no profile ignores them all.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub search_profiles: BTreeMap<String, SearchProfile>,
    /// The graph-traversal mode of `search_code` (`mode = "traverse"`) and its
    /// limits. Off by default; while it is off `search_code` is exactly the
    /// search it was before the mode existed. Left out of the serialized
    /// configuration while it equals the default, so `config` and the layer
    /// reports stay as they were.
    #[serde(skip_serializing_if = "TraverseConfig::is_default")]
    pub traverse: TraverseConfig,
    /// `[recipes]`: the named recipe tools (`config_readers`, `impact_rings`,
    /// `flow_path`, `flow_from`, `config_neighbours`). Off by default; while
    /// off the tool list, schemas, REST routes and server instructions are
    /// those of a server without them. Left out of the serialized
    /// configuration while it equals the default.
    #[serde(skip_serializing_if = "RecipesConfig::is_default")]
    pub recipes: RecipesConfig,
    /// `[compose]`: the `lci_compose` tool (operator steps in one call). Off
    /// by default, with the same guarantees as `[recipes]`.
    #[serde(skip_serializing_if = "ComposeConfig::is_default")]
    pub compose: ComposeConfig,
    /// `[system_one]`: the small model that steers a traversal (a local
    /// server, a hosted provider or another machine). Off when absent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_one: Option<SystemOneConfig>,
    /// Which service orders the shortlist when `[channels].reranker` is on:
    /// `"reranker"` (the default) or `"system_one"`, which needs
    /// `[system_one]` with `provider = "system_one_wire"`. Left out of the
    /// serialized configuration while it is the default.
    #[serde(skip_serializing_if = "Rescorer::is_default")]
    pub rescorer: Rescorer,
    /// `[run_profile]`: the home of the switches that name a run's
    /// result-affecting configuration (`walk.groups`, `walk.grep_corroboration`, `walk.pricing`,
    /// `traverse.map_freshness`, `traverse.answer_labels`, `traverse.card_paths`, `scip.mode`, `lsp.sessions`,
    /// `lsp.outline_cache`, `lsp.implementations`, `lsp.call_hierarchy`, `lsp.sync`,
    /// `lsp.places`, `lsp.roots`, `lsp.breaker`, `freshness.verify_on_read`, `freshness.stat_gate`, `seeds.bm25`, `seeds.literal`, `seeds.repo_map` and `grep.structural`). Always
    /// serialized; the hashed profile of the whole configuration is
    /// [`Config::run_profile`].
    pub run_profile: RunProfileConfig,
}

/// How a traversal ranks its results (`[traverse] fusion`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraverseFusion {
    /// The path potential lifted by `corroboration_boost` per extra independent
    /// map.
    #[default]
    Boost,
    /// Weighted reciprocal rank fusion over the evidence sources.
    Rrf,
    /// Capped log-odds over the evidence sources: each source adds
    /// `weight * clamp(logit(potential), -3, 3)`, and a source that did not
    /// deliver the node costs the cap.
    #[serde(rename = "log_odds")]
    LogOdds,
}

impl TraverseFusion {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Boost => "boost",
            Self::Rrf => "rrf",
            Self::LogOdds => "log_odds",
        }
    }

    /// Whether the fusion ranks by the best potential each map delivered a
    /// node with (every fusion but `boost`): the walk then records it, orders
    /// ties by node id and has no score bound.
    pub const fn uses_reach(self) -> bool {
        !matches!(self, Self::Boost)
    }
}

impl std::str::FromStr for TraverseFusion {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "boost" => Ok(Self::Boost),
            "rrf" => Ok(Self::Rrf),
            "log_odds" => Ok(Self::LogOdds),
            other => Err(format!(
                "fusion must be \"boost\", \"rrf\" or \"log_odds\" (got {other:?})"
            )),
        }
    }
}

/// What the configured reranker's scores mean (`[traverse] rerank_scores`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraverseRerankScores {
    /// A score is a relevance in `[0, 1]` (llama.cpp's `/rerank` returns
    /// probabilities); one outside the range is clamped.
    #[default]
    Probability,
    /// A score is a logit; its relevance is `1 / (1 + e^-score)`.
    Logit,
}

impl TraverseRerankScores {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Probability => "probability",
            Self::Logit => "logit",
        }
    }
}

impl std::str::FromStr for TraverseRerankScores {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "probability" => Ok(Self::Probability),
            "logit" => Ok(Self::Logit),
            other => Err(format!(
                "rerank_scores must be \"probability\" or \"logit\" (got {other:?})"
            )),
        }
    }
}

/// How the first-stage answers (the exact operators' cards and, with
/// `answer_seeds`, the start nodes) join the walk's results
/// (`[traverse] first_stage`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TraverseFirstStage {
    /// The results are the walk's alone, as without the setting.
    Off,
    /// The first-stage list (at most `first_k`), then the walk's results.
    #[default]
    Append,
    /// First-stage and walk results alternately, first stage first, cut to
    /// `top_k` in total.
    Interleave,
}

impl TraverseFirstStage {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Append => "append",
            Self::Interleave => "interleave",
        }
    }
}

impl std::str::FromStr for TraverseFirstStage {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "off" => Ok(Self::Off),
            "append" => Ok(Self::Append),
            "interleave" => Ok(Self::Interleave),
            other => Err(format!(
                "first_stage must be \"off\", \"append\" or \"interleave\" (got {other:?})"
            )),
        }
    }
}

/// The names a `[traverse.policies]` table may use: the steering presets
/// (`steer.policy`, `--policy`). A test in the application crate keeps this
/// list equal to the presets.
pub const TRAVERSE_POLICY_NAMES: [&str; 5] = ["explore", "impact", "flow", "where-to", "history"];

/// What a steering policy (a question type) sets about the first stage and
/// the walk, `[traverse.policies.<name>]`. Every field is optional; a field
/// left out falls through to the policy's built-in value (the preset file of
/// the application) and then to the `[traverse]` key of the same name. The
/// built-in values are the measured defaults (N0, N0b of the multimap spike
/// v2): see `config.example.toml`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TraversePolicy {
    /// `false`: the first stage alone answers. The walk takes one pop (so the
    /// response still carries a handle a caller may continue) and its cards
    /// are not returned. `true`: the walk runs and its cards follow the
    /// first stage's. Only with a first stage (`first_stage` other than
    /// `"off"`); a first-stage-less call always returns the walk.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub walk: Option<bool>,
    /// The walk runs only when the call asks for at least this many answers,
    /// `first_k + top_k`; below it the first stage alone answers (`walk =
    /// false`). `0`: no minimum. Needs the walk to be on.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub walk_min_answers: Option<u32>,
    /// Maps added to the default maps when the call names none (for example
    /// `exact.closure` for impact questions). A call that names its maps uses
    /// exactly those. Maps that read the tree are skipped when the call has
    /// no tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_maps: Option<Vec<String>>,
    /// `first_k` for calls with this policy, `0..=200`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_k: Option<u32>,
    /// `seed_k` for calls with this policy, `1..=40`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seed_k: Option<u32>,
}

impl TraversePolicy {
    /// Most answers a call may ask for with `first_k + top_k`.
    pub const MAX_WALK_MIN_ANSWERS: u32 = 240;

    /// `self` with every field it leaves out taken from `base`.
    #[must_use]
    pub fn over(&self, base: &Self) -> Self {
        Self {
            walk: self.walk.or(base.walk),
            walk_min_answers: self.walk_min_answers.or(base.walk_min_answers),
            extra_maps: self.extra_maps.clone().or_else(|| base.extra_maps.clone()),
            first_k: self.first_k.or(base.first_k),
            seed_k: self.seed_k.or(base.seed_k),
        }
    }

    /// Checks the values; `at` names the table in the message.
    pub fn validate(&self, at: &str) -> Result<()> {
        if let Some(k) = self.first_k {
            ensure!(
                k <= TraverseConfig::MAX_FIRST_K_LIMIT,
                "{at}.first_k must be 0..={} (got {k})",
                TraverseConfig::MAX_FIRST_K_LIMIT
            );
        }
        if let Some(k) = self.seed_k {
            ensure!(
                (1..=TraverseConfig::MAX_SEED_K_LIMIT).contains(&k),
                "{at}.seed_k must be 1..={} (got {k})",
                TraverseConfig::MAX_SEED_K_LIMIT
            );
        }
        if let Some(n) = self.walk_min_answers {
            ensure!(
                n <= Self::MAX_WALK_MIN_ANSWERS,
                "{at}.walk_min_answers must be 0..={} (got {n})",
                Self::MAX_WALK_MIN_ANSWERS
            );
        }
        for map in self.extra_maps.iter().flatten() {
            ensure!(
                !map.trim().is_empty(),
                "{at}.extra_maps must not hold an empty name"
            );
        }
        Ok(())
    }
}

/// `[traverse]`: the multimap traversal behind `search_code`'s
/// `mode = "traverse"` (spine brief 1.7). The limits are also the caps of a
/// request: a `budget` in the request may lower them, never raise them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TraverseConfig {
    /// Accept `mode = "traverse"` in `search_code` and say so in the server
    /// instructions. `false`: the mode is refused with an error, the tool's
    /// schema and instructions are those of a server without it, and every
    /// other `search_code` call is unchanged.
    pub enabled: bool,
    /// The SCIP index a traversal walks (map `graph.scip`), as written by
    /// `rust-analyzer scip`. A relative path is taken from the workspace
    /// directory. The tree it indexes is the workspace.
    pub scip_index: String,
    /// Most pops (node expansions) of one traversal, `1..=2000`.
    pub max_pops: u32,
    /// Depth at which a traversal records states without expanding them, so
    /// the deepest state it expands is at `max_depth - 1`; `1..=32`.
    pub max_depth: u32,
    /// Most estimated tokens the expansions of one traversal may cost,
    /// `1..=1000000`.
    pub max_est_tokens: u64,
    /// Longest the walk of one call may run, in milliseconds, `1..=60000`.
    /// The hybrid search that finds the start nodes does not count against
    /// it: that search is bounded by `embedding_timeout_seconds` and
    /// `reranker_timeout_seconds`, so a call takes at most those plus this
    /// (and the time to load the index, which does count).
    pub deadline_ms: u64,
    /// Most stops (handler consultations) of one traversal, `0..=32`.
    pub max_stops: u32,
    /// Confidence a rung of the steering ladder needs to decide a stop,
    /// from `0` to `1`. Only the rules rung exists, with confidence 1 on a
    /// clean match and 0 otherwise: any value above `0` behaves the same,
    /// and `0` lets an unmatched rule list decide with no actions, so no
    /// stop escalates to the caller (a rule's own `stop` and a list cut
    /// short still end the call).
    pub tau: f32,
    /// How much each independent map beyond the first that reached a node
    /// lifts the node's rank: `potential * (1 + boost * (maps - 1))`. A
    /// number from `0` (rank by potential alone) to `10`. A resumed
    /// traversal must be given the boost its handle was made with.
    pub corroboration_boost: f64,
    /// How the results of a traversal are ranked: `"boost"` (the default) ranks
    /// by the path potential lifted by `corroboration_boost`; `"rrf"` ranks by
    /// weighted reciprocal rank fusion over the evidence sources (one ranked
    /// list per independent map, the maps built on one graph counting as one
    /// source), each node scoring `weight / (rank + 2)` per source;
    /// `"log_odds"` ranks by capped log-odds over the same sources, each
    /// source adding `weight * clamp(ln(p / (1 - p)), -3, 3)` for the best
    /// potential `p` it delivered the node with, and `-3 * weight` when it
    /// did not deliver the node. Under `"rrf"` and `"log_odds"` the score
    /// bound is off and `corroboration_boost` does not rank.
    /// A resumed traversal keeps the fusion its handle was made with.
    pub fusion: TraverseFusion,
    /// Connective nodes (files, commits, config keys) are expanded without
    /// costing a pop; a traversal ends when such free pops reach `max_pops`,
    /// and their tokens are charged as for any expansion. A resumed traversal keeps the
    /// setting its handle was made with.
    pub pass_through: bool,
    /// Reranker frontier: after the gate and the beam have kept the edges of
    /// an expansion, one batched reranker call scores the kept destinations
    /// against the question, and each kept hop's value is multiplied by
    /// `rerank_floor + (1 - rerank_floor) * s`, `s` the relevance in `[0, 1]`.
    /// The factor is at most 1, so the score bound stays sound. A failed call
    /// leaves the factor at 1. A resumed traversal must be given the same
    /// setting (its handle keeps it).
    pub rerank_frontier: bool,
    /// The factor a hop gets when the reranker rates its destination 0; from
    /// `0` to `1` (`1` switches the effect off). Only with `rerank_frontier`.
    pub rerank_floor: f64,
    /// What the reranker's scores are, fixed by configuration so that a
    /// document's relevance never depends on the others of its call:
    /// `"probability"` (the default; llama.cpp's `/rerank`) clamps each score
    /// to `[0, 1]`; `"logit"` maps each through `1 / (1 + e^-score)`. Only with
    /// `rerank_frontier`. A resumed traversal must be given the same setting
    /// (its handle keeps it with the reranker's model and endpoint).
    pub rerank_scores: TraverseRerankScores,
    /// Most model calls (the reranker frontier's calls; later System One's) one
    /// traversal may make, a continuation counting on from the calls before
    /// it, `1..`. Once the cap is reached the frontier stops rating (the
    /// hops keep factor 1) and the walk goes on. Absent: no cap.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_model_calls: Option<u32>,
    /// Whether the score bound may end a traversal early (stop reason
    /// `bound`). `false`: the walk runs to its budgets or a dry frontier. A
    /// resumed traversal keeps the setting its handle was made with.
    pub score_bound: bool,
    /// A table of fitted hop priors (`cargo xtask maps fit-priors` writes
    /// one), read when a traversal starts; a relative path is taken from the
    /// working directory. Needs `question_type`. Absent: hops are priced by
    /// the guessed table. A continuation handle keeps the table's content hash
    /// and is refused when the file has changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priors: Option<String>,
    /// The question type (`impact`, `flow`, `where-to`, `history`) whose
    /// priors in `priors` price the hops; a type the table does not hold is
    /// priced by the guess. Needs `priors`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub question_type: Option<String>,
    /// A fitted tie-break file (`cargo xtask maps fit-tiebreak` writes one): a
    /// weighted sum of result features that orders results tied at the same
    /// rank. Read when a traversal starts; a relative path is taken from the
    /// working directory. Absent: ties keep discovery order. A continuation
    /// handle keeps the file's content hash and is refused when the file has
    /// changed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tie_break: Option<String>,
    /// Hits of the hybrid search (the pipeline `search_code` runs, with the
    /// profile of the request) that become start nodes when a request gives
    /// a question and no seeds, `1..=40`.
    pub seed_k: u32,
    /// How the first-stage answers join the walk's results: `"append"`
    /// (default; the first stage, then the walk's results), `"interleave"` or
    /// `"off"` (the results are the walk's alone). The first stage is the
    /// cards of the exact operators the call selected (`exact.closure`,
    /// `exact.pickaxe`), in the order of the maps list, then, with
    /// `answer_seeds`, the start nodes. A node is listed once, at its first
    /// place. A continuation (`handle`) returns the walk alone. Measured: N0,
    /// N0b of the multimap spike v2.
    pub first_stage: TraverseFirstStage,
    /// Most first-stage answers returned; `0` is no cap, `0..=200`. A
    /// steering policy may set its own (`policies`).
    pub first_k: u32,
    /// Also answer with the start nodes (cards with `origin: "seed"`) after
    /// the exact cards (default on). Only with `first_stage` other than
    /// `"off"`.
    pub answer_seeds: bool,
    /// Add the exact cards whose node the SCIP graph holds to the walk's start
    /// nodes, after the others (default on; measured as neutral).
    pub exact_seeds: bool,
    /// Per steering policy (`steer.policy`, `--policy`) settings of the first
    /// stage and the walk, `[traverse.policies.<name>]` with the names
    /// `explore`, `impact`, `flow`, `where-to`, `history`. Keys given here win
    /// over the policy's built-in values (the measured defaults, see
    /// `config.example.toml`), which win over the `[traverse]` keys of the same
    /// name. Empty: the built-in values apply.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub policies: BTreeMap<String, TraversePolicy>,
    /// Most edges of one expansion the walk follows once the expansion offers
    /// more than 8: the best `beam` by the hop score are adopted and the rest
    /// are dropped (a search heuristic, not a limit of the maps). `16` is
    /// what the walk always used; the recipes raise it so that a node with
    /// many readers or callers is not cut. A resumed traversal runs with the
    /// beam of the call that resumes it. Allowed: `1..=256`.
    pub beam: usize,
}

impl TraverseConfig {
    pub const MAX_BEAM_LIMIT: usize = 256;
    pub const MAX_FIRST_K_LIMIT: u32 = 200;
    pub const MAX_POPS_LIMIT: u32 = 2000;
    pub const MAX_SEED_K_LIMIT: u32 = 40;
    pub const MAX_DEPTH_LIMIT: u32 = 32;
    pub const MAX_EST_TOKENS_LIMIT: u64 = 1_000_000;
    pub const MAX_DEADLINE_MS_LIMIT: u64 = 60_000;
    pub const MAX_STOPS_LIMIT: u32 = 32;
    pub const MAX_CORROBORATION_BOOST: f64 = 10.0;

    fn is_default(&self) -> bool {
        *self == Self::default()
    }

    fn validate(&self) -> Result<()> {
        ensure!(
            !self.scip_index.trim().is_empty(),
            "traverse.scip_index must be nonempty"
        );
        ensure!(
            (1..=Self::MAX_POPS_LIMIT).contains(&self.max_pops),
            "traverse.max_pops must be 1..={} (got {})",
            Self::MAX_POPS_LIMIT,
            self.max_pops
        );
        ensure!(
            (1..=Self::MAX_DEPTH_LIMIT).contains(&self.max_depth),
            "traverse.max_depth must be 1..={} (got {})",
            Self::MAX_DEPTH_LIMIT,
            self.max_depth
        );
        ensure!(
            (1..=Self::MAX_EST_TOKENS_LIMIT).contains(&self.max_est_tokens),
            "traverse.max_est_tokens must be 1..={} (got {})",
            Self::MAX_EST_TOKENS_LIMIT,
            self.max_est_tokens
        );
        ensure!(
            (1..=Self::MAX_DEADLINE_MS_LIMIT).contains(&self.deadline_ms),
            "traverse.deadline_ms must be 1..={} (got {})",
            Self::MAX_DEADLINE_MS_LIMIT,
            self.deadline_ms
        );
        ensure!(
            self.max_stops <= Self::MAX_STOPS_LIMIT,
            "traverse.max_stops must be 0..={} (got {})",
            Self::MAX_STOPS_LIMIT,
            self.max_stops
        );
        ensure!(
            self.tau.is_finite() && (0.0..=1.0).contains(&self.tau),
            "traverse.tau must be a number from 0 to 1 (got {})",
            self.tau
        );
        ensure!(
            (0.0..=Self::MAX_CORROBORATION_BOOST).contains(&self.corroboration_boost),
            "traverse.corroboration_boost must be a number from 0 to {} (got {})",
            Self::MAX_CORROBORATION_BOOST,
            self.corroboration_boost
        );
        ensure!(
            (0.0..=1.0).contains(&self.rerank_floor),
            "traverse.rerank_floor must be a number from 0 to 1 (got {})",
            self.rerank_floor
        );
        ensure!(
            self.max_model_calls.is_none_or(|n| n >= 1),
            "traverse.max_model_calls must be at least 1 (leave it out for no cap)"
        );
        ensure!(
            (1..=Self::MAX_SEED_K_LIMIT).contains(&self.seed_k),
            "traverse.seed_k must be 1..={} (got {})",
            Self::MAX_SEED_K_LIMIT,
            self.seed_k
        );
        ensure!(
            (1..=Self::MAX_BEAM_LIMIT).contains(&self.beam),
            "traverse.beam must be 1..={} (got {})",
            Self::MAX_BEAM_LIMIT,
            self.beam
        );
        ensure!(
            self.first_k <= Self::MAX_FIRST_K_LIMIT,
            "traverse.first_k must be 0..={} (got {})",
            Self::MAX_FIRST_K_LIMIT,
            self.first_k
        );
        for (name, policy) in &self.policies {
            ensure!(
                TRAVERSE_POLICY_NAMES.contains(&name.as_str()),
                "traverse.policies.{name}: unknown policy; the policies are {}",
                TRAVERSE_POLICY_NAMES.join(", ")
            );
            policy.validate(&format!("traverse.policies.{name}"))?;
        }
        ensure!(
            self.priors.is_some() == self.question_type.is_some(),
            "traverse.priors and traverse.question_type go together: a table is read for one question type"
        );
        for (key, value) in [
            ("priors", &self.priors),
            ("question_type", &self.question_type),
            ("tie_break", &self.tie_break),
        ] {
            ensure!(
                value.as_deref().is_none_or(|v| !v.trim().is_empty()),
                "traverse.{key} must be nonempty"
            );
        }
        Ok(())
    }
}

impl Default for TraverseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            scip_index: "index.scip".into(),
            max_pops: 200,
            max_depth: 6,
            max_est_tokens: 20_000,
            deadline_ms: 5_000,
            max_stops: 4,
            tau: 0.8,
            corroboration_boost: 0.1,
            fusion: TraverseFusion::Boost,
            pass_through: false,
            rerank_frontier: false,
            rerank_floor: 0.25,
            rerank_scores: TraverseRerankScores::Probability,
            max_model_calls: None,
            score_bound: true,
            priors: None,
            question_type: None,
            tie_break: None,
            seed_k: 8,
            first_stage: TraverseFirstStage::Append,
            first_k: 10,
            answer_seeds: true,
            exact_seeds: true,
            policies: BTreeMap::new(),
            beam: 16,
        }
    }
}

/// `[recipes]`: the named server-side recipes. They read the SCIP index of
/// `[traverse] scip_index` and, for the config recipes, the tree's
/// `config.example.toml`; `[traverse] enabled` is not needed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RecipesConfig {
    /// Serve the recipe tools over MCP and REST. `false`: no recipe tool is
    /// listed or routed, and the other tools are unchanged.
    pub enabled: bool,
    /// Let a judge drop the walk results of `flow_from` that do not answer
    /// the question. No judge is wired in this build: `true` only adds a note
    /// to the answer and leaves the walk order as it is.
    pub flow_judge: bool,
}

impl RecipesConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// `[compose]`: the `lci_compose` tool.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ComposeConfig {
    /// Serve `lci_compose` over MCP and REST. `false`: not listed or routed.
    pub enabled: bool,
}

impl ComposeConfig {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
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
/// semantics. (The transport has since put every language server in a Job
/// Object of its own on Windows, which would also contain such a chain, but
/// `java` stays a direct child: the design needs no multi-hop tree.)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct JavaLspConfig {
    /// Path to the jdtls installation directory. `None` (the default) keeps
    /// Java navigation disabled. An empty or whitespace-only value is
    /// rejected by validation rather than silently counting as enabled. A
    /// relative path is resolved against the process's working directory.
    pub path: Option<String>,
    /// Additional JVM/jdtls arguments. JVM options (`-X...`, `-D...`,
    /// `-javaagent:`, `--add-*` and the other JVM flags) go before `-jar`;
    /// every other argument goes after the `-data <dir>` flag this
    /// application always supplies.
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
/// languages `c` and `cpp` (`.c`, `.h`, `.cpp`, `.cc`, `.cxx`, `.hpp`, `.hh`,
/// `.hxx`), the same one-server-many-languages shape `TypeScriptLspConfig`
/// uses for its four ECMAScript-family languages. C/C++ navigation is optional and fail-open: an absent or empty
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

/// Optional Markdown language-server (marksman) configuration.
///
/// Prose navigation is optional and fail-open: an absent or empty
/// `[markdown]` section keeps the server disabled without affecting
/// retrieval. marksman is reached only for an indexed prose file, so it
/// needs `[index] prose = true` as well. Its stdio mode is the `server`
/// subcommand (`marksman server`), which the adapter forces before the
/// configured `args`. The shared `lsp_timeout_seconds` applies here too.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MarkdownLspConfig {
    /// Path to the standalone marksman executable. `None` (the default)
    /// keeps prose navigation disabled. An empty or whitespace-only value is
    /// rejected by validation rather than silently counting as enabled.
    pub path: Option<String>,
    /// Additional arguments passed to marksman after `server`.
    pub args: Vec<String>,
    /// Explicitly disable the marksman server even when a path is configured.
    pub disabled: bool,
}

impl MarkdownLspConfig {
    /// Whether the Markdown language server is enabled.
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
            embedding_api_key_env: None,
            reranker_api_key_env: None,
            data_dir: base.join("local-code-intelligence"),
            port: DEFAULT_PORT,
            default_top_k: 8,
            embedding_batch_size: 8,
            embedding_query_template: DEFAULT_EMBEDDING_QUERY_TEMPLATE.into(),
            embedding_document_template: DEFAULT_EMBEDDING_DOCUMENT_TEMPLATE.into(),
            embedding_timeout_seconds: 120,
            reranker_timeout_seconds: 120,
            readiness_timeout_seconds: 15,
            ripgrep_path: "rg".into(),
            semantic_candidate_count: 40,
            lexical_candidate_count: 40,
            rerank_candidate_count: 24,
            rerank_document_max_bytes: 4096,
            rrf_k: 60.0,
            watch_poll_milliseconds: 2000,
            watch_debounce_milliseconds: 750,
            rust_analyzer_path: "rust-analyzer".into(),
            rust_analyzer_trusted: false,
            lsp_timeout_seconds: 60,
            lsp_start_timeout_seconds: 0,
            lsp_candidate_count: 40,
            lsp_search_budget_ms: 1500,
            lsp_idle_timeout_seconds: 1800,
            lsp_stuck_seconds: 900,
            csharp: CSharpLspConfig::default(),
            typescript: TypeScriptLspConfig::default(),
            python: PythonLspConfig::default(),
            go: GoLspConfig::default(),
            java: JavaLspConfig::default(),
            clangd: ClangdLspConfig::default(),
            markdown: MarkdownLspConfig::default(),
            index: IndexConfig::default(),
            channels: ChannelsConfig::default(),
            fusion: FusionConfig::default(),
            docs_lane: DocsLaneConfig::default(),
            search_profiles: BTreeMap::new(),
            traverse: TraverseConfig::default(),
            recipes: RecipesConfig::default(),
            compose: ComposeConfig::default(),
            system_one: None,
            rescorer: Rescorer::default(),
            run_profile: RunProfileConfig::default(),
        }
    }
}

impl Config {
    /// The longest request, idle, or stuck LSP timeout accepted: one day.
    /// Request timeouts are added to the clock; absurd values cannot be.
    pub const MAX_LSP_TIMEOUT_SECONDS: u64 = 86_400;

    pub fn load(path: Option<&Path>) -> Result<Self> {
        let mut config: Self = match path {
            Some(p) => toml::from_str(&std::fs::read_to_string(p).context("read configuration")?)?,
            None => Self::default(),
        };
        config.trim_model_settings();
        config.validate()?;
        Ok(config)
    }

    fn trim_model_settings(&mut self) {
        self.embedding_url = self.embedding_url.trim().to_owned();
        self.embedding_model = self.embedding_model.trim().to_owned();
        self.reranker_url = self.reranker_url.trim().to_owned();
        self.reranker_model = self.reranker_model.trim().to_owned();
        if let Some(system_one) = &mut self.system_one {
            system_one.trim();
        }
    }

    /// `[run_profile.walk]`: the corroboration discount is a share, and claim
    /// groups are not defined under reciprocal rank fusion (its sources stay
    /// per map).
    fn validate_run_profile(&self) -> Result<()> {
        let walk = &self.run_profile.walk;
        ensure!(
            (0.0..=1.0).contains(&walk.grep_corroboration),
            "run_profile.walk.grep_corroboration must be a number from 0 to 1 (got {})",
            walk.grep_corroboration
        );
        ensure!(
            !(walk.groups == WalkGroups::Claim && self.traverse.fusion.uses_reach()),
            "run_profile.walk.groups = \"claim\" cannot be combined with traverse.fusion = \"{}\": the fusion sources stay per map; use traverse.fusion = \"boost\" or groups = \"tool\"",
            self.traverse.fusion.as_str()
        );
        Ok(())
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
        self.docs_lane.validate()?;
        self.traverse.validate()?;
        self.validate_run_profile()?;
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
            self.lsp_timeout_seconds <= Self::MAX_LSP_TIMEOUT_SECONDS,
            "lsp_timeout_seconds must be 1..={} (got {})",
            Self::MAX_LSP_TIMEOUT_SECONDS,
            self.lsp_timeout_seconds
        );
        ensure!(
            self.lsp_start_timeout_seconds <= Self::MAX_LSP_TIMEOUT_SECONDS,
            "lsp_start_timeout_seconds must be 0..={} (got {}); 0 means the start is bounded by lsp_timeout_seconds",
            Self::MAX_LSP_TIMEOUT_SECONDS,
            self.lsp_start_timeout_seconds
        );
        ensure!(
            self.lsp_idle_timeout_seconds <= Self::MAX_LSP_TIMEOUT_SECONDS,
            "lsp_idle_timeout_seconds must be 0..={} (got {}); 0 means never release",
            Self::MAX_LSP_TIMEOUT_SECONDS,
            self.lsp_idle_timeout_seconds
        );
        ensure!(
            self.lsp_stuck_seconds <= Self::MAX_LSP_TIMEOUT_SECONDS,
            "lsp_stuck_seconds must be 0..={} (got {}); 0 means never replace",
            Self::MAX_LSP_TIMEOUT_SECONDS,
            self.lsp_stuck_seconds
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
        if !self.markdown.disabled
            && let Some(path) = &self.markdown.path
        {
            ensure!(
                !path.trim().is_empty(),
                "markdown.path must be nonempty when set; omit it or set markdown.disabled = true to keep prose navigation disabled"
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
            ensure!(
                resolved.docs_top_k <= DocsLaneConfig::MAX_TOP_K,
                "search_profiles.{name}: docs_top_k must be 0..={} (got {})",
                DocsLaneConfig::MAX_TOP_K,
                resolved.docs_top_k
            );
            ensure!(
                profile.response == Some(McpResponse::Compact)
                    || (profile.compact_max_hit_lines.is_none()
                        && profile.compact_max_response_chars.is_none()),
                "search_profiles.{name}: compact_max_hit_lines and compact_max_response_chars apply only with response = \"compact\""
            );
            if let Some(lines) = profile.compact_max_hit_lines {
                ensure!(
                    (1..=COMPACT_MAX_HIT_LINES_LIMIT).contains(&lines),
                    "search_profiles.{name}: compact_max_hit_lines must be 1..={COMPACT_MAX_HIT_LINES_LIMIT} (got {lines})"
                );
            }
            if let Some(chars) = profile.compact_max_response_chars {
                ensure!(
                    COMPACT_MAX_RESPONSE_CHARS_RANGE.contains(&chars),
                    "search_profiles.{name}: compact_max_response_chars must be {}..={} (got {chars})",
                    COMPACT_MAX_RESPONSE_CHARS_RANGE.start(),
                    COMPACT_MAX_RESPONSE_CHARS_RANGE.end()
                );
            }
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
        for (name, value) in [
            ("embedding_url", &self.embedding_url),
            ("reranker_url", &self.reranker_url),
        ] {
            let url = reqwest::Url::parse(value)?;
            ensure!(
                matches!(url.scheme(), "http" | "https"),
                "model URL must use HTTP(S)"
            );
            // The effective port: an explicit one, else the scheme's default.
            ensure!(
                url.port_or_known_default() != Some(GENERATION_MODEL_PORT),
                "`{name}` uses port {GENERATION_MODEL_PORT}, which is reserved for the generation model; LCI never contacts it; choose another port"
            );
        }
        ensure!(
            !self.embedding_model.trim().is_empty(),
            "embedding_model must be nonempty"
        );
        ensure!(
            !self.reranker_model.trim().is_empty(),
            "reranker_model must be nonempty"
        );
        validate_api_key_env(
            "embedding_api_key_env",
            self.embedding_api_key_env.as_deref(),
        )?;
        validate_api_key_env("reranker_api_key_env", self.reranker_api_key_env.as_deref())?;
        if let Some(system_one) = &self.system_one {
            system_one.validate()?;
        }
        if self.rescorer == Rescorer::SystemOne {
            let Some(system_one) = &self.system_one else {
                bail!("rescorer = \"system_one\" needs a [system_one] section");
            };
            ensure!(
                system_one.provider == crate::provider::ProviderKind::SystemOneWire,
                "rescorer = \"system_one\" needs system_one.provider = \"system_one_wire\""
            );
        }
        Ok(())
    }

    /// The query-time settings a search runs with when it names no profile:
    /// `[channels]` and `rerank_candidate_count` as configured.
    pub fn base_search_settings(&self) -> SearchSettings {
        SearchSettings {
            profile: None,
            channels: self.channels,
            rerank_candidate_count: self.rerank_candidate_count,
            docs_top_k: self.docs_lane.top_k,
            docs_reranker: self.docs_lane.reranker,
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

    /// How the MCP endpoint of `profile` presents itself: `None` (`/mcp`) and
    /// a profile that sets none of `tools`, `response` and the compact limits
    /// give every tool and the full result. Presentation only: it is not part
    /// of [`SearchSettings`] and never changes what a search retrieves. A name
    /// that is not configured is an error, as in [`Self::search_settings`].
    pub fn mcp_presentation(&self, profile: Option<&str>) -> Result<McpPresentation> {
        let Some(name) = profile else {
            return Ok(McpPresentation::default());
        };
        // Reuses the unknown-name error of `search_settings`.
        self.search_settings(Some(name))?;
        Ok(self.search_profiles[name].mcp_presentation())
    }

    /// `overrides` applied over the top-level settings, under `name`.
    fn resolve_search_profile(&self, name: &str, overrides: &SearchProfile) -> SearchSettings {
        let channels = ChannelsConfig {
            semantic: overrides.semantic.unwrap_or(self.channels.semantic),
            lexical: overrides.lexical.unwrap_or(self.channels.lexical),
            lsp: overrides.lsp.unwrap_or(self.channels.lsp),
            reranker: overrides.reranker.unwrap_or(self.channels.reranker),
        };
        SearchSettings {
            profile: Some(name.to_owned()),
            channels,
            rerank_candidate_count: overrides
                .rerank_candidate_count
                .unwrap_or(self.rerank_candidate_count),
            docs_top_k: overrides.docs_top_k.unwrap_or(self.docs_lane.top_k),
            docs_reranker: overrides.docs_reranker.unwrap_or(self.docs_lane.reranker),
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
    fn index_prose_is_off_by_default_and_a_file_can_turn_it_on() {
        let config: Config = toml::from_str(&base_toml()).unwrap();
        assert!(!config.index.prose);
        let on: Config =
            toml::from_str(&format!("{}[index]\nprose = true\n", base_toml())).unwrap();
        on.validate().unwrap();
        assert!(on.index.prose);
        assert!(
            toml::from_str::<Config>(&format!("{}[index]\nprse = true\n", base_toml())).is_err()
        );
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
    fn whitespace_only_model_names_are_rejected_during_validation() {
        for (name, config) in [
            (
                "embedding_model",
                Config {
                    embedding_model: " \t\n ".to_owned(),
                    ..Config::default()
                },
            ),
            (
                "reranker_model",
                Config {
                    reranker_model: " \t\n ".to_owned(),
                    ..Config::default()
                },
            ),
        ] {
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(name), "{error}");
            assert!(error.contains("nonempty"), "{error}");
        }
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
    fn an_lsp_timeout_that_cannot_be_added_to_the_clock_is_rejected() {
        for seconds in [Config::MAX_LSP_TIMEOUT_SECONDS + 1, u64::MAX] {
            let mut config: Config = toml::from_str(&base_toml()).unwrap();
            config.lsp_timeout_seconds = seconds;
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("lsp_timeout_seconds"), "{error}");
        }
        let mut config: Config = toml::from_str(&base_toml()).unwrap();
        config.lsp_timeout_seconds = Config::MAX_LSP_TIMEOUT_SECONDS;
        config.validate().unwrap();
    }

    #[test]
    fn readiness_timeout_defaults_to_fifteen_seconds_and_keeps_its_range() {
        assert_eq!(Config::default().readiness_timeout_seconds, 15);
        let config: Config = toml::from_str(&base_toml()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.readiness_timeout_seconds, 15);
        for seconds in [0, 31] {
            let invalid = Config {
                readiness_timeout_seconds: seconds,
                ..Config::default()
            };
            assert!(invalid.validate().is_err(), "{seconds} s is out of range");
        }
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
    fn lsp_idle_timeout_seconds_outside_its_range_is_rejected() {
        for seconds in [Config::MAX_LSP_TIMEOUT_SECONDS + 1, u64::MAX] {
            let mut config: Config = toml::from_str(&base_toml()).unwrap();
            config.lsp_idle_timeout_seconds = seconds;
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("lsp_idle_timeout_seconds"), "{error}");
        }
        for seconds in [0, 1, Config::MAX_LSP_TIMEOUT_SECONDS] {
            let mut config: Config = toml::from_str(&base_toml()).unwrap();
            config.lsp_idle_timeout_seconds = seconds;
            config.validate().unwrap();
        }
    }

    #[test]
    fn lsp_start_timeout_seconds_outside_its_range_is_rejected() {
        let config: Config = toml::from_str(&base_toml()).unwrap();
        assert_eq!(config.lsp_start_timeout_seconds, 0);
        for seconds in [Config::MAX_LSP_TIMEOUT_SECONDS + 1, u64::MAX] {
            let mut config: Config = toml::from_str(&base_toml()).unwrap();
            config.lsp_start_timeout_seconds = seconds;
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("lsp_start_timeout_seconds"), "{error}");
        }
        for seconds in [0, 1, Config::MAX_LSP_TIMEOUT_SECONDS] {
            let mut config: Config = toml::from_str(&base_toml()).unwrap();
            config.lsp_start_timeout_seconds = seconds;
            config.validate().unwrap();
        }
    }

    #[test]
    fn lsp_stuck_seconds_outside_its_range_is_rejected() {
        for seconds in [Config::MAX_LSP_TIMEOUT_SECONDS + 1, u64::MAX] {
            let mut config: Config = toml::from_str(&base_toml()).unwrap();
            config.lsp_stuck_seconds = seconds;
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains("lsp_stuck_seconds"), "{error}");
        }
        for seconds in [0, 1, Config::MAX_LSP_TIMEOUT_SECONDS] {
            let mut config: Config = toml::from_str(&base_toml()).unwrap();
            config.lsp_stuck_seconds = seconds;
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
    fn lsp_stuck_seconds_defaults_to_fifteen_minutes_allows_zero_and_round_trips() {
        let config: Config = toml::from_str(&base_toml()).unwrap();
        config.validate().unwrap();
        assert_eq!(config.lsp_stuck_seconds, 900);
        assert_eq!(Config::default().lsp_stuck_seconds, 900);

        // 0 means never replace; it is a valid setting, not an error.
        for seconds in [0, 1, 3600] {
            let configured: Config =
                toml::from_str(&format!("{}lsp_stuck_seconds = {seconds}\n", base_toml())).unwrap();
            configured.validate().unwrap();
            assert_eq!(configured.lsp_stuck_seconds, seconds);

            let reloaded: Config = toml::from_str(&toml::to_string(&configured).unwrap()).unwrap();
            reloaded.validate().unwrap();
            assert_eq!(reloaded.lsp_stuck_seconds, seconds);
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
    fn markdown_section_enables_server_and_validates_the_path() {
        let default: Config = toml::from_str(&base_toml()).unwrap();
        assert!(!default.markdown.enabled() && default.markdown.path.is_none());
        let on: Config =
            toml::from_str(&format!("{}[markdown]\npath = \"marksman\"\n", base_toml())).unwrap();
        on.validate().unwrap();
        assert!(on.markdown.enabled());
        let off: Config = toml::from_str(&format!(
            "{}[markdown]\npath = \"marksman\"\ndisabled = true\n",
            base_toml()
        ))
        .unwrap();
        assert!(!off.markdown.enabled());
        let blank: Config =
            toml::from_str(&format!("{}[markdown]\npath = \"   \"\n", base_toml())).unwrap();
        let error = blank.validate().unwrap_err();
        assert!(error.to_string().contains("markdown.path"));
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

    /// A configuration with `key` set to `url` and everything else default.
    fn config_with_url(key: &str, url: &str) -> Config {
        toml::from_str(&format!("{}{key} = '{url}'\n", base_toml())).unwrap()
    }

    #[test]
    fn a_model_url_on_the_generation_models_port_is_rejected_for_any_host() {
        assert_eq!(GENERATION_MODEL_PORT, 8765);
        for key in ["embedding_url", "reranker_url"] {
            for url in [
                "http://127.0.0.1:8765/v1",
                "http://localhost:8765/v1",
                "http://LocalHost:8765/v1",
                "http://[::1]:8765/v1",
                "http://127.1:8765/v1",
                "http://0.0.0.0:8765/rerank",
                "http://192.168.1.20:8765/v1",
                "http://models.example.com:8765/v1",
                "https://models.example.com:8765",
                "http://user:secret@models.example.com:8765/v1",
                "http://models.example.com:08765/v1",
            ] {
                let error = config_with_url(key, url)
                    .validate()
                    .unwrap_err()
                    .to_string();
                assert!(error.contains(&format!("`{key}`")), "{url}: {error}");
                assert!(error.contains("port 8765"), "{url}: {error}");
                assert!(error.contains("generation model"), "{url}: {error}");
                assert!(error.contains("never contacts it"), "{url}: {error}");
                // The message never repeats the URL, which may hold credentials.
                assert!(!error.contains("secret"), "{url}: {error}");
            }
        }
    }

    #[test]
    fn the_rejection_names_the_key_that_holds_the_port() {
        let both = Config {
            data_dir: std::env::temp_dir(),
            embedding_url: "http://localhost:8766/v1".into(),
            reranker_url: "http://localhost:8765/rerank".into(),
            ..Config::default()
        };
        let error = both.validate().unwrap_err().to_string();
        assert!(error.contains("`reranker_url`"), "{error}");
        assert!(!error.contains("`embedding_url`"), "{error}");
    }

    #[test]
    fn a_model_url_on_another_port_or_with_no_explicit_port_is_accepted() {
        for key in ["embedding_url", "reranker_url"] {
            for url in [
                "http://localhost:8766/v1",
                "http://localhost:8767/rerank",
                "http://localhost:8764/v1",
                "http://localhost:8768/v1",
                "http://localhost:18765/v1",
                // No explicit port: the scheme's default (80 or 443) applies.
                "http://localhost/v1",
                "https://models.example.com/v1",
                "http://localhost:80/v1",
                "https://models.example.com:443",
                // 8765 only in the path, query or credentials is not a port.
                "http://localhost/8765",
                "http://localhost:8766/v1?port=8765",
                "http://user:8765@localhost:8766/v1",
                "http://models-8765.example.com/v1",
            ] {
                config_with_url(key, url)
                    .validate()
                    .unwrap_or_else(|error| {
                        panic!("{key} = {url} should validate: {error}");
                    });
            }
        }
        Config::default().validate().unwrap();
        assert_eq!(Config::default().embedding_url, "http://localhost:8766/v1");
        assert_eq!(
            Config::default().reranker_url,
            "http://localhost:8767/rerank"
        );
    }

    /// The port rule adds no new failure for a URL that does not parse or
    /// does not use HTTP(S): those are rejected exactly as before.
    #[test]
    fn unparsable_and_non_http_model_urls_keep_their_existing_errors() {
        let error = config_with_url("embedding_url", "not a url")
            .validate()
            .unwrap_err()
            .to_string();
        assert!(!error.contains("8765"), "{error}");
        let error = config_with_url("reranker_url", "")
            .validate()
            .unwrap_err()
            .to_string();
        assert!(!error.contains("8765"), "{error}");
        // The scheme check still comes first, even on the reserved port.
        let error = config_with_url("embedding_url", "ftp://localhost:8765/v1")
            .validate()
            .unwrap_err()
            .to_string();
        assert!(error.contains("must use HTTP(S)"), "{error}");
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
            docs_top_k: 2,
            docs_reranker: false,
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
    fn mcp_presentation_is_full_unless_a_profile_asks_for_search_tools_or_a_compact_result() {
        let toml_text = format!(
            "{}[search_profiles.plain]\nreranker = false\n\
             [search_profiles.tools_only]\ntools = \"search\"\n\
             [search_profiles.compact]\nresponse = \"compact\"\n\
             [search_profiles.tuned]\ntools = \"search\"\nresponse = \"compact\"\n\
             compact_max_hit_lines = 12\ncompact_max_response_chars = 5000\n\
             [search_profiles.fullagain]\ntools = \"full\"\nresponse = \"full\"\n",
            base_toml()
        );
        let config: Config = toml::from_str(&toml_text).unwrap();
        config.validate().unwrap();
        let full = McpPresentation::default();
        assert_eq!(full.tools, McpToolList::Full);
        assert_eq!(full.compact, None);
        assert_eq!(config.mcp_presentation(None).unwrap(), full);
        assert_eq!(config.mcp_presentation(Some("plain")).unwrap(), full);
        assert_eq!(config.mcp_presentation(Some("fullagain")).unwrap(), full);
        assert_eq!(
            config.mcp_presentation(Some("tools_only")).unwrap(),
            McpPresentation {
                tools: McpToolList::Search,
                compact: None,
            }
        );
        // The documented defaults: 40 lines per hit, 16,000 characters.
        assert_eq!(DEFAULT_COMPACT_MAX_HIT_LINES, 40);
        assert_eq!(DEFAULT_COMPACT_MAX_RESPONSE_CHARS, 16_000);
        assert_eq!(
            config.mcp_presentation(Some("compact")).unwrap(),
            McpPresentation {
                tools: McpToolList::Full,
                compact: Some(CompactLimits {
                    max_hit_lines: 40,
                    max_response_chars: 16_000,
                }),
            }
        );
        assert_eq!(
            config.mcp_presentation(Some("tuned")).unwrap(),
            McpPresentation {
                tools: McpToolList::Search,
                compact: Some(CompactLimits {
                    max_hit_lines: 12,
                    max_response_chars: 5000,
                }),
            }
        );
        let error = config.mcp_presentation(Some("nope")).unwrap_err();
        assert!(
            error.to_string().contains("unknown search profile"),
            "{error}"
        );
        // The presentation keys never reach the retrieval settings.
        assert_eq!(
            config.search_settings(Some("tuned")).unwrap().channels,
            config.base_search_settings().channels
        );
        assert_eq!(
            config
                .search_settings(Some("tuned"))
                .unwrap()
                .rerank_candidate_count,
            config.rerank_candidate_count
        );
    }

    #[test]
    fn invalid_presentation_keys_of_a_search_profile_are_rejected_with_the_profile_name() {
        for (keys, expected) in [
            ("tools = \"everything\"", None),
            ("response = \"tiny\"", None),
            (
                "response = \"compact\"\ncompact_max_hit_lines = 0",
                Some("compact_max_hit_lines"),
            ),
            (
                "response = \"compact\"\ncompact_max_hit_lines = 1001",
                Some("compact_max_hit_lines"),
            ),
            (
                "response = \"compact\"\ncompact_max_response_chars = 999",
                Some("compact_max_response_chars"),
            ),
            (
                "response = \"compact\"\ncompact_max_response_chars = 1000001",
                Some("compact_max_response_chars"),
            ),
            ("compact_max_hit_lines = 20", Some("only with response")),
            (
                "response = \"full\"\ncompact_max_response_chars = 5000",
                Some("only with response"),
            ),
        ] {
            let toml_text = format!("{}[search_profiles.p]\n{keys}\n", base_toml());
            match (toml::from_str::<Config>(&toml_text), expected) {
                (Err(_), None) => {}
                (Ok(config), Some(expected)) => {
                    let error = config.validate().unwrap_err().to_string();
                    assert!(error.contains("search_profiles.p"), "{keys}: {error}");
                    assert!(error.contains(expected), "{keys}: {error}");
                }
                (parsed, expected) => {
                    panic!("{keys}: unexpected {parsed:?} for {expected:?}")
                }
            }
        }
        // Bounds are inclusive.
        for keys in [
            "response = \"compact\"\ncompact_max_hit_lines = 1\ncompact_max_response_chars = 1000",
            "response = \"compact\"\ncompact_max_hit_lines = 1000\ncompact_max_response_chars = 1000000",
        ] {
            let toml_text = format!("{}[search_profiles.p]\n{keys}\n", base_toml());
            let config: Config = toml::from_str(&toml_text).unwrap();
            config.validate().unwrap();
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

    #[test]
    fn traverse_is_off_by_default_and_invisible_while_it_is_the_default() {
        let config: Config = toml::from_str(&base_toml()).unwrap();
        config.validate().unwrap();
        assert!(!config.traverse.enabled);
        assert_eq!(config.traverse, TraverseConfig::default());
        // Not in the serialized configuration, so `config` and the layer
        // reports are what they were before the section existed.
        let value = serde_json::to_value(&config).unwrap();
        assert!(value.get("traverse").is_none(), "{value}");
        // A changed section is reported, with every key.
        let on: Config =
            toml::from_str(&format!("{}[traverse]\nenabled = true\n", base_toml())).unwrap();
        on.validate().unwrap();
        let value = serde_json::to_value(&on).unwrap();
        assert_eq!(value["traverse"]["enabled"], true);
        assert_eq!(value["traverse"]["scip_index"], "index.scip");
        assert_eq!(value["traverse"]["max_pops"], 200);
        assert_eq!(value["traverse"]["seed_k"], 8);
        assert_eq!(value["traverse"]["corroboration_boost"], 0.1);
        assert_eq!(value["traverse"]["fusion"], "boost");
        // The fusion is chosen by name; a changed one is reported as written.
        let rrf: Config = toml::from_str(&format!(
            "{}[traverse]
fusion = \"rrf\"
",
            base_toml()
        ))
        .unwrap();
        rrf.validate().unwrap();
        assert_eq!(rrf.traverse.fusion, TraverseFusion::Rrf);
        assert_eq!(
            serde_json::to_value(&rrf).unwrap()["traverse"]["fusion"],
            "rrf"
        );
        assert_eq!("rrf".parse::<TraverseFusion>(), Ok(TraverseFusion::Rrf));
        assert_eq!("boost".parse::<TraverseFusion>(), Ok(TraverseFusion::Boost));
        let log_odds: Config = toml::from_str(&format!(
            "{}[traverse]
fusion = \"log_odds\"
",
            base_toml()
        ))
        .unwrap();
        log_odds.validate().unwrap();
        assert_eq!(log_odds.traverse.fusion, TraverseFusion::LogOdds);
        assert_eq!(
            serde_json::to_value(&log_odds).unwrap()["traverse"]["fusion"],
            "log_odds"
        );
        assert_eq!(
            "log_odds".parse::<TraverseFusion>(),
            Ok(TraverseFusion::LogOdds)
        );
        assert_eq!(TraverseFusion::LogOdds.as_str(), "log_odds");
        // Pass-through is off unless set, and only a boolean sets it.
        assert_eq!(value["traverse"]["pass_through"], false);
        let free: Config =
            toml::from_str(&format!("{}[traverse]\npass_through = true\n", base_toml())).unwrap();
        free.validate().unwrap();
        assert!(free.traverse.pass_through);
        let text = format!("{}[traverse]\npass_through = \"yes\"\n", base_toml());
        assert!(toml::from_str::<Config>(&text).is_err());
        // The reranker frontier is off unless set; the floor defaults to 0.25.
        assert_eq!(value["traverse"]["rerank_frontier"], false);
        assert_eq!(value["traverse"]["rerank_floor"], 0.25);
        let text = format!(
            "{}[traverse]\nrerank_frontier = true\nrerank_floor = 0.5\n",
            base_toml()
        );
        let reranked: Config = toml::from_str(&text).unwrap();
        reranked.validate().unwrap();
        assert!(reranked.traverse.rerank_frontier);
        assert_eq!(reranked.traverse.rerank_floor, 0.5);
        let text = format!("{}[traverse]\nrerank_frontier = \"yes\"\n", base_toml());
        assert!(toml::from_str::<Config>(&text).is_err());
        // The reranker's scores are probabilities unless the file says logits,
        // and no model-call cap is set unless it is given.
        assert_eq!(value["traverse"]["rerank_scores"], "probability");
        assert!(value["traverse"].get("max_model_calls").is_none());
        let text = format!(
            "{}[traverse]\nrerank_scores = \"logit\"\nmax_model_calls = 5\n",
            base_toml()
        );
        let logits: Config = toml::from_str(&text).unwrap();
        logits.validate().unwrap();
        assert_eq!(logits.traverse.rerank_scores, TraverseRerankScores::Logit);
        assert_eq!(logits.traverse.max_model_calls, Some(5));
        let json = serde_json::to_value(&logits).unwrap();
        assert_eq!(json["traverse"]["rerank_scores"], "logit");
        assert_eq!(json["traverse"]["max_model_calls"], 5);
        for bad in ["nope", "Logit", ""] {
            let text = format!("{}[traverse]\nrerank_scores = \"{bad}\"\n", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad:?}");
            assert!(bad.parse::<TraverseRerankScores>().is_err(), "{bad:?}");
        }
        assert_eq!(
            "probability".parse::<TraverseRerankScores>(),
            Ok(TraverseRerankScores::Probability)
        );
        // The score bound is on unless switched off.
        assert_eq!(value["traverse"]["score_bound"], true);
        let text = format!("{}[traverse]\nscore_bound = false\n", base_toml());
        assert!(
            !toml::from_str::<Config>(&text)
                .unwrap()
                .traverse
                .score_bound
        );
    }

    #[test]
    fn the_removed_edge_bandit_keys_are_refused_by_name() {
        for key in [
            "edge_bandit",
            "bandit_rate",
            "bandit_signal",
            "bandit_threshold",
            "bandit_every",
            "bandit_sample",
            "bandit_seed",
            "bandit_fixed",
        ] {
            let text = format!("{}[traverse]\n{key} = 1\n", base_toml());
            let error = toml::from_str::<Config>(&text).unwrap_err().to_string();
            assert!(error.contains(key), "{key}: {error}");
        }
    }

    #[test]
    fn the_first_stage_keys_default_to_the_measured_values_and_parse() {
        let defaults = TraverseConfig::default();
        assert_eq!(defaults.first_stage, TraverseFirstStage::Append);
        assert_eq!(defaults.first_k, 10);
        assert!(defaults.answer_seeds && defaults.exact_seeds);
        assert!(defaults.policies.is_empty());
        let on: Config = toml::from_str(&format!(
            "{}[traverse]
first_stage = \"interleave\"
first_k = 0
answer_seeds = true
exact_seeds = true
",
            base_toml()
        ))
        .unwrap();
        on.validate().unwrap();
        assert_eq!(on.traverse.first_stage, TraverseFirstStage::Interleave);
        assert_eq!(on.traverse.first_k, 0);
        assert!(on.traverse.answer_seeds && on.traverse.exact_seeds);
        let json = serde_json::to_value(&on).unwrap();
        assert_eq!(json["traverse"]["first_stage"], "interleave");
        assert_eq!(json["traverse"]["first_k"], 0);
        let append: Config = toml::from_str(&format!(
            "{}[traverse]
first_stage = \"append\"
",
            base_toml()
        ))
        .unwrap();
        append.validate().unwrap();
        assert_eq!(append.traverse.first_stage, TraverseFirstStage::Append);
        assert_eq!(append.traverse.first_k, 10);
        for bad in ["nope", "Append", ""] {
            let text = format!(
                "{}[traverse]
first_stage = \"{bad}\"
",
                base_toml()
            );
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad:?}");
            assert!(bad.parse::<TraverseFirstStage>().is_err(), "{bad:?}");
        }
        assert_eq!(
            "interleave".parse::<TraverseFirstStage>(),
            Ok(TraverseFirstStage::Interleave)
        );
        let mut config = Config {
            data_dir: std::env::temp_dir(),
            ..Config::default()
        };
        config.traverse.first_k = TraverseConfig::MAX_FIRST_K_LIMIT + 1;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("first_k"), "{error}");
    }

    #[test]
    fn the_policy_tables_parse_layer_over_each_other_and_are_checked() {
        let on: Config = toml::from_str(&format!(
            "{}[traverse]
[traverse.policies.impact]
walk = true
first_k = 12
extra_maps = [\"exact.closure\"]

[traverse.policies.where-to]
walk_min_answers = 14
",
            base_toml()
        ))
        .unwrap();
        on.validate().unwrap();
        let impact = &on.traverse.policies["impact"];
        assert_eq!(impact.walk, Some(true));
        assert_eq!(impact.first_k, Some(12));
        assert_eq!(impact.seed_k, None);
        assert_eq!(on.traverse.policies["where-to"].walk_min_answers, Some(14));
        let json = serde_json::to_value(&on).unwrap();
        assert_eq!(json["traverse"]["policies"]["impact"]["first_k"], 12);
        assert!(
            json["traverse"]["policies"]["impact"]
                .get("seed_k")
                .is_none()
        );

        let built_in = TraversePolicy {
            walk: Some(false),
            first_k: Some(20),
            seed_k: Some(20),
            ..TraversePolicy::default()
        };
        let merged = impact.over(&built_in);
        assert_eq!(merged.walk, Some(true));
        assert_eq!(merged.first_k, Some(12));
        assert_eq!(merged.seed_k, Some(20));
        assert_eq!(
            merged.extra_maps.as_deref(),
            Some(&["exact.closure".to_owned()][..])
        );

        for (text, key) in [
            ("[traverse.policies.nope]\nwalk = true\n", "unknown policy"),
            (
                "[traverse.policies.flow]\nfirst_k = 201\n",
                "policies.flow.first_k",
            ),
            (
                "[traverse.policies.flow]\nseed_k = 0\n",
                "policies.flow.seed_k",
            ),
            (
                "[traverse.policies.flow]\nwalk_min_answers = 241\n",
                "policies.flow.walk_min_answers",
            ),
            (
                "[traverse.policies.flow]\nextra_maps = [\" \"]\n",
                "extra_maps",
            ),
        ] {
            let config: Config =
                toml::from_str(&format!("{}[traverse]\n{text}", base_toml())).unwrap();
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(key), "{text}: {error}");
        }
        let unknown_key = format!("{}[traverse.policies.flow]\nwalking = true\n", base_toml());
        assert!(toml::from_str::<Config>(&unknown_key).is_err());
    }

    #[test]
    fn the_recipe_and_compose_sections_are_off_by_default_and_parse() {
        let plain: Config = toml::from_str(&base_toml()).unwrap();
        assert_eq!(plain.recipes, RecipesConfig::default());
        assert!(!plain.recipes.enabled && !plain.recipes.flow_judge && !plain.compose.enabled);
        // Left out of the serialized configuration while default.
        let json = serde_json::to_value(&plain).unwrap();
        assert!(json.get("recipes").is_none() && json.get("compose").is_none());
        let on: Config = toml::from_str(&format!(
            "{}[recipes]
enabled = true
[compose]
enabled = true
",
            base_toml()
        ))
        .unwrap();
        on.validate().unwrap();
        assert!(on.recipes.enabled && on.compose.enabled && !on.recipes.flow_judge);
        let json = serde_json::to_value(&on).unwrap();
        assert_eq!(json["recipes"]["enabled"], true);
        assert_eq!(json["compose"]["enabled"], true);
        for bad in [
            "[recipes]
nope = 1
",
            "[compose]
nope = 1
",
        ] {
            assert!(
                toml::from_str::<Config>(&format!("{}{bad}", base_toml())).is_err(),
                "{bad}"
            );
        }
    }

    #[test]
    fn the_beam_defaults_to_what_the_walk_always_used_and_parses() {
        assert_eq!(TraverseConfig::default().beam, 16);
        // The default beam leaves the serialized traverse section alone.
        let plain: Config = toml::from_str(&base_toml()).unwrap();
        assert!(
            serde_json::to_value(&plain)
                .unwrap()
                .get("traverse")
                .is_none()
        );
        let wide: Config = toml::from_str(&format!(
            "{}[traverse]
beam = 64
",
            base_toml()
        ))
        .unwrap();
        wide.validate().unwrap();
        assert_eq!(wide.traverse.beam, 64);
        assert_eq!(serde_json::to_value(&wide).unwrap()["traverse"]["beam"], 64);
    }

    #[test]
    fn traverse_rejects_unknown_keys_and_out_of_range_limits() {
        let text = format!("{}[traverse]\nnope = 1\n", base_toml());
        assert!(toml::from_str::<Config>(&text).is_err());
        // An unknown fusion is refused when the file is read, not defaulted.
        for fusion in ["nope", "RRF", ""] {
            let text = format!(
                "{}[traverse]
fusion = \"{fusion}\"
",
                base_toml()
            );
            assert!(toml::from_str::<Config>(&text).is_err(), "{fusion:?}");
            assert!(fusion.parse::<TraverseFusion>().is_err(), "{fusion:?}");
        }
        type Set = fn(&mut TraverseConfig);
        let bad: [(&str, Set); 25] = [
            ("scip_index", |t| t.scip_index = " ".into()),
            ("max_pops", |t| t.max_pops = 0),
            ("max_pops", |t| {
                t.max_pops = TraverseConfig::MAX_POPS_LIMIT + 1;
            }),
            ("max_depth", |t| t.max_depth = 0),
            ("max_est_tokens", |t| t.max_est_tokens = 0),
            ("deadline_ms", |t| t.deadline_ms = 0),
            ("max_stops", |t| {
                t.max_stops = TraverseConfig::MAX_STOPS_LIMIT + 1;
            }),
            ("tau", |t| t.tau = 1.5),
            ("tau", |t| t.tau = f32::NAN),
            ("corroboration_boost", |t| t.corroboration_boost = -0.1),
            ("corroboration_boost", |t| t.corroboration_boost = f64::NAN),
            ("corroboration_boost", |t| t.corroboration_boost = 1e308),
            ("corroboration_boost", |t| {
                t.corroboration_boost = TraverseConfig::MAX_CORROBORATION_BOOST + 0.5;
            }),
            ("rerank_floor", |t| t.rerank_floor = -0.1),
            ("rerank_floor", |t| t.rerank_floor = 1.5),
            ("rerank_floor", |t| t.rerank_floor = f64::NAN),
            ("max_model_calls", |t| t.max_model_calls = Some(0)),
            ("priors", |t| t.priors = Some("table.json".into())),
            ("question_type", |t| t.question_type = Some("flow".into())),
            ("priors", |t| {
                t.priors = Some(" ".into());
                t.question_type = Some("flow".into());
            }),
            ("tie_break", |t| t.tie_break = Some(" ".into())),
            ("seed_k", |t| t.seed_k = 0),
            ("seed_k", |t| {
                t.seed_k = TraverseConfig::MAX_SEED_K_LIMIT + 1;
            }),
            ("beam", |t| t.beam = 0),
            ("beam", |t| t.beam = TraverseConfig::MAX_BEAM_LIMIT + 1),
        ];
        for (key, set) in bad {
            let mut config = Config {
                data_dir: std::env::temp_dir(),
                ..Config::default()
            };
            set(&mut config.traverse);
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(&format!("traverse.{key}")), "{key}: {error}");
        }
        let mut config = Config {
            data_dir: std::env::temp_dir(),
            ..Config::default()
        };
        config.traverse.max_stops = 0;
        config.traverse.tau = 0.0;
        for boost in [0.0, TraverseConfig::MAX_CORROBORATION_BOOST] {
            config.traverse.corroboration_boost = boost;
            config.validate().unwrap();
        }
        config.traverse.priors = Some("table.json".into());
        config.traverse.question_type = Some("flow".into());
        config.traverse.tie_break = Some("tie-break.json".into());
        config.validate().unwrap();
    }

    #[test]
    fn walk_grep_corroboration_is_validated() {
        let with = |share: f64| Config {
            data_dir: std::env::temp_dir(),
            run_profile: RunProfileConfig {
                walk: WalkProfile {
                    grep_corroboration: share,
                    ..WalkProfile::default()
                },
                ..RunProfileConfig::default()
            },
            ..Config::default()
        };
        for bad in [-0.1, 1.5, f64::NAN, f64::INFINITY] {
            let error = with(bad).validate().unwrap_err().to_string();
            assert!(
                error.contains("run_profile.walk.grep_corroboration"),
                "{bad}: {error}"
            );
        }
        for good in [0.0, 0.5, 1.0] {
            with(good).validate().unwrap();
        }
        // The file reads the keys; the defaults are today's behaviour.
        let defaults = Config::default().run_profile.walk;
        assert_eq!(defaults.groups, WalkGroups::Tool);
        assert_eq!(defaults.grep_corroboration, 1.0);
        let text = format!(
            "{}[run_profile.walk]
groups = \"claim\"
grep_corroboration = 0.5
",
            base_toml()
        );
        let read: Config = toml::from_str(&text).unwrap();
        read.validate().unwrap();
        assert_eq!(read.run_profile.walk.groups, WalkGroups::Claim);
        assert_eq!(read.run_profile.walk.grep_corroboration, 0.5);
        for bad in [
            "groups = \"Claim\"",
            "groups = \"\"",
            "groups = 1",
            "nope = 1",
            "grep_corroboration = \"half\"",
        ] {
            let text = format!(
                "{}[run_profile.walk]
{bad}
",
                base_toml()
            );
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
        assert_eq!("claim".parse::<WalkGroups>(), Ok(WalkGroups::Claim));
        assert!("both".parse::<WalkGroups>().is_err());
    }

    #[test]
    fn claim_groups_refuse_rrf_fusion() {
        let mut config = Config {
            data_dir: std::env::temp_dir(),
            ..Config::default()
        };
        config.traverse.fusion = TraverseFusion::Rrf;
        config.validate().unwrap();
        config.run_profile.walk.groups = WalkGroups::Claim;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("run_profile.walk.groups"), "{error}");
        assert!(error.contains("traverse.fusion"), "{error}");
        config.traverse.fusion = TraverseFusion::Boost;
        config.validate().unwrap();
    }

    #[test]
    fn claim_groups_refuse_log_odds_fusion() {
        let mut config = Config {
            data_dir: std::env::temp_dir(),
            ..Config::default()
        };
        config.traverse.fusion = TraverseFusion::LogOdds;
        config.validate().unwrap();
        config.run_profile.walk.groups = WalkGroups::Claim;
        let error = config.validate().unwrap_err().to_string();
        assert!(error.contains("run_profile.walk.groups"), "{error}");
        assert!(error.contains("log_odds"), "{error}");
        config.traverse.fusion = TraverseFusion::Boost;
        config.validate().unwrap();
    }

    #[test]
    fn system_one_is_off_when_absent_and_read_from_its_section() {
        let off: Config = toml::from_str(&base_toml()).unwrap();
        assert!(off.system_one.is_none());
        // Absent keys stay out of the serialized configuration.
        let json = serde_json::to_value(&off).unwrap();
        assert!(json.get("system_one").is_none());
        assert!(json.get("embedding_api_key_env").is_none());

        let on: Config = toml::from_str(&format!(
            "{}embedding_api_key_env = \"EMBED_KEY\"
[system_one]
model = \"m\"
timeout_ms = 500

[system_one.capabilities]
constraint = \"gbnf\"
",
            base_toml()
        ))
        .unwrap();
        on.validate().unwrap();
        let system_one = on.system_one.unwrap();
        assert_eq!(system_one.timeout_ms, 500);
        assert_eq!(system_one.url, "http://127.0.0.1:8080/v1");
        assert_eq!(
            system_one.capabilities.constraint,
            crate::provider::ConstraintSupport::Gbnf
        );
        assert_eq!(on.embedding_api_key_env.as_deref(), Some("EMBED_KEY"));

        let remote: Config = toml::from_str(&format!(
            "{}[system_one]
url = \"https://api.example.com/v1\"
",
            base_toml()
        ))
        .unwrap();
        assert!(
            remote
                .validate()
                .unwrap_err()
                .to_string()
                .contains("system_one.allow_remote")
        );
        let pasted: Config = toml::from_str(&format!(
            "{}reranker_api_key_env = \"sk-pasted-key\"
",
            base_toml()
        ))
        .unwrap();
        let error = pasted.validate().unwrap_err().to_string();
        assert!(
            error.contains("reranker_api_key_env") && !error.contains("sk-pasted"),
            "{error}"
        );
    }

    #[test]
    fn the_rescorer_defaults_to_the_reranker_and_system_one_needs_its_section() {
        let off: Config = toml::from_str(&base_toml()).unwrap();
        assert_eq!(off.rescorer, Rescorer::Reranker);
        assert!(
            off.channels.reranker,
            "[channels].reranker keeps its meaning"
        );
        let json = serde_json::to_value(&off).unwrap();
        assert!(
            json.get("rescorer").is_none(),
            "the default is not serialized"
        );

        let wire = "[system_one]\nprovider = \"system_one_wire\"\nurl = \"http://127.0.0.1:11435\"\nmodel = \"decider\"\ntimeout_ms = 10000\nbatch_size = 8\n";
        let on: Config =
            toml::from_str(&format!("{}rescorer = \"system_one\"\n{wire}", base_toml())).unwrap();
        on.validate().unwrap();
        assert_eq!(on.rescorer, Rescorer::SystemOne);
        assert_eq!(on.system_one.as_ref().unwrap().batch_size, 8);
        assert_eq!(serde_json::to_value(&on).unwrap()["rescorer"], "system_one");
        // A wire section alone changes nothing: the rescorer stays the reranker.
        let section_only: Config = toml::from_str(&format!("{}{wire}", base_toml())).unwrap();
        section_only.validate().unwrap();
        assert_eq!(section_only.rescorer, Rescorer::Reranker);

        for (name, toml_text, needle) in [
            (
                "no section",
                format!("{}rescorer = \"system_one\"\n", base_toml()),
                "needs a [system_one] section",
            ),
            (
                "wrong provider",
                format!(
                    "{}rescorer = \"system_one\"\n[system_one]\nmodel = \"m\"\n",
                    base_toml()
                ),
                "system_one_wire",
            ),
            (
                "remote without allow_remote",
                format!(
                    "{}rescorer = \"system_one\"\n[system_one]\nprovider = \"system_one_wire\"\nurl = \"https://wire.example.com\"\n",
                    base_toml()
                ),
                "system_one.allow_remote",
            ),
            (
                "generation port",
                format!(
                    "{}rescorer = \"system_one\"\n[system_one]\nprovider = \"system_one_wire\"\nurl = \"http://127.0.0.1:8765\"\n",
                    base_toml()
                ),
                "8765",
            ),
        ] {
            let config: Config = toml::from_str(&toml_text).unwrap();
            let error = config.validate().unwrap_err().to_string();
            assert!(error.contains(needle), "{name}: {error}");
        }
        assert!(toml::from_str::<Config>(&format!("{}rescorer = \"jev\"\n", base_toml())).is_err());
    }

    #[test]
    fn the_docs_lane_defaults_are_two_of_twelve_and_stay_out_of_the_serialized_config() {
        let config = Config {
            data_dir: std::env::temp_dir(),
            ..Config::default()
        };
        config.validate().unwrap();
        assert_eq!(
            config.docs_lane,
            DocsLaneConfig {
                enabled: true,
                top_k: 2,
                rerank_candidate_count: 12,
                semantic_candidate_count: 20,
                reranker: false,
            }
        );
        let text = toml::to_string(&config).unwrap();
        assert!(!text.contains("docs_lane"), "{text}");
        let settings = config.base_search_settings();
        assert_eq!((settings.docs_top_k, settings.docs_reranker), (2, false));

        // The docs reranker ignores `[channels].reranker`; a profile may set it.
        let reloaded: Config = toml::from_str(&format!(
            "{}[channels]\nreranker = true\n\
             [docs_lane]\ntop_k = 5\n\
             [search_profiles.inherit]\n\
             [search_profiles.split]\nreranker = false\ndocs_reranker = true\ndocs_top_k = 0\n",
            base_toml()
        ))
        .unwrap();
        reloaded.validate().unwrap();
        let base = reloaded.base_search_settings();
        assert_eq!((base.docs_top_k, base.docs_reranker), (5, false));
        assert!(base.channels.reranker);
        let inherit = reloaded.search_settings(Some("inherit")).unwrap();
        assert_eq!((inherit.docs_top_k, inherit.docs_reranker), (5, false));
        let split = reloaded.search_settings(Some("split")).unwrap();
        assert_eq!((split.docs_top_k, split.docs_reranker), (0, true));
        assert!(!split.channels.reranker);
        let text = toml::to_string(&reloaded).unwrap();
        assert!(text.contains("[docs_lane]"), "{text}");
    }

    #[test]
    fn the_docs_lane_reranker_is_its_own_switch_and_a_profile_override_wins() {
        let on: Config = toml::from_str(&format!(
            "{}[channels]\nreranker = false\n\
             [docs_lane]\nreranker = true\n\
             [search_profiles.inherit]\n\
             [search_profiles.off]\ndocs_reranker = false\n\
             [search_profiles.chan]\nreranker = true\n",
            base_toml()
        ))
        .unwrap();
        on.validate().unwrap();
        assert!(on.docs_lane.reranker);
        assert!(on.base_search_settings().docs_reranker);
        assert!(!on.base_search_settings().channels.reranker);
        assert!(on.search_settings(Some("inherit")).unwrap().docs_reranker);
        assert!(!on.search_settings(Some("off")).unwrap().docs_reranker);
        assert!(on.search_settings(Some("chan")).unwrap().docs_reranker);
        let text = toml::to_string(&on).unwrap();
        assert!(text.contains("reranker = true"), "{text}");

        let off: Config = toml::from_str(&format!(
            "{}[search_profiles.chan]\nreranker = true\n",
            base_toml()
        ))
        .unwrap();
        off.validate().unwrap();
        assert!(off.base_search_settings().channels.reranker);
        assert!(!off.base_search_settings().docs_reranker);
        assert!(!off.search_settings(Some("chan")).unwrap().docs_reranker);
    }

    #[test]
    fn the_docs_lane_ranges_are_checked_and_unknown_keys_rejected() {
        for (keys, accepted) in [
            ("top_k = 0", true),
            ("top_k = 10", true),
            ("top_k = 11", false),
            ("rerank_candidate_count = 1", true),
            ("rerank_candidate_count = 200", true),
            ("rerank_candidate_count = 0", false),
            ("rerank_candidate_count = 201", false),
            ("semantic_candidate_count = 1", true),
            ("semantic_candidate_count = 200", true),
            ("semantic_candidate_count = 0", false),
            ("semantic_candidate_count = 201", false),
            ("enabled = false", true),
        ] {
            let config: Config =
                toml::from_str(&format!("{}[docs_lane]\n{keys}\n", base_toml())).unwrap();
            let result = config.validate();
            // Only the key is printed: CodeQL reads any validate() error as possibly holding a secret.
            assert_eq!(result.is_ok(), accepted, "{keys}");
            if let Err(error) = result {
                assert!(error.to_string().contains("docs_lane."), "{keys}");
            }
        }
        assert!(
            toml::from_str::<Config>(&format!("{}[docs_lane]\ntopk = 3\n", base_toml())).is_err()
        );
        // A profile's docs_top_k is checked against the same range.
        let config: Config = toml::from_str(&format!(
            "{}[search_profiles.p]\ndocs_top_k = 11\n",
            base_toml()
        ))
        .unwrap();
        let error = config.validate().unwrap_err().to_string();
        assert!(
            error.contains("search_profiles.p") && error.contains("docs_top_k"),
            "{error}"
        );
    }
}
