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

use crate::{app::App, config::TraverseConfig, filter::FilterRequest};
use anyhow::{Context, Result, anyhow, bail};
use lci_core::node::{NodeId, NodeKind};
use lci_maps::{
    Budget, Contribution, EdgeKind, Evidence, FreshnessModel, Map, MapDescriptor, MapHit, MapId,
    MapRequest, MapResponse, MapStatus, Registry, SnapshotRef, SnapshotView, SourceLabel,
    config_key::{CONFIG_KEY_MAP_ID, ConfigKeyMap, ConfigKeyOptions},
    contains::{CONTAINS_MAP_ID, ContainsMap},
    git_history::{GIT_HISTORY_ID, GitHistoryConfig, GitHistoryMap, symbol_files},
    impact::{Decl, ImpactIndex},
    live_grep::{GREP_LIVE_MAP_ID, LiveGrep, ScipSpans},
    probe_map::{PROBE_COMPILE_ID, ProbeConfig, ProbeMap},
    scip_graph::{DirSource, SCIP_MAP_ID, ScipMap, SourceText},
    scip_impact::{ImpactMap, ImpactOptions, SCIP_IMPACT_ID},
    search_maps::{
        ChunkSearch, LexicalChunks, SEARCH_LEXICAL_ID, SEARCH_VECTOR_ID, ScipDecls, VectorChunks,
    },
    traverse::{
        Seed, Traversal, TraversalBudget, TraversalResult,
        handle::HandleStore,
        handler::{Checkpoint, StopPolicy},
        ladder::Ladder,
        rules::RuleList,
        source::OnTheFly,
        steer::Steer,
        stop::Card,
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
    sync::{Arc, Mutex, OnceLock, PoisonError},
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
/// closure of the SCIP impact index).
pub const OPT_IN_MAPS: [MapId; 7] = [
    GREP_LIVE_MAP_ID,
    CONFIG_KEY_MAP_ID,
    GIT_HISTORY_ID,
    PROBE_COMPILE_ID,
    SEARCH_LEXICAL_ID,
    SEARCH_VECTOR_ID,
    SCIP_IMPACT_ID,
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

/// `budget` of a traversal request. Each value may lower the `[traverse]`
/// cap of the same name, never raise it.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetArgs {
    /// Most node expansions (pops).
    pub pops: Option<u32>,
    /// Deepest state that is expanded.
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
    /// Rung 1 of the ladder: an array of `{"when": predicate, "then": action}`
    /// rules (at most 64). Predicates: `kind`, `path_glob`, `witness`, `map`,
    /// `score`, `fanout`, `depth`, `budget_left`, `seen_before`, `all`, `any`,
    /// `not`. Actions: `prune`, `expand`, `continue`, `stop`, `{"boost": f}`,
    /// `{"map_on": id}`, `{"map_off": id}`, `{"map_weight": {"id", "weight"}}`,
    /// `{"rerank": "witness"|"depth"|"fanout"}`, `{"redirect": [node ids]}`,
    /// `{"emit": note}`. They can only shrink, reorder or redirect the walk.
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

type ConfigKeyCache =
    Mutex<HashMap<(PathBuf, PathBuf), ((Option<SystemTime>, u64), Arc<ConfigKeyMap>)>>;

/// The `config.key` map of a SCIP index and a tree, built once per index
/// stamp (a tree edit after that is not seen; the map's freshness is
/// `global`). Declarations take the ids of the SCIP graph.
async fn config_key_map(index: &Path, root: &Path, loaded: &Loaded) -> Result<Arc<ConfigKeyMap>> {
    static CACHE: OnceLock<ConfigKeyCache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = (index.to_path_buf(), root.to_path_buf());
    if let Some((stamp, map)) = cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        && *stamp == loaded.stamp
    {
        return Ok(map.clone());
    }
    let (index, root, scip) = (index.to_path_buf(), root.to_path_buf(), loaded.scip.clone());
    let map = tokio::task::spawn_blocking(move || -> Result<ConfigKeyMap> {
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
    let map = Arc::new(map);
    cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, (loaded.stamp, map.clone()));
    Ok(map)
}

type ImpactCache = Mutex<HashMap<(PathBuf, PathBuf), ((Option<SystemTime>, u64), Arc<ImpactMap>)>>;

/// The `scip.impact` map of a SCIP index and a tree, built once per index
/// stamp. Units the SCIP graph holds take its node ids.
async fn impact_map(index: &Path, root: &Path, loaded: &Loaded) -> Result<Arc<ImpactMap>> {
    static CACHE: OnceLock<ImpactCache> = OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    let key = (index.to_path_buf(), root.to_path_buf());
    if let Some((stamp, map)) = cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        && *stamp == loaded.stamp
    {
        return Ok(map.clone());
    }
    let (index, root, scip) = (index.to_path_buf(), root.to_path_buf(), loaded.scip.clone());
    let map = tokio::task::spawn_blocking(move || -> Result<ImpactMap> {
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
    let map = Arc::new(map);
    cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, (loaded.stamp, map.clone()));
    Ok(map)
}

/// Matches the question's words against symbol names. Text only: it never
/// expands a node. A stand-in for the lexical and vector channels until they
/// are wrapped as maps.
struct NameSeeds {
    descriptor: MapDescriptor,
    names: Vec<(NodeId, HashSet<String>)>,
}

impl NameSeeds {
    fn new(scip: &ScipMap) -> Self {
        let graph = scip.graph();
        Self {
            descriptor: MapDescriptor {
                id: SEEDS_MAP,
                version: 1,
                contribution: Contribution::RankList,
                accepts_text: true,
                accepts: Vec::new(),
                emits: vec![NodeKind::Symbol],
                freshness: FreshnessModel::PerFile,
                source_labels: vec![SourceLabel::Lexical],
                needs: Vec::new(),
            },
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

#[lci_maps::async_trait]
impl Map for NameSeeds {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        response.hits = self
            .matches(request.text.unwrap_or_default(), request.limit)
            .into_iter()
            .zip(1..)
            .map(|((node, _), rank)| MapHit {
                node,
                rank,
                score: None,
                evidence: vec![Evidence {
                    via: None,
                    edge: EdgeKind::QueryMatch,
                    label: SourceLabel::Lexical,
                    detail: None,
                }],
            })
            .collect();
        response
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

/// A start node and what is known of where it came from.
struct MappedSeed {
    node: NodeId,
    /// The `seed_hits` record of the response and the trace.
    info: Value,
    /// The label of the first retriever that found it.
    label: SourceLabel,
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

fn label_of(retriever: &str) -> SourceLabel {
    match retriever {
        "lexical" => SourceLabel::Lexical,
        "lsp" => SourceLabel::Lsp,
        _ => SourceLabel::Vector,
    }
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
            seeds.push(MappedSeed {
                node,
                info,
                label: hit
                    .retrievers
                    .first()
                    .map_or(SourceLabel::Vector, |r| label_of(r)),
            });
        }
    }
    seeds
}

/// The start nodes the hybrid search found, as a rank-list map (they are
/// computed before the walk; the map only reports them).
struct HybridSeeds {
    descriptor: MapDescriptor,
    hits: Vec<(NodeId, SourceLabel)>,
}

impl HybridSeeds {
    fn new(hits: Vec<(NodeId, SourceLabel)>) -> Self {
        Self {
            descriptor: MapDescriptor {
                id: SEEDS_HYBRID,
                version: 1,
                contribution: Contribution::RankList,
                accepts_text: true,
                accepts: Vec::new(),
                emits: vec![NodeKind::Symbol, NodeKind::Chunk, NodeKind::File],
                freshness: FreshnessModel::PerFile,
                source_labels: vec![SourceLabel::Vector, SourceLabel::Lexical, SourceLabel::Lsp],
                needs: Vec::new(),
            },
            hits,
        }
    }
}

#[lci_maps::async_trait]
impl Map for HybridSeeds {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        response.hits = self
            .hits
            .iter()
            .take(request.limit)
            .zip(1..)
            .map(|((node, label), rank)| MapHit {
                node: node.clone(),
                rank,
                score: None,
                evidence: vec![Evidence {
                    via: None,
                    edge: EdgeKind::QueryMatch,
                    label: *label,
                    detail: None,
                }],
            })
            .collect();
        response
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
    let (mut budget, mut notes) = clamp_budget(limits, call.budget);
    let handler = build_ladder(limits, call.steer, budget.pops, &mut notes)?;
    let actions = match (&call.steer.actions, call.handle) {
        (Some(_), None) => {
            bail!("steer.actions answers a checkpoint: give the handle it came with")
        }
        (Some(value), Some(_)) => Some(
            Steer::from_json(&json!({ "actions": value }))
                .map_err(|e| anyhow!("steer.actions: {e}"))?,
        ),
        (None, _) => None,
    };
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
    if let Some(snapshot) = &snapshot {
        snapshot
            .check_identity(&identity)
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
    let hybrid_hits = found
        .nodes
        .iter()
        .filter(|seed| seed.map == Some(SEEDS_HYBRID))
        .zip(&found.labels)
        .map(|(seed, label)| (seed.node.clone(), *label))
        .collect();
    register(
        &mut registry,
        &mut plan_maps,
        Box::new(HybridSeeds::new(hybrid_hits)),
        wants(SEEDS_HYBRID),
    )?;
    register(
        &mut registry,
        &mut plan_maps,
        Box::new(Shared(loaded.names.clone())),
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
    budget.deadline += setup.elapsed() + early;
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());

    let policy = handler.as_ref().map(|(_, _, plan)| plan.clone());
    // The maps that expand nodes: the most a state can be corroborated by,
    // which the score bound needs (the seed maps only start the walk).
    let expanding = plan_maps
        .iter()
        .filter(|map| map.enabled && map.id != SEEDS_HYBRID && map.id != SEEDS_MAP)
        .count();
    let traversal = match snapshot {
        Some(snapshot) => Traversal::resumed(OnTheFly::new(&registry, &view), snapshot, budget),
        None => {
            if found.nodes.is_empty() {
                return Ok(Report {
                    response: no_seeds_response(call, &found, notes),
                    trace: None,
                });
            }
            let plan = StartPlan {
                maps: plan_maps,
                policy: policy.unwrap_or_default(),
                seed_source: Some(found.source.to_owned()),
                seed_hits: found.details.clone(),
            };
            Traversal::new(OnTheFly::new(&registry, &view), found.nodes.clone(), budget)
                .question(call.question)
                .plan(plan)
        }
    };
    let traversal = traversal.identity(identity).top_k(call.top_k, expanding);
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
        &loaded,
        handle.as_deref(),
        trace_path.as_deref(),
        notes,
    );
    if let Some(history) = &git_history {
        add_commits(&mut response, history);
    }
    Ok(Report {
        response,
        trace: trace_path,
    })
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
    /// The label of each hybrid seed's first retriever, in order.
    labels: Vec<SourceLabel>,
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
                        found.labels.push(seed.label);
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

fn response_json(
    call: &Call<'_>,
    result: &TraversalResult,
    found: &Seeds,
    loaded: &Loaded,
    handle: Option<&str>,
    trace_path: Option<&Path>,
    notes: Vec<String>,
) -> Value {
    let mut results: Vec<(usize, &lci_maps::traverse::State)> = result
        .states
        .iter()
        .enumerate()
        .filter(|(_, state)| state.parent.is_some() && !result.cleared.contains(&state.node))
        .collect();
    let rank = |state: &lci_maps::traverse::State| result.ranked_potential(state);
    results.sort_by(|a, b| rank(b.1).total_cmp(&rank(a.1)).then(a.0.cmp(&b.0)));
    let results: Vec<Value> = results
        .into_iter()
        .take(call.top_k)
        .map(|(_, state)| {
            trace::card_json(&Card {
                node: state.node.clone(),
                via: state.via,
                witness: state.witness,
                potential: state.potential,
                depth: state.depth,
                maps: state.maps.iter().copied().collect(),
            })
        })
        .collect();
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
            "stops": totals.stops,
            "steers": totals.steers,
            "per_map": totals.per_map,
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
        if let Some(reason) = result.stop_reason {
            object.insert("stop_reason".into(), json!(reason.as_str()));
        }
        if let Some(checkpoint) = &result.checkpoint {
            object.insert("handle".into(), json!(handle));
            object.insert("checkpoint".into(), checkpoint_response(checkpoint));
        }
        if !notes.is_empty() {
            object.insert("notes".into(), json!(notes));
        }
    }
    add_sites(&mut response, loaded, &found.details);
    response
}

/// Every card of the response (the result items and the checkpoint's answer,
/// visited cards and frontier) gets `site: {path, line, col}` where the
/// source knows where the node is: the definition of a SCIP symbol (the line
/// and column of its name), the first line of the chunk of a hybrid seed that
/// is a chunk node, the start of a file node. Paths are repository-relative
/// with `/`; lines and columns count from 1, like the other line ranges of
/// `search_code`. A node nothing locates has no `site`.
fn add_sites(response: &mut Value, loaded: &Loaded, seeds: &[Value]) {
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
    use serde_json::json;
    use std::sync::Mutex;

    fn limits() -> TraverseConfig {
        TraverseConfig {
            enabled: true,
            ..TraverseConfig::default()
        }
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

    #[test]
    fn a_chunk_maps_to_the_function_whose_name_it_holds() {
        let loaded = loaded();
        for (lines, name) in [
            ((1, 10), "search"),
            ((13, 15), "parse_query"),
            ((13, 13), "parse_query"),
            ((16, 40), "rank_hits"),
            // the nested function starts here, whatever lies around it
            ((21, 23), "score_inner"),
        ] {
            let got = mapped(&loaded, "x", &[hit("src/search.rs", lines, 1)]);
            assert_eq!(
                got.first(),
                Some(&pair(name, "defined")),
                "{lines:?}: {got:?}"
            );
        }
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
    fn lines_are_counted_from_1_in_chunks_and_from_0_in_the_graph() {
        let loaded = loaded();
        // `parse_query` is on 0-based line 12, so on 1-based line 13.
        let at = |lines| mapped(&loaded, "x", &[hit("src/search.rs", lines, 1)]);
        assert_eq!(at((13, 13)), [pair("parse_query", "defined")]);
        // Line 12 (1-based) is the gap line before it: no function.
        assert_eq!(at((12, 12))[0].1, "chunk");
        // Line 14 (1-based) is inside its body, past the name.
        assert_eq!(at((14, 14)), [pair("parse_query", "inside")]);
        // Line 15 (1-based) is its last line; line 16 is the next function's.
        assert_eq!(at((15, 15)), [pair("parse_query", "inside")]);
        assert_eq!(at((16, 16)), [pair("rank_hits", "defined")]);
    }

    #[test]
    fn a_chunk_inside_a_function_maps_to_the_innermost_function_around_it() {
        let loaded = loaded();
        let at = |lines| mapped(&loaded, "x", &[hit("src/search.rs", lines, 1)]);
        // Inside `rank_hits`, before the nested function.
        assert_eq!(at((17, 19)), [pair("rank_hits", "inside")]);
        // Inside the nested function: it, not the function around it.
        assert_eq!(at((22, 23)), [pair("score_inner", "inside")]);
        // Past the nested function, inside the outer one again.
        assert_eq!(at((27, 30)), [pair("rank_hits", "inside")]);
        // Straddling the end of the nested function: only the outer covers it.
        assert_eq!(at((23, 28)), [pair("rank_hits", "inside")]);
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
        add_sites(&mut response, &loaded, &details);
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
        let (info, label) = (&seeds[0].info, seeds[0].label);
        assert_eq!(info["kind"], "symbol");
        assert_eq!(info["retrievers"], json!(["lexical"]));
        assert_eq!(info["score"], 0.5);
        assert_eq!(info["score_kind"], "fusion", "no reranker score: fusion");
        assert_eq!(info["hit_rank"], 4);
        assert_eq!(info["channel_ranks"]["lexical"], 2);
        assert_eq!(info["channel_ranks"]["semantic"], json!(null));
        assert_eq!(info["path"], "src/search.rs");
        assert_eq!(info["lines"], json!([13, 15]));
        assert_eq!(label, SourceLabel::Lexical);
        // With a reranker score, that is the score.
        let seeds = map_hits(&loaded, "x", &[hit("src/search.rs", (13, 15), 1)]);
        assert_eq!(seeds[0].info["score"], 0.75);
        assert_eq!(seeds[0].info["score_kind"], "reranker");
        assert_eq!(seeds[0].label, SourceLabel::Vector);
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
        assert_eq!(found.labels.len(), 2);
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
        for id in [SEARCH_LEXICAL_ID, SEARCH_VECTOR_ID, SCIP_IMPACT_ID] {
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
}
