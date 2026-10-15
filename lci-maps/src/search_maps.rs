//! The `search.lexical` and `search.vector` maps (multimap wave W0c): the
//! two retrieval channels of the hybrid search turned around, so that a
//! traversal can ask them from a node instead of from a question.
//!
//! # What they do
//!
//! From a symbol, file or chunk node:
//!
//! * `search.lexical` builds a query from the node's own name (a symbol's
//!   name; the stem of a file's name, a chunk standing for its file), runs the
//!   index's lexical search for it (ripgrep over the tree, credited lines
//!   mapped onto the snapshot's chunks and ranked by BM25, as the search of the
//!   normal mode does) and offers the best chunks. Edge kind
//!   [`EdgeKind::QueryMatch`]. A chunk that holds the name as a whole
//!   identifier has witness [`WitnessClass::Exact`], one that holds only a
//!   piece of it [`WitnessClass::Text`]. Exact is no proof of a reference
//!   (a word in a comment counts; see `grep.live`).
//! * `search.vector` takes the stored embedding of the node's chunk (the chunk
//!   that holds a symbol's name line; a chunk node itself; the first chunk of
//!   a file) and offers the chunks nearest to it. No embedding call is made
//!   when the vector is stored. When it is not (the node has no chunk in the
//!   index, or the row has no vector), the map makes one embedding call of
//!   the node's text (the chunk's code, else the node's name) and counts it as
//!   an external call of the budget; with none left it offers nothing. Edge
//!   kind [`EdgeKind::Near`], witness [`WitnessClass::Heuristic`], confidence
//!   the cosine similarity in whole percent.
//!
//! The node's own chunks are never offered: for a symbol the chunks that hold
//! its name line or lie inside its definition (when a [`DeclLocator`] knows
//! them); for a chunk the chunk itself; for a file all of its chunks. Each map
//! offers at most [`SearchMapOptions::k`] chunks (default 8). The site of an
//! edge is the first line of the chunk (0-based, column 0) with no blob: an
//! edge source fills it from the snapshot view.
//!
//! # Where the search lives
//!
//! The maps do not know LanceDB, ripgrep or the embedder: they ask a
//! [`ChunkSearch`], which the root crate implements over the workspace's index
//! (`app::traverse`, `AppChunks`). This keeps the crate dependent on
//! `lci-core` only and lets the tests run on in-memory fixtures.
//!
//! # Fail-open
//!
//! A search that fails (no index, the embedder down, ripgrep missing) gives no
//! edges for that call and is recorded: [`Failures`] counts the errors and
//! keeps the first. A seeded `query` reports it as a warning (the status
//! `Failed` when nothing else answered, `Partial` otherwise).
//!
//! # Freshness
//!
//! `search.lexical` reads the working tree and maps it onto the snapshot, as
//! the lexical channel does: [`FreshnessModel::Live`]. `search.vector` reads
//! the snapshot but answers with the chunks nearest to the node's vector among
//! all of the index, so a new, changed or deleted file anywhere can change the
//! answer for a node whose own file did not: [`FreshnessModel::Global`]. A
//! pre-merged adjacency therefore asks it again whenever the snapshot moves.

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, Evidence, Freshness, FreshnessModel, Map,
    MapDescriptor, MapHit, MapId, MapRequest, MapResponse, MapStatus, Need, Observed, SnapshotView,
    SourceLabel,
    edge::{Edge, Expanded, MapCard, QuestionType, Site, WitnessClass, cap_edges, conf_from_unit},
    live_grep::{CostLog, GrepOptions, needle_of},
    scip_graph::ScipMap,
};
use lci_core::node::{NodeId, NodeKind};
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

/// The id of [`LexicalChunks`].
pub const SEARCH_LEXICAL_ID: MapId = "search.lexical";
/// The id of [`VectorChunks`].
pub const SEARCH_VECTOR_ID: MapId = "search.vector";

/// Confidence of the best chunk that holds the name as a whole identifier,
/// and of the best that holds only a piece of it; each later rank is one
/// point lower (at most seven).
const CONF_EXACT: u8 = 60;
const CONF_TEXT: u8 = 20;
/// A search is asked for this many times `k` chunks, so that the node's own
/// chunks (all of a file's, for a file node) can be dropped.
const OVERFETCH: usize = 4;
/// Extra chunks asked for on top of the overfetch.
const OVERFETCH_PLUS: usize = 8;

#[derive(Debug, Clone)]
pub struct SearchMapOptions {
    /// Most chunks one call offers.
    pub k: usize,
    /// Names shorter than this (in bytes) make no lexical query.
    pub min_name_len: usize,
    /// Names that make no lexical query (they would return noise).
    pub skip_names: BTreeSet<String>,
}

impl Default for SearchMapOptions {
    fn default() -> Self {
        let grep = GrepOptions::default();
        Self {
            k: 8,
            min_name_len: grep.min_name_len,
            skip_names: grep.skip_names,
        }
    }
}

/// A chunk of the index as a search returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkHit {
    /// The chunk's node id.
    pub node: NodeId,
    /// Relative to the workspace, `/` separated.
    pub path: String,
    /// 1-based, inclusive (as the index stores them).
    pub start_line: u32,
    pub end_line: u32,
    pub code: String,
    /// BM25 for a lexical search, cosine similarity (0 to 1 for similar
    /// chunks) for a vector search.
    pub score: f32,
}

/// The chunks of a workspace's index, as the two maps need them. Every
/// method may fail; the maps record the failure and offer nothing.
#[crate::async_trait]
pub trait ChunkSearch: Send + Sync {
    /// The best `limit` chunks for `query`, best first (the lexical channel
    /// of the hybrid search).
    async fn lexical(&self, query: &str, limit: usize) -> Result<Vec<ChunkHit>, String>;

    /// The chunk of `path` that holds `line` (1-based), or, with no line, the
    /// first chunk of the file.
    async fn chunk_at(&self, path: &str, line: Option<u32>) -> Result<Option<ChunkHit>, String>;

    /// The stored embedding of a chunk, if the index has one.
    async fn stored_vector(&self, chunk: &NodeId) -> Result<Option<Vec<f32>>, String>;

    /// One embedding call (the configured embedder) of a document text.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, String>;

    /// The `limit` chunks nearest to `vector`, nearest first, with the cosine
    /// similarity as `score`.
    async fn nearest(&self, vector: &[f32], limit: usize) -> Result<Vec<ChunkHit>, String>;

    /// The lines (1-based, inclusive) of every chunk of the index, so that
    /// `structure.contains` can expand the chunks these maps reach.
    async fn chunk_lines(&self) -> Result<Vec<(NodeId, (u32, u32))>, String>;
}

/// Where a declaration is, as the SCIP graph knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeclSite {
    pub path: String,
    /// The line of the name, 0-based.
    pub line: u32,
    /// The first and last line of the definition, 0-based, inclusive.
    pub span: (u32, u32),
}

/// Finds where a symbol node is defined.
pub trait DeclLocator: Send + Sync {
    fn site_of(&self, node: &NodeId) -> Option<DeclSite>;
}

/// The definitions of a [`ScipMap`]'s graph nodes.
#[derive(Debug, Clone, Default)]
pub struct ScipDecls {
    by_node: HashMap<NodeId, DeclSite>,
}

impl ScipDecls {
    pub fn from_scip(map: &ScipMap) -> Self {
        let by_node = map
            .graph()
            .nodes
            .iter()
            .enumerate()
            .map(|(i, node)| {
                (
                    map.node_id(i).clone(),
                    DeclSite {
                        path: node.file.replace('\\', "/"),
                        line: node.line,
                        span: node.span,
                    },
                )
            })
            .collect();
        Self { by_node }
    }

    pub fn len(&self) -> usize {
        self.by_node.len()
    }

    pub fn is_empty(&self) -> bool {
        self.by_node.is_empty()
    }
}

impl DeclLocator for ScipDecls {
    fn site_of(&self, node: &NodeId) -> Option<DeclSite> {
        self.by_node.get(node).cloned()
    }
}

/// How many searches failed and why the first did.
#[derive(Debug, Default)]
pub struct Failures {
    count: AtomicU64,
    first: Mutex<Option<String>>,
}

impl Failures {
    fn record(&self, why: &str) {
        self.count.fetch_add(1, Ordering::Relaxed);
        let mut first = self.first.lock().unwrap_or_else(PoisonError::into_inner);
        if first.is_none() {
            *first = Some(why.to_owned());
        }
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn first(&self) -> Option<String> {
        self.first
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// The chunks that are the node itself.
enum Own {
    Nothing,
    Chunk(NodeId),
    File(String),
    Decl(DeclSite),
}

impl Own {
    fn of(node: &NodeId, decls: Option<&dyn DeclLocator>) -> Self {
        match node {
            NodeId::Chunk { .. } => Self::Chunk(node.clone()),
            NodeId::File { path } => Self::File(path.clone()),
            NodeId::Symbol(_) => decls
                .and_then(|d| d.site_of(node))
                .map_or(Self::Nothing, Self::Decl),
            _ => Self::Nothing,
        }
    }

    fn holds(&self, hit: &ChunkHit) -> bool {
        match self {
            Self::Nothing => false,
            Self::Chunk(id) => hit.node == *id,
            Self::File(path) => hit.path == *path,
            Self::Decl(site) => {
                // Lines of the hit, 0-based like the site.
                let (from, to) = (
                    hit.start_line.saturating_sub(1),
                    hit.end_line.saturating_sub(1),
                );
                hit.path == site.path
                    && ((from..=to).contains(&site.line)
                        || (from >= site.span.0 && to <= site.span.1))
            }
        }
    }
}

fn is_ident_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Whether `code` holds `ident` with no identifier character on either side.
fn holds_whole(code: &str, ident: &str) -> bool {
    let bytes = code.as_bytes();
    code.match_indices(ident).any(|(at, _)| {
        (at == 0 || !is_ident_byte(bytes[at - 1]))
            && bytes
                .get(at + ident.len())
                .is_none_or(|&b| !is_ident_byte(b))
    })
}

fn site_of_chunk(hit: &ChunkHit) -> Site {
    Site {
        path: hit.path.clone(),
        blob: String::new(),
        line: hit.start_line.saturating_sub(1),
        col: 0,
    }
}

/// What one expansion found.
struct Outcome {
    edges: Vec<Edge>,
    error: Option<String>,
    /// What the expansion cost, found or not.
    cost: Cost,
}

impl Outcome {
    fn none() -> Self {
        Self {
            edges: Vec::new(),
            error: None,
            cost: Cost::default(),
        }
    }

    /// A search that failed after `cost` was spent.
    fn failed(why: String, cost: Cost) -> Self {
        Self {
            edges: Vec::new(),
            error: Some(why),
            cost,
        }
    }
}

/// What both maps share: one expansion, and the descriptor.
#[crate::async_trait]
trait Expander: Send + Sync {
    fn descriptor(&self) -> &MapDescriptor;
    fn label(&self) -> SourceLabel;
    async fn outcome(&self, node: &NodeId, budget: Budget) -> Outcome;
}

/// A seeded query through [`Expander::outcome`]: the best edge to each chunk,
/// in confidence order, capped at the request's limit.
async fn query_via(
    map: &dyn Expander,
    request: &MapRequest<'_>,
    view: &SnapshotView<'_>,
) -> MapResponse {
    let started = Instant::now();
    let mut response = MapResponse::empty(map.descriptor(), view, MapStatus::Ok);
    let mut edges = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut external_calls = 0u32;
    for (seed, _) in request.seeds {
        let outcome = map.outcome(seed, request.budget).await;
        external_calls = external_calls.saturating_add(outcome.cost.external_calls);
        edges.extend(outcome.edges);
        errors.extend(outcome.error);
    }
    cap_edges(&mut edges, usize::MAX);
    let mut seen = HashSet::new();
    edges.retain(|edge| seen.insert(edge.dst.clone()));
    edges.truncate(request.limit);
    response.status = match (errors.first(), edges.is_empty()) {
        (Some(why), true) => MapStatus::Failed {
            warning: why.clone(),
        },
        (Some(why), false) => MapStatus::Partial {
            note: None,
            warning: Some(why.clone()),
        },
        (None, _) => MapStatus::Ok,
    };
    response.cost = Cost {
        elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
        nodes_touched: edges.len() as u64,
        external_calls,
        ..Cost::default()
    };
    if map.descriptor().freshness == FreshnessModel::Live {
        response.freshness = Freshness {
            observed: Observed::LiveTree,
            ..response.freshness
        };
    }
    let label = map.label();
    response.hits = edges
        .into_iter()
        .zip(1..)
        .map(|(edge, rank)| MapHit {
            node: edge.dst,
            rank,
            score: Some(f32::from(edge.conf) / 100.0),
            evidence: vec![Evidence {
                via: Some(edge.src),
                edge: edge.kind,
                label,
                detail: edge.site.map(|s| format!("{}:{}", s.path, s.line + 1)),
            }],
        })
        .collect();
    response
}

// ---------------------------------------------------------------- lexical

/// `search.lexical`; see the module notes.
pub struct LexicalChunks<'a> {
    descriptor: MapDescriptor,
    search: Arc<dyn ChunkSearch + 'a>,
    decls: Option<Arc<dyn DeclLocator>>,
    options: SearchMapOptions,
    log: CostLog,
    failures: Failures,
}

impl<'a> LexicalChunks<'a> {
    pub fn new(search: Arc<dyn ChunkSearch + 'a>, decls: Option<Arc<dyn DeclLocator>>) -> Self {
        Self::with_options(search, decls, SearchMapOptions::default())
    }

    pub fn with_options(
        search: Arc<dyn ChunkSearch + 'a>,
        decls: Option<Arc<dyn DeclLocator>>,
        mut options: SearchMapOptions,
    ) -> Self {
        options.k = options.k.clamp(1, EXPAND_LIMIT);
        Self {
            descriptor: MapDescriptor {
                id: SEARCH_LEXICAL_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::Symbol, NodeKind::File, NodeKind::Chunk],
                emits: vec![NodeKind::Chunk],
                freshness: FreshnessModel::Live,
                source_labels: vec![SourceLabel::Lexical],
                needs: vec![Need::Ripgrep],
            },
            search,
            decls,
            options,
            log: CostLog::default(),
            failures: Failures::default(),
        }
    }

    pub fn options(&self) -> &SearchMapOptions {
        &self.options
    }

    /// The call times kept so far.
    pub fn log(&self) -> &CostLog {
        &self.log
    }

    pub fn failures(&self) -> &Failures {
        &self.failures
    }

    fn query_name(&self, node: &NodeId) -> Option<String> {
        let name = needle_of(node)?;
        (name.len() >= self.options.min_name_len && !self.options.skip_names.contains(&name))
            .then_some(name)
    }
}

#[crate::async_trait]
impl Expander for LexicalChunks<'_> {
    fn descriptor(&self) -> &MapDescriptor {
        &self.descriptor
    }

    fn label(&self) -> SourceLabel {
        SourceLabel::Lexical
    }

    async fn outcome(&self, node: &NodeId, _budget: Budget) -> Outcome {
        let Some(name) = self.query_name(node) else {
            return Outcome::none();
        };
        let started = Instant::now();
        let limit = self.options.k * OVERFETCH + OVERFETCH_PLUS;
        let hits = match self.search.lexical(&name, limit).await {
            Ok(hits) => hits,
            Err(why) => {
                let why = format!("search.lexical: {why}");
                self.failures.record(&why);
                return Outcome::failed(why, Cost::default());
            }
        };
        let own = Own::of(node, self.decls.as_deref());
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.log.record(elapsed_ms);
        let cost = Cost {
            elapsed_ms,
            nodes_touched: hits.len() as u64,
            ..Cost::default()
        };
        let mut seen = HashSet::new();
        let mut edges: Vec<Edge> = hits
            .iter()
            .filter(|hit| !own.holds(hit) && hit.node != *node && seen.insert(hit.node.clone()))
            .take(self.options.k)
            .zip(0u8..)
            .map(|(hit, place)| {
                let exact = holds_whole(&hit.code, &name);
                let (witness, base) = if exact {
                    (WitnessClass::Exact, CONF_EXACT)
                } else {
                    (WitnessClass::Text, CONF_TEXT)
                };
                Edge {
                    src: node.clone(),
                    dst: hit.node.clone(),
                    kind: EdgeKind::QueryMatch,
                    map: SEARCH_LEXICAL_ID,
                    witness,
                    conf: base.saturating_sub(place.min(7)),
                    site: Some(site_of_chunk(hit)),
                }
            })
            .collect();
        cap_edges(&mut edges, EXPAND_LIMIT);
        Outcome {
            edges,
            error: None,
            cost,
        }
    }
}

#[crate::async_trait]
impl Map for LexicalChunks<'_> {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        query_via(self, request, view).await
    }

    async fn expand(&self, node: &NodeId, budget: Budget, _view: &SnapshotView<'_>) -> Expanded {
        let outcome = self.outcome(node, budget).await;
        Expanded::new(outcome.edges, outcome.cost)
    }

    fn card(&self) -> MapCard {
        MapCard {
            expand_p50_ms: self.log.p50(),
            coverage: self.descriptor.emits.clone(),
            freshness_lag: None,
            good_for: vec![QuestionType::WhereTo, QuestionType::Impact],
        }
    }
}

// ----------------------------------------------------------------- vector

/// `search.vector`; see the module notes.
pub struct VectorChunks<'a> {
    descriptor: MapDescriptor,
    search: Arc<dyn ChunkSearch + 'a>,
    decls: Option<Arc<dyn DeclLocator>>,
    options: SearchMapOptions,
    log: CostLog,
    failures: Failures,
}

impl<'a> VectorChunks<'a> {
    pub fn new(search: Arc<dyn ChunkSearch + 'a>, decls: Option<Arc<dyn DeclLocator>>) -> Self {
        Self::with_options(search, decls, SearchMapOptions::default())
    }

    pub fn with_options(
        search: Arc<dyn ChunkSearch + 'a>,
        decls: Option<Arc<dyn DeclLocator>>,
        mut options: SearchMapOptions,
    ) -> Self {
        options.k = options.k.clamp(1, EXPAND_LIMIT);
        Self {
            descriptor: MapDescriptor {
                id: SEARCH_VECTOR_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::Symbol, NodeKind::File, NodeKind::Chunk],
                emits: vec![NodeKind::Chunk],
                freshness: FreshnessModel::Global,
                source_labels: vec![SourceLabel::Vector],
                needs: vec![Need::Embedding],
            },
            search,
            decls,
            options,
            log: CostLog::default(),
            failures: Failures::default(),
        }
    }

    pub fn options(&self) -> &SearchMapOptions {
        &self.options
    }

    /// The call times kept so far.
    pub fn log(&self) -> &CostLog {
        &self.log
    }

    pub fn failures(&self) -> &Failures {
        &self.failures
    }

    /// The chunk that stands for `node`: a chunk node itself, the chunk that
    /// holds a symbol's name line, the first chunk of a file.
    async fn anchor(&self, node: &NodeId) -> Result<Option<ChunkHit>, String> {
        match node {
            NodeId::Symbol(_) => match self.decls.as_deref().and_then(|d| d.site_of(node)) {
                Some(site) => self.search.chunk_at(&site.path, Some(site.line + 1)).await,
                None => Ok(None),
            },
            NodeId::File { path } => self.search.chunk_at(path, None).await,
            // The stored vector is looked up by id. The code is not known
            // here: a chunk without a stored vector is embedded by its name.
            NodeId::Chunk { path, .. } => Ok(Some(ChunkHit {
                node: node.clone(),
                path: path.clone(),
                start_line: 1,
                end_line: 1,
                code: String::new(),
                score: 0.0,
            })),
            _ => Ok(None),
        }
    }

    /// The vector of `node` and the external calls it took, which count
    /// whether the call worked or not.
    async fn vector_of(
        &self,
        node: &NodeId,
        budget: Budget,
    ) -> (Result<Option<Vec<f32>>, String>, u32) {
        let anchor = match self.anchor(node).await {
            Ok(anchor) => anchor,
            Err(why) => return (Err(why), 0),
        };
        let mut text = None;
        if let Some(anchor) = &anchor {
            match self.search.stored_vector(&anchor.node).await {
                Ok(Some(vector)) => return (Ok(Some(vector)), 0),
                Ok(None) => {}
                Err(why) => return (Err(why), 0),
            }
            text = Some(anchor.code.clone()).filter(|code| !code.trim().is_empty());
        }
        let text = text.or_else(|| needle_of(node));
        let Some(text) = text else {
            return (Ok(None), 0);
        };
        if budget.max_external_calls == 0 {
            return (Ok(None), 0);
        }
        (self.search.embed(&text).await.map(Some), 1)
    }
}

#[crate::async_trait]
impl Expander for VectorChunks<'_> {
    fn descriptor(&self) -> &MapDescriptor {
        &self.descriptor
    }

    fn label(&self) -> SourceLabel {
        SourceLabel::Vector
    }

    async fn outcome(&self, node: &NodeId, budget: Budget) -> Outcome {
        let started = Instant::now();
        let fail = |why: String, calls: u32| {
            let why = format!("search.vector: {why}");
            self.failures.record(&why);
            let cost = Cost {
                elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
                external_calls: calls,
                ..Cost::default()
            };
            Outcome::failed(why, cost)
        };
        let (vector, calls) = self.vector_of(node, budget).await;
        let vector = match vector {
            Ok(Some(found)) => found,
            Ok(None) => return Outcome::none(),
            Err(why) => return fail(why, calls),
        };
        let limit = self.options.k * OVERFETCH + OVERFETCH_PLUS;
        let hits = match self.search.nearest(&vector, limit).await {
            Ok(hits) => hits,
            Err(why) => return fail(why, calls),
        };
        let own = Own::of(node, self.decls.as_deref());
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.log.record(elapsed_ms);
        let cost = Cost {
            elapsed_ms,
            external_calls: calls,
            nodes_touched: hits.len() as u64,
            ..Cost::default()
        };
        let mut seen = HashSet::new();
        let mut edges: Vec<Edge> = hits
            .iter()
            .filter(|hit| !own.holds(hit) && hit.node != *node && seen.insert(hit.node.clone()))
            .take(self.options.k)
            .map(|hit| Edge {
                src: node.clone(),
                dst: hit.node.clone(),
                kind: EdgeKind::Near,
                map: SEARCH_VECTOR_ID,
                witness: WitnessClass::Heuristic,
                conf: conf_from_unit(hit.score),
                site: Some(site_of_chunk(hit)),
            })
            .collect();
        cap_edges(&mut edges, EXPAND_LIMIT);
        Outcome {
            edges,
            error: None,
            cost,
        }
    }
}

#[crate::async_trait]
impl Map for VectorChunks<'_> {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        query_via(self, request, view).await
    }

    async fn expand(&self, node: &NodeId, budget: Budget, _view: &SnapshotView<'_>) -> Expanded {
        let outcome = self.outcome(node, budget).await;
        Expanded::new(outcome.edges, outcome.cost)
    }

    fn card(&self) -> MapCard {
        MapCard {
            expand_p50_ms: self.log.p50(),
            coverage: self.descriptor.emits.clone(),
            freshness_lag: None,
            good_for: vec![QuestionType::WhereTo],
        }
    }
}

#[cfg(test)]
mod tests;
