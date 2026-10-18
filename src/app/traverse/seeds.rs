//! Start nodes: the name-token stand-in, the hybrid search seam, the mapping of
//! search hits to graph nodes, and the descriptor-only seed maps.

use super::first_stage::{EXACT_CLOSURE_ID, EXACT_PICKAXE_ID};
use super::{Call, SEEDS_HYBRID, SEEDS_MAP, chunks, index::Loaded};
use crate::{app::App, config::TraverseConfig, filter::FilterRequest, models::Models};
use anyhow::{Context, Result, anyhow};
use lci_core::node::{NodeId, NodeKind};
use lci_maps::{
    Contribution, FreshnessModel, Map, MapDescriptor, MapId, MapRequest, MapResponse, MapStatus,
    SnapshotView, SourceLabel,
    contains::CONTAINS_MAP_ID,
    impact::code_names,
    scip_graph::ScipMap,
    scip_implements::{ImplementsMap, SCIP_IMPLEMENTS_ID, TargetKind},
    search_maps::ChunkSearch,
    traverse::Seed,
    walk::tokenize,
};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

/// Start nodes the name-token stand-in takes from the question.
const SEED_COUNT: usize = 3;
/// Most symbols one hit of the hybrid search may start from (a chunk that
/// holds more definitions keeps the ones the question's words name best).
const MAX_SYMBOLS_PER_HIT: usize = 3;

/// Matches the question's words against symbol names: the stand-in for the
/// lexical and vector channels until they are wrapped as maps.
pub(super) struct NameSeeds {
    names: Vec<(NodeId, HashSet<String>)>,
}

impl NameSeeds {
    pub(super) fn new(scip: &ScipMap) -> Self {
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

/// A start node and what is known of where it came from.
pub(super) struct MappedSeed {
    pub(super) node: NodeId,
    /// The `seed_hits` record of the response and the trace.
    pub(super) info: Value,
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
pub(super) fn map_hits(loaded: &Loaded, question: &str, hits: &[SeedHit]) -> Vec<MappedSeed> {
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
pub(super) struct SeedMap {
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
    pub(super) fn hybrid() -> Self {
        Self::new(
            SEEDS_HYBRID,
            vec![NodeKind::Symbol, NodeKind::Chunk, NodeKind::File],
            vec![SourceLabel::Vector, SourceLabel::Lexical, SourceLabel::Lsp],
        )
    }

    /// The symbols whose names hold the question's words.
    pub(super) fn name_tokens() -> Self {
        Self::new(
            SEEDS_MAP,
            vec![NodeKind::Symbol],
            vec![SourceLabel::Lexical],
        )
    }

    /// The declarations that reference the symbol the question names.
    pub(super) fn exact_closure() -> Self {
        Self::new(
            EXACT_CLOSURE_ID,
            vec![NodeKind::Symbol],
            vec![SourceLabel::Scip],
        )
    }

    /// The commits `git log -G/-S/--grep` finds for a history question.
    pub(super) fn exact_pickaxe() -> Self {
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

fn parse_seeds(seeds: &[String]) -> Result<Vec<NodeId>> {
    seeds
        .iter()
        .map(|text| NodeId::from_str(text).map_err(|e| anyhow!("seed {text:?}: {e}")))
        .collect()
}

/// The start nodes of a call and where they came from.
#[derive(Default)]
pub(super) struct Seeds {
    pub(super) nodes: Vec<Seed>,
    /// Where the start nodes came from: `given`, `hybrid`, `name_tokens`,
    /// `traits` (the traits and trait methods the question's paths name, with
    /// `scip.implements` on), `traits+hybrid`, `traits+name_tokens` or
    /// `traits+exact` (those, with the other source's nodes after them),
    /// `exact` (the first stage's cards, set by `run`) or `none`; empty on a
    /// continuation.
    pub(super) source: &'static str,
    /// How many start nodes are the traits and trait methods the question's
    /// paths name; `Some` only when the call walks `scip.implements` (0 when
    /// no path named one), so a bench can tell them from fallbacks.
    pub(super) trait_seeds: Option<usize>,
    /// The given start nodes (`--seeds`) that are traits or trait methods of
    /// `scip.implements`: walked as trait seeds, not counted in `trait_seeds`.
    pub(super) given_traits: Vec<NodeId>,
    /// One record per start node: the retrievers that found it and its score.
    pub(super) details: Vec<Value>,
    /// Why the stand-in replaced the hybrid search, if it did.
    pub(super) fallback: Option<String>,
    pub(super) notes: Vec<String>,
    /// How long the hybrid search ran (zero when it did not). The walk's
    /// deadline does not run while it does: the search is bounded by the
    /// embedding and reranker timeouts, not by `deadline_ms`.
    pub(super) search_time: Duration,
}

impl Seeds {
    /// The start nodes that are the traits and trait methods the question
    /// named ([`add_trait_seeds`] puts them first, `trait_seeds` of them), and
    /// the given ones that are traits or trait methods ([`Self::given_traits`]);
    /// none on a continuation.
    pub(super) fn trait_nodes(&self) -> impl Iterator<Item = NodeId> + '_ {
        let count = self.trait_seeds.unwrap_or(0);
        self.nodes
            .iter()
            .take(count)
            .map(|seed| seed.node.clone())
            .chain(self.given_traits.iter().cloned())
    }
}

/// Finds the start nodes: the caller's, else the hybrid search's, else (when
/// that is unavailable or finds nothing) the name-token stand-in, and the
/// stand-in alone when it is the only seeds map wanted.
pub(super) async fn find_seeds(
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
            Some(search) => {
                let started = Instant::now();
                let hits = search.search(call.question, limits.seed_k as usize).await;
                found.search_time = started.elapsed();
                match hits {
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
                }
            }
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

/// Most start nodes the trait paths of one question give.
const MAX_TRAIT_SEEDS: usize = 6;

/// How the traits and trait methods a question names join the start nodes of
/// the other sources (the hybrid search or the stand-in).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TraitSeedMode {
    /// `Replace` when at least one start node is an exact match (the
    /// qualifiers of the written path are on the symbol's path), `Prepend`
    /// when the paths matched by their last segment alone.
    #[default]
    Auto,
    /// The trait start nodes are the only ones from the question's own
    /// sources (the first stage may still add its cards after them).
    Replace,
    /// The trait start nodes go first, the other sources' nodes after them.
    Prepend,
}

impl FromStr for TraitSeedMode {
    type Err = anyhow::Error;

    fn from_str(text: &str) -> Result<Self> {
        match text {
            "auto" => Ok(Self::Auto),
            "replace" => Ok(Self::Replace),
            "prepend" => Ok(Self::Prepend),
            other => Err(anyhow!(
                "trait seeds mode {other:?}: use auto, replace or prepend"
            )),
        }
    }
}

/// Puts the traits and trait methods the question names among the start
/// nodes, when the call walks `scip.implements`: a path in a code span of the
/// question (`tauri::Manager`, `Runtime::run`) that names a trait or a trait
/// method declaration of the map ([`ImplementsMap::find`]) starts the walk at
/// that node, from which the map's edges reach the implementing types or
/// members. Expanding such a start node follows every one of those
/// `scip.implements` edges (no gate, no beam) and ranks them ahead of its other
/// edges ([`lci_maps::traverse::frontier::hop_from`]; [`Seeds::trait_nodes`]
/// names the nodes), so no implementor is lost to the beam or to a reference
/// that prices higher. They go in front of the start nodes of the other sources
/// (`Prepend`) or take their place (`Replace`; `Auto` replaces when a path is
/// an exact match). Call it only when `scip.implements` is on; a call without
/// that map never gets here, so its start nodes are those of [`find_seeds`]
/// alone. The seeds are not credited to a map (`Seed::map` is `None`, as for
/// given seeds): no map ranks them, the question's own words named them.
///
/// Sets [`Seeds::trait_seeds`] (0 when nothing was added). When the caller
/// gave `--seeds`, none is added; the given start nodes that are targets of the
/// map are walked as trait seeds ([`Seeds::given_traits`]). Nothing changes
/// when no path names a target, or on a continuation (which does not look for
/// seeds). The source becomes `traits`, or `traits+<source>` when the other
/// start nodes stay. Returns the note the response carries, if seeds were
/// added.
pub(super) fn add_trait_seeds(
    found: &mut Seeds,
    map: &ImplementsMap,
    question: &str,
    mode: TraitSeedMode,
) -> Option<String> {
    found.trait_seeds = Some(0);
    if found.source == "given" {
        found.given_traits = found
            .nodes
            .iter()
            .filter(|seed| map.targets().iter().any(|t| t.node == seed.node))
            .map(|seed| seed.node.clone())
            .collect();
        return None;
    }
    let (names, _) = code_names(question);
    let named: Vec<_> = map
        .find(&names, MAX_TRAIT_SEEDS)
        .into_iter()
        .take(MAX_TRAIT_SEEDS)
        .collect();
    if named.is_empty() {
        return None;
    }
    let exact = named.iter().filter(|hit| hit.exact).count();
    let replace = match mode {
        TraitSeedMode::Replace => true,
        TraitSeedMode::Prepend => false,
        TraitSeedMode::Auto => exact > 0,
    };
    let mut nodes: Vec<Seed> = Vec::new();
    let mut details: Vec<Value> = Vec::new();
    for hit in &named {
        nodes.push(Seed {
            node: hit.target.node.clone(),
            map: None,
        });
        details.push(json!({
            "node": hit.target.node.to_string(),
            "kind": "symbol",
            "how": match hit.target.kind {
                TargetKind::Trait => "trait",
                TargetKind::Method => "trait_method",
            },
            "written": hit.path,
            "retrievers": [SCIP_IMPLEMENTS_ID],
            "score": u32::from(hit.exact),
            "score_kind": if hit.exact { "path_match" } else { "name_match_only" },
        }));
    }
    let count = named.len();
    let loose = count - exact;
    let before = found.source;
    let other = found.nodes.len();
    let what = if replace {
        found.nodes.clear();
        found.details.clear();
        found.source = "traits";
        if other == 0 {
            "no other source gave start nodes".to_owned()
        } else {
            format!("they replace the {other} start nodes of seed_source {before}")
        }
    } else {
        let aligned = found.details.len() == found.nodes.len();
        let taken: HashSet<NodeId> = nodes.iter().map(|seed| seed.node.clone()).collect();
        for (i, seed) in std::mem::take(&mut found.nodes).into_iter().enumerate() {
            if taken.contains(&seed.node) {
                continue;
            }
            if aligned {
                details.push(found.details[i].clone());
            }
            nodes.push(seed);
        }
        if !aligned {
            details.append(&mut found.details);
        }
        found.source = match before {
            "hybrid" => "traits+hybrid",
            "name_tokens" => "traits+name_tokens",
            "exact" => "traits+exact",
            _ => "traits",
        };
        if other == 0 {
            "no other source gave start nodes".to_owned()
        } else {
            format!("the {other} start nodes of seed_source {before} follow them")
        }
    };
    found.nodes = nodes;
    found.details = details;
    found.trait_seeds = Some(count);
    Some(format!(
        "{count} start nodes are the traits and trait methods the question's paths name ({loose} of them by the last segment alone); {what}; {SCIP_IMPLEMENTS_ID} follows their edges to the implementing types and members"
    ))
}

/// Adds `trait_seeds` (how many start nodes are the traits and trait methods
/// the question named) to a response of a call that walks `scip.implements`;
/// a call without it has no such field.
pub(super) fn add_trait_seed_count(response: &mut Value, found: &Seeds) {
    if let (Some(count), Some(object)) = (found.trait_seeds, response.as_object_mut()) {
        object.insert("trait_seeds".into(), json!(count));
    }
}
