//! The map layer of the multimap spike (slice S2; spec
//! `docs/spike/multimap/specs/map-trait.md`).
//!
//! A [`Map`] answers one request (a query text and/or seed nodes) with
//! ranked [`NodeId`]s and evidence. The [`Registry`] calls every enabled map,
//! turns any failure into a status instead of an error (contract C1), fuses
//! the answers with today's weighted RRF arithmetic in today's order, and
//! sorts with today's tie rule. Nothing on the production search path uses
//! this crate yet: the legacy channels are wrapped as maps in the root crate
//! and checked against `fuse_hits` bit for bit (test T0).

pub mod audit;
pub mod crates;
pub mod fresh;
pub mod kit;
pub mod kit_c3;
pub mod labels;
pub mod run_record;
pub mod scratch;
pub mod symdefs;

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
pub const LEGACY_ORDER: [MapId; 3] = ["vec.chunk.code", "lexical", "lsp.symbols"];

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

/// The update model of maps-inventory.md, "Update model vocabulary".
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

/// The kind of edge an evidence item follows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeKind {
    /// The node matched the query text.
    QueryMatch,
    /// A prior value attached to the node.
    Prior,
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
    /// A live map read the working tree and mapped it onto the snapshot.
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
}

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
    let legacy = LEGACY_ORDER.iter().position(|id| *id == descriptor.id);
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
