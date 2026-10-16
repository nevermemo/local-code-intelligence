//! Graph traversal for `search_code` `mode = "traverse"` and for the
//! `lci traverse` command (multimap spine slices S6 and S6b; spine brief 1.7).
//!
//! One function, [`run`], serves both. It loads the SCIP index (map
//! `graph.scip`, cached by file stamp), finds the start nodes, builds the
//! steering ladder from the caller's rules, runs the traversal from
//! `lci-maps`, writes the trace, and answers with the reached nodes, the
//! totals, a summary of what the handlers decided and, only when the call
//! ended with work left (the ladder could not decide a stop, or the budget ran
//! out), a `handle` and the `checkpoint`.
//!
//! Start nodes (`seed_source` in the response and in the trace):
//!
//! * `given`: the caller's node ids.
//! * `hybrid`: the top `[traverse] seed_k` hits of the hybrid search that
//!   `search_code` runs in its normal mode (lexical and vector retrieval and
//!   the reranker the configured profile enables), each hit mapped to the
//!   SCIP symbols its chunk defines (or, for a chunk inside a function, the
//!   innermost function around it), else to its chunk node. Every seed
//!   records the retrievers that found the hit and its score ([`map_hits`]).
//! * `name_tokens`: the stand-in that matches the question's words against
//!   symbol names (map `seeds.scip_names`). Used only when the caller asked
//!   for that map alone, or as the visible fallback when the hybrid search is
//!   unavailable or finds nothing; the response says why (`seed_fallback`).
//!
//! The mode reads the SCIP index and, for `hybrid`, runs the search over the
//! workspace's index (which `search_code` also creates when it is missing).
//! The request can only shrink or reorder the walk: its budget is clamped to
//! the `[traverse]` caps, its rules are parsed into the closed rule AST, and
//! the only way to add pops or tokens is a `budget` action in `steer.actions`
//! on a continuation, within the engine's own limits.

mod chunks;
mod first_stage;
pub mod presets;

use crate::{
    app::App,
    config::{TraverseConfig, TraverseFirstStage},
    filter::FilterRequest,
    models::Models,
};
use anyhow::{Context, Result, anyhow, bail};
use first_stage::{
    BudgetedGit, EXACT_CLOSURE_ID, EXACT_PICKAXE_ID, ExactCard, FirstStage, closure_cards, merge,
    origin_counts, pickaxe_cards, seed_cards,
};
use lci_core::{
    config::TraverseRerankScores,
    models::HttpStatusError,
    node::{NodeId, NodeKind},
};
use lci_maps::{
    Budget, Contribution, EdgeKind, FreshnessModel, Map, MapDescriptor, MapId, MapRequest,
    MapResponse, MapStatus, Registry, SnapshotRef, SnapshotView, SourceLabel,
    config_key::{CONFIG_KEY_MAP_ID, ConfigKeyMap, ConfigKeyOptions},
    contains::{CONTAINS_MAP_ID, ContainsMap},
    git_history::{GIT_HISTORY_ID, GitHistoryConfig, GitHistoryMap, symbol_files},
    impact::{Decl, ImpactIndex},
    live_grep::{GREP_LIVE_MAP_ID, LiveGrep, ScipSpans},
    lsp_live::{LSP_LIVE_ID, LspLive, ManagerBackend, ScipPlaces},
    probe_map::{PROBE_COMPILE_ID, ProbeConfig, ProbeMap},
    scip_graph::{DirSource, SCIP_MAP_ID, ScipMap, SourceText},
    scip_impact::{ImpactMap, ImpactOptions, SCIP_IMPACT_ID},
    search_maps::{
        ChunkSearch, LexicalChunks, SEARCH_LEXICAL_ID, SEARCH_VECTOR_ID, ScipDecls, VectorChunks,
    },
    traverse::{
        Seed, Traversal, TraversalBudget, TraversalResult,
        bandit::{Options as BanditOptions, Settings as BanditSettings, Signal as BanditSignal},
        handle::HandleStore,
        handler::{Action, Checkpoint, RAISE_MAX_POPS, RAISE_MAX_TOKENS, StopPolicy},
        ladder::Ladder,
        lint::{LintContext, lint},
        priors::HopPriors,
        rescore::{FrontierRescorer, RerankFailure, RescoreError},
        rules::RuleList,
        source::OnTheFly,
        steer::Steer,
        stop::Card,
        tiebreak::TieBreak,
        trace::{self, PlanMap, PlanPolicy, StartPlan},
    },
    walk::tokenize,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{
        Arc, Mutex, OnceLock, PoisonError,
        atomic::{AtomicU32, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

/// The map that finds the start nodes from the question's words (the
/// stand-in, `seed_source` `name_tokens`).
pub const SEEDS_MAP: MapId = "seeds.scip_names";
/// The map of the start nodes the hybrid search found (`seed_source`
/// `hybrid`).
pub const SEEDS_HYBRID: MapId = "seeds.hybrid";
/// The maps a traversal can use, in registration order. All of them are on
/// unless the caller names the ones it wants; `structure.contains` is what
/// lets a chunk or file start node (what the hybrid search often finds) reach
/// the symbols the SCIP graph walks.
pub const MAPS: [MapId; 4] = [SEEDS_HYBRID, SEEDS_MAP, SCIP_MAP_ID, CONTAINS_MAP_ID];
/// The maps a traversal uses only when the call names them (`--maps`); all
/// need the tree and none is part of the default: `grep.live` searches its
/// current bytes, `config.key` runs the M1.2 extractor (needs `rg`),
/// `git.history` reads the git history up to its HEAD, and `probe.compile`
/// runs `cargo check` on a scratch copy (seconds per node). The three maps of
/// wave W0c: `search.lexical` (the lexical search from a node's own name),
/// `search.vector` (the chunks nearest to a node's stored embedding; one
/// counted embedding call when none is stored; needs the embedder) and
/// `scip.impact` (the declarations that reference a symbol, the reverse
/// closure of the SCIP impact index). `lsp.live` asks a language server for
/// definition, references and document symbols (starts the server at the first
/// expansion; needs its binary). The two exact operators `exact.closure` (the
/// declarations that reference the symbol the question names, from the same
/// index as `scip.impact`) and `exact.pickaxe` (the commits `git log -G/-S/--grep`
/// finds for a history question) are never walked: their cards are the first
/// stage (see [`first_stage`] and `[traverse] first_stage`).
pub const OPT_IN_MAPS: [MapId; 10] = [
    GREP_LIVE_MAP_ID,
    CONFIG_KEY_MAP_ID,
    GIT_HISTORY_ID,
    PROBE_COMPILE_ID,
    SEARCH_LEXICAL_ID,
    SEARCH_VECTOR_ID,
    SCIP_IMPACT_ID,
    LSP_LIVE_ID,
    EXACT_CLOSURE_ID,
    EXACT_PICKAXE_ID,
];
/// Start nodes the name-token stand-in takes from the question.
const SEED_COUNT: usize = 3;
/// Most symbols one hit of the hybrid search may start from (a chunk that
/// holds more definitions keeps the ones the question's words name best).
const MAX_SYMBOLS_PER_HIT: usize = 3;
/// External calls a traversal may make; no map here makes any.
const EXTERNAL_CALLS: u32 = 100;
/// Most results a call returns.
const MAX_TOP_K: usize = 40;
/// Longest accepted handle text.
const MAX_HANDLE: usize = 64;
/// Longest one `git log` call of `exact.pickaxe` may run.
const EXACT_GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// Commits `exact.pickaxe` asks for when `[traverse] first_k` is 0 (no cap).
const PICKAXE_UNCAPPED: usize = 100;

/// `budget` of a traversal request. Each value may lower the `[traverse]`
/// cap of the same name, never raise it.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetArgs {
    /// Most node expansions (pops).
    pub pops: Option<u32>,
    /// Depth at which states are recorded without being expanded: a state at
    /// `max_depth` is in the results but not expanded, so the deepest state
    /// expanded is at `max_depth - 1`.
    pub max_depth: Option<u32>,
    /// Most estimated tokens the expansions may cost.
    pub est_tokens: Option<u64>,
    /// Longest the call may run, in milliseconds.
    pub deadline_ms: Option<u64>,
}

/// `steer` of a traversal request: the rules the ladder applies at each
/// stop, how many stops there may be, and, on a continuation, the outer
/// agent's answer to a returned checkpoint.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SteerArgs {
    /// A named policy: `explore` (what a call with no policy runs), `impact`,
    /// `flow` or `where-to`. Its settings apply; the fields given beside it
    /// replace the preset's. The response's `effective_policy` shows what ran.
    pub policy: Option<String>,
    /// Rung 1 of the ladder: an array of `{"when": predicate, "then": action}`
    /// rules (at most 64). Predicates: `kind`, `path_glob`, `witness`, `map`,
    /// `score`, `fanout`, `depth`, `budget_left`, `seen_before`, `all`, `any`,
    /// `not`. Actions: `prune`, `expand`, `continue`, `stop`, `{"boost": f}`,
    /// `{"map_on": id}`, `{"map_off": id}`, `{"map_weight": {"id", "weight"}}`,
    /// `{"redirect": [node ids]}`, `{"emit": note}`. They can only shrink, reorder or redirect the walk.
    pub rules: Option<Value>,
    /// Reserved for an embedded script handler; not available yet.
    pub script: Option<String>,
    /// The rungs, in order, ending with `agent`. Only `rules` and `agent` are
    /// available. A stop no rung can decide returns the checkpoint to the
    /// caller.
    pub ladder: Option<Vec<String>>,
    /// Most stops in the traversal (never above the `[traverse]` cap).
    pub max_stops: Option<u32>,
    /// Confidence a rung needs to decide a stop, 0 to 1.
    pub tau: Option<f32>,
    /// A stop every this many pops. Without it, stops happen every
    /// `pops / (max_stops + 1)` pops while rules are given.
    pub stop_every: Option<u32>,
    /// Also stop when this many new disagreements have been recorded since
    /// the last stop (a node a fuzzy map offered that a structural map, which
    /// answered the same expansion, did not). At least 1.
    /// Counts toward `max_stops`; given alone it starts the ladder like
    /// `stop_every` does, with the periodic stops at their default spacing.
    pub on_disagreement: Option<u32>,
    /// Also stop when the walk is estimated to have found this percent of
    /// what it will find (1 to 100). Counts toward `max_stops`; given alone it
    /// starts the ladder like `stop_every` does.
    pub likely_complete: Option<u32>,
    /// Only with `handle`: the outer agent's answer to the checkpoint,
    /// `[{"node"?: id, "action": ..}]` with the actions of the rule list plus
    /// `{"budget": {"pops": n, "tokens": n}}` (bounded).
    pub actions: Option<Value>,
}

/// One traversal call.
pub struct Call<'a> {
    /// The SCIP index to walk.
    pub scip_index: &'a Path,
    /// The tree it indexes (read for the enclosing-range repair), if known.
    pub scip_root: Option<&'a Path>,
    pub question: &'a str,
    /// Explicit start nodes (canonical node ids); empty finds them from the
    /// question.
    pub seeds: &'a [String],
    /// The hybrid search that finds the start nodes from the question; `None`
    /// when the call has no workspace to search (the stand-in is used and
    /// says so).
    pub seed_search: Option<&'a dyn SeedSearch>,
    /// Results to return, 1 through 40.
    pub top_k: usize,
    /// The maps to use; `None` is all of [`MAPS`] (not [`OPT_IN_MAPS`]).
    pub maps: Option<&'a [String]>,
    pub budget: &'a BudgetArgs,
    pub steer: &'a SteerArgs,
    /// Continue the traversal this handle names.
    pub handle: Option<&'a str>,
    /// The settings `lsp.live` starts its language servers with (server
    /// paths, `lsp_timeout_seconds`); `None` is the defaults.
    pub lsp: Option<&'a lci_core::config::Config>,
}

/// Where a call keeps what it writes.
pub struct Dirs {
    /// `<name>.jsonl` traces.
    pub traces: PathBuf,
    /// `<handle>.json` snapshots.
    pub handles: PathBuf,
}

impl Dirs {
    /// The layout under the data directory: `traces/` (brief 1.6) and
    /// `traverse/handles/`.
    pub fn under(data_dir: &Path) -> Self {
        Self {
            traces: data_dir.join("traces"),
            handles: data_dir.join("traverse").join("handles"),
        }
    }
}

/// What a call produced.
pub struct Report {
    /// The JSON the caller gets.
    pub response: Value,
    /// The trace file, when it could be written.
    pub trace: Option<PathBuf>,
}

/// The SCIP index and the name index over it, shared between calls.
struct Loaded {
    stamp: (Option<SystemTime>, u64),
    scip: Arc<ScipMap>,
    names: Arc<NameSeeds>,
    /// The containment of the graph's files and symbols (map
    /// `structure.contains`); a call adds the lines of its chunk seeds.
    contains: ContainsMap,
    /// The graph nodes of each file (path as the index writes it, `/`
    /// separated), in graph order.
    by_file: HashMap<String, Vec<usize>>,
}

impl Loaded {
    fn new(stamp: (Option<SystemTime>, u64), scip: ScipMap) -> Self {
        let names = NameSeeds::new(&scip);
        let contains = ContainsMap::new(&scip);
        let mut by_file: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, node) in scip.graph().nodes.iter().enumerate() {
            by_file
                .entry(node.file.replace('\\', "/"))
                .or_default()
                .push(i);
        }
        Self {
            stamp,
            scip: Arc::new(scip),
            names: Arc::new(names),
            contains,
            by_file,
        }
    }
}

type Cache = Mutex<HashMap<(PathBuf, Option<PathBuf>), Arc<Loaded>>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

async fn load(index: &Path, root: Option<&Path>) -> Result<Arc<Loaded>> {
    let meta = std::fs::metadata(index).with_context(|| {
        format!(
            "no SCIP index at {}; write one with `rust-analyzer scip` in the workspace or set traverse.scip_index",
            index.display()
        )
    })?;
    let stamp = (meta.modified().ok(), meta.len());
    let key = (index.to_path_buf(), root.map(Path::to_path_buf));
    if let Some(found) = cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .filter(|loaded| loaded.stamp == stamp)
    {
        return Ok(found.clone());
    }
    let (path, root_dir) = (index.to_path_buf(), root.map(Path::to_path_buf));
    let loaded = tokio::task::spawn_blocking(move || -> Result<Loaded> {
        let source = root_dir.map(DirSource);
        let graph = lci_maps::scip_graph::ScipGraph::load(
            &path,
            source.as_ref().map(|s| s as &dyn SourceText),
        )
        .map_err(|error| anyhow!(error))?;
        Ok(Loaded::new(stamp, ScipMap::with_default_ids(graph)))
    })
    .await
    .context("load the SCIP index")??;
    let loaded = Arc::new(loaded);
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, loaded.clone());
    Ok(loaded)
}

type Stamp = (Option<SystemTime>, u64);

/// The maps built from a SCIP index and a tree, one per (index, tree), each
/// valid for the stamp of the index it was built from.
struct StampCache<M>(Mutex<HashMap<(PathBuf, PathBuf), Built<M>>>);

/// A map with the stamp of the index it was built from.
type Built<M> = (Stamp, Arc<M>);

impl<M> Default for StampCache<M> {
    fn default() -> Self {
        Self(Mutex::default())
    }
}

impl<M> StampCache<M> {
    fn get(&self, index: &Path, root: &Path, stamp: Stamp) -> Option<Arc<M>> {
        let cache = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let (built_at, map) = cache.get(&(index.to_path_buf(), root.to_path_buf()))?;
        (*built_at == stamp).then(|| map.clone())
    }

    fn put(&self, index: &Path, root: &Path, stamp: Stamp, map: M) -> Arc<M> {
        let map = Arc::new(map);
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(
                (index.to_path_buf(), root.to_path_buf()),
                (stamp, map.clone()),
            );
        map
    }
}

/// The `config.key` map of a SCIP index and a tree, built once per index
/// stamp (a tree edit after that is not seen; the map's freshness is
/// `global`). Declarations take the ids of the SCIP graph.
async fn config_key_map(index: &Path, root: &Path, loaded: &Loaded) -> Result<Arc<ConfigKeyMap>> {
    static CACHE: OnceLock<StampCache<ConfigKeyMap>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(map) = cache.get(index, root, loaded.stamp) {
        return Ok(map);
    }
    let (index_buf, root_buf, scip) =
        (index.to_path_buf(), root.to_path_buf(), loaded.scip.clone());
    let map = tokio::task::spawn_blocking(move || -> Result<ConfigKeyMap> {
        let (index, root) = (index_buf, root_buf);
        let source = DirSource(root.clone());
        let ix = ImpactIndex::load(&index, Some(&source)).map_err(|error| anyhow!(error))?;
        let resolve = |decl: &Decl| {
            scip.graph()
                .node_index(&decl.symbol)
                .map(|node| scip.node_id(node).clone())
        };
        ConfigKeyMap::extract(&ix, &root, &source, &resolve, ConfigKeyOptions::default())
            .map_err(|error| anyhow!(error))
    })
    .await
    .context("build config.key")??;
    Ok(cache.put(index, root, loaded.stamp, map))
}

/// The `scip.impact` map of a SCIP index and a tree, built once per index
/// stamp. Units the SCIP graph holds take its node ids.
async fn impact_map(index: &Path, root: &Path, loaded: &Loaded) -> Result<Arc<ImpactMap>> {
    static CACHE: OnceLock<StampCache<ImpactMap>> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(map) = cache.get(index, root, loaded.stamp) {
        return Ok(map);
    }
    let (index_buf, root_buf, scip) =
        (index.to_path_buf(), root.to_path_buf(), loaded.scip.clone());
    let map = tokio::task::spawn_blocking(move || -> Result<ImpactMap> {
        let (index, root) = (index_buf, root_buf);
        let source = DirSource(root);
        let ix = ImpactIndex::load(&index, Some(&source)).map_err(|error| anyhow!(error))?;
        let resolve = |symbol: &str| {
            scip.graph()
                .node_index(symbol)
                .map(|node| scip.node_id(node).clone())
        };
        Ok(ImpactMap::new(ix, &resolve, ImpactOptions::default()))
    })
    .await
    .context("build scip.impact")??;
    Ok(cache.put(index, root, loaded.stamp, map))
}

/// Matches the question's words against symbol names: the stand-in for the
/// lexical and vector channels until they are wrapped as maps.
struct NameSeeds {
    names: Vec<(NodeId, HashSet<String>)>,
}

impl NameSeeds {
    fn new(scip: &ScipMap) -> Self {
        let graph = scip.graph();
        Self {
            names: (0..graph.nodes.len())
                .map(|i| {
                    (
                        scip.node_id(i).clone(),
                        tokenize(&graph.nodes[i].name).into_iter().collect(),
                    )
                })
                .collect(),
        }
    }

    /// The best `limit` nodes by the number of the text's words in their
    /// name, with that number.
    fn matches(&self, text: &str, limit: usize) -> Vec<(NodeId, usize)> {
        let words = tokenize(text);
        let mut scored: Vec<(usize, &NodeId)> = self
            .names
            .iter()
            .map(|(id, tokens)| (words.iter().filter(|w| tokens.contains(*w)).count(), id))
            .filter(|(n, _)| *n > 0)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        scored
            .into_iter()
            .take(limit)
            .map(|(n, id)| (id.clone(), n))
            .collect()
    }
}

/// One hit of the hybrid search, as the start-node mapping needs it.
#[derive(Debug, Clone, PartialEq)]
pub struct SeedHit {
    /// The chunk's file, relative to the workspace.
    pub path: String,
    /// The chunk's lines, 1-based and inclusive (as the index stores them).
    pub start_line: u32,
    pub end_line: u32,
    pub content_hash: String,
    /// The chunk's ordinal among the chunks of its file with the same
    /// content hash (a part of its node id); `None` when the index's chunk
    /// list could not be read, which reads as 0.
    pub ordinal: Option<u32>,
    /// Position in the search result, 1-based.
    pub rank: usize,
    /// The retrievers that returned the chunk (`semantic`, `lexical`, `lsp`).
    pub retrievers: Vec<String>,
    pub semantic_rank: Option<usize>,
    pub lexical_rank: Option<usize>,
    pub lsp_rank: Option<usize>,
    pub fusion_score: f32,
    pub reranker_score: Option<f32>,
}

impl SeedHit {
    /// The score the search ordered the hit by: the reranker's when it ran
    /// for the hit, else the fusion score, with the name of which it is.
    fn score(&self) -> (f32, &'static str) {
        match self.reranker_score {
            Some(score) => (score, "reranker"),
            None => (self.fusion_score, "fusion"),
        }
    }
}

/// The search that turns a question into [`SeedHit`]s.
#[lci_maps::async_trait]
pub trait SeedSearch: Send + Sync {
    /// The best `k` hits of the question, best first.
    async fn search(&self, question: &str, k: usize) -> Result<Vec<SeedHit>>;

    /// The chunks of the workspace's index, for the maps `search.lexical` and
    /// `search.vector`; `None` when this search has no index to offer.
    fn chunk_search(&self) -> Option<Arc<dyn ChunkSearch + '_>> {
        None
    }

    /// The reranker client, for `[traverse] rerank_frontier`; `None` when this
    /// search has none.
    fn reranker(&self) -> Option<&Models> {
        None
    }

    /// The reranker's model and endpoint as text a handle keeps, so a resume
    /// can refuse another reranker; `None` when this search cannot say. The
    /// text names no path and no secret.
    fn reranker_identity(&self) -> Option<String> {
        None
    }
}

/// The search `search_code` runs in its normal mode, over one workspace and
/// with the retrievers and reranker of one profile.
pub struct AppSeeds<'a> {
    pub app: &'a App,
    pub workspace: &'a Path,
    pub profile: Option<&'a str>,
}

#[lci_maps::async_trait]
impl SeedSearch for AppSeeds<'_> {
    fn chunk_search(&self) -> Option<Arc<dyn ChunkSearch + '_>> {
        Some(Arc::new(chunks::AppChunks::new(self.app, self.workspace)))
    }

    fn reranker(&self) -> Option<&Models> {
        Some(&self.app.models)
    }

    fn reranker_identity(&self) -> Option<String> {
        let config = &self.app.config;
        // The endpoint is kept as a hash: the URL may carry a host or a token
        // that does not belong in a handle.
        Some(format!(
            "{} @ u{}",
            config.reranker_model,
            lci_maps::traverse::trace::hash16(config.reranker_url.as_bytes())
        ))
    }

    async fn search(&self, question: &str, k: usize) -> Result<Vec<SeedHit>> {
        let report = self
            .app
            .search_in_profile(
                self.workspace,
                question,
                Some(k),
                FilterRequest::default(),
                self.profile,
            )
            .await?;
        if report.results.is_empty() {
            return Ok(Vec::new());
        }
        // The ordinal of a chunk in its node id needs the chunk list of its
        // file, which only the index's manifest has.
        let data_dir = self.app.config.data_dir.clone();
        let workspace = report.workspace.clone();
        let manifest =
            tokio::task::spawn_blocking(move || crate::manifest::try_load(&data_dir, &workspace))
                .await
                .context("read the chunk list of the index")?
                .ok();
        Ok(report
            .results
            .iter()
            .zip(1..)
            .map(|(hit, rank)| {
                let chunk = &hit.chunk;
                let ordinal = manifest.as_ref().and_then(|manifest| {
                    let file = manifest.files.get(&chunk.relative_file_path)?;
                    let at = file.chunks.iter().position(|c| {
                        (c.start_line, c.end_line, &c.content_hash)
                            == (chunk.start_line, chunk.end_line, &chunk.content_hash)
                    })?;
                    match lci_core::node::chunk_ids(&file.chunks).swap_remove(at) {
                        NodeId::Chunk { ordinal, .. } => Some(ordinal),
                        _ => None,
                    }
                });
                SeedHit {
                    path: chunk.relative_file_path.clone(),
                    start_line: chunk.start_line,
                    end_line: chunk.end_line,
                    content_hash: chunk.content_hash.clone(),
                    ordinal,
                    rank,
                    retrievers: hit.retrieval_channels.clone(),
                    semantic_rank: hit.semantic_rank,
                    lexical_rank: hit.lexical_rank,
                    lsp_rank: hit.lsp_rank,
                    fusion_score: hit.fusion_score,
                    reranker_score: hit.reranker_score,
                }
            })
            .collect())
    }
}

/// A search that is known to be unavailable: it fails with the reason, which
/// the response names as the reason for the stand-in.
pub struct NoSearch(pub String);

#[lci_maps::async_trait]
impl SeedSearch for NoSearch {
    async fn search(&self, _question: &str, _k: usize) -> Result<Vec<SeedHit>> {
        Err(anyhow!("{}", self.0))
    }
}

/// The longest text the reranker frontier sends for one node, in characters.
const RERANK_TEXT_CHARS: usize = 1000;

/// The edge bandit's settings from the configuration (none: it is off). A name in
/// `bandit_fixed` that is not an edge kind is an error, not ignored.
fn bandit_settings(limits: &TraverseConfig) -> Result<Option<BanditSettings>> {
    if !limits.edge_bandit {
        return Ok(None);
    }
    let mut fixed = std::collections::BTreeMap::new();
    for (name, weight) in &limits.bandit_fixed {
        let Some(kind) = EdgeKind::from_name(name) else {
            let names: Vec<&str> = EdgeKind::ALL.iter().map(|kind| kind.as_str()).collect();
            bail!(
                "[traverse] bandit_fixed names {name:?}, which is not an edge kind (one of: {})",
                names.join(", ")
            );
        };
        fixed.insert(kind, *weight);
    }
    Ok(Some(BanditSettings {
        rate: limits.bandit_rate,
        signal: limits.bandit_signal,
        options: BanditOptions {
            threshold: limits.bandit_threshold,
            every: limits.bandit_every,
            sample: limits.bandit_sample,
            seed: limits.bandit_seed,
            fixed,
        },
    }))
}

/// The reranker frontier's rescorer: rates the destinations of an expansion
/// with the configured reranker (`Models::rerank`, one batched call). The text
/// of a node is what the call already holds without a new index query: for a
/// SCIP symbol its file, kind and `Owner::name` from the graph in memory, for
/// a file its path, for any other node its id (a chunk's id names its file).
/// Not the source text.
struct GraphRescorer<'a> {
    models: &'a Models,
    scip: Arc<ScipMap>,
    by_node: HashMap<NodeId, usize>,
    /// What the reranker's scores mean (`[traverse] rerank_scores`).
    scores: TraverseRerankScores,
    /// The reranker's model and endpoint as text for a handle, without the
    /// scores setting (see [`FrontierRescorer::identity`]).
    reranker: String,
    /// The error of every call that failed, in order.
    errors: Mutex<Vec<String>>,
    /// Scores that lay outside `[0, 1]` and were clamped (probability reading).
    out_of_range: AtomicU32,
}

impl<'a> GraphRescorer<'a> {
    /// `reranker` is [`SeedSearch::reranker_identity`] (none: the search does
    /// not say, and the handle cannot tell one reranker from another).
    fn new(
        models: &'a Models,
        scip: Arc<ScipMap>,
        reranker: Option<String>,
        scores: TraverseRerankScores,
    ) -> Self {
        let by_node = (0..scip.graph().nodes.len())
            .map(|i| (scip.node_id(i).clone(), i))
            .collect();
        Self {
            models,
            scip,
            by_node,
            scores,
            reranker: reranker.unwrap_or_else(|| "reranker of unknown model".to_owned()),
            errors: Mutex::default(),
            out_of_range: AtomicU32::new(0),
        }
    }

    /// How many scores were clamped into `[0, 1]` over this call.
    fn clamped(&self) -> u32 {
        self.out_of_range.load(Ordering::Relaxed)
    }

    fn text(&self, node: &NodeId) -> String {
        let text = match (self.by_node.get(node), node) {
            (Some(&i), _) => {
                let n = &self.scip.graph().nodes[i];
                let kind = match n.kind {
                    lci_maps::scip_graph::FnKind::Function => "fn",
                    lci_maps::scip_graph::FnKind::Method => "method",
                };
                format!("{}\n{kind} {}", n.file.replace('\\', "/"), n.name)
            }
            (None, NodeId::File { path }) => path.clone(),
            (None, other) => other.to_string(),
        };
        text.chars().take(RERANK_TEXT_CHARS).collect()
    }

    fn errors(&self) -> Vec<String> {
        self.errors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// One reranker score as a relevance in `[0, 1]`, by what the score means and
/// from that document's score alone: a probability is used as it is (clamped
/// into the range), a logit goes through the logistic function
/// `1 / (1 + e^-x)`. The second value is whether a probability lay outside
/// `[0, 1]`. Whatever the other documents of the call scored, the same score
/// gives the same relevance.
fn relevance(score: f64, scores: TraverseRerankScores) -> (f64, bool) {
    match scores {
        TraverseRerankScores::Probability => (
            score.clamp(0.0, 1.0),
            score.is_finite() && !(0.0..=1.0).contains(&score),
        ),
        TraverseRerankScores::Logit => (1.0 / (1.0 + (-score).exp()), false),
    }
}

/// Why a reranker call failed, from the error `Models::rerank` returned: an
/// error status, a timeout, a service that cannot be reached (or is skipped
/// as recently unreachable), an answer that cannot be read or holds the wrong
/// number of scores, any other.
fn classify(error: &anyhow::Error) -> RerankFailure {
    if error.downcast_ref::<HttpStatusError>().is_some() {
        return RerankFailure::Status;
    }
    let request = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<reqwest::Error>());
    if let Some(request) = request {
        if request.is_timeout() {
            return RerankFailure::Timeout;
        }
        if request.is_connect() {
            return RerankFailure::Unreachable;
        }
    }
    let text = format!("{error:#}");
    if text.contains("recently unreachable") {
        RerankFailure::Unreachable
    } else if text.contains("count mismatch") {
        RerankFailure::CountMismatch
    } else if text.contains("invalid reranker") || text.contains("byte limit") {
        RerankFailure::Malformed
    } else {
        RerankFailure::Other
    }
}

#[lci_maps::async_trait]
impl FrontierRescorer for &GraphRescorer<'_> {
    fn identity(&self) -> String {
        format!("{} scores={}", self.reranker, self.scores.as_str())
    }

    async fn relevance(&self, question: &str, nodes: &[NodeId]) -> Result<Vec<f64>, RescoreError> {
        let answer = if question.trim().is_empty() {
            Err(RescoreError::new(
                RerankFailure::Other,
                "the question is empty",
            ))
        } else {
            let documents: Vec<String> = nodes.iter().map(|node| self.text(node)).collect();
            self.models
                .rerank(question, &documents)
                .await
                .map_err(|error| RescoreError::new(classify(&error), format!("{error:#}")))
        };
        let ranks = match answer {
            Ok(ranks) => ranks,
            Err(error) => {
                self.errors
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(error.to_string());
                return Err(error);
            }
        };
        let mut scores = vec![f64::NAN; nodes.len()];
        for (index, score) in ranks {
            scores[index] = f64::from(score);
        }
        let mut clamped = 0;
        let rated = scores
            .iter()
            .map(|&score| {
                let (relevance, outside) = relevance(score, self.scores);
                clamped += u32::from(outside);
                relevance
            })
            .collect();
        self.out_of_range.fetch_add(clamped, Ordering::Relaxed);
        Ok(rated)
    }
}

/// A start node and what is known of where it came from.
struct MappedSeed {
    node: NodeId,
    /// The `seed_hits` record of the response and the trace.
    info: Value,
}

/// Where in the graph a hit's chunk is.
enum Place {
    /// Functions whose name is on a line of the chunk, at most
    /// [`MAX_SYMBOLS_PER_HIT`] of `total`.
    Defined { nodes: Vec<usize>, total: usize },
    /// The chunk lies inside this function (the innermost around it).
    Inside(usize),
    /// No function of the graph: the chunk stays a chunk node.
    Nowhere,
}

/// Finds the graph nodes a hit's chunk defines or lies in. Lines: the chunk's
/// are 1-based, the graph's 0-based; a chunk holds a function when the line
/// of the function's name is one of the chunk's lines, and lies in a function
/// when the function's whole range covers the chunk's lines. A chunk that
/// does neither (the gap between two functions, a type, a constant, a file
/// the index does not list) is not attributed to its neighbour.
fn place(loaded: &Loaded, hit: &SeedHit, words: &[String]) -> Place {
    let graph = loaded.scip.graph();
    let Some(nodes) = loaded.by_file.get(&hit.path.replace('\\', "/")) else {
        return Place::Nowhere;
    };
    let first = hit.start_line.saturating_sub(1);
    let last = hit.end_line.max(hit.start_line).saturating_sub(1);
    let mut defined: Vec<usize> = nodes
        .iter()
        .copied()
        .filter(|&i| (first..=last).contains(&graph.nodes[i].line))
        .collect();
    if !defined.is_empty() {
        let total = defined.len();
        if total > MAX_SYMBOLS_PER_HIT {
            let overlap = |i: usize| {
                let tokens = &loaded.names.names[i].1;
                words.iter().filter(|w| tokens.contains(*w)).count()
            };
            defined.sort_by_key(|&i| (std::cmp::Reverse(overlap(i)), graph.nodes[i].line, i));
            defined.truncate(MAX_SYMBOLS_PER_HIT);
        }
        defined.sort_by_key(|&i| (graph.nodes[i].line, i));
        return Place::Defined {
            nodes: defined,
            total,
        };
    }
    let around = nodes
        .iter()
        .copied()
        .filter(|&i| {
            let (from, to) = graph.nodes[i].span;
            from <= first && last <= to
        })
        .min_by_key(|&i| {
            let (from, to) = graph.nodes[i].span;
            (to - from, std::cmp::Reverse(from), i)
        });
    around.map_or(Place::Nowhere, Place::Inside)
}

/// Maps the hits of the hybrid search to start nodes, best hit first: a
/// symbol node of the SCIP graph where the hit's chunk defines or lies in a
/// function (see [`place`]), else the chunk's own node. A node that several
/// hits map to is one seed that lists the ranks of all of them; its other
/// fields are those of its best hit. A hit whose file or hash cannot make a
/// node id is dropped.
fn map_hits(loaded: &Loaded, question: &str, hits: &[SeedHit]) -> Vec<MappedSeed> {
    let words = tokenize(question);
    let graph = loaded.scip.graph();
    let mut seeds: Vec<MappedSeed> = Vec::new();
    let mut at: HashMap<NodeId, usize> = HashMap::new();
    for hit in hits {
        let (score, score_kind) = hit.score();
        // (node, how, definitions in the chunk, graph index)
        let candidates: Vec<(NodeId, &'static str, Option<usize>, Option<usize>)> =
            match place(loaded, hit, &words) {
                Place::Defined { nodes, total } => nodes
                    .into_iter()
                    .map(|i| {
                        (
                            loaded.scip.node_id(i).clone(),
                            "defined",
                            Some(total),
                            Some(i),
                        )
                    })
                    .collect(),
                Place::Inside(i) => vec![(loaded.scip.node_id(i).clone(), "inside", None, Some(i))],
                Place::Nowhere => {
                    let chunk = NodeId::chunk(
                        hit.path.clone(),
                        hit.content_hash.clone(),
                        hit.ordinal.unwrap_or(0),
                    )
                    .or_else(|_| NodeId::file(hit.path.clone()));
                    match chunk {
                        Ok(node) => vec![(node, "chunk", None, None)],
                        Err(_) => Vec::new(),
                    }
                }
            };
        for (node, how, symbols_in_chunk, graph_index) in candidates {
            if let Some(&known) = at.get(&node) {
                if let Some(list) = seeds[known].info["also_hit_ranks"].as_array_mut() {
                    list.push(json!(hit.rank));
                }
                continue;
            }
            let symbol = node.kind() == NodeKind::Symbol;
            let mut info = json!({
                "node": node.to_string(),
                "kind": if symbol { "symbol" } else if node.kind() == NodeKind::Chunk { "chunk" } else { "file" },
                "how": how,
                "path": hit.path,
                "lines": [hit.start_line, hit.end_line],
                "hit_rank": hit.rank,
                "retrievers": hit.retrievers,
                "channel_ranks": {
                    "semantic": hit.semantic_rank,
                    "lexical": hit.lexical_rank,
                    "lsp": hit.lsp_rank,
                },
                "score": score,
                "score_kind": score_kind,
                "also_hit_ranks": [],
            });
            if let Some(object) = info.as_object_mut() {
                if let Some(total) = symbols_in_chunk {
                    object.insert("symbols_in_chunk".into(), json!(total));
                }
                if let Some(index) = graph_index {
                    object.insert("symbol".into(), json!(graph.nodes[index].name));
                }
            }
            at.insert(node.clone(), seeds.len());
            seeds.push(MappedSeed { node, info });
        }
    }
    seeds
}

/// A start-node or first-stage source as a registered map: its descriptor
/// only. The walk never queries it; registering it puts its id in the
/// selectors, the maps list and the trace (`seeds.hybrid`, `seeds.scip_names`,
/// `exact.closure`, `exact.pickaxe`).
struct SeedMap {
    descriptor: MapDescriptor,
}

impl SeedMap {
    fn new(id: MapId, emits: Vec<NodeKind>, labels: Vec<SourceLabel>) -> Self {
        Self {
            descriptor: MapDescriptor {
                id,
                version: 1,
                contribution: Contribution::RankList,
                accepts_text: true,
                accepts: Vec::new(),
                emits,
                freshness: FreshnessModel::PerFile,
                source_labels: labels,
                needs: Vec::new(),
            },
        }
    }

    /// The hits of the hybrid search.
    fn hybrid() -> Self {
        Self::new(
            SEEDS_HYBRID,
            vec![NodeKind::Symbol, NodeKind::Chunk, NodeKind::File],
            vec![SourceLabel::Vector, SourceLabel::Lexical, SourceLabel::Lsp],
        )
    }

    /// The symbols whose names hold the question's words.
    fn name_tokens() -> Self {
        Self::new(
            SEEDS_MAP,
            vec![NodeKind::Symbol],
            vec![SourceLabel::Lexical],
        )
    }

    /// The declarations that reference the symbol the question names.
    fn exact_closure() -> Self {
        Self::new(
            EXACT_CLOSURE_ID,
            vec![NodeKind::Symbol],
            vec![SourceLabel::Scip],
        )
    }

    /// The commits `git log -G/-S/--grep` finds for a history question.
    fn exact_pickaxe() -> Self {
        Self::new(
            EXACT_PICKAXE_ID,
            vec![NodeKind::Commit],
            vec![SourceLabel::Git],
        )
    }
}

#[lci_maps::async_trait]
impl Map for SeedMap {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, _request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        MapResponse::empty(&self.descriptor, view, MapStatus::Ok)
    }
}

/// A map shared between calls, registered by value.
struct Shared<M>(Arc<M>);

#[lci_maps::async_trait]
impl<M: Map> Map for Shared<M> {
    fn describe(&self) -> &MapDescriptor {
        self.0.describe()
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        self.0.query(request, view).await
    }

    async fn expand(
        &self,
        node: &NodeId,
        budget: Budget,
        view: &SnapshotView<'_>,
    ) -> lci_maps::edge::Expanded {
        self.0.expand(node, budget, view).await
    }

    fn card(&self) -> lci_maps::edge::MapCard {
        self.0.card()
    }
}

/// The caller's budget over the caps: the lower of the two for each value.
/// Anything the request asked for above a cap is named in the second part.
fn clamp_budget(limits: &TraverseConfig, asked: &BudgetArgs) -> (TraversalBudget, Vec<String>) {
    let mut notes = Vec::new();
    let mut take = |name: &str, asked: Option<u64>, cap: u64| -> u64 {
        match asked {
            Some(value) if value > cap => {
                notes.push(format!(
                    "budget.{name} {value} is above the cap {cap}; the cap applies"
                ));
                cap
            }
            Some(value) => value,
            None => cap,
        }
    };
    let pops = take(
        "pops",
        asked.pops.map(u64::from),
        u64::from(limits.max_pops),
    );
    let depth = take(
        "max_depth",
        asked.max_depth.map(u64::from),
        u64::from(limits.max_depth),
    );
    let tokens = take("est_tokens", asked.est_tokens, limits.max_est_tokens);
    let deadline = take("deadline_ms", asked.deadline_ms, limits.deadline_ms);
    let budget = TraversalBudget {
        pops: u32::try_from(pops).unwrap_or(u32::MAX).max(1),
        max_depth: u32::try_from(depth).unwrap_or(u32::MAX),
        deadline: Instant::now() + Duration::from_millis(deadline.max(1)),
        est_tokens: tokens.max(1),
        external_calls: EXTERNAL_CALLS,
    };
    (budget, notes)
}

/// The handler a call runs under, if it has one, with the stop policy and
/// what the trace records of the policy. The stop policy is only meaningful
/// with a handler.
fn build_ladder(
    limits: &TraverseConfig,
    steer: &SteerArgs,
    pops: u32,
    notes: &mut Vec<String>,
) -> Result<Option<(Ladder, StopPolicy, PlanPolicy)>> {
    if steer.script.is_some() {
        bail!("steer.script is not available yet; give the rules in steer.rules");
    }
    if let Some(ladder) = &steer.ladder {
        let mut seen_agent = false;
        for (at, rung) in ladder.iter().enumerate() {
            if seen_agent {
                bail!("steer.ladder: `agent` must be the last rung");
            }
            match rung.as_str() {
                "rules" if at == 0 => {}
                "rules" => bail!("steer.ladder: `rules` must be the first rung"),
                "agent" => seen_agent = true,
                "system_one" => bail!("steer.ladder: the system_one rung is not available yet"),
                other => bail!(
                    "steer.ladder: unknown rung {other:?}; the rungs are rules, system_one and agent"
                ),
            }
        }
        if !seen_agent {
            bail!(
                "steer.ladder must end with `agent`: a stop no rung can decide is returned to the caller"
            );
        }
    }
    if matches!(steer.on_disagreement, Some(0)) {
        bail!("steer.on_disagreement must be at least 1");
    }
    if matches!(steer.likely_complete, Some(0) | Some(101..)) {
        bail!("steer.likely_complete is a percent from 1 to 100");
    }
    let triggered = steer.stop_every.is_some()
        || steer.on_disagreement.is_some()
        || steer.likely_complete.is_some();
    if steer.rules.is_none() && !triggered {
        if steer.max_stops.is_some() || steer.tau.is_some() {
            notes.push(
                "steer.max_stops and steer.tau do nothing without steer.rules or a stop trigger                  (steer.stop_every, steer.on_disagreement, steer.likely_complete)"
                    .to_owned(),
            );
        }
        return Ok(None);
    }
    let rules = match &steer.rules {
        Some(value) => RuleList::from_json(value).map_err(|e| anyhow!("steer.rules: {e}"))?,
        None => RuleList::new(Vec::new()).map_err(|e| anyhow!("steer.rules: {e}"))?,
    };
    let tau = steer.tau.unwrap_or(limits.tau);
    let ladder = Ladder::rules(tau, rules).map_err(|e| anyhow!("steer.tau: {e}"))?;
    let max_stops = match steer.max_stops {
        Some(asked) if asked > limits.max_stops => {
            notes.push(format!(
                "steer.max_stops {asked} is above the cap {}; the cap applies",
                limits.max_stops
            ));
            limits.max_stops
        }
        Some(asked) => asked,
        None => limits.max_stops,
    };
    let every = match steer.stop_every {
        Some(0) => bail!("steer.stop_every must be at least 1"),
        Some(every) => every,
        None => pops.div_ceil(max_stops + 1).max(1),
    };
    let plan = PlanPolicy {
        ladder: steer
            .ladder
            .clone()
            .unwrap_or_else(|| vec!["rules".to_owned(), "agent".to_owned()]),
        tau: Some(ladder.tau()),
        rules: steer
            .rules
            .as_ref()
            .and_then(Value::as_array)
            .map_or(0, |rules| rules.len() as u32),
    };
    let mut policy = StopPolicy::every(every).max_stops(max_stops);
    if let Some(n) = steer.on_disagreement {
        policy = policy.on_disagreement(n);
    }
    if let Some(percent) = steer.likely_complete {
        policy = policy.likely_complete(percent);
    }
    Ok(Some((ladder, policy, plan)))
}

fn parse_seeds(seeds: &[String]) -> Result<Vec<NodeId>> {
    seeds
        .iter()
        .map(|text| NodeId::from_str(text).map_err(|e| anyhow!("seed {text:?}: {e}")))
        .collect()
}

fn check_handle(handle: &str) -> Result<()> {
    if handle.is_empty() || handle.len() > MAX_HANDLE {
        bail!("handle is not a handle this server issued");
    }
    Ok(())
}

/// What a handle is bound to: the workspace (the canonical tree root, if the
/// call has one) and the index (its canonical path, size and modification
/// time), each as a short hash so a handle names no path. A handle made for
/// other nodes than the index holds now would resume on the wrong graph.
fn identity_of(call: &Call<'_>, loaded: &Loaded) -> String {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let workspace = call.scip_root.map_or_else(
        || "no tree".to_owned(),
        |root| canonical(root).to_string_lossy().into_owned(),
    );
    let (modified, len) = loaded.stamp;
    let nanos = modified
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    let index = format!(
        "{}|{len}|{nanos}",
        canonical(call.scip_index).to_string_lossy()
    );
    format!(
        "w{}.i{}",
        trace::hash16(workspace.as_bytes()),
        trace::hash16(index.as_bytes())
    )
}

/// Runs one traversal call. Errors are the caller's mistakes (a bad rule, an
/// unknown handle, a missing index) or the data directory not being writable
/// for a handle; a failed trace write is a note, not an error.
pub async fn run(limits: &TraverseConfig, dirs: &Dirs, call: &Call<'_>) -> Result<Report> {
    if call.top_k == 0 || call.top_k > MAX_TOP_K {
        bail!("top_k must be 1 through {MAX_TOP_K}");
    }
    let wanted: Vec<MapId> = match call.maps {
        None => MAPS.to_vec(),
        Some(ids) => ids
            .iter()
            .map(|id| {
                MAPS.iter()
                    .chain(&OPT_IN_MAPS)
                    .copied()
                    .find(|known| known == id)
                    .ok_or_else(|| {
                        anyhow!(
                            "unknown map {id:?}; the maps are {}, and, only when named, {}",
                            MAPS.join(", "),
                            OPT_IN_MAPS.join(", ")
                        )
                    })
            })
            .collect::<Result<_>>()?,
    };
    if wanted.iter().any(|id| OPT_IN_MAPS.contains(id)) && call.scip_root.is_none() {
        bail!("the maps named beyond the defaults read the tree: give its root");
    }
    let priors = match (&limits.priors, &limits.question_type) {
        (None, None) => HopPriors::default(),
        (Some(path), Some(question_type)) => HopPriors::open(path, question_type)
            .map_err(|e| anyhow!("[traverse] priors {path}: {e}"))?,
        _ => bail!("[traverse] priors and question_type go together"),
    };
    let tie_break = (limits.tie_break.as_deref())
        .map(|path| TieBreak::open(path).map_err(|e| anyhow!("[traverse] tie_break {path}: {e}")))
        .transpose()?;
    let (steer, preset) = presets::resolve(call.steer)?;
    let (mut budget, mut warnings) = clamp_budget(limits, call.budget);
    let mut notes = Vec::new();
    let handler = build_ladder(limits, &steer, budget.pops, &mut warnings)?;
    let actions = match (&steer.actions, call.handle) {
        (Some(_), None) => {
            bail!("steer.actions answers a checkpoint: give the handle it came with")
        }
        (Some(value), Some(_)) => Some(
            Steer::from_json(&json!({ "actions": value }))
                .map_err(|e| anyhow!("steer.actions: {e}"))?,
        ),
        (None, _) => None,
    };
    warnings.extend(lint_steer(
        &steer,
        handler.as_ref().map(|(_, stops, _)| stops),
        actions.as_ref(),
        &budget,
        &wanted,
    ));
    let store = HandleStore::in_dir(&dirs.handles);
    let snapshot = match call.handle {
        Some(handle) => {
            check_handle(handle)?;
            Some(
                store
                    .get(handle)
                    .map_err(|e| anyhow!("handle {handle}: {e}"))?,
            )
        }
        None => None,
    };

    let loaded = load(call.scip_index, call.scip_root).await?;
    // A handle resumes on the workspace and index it was made for.
    let identity = identity_of(call, &loaded);
    let make_rescorer = |setting: &str| match call
        .seed_search
        .and_then(|search| search.reranker().map(|models| (search, models)))
    {
        Some((search, models)) => Ok(GraphRescorer::new(
            models,
            loaded.scip.clone(),
            search.reranker_identity(),
            limits.rerank_scores,
        )),
        None => Err(anyhow!(
            "[traverse] {setting} needs the reranker: the call needs the application (give --root with a question or seeds)"
        )),
    };
    let rescorer = if limits.rerank_frontier {
        Some(make_rescorer("rerank_frontier")?)
    } else {
        None
    };
    // The edge bandit's own rescorer, independent of rerank_frontier.
    let bandit = bandit_settings(limits)?;
    let bandit_rescorer = match &bandit {
        Some(settings) if settings.signal == BanditSignal::Rerank => {
            Some(make_rescorer("bandit_signal = \"rerank\"")?)
        }
        _ => None,
    };
    if let Some(snapshot) = &snapshot {
        snapshot
            .check_identity(&identity)
            .map_err(|e| anyhow!("{e}"))?;
        snapshot.check_priors(&priors).map_err(|e| anyhow!("{e}"))?;
        snapshot
            .check_tie_break(tie_break.as_ref())
            .map_err(|e| anyhow!("{e}"))?;
        let reranker = rescorer.as_ref().map(|rescorer| rescorer.identity());
        snapshot
            .check_rerank(
                reranker
                    .as_deref()
                    .map(|identity| (limits.rerank_floor, identity)),
            )
            .map_err(|e| anyhow!("{e}"))?;
        // What a resume must repeat for the walk to be the same one.
        snapshot
            .check_boost(limits.corroboration_boost)
            .map_err(|e| anyhow!("{e}"))?;
        snapshot
            .check_top_k(call.top_k)
            .map_err(|e| anyhow!("{e}"))?;
        let bandit_identity = bandit_rescorer.as_ref().map(|rescorer| rescorer.identity());
        snapshot
            .check_bandit(
                bandit
                    .as_ref()
                    .map(|settings| settings.fingerprint(bandit_identity.as_deref()))
                    .as_deref(),
            )
            .map_err(|e| anyhow!("{e}"))?;
    }
    let wants = |id: MapId| wanted.contains(&id);

    // The start nodes (not on a continuation, which resumes the old ones).
    let mut found = Seeds::default();
    if snapshot.is_none() {
        found = find_seeds(limits, call, &loaded, &wanted).await?;
        notes.append(&mut found.notes);
    }

    let mut registry = Registry::new(60.0);
    let mut plan_maps = Vec::new();
    register(
        &mut registry,
        &mut plan_maps,
        Box::new(SeedMap::hybrid()),
        wants(SEEDS_HYBRID),
    )?;
    register(
        &mut registry,
        &mut plan_maps,
        Box::new(SeedMap::name_tokens()),
        wants(SEEDS_MAP),
    )?;
    register(
        &mut registry,
        &mut plan_maps,
        Box::new(Shared(loaded.scip.clone())),
        wants(SCIP_MAP_ID),
    )?;
    // The chunks the search maps reach have no lines in their ids: with a
    // search map on, every chunk of the index is given to structure.contains
    // so that it can expand them.
    let early = Instant::now();
    let mut lines = chunk_lines(&found);
    let mut search_index: Option<Arc<dyn ChunkSearch + '_>> = None;
    if wants(SEARCH_LEXICAL_ID) || wants(SEARCH_VECTOR_ID) {
        match call.seed_search.and_then(|search| search.chunk_search()) {
            None => notes.push(
                "search.lexical and search.vector need the workspace index: give --root and a question or seeds, with the application available".to_owned(),
            ),
            Some(index) => {
                match index.chunk_lines().await {
                    Ok(all) => {
                        let known: HashSet<NodeId> =
                            lines.iter().map(|(node, _)| node.clone()).collect();
                        lines.extend(all.into_iter().filter(|(node, _)| !known.contains(node)));
                    }
                    Err(error) => notes.push(format!(
                        "the search maps found no chunk lines (structure.contains cannot expand the chunks they reach): {error}"
                    )),
                }
                search_index = Some(index);
            }
        }
    }
    let early = early.elapsed();
    register(
        &mut registry,
        &mut plan_maps,
        Box::new(loaded.contains.with_chunk_lines(lines)),
        wants(CONTAINS_MAP_ID),
    )?;
    // Building the tree maps is setup, not walking: the deadline does not run
    // while it happens.
    let setup = Instant::now();
    if let (true, Some(root)) = (wants(GREP_LIVE_MAP_ID), call.scip_root) {
        let grep = LiveGrep::new(root, Some(Arc::new(ScipSpans::from_scip(&loaded.scip))));
        // Measure the map's cost before the walk prices a hop by it.
        grep.calibrate(3).await;
        register(&mut registry, &mut plan_maps, Box::new(grep), true)?;
    }
    if let (true, Some(root)) = (wants(CONFIG_KEY_MAP_ID), call.scip_root) {
        let map = match config_key_map(call.scip_index, root, &loaded).await {
            Ok(map) => {
                let stats = map.stats();
                notes.push(format!(
                    "config.key: {} keys, {} exact and {} text edges (text edges off), built in {:.0} ms",
                    stats.keys, stats.exact, stats.text, stats.build_ms
                ));
                map
            }
            Err(error) => {
                notes.push(format!("config.key has no edges: {error:#}"));
                Arc::new(ConfigKeyMap::empty(ConfigKeyOptions::default()))
            }
        };
        register(&mut registry, &mut plan_maps, Box::new(Shared(map)), true)?;
    }
    let mut git_history = None;
    if wants(GIT_HISTORY_ID) {
        match call.scip_root {
            None => notes.push("git.history needs the tree: give --root".to_owned()),
            Some(root) => {
                let table = symbol_files(&loaded.scip);
                match GitHistoryMap::open_with(root, GitHistoryConfig::default(), table) {
                    Ok(map) => {
                        let map = Arc::new(map);
                        register(
                            &mut registry,
                            &mut plan_maps,
                            Box::new(Shared(map.clone())),
                            true,
                        )?;
                        git_history = Some(map);
                    }
                    Err(error) => notes.push(format!("git.history is unavailable: {error}")),
                }
            }
        }
    }
    if wants(PROBE_COMPILE_ID) {
        match call.scip_root {
            None => notes.push("probe.compile needs the tree: give --root".to_owned()),
            Some(root) => {
                let map = ProbeMap::new(loaded.scip.clone(), ProbeConfig::for_tree(root));
                register(
                    &mut registry,
                    &mut plan_maps,
                    Box::new(Shared(Arc::new(map))),
                    true,
                )?;
            }
        }
    }
    let mut lsp_live = None;
    if let (true, Some(root)) = (wants(LSP_LIVE_ID), call.scip_root) {
        // The servers of the call's settings; a server starts at the first
        // expansion and stops when the map drops.
        let workspace = dunce::canonicalize(root)
            .map(|path| lci_core::workspace::Workspace {
                id: lci_core::workspace::hash(&path.to_string_lossy()),
                path,
            })
            .map_err(|error| error.to_string());
        let backend = match workspace {
            Ok(workspace) => {
                let defaults = lci_core::config::Config::default();
                ManagerBackend::open(workspace, call.lsp.unwrap_or(&defaults)).await
            }
            Err(error) => Err(error),
        };
        match backend {
            Ok(backend) => {
                let places = Arc::new(ScipPlaces::from_scip(&loaded.scip));
                let map = Arc::new(LspLive::new(Box::new(backend), places));
                register(
                    &mut registry,
                    &mut plan_maps,
                    Box::new(Shared(map.clone())),
                    true,
                )?;
                lsp_live = Some(map);
            }
            Err(error) => notes.push(format!("lsp.live is unavailable: {error}")),
        }
    }
    let mut impact = None;
    if wants(SCIP_IMPACT_ID) {
        match call.scip_root {
            None => notes.push("scip.impact needs the tree: give --root".to_owned()),
            Some(root) => {
                let map = match impact_map(call.scip_index, root, &loaded).await {
                    Ok(map) => {
                        let stats = map.stats();
                        notes.push(format!(
                            "scip.impact: {} definitions, {} with a node id ({} those of graph.scip), cap {} users per node",
                            stats.defs,
                            stats.nodes,
                            stats.resolved,
                            map.options().cap
                        ));
                        map
                    }
                    Err(error) => {
                        notes.push(format!("scip.impact has no edges: {error:#}"));
                        Arc::new(ImpactMap::empty(ImpactOptions::default()))
                    }
                };
                impact = Some(map.clone());
                register(&mut registry, &mut plan_maps, Box::new(Shared(map)), true)?;
            }
        }
    }
    // The exact operators are descriptor-only maps; their cards are the first
    // stage, made once per call and not on a continuation.
    if wants(EXACT_CLOSURE_ID) {
        register(
            &mut registry,
            &mut plan_maps,
            Box::new(SeedMap::exact_closure()),
            true,
        )?;
    }
    if wants(EXACT_PICKAXE_ID) {
        register(
            &mut registry,
            &mut plan_maps,
            Box::new(SeedMap::exact_pickaxe()),
            true,
        )?;
    }
    let exact_wanted = wants(EXACT_CLOSURE_ID) || wants(EXACT_PICKAXE_ID);
    let mut stage = FirstStage::off();
    let mut exact_report: Vec<Value> = Vec::new();
    let mut stage_time = Duration::ZERO;
    if snapshot.is_some() {
        if exact_wanted || limits.first_stage != TraverseFirstStage::Off || limits.exact_seeds {
            notes.push(
                "the first stage is not run again on a continuation: the results are the walk's alone (the first response carried the first-stage list)"
                    .to_owned(),
            );
        }
    } else if let Some(root) = call.scip_root {
        let mut exact: Vec<ExactCard> = Vec::new();
        for id in wanted.iter().copied() {
            if id == EXACT_CLOSURE_ID {
                match impact_map(call.scip_index, root, &loaded).await {
                    Ok(map) => {
                        let scip = loaded.scip.clone();
                        let in_graph = |symbol: &str| scip.graph().node_index(symbol).is_some();
                        let found = closure_cards(&map, &in_graph, call.question);
                        notes.push(found.note);
                        exact_report.push(json!({
                            "map": id,
                            "answered": !found.cards.is_empty(),
                            "cards": found.cards.len(),
                        }));
                        exact.extend(found.cards);
                    }
                    Err(error) => {
                        notes.push(format!("{id}: no index to read: {error:#}"));
                        exact_report.push(json!({"map": id, "answered": false, "cards": 0}));
                    }
                }
            } else if id == EXACT_PICKAXE_ID {
                let (tree, question) = (root.to_path_buf(), call.question.to_owned());
                let k = match limits.first_k {
                    0 => PICKAXE_UNCAPPED,
                    k => k as usize,
                };
                // The git calls share what is left of the call's deadline
                // (the walk's deadline is not extended by them, see below).
                let git = BudgetedGit {
                    git: PathBuf::from("git"),
                    tree,
                    per_call: EXACT_GIT_TIMEOUT,
                    end: budget.deadline,
                };
                let started = Instant::now();
                let found = tokio::task::spawn_blocking(move || pickaxe_cards(&git, &question, k))
                    .await
                    .context("run exact.pickaxe")?;
                stage_time += started.elapsed();
                notes.push(found.note);
                exact_report.push(json!({
                    "map": id,
                    "answered": !found.cards.is_empty(),
                    "cards": found.cards.len(),
                    "git_calls": found.git_calls,
                }));
                stage.git_calls += found.git_calls;
                exact.extend(found.cards);
            }
        }
        if limits.first_stage != TraverseFirstStage::Off {
            stage.mode = limits.first_stage;
            stage.first_k = limits.first_k as usize;
            stage.cards = exact.iter().map(|card| card.card.clone()).collect();
            if limits.answer_seeds {
                stage.cards.extend(seed_cards(&found.nodes, &found.details));
            }
        }
        if limits.exact_seeds {
            // After the others, and only nodes the graph holds.
            let aligned = found.details.len() == found.nodes.len();
            for card in exact.iter().filter(|card| card.in_graph) {
                if found.nodes.iter().any(|seed| seed.node == card.node) {
                    continue;
                }
                found.nodes.push(Seed {
                    node: card.node.clone(),
                    map: Some(card.op),
                });
                if aligned {
                    found.details.push(json!({
                        "node": card.node.to_string(),
                        "kind": "symbol",
                        "how": "exact",
                        "retrievers": [card.op],
                        "score": card.held,
                        "score_kind": "references_held",
                    }));
                }
            }
            if matches!(found.source, "" | "none") && !found.nodes.is_empty() {
                found.source = "exact";
            }
        }
    }
    let mut search_maps: (
        Option<Arc<LexicalChunks<'_>>>,
        Option<Arc<VectorChunks<'_>>>,
    ) = (None, None);
    if let Some(index) = &search_index {
        let decls: Arc<dyn lci_maps::search_maps::DeclLocator> =
            Arc::new(ScipDecls::from_scip(&loaded.scip));
        if wants(SEARCH_LEXICAL_ID) {
            let map = Arc::new(LexicalChunks::new(index.clone(), Some(decls.clone())));
            register(
                &mut registry,
                &mut plan_maps,
                Box::new(Shared(map.clone())),
                true,
            )?;
            search_maps.0 = Some(map);
        }
        if wants(SEARCH_VECTOR_ID) {
            let map = Arc::new(VectorChunks::new(index.clone(), Some(decls)));
            register(
                &mut registry,
                &mut plan_maps,
                Box::new(Shared(map.clone())),
                true,
            )?;
            search_maps.1 = Some(map);
        }
    }
    // Setup does not run the walk's deadline, but the first stage's git calls
    // do: they were bounded by the deadline and count against it.
    budget.deadline += setup.elapsed().saturating_sub(stage_time) + early;
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());

    let policy = handler.as_ref().map(|(_, _, plan)| plan.clone());
    // A continuation runs the weights and switches its handle replays.
    let running_maps = match &snapshot {
        Some(snapshot) => presets::restored(&plan_maps, &snapshot.weights, &snapshot.switches),
        None => plan_maps.clone(),
    };
    let (effective, plan_effective) = presets::effective(
        preset,
        &steer,
        handler.as_ref().map(|(_, stops, plan)| (stops, plan)),
        &running_maps,
    );
    // The most a state can be corroborated by, which the score bound needs
    // (the seed maps only start the walk; maps over one graph count once).
    let expanding = registry.independent_expanders();
    let traversal = match snapshot {
        Some(snapshot) => Traversal::resumed(OnTheFly::new(&registry, &view), snapshot, budget),
        None => {
            if found.nodes.is_empty() {
                let mut response = no_seeds_response(call, &found, notes);
                add_first_stage_only(&mut response, &stage, call.top_k, &exact_report);
                add_policy(&mut response, effective, warnings);
                return Ok(Report {
                    response,
                    trace: None,
                });
            }
            let plan = StartPlan {
                maps: plan_maps,
                policy: policy.unwrap_or_default(),
                effective: Some(plan_effective),
                seed_source: Some(found.source.to_owned()),
                seed_hits: found.details.clone(),
            };
            Traversal::new(OnTheFly::new(&registry, &view), found.nodes.clone(), budget)
                .question(call.question)
                .plan(plan)
        }
    };
    let traversal = traversal
        .identity(identity)
        .corroboration_boost(limits.corroboration_boost)
        .fusion(limits.fusion)
        .pass_through(limits.pass_through)
        .score_bound(limits.score_bound)
        .edge_bandit_with(bandit)
        .priors(priors)
        .tie_break(tie_break)
        .top_k(call.top_k, expanding)
        .expanders(registry.expanding_maps());
    let traversal = match &rescorer {
        Some(rescorer) => traversal.rerank_frontier(rescorer, limits.rerank_floor),
        None => traversal,
    };
    let traversal = match &bandit_rescorer {
        Some(rescorer) => traversal.bandit_rescorer(rescorer),
        None => traversal,
    };
    // The cap on model calls covers both rescorers.
    let traversal = if rescorer.is_some() || bandit_rescorer.is_some() {
        traversal.model_calls(limits.max_model_calls)
    } else {
        traversal
    };
    let traversal = match handler {
        Some((ladder, stops, _)) => traversal.handler(ladder, stops),
        None => traversal,
    };
    let traversal = match actions {
        Some(steer) => traversal.steer(steer),
        None => traversal,
    };
    let result = traversal.run().await;

    let handle = match &result.snapshot {
        Some(snapshot) => Some(
            store
                .put(snapshot)
                .context("keep the handle for a continuation")?,
        ),
        None => None,
    };
    let trace_path = match trace::write_trace(&dirs.traces, &result) {
        Ok(path) => Some(path),
        Err(error) => {
            notes.push(format!("the trace was not written: {error}"));
            None
        }
    };
    let failures = [
        search_maps
            .0
            .as_ref()
            .map(|map| (SEARCH_LEXICAL_ID, map.failures())),
        search_maps
            .1
            .as_ref()
            .map(|map| (SEARCH_VECTOR_ID, map.failures())),
    ];
    for (id, failures) in failures.into_iter().flatten() {
        if let Some(first) = failures.first() {
            notes.push(format!(
                "{id}: {} calls failed and gave no edges (fail-open); the first: {first}",
                failures.count()
            ));
        }
    }
    // The engine's warnings: reranker failures by reason, the model_calls cap,
    // a boost or switches the handle did not take.
    warnings.extend(result.warnings.iter().cloned());
    if let Some(rescorer) = &bandit_rescorer
        && let Some(first) = rescorer.errors().first()
    {
        notes.push(format!(
            "edge_bandit: the first of the reranker calls that failed in this call: {first}"
        ));
    }
    if let Some(rescorer) = &rescorer {
        let errors = rescorer.errors();
        if let Some(first) = errors.first() {
            notes.push(format!(
                "rerank_frontier: the first of the reranker calls that failed in this call: {first}"
            ));
        }
        if rescorer.clamped() > 0 {
            warnings.push(format!(
                "rerank_frontier: {} reranker scores lay outside [0, 1] and were clamped; [traverse] rerank_scores is \"{}\"",
                rescorer.clamped(),
                limits.rerank_scores.as_str()
            ));
        }
    }
    if let Some(error) = lsp_live.as_ref().and_then(|map| map.last_error()) {
        notes.push(format!("lsp.live gave no edges for a node: {error}"));
    }
    // What the new maps cost per call, measured in this run (the p50 ring the
    // map cards report).
    let costs = [
        search_maps
            .0
            .as_ref()
            .map(|map| (SEARCH_LEXICAL_ID, map.log())),
        search_maps
            .1
            .as_ref()
            .map(|map| (SEARCH_VECTOR_ID, map.log())),
        impact.as_ref().map(|map| (SCIP_IMPACT_ID, map.log())),
    ];
    for (id, log) in costs.into_iter().flatten() {
        if let Some(p50) = log.p50() {
            notes.push(format!(
                "{id}: expand p50 {p50:.1} ms over {} timed calls",
                log.len()
            ));
        }
    }
    let mut response = response_json(
        call,
        &result,
        &found,
        handle.as_deref(),
        trace_path.as_deref(),
        notes,
        &stage,
    );
    if let (false, Some(object)) = (exact_report.is_empty(), response.as_object_mut()) {
        object.insert("exact".into(), json!(exact_report));
    }
    add_policy(&mut response, effective, warnings);
    add_sites(&mut response, &loaded, impact.as_deref(), &found.details);
    if let Some(history) = &git_history {
        add_commits(&mut response, history);
    }
    Ok(Report {
        response,
        trace: trace_path,
    })
}

/// `effective_policy` (every response) and `warnings` (when there are any).
fn add_policy(response: &mut Value, effective: Value, warnings: Vec<String>) {
    if let Some(object) = response.as_object_mut() {
        object.insert("effective_policy".into(), effective);
        if !warnings.is_empty() {
            object.insert("warnings".into(), json!(warnings));
        }
    }
}

/// The steer lint of a call (see [`lci_maps::traverse::lint`]), plus the two
/// checks that need the steer's own fields: a ladder that omits `rules` while
/// rules are given, and `budget` actions that add more than the engine
/// allows in total. The lint reads the call; nothing it finds stops the run.
fn lint_steer(
    steer: &SteerArgs,
    stops: Option<&StopPolicy>,
    actions: Option<&Steer>,
    budget: &TraversalBudget,
    selected: &[MapId],
) -> Vec<String> {
    let mut found = Vec::new();
    if let Some(stops) = stops {
        let known: Vec<&str> = MAPS.iter().chain(&OPT_IN_MAPS).copied().collect();
        let list = steer
            .rules
            .as_ref()
            .and_then(|r| RuleList::from_json(r).ok());
        let cx = LintContext {
            pops: budget.pops,
            max_depth: budget.max_depth,
            known_maps: &known,
            selected_maps: selected,
        };
        found.extend(lint(
            list.as_ref().map_or(&[][..], RuleList::rules),
            stops,
            &cx,
        ));
    }
    if steer.rules.is_some()
        && steer
            .ladder
            .as_ref()
            .is_some_and(|ladder| !ladder.iter().any(|rung| rung == "rules"))
    {
        found.push(
            "steer.ladder lists no `rules` rung while steer.rules is given: the rules rung runs first regardless"
                .to_owned(),
        );
    }
    let (pops, tokens) = actions.map_or((0, 0), |steer| {
        steer
            .actions
            .iter()
            .fold((0u64, 0u64), |(pops, tokens), directive| {
                match &directive.action {
                    Action::Budget { pops: p, tokens: t } => (pops + u64::from(*p), tokens + t),
                    _ => (pops, tokens),
                }
            })
    });
    if pops > u64::from(RAISE_MAX_POPS) || tokens > RAISE_MAX_TOKENS {
        found.push(format!(
            "steer.actions raise the budget by {pops} pops and {tokens} tokens; the most a traversal accepts is {RAISE_MAX_POPS} pops and {RAISE_MAX_TOKENS} tokens in total, and the cap applies"
        ));
    }
    found
}

/// The lines of the chunk start nodes (1-based, inclusive), which a chunk's
/// node id does not carry, for `structure.contains`.
fn chunk_lines(found: &Seeds) -> Vec<(NodeId, (u32, u32))> {
    found
        .nodes
        .iter()
        .zip(&found.details)
        .filter(|(seed, _)| seed.node.kind() == NodeKind::Chunk)
        .filter_map(|(seed, info)| {
            let line = |i: usize| u32::try_from(info["lines"][i].as_u64()?).ok();
            Some((seed.node.clone(), (line(0)?, line(1)?)))
        })
        .collect()
}

/// Gives every commit card the subject (first line) and date of its commit.
fn add_commits(response: &mut Value, history: &GitHistoryMap) {
    for list in ["/results", "/checkpoint/answer"] {
        let Some(Value::Array(items)) = response.pointer_mut(list) else {
            continue;
        };
        for card in items {
            let commit = card["node"]
                .as_str()
                .and_then(|node| NodeId::from_str(node).ok())
                .and_then(|node| match node {
                    NodeId::Commit { sha } => history.commit_node(&sha),
                    _ => None,
                });
            if let (Some(commit), Some(object)) = (commit, card.as_object_mut()) {
                object.insert(
                    "commit".into(),
                    json!({"subject": commit.subject, "date": commit.date}),
                );
            }
        }
    }
}

/// The start nodes of a call and where they came from.
#[derive(Default)]
struct Seeds {
    nodes: Vec<Seed>,
    /// `given`, `hybrid` or `name_tokens`; empty on a continuation.
    source: &'static str,
    /// One record per start node: the retrievers that found it and its score.
    details: Vec<Value>,
    /// Why the stand-in replaced the hybrid search, if it did.
    fallback: Option<String>,
    notes: Vec<String>,
}

/// Finds the start nodes: the caller's, else the hybrid search's, else (when
/// that is unavailable or finds nothing) the name-token stand-in, and the
/// stand-in alone when it is the only seeds map wanted.
async fn find_seeds(
    limits: &TraverseConfig,
    call: &Call<'_>,
    loaded: &Loaded,
    wanted: &[MapId],
) -> Result<Seeds> {
    let explicit = parse_seeds(call.seeds)?;
    if !explicit.is_empty() {
        return Ok(Seeds {
            nodes: explicit
                .into_iter()
                .map(|node| Seed { node, map: None })
                .collect(),
            source: "given",
            ..Seeds::default()
        });
    }
    let mut found = Seeds::default();
    let mut reason = None;
    if wanted.contains(&SEEDS_HYBRID) {
        match call.seed_search {
            None => {
                reason = Some(
                    "no search is available for this call (it has no workspace to search)"
                        .to_owned(),
                );
            }
            Some(_) if call.question.trim().is_empty() => {
                reason = Some("the question is empty".to_owned());
            }
            Some(search) => match search.search(call.question, limits.seed_k as usize).await {
                Err(error) => reason = Some(format!("the hybrid search failed: {error:#}")),
                Ok(hits) if hits.is_empty() => {
                    reason = Some("the hybrid search returned no hits".to_owned());
                }
                Ok(hits) => {
                    let mapped = map_hits(loaded, call.question, &hits);
                    let kinds = |kind: &str| {
                        mapped
                            .iter()
                            .filter(|seed| seed.info["kind"] == kind)
                            .count()
                    };
                    let chunks = kinds("chunk") + kinds("file");
                    if chunks > 0 {
                        found.notes.push(if wanted.contains(&CONTAINS_MAP_ID) {
                            format!(
                                "{chunks} of {} start nodes are chunks or files that the SCIP graph does not hold; {CONTAINS_MAP_ID} expands them to the symbols they contain",
                                mapped.len()
                            )
                        } else {
                            format!(
                                "{chunks} of {} start nodes are chunks or files that the SCIP graph does not hold; nothing expands them (add {CONTAINS_MAP_ID} to the maps)",
                                mapped.len()
                            )
                        });
                    }
                    found.source = "hybrid";
                    for seed in mapped {
                        found.nodes.push(Seed {
                            node: seed.node,
                            map: Some(SEEDS_HYBRID),
                        });
                        found.details.push(seed.info);
                    }
                    return Ok(found);
                }
            },
        }
    }
    if wanted.contains(&SEEDS_MAP) {
        found.source = "name_tokens";
        for (node, matched) in loaded.names.matches(call.question, SEED_COUNT) {
            found.details.push(json!({
                "node": node.to_string(),
                "kind": "symbol",
                "how": "name",
                "retrievers": ["name_tokens"],
                "score": matched,
                "score_kind": "words_in_name",
            }));
            found.nodes.push(Seed {
                node,
                map: Some(SEEDS_MAP),
            });
        }
        if let Some(reason) = reason {
            found.notes.push(format!(
                "seed_source is name_tokens, the stand-in that matches the question's words against symbol names: {reason}"
            ));
            found.fallback = Some(reason);
        }
    } else if let Some(reason) = reason {
        found.source = "hybrid";
        found.fallback = Some(reason);
    }
    if found.source.is_empty() {
        found.source = "none";
    }
    Ok(found)
}

fn no_seeds_response(call: &Call<'_>, found: &Seeds, notes: Vec<String>) -> Value {
    let mut response = json!({
        "mode": "traverse",
        "status": "no_seeds",
        "stop_reason": "no_seeds",
        "question": call.question,
        "seeds": [],
        "seed_source": found.source,
        "results": [],
        "notes": notes_with(
            notes,
            "no start node: the hybrid search and the symbol names in the question found nothing in the SCIP graph, and no seeds were given; name a function or method the code defines",
        ),
    });
    if let (Some(reason), Some(object)) = (&found.fallback, response.as_object_mut()) {
        object.insert("seed_fallback".into(), json!(reason));
    }
    response
}

/// Adds a map to the registry (weight 1, as every map here) and to the
/// plan the trace records, switched off when the call did not ask for it.
fn register<'a>(
    registry: &mut Registry<'a>,
    plan: &mut Vec<PlanMap>,
    map: Box<dyn Map + 'a>,
    enabled: bool,
) -> Result<()> {
    let descriptor = map.describe();
    plan.push(PlanMap {
        id: descriptor.id.to_owned(),
        version: descriptor.version,
        weight: 1.0,
        enabled,
    });
    let id = descriptor.id;
    registry
        .register(map, 1.0, 20)
        .map_err(|e| anyhow!("{e}"))?;
    if !enabled {
        registry.set_enabled(id, false);
    }
    Ok(())
}

fn notes_with(mut notes: Vec<String>, note: &str) -> Vec<String> {
    notes.push(note.to_owned());
    notes
}

/// Gives a response that has no walk (no start node) the first-stage cards as
/// its results, and the report of the exact operators.
fn add_first_stage_only(
    response: &mut Value,
    stage: &FirstStage,
    top_k: usize,
    exact_report: &[Value],
) {
    let Some(object) = response.as_object_mut() else {
        return;
    };
    if stage.active() {
        let results = merge(stage.mode, &stage.cards, &[], stage.first_k, top_k);
        // The counts sit under `totals`, as on a response with a walk.
        object.insert(
            "totals".into(),
            json!({
                "first_stage": origin_counts(&results),
                "physical_external_calls": stage.git_calls,
            }),
        );
        object.insert("results".into(), json!(results));
    }
    if !exact_report.is_empty() {
        object.insert("exact".into(), json!(exact_report));
    }
}

fn response_json(
    call: &Call<'_>,
    result: &TraversalResult,
    found: &Seeds,
    handle: Option<&str>,
    trace_path: Option<&Path>,
    notes: Vec<String>,
    stage: &FirstStage,
) -> Value {
    // With a first stage the walk gives as many cards as can be left once the
    // first stage's repeats are skipped.
    let walk_cards = if stage.active() {
        call.top_k + stage.cards.len()
    } else {
        call.top_k
    };
    let results: Vec<Value> = result
        .ranked()
        .into_iter()
        .take(walk_cards)
        .map(|at| {
            let state = &result.states[at];
            let mut card = trace::card_json(&Card::ranked(state, result.ranked_potential(state)));
            // The tie-break features a card does not already carry, so a fit can
            // read them from the raw output.
            if let Some(object) = card.as_object_mut() {
                object.extend(
                    (result.facts(state).card_fields()).map(|(key, value)| (key.to_owned(), value)),
                );
            }
            card
        })
        .collect();
    let results = if stage.active() {
        merge(
            stage.mode,
            &stage.cards,
            &results,
            stage.first_k,
            call.top_k,
        )
    } else {
        results
    };
    let first_stage_counts = stage.active().then(|| origin_counts(&results));
    let steering: Vec<Value> = result
        .events
        .iter()
        .filter(|event| event["ev"] == "steer")
        .map(|event| {
            json!({
                "stop": event["stop"],
                "rung": event["rung"],
                "handler": event["handler"],
                "confidence": event["confidence"],
                "actions": event["actions"],
                "outcome": event["outcome"],
            })
        })
        .collect();
    let totals = &result.totals;
    // Per map: the calls made to it over the whole traversal and the
    // milliseconds it took in this call.
    let per_map: serde_json::Map<String, Value> = result
        .maps
        .iter()
        .map(|(id, stat)| {
            let ms = result.map_ms.get(id).copied().unwrap_or(0.0);
            ((*id).to_owned(), json!({"expands": stat.calls, "ms": ms}))
        })
        .collect();
    let mut notes = notes;
    if result.fusion == lci_maps::traverse::Fusion::Rrf {
        notes.push(
            "fusion rrf: results are ranked by weighted reciprocal rank fusion over the evidence sources (ranked_potential is the fused score); the score bound is off"
                .to_owned(),
        );
    }
    if result.events.iter().any(|e| e["external_spent"] == true) {
        notes.push(
            "the external-call allowance was spent: from then on only maps that need no external call were asked"
                .to_owned(),
        );
    }
    let mut response = json!({
        "mode": "traverse",
        "status": result.status.as_str(),
        "question": call.question,
        "seeds": found.nodes.iter().map(|seed| seed.node.to_string()).collect::<Vec<_>>(),
        "results": results,
        "totals": {
            "pops": totals.pops,
            "hops": totals.hops,
            "cross_map_hops": totals.cross_map_hops,
            "tokens": totals.tokens,
            // The allowance's account (a retried pop pays what its earlier run
            // did not) and the work that ran: every map call and the external
            // calls they made.
            "external_calls": totals.external_calls,
            "physical_map_calls": totals.physical_map_calls,
            // The git calls of `exact.pickaxe` are physical external calls too.
            "physical_external_calls": totals.physical_external_calls + stage.git_calls,
            "stops": totals.stops,
            "steers": totals.steers,
            "per_map": per_map,
            "elapsed_ms": totals.elapsed_ms,
        },
        "steering": steering,
        "trace": {
            "hash": trace::trace_hash(&result.events),
            "events": result.events.len(),
            "file": trace_path.and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()),
        },
    });
    if let Some(object) = response.as_object_mut() {
        if !found.source.is_empty() {
            object.insert("seed_source".into(), json!(found.source));
            object.insert("seed_hits".into(), json!(found.details));
            if let Some(reason) = &found.fallback {
                object.insert("seed_fallback".into(), json!(reason));
            }
        }
        object.insert("stop_reason".into(), json!(result.stop_reason.as_str()));
        if let (Some(counts), Some(totals)) = (
            first_stage_counts,
            object.get_mut("totals").and_then(Value::as_object_mut),
        ) {
            totals.insert("first_stage".into(), counts);
        }
        if let (true, Some(totals)) = (
            result.pass_through,
            object.get_mut("totals").and_then(Value::as_object_mut),
        ) {
            totals.insert(
                "pass_through_pops".into(),
                json!(result.totals.pass_through_pops),
            );
        }
        if let (Some(bandit), Some(totals)) = (
            &result.bandit,
            object.get_mut("totals").and_then(Value::as_object_mut),
        ) {
            totals.insert("bandit".into(), bandit.to_json());
            if bandit.signal == BanditSignal::Rerank {
                totals.insert("bandit_calls".into(), json!(result.totals.bandit_calls));
                totals.insert("bandit_docs".into(), json!(result.totals.bandit_docs));
            }
        }
        if let (Some(_), Some(totals)) = (
            result.rerank_floor,
            object.get_mut("totals").and_then(Value::as_object_mut),
        ) {
            // `rerank_ms` is this call's wall clock; the counts are the whole
            // traversal's.
            totals.insert("rerank_calls".into(), json!(result.totals.rerank_calls));
            totals.insert("rerank_docs".into(), json!(result.totals.rerank_docs));
            totals.insert(
                "rerank_failures".into(),
                json!(result.totals.rerank_failures),
            );
            totals.insert(
                "rerank_failure_reasons".into(),
                Value::Object(
                    result
                        .totals
                        .rerank_failure_reasons
                        .iter()
                        .map(|(reason, n)| (reason.as_str().to_owned(), json!(n)))
                        .collect(),
                ),
            );
            totals.insert("rerank_skipped".into(), json!(result.totals.rerank_skipped));
            totals.insert("rerank_ms".into(), json!(result.rerank_ms));
        }
        if let Some(checkpoint) = &result.checkpoint {
            object.insert("handle".into(), json!(handle));
            object.insert("checkpoint".into(), checkpoint_response(checkpoint));
        }
        if !notes.is_empty() {
            object.insert("notes".into(), json!(notes));
        }
    }
    response
}

/// Every card of the response (the result items and the checkpoint's answer,
/// visited cards and frontier) gets `site: {path, line, col}` where the
/// source knows where the node is: the definition of a SCIP symbol (the line
/// and column of its name), the first line of the chunk of a hybrid seed that
/// is a chunk node, the start of a file node. Paths are repository-relative
/// with `/`; lines and columns count from 1, like the other line ranges of
/// `search_code`. A node nothing locates has no `site`. The start nodes
/// (`seeds`, which a complete run does not return as results) are located in
/// `seed_sites`, by node id. A symbol the SCIP graph does not hold (a type
/// `scip.impact` offers) is located by that map.
fn add_sites(response: &mut Value, loaded: &Loaded, impact: Option<&ImpactMap>, seeds: &[Value]) {
    const LISTS: [&str; 4] = [
        "/results",
        "/checkpoint/answer",
        "/checkpoint/visited_cards",
        "/checkpoint/frontier",
    ];
    let mut wanted: HashSet<String> = HashSet::new();
    for list in LISTS {
        if let Some(Value::Array(items)) = response.pointer(list) {
            wanted.extend(
                items
                    .iter()
                    .filter_map(|card| card["node"].as_str().map(str::to_owned)),
            );
        }
    }
    let given: Vec<String> = response["seeds"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|seed| seed.as_str().map(str::to_owned))
        .collect();
    wanted.extend(given.iter().cloned());
    if wanted.is_empty() {
        return;
    }
    let mut sites: HashMap<String, Value> = HashMap::new();
    // A file node is at its first line (`structure.contains` climbs to files,
    // grep.live names them).
    for node in &wanted {
        if let Ok(NodeId::File { path }) = NodeId::from_str(node) {
            sites.insert(node.clone(), site_json(&path, 1, 1));
        }
    }
    // Chunk and file seeds first, so a symbol's definition wins.
    for seed in seeds {
        let (Some(node), Some(path), Some(line)) = (
            seed["node"].as_str(),
            seed["path"].as_str(),
            seed["lines"][0].as_u64().or(Some(1)),
        ) else {
            continue;
        };
        if wanted.contains(node) && seed["kind"] != "symbol" {
            sites.insert(node.to_owned(), site_json(path, line, 1));
        }
    }
    let graph = loaded.scip.graph();
    for (i, node) in graph.nodes.iter().enumerate() {
        let id = loaded.scip.node_id(i).to_string();
        if wanted.contains(&id) {
            sites.insert(
                id,
                site_json(
                    &node.file,
                    u64::from(node.line) + 1,
                    u64::from(node.col) + 1,
                ),
            );
        }
    }
    if let Some(impact) = impact {
        for node in &wanted {
            let found = NodeId::from_str(node)
                .ok()
                .and_then(|id| impact.site_of(&id));
            if let (false, Some((path, line, col))) = (sites.contains_key(node), found) {
                sites.insert(
                    node.clone(),
                    site_json(path, u64::from(line) + 1, u64::from(col) + 1),
                );
            }
        }
    }
    let seed_sites: serde_json::Map<String, Value> = given
        .iter()
        .filter_map(|seed| Some((seed.clone(), sites.get(seed)?.clone())))
        .collect();
    if let (false, Some(object)) = (seed_sites.is_empty(), response.as_object_mut()) {
        object.insert("seed_sites".into(), Value::Object(seed_sites));
    }
    for list in LISTS {
        let Some(Value::Array(items)) = response.pointer_mut(list) else {
            continue;
        };
        for card in items {
            let site = card["node"]
                .as_str()
                .and_then(|node| sites.get(node))
                .cloned();
            if let (Some(site), Some(object)) = (site, card.as_object_mut()) {
                object.insert("site".into(), site);
            }
        }
    }
}

fn site_json(path: &str, line: u64, col: u64) -> Value {
    json!({"path": path.replace('\\', "/"), "line": line, "col": col})
}

fn checkpoint_response(checkpoint: &Checkpoint) -> Value {
    let mut value = trace::checkpoint_json(checkpoint);
    if let Some(object) = value.as_object_mut() {
        object.insert("handle".into(), json!(checkpoint.handle));
    }
    value
}

/// `search_code` with `mode = "traverse"`: the workspace's SCIP index
/// (`traverse.scip_index`, relative to the workspace), the data directory
/// for traces and handles, the limits of `[traverse]` and, for the start
/// nodes, the hybrid search of `app` over the workspace with the retrievers
/// of `profile` (the endpoint's `[search_profiles]` entry, if any). An error
/// while the mode is off.
#[allow(clippy::too_many_arguments)]
pub async fn search(
    app: &App,
    workspace_path: &str,
    query: &str,
    top_k: Option<usize>,
    budget: Option<&BudgetArgs>,
    steer: Option<&SteerArgs>,
    handle: Option<&str>,
    profile: Option<&str>,
) -> Result<Value> {
    let config = &app.config;
    let limits = &config.traverse;
    if !limits.enabled {
        bail!(
            "mode = \"traverse\" is switched off on this server; set `enabled = true` under [traverse] in its configuration"
        );
    }
    let workspace = dunce::canonicalize(workspace_path)
        .with_context(|| format!("workspace_path {workspace_path:?} is not a directory"))?;
    if !workspace.is_dir() {
        bail!("workspace_path {workspace_path:?} is not a directory");
    }
    let scip_index = workspace.join(&limits.scip_index);
    let (no_budget, no_steer) = (BudgetArgs::default(), SteerArgs::default());
    let seed_search = AppSeeds {
        app,
        workspace: &workspace,
        profile,
    };
    let call = Call {
        scip_index: &scip_index,
        scip_root: Some(&workspace),
        question: query,
        seeds: &[],
        seed_search: Some(&seed_search),
        top_k: top_k.unwrap_or(config.default_top_k),
        maps: None,
        budget: budget.unwrap_or(&no_budget),
        steer: steer.unwrap_or(&no_steer),
        handle,
        lsp: Some(config),
    };
    Ok(run(limits, &Dirs::under(&config.data_dir), &call)
        .await?
        .response)
}

/// Text for the server instructions when the mode is on.
pub const INSTRUCTIONS: &str = " search_code also takes mode=\"traverse\": instead of a ranked list it walks the code graph (SCIP index: callers, callees, references) from the symbols that the hybrid search for the query finds (the search of the normal mode, then mapped to the index; a stand-in that matches symbol names takes over only if the search is unavailable or finds nothing, and seed_source says which), under a budget, and returns the reached nodes with the trace totals. steer.rules is an array of {when, then} rules (prune, expand, boost, map on/off, stop, emit) applied at each stop; a stop no rule decides, or an exhausted budget, returns a handle and a checkpoint, and the next call carries the handle and steer.actions. The mode needs an index.scip in the workspace, and the search that finds the start nodes uses the same index and model services as the normal mode.";

#[cfg(test)]
mod tests {
    use super::*;
    use lci_maps::EdgeKind;
    use serde_json::json;
    use std::sync::Mutex;

    fn limits() -> TraverseConfig {
        TraverseConfig {
            enabled: true,
            ..TraverseConfig::default()
        }
    }

    #[test]
    fn the_configured_boost_starts_at_the_engines_default() {
        assert_eq!(
            TraverseConfig::default().corroboration_boost,
            lci_maps::traverse::DEFAULT_CORROBORATION_BOOST
        );
    }

    #[test]
    fn a_budget_above_a_cap_is_lowered_and_named() {
        let asked = BudgetArgs {
            pops: Some(5000),
            max_depth: Some(3),
            est_tokens: None,
            deadline_ms: Some(10),
        };
        let (budget, notes) = clamp_budget(&limits(), &asked);
        assert_eq!(budget.pops, 200);
        assert_eq!(budget.max_depth, 3);
        assert_eq!(budget.est_tokens, 20_000);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("budget.pops 5000"), "{notes:?}");
    }

    #[test]
    fn a_ladder_must_end_with_the_agent_and_cannot_use_system_one() {
        let ladder = |rungs: &[&str]| SteerArgs {
            ladder: Some(rungs.iter().map(|s| (*s).to_owned()).collect()),
            ..SteerArgs::default()
        };
        let build = |steer: &SteerArgs| build_ladder(&limits(), steer, 100, &mut Vec::new());
        assert!(build(&ladder(&["rules", "agent"])).is_ok());
        assert!(build(&ladder(&["agent"])).is_ok());
        for bad in [
            &["rules"][..],
            &["rules", "system_one", "agent"],
            &["agent", "rules"],
            &["agent", "agent"],
            &["rules", "oracle", "agent"],
        ] {
            assert!(build(&ladder(bad)).is_err(), "{bad:?}");
        }
        let script = SteerArgs {
            script: Some("1".into()),
            ..SteerArgs::default()
        };
        assert!(build(&script).is_err());
    }

    #[test]
    fn rules_make_a_handler_and_their_absence_makes_none() {
        let mut notes = Vec::new();
        assert!(
            build_ladder(&limits(), &SteerArgs::default(), 100, &mut notes)
                .unwrap()
                .is_none()
        );
        let with_rules = SteerArgs {
            rules: Some(json!([{"when": {"path_glob": "tests/**"}, "then": "prune"}])),
            max_stops: Some(99),
            ..SteerArgs::default()
        };
        let (ladder, stops, plan) = build_ladder(&limits(), &with_rules, 100, &mut notes)
            .unwrap()
            .unwrap();
        assert_eq!(ladder.len(), 1);
        assert_eq!(plan.ladder, ["rules", "agent"], "the default ladder");
        assert_eq!(plan.rules, 1);
        assert_eq!(plan.tau, Some(0.8));
        assert_eq!(stops.max_stops, 4, "capped by the configuration");
        assert_eq!(stops.every_pops, 20, "100 pops over 5 spans");
        assert_eq!(notes.len(), 1, "{notes:?}");
        let bad = SteerArgs {
            rules: Some(json!([{"when": {"nope": 1}, "then": "prune"}])),
            ..SteerArgs::default()
        };
        let error = build_ladder(&limits(), &bad, 100, &mut notes)
            .err()
            .unwrap();
        assert!(error.to_string().starts_with("steer.rules: "), "{error}");
    }

    #[test]
    fn the_disagreement_and_likely_complete_triggers_come_from_the_steer_json() {
        // The shape of docs/spike/multimap/v2/policies/ladder-rules-only.json.
        let steer: SteerArgs = serde_json::from_value(json!({
            "rules": [{"when": {"kind": "test"}, "then": "prune"}],
            "max_stops": 3,
            "stop_every": 10,
            "on_disagreement": 5,
            "likely_complete": 90
        }))
        .unwrap();
        let (_, stops, _) = build_ladder(&limits(), &steer, 100, &mut Vec::new())
            .unwrap()
            .unwrap();
        assert_eq!(stops.every_pops, 10);
        assert_eq!(stops.max_stops, 3);
        assert_eq!(stops.on_disagreement, 5);
        assert_eq!(stops.likely_complete, 90);
        assert_eq!(stops.budget_share, 0);
        assert_eq!(stops.on_stagnation, 0);

        // Absent, they stay off.
        let plain: SteerArgs = serde_json::from_value(json!({"stop_every": 10})).unwrap();
        let (_, stops, _) = build_ladder(&limits(), &plain, 100, &mut Vec::new())
            .unwrap()
            .unwrap();
        assert_eq!((stops.on_disagreement, stops.likely_complete), (0, 0));

        // Either alone starts the ladder, as stop_every does; the periodic
        // stops keep their default spacing.
        for body in [
            json!({"on_disagreement": 2}),
            json!({"likely_complete": 80}),
        ] {
            let alone: SteerArgs = serde_json::from_value(body).unwrap();
            let mut notes = Vec::new();
            let (_, stops, _) = build_ladder(&limits(), &alone, 100, &mut notes)
                .unwrap()
                .unwrap();
            assert_eq!(stops.every_pops, 20, "100 pops over 5 spans");
            assert!(notes.is_empty(), "{notes:?}");
        }

        // Out-of-range values are errors, not silent switch-offs.
        for body in [
            json!({"on_disagreement": 0}),
            json!({"likely_complete": 0}),
            json!({"likely_complete": 101}),
        ] {
            let bad: SteerArgs = serde_json::from_value(body.clone()).unwrap();
            let error = build_ladder(&limits(), &bad, 100, &mut Vec::new())
                .err()
                .unwrap_or_else(|| panic!("{body} was accepted"));
            assert!(error.to_string().starts_with("steer."), "{error}");
        }

        // Unknown keys are still refused.
        assert!(serde_json::from_value::<SteerArgs>(json!({"on_stagnation": 3})).is_err());
    }

    // ---- steer lint and the effective policy (slice S9a) -------------------

    /// The warnings of `steer` in a call with the default budget and maps.
    fn warnings_of(steer: &SteerArgs) -> Vec<String> {
        let (steer, _) = presets::resolve(steer).unwrap();
        let limits = TraverseConfig {
            max_stops: 8,
            ..limits()
        };
        let (budget, mut warnings) = clamp_budget(&limits, &BudgetArgs::default());
        let handler = build_ladder(&limits, &steer, budget.pops, &mut warnings).unwrap();
        let actions = steer
            .actions
            .as_ref()
            .map(|value| Steer::from_json(&json!({ "actions": value })).unwrap());
        warnings.extend(lint_steer(
            &steer,
            handler.as_ref().map(|(_, stops, _)| stops),
            actions.as_ref(),
            &budget,
            &MAPS,
        ));
        warnings
    }

    #[test]
    fn the_presets_and_the_shipped_policy_file_lint_clean() {
        let file = include_str!("../../docs/spike/multimap/v2/policies/ladder-rules-only.json");
        let mut steers: Vec<SteerArgs> = vec![serde_json::from_str(file).unwrap()];
        steers.extend(presets::FILES.iter().map(|(name, _)| SteerArgs {
            policy: Some((*name).to_owned()),
            ..SteerArgs::default()
        }));
        for steer in steers {
            assert_eq!(warnings_of(&steer), Vec::<String>::new(), "{steer:?}");
        }
    }

    #[test]
    fn the_steer_checks_that_need_the_steer_fields_fire_on_a_minimal_bad_input() {
        let steer = |body: Value| serde_json::from_value::<SteerArgs>(body).unwrap();
        for (body, needle) in [
            (
                json!({"rules": [], "ladder": ["agent"]}),
                "lists no `rules` rung",
            ),
            (
                json!({"rules": [{"when": {"map": "nope"}, "then": "prune"}]}),
                "rule 0 cannot fire",
            ),
            (json!({"stop_every": 5000}), "stop_every 5000"),
            (json!({"rules": [], "max_stops": 99}), "max_stops 99"),
            (json!({"max_stops": 2}), "do nothing"),
            (
                json!({"rules": [], "actions": [{"action": {"budget": {"pops": 500}}}]}),
                "raise the budget by 500 pops",
            ),
        ] {
            let found = warnings_of(&steer(body.clone()));
            assert!(
                found.iter().any(|line| line.contains(needle)),
                "{body}: {found:?}"
            );
        }
    }

    #[test]
    fn the_effective_policy_shows_the_defaults_a_call_ran_with() {
        let steer = SteerArgs {
            rules: Some(json!([{"when": {"kind": "test"}, "then": "prune"}])),
            stop_every: Some(7),
            ..SteerArgs::default()
        };
        let handler = build_ladder(&limits(), &steer, 100, &mut Vec::new())
            .unwrap()
            .unwrap();
        let maps = [PlanMap {
            id: "graph.scip".into(),
            version: 1,
            weight: 1.0,
            enabled: true,
        }];
        let (body, plan) = presets::effective(
            Some("impact".into()),
            &steer,
            Some((&handler.1, &handler.2)),
            &maps,
        );
        assert_eq!(body["preset"], "impact");
        assert_eq!(body["stops"]["every_pops"], 7);
        assert_eq!(body["stops"]["max_stops"], 4, "the configured cap");
        assert_eq!(body["ladder"], json!(["rules", "agent"]));
        assert_eq!(body["tau"], 0.8);
        assert_eq!(body["maps"][0]["weight"], 1.0);
        assert_eq!(
            (body["hash"].as_str(), plan.preset.as_deref()),
            (Some(plan.hash.as_str()), Some("impact"))
        );
        // The hash is of the content, not of the name.
        let (renamed, _) = presets::effective(None, &steer, Some((&handler.1, &handler.2)), &maps);
        assert_eq!(renamed["hash"], body["hash"]);
        let (none, _) =
            presets::effective(Some("explore".into()), &SteerArgs::default(), None, &maps);
        assert_eq!(none["stops"], Value::Null);
        assert_ne!(none["hash"], body["hash"]);
    }

    // ---- start nodes from the hybrid search (slice S6b) --------------------

    use lci_maps::scip_graph::{RawDocument, RawOccurrence, ScipGraph};
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn symbol(name: &str) -> String {
        format!("rust-analyzer cargo demo 0.1.0 m/{name}().")
    }

    /// A definition of `name` on 0-based `line`, with an enclosing range of
    /// `lines.0..=lines.1`.
    fn defined(name: &str, line: u32, lines: (u32, u32)) -> RawOccurrence {
        RawOccurrence {
            range: (line, 3, line, 3 + name.len() as u32),
            symbol: symbol(name),
            roles: 1,
            enclosing: Some((lines.0, 0, lines.1, 1)),
        }
    }

    /// `src/search.rs` (0-based lines; the hits below count from 1):
    /// `search` 0..=9; a gap of two lines; `parse_query` 12..=14;
    /// `rank_hits` 15..=30 with the nested `score_inner` 20..=24; five
    /// one-line functions on lines 40 to 44. `src/other.rs` has `helper`.
    fn loaded() -> Loaded {
        let one_liners = [
            "load_user",
            "save_user",
            "drop_user",
            "list_user",
            "find_user",
        ];
        let mut search = vec![
            defined("search", 0, (0, 9)),
            defined("parse_query", 12, (12, 14)),
            defined("rank_hits", 15, (15, 30)),
            defined("score_inner", 20, (20, 24)),
        ];
        for (i, name) in one_liners.iter().enumerate() {
            let line = 40 + i as u32;
            search.push(defined(name, line, (line, line)));
        }
        let docs = vec![
            RawDocument {
                path: "src/search.rs".into(),
                occurrences: search,
            },
            RawDocument {
                path: "src/other.rs".into(),
                occurrences: vec![defined("helper", 3, (3, 5))],
            },
        ];
        Loaded::new(
            (None, 0),
            ScipMap::with_default_ids(ScipGraph::build(&docs, None)),
        )
    }

    #[test]
    fn a_reranker_score_becomes_relevance_by_what_the_setting_says_and_by_itself() {
        use TraverseRerankScores::{Logit, Probability};
        // Probabilities pass through; one outside [0, 1] is clamped and counted.
        assert_eq!(relevance(0.2, Probability), (0.2, false));
        assert_eq!(relevance(1.0, Probability), (1.0, false));
        assert_eq!(relevance(1.5, Probability), (1.0, true));
        assert_eq!(relevance(-0.5, Probability), (0.0, true));
        // Logits go through the logistic function.
        assert_eq!(relevance(0.0, Logit), (0.5, false));
        let (low, high) = (relevance(-2.0, Logit).0, relevance(3.0, Logit).0);
        assert!(low < 0.5 && high > 0.5 && high < 1.0);
        assert_eq!(relevance(0.5, Logit).0, 1.0 / (1.0 + (-0.5f64).exp()));
        // A score means the same whatever the other documents of its call
        // scored: a probability beside a stray logit is not rescaled.
        assert_eq!(relevance(0.2, Probability).0, 0.2);
        assert_eq!(relevance(0.75, Probability).0, 0.75);
    }

    #[test]
    fn a_failed_rerank_call_is_classified_by_what_went_wrong() {
        let status = anyhow::Error::new(HttpStatusError {
            status: 503,
            body: String::new(),
        })
        .context("reranker HTTP error");
        let cases = [
            (status, RerankFailure::Status),
            (
                anyhow!("reranker skipped as recently unreachable (next attempt in about 9s)"),
                RerankFailure::Unreachable,
            ),
            (
                anyhow!("reranker response count mismatch"),
                RerankFailure::CountMismatch,
            ),
            (
                anyhow!("invalid reranker index/score"),
                RerankFailure::Malformed,
            ),
            (
                anyhow!("reranker response body exceeds 1 byte limit"),
                RerankFailure::Malformed,
            ),
            (
                anyhow::Error::msg("expected value").context("invalid reranker response"),
                RerankFailure::Malformed,
            ),
            (anyhow!("something else"), RerankFailure::Other),
        ];
        for (error, want) in cases {
            assert_eq!(classify(&error), want, "{error:#}");
        }
    }

    /// Answers each connection with one canned HTTP response (a stand-in
    /// reranker on a loopback port) and returns its URL.
    async fn canned_reranker(status: &'static str, body: &'static str) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/rerank", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let mut seen = Vec::new();
                let mut chunk = [0u8; 4096];
                // Read the request: the head, then the body it announces.
                while let Ok(n) = stream.read(&mut chunk).await {
                    if n == 0 {
                        break;
                    }
                    seen.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&seen).into_owned();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text[..end]
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if seen.len() >= end + 4 + length {
                            break;
                        }
                    }
                }
                let reply = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(reply.as_bytes()).await;
                let _ = stream.shutdown().await;
            }
        });
        url
    }

    fn rescorer_at<'a>(
        models: &'a Models,
        loaded: &Loaded,
        scores: TraverseRerankScores,
    ) -> GraphRescorer<'a> {
        GraphRescorer::new(
            models,
            loaded.scip.clone(),
            Some("model @ uabc".into()),
            scores,
        )
    }

    fn models_at(url: String) -> Models {
        Models::new(&lci_core::config::Config {
            reranker_url: url,
            ..Default::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn the_frontier_rates_nodes_by_what_the_graph_holds_and_records_a_failed_call() {
        let loaded = loaded();
        let models = models_at("http://127.0.0.1:1/rerank".into());
        let rescorer = rescorer_at(&models, &loaded, TraverseRerankScores::Probability);
        let symbol = loaded.scip.node_id(0).clone();
        let text = rescorer.text(&symbol);
        assert!(text.starts_with("src/search.rs\nfn "), "{text:?}");
        let file = NodeId::file("src/new.rs").unwrap();
        assert_eq!(rescorer.text(&file), "src/new.rs");
        let long = NodeId::file(format!("src/{}.rs", "x".repeat(3000))).unwrap();
        assert_eq!(rescorer.text(&long).chars().count(), RERANK_TEXT_CHARS);

        // The reranker is unreachable: the call fails with that reason, the
        // error is kept, and an empty question is refused without asking.
        let error = (&rescorer)
            .relevance("rank hits", &[symbol.clone(), file.clone()])
            .await
            .unwrap_err();
        assert_eq!(error.reason, RerankFailure::Unreachable, "{error}");
        assert!(!error.message.is_empty());
        let empty = (&rescorer).relevance("  ", &[symbol]).await.unwrap_err();
        assert_eq!(empty.message, "the question is empty");
        assert_eq!(rescorer.errors().len(), 2);
    }

    #[tokio::test]
    async fn the_rescorer_reports_a_reason_per_failure_and_scores_by_its_setting() {
        let loaded = loaded();
        let nodes = [
            loaded.scip.node_id(0).clone(),
            NodeId::file("src/new.rs").unwrap(),
        ];

        // An error status.
        let url = canned_reranker("500 Internal Server Error", "{\"error\":\"x\"}").await;
        let models = models_at(url);
        let rescorer = rescorer_at(&models, &loaded, TraverseRerankScores::Probability);
        let error = (&rescorer).relevance("q", &nodes).await.unwrap_err();
        assert_eq!(error.reason, RerankFailure::Status, "{error}");
        // An answer that is not JSON.
        let models = models_at(canned_reranker("200 OK", "not json").await);
        let rescorer = rescorer_at(&models, &loaded, TraverseRerankScores::Probability);
        let error = (&rescorer).relevance("q", &nodes).await.unwrap_err();
        assert_eq!(error.reason, RerankFailure::Malformed, "{error}");
        // The wrong number of scores.
        let one = "{\"results\":[{\"index\":0,\"relevance_score\":0.5}]}";
        let models = models_at(canned_reranker("200 OK", one).await);
        let rescorer = rescorer_at(&models, &loaded, TraverseRerankScores::Probability);
        let error = (&rescorer).relevance("q", &nodes).await.unwrap_err();
        assert_eq!(error.reason, RerankFailure::CountMismatch, "{error}");

        // Two scores, one outside [0, 1]: each is read by itself.
        let body = "{\"results\":[{\"index\":0,\"relevance_score\":0.25},{\"index\":1,\"relevance_score\":4.0}]}";
        let models = models_at(canned_reranker("200 OK", body).await);
        let probability = rescorer_at(&models, &loaded, TraverseRerankScores::Probability);
        let got = (&probability).relevance("q", &nodes).await.unwrap();
        assert_eq!(
            got,
            [0.25, 1.0],
            "the 4.0 is clamped, the 0.25 is not rescaled"
        );
        assert_eq!(probability.clamped(), 1);
        let models = models_at(canned_reranker("200 OK", body).await);
        let logit = rescorer_at(&models, &loaded, TraverseRerankScores::Logit);
        let got = (&logit).relevance("q", &nodes).await.unwrap();
        assert_eq!(got[0], 1.0 / (1.0 + (-0.25f64).exp()));
        assert_eq!(got[1], 1.0 / (1.0 + (-4.0f64).exp()));
        assert_eq!(logit.clamped(), 0);
    }

    #[test]
    fn the_rescorers_identity_names_the_reranker_and_how_its_scores_are_read() {
        let loaded = loaded();
        let models = Models::new(&lci_core::config::Config::default()).unwrap();
        let identity = |reranker: Option<&str>, scores| {
            let rescorer = GraphRescorer::new(
                &models,
                loaded.scip.clone(),
                reranker.map(str::to_owned),
                scores,
            );
            (&rescorer).identity()
        };
        let probability = identity(Some("m @ u1"), TraverseRerankScores::Probability);
        assert_eq!(probability, "m @ u1 scores=probability");
        assert_eq!(
            identity(Some("m @ u1"), TraverseRerankScores::Logit),
            "m @ u1 scores=logit"
        );
        assert_ne!(
            identity(Some("m @ u2"), TraverseRerankScores::Probability),
            probability
        );
        assert!(identity(None, TraverseRerankScores::Probability).contains("unknown"));
    }

    #[tokio::test]
    async fn the_reranker_frontier_needs_the_application() {
        let tree = tempfile::tempdir().unwrap();
        let index = tree.path().join("index.scip");
        std::fs::write(&index, []).unwrap();
        let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
        let call = Call {
            scip_index: &index,
            scip_root: Some(tree.path()),
            question: "q",
            seeds: &[],
            seed_search: None,
            top_k: 8,
            maps: None,
            budget: &budget,
            steer: &steer,
            handle: None,
            lsp: None,
        };
        let limits = TraverseConfig {
            rerank_frontier: true,
            ..limits()
        };
        let error = run(&limits, &Dirs::under(tree.path()), &call)
            .await
            .err()
            .expect("no reranker")
            .to_string();
        assert!(
            error.contains("rerank_frontier needs the reranker"),
            "{error}"
        );
    }

    // ---- a resume repeats what its handle was made with (slice R1b) --------

    /// A search that only offers the reranker (the seeds are given).
    struct WithReranker {
        models: Models,
        identity: &'static str,
    }

    #[lci_maps::async_trait]
    impl SeedSearch for WithReranker {
        async fn search(&self, _question: &str, _k: usize) -> Result<Vec<SeedHit>> {
            Err(anyhow!("not asked: the seeds are given"))
        }

        fn reranker(&self) -> Option<&Models> {
            Some(&self.models)
        }

        fn reranker_identity(&self) -> Option<String> {
            Some(self.identity.to_owned())
        }
    }

    /// An empty index in a temporary tree, and three file nodes to start from:
    /// with a cap of one pop the first call leaves a handle.
    struct Tree {
        dir: tempfile::TempDir,
        index: PathBuf,
        seeds: Vec<String>,
    }

    impl Tree {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let index = dir.path().join("index.scip");
            std::fs::write(&index, []).unwrap();
            let seeds = ["src/a.rs", "src/b.rs", "src/c.rs"]
                .map(|path| NodeId::file(path).unwrap().to_string())
                .to_vec();
            Self { dir, index, seeds }
        }

        async fn run(
            &self,
            limits: &TraverseConfig,
            search: Option<&dyn SeedSearch>,
            steer: &SteerArgs,
            handle: Option<&str>,
            top_k: usize,
        ) -> Result<Value> {
            let budget = BudgetArgs {
                pops: Some(1),
                ..BudgetArgs::default()
            };
            let call = Call {
                scip_index: &self.index,
                scip_root: Some(self.dir.path()),
                question: "q",
                seeds: &self.seeds,
                seed_search: search,
                top_k,
                maps: None,
                budget: &budget,
                steer,
                handle,
                lsp: None,
            };
            Ok(run(limits, &Dirs::under(self.dir.path()), &call)
                .await?
                .response)
        }
    }

    fn handle_of(response: &Value) -> String {
        response["handle"]
            .as_str()
            .unwrap_or_else(|| panic!("no handle in {response}"))
            .to_owned()
    }

    #[tokio::test]
    async fn a_resume_must_repeat_the_boost_and_the_top_k_of_its_handle() {
        let tree = Tree::new();
        let steer = SteerArgs::default();
        let first = tree.run(&limits(), None, &steer, None, 8).await.unwrap();
        let handle = handle_of(&first);
        let totals = &first["totals"];
        assert!(
            totals["physical_map_calls"].as_u64().unwrap() >= 1,
            "{totals}"
        );
        assert!(totals["physical_external_calls"].is_u64() && totals["external_calls"].is_u64());
        assert!(
            totals.get("rerank_failure_reasons").is_none(),
            "no frontier, no rerank totals"
        );

        // The same settings resume.
        tree.run(&limits(), None, &steer, Some(&handle), 8)
            .await
            .unwrap();
        // Another boost or another k is refused, naming both.
        let other = TraverseConfig {
            corroboration_boost: 0.5,
            ..limits()
        };
        let error = tree
            .run(&other, None, &steer, Some(&handle), 8)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("boost") && error.contains("0.5") && error.contains("0.1"),
            "{error}"
        );
        let error = tree
            .run(&limits(), None, &steer, Some(&handle), 3)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("top_k") || error.contains("k of"), "{error}");
        assert!(error.contains('8') && error.contains('3'), "{error}");
    }

    #[tokio::test]
    async fn a_resume_must_repeat_the_reranker_its_handle_was_made_with() {
        let tree = Tree::new();
        let steer = SteerArgs::default();
        let on = TraverseConfig {
            rerank_frontier: true,
            ..limits()
        };
        let models = || models_at("http://127.0.0.1:1/rerank".into());
        let search = |identity| WithReranker {
            models: models(),
            identity,
        };
        let (a, b) = (search("model-a @ u1"), search("model-b @ u1"));

        let first = tree.run(&on, Some(&a), &steer, None, 8).await.unwrap();
        let handle = handle_of(&first);
        // The totals say what ran: the physical map calls, and the reranker's
        // failures by reason (none here) and skipped expansions.
        let totals = &first["totals"];
        assert!(
            totals["physical_map_calls"].as_u64().unwrap() >= 1,
            "{totals}"
        );
        assert_eq!(totals["rerank_failure_reasons"], json!({}));
        assert_eq!(totals["rerank_skipped"], 0);
        // The same model, endpoint and score reading resume.
        tree.run(&on, Some(&a), &steer, Some(&handle), 8)
            .await
            .unwrap();
        // Another model, another score reading, the frontier off, and a
        // different floor are each refused.
        async fn resume_refused(
            tree: &Tree,
            limits: &TraverseConfig,
            search: Option<&dyn SeedSearch>,
            handle: &str,
        ) -> String {
            tree.run(limits, search, &SteerArgs::default(), Some(handle), 8)
                .await
                .unwrap_err()
                .to_string()
        }
        let error = resume_refused(&tree, &on, Some(&b), &handle).await;
        assert!(
            error.contains("model-a") && error.contains("model-b"),
            "{error}"
        );
        let logit = TraverseConfig {
            rerank_scores: TraverseRerankScores::Logit,
            ..on.clone()
        };
        let error = resume_refused(&tree, &logit, Some(&a), &handle).await;
        assert!(
            error.contains("scores=probability") && error.contains("scores=logit"),
            "{error}"
        );
        let error = resume_refused(&tree, &limits(), None, &handle).await;
        assert!(error.contains("rerank"), "{error}");
        let floor = TraverseConfig {
            rerank_floor: 0.5,
            ..on.clone()
        };
        let error = resume_refused(&tree, &floor, Some(&a), &handle).await;
        assert!(error.contains("floor"), "{error}");
    }

    #[tokio::test]
    async fn a_continuations_effective_policy_shows_the_weights_and_switches_it_restored() {
        let tree = Tree::new();
        // The rules run at the stop after the first pop: weigh one map down and
        // switch another off.
        let steer = SteerArgs {
            rules: Some(json!([
                {"when": {"depth": {"ge": 0}}, "then": {"map_weight": {"id": "structure.contains", "weight": 0.5}}},
                {"when": {"depth": {"ge": 0}}, "then": {"map_off": "graph.scip"}},
            ])),
            stop_every: Some(1),
            ..SteerArgs::default()
        };
        let first = tree.run(&limits(), None, &steer, None, 8).await.unwrap();
        let shown = |response: &Value, id: &str| -> (f64, bool) {
            let map = response["effective_policy"]["maps"]
                .as_array()
                .unwrap()
                .iter()
                .find(|map| map["id"] == id)
                .unwrap_or_else(|| panic!("no {id} in {response}"));
            (
                map["weight"].as_f64().unwrap(),
                map["enabled"].as_bool().unwrap(),
            )
        };
        // The first call shows the maps as registered.
        assert_eq!(shown(&first, "structure.contains"), (1.0, true));
        assert_eq!(shown(&first, "graph.scip"), (1.0, true));
        let handle = handle_of(&first);
        let second = tree
            .run(&limits(), None, &SteerArgs::default(), Some(&handle), 8)
            .await
            .unwrap();
        assert_eq!(shown(&second, "structure.contains"), (0.5, true));
        assert_eq!(shown(&second, "graph.scip"), (1.0, false));
        assert_ne!(
            first["effective_policy"]["hash"],
            second["effective_policy"]["hash"]
        );
    }

    #[test]
    fn a_handle_is_bound_to_the_workspace_and_the_index() {
        let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
        let call = |index: &'static str, root: Option<&'static str>| Call {
            scip_index: Path::new(index),
            scip_root: root.map(Path::new),
            question: "q",
            seeds: &[],
            seed_search: None,
            top_k: 8,
            maps: None,
            budget: &budget,
            steer: &steer,
            handle: None,
            lsp: None,
        };
        let stamped = |len: u64| {
            let mut loaded = loaded();
            loaded.stamp = (Some(SystemTime::UNIX_EPOCH + Duration::from_secs(5)), len);
            loaded
        };
        let base = identity_of(&call("graph.scip", Some("work/a")), &stamped(10));
        assert_eq!(
            base,
            identity_of(&call("graph.scip", Some("work/a")), &stamped(10)),
            "the same workspace and index give the same identity"
        );
        for other in [
            identity_of(&call("graph.scip", Some("work/b")), &stamped(10)),
            identity_of(&call("graph.scip", None), &stamped(10)),
            identity_of(&call("other.scip", Some("work/a")), &stamped(10)),
            identity_of(&call("graph.scip", Some("work/a")), &stamped(11)),
        ] {
            assert_ne!(base, other);
        }
        // A short hash of each, not the paths.
        assert!(!base.contains("work") && !base.contains("scip"), "{base}");
    }

    fn hit(path: &str, lines: (u32, u32), rank: usize) -> SeedHit {
        SeedHit {
            path: path.into(),
            start_line: lines.0,
            end_line: lines.1,
            content_hash: "abc123".into(),
            ordinal: None,
            rank,
            retrievers: vec!["semantic".into(), "lexical".into()],
            semantic_rank: Some(rank),
            lexical_rank: None,
            lsp_rank: None,
            fusion_score: 0.25,
            reranker_score: Some(0.75),
        }
    }

    /// The name (or node id of a chunk) and `how` of each seed the hits map to.
    fn mapped(loaded: &Loaded, question: &str, hits: &[SeedHit]) -> Vec<(String, String)> {
        map_hits(loaded, question, hits)
            .iter()
            .map(|seed| {
                let name = match seed.info["symbol"].as_str() {
                    Some(symbol) => symbol.rsplit("::").next().unwrap().to_owned(),
                    None => seed.node.to_string(),
                };
                (name, seed.info["how"].as_str().unwrap().to_owned())
            })
            .collect()
    }

    fn pair(name: &str, how: &str) -> (String, String) {
        (name.to_owned(), how.to_owned())
    }

    /// Lines are counted from 1 in chunks and from 0 in the graph: `parse_query`
    /// is on 0-based line 12, so on 1-based line 13, and ends on line 15.
    #[test]
    fn a_chunk_maps_to_the_function_it_defines_or_the_innermost_one_around_it() {
        let loaded = loaded();
        // (lines, name, how, the only seed): only some rows pin the whole list.
        for (lines, name, how, only) in [
            ((1, 10), "search", "defined", false),
            ((13, 15), "parse_query", "defined", false),
            ((16, 40), "rank_hits", "defined", false),
            // the nested function starts here, whatever lies around it
            ((21, 23), "score_inner", "defined", false),
            ((13, 13), "parse_query", "defined", true),
            // past the name, inside the body; line 15 is its last line
            ((14, 14), "parse_query", "inside", true),
            ((15, 15), "parse_query", "inside", true),
            ((16, 16), "rank_hits", "defined", true),
            // inside `rank_hits`, before the nested function
            ((17, 19), "rank_hits", "inside", true),
            // inside the nested function: it, not the function around it
            ((22, 23), "score_inner", "inside", true),
            // past the nested function, inside the outer one again
            ((27, 30), "rank_hits", "inside", true),
            // straddling the end of the nested function: only the outer covers it
            ((23, 28), "rank_hits", "inside", true),
        ] {
            let got = mapped(&loaded, "x", &[hit("src/search.rs", lines, 1)]);
            assert_eq!(got.first(), Some(&pair(name, how)), "{lines:?}: {got:?}");
            assert!(!only || got.len() == 1, "{lines:?}: {got:?}");
        }
        // Line 12 (1-based) is the gap line before `parse_query`: no function.
        assert_eq!(
            mapped(&loaded, "x", &[hit("src/search.rs", (12, 12), 1)])[0].1,
            "chunk"
        );
        // A chunk in the other file.
        let got = mapped(&loaded, "x", &[hit("src/other.rs", (4, 6), 1)]);
        assert_eq!(got, [pair("helper", "defined")]);
    }

    #[tokio::test]
    async fn a_chunk_seed_between_functions_is_expanded_by_the_containment_map() {
        let loaded = loaded();
        // Line 12 (1-based) is the gap before `parse_query`: a chunk seed.
        let hits = [
            hit("src/search.rs", (11, 12), 1),
            hit("src/other.rs", (4, 6), 2),
        ];
        let seeds = map_hits(&loaded, "x", &hits);
        let found = Seeds {
            nodes: seeds
                .iter()
                .map(|seed| Seed {
                    node: seed.node.clone(),
                    map: Some(SEEDS_HYBRID),
                })
                .collect(),
            details: seeds.iter().map(|seed| seed.info.clone()).collect(),
            ..Seeds::default()
        };
        // Only the chunk node has lines to give.
        let lines = chunk_lines(&found);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!((lines[0].0.kind(), lines[0].1), (NodeKind::Chunk, (11, 12)));

        let map = loaded.contains.with_chunk_lines(lines.clone());
        let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
        let budget = Budget {
            deadline: Instant::now() + Duration::from_secs(5),
            max_external_calls: u32::MAX,
            max_nodes_touched: u64::MAX,
        };
        let edges = map.expand(&lines[0].0, budget, &view).await.edges;
        // No function lies on those lines: all the file's functions follow.
        assert_eq!(edges.len(), 9, "{edges:?}");
        assert!(
            edges
                .iter()
                .all(|e| e.kind == EdgeKind::Contains && e.map == CONTAINS_MAP_ID)
        );
    }

    #[test]
    fn a_chunk_between_functions_or_in_an_unknown_file_stays_a_chunk_node() {
        let loaded = loaded();
        // The gap after `search` (1-based 11 and 12) belongs to no function.
        let seeds = map_hits(&loaded, "x", &[hit("src/search.rs", (11, 12), 1)]);
        assert_eq!(seeds.len(), 1);
        assert_eq!(seeds[0].info["kind"], "chunk");
        assert_eq!(
            seeds[0].node,
            NodeId::chunk("src/search.rs", "abc123", 0).unwrap()
        );
        // Past the last function; and a file the index does not list.
        for (path, lines) in [("src/search.rs", (60, 70)), ("src/missing.rs", (1, 9))] {
            let seeds = map_hits(&loaded, "x", &[hit(path, lines, 1)]);
            assert_eq!(seeds[0].info["kind"], "chunk", "{path}");
            assert_eq!(seeds[0].node.path(), Some(path));
        }
        // The ordinal of the chunk is part of its id.
        let mut second = hit("src/missing.rs", (1, 9), 1);
        second.ordinal = Some(1);
        assert_eq!(
            map_hits(&loaded, "x", &[second])[0].node,
            NodeId::chunk("src/missing.rs", "abc123", 1).unwrap()
        );
        // A chunk with no usable hash falls back to its file; none at all
        // to no seed.
        let mut bare = hit("src/missing.rs", (1, 9), 1);
        bare.content_hash.clear();
        let seeds = map_hits(&loaded, "x", &[bare]);
        assert_eq!(seeds[0].node, NodeId::file("src/missing.rs").unwrap());
        assert_eq!(seeds[0].info["kind"], "file");
        assert!(map_hits(&loaded, "x", &[hit("", (1, 9), 1)]).is_empty());
    }

    #[test]
    fn cards_get_a_site_where_the_source_knows_one() {
        let loaded = loaded();
        let seeds = map_hits(
            &loaded,
            "x",
            &[
                hit("src/search.rs", (11, 12), 1),
                hit("src/search.rs", (2, 2), 2),
            ],
        );
        let symbol = seeds.iter().find(|s| s.info["kind"] == "symbol").unwrap();
        let chunk = seeds.iter().find(|s| s.info["kind"] == "chunk").unwrap();
        let unknown = NodeId::ConfigKey {
            namespace: lci_core::node::ConfigNamespace::Toml,
            key: "nope.key".into(),
        }
        .to_string();
        let file = NodeId::file("src/elsewhere.rs").unwrap().to_string();
        let card = |node: &str| json!({"node": node, "via": null});
        let mut response = json!({
            "results": [
                card(&symbol.node.to_string()),
                card(&chunk.node.to_string()),
                card(&unknown),
                card(&file),
            ],
            "checkpoint": {
                "answer": [card(&symbol.node.to_string())],
                "visited_cards": [],
                "frontier": [card(&symbol.node.to_string())],
            },
        });
        let details: Vec<Value> = seeds.iter().map(|s| s.info.clone()).collect();
        add_sites(&mut response, &loaded, None, &details);
        let site = &response["results"][0]["site"];
        assert_eq!(site["path"], "src/search.rs");
        assert!(site["line"].as_u64().unwrap() >= 1 && site["col"].as_u64().unwrap() >= 1);
        // A chunk node: its file and the first line of the chunk.
        assert_eq!(
            response["results"][1]["site"],
            json!({"path": "src/search.rs", "line": 11, "col": 1})
        );
        // Nothing locates this one, so the field is left out.
        assert!(response["results"][2].get("site").is_none());
        // A file node is at its first line, wherever it is.
        assert_eq!(
            response["results"][3]["site"],
            json!({"path": "src/elsewhere.rs", "line": 1, "col": 1})
        );
        // The checkpoint's cards carry the same site as the results'.
        assert_eq!(response["checkpoint"]["answer"][0]["site"], *site);
        assert_eq!(response["checkpoint"]["frontier"][0]["site"], *site);
    }

    #[test]
    fn a_seed_has_its_site_on_a_run_that_returns_no_checkpoint() {
        let loaded = loaded();
        let seeds = map_hits(&loaded, "x", &[hit("src/search.rs", (13, 15), 1)]);
        let symbol = seeds.iter().find(|s| s.info["kind"] == "symbol").unwrap();
        let given = NodeId::file("src/given.rs").unwrap().to_string();
        // A complete run: the seeds are not results, and there is no checkpoint.
        let mut response = json!({
            "seeds": [symbol.node.to_string(), given],
            "results": [],
        });
        let details: Vec<Value> = seeds.iter().map(|s| s.info.clone()).collect();
        add_sites(&mut response, &loaded, None, &details);
        let sites = &response["seed_sites"];
        assert_eq!(sites[symbol.node.to_string()]["path"], "src/search.rs");
        assert!(sites[symbol.node.to_string()]["line"].as_u64().unwrap() >= 1);
        assert_eq!(
            sites[&given],
            json!({"path": "src/given.rs", "line": 1, "col": 1})
        );
    }

    #[test]
    fn a_symbol_outside_the_graph_is_located_by_scip_impact() {
        // A struct is no node of the SCIP graph (functions only); scip.impact
        // gives it an id of its own and knows where it is defined.
        let symbol = "rust-analyzer cargo demo 0.1.0 types/Config#".to_owned();
        let docs = vec![RawDocument {
            path: "src/types.rs".into(),
            occurrences: vec![RawOccurrence {
                range: (2, 11, 2, 17),
                symbol: symbol.clone(),
                roles: 1,
                enclosing: Some((2, 0, 4, 1)),
            }],
        }];
        let kinds = HashMap::from([(
            symbol.clone(),
            scip::types::symbol_information::Kind::Struct,
        )]);
        let impact = ImpactMap::new(
            ImpactIndex::build(docs, &kinds, None),
            &|_| None,
            ImpactOptions::default(),
        );
        let node = impact.node_of(&symbol).unwrap().to_string();
        let loaded = loaded();
        let mut response = json!({"results": [{"node": node, "via": "scip.impact"}]});
        add_sites(&mut response, &loaded, None, &[]);
        assert!(response["results"][0].get("site").is_none());
        add_sites(&mut response, &loaded, Some(&impact), &[]);
        assert_eq!(
            response["results"][0]["site"],
            json!({"path": "src/types.rs", "line": 3, "col": 12})
        );
    }

    #[test]
    fn a_chunk_with_many_definitions_keeps_the_ones_the_question_names() {
        let loaded = loaded();
        // Lines 41 to 45 (1-based) hold five one-line functions.
        let hits = [hit("src/search.rs", (41, 45), 1)];
        let got = mapped(&loaded, "save user and find user", &hits);
        assert_eq!(
            got,
            [
                pair("load_user", "defined"),
                pair("save_user", "defined"),
                pair("find_user", "defined"),
            ],
            "the two named ones, then the first by line among the rest, in line order"
        );
        let seeds = map_hits(&loaded, "save user and find user", &hits);
        assert!(seeds.iter().all(|s| s.info["symbols_in_chunk"] == 5));
        // A question that names none keeps the first three by line.
        let got = mapped(&loaded, "zebra", &hits);
        assert_eq!(
            got.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["load_user", "save_user", "drop_user"]
        );
    }

    #[test]
    fn hits_that_share_a_node_make_one_seed_and_a_windows_path_matches() {
        let loaded = loaded();
        let hits = [
            hit("src/search.rs", (13, 15), 1),
            hit("src\\other.rs", (4, 6), 2),
            hit("src/search.rs", (14, 14), 3),
        ];
        let seeds = map_hits(&loaded, "x", &hits);
        assert_eq!(seeds.len(), 2);
        assert_eq!(seeds[0].info["hit_rank"], 1);
        assert_eq!(seeds[0].info["also_hit_ranks"], json!([3]));
        let helper = seeds[1].info["symbol"].as_str().unwrap();
        assert!(helper.ends_with("helper"), "{helper}");
    }

    #[test]
    fn a_seed_records_the_retrievers_and_the_score_that_found_it() {
        let loaded = loaded();
        let mut found = hit("src/search.rs", (13, 15), 4);
        found.retrievers = vec!["lexical".into()];
        found.semantic_rank = None;
        found.lexical_rank = Some(2);
        found.reranker_score = None;
        found.fusion_score = 0.5;
        let seeds = map_hits(&loaded, "x", &[found]);
        let info = &seeds[0].info;
        assert_eq!(info["kind"], "symbol");
        assert_eq!(info["retrievers"], json!(["lexical"]));
        assert_eq!(info["score"], 0.5);
        assert_eq!(info["score_kind"], "fusion", "no reranker score: fusion");
        assert_eq!(info["hit_rank"], 4);
        assert_eq!(info["channel_ranks"]["lexical"], 2);
        assert_eq!(info["channel_ranks"]["semantic"], json!(null));
        assert_eq!(info["path"], "src/search.rs");
        assert_eq!(info["lines"], json!([13, 15]));
        // With a reranker score, that is the score.
        let seeds = map_hits(&loaded, "x", &[hit("src/search.rs", (13, 15), 1)]);
        assert_eq!(seeds[0].info["score"], 0.75);
        assert_eq!(seeds[0].info["score_kind"], "reranker");
    }

    /// A search with a canned answer that counts its calls.
    struct Canned {
        answer: std::result::Result<Vec<SeedHit>, String>,
        calls: AtomicUsize,
        asked: Mutex<Vec<(String, usize)>>,
    }

    impl Canned {
        fn new(answer: std::result::Result<Vec<SeedHit>, String>) -> Self {
            Self {
                answer,
                calls: AtomicUsize::new(0),
                asked: Mutex::new(Vec::new()),
            }
        }
    }

    #[lci_maps::async_trait]
    impl SeedSearch for Canned {
        async fn search(&self, question: &str, k: usize) -> Result<Vec<SeedHit>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.asked.lock().unwrap().push((question.to_owned(), k));
            self.answer.clone().map_err(|e| anyhow!("{e}"))
        }
    }

    async fn seeds_for(
        search: Option<&dyn SeedSearch>,
        question: &str,
        given: &[String],
        wanted: &[MapId],
    ) -> Seeds {
        let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
        let call = Call {
            scip_index: Path::new("unused"),
            scip_root: None,
            question,
            seeds: given,
            seed_search: search,
            top_k: 8,
            maps: None,
            budget: &budget,
            steer: &steer,
            handle: None,
            lsp: None,
        };
        find_seeds(&limits(), &call, &loaded(), wanted)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn the_hybrid_search_finds_the_seeds_and_asks_for_seed_k_hits() {
        let search = Canned::new(Ok(vec![
            hit("src/search.rs", (13, 15), 1),
            hit("src/other.rs", (4, 6), 2),
        ]));
        let found = seeds_for(Some(&search), "how are queries parsed", &[], &MAPS).await;
        assert_eq!(found.source, "hybrid");
        assert!(found.fallback.is_none());
        assert_eq!(found.nodes.len(), 2);
        assert!(found.nodes.iter().all(|s| s.map == Some(SEEDS_HYBRID)));
        assert_eq!(found.details.len(), 2);
        assert_eq!(
            *search.asked.lock().unwrap(),
            [("how are queries parsed".to_owned(), 8)],
            "[traverse] seed_k defaults to 8"
        );
    }

    #[tokio::test]
    async fn the_stand_in_is_a_visible_fallback_and_never_silent() {
        let named = "how does rank hits score a hit";
        let failing = Canned::new(Err("the embedder is down".into()));
        let empty = Canned::new(Ok(Vec::new()));
        for (search, why) in [
            (Some(&failing as &dyn SeedSearch), "embedder is down"),
            (Some(&empty as &dyn SeedSearch), "no hits"),
            (None, "no workspace"),
        ] {
            let found = seeds_for(search, named, &[], &MAPS).await;
            assert_eq!(found.source, "name_tokens", "{why}");
            assert!(!found.nodes.is_empty(), "{why}: the stand-in found names");
            let reason = found.fallback.as_deref().unwrap();
            assert!(reason.contains(why), "{reason}");
            assert!(found.notes[0].contains("name_tokens"), "{:?}", found.notes);
            assert!(
                found
                    .details
                    .iter()
                    .all(|d| d["retrievers"] == json!(["name_tokens"]))
            );
        }
    }

    #[tokio::test]
    async fn explicit_seeds_and_the_stand_in_alone_do_not_search() {
        let search = Canned::new(Ok(vec![hit("src/search.rs", (13, 15), 1)]));
        let id = "sym:rust:search::search/fn";
        let given = seeds_for(Some(&search), "q", &[id.to_owned()], &MAPS).await;
        assert_eq!(given.source, "given");
        assert_eq!(given.nodes.len(), 1);
        assert!(given.nodes[0].map.is_none());
        // Only the stand-in map wanted: no search, no fallback note.
        let names = seeds_for(Some(&search), "rank hits", &[], &[SEEDS_MAP, SCIP_MAP_ID]).await;
        assert_eq!(names.source, "name_tokens");
        assert!(names.fallback.is_none() && names.notes.is_empty());
        assert_eq!(search.calls.load(Ordering::SeqCst), 0);
        // Hybrid wanted, nothing found anywhere, no stand-in: the reason is
        // kept and the source says hybrid.
        let none = seeds_for(None, "zebra", &[], &[SEEDS_HYBRID, SCIP_MAP_ID]).await;
        assert!(none.nodes.is_empty() && none.source == "hybrid");
        assert!(none.fallback.is_some());
        let nothing = seeds_for(None, "zebra", &[], &[SCIP_MAP_ID]).await;
        assert_eq!(nothing.source, "none");
    }

    #[tokio::test]
    async fn the_tree_maps_are_named_not_default_and_need_the_root() {
        assert!(OPT_IN_MAPS.iter().all(|id| !MAPS.contains(id)));
        let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
        let run_with = |maps: &[&str]| {
            let maps: Vec<String> = maps.iter().map(|id| (*id).to_owned()).collect();
            let (budget, steer) = (budget.clone(), steer.clone());
            async move {
                let dirs = Dirs::under(Path::new("unused"));
                let call = Call {
                    scip_index: Path::new("unused"),
                    scip_root: None,
                    question: "q",
                    seeds: &[],
                    seed_search: None,
                    top_k: 8,
                    maps: Some(&maps),
                    budget: &budget,
                    steer: &steer,
                    handle: None,
                    lsp: None,
                };
                run(&limits(), &dirs, &call)
                    .await
                    .err()
                    .map(|e| e.to_string())
            }
        };
        let no_root = run_with(&["graph.scip", "grep.live"]).await.unwrap();
        assert!(no_root.contains("give its root"), "{no_root}");
        let no_root = run_with(&["config.key"]).await.unwrap();
        assert!(no_root.contains("give its root"), "{no_root}");
        for id in [
            SEARCH_LEXICAL_ID,
            SEARCH_VECTOR_ID,
            SCIP_IMPACT_ID,
            LSP_LIVE_ID,
            EXACT_CLOSURE_ID,
            EXACT_PICKAXE_ID,
        ] {
            assert!(!MAPS.contains(&id), "{id} is opt-in");
            let no_root = run_with(&[id]).await.unwrap();
            assert!(no_root.contains("give its root"), "{id}: {no_root}");
        }
        let unknown = run_with(&["nope"]).await.unwrap();
        assert!(
            unknown.contains("grep.live")
                && unknown.contains("config.key")
                && unknown.contains("search.lexical")
                && unknown.contains("search.vector")
                && unknown.contains("scip.impact"),
            "{unknown}"
        );
    }

    /// One call over the three-file tree: the given maps and first-stage keys.
    async fn first_stage_run(tree: &Tree, limits: &TraverseConfig, maps: Option<&[&str]>) -> Value {
        let maps: Option<Vec<String>> =
            maps.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect());
        let (budget, steer) = (
            BudgetArgs {
                pops: Some(1),
                ..BudgetArgs::default()
            },
            SteerArgs::default(),
        );
        let call = Call {
            scip_index: &tree.index,
            scip_root: Some(tree.dir.path()),
            question: "q",
            seeds: &tree.seeds,
            seed_search: None,
            top_k: 8,
            maps: maps.as_deref(),
            budget: &budget,
            steer: &steer,
            handle: None,
            lsp: None,
        };
        run(limits, &Dirs::under(tree.dir.path()), &call)
            .await
            .unwrap()
            .response
    }

    #[tokio::test]
    async fn the_first_stage_is_off_by_default_and_leaves_the_output_as_it_was() {
        let tree = Tree::new();
        let response = first_stage_run(&tree, &limits(), None).await;
        let results = response["results"].as_array().expect("results");
        assert!(
            results.iter().all(|card| card.get("origin").is_none()),
            "{response}"
        );
        // The other keys do nothing while the mode is off.
        let others = TraverseConfig {
            first_k: 3,
            answer_seeds: true,
            ..limits()
        };
        let same = first_stage_run(&tree, &others, None).await;
        assert_eq!(same["results"], response["results"]);
        assert!(same["totals"].get("first_stage").is_none(), "{same}");
        assert!(
            response["totals"].get("first_stage").is_none(),
            "{response}"
        );
        assert!(response.get("exact").is_none(), "{response}");

        // Naming the exact maps without turning the first stage on changes
        // no card either; the report says what each map answered.
        let named =
            first_stage_run(&tree, &limits(), Some(&["graph.scip", EXACT_CLOSURE_ID])).await;
        assert!(
            named["results"]
                .as_array()
                .unwrap()
                .iter()
                .all(|card| card.get("origin").is_none()),
            "{named}"
        );
        assert!(named["totals"].get("first_stage").is_none(), "{named}");
        assert_eq!(named["exact"][0]["map"], EXACT_CLOSURE_ID, "{named}");
        assert_eq!(named["exact"][0]["answered"], false, "{named}");
    }

    #[tokio::test]
    async fn an_exact_map_that_resolves_nothing_abstains_with_a_note() {
        let tree = Tree::new();
        let limits = TraverseConfig {
            first_stage: TraverseFirstStage::Append,
            answer_seeds: true,
            ..limits()
        };
        let response =
            first_stage_run(&tree, &limits, Some(&["graph.scip", EXACT_CLOSURE_ID])).await;
        let notes = response["notes"].to_string();
        assert!(notes.contains("exact.closure"), "{response}");
        assert_eq!(response["exact"][0]["answered"], false, "{response}");
        // The first stage is the seeds, then the walk's cards after them.
        let results = response["results"].as_array().expect("results");
        let origins: Vec<&str> = results
            .iter()
            .map(|card| card["origin"].as_str().expect("origin"))
            .collect();
        assert!(origins.contains(&"seed"), "{origins:?}");
        let first_walk = origins.iter().position(|o| *o == "walk");
        let last_seed = origins.iter().rposition(|o| *o == "seed").unwrap();
        assert!(
            first_walk.is_none_or(|walk| walk > last_seed),
            "{origins:?}"
        );
        let counts = &response["totals"]["first_stage"];
        assert_eq!(
            counts["seeds"].as_u64().unwrap() as usize,
            origins.iter().filter(|o| **o == "seed").count(),
            "{response}"
        );
        // No node is listed twice.
        let mut nodes: Vec<&str> = results
            .iter()
            .map(|card| card["node"].as_str().unwrap())
            .collect();
        let listed = nodes.len();
        nodes.sort_unstable();
        nodes.dedup();
        assert_eq!(nodes.len(), listed, "{response}");
    }

    #[tokio::test]
    async fn a_continuation_does_not_run_the_first_stage_again() {
        let tree = Tree::new();
        let named = ["graph.scip", EXACT_CLOSURE_ID];
        let limits = TraverseConfig {
            first_stage: TraverseFirstStage::Append,
            answer_seeds: true,
            ..limits()
        };
        let first = first_stage_run(&tree, &limits, Some(&named)).await;
        if first.get("handle").is_none() {
            return; // the walk finished within one pop: nothing to continue
        }
        let handle = handle_of(&first);
        let maps: Vec<String> = named.iter().map(|id| (*id).to_owned()).collect();
        let (budget, steer) = (
            BudgetArgs {
                pops: Some(1),
                ..BudgetArgs::default()
            },
            SteerArgs::default(),
        );
        let call = Call {
            scip_index: &tree.index,
            scip_root: Some(tree.dir.path()),
            question: "q",
            seeds: &[],
            seed_search: None,
            top_k: 8,
            maps: Some(&maps),
            budget: &budget,
            steer: &steer,
            handle: Some(&handle),
            lsp: None,
        };
        let next = run(&limits, &Dirs::under(tree.dir.path()), &call)
            .await
            .unwrap()
            .response;
        assert!(
            next["notes"].to_string().contains("not run again"),
            "{next}"
        );
        assert!(next["totals"].get("first_stage").is_none(), "{next}");
    }

    // ---- the exact operators answering (slice BR4b) -----------------------

    /// The source of the tree the exact operators are asked about.
    const EXACT_SOURCE: &str = "struct Foo;\nfn alpha() { let _ = Foo; }\nfn beta() { Foo; Foo; }\nstruct Holder { f: Foo }\n";

    /// A tree with `a.rs` (see [`EXACT_SOURCE`]) and a SCIP index over it:
    /// `alpha` holds one reference to `Foo`, `beta` two, `Holder` (a type, so
    /// not a node of the SCIP graph) one.
    struct ScipTree {
        dir: tempfile::TempDir,
        index: PathBuf,
    }

    impl ScipTree {
        #[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
        fn new() -> Self {
            use scip::types::symbol_information::Kind;
            use scip::types::{Document, Index, Occurrence, SymbolInformation};
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("a.rs"), EXACT_SOURCE).unwrap();
            let sym = |path: &str| format!("rust-analyzer cargo demo 0.1.0 {path}");
            let occurrence = |range: &[i32], symbol: &str, roles: i32, enclosing: &[i32]| {
                let mut o = Occurrence::default();
                o.range = range.to_vec();
                o.symbol = sym(symbol);
                o.symbol_roles = roles;
                o.enclosing_range = enclosing.to_vec();
                o
            };
            let info = |symbol: &str, kind: Kind| {
                let mut i = SymbolInformation::default();
                i.symbol = sym(symbol);
                i.kind = kind.into();
                i
            };
            let mut document = Document::default();
            document.relative_path = "a.rs".to_owned();
            document.occurrences = vec![
                occurrence(&[0, 7, 10], "a/Foo#", 1, &[0, 0, 0, 11]),
                occurrence(&[1, 3, 8], "a/alpha().", 1, &[1, 0, 1, 27]),
                occurrence(&[1, 21, 24], "a/Foo#", 0, &[]),
                occurrence(&[2, 3, 7], "a/beta().", 1, &[2, 0, 2, 23]),
                occurrence(&[2, 12, 15], "a/Foo#", 0, &[]),
                occurrence(&[2, 17, 20], "a/Foo#", 0, &[]),
                occurrence(&[3, 7, 13], "a/Holder#", 1, &[3, 0, 3, 24]),
                occurrence(&[3, 19, 22], "a/Foo#", 0, &[]),
            ];
            document.symbols = vec![
                info("a/Foo#", Kind::Struct),
                info("a/alpha().", Kind::Function),
                info("a/beta().", Kind::Function),
                info("a/Holder#", Kind::Struct),
            ];
            let mut message = Index::default();
            message.documents = vec![document];
            let index = dir.path().join("index.scip");
            scip::write_message_to_file(&index, message).unwrap();
            Self { dir, index }
        }

        /// `a.rs` in a git repository of one commit.
        fn commit(&self) {
            let git = |args: &[&str]| {
                let out = std::process::Command::new("git")
                    .arg("-C")
                    .arg(self.dir.path())
                    .args(["-c", "user.name=t", "-c", "user.email=t@example.com"])
                    .args(["-c", "commit.gpgsign=false"])
                    .args(args)
                    .output()
                    .unwrap();
                assert!(out.status.success(), "git {args:?}: {out:?}");
            };
            git(&["init", "-q"]);
            git(&["add", "a.rs"]);
            git(&["commit", "-q", "-m", "add Foo"]);
        }

        async fn run(
            &self,
            limits: &TraverseConfig,
            maps: &[&str],
            seeds: &[String],
            deadline_ms: Option<u64>,
        ) -> Value {
            let maps: Vec<String> = maps.iter().map(|id| (*id).to_owned()).collect();
            let (budget, steer) = (
                BudgetArgs {
                    pops: Some(3),
                    deadline_ms,
                    ..BudgetArgs::default()
                },
                SteerArgs::default(),
            );
            let call = Call {
                scip_index: &self.index,
                scip_root: Some(self.dir.path()),
                question: if maps.iter().any(|id| id == EXACT_PICKAXE_ID) {
                    "Which commit first created `Foo`?"
                } else {
                    "What breaks if `Foo` changes?"
                },
                seeds,
                seed_search: None,
                top_k: 8,
                maps: Some(&maps),
                budget: &budget,
                steer: &steer,
                handle: None,
                lsp: None,
            };
            run(limits, &Dirs::under(self.dir.path()), &call)
                .await
                .unwrap()
                .response
        }
    }

    fn stage_limits(exact_seeds: bool, answer_seeds: bool) -> TraverseConfig {
        TraverseConfig {
            first_stage: TraverseFirstStage::Append,
            exact_seeds,
            answer_seeds,
            ..limits()
        }
    }

    fn listed(response: &Value) -> Vec<(String, String)> {
        response["results"]
            .as_array()
            .expect("results")
            .iter()
            .map(|card| {
                (
                    card["node"].as_str().unwrap().to_owned(),
                    card["origin"].as_str().unwrap().to_owned(),
                )
            })
            .collect()
    }

    #[tokio::test]
    async fn exact_closure_answers_a_question_that_starts_no_walk() {
        let tree = ScipTree::new();
        let maps = ["graph.scip", EXACT_CLOSURE_ID];
        let response = tree
            .run(&stage_limits(false, false), &maps, &[], None)
            .await;
        // Nothing the walk can start from: the first stage is the answer, under
        // `totals` as on every response.
        assert_eq!(response["status"], "no_seeds", "{response}");
        assert_eq!(response["totals"]["first_stage"]["exact"], 3, "{response}");
        assert_eq!(response["totals"]["first_stage"]["walk"], 0, "{response}");
        assert!(response.get("first_stage").is_none(), "{response}");
        let results = response["results"].as_array().unwrap();
        // beta holds two references, then alpha and Holder one each.
        assert_eq!(results[0]["site"]["path"], "a.rs", "{response}");
        assert_eq!(results[0]["site"]["line"], 3, "{response}");
        assert_eq!(results[0]["held"], 2, "{response}");
        assert!(results.iter().all(|card| card["origin"] == "exact"));
        // Holder is not a node of the SCIP graph: its card keeps the site the
        // operator gave it (nothing else could locate it).
        let holder = results
            .iter()
            .find(|card| card["name"] == "Holder")
            .unwrap_or_else(|| panic!("no Holder card in {response}"));
        assert_eq!(holder["site"]["line"], 4, "{response}");
        assert_eq!(response["exact"][0]["answered"], true, "{response}");
        let notes = response["notes"].to_string();
        assert!(
            notes.contains("exact.closure: names 1, anchored yes"),
            "{notes}"
        );
    }

    #[tokio::test]
    async fn exact_seeds_start_the_walk_from_the_exact_nodes_the_graph_holds() {
        let tree = ScipTree::new();
        let maps = ["graph.scip", EXACT_CLOSURE_ID];
        let response = tree.run(&stage_limits(true, false), &maps, &[], None).await;
        assert_ne!(response["status"], "no_seeds", "{response}");
        assert_eq!(response["seed_source"], "exact", "{response}");
        // alpha and beta, not Holder (the graph holds functions only).
        assert_eq!(response["seeds"].as_array().unwrap().len(), 2, "{response}");
        let hits = response["seed_hits"].as_array().unwrap();
        assert!(hits.iter().all(|hit| hit["how"] == "exact"), "{response}");
        // The exact cards lead, each node once, and the walk's cards come
        // after them; a card's site is where it is defined.
        let cards = listed(&response);
        assert!(
            cards[..3].iter().all(|(_, origin)| origin == "exact"),
            "{cards:?}"
        );
        assert!(
            cards[3..].iter().all(|(_, origin)| origin == "walk"),
            "{cards:?}"
        );
        let mut nodes: Vec<&String> = cards.iter().map(|(node, _)| node).collect();
        nodes.sort();
        nodes.dedup();
        assert_eq!(nodes.len(), cards.len(), "{cards:?}");
        let first = &response["results"][0];
        assert_eq!(first["site"]["line"], 3, "{response}");
        assert_eq!(first["site"]["col"], 4, "{response}");
    }

    #[tokio::test]
    async fn a_seed_the_exact_stage_also_lists_is_listed_once_and_not_started_twice() {
        let tree = ScipTree::new();
        let maps = ["graph.scip", EXACT_CLOSURE_ID];
        let first = tree
            .run(&stage_limits(false, false), &maps, &[], None)
            .await;
        let beta = first["results"][0]["node"].as_str().unwrap().to_owned();
        // The given seed is also the first exact card: with answer_seeds it is
        // listed once, as the exact card (the first stage lists its own first).
        let response = tree
            .run(
                &stage_limits(true, true),
                &maps,
                std::slice::from_ref(&beta),
                None,
            )
            .await;
        let cards = listed(&response);
        assert_eq!(
            cards.iter().filter(|(node, _)| *node == beta).count(),
            1,
            "{cards:?}"
        );
        assert_eq!(cards[0], (beta.clone(), "exact".to_owned()), "{cards:?}");
        assert_eq!(response["totals"]["first_stage"]["seeds"], 0, "{response}");
        // exact_seeds adds alpha and leaves beta the one start node it was.
        let seeds = response["seeds"].as_array().unwrap();
        assert_eq!(seeds.len(), 2, "{response}");
        assert_eq!(seeds[0], beta.as_str(), "{response}");
    }

    #[tokio::test]
    async fn exact_pickaxe_answers_with_commits_and_counts_its_git_calls() {
        let tree = ScipTree::new();
        tree.commit();
        let maps = ["graph.scip", EXACT_PICKAXE_ID];
        let limits = stage_limits(false, false);
        let response = tree.run(&limits, &maps, &[], None).await;
        let results = response["results"].as_array().unwrap();
        assert_eq!(results.len(), 1, "{response}");
        assert!(
            results[0]["node"].as_str().unwrap().starts_with("commit:"),
            "{response}"
        );
        assert_eq!(results[0]["origin"], "exact", "{response}");
        let git_calls = response["exact"][0]["git_calls"].as_u64().unwrap();
        assert!(git_calls >= 1, "{response}");
        assert_eq!(
            response["totals"]["physical_external_calls"], git_calls,
            "{response}"
        );
        let notes = response["notes"].to_string();
        assert!(
            notes.contains("exact.pickaxe: class H1, 1 commits"),
            "{notes}"
        );

        // With a walk, the commit leads the results and the git calls add to
        // the totals the walk reports.
        let closure = ["graph.scip", EXACT_CLOSURE_ID];
        let first = tree
            .run(&stage_limits(false, false), &closure, &[], None)
            .await;
        let beta = first["results"][0]["node"].as_str().unwrap().to_owned();
        let walked = tree
            .run(&limits, &maps, std::slice::from_ref(&beta), None)
            .await;
        let cards = listed(&walked);
        assert!(cards[0].0.starts_with("commit:"), "{cards:?}");
        assert_eq!(cards[0].1, "exact");
        assert_eq!(walked["totals"]["first_stage"]["exact"], 1, "{walked}");
        assert!(
            walked["totals"]["physical_external_calls"]
                .as_u64()
                .unwrap()
                >= git_calls,
            "{walked}"
        );
    }

    #[tokio::test]
    async fn exact_pickaxe_stops_asking_git_when_the_deadline_has_passed() {
        let tree = ScipTree::new();
        tree.commit();
        let maps = ["graph.scip", EXACT_PICKAXE_ID];
        // The deadline is spent by the time the index is loaded: the first
        // stage runs no git, keeps what it found (nothing) and says so.
        let response = tree
            .run(&stage_limits(false, false), &maps, &[], Some(1))
            .await;
        assert_eq!(response["exact"][0]["git_calls"], 0, "{response}");
        assert_eq!(response["exact"][0]["answered"], false, "{response}");
        let notes = response["notes"].to_string();
        assert!(
            notes.contains("stopped: the first stage's time ran out"),
            "{notes}"
        );
    }

    #[tokio::test]
    async fn lsp_live_starts_the_servers_the_call_configures() {
        let tree = tempfile::tempdir().unwrap();
        let index = tree.path().join("index.scip");
        std::fs::write(&index, []).unwrap();
        let lsp = lci_core::config::Config {
            rust_analyzer_path: "lci-no-such-language-server".into(),
            ..Default::default()
        };
        let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
        let maps = [LSP_LIVE_ID.to_owned()];
        let call = Call {
            scip_index: &index,
            scip_root: Some(tree.path()),
            question: "q",
            seeds: &[],
            seed_search: None,
            top_k: 8,
            maps: Some(&maps),
            budget: &budget,
            steer: &steer,
            handle: None,
            lsp: Some(&lsp),
        };
        let dirs = Dirs::under(tree.path());
        let report = run(&limits(), &dirs, &call).await.unwrap();
        let notes = report.response["notes"].to_string();
        assert!(notes.contains("lsp.live is unavailable"), "{notes}");
    }

    #[tokio::test]
    async fn a_tie_break_file_that_cannot_be_read_is_an_error() {
        let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
        let call = Call {
            scip_index: Path::new("unused"),
            scip_root: None,
            question: "q",
            seeds: &[],
            seed_search: None,
            top_k: 8,
            maps: None,
            budget: &budget,
            steer: &steer,
            handle: None,
            lsp: None,
        };
        let limits = TraverseConfig {
            tie_break: Some("no-such-tie-break.json".into()),
            ..limits()
        };
        let dirs = Dirs::under(Path::new("unused"));
        let error = run(&limits, &dirs, &call).await.err().expect("an error");
        let error = error.to_string();
        assert!(
            error.contains("tie_break no-such-tie-break.json"),
            "{error}"
        );
    }

    #[test]
    fn bandit_settings_are_none_when_off_and_name_an_unknown_fixed_kind() {
        assert!(bandit_settings(&limits()).unwrap().is_none());
        let mut on = limits();
        on.edge_bandit = true;
        on.bandit_fixed.insert("contains".into(), 1.5);
        let settings = bandit_settings(&on).unwrap().expect("on");
        assert_eq!(settings.options.fixed[&EdgeKind::Contains], 1.5);
        on.bandit_fixed.insert("no_such_kind".into(), 1.2);
        let error = bandit_settings(&on).expect_err("an error").to_string();
        assert!(error.contains("\"no_such_kind\""), "{error}");
        assert!(error.contains("not an edge kind"), "{error}");
        assert!(
            error.contains("contains"),
            "the valid kinds are listed: {error}"
        );
    }
}
