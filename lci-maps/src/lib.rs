//! The maps, the registry and the traversal engine behind
//! `search_code mode="traverse"`, `lci traverse` and the recipes.
//!
//! A [`Map`] answers one request (a query text and/or seed nodes) with
//! ranked [`NodeId`]s and evidence. The [`Registry`] calls every enabled map,
//! turns any failure into a status instead of an error (contract C1), fuses
//! the answers with the weighted RRF arithmetic of the production search in
//! its order, and sorts with its tie rule. The [`traverse`] engine walks the
//! typed edges the maps expose. The default `search_code` ranking does not
//! call the registry: the retrieval channels are wrapped as maps in the root
//! crate and checked against `fuse_hits` bit for bit (test T0).
//!
//! History: slice S2 of the multimap spike; spec
//! `docs/spike/multimap/specs/map-trait.md`.

pub mod audit;
pub mod config_key;
pub mod contains;
pub mod crates;
pub mod docs_mentions;
pub mod edge;
pub mod exact_history;
pub mod fresh;
pub mod git_history;
pub mod impact;
pub mod kit;
pub mod kit_c3;
pub mod labels;
pub mod live_grep;
pub mod lsp_live;
pub mod probe;
pub mod probe_map;
pub mod process;
pub mod reads_key;
pub mod run_record;
pub mod scip_graph;
pub mod scip_impact;
pub mod scip_implements;
pub mod scratch;
pub mod search_maps;
pub mod structural;
pub mod symdefs;
pub mod tested_by;
pub mod traverse;
pub mod walk;

/// Re-exported so a crate that implements [`Map`] needs no direct
/// dependency on `async-trait`.
pub use async_trait::async_trait;

use futures::FutureExt;
use lci_core::{
    chunk::Chunk,
    node::{NodeId, NodeKind, chunk_ids},
};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet},
    panic::AssertUnwindSafe,
    time::{Duration, Instant},
};

/// A map's stable dotted id (`lexical`, `vec.chunk.code`, ...).
pub type MapId = &'static str;

/// The three legacy channels, fused first and in this order, so the f32
/// sums are those of `fuse_hits` (lesson L14, decision M-3).
pub const CHANNEL_ORDER: [MapId; 3] = ["vec.chunk.code", "lexical", "lsp.symbols"];

/// How fusion uses a map's output (map-trait.md 3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Contribution {
    /// Ranked hits; each adds `weight / (rrf_k + rank)`.
    RankList,
    /// A value per seed node, added unweighted after the rank lists.
    Prior,
    /// Removes nodes before fusion. Not supported by the S2 registry.
    Filter,
    /// Ranked hits reached from seeds; fused like a rank list. In S2 its
    /// seeds are empty, as for a text map.
    Expansion,
}

/// The update model of `docs/spike/multimap/engine.md`, "Maps".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FreshnessModel {
    PerFile,
    PerCommit,
    Live,
    Global,
}

/// The closed evidence vocabulary of contract C5.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceLabel {
    Vector,
    Lexical,
    Lsp,
    Scip,
    Tags,
    Heuristic,
    Git,
    Config,
    Doc,
    Agent,
    Prior,
}

/// What a map needs from outside the process, for readiness and fail-open
/// reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Need {
    Embedding,
    Reranker,
    Lsp,
    Git,
    Ripgrep,
}

/// The static description of a map.
#[derive(Debug, Clone)]
pub struct MapDescriptor {
    pub id: MapId,
    /// Bumped when the output for the same input changes.
    pub version: u32,
    pub contribution: Contribution,
    pub accepts_text: bool,
    pub accepts: Vec<NodeKind>,
    pub emits: Vec<NodeKind>,
    pub freshness: FreshnessModel,
    pub source_labels: Vec<SourceLabel>,
    pub needs: Vec<Need>,
}

/// Contract C8. Only the deadline is enforced by the S2 registry.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    pub deadline: Instant,
    pub max_external_calls: u32,
    pub max_nodes_touched: u64,
}

/// One call's input.
#[derive(Debug, Clone, Copy)]
pub struct MapRequest<'r> {
    pub text: Option<&'r str>,
    pub seeds: &'r [(NodeId, f32)],
    pub limit: usize,
    pub budget: Budget,
    pub params: &'r serde_json::Value,
    pub as_of: Option<&'r str>,
}

/// The snapshot an answer refers to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SnapshotRef {
    pub indexed_at: String,
    pub fingerprint: String,
}

/// Read-only context of one run: the snapshot and its filtered universe
/// of chunks, with their NodeIds interned to positions.
pub struct SnapshotView<'a> {
    pub snapshot: SnapshotRef,
    chunks: Vec<&'a Chunk>,
    ids: Vec<NodeId>,
    index: HashMap<NodeId, usize>,
    by_lines: HashMap<(&'a str, u32, u32, &'a str), usize>,
}

impl<'a> SnapshotView<'a> {
    /// `chunks` is the filtered universe (today's `all_chunks` after
    /// `hit_matches`).
    pub fn new(snapshot: SnapshotRef, chunks: Vec<&'a Chunk>) -> Self {
        let ids = chunk_ids(&chunks);
        let index = ids.iter().cloned().zip(0..).collect();
        let mut by_lines = HashMap::new();
        for (position, chunk) in chunks.iter().enumerate() {
            by_lines
                .entry((
                    chunk.relative_file_path.as_str(),
                    chunk.start_line,
                    chunk.end_line,
                    chunk.content_hash.as_str(),
                ))
                .or_insert(position);
        }
        Self {
            snapshot,
            chunks,
            ids,
            index,
            by_lines,
        }
    }

    pub fn len(&self) -> usize {
        self.chunks.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }

    pub fn ids(&self) -> &[NodeId] {
        &self.ids
    }

    pub fn contains(&self, node: &NodeId) -> bool {
        self.index.contains_key(node)
    }

    pub fn chunk(&self, node: &NodeId) -> Option<&'a Chunk> {
        self.index.get(node).map(|&position| self.chunks[position])
    }

    /// The NodeId of a chunk of the universe given by value (a legacy
    /// channel returns clones). Two chunks with the same path, lines and
    /// hash resolve to the first; within one snapshot only the N-5 twins can
    /// be such a pair.
    pub fn locate(&self, chunk: &Chunk) -> Option<&NodeId> {
        self.by_lines
            .get(&(
                chunk.relative_file_path.as_str(),
                chunk.start_line,
                chunk.end_line,
                chunk.content_hash.as_str(),
            ))
            .map(|&position| &self.ids[position])
    }
}

/// The kind of edge an evidence item or an [`edge::Edge`] follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum EdgeKind {
    /// The node matched the query text.
    QueryMatch,
    /// A prior value attached to the node.
    Prior,
    /// A function or method refers to another.
    CallsOrRefs,
    /// A declaration encloses another.
    Contains,
    Imports,
    Implements,
    Tests,
    /// A config key is read or set by a declaration.
    ReadsKey,
    Mentions,
    /// A commit touched a file or declaration.
    Touched,
    Cochange,
    /// A compiler error lands at the node.
    CompileErrorAt,
    /// Near in embedding space.
    Near,
    /// A declaration lies in a file or in an enclosing declaration (the
    /// reverse of `Contains`, for climbing).
    DefinedIn,
}

impl EdgeKind {
    /// The stable name used in run files and traces.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::QueryMatch => "query_match",
            Self::Prior => "prior",
            Self::CallsOrRefs => "calls_or_refs",
            Self::Contains => "contains",
            Self::Imports => "imports",
            Self::Implements => "implements",
            Self::Tests => "tests",
            Self::ReadsKey => "reads_key",
            Self::Mentions => "mentions",
            Self::Touched => "touched",
            Self::Cochange => "cochange",
            Self::CompileErrorAt => "compile_error_at",
            Self::Near => "near",
            Self::DefinedIn => "defined_in",
        }
    }

    /// Every kind, in declaration order.
    pub const ALL: [Self; 14] = [
        Self::QueryMatch,
        Self::Prior,
        Self::CallsOrRefs,
        Self::Contains,
        Self::Imports,
        Self::Implements,
        Self::Tests,
        Self::ReadsKey,
        Self::Mentions,
        Self::Touched,
        Self::Cochange,
        Self::CompileErrorAt,
        Self::Near,
        Self::DefinedIn,
    ];

    /// The kind whose [`EdgeKind::as_str`] is `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == name)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Evidence {
    pub via: Option<NodeId>,
    pub edge: EdgeKind,
    pub label: SourceLabel,
    /// Bounded free text, for example the lexical channel's credited lines.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MapHit {
    pub node: NodeId,
    /// One-based; the value fusion consumes.
    pub rank: u32,
    /// The map's own score; never enters RRF. A prior map's value.
    pub score: Option<f32>,
    pub evidence: Vec<Evidence>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum MapStatus {
    Ok,
    /// Part of the answer is missing; the hits returned are kept. A budget
    /// that ran out is a note; a source that failed while others answered
    /// (one language server of several) is a warning. At least one is set.
    /// A map with no other field for a note about a complete answer (the LSP
    /// map's kept-session or restart note, N110) also reports `Partial`; the
    /// registry fuses its hits as it does `Ok`.
    Partial {
        note: Option<String>,
        warning: Option<String>,
    },
    /// Switched off or not applicable. Silent: never a warning or a note.
    Skipped {
        reason: String,
    },
    /// Reported as a warning; any hits are ignored.
    Failed {
        warning: String,
    },
}

/// Measured for one call (map-trait.md 3.3).
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Cost {
    pub elapsed_ms: f64,
    pub external_calls: u32,
    pub nodes_touched: u64,
    pub bytes_read: u64,
    pub est_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Observed {
    Snapshot,
    /// A live map read the working tree and mapped it onto the snapshot
    /// (`lsp.live` asks a running language server instead: it reads the disk
    /// when the call started the server, and a kept one is bounded by its tree
    /// stamp).
    LiveTree,
    AsOf(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Freshness {
    pub model: FreshnessModel,
    pub snapshot: SnapshotRef,
    pub observed: Observed,
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MapResponse {
    pub hits: Vec<MapHit>,
    pub status: MapStatus,
    pub cost: Cost,
    pub freshness: Freshness,
}

impl MapResponse {
    /// A response with no hits, for `status`, as `descriptor` describes it.
    pub fn empty(descriptor: &MapDescriptor, view: &SnapshotView<'_>, status: MapStatus) -> Self {
        Self {
            hits: Vec::new(),
            status,
            cost: Cost::default(),
            freshness: Freshness {
                model: descriptor.freshness,
                snapshot: view.snapshot.clone(),
                observed: Observed::Snapshot,
                stale: false,
            },
        }
    }
}

/// One index (stored or live) over the NodeId space.
#[async_trait::async_trait]
pub trait Map: Send + Sync {
    fn describe(&self) -> &MapDescriptor;

    /// One call. Implementations should not panic or exceed the deadline,
    /// but the registry does not rely on it: it catches a panic, stops
    /// waiting at the deadline and checks the answer (contract C1).
    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse;

    /// The edges leaving `node`, sorted and capped at [`EXPAND_LIMIT`]. The
    /// default is a seeded query: `node` the only seed, no text, hits turned
    /// into edges ([`edge::edges_from_response`]). Graph maps override it.
    ///
    /// The answer carries the cost of the call beside the edges
    /// ([`edge::Expanded`]), once, whatever the number of edges: a call that
    /// found nothing still cost. A map that makes external calls keeps to
    /// `budget.max_external_calls` and reports the calls it made.
    async fn expand(
        &self,
        node: &NodeId,
        budget: Budget,
        view: &SnapshotView<'_>,
    ) -> edge::Expanded {
        let seeds = [(node.clone(), 1.0)];
        let params = serde_json::Value::Null;
        let request = MapRequest {
            text: None,
            seeds: &seeds,
            limit: EXPAND_LIMIT,
            budget,
            params: &params,
            as_of: None,
        };
        let response = self.query(&request, view).await;
        edge::Expanded::new(
            edge::edges_from_response(node, self.describe(), &response, EXPAND_LIMIT),
            response.cost,
        )
    }

    /// What this map says about itself to a traversal that prices hops.
    fn card(&self) -> edge::MapCard {
        edge::MapCard::from_descriptor(self.describe())
    }
}

/// The most edges one [`Map::expand`] returns; also the `limit` of its
/// default seeded query.
pub const EXPAND_LIMIT: usize = 256;

/// A map call as the registry recorded it.
#[derive(Debug, Clone)]
pub struct MapCall {
    pub id: MapId,
    pub version: u32,
    pub contribution: Contribution,
    /// After the registry's checks: a failure here may be the registry's
    /// verdict on an answer (a panic, a deadline, an invalid hit).
    pub response: MapResponse,
    /// Wall time the registry measured around the call.
    pub engine_ms: f64,
}

/// One map's part in a fused score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Part {
    pub map: MapId,
    /// The rank fused, or `None` for a prior.
    pub rank: Option<u32>,
    /// What was added: `weight / (rrf_k + rank)` or the prior value.
    pub value: f32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fused {
    pub node: NodeId,
    pub score: f32,
    pub parts: Vec<Part>,
}

/// The outcome of [`Registry::run`]: every call in fusion order, and the
/// fused pool sorted best first.
#[derive(Debug, Clone)]
pub struct Run {
    pub calls: Vec<MapCall>,
    pub fused: Vec<Fused>,
}

impl Run {
    /// `(map, warning)` for every failed call and every partial answer
    /// with a failed source, in fusion order.
    pub fn warnings(&self) -> Vec<(MapId, &str)> {
        self.calls
            .iter()
            .filter_map(|call| match &call.response.status {
                MapStatus::Failed { warning }
                | MapStatus::Partial {
                    warning: Some(warning),
                    ..
                } => Some((call.id, warning.as_str())),
                _ => None,
            })
            .collect()
    }

    /// `(map, note)` for every partial answer with a note, in fusion order.
    pub fn notes(&self) -> Vec<(MapId, &str)> {
        self.calls
            .iter()
            .filter_map(|call| match &call.response.status {
                MapStatus::Partial {
                    note: Some(note), ..
                } => Some((call.id, note.as_str())),
                _ => None,
            })
            .collect()
    }
}

struct Entry<'a> {
    map: Box<dyn Map + 'a>,
    weight: f32,
    limit: usize,
    enabled: bool,
}

/// Maps in fusion order with their weights; runs and fuses them.
pub struct Registry<'a> {
    entries: Vec<Entry<'a>>,
    rrf_k: f32,
}

/// Why [`Registry::register`] refused a map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    Duplicate(MapId),
    /// Filter maps land with contract C2, after S2.
    FilterUnsupported(MapId),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Duplicate(id) => write!(f, "map {id} is already registered"),
            Self::FilterUnsupported(id) => write!(f, "map {id}: filter maps are not supported yet"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// Fusion position of a map: the legacy three in their order, then other
/// rank lists and expansions by id, then priors by id (decision M-3).
fn slot(descriptor: &MapDescriptor) -> (u8, usize, MapId) {
    let legacy = CHANNEL_ORDER.iter().position(|id| *id == descriptor.id);
    match (descriptor.contribution, legacy) {
        (Contribution::Prior, _) => (2, 0, descriptor.id),
        (_, Some(position)) => (0, position, descriptor.id),
        _ => (1, 0, descriptor.id),
    }
}

impl<'a> Registry<'a> {
    pub fn new(rrf_k: f32) -> Self {
        Self {
            entries: Vec::new(),
            rrf_k,
        }
    }

    /// Adds `map` with its fusion `weight` (ignored for a prior) and the
    /// most hits it may return. Registration order never matters: the
    /// entries are kept in fusion order.
    pub fn register(
        &mut self,
        map: Box<dyn Map + 'a>,
        weight: f32,
        limit: usize,
    ) -> Result<&mut Self, RegisterError> {
        let descriptor = map.describe();
        if self
            .entries
            .iter()
            .any(|e| e.map.describe().id == descriptor.id)
        {
            return Err(RegisterError::Duplicate(descriptor.id));
        }
        if descriptor.contribution == Contribution::Filter {
            return Err(RegisterError::FilterUnsupported(descriptor.id));
        }
        self.entries.push(Entry {
            map,
            weight,
            limit,
            enabled: true,
        });
        self.entries.sort_by_key(|entry| slot(entry.map.describe()));
        Ok(self)
    }

    /// A disabled map is not called; its call is recorded as `Skipped`,
    /// which is silent (no warning, no note), as a disabled channel is today.
    /// Returns whether a map with that id is registered.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> bool {
        let mut found = false;
        for entry in &mut self.entries {
            if entry.map.describe().id == id {
                entry.enabled = enabled;
                found = true;
            }
        }
        found
    }

    /// How many independent maps can offer a traversal edge: the registered
    /// maps that accept some node kind (a start-node source accepts none),
    /// enabled or not (a steer can switch one on during a walk), with the
    /// maps whose cards say `dependent` counted once together. The most a
    /// node can be corroborated by.
    pub fn independent_expanders(&self) -> usize {
        let (mut independent, mut dependent) = (0, false);
        for entry in &self.entries {
            if !entry.map.describe().accepts.is_empty() {
                if entry.map.card().dependent {
                    dependent = true;
                } else {
                    independent += 1;
                }
            }
        }
        independent + usize::from(dependent)
    }

    /// The ids of the registered maps that accept some node kind, enabled or
    /// not, in fusion order: the maps [`Registry::independent_expanders`]
    /// counts, for `Traversal::expanders`.
    pub fn expanding_maps(&self) -> Vec<MapId> {
        self.entries
            .iter()
            .filter(|e| !e.map.describe().accepts.is_empty())
            .map(|e| e.map.describe().id)
            .collect()
    }

    /// The registered ids in fusion order.
    pub fn ids(&self) -> Vec<MapId> {
        self.entries.iter().map(|e| e.map.describe().id).collect()
    }

    /// Runs every map once: rank lists and expansions side by side, each
    /// under `budget`; fuses them in fusion order; then asks every prior map
    /// about the fused nodes and adds its values unweighted; then sorts.
    ///
    /// Needs a Tokio runtime with the time driver (the deadline is a Tokio
    /// timeout). `Map::describe` is called outside the fail-open boundary:
    /// it must not panic.
    pub async fn run(&self, text: Option<&str>, budget: Duration, view: &SnapshotView<'_>) -> Run {
        let params = serde_json::Value::Null;
        let (ranked, priors): (Vec<&Entry<'a>>, Vec<&Entry<'a>>) = self
            .entries
            .iter()
            .partition(|e| e.map.describe().contribution != Contribution::Prior);
        let mut calls: Vec<MapCall> = futures::future::join_all(
            ranked
                .iter()
                .map(|entry| call(entry, text, &[], budget, &params, view, |_| true)),
        )
        .await;

        let mut fused: Vec<Fused> = Vec::new();
        let mut position = HashMap::<NodeId, usize>::new();
        for (entry, call) in ranked.iter().zip(&calls) {
            if !matches!(
                call.response.status,
                MapStatus::Ok | MapStatus::Partial { .. }
            ) {
                continue;
            }
            for hit in &call.response.hits {
                let value = entry.weight / (self.rrf_k + hit.rank as f32);
                let part = Part {
                    map: call.id,
                    rank: Some(hit.rank),
                    value,
                };
                match position.get(&hit.node) {
                    Some(&at) => {
                        fused[at].score += value;
                        fused[at].parts.push(part);
                    }
                    None => {
                        position.insert(hit.node.clone(), fused.len());
                        fused.push(Fused {
                            node: hit.node.clone(),
                            // `0.0 + value` is `value`, as in `fuse_hits`.
                            score: 0.0 + value,
                            parts: vec![part],
                        });
                    }
                }
            }
        }

        let seeds: Vec<(NodeId, f32)> = fused.iter().map(|f| (f.node.clone(), f.score)).collect();
        let seed_set: HashSet<&NodeId> = seeds.iter().map(|(node, _)| node).collect();
        for entry in priors {
            let call = call(entry, text, &seeds, budget, &params, view, |node| {
                seed_set.contains(node)
            })
            .await;
            if matches!(
                call.response.status,
                MapStatus::Ok | MapStatus::Partial { .. }
            ) {
                for hit in &call.response.hits {
                    let value = hit.score.expect("checked by `call`");
                    let at = position[&hit.node];
                    fused[at].score += value;
                    fused[at].parts.push(Part {
                        map: call.id,
                        rank: None,
                        value,
                    });
                }
            }
            calls.push(call);
        }

        fused.sort_by(|a, b| fused_order(a, b, view));
        Run { calls, fused }
    }
}

/// Runs a stored map's CPU or file work on Tokio's blocking pool
/// (`tokio::task::spawn_blocking`) and waits for it, so the work does not
/// hold an async worker (merge condition M5). A map calls it from `query`
/// for the part that needs no borrowed request or view data; the rest
/// (projection onto the view, the limit) stays in `query`.
///
/// The registry's timeout pre-empts the wait: past the deadline the map's
/// future, and this wait with it, is dropped and the run goes on with a
/// note. The work itself cannot be stopped; it runs to its end, detached.
/// Bound on that detached work: a map calls this at most once per query,
/// so at most one task per stored map and search is in flight, and Tokio
/// runs at most `max_blocking_threads` of them at once (512 unless the
/// runtime sets another number), queueing the rest.
///
/// A panic in `work` resumes in the caller, so the registry reports it as
/// the map's own panic.
pub async fn off_thread<T: Send + 'static>(work: impl FnOnce() -> T + Send + 'static) -> T {
    match tokio::task::spawn_blocking(work).await {
        Ok(value) => value,
        Err(error) => match error.try_into_panic() {
            Ok(payload) => std::panic::resume_unwind(payload),
            // Only a runtime shutting down cancels a blocking task.
            Err(error) => panic!("the blocking work of a map was cancelled: {error}"),
        },
    }
}

/// Today's `fusion_order`: score descending, then path, start line, end
/// line and content hash of the chunk; the NodeId string only when a node
/// is not a chunk of the view (contract C6).
pub fn fused_order(a: &Fused, b: &Fused, view: &SnapshotView<'_>) -> Ordering {
    b.score
        .total_cmp(&a.score)
        .then_with(|| match (view.chunk(&a.node), view.chunk(&b.node)) {
            (Some(x), Some(y)) => (
                &x.relative_file_path,
                x.start_line,
                x.end_line,
                &x.content_hash,
            )
                .cmp(&(
                    &y.relative_file_path,
                    y.start_line,
                    y.end_line,
                    &y.content_hash,
                )),
            _ => a.node.cmp(&b.node),
        })
}

/// Calls one map with fail-open: a disabled map is skipped silently; a
/// panic, a missed deadline or an invalid answer becomes a status naming the
/// map; the answer is otherwise kept as given.
async fn call<'a>(
    entry: &Entry<'a>,
    text: Option<&str>,
    seeds: &[(NodeId, f32)],
    budget: Duration,
    params: &serde_json::Value,
    view: &SnapshotView<'_>,
    allowed: impl Fn(&NodeId) -> bool,
) -> MapCall {
    let descriptor = entry.map.describe();
    let started = Instant::now();
    let record = |response: MapResponse| MapCall {
        id: descriptor.id,
        version: descriptor.version,
        contribution: descriptor.contribution,
        response,
        engine_ms: started.elapsed().as_secs_f64() * 1000.0,
    };
    if !entry.enabled {
        return record(MapResponse::empty(
            descriptor,
            view,
            MapStatus::Skipped {
                reason: "disabled".into(),
            },
        ));
    }
    let request = MapRequest {
        text,
        seeds,
        limit: entry.limit,
        budget: Budget {
            deadline: started + budget,
            max_external_calls: u32::MAX,
            max_nodes_touched: u64::MAX,
        },
        params,
        as_of: None,
    };
    let answer = tokio::time::timeout(
        budget,
        AssertUnwindSafe(entry.map.query(&request, view)).catch_unwind(),
    )
    .await;
    let failed =
        |warning: String| MapResponse::empty(descriptor, view, MapStatus::Failed { warning });
    let response = match answer {
        Err(_) => MapResponse::empty(
            descriptor,
            view,
            MapStatus::Partial {
                note: Some(format!(
                    "map {} did not answer within {} ms; its results are missing for this search",
                    descriptor.id,
                    budget.as_millis()
                )),
                warning: None,
            },
        ),
        Ok(Err(_)) => failed(format!("map {} panicked", descriptor.id)),
        Ok(Ok(response)) => match check(descriptor, &response, entry.limit, view, &allowed) {
            Ok(()) => response,
            Err(reason) => {
                let mut warning =
                    format!("map {} returned an invalid answer: {reason}", descriptor.id);
                // Keep what a partial answer said about itself.
                if let MapStatus::Partial { note, warning: own } = &response.status {
                    for text in [own, note].into_iter().flatten() {
                        warning.push_str("; ");
                        warning.push_str(text);
                    }
                }
                failed(warning)
            }
        },
    };
    record(response)
}

/// The registry's checks on an answer with hits: a partial answer names
/// what is missing, every node is a chunk of
/// the view (and, for a prior, one of its seeds), no node twice, ranks
/// are at least 1 and never fall, scores are finite, a prior carries its value,
/// no more than `limit` hits, and every hit carries evidence whose labels the
/// map's card lists (contract C5, [`labels::check_labels`]; merge condition
/// M6 moved it here from the `labels::Labelled` wrapper, so a map registered
/// without the wrapper is checked too).
fn check(
    descriptor: &MapDescriptor,
    response: &MapResponse,
    limit: usize,
    view: &SnapshotView<'_>,
    allowed: &impl Fn(&NodeId) -> bool,
) -> Result<(), String> {
    if !matches!(response.status, MapStatus::Ok | MapStatus::Partial { .. }) {
        return Ok(());
    }
    // A partial answer must say what is missing (`MapStatus::Partial`).
    if let MapStatus::Partial {
        note: None,
        warning: None,
    } = response.status
    {
        return Err("a partial answer with neither a note nor a warning".into());
    }
    if descriptor.contribution != Contribution::Prior && response.hits.len() > limit {
        return Err(format!(
            "{} hits over the limit {limit}",
            response.hits.len()
        ));
    }
    let mut seen = HashSet::new();
    let mut previous = 0;
    for hit in &response.hits {
        if !view.contains(&hit.node) || !allowed(&hit.node) {
            return Err(format!("{} is not in the searched universe", hit.node));
        }
        if !seen.insert(&hit.node) {
            return Err(format!("{} returned twice", hit.node));
        }
        // Ranks may tie (the fusion unit cases give three hits rank 1) but
        // never fall, and start at 1.
        if hit.rank == 0 || hit.rank < previous {
            return Err(format!("rank {} after {previous}", hit.rank));
        }
        previous = hit.rank;
        match hit.score {
            Some(score) if !score.is_finite() => {
                return Err(format!("non-finite score for {}", hit.node));
            }
            None if descriptor.contribution == Contribution::Prior => {
                return Err(format!("no prior value for {}", hit.node));
            }
            _ => {}
        }
    }
    labels::check_labels(descriptor, response)
}

#[cfg(test)]
mod tests;
