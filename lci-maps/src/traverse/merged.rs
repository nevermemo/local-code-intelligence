//! The pre-merged edge source (spine brief 1.3, slice S5): the union of what
//! the enabled maps say about every node, asked once and fused with the same
//! function as [`OnTheFly`], so that a traversal reads a row instead of
//! asking the maps.
//!
//! [`MergedAdjacency::build`] asks [`OnTheFly`] about every node of the
//! snapshot (the view's files and chunks, plus any extra roots) and then about
//! every node those answers name, as long as some map accepts the node's
//! kind. Rows hold the edges before fusion (so a map switched off for one
//! traversal can be left out and the rest re-fused) and after it (the common
//! case, no work at lookup). [`PreMerged`] is the [`EdgeSource`] over it.
//!
//! # Equality with the on-the-fly source
//!
//! For every node the adjacency covers, [`PreMerged`] returns the edges
//! [`OnTheFly`] returns, in the same order, and the same calls (map and edge
//! count), with two differences by design: a lookup costs nothing (the calls
//! carry no cost), and a node the build never reached gets one call named
//! [`GAP_MAP`] with a failure note when some map accepts its kind (the
//! `max_nodes` cap, or a node nothing reaches from the roots). A node no map
//! accepts has no row and no edges, as on the fly. The auditor
//! ([`crate::audit::audit_edge_sources`]) checks this on fixtures.
//!
//! # Incremental update, keyed by blob
//!
//! A file's blob ([`FileBlobs`]) is a hash of its content as the snapshot
//! holds it, not a path. [`MergedAdjacency::update`] reuses a row when every
//! blob the row depends on is unchanged: the blob of the file the node
//! belongs to (when its id names a path) and the blobs of the files its
//! edges' sites are in. A node id still contains the path (that is its
//! identity); the staleness test never looks at a path, a modification time
//! or a file list. What forces a row to be asked again:
//!
//! * a dependency blob appeared, vanished or changed (or its number of files
//!   changed);
//! * the row has no dependency at all (a node with no path and no edges with
//!   a site) or one of its sites is in a file the view does not hold: such a
//!   row is reused only when no blob of the view changed;
//! * the row is incomplete (a map failed or missed the deadline);
//! * the map set (ids and versions) changed: everything is asked again;
//! * the snapshot fingerprint changed and some enabled map is not
//!   [`FreshnessModel::PerFile`]: everything is asked again (such a map may
//!   answer differently for a file nobody touched).
//!
//! A map declared [`FreshnessModel::Live`] reads the working tree, which the
//! snapshot's blobs do not describe: a row that asked one is asked again at
//! every update.
//!
//! The per-file trust is the maps' declared freshness model: a `PerFile` map's
//! edges from a node change only when a file the edges come from changes.
//! A map that breaks that declaration makes an incremental update differ from
//! a full build; `update` then equals `build` only for maps that keep it.

use super::source::{EdgeSource, ExpandCall, Expansion, FusedEdge, OnTheFly, fuse};
use crate::{
    Budget, Cost, FreshnessModel, MapId, Registry, SnapshotView,
    audit::canonical_fused,
    edge::{Edge, MapCard},
};
use lci_core::node::{NodeId, NodeKind};
use sha2::{Digest, Sha256};
use std::{
    borrow::Borrow,
    collections::{BTreeMap, BTreeSet, HashSet, VecDeque},
    time::{Duration, Instant},
};

/// The map name on the call [`PreMerged`] reports for a node the adjacency
/// does not hold although a map accepts its kind.
pub const GAP_MAP: MapId = "merged.adjacency";

/// The content hash of each file of a snapshot view.
///
/// A file's blob is the SHA-256 (first 16 bytes, hex) of its chunk list as
/// the view holds it: per chunk, in order, the line range and the content
/// hash the scanner stored. Equal content gives equal blobs and any edit that
/// changes a chunk's text or position changes the blob. The view is the
/// filtered universe, so the blob is that of the file as seen through the
/// filter.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileBlobs {
    by_path: BTreeMap<String, String>,
    /// How many files have each blob.
    files: BTreeMap<String, u32>,
}

impl FileBlobs {
    pub fn of(view: &SnapshotView<'_>) -> Self {
        let mut chunks: BTreeMap<&str, Vec<(u32, u32, u32, &str)>> = BTreeMap::new();
        for id in view.ids() {
            if let NodeId::Chunk {
                path,
                content_hash,
                ordinal,
            } = id
            {
                let (start, end) = view
                    .chunk(id)
                    .map_or((0, 0), |chunk| (chunk.start_line, chunk.end_line));
                chunks
                    .entry(path)
                    .or_default()
                    .push((*ordinal, start, end, content_hash));
            }
        }
        let mut blobs = Self::default();
        for (path, mut list) in chunks {
            list.sort_unstable();
            let mut hasher = Sha256::new();
            hasher.update(b"lci-file-blob/1\n");
            for (_, start, end, hash) in list {
                hasher.update(format!("{start} {end} {hash}\n").as_bytes());
            }
            let blob = hasher.finalize().iter().take(16).fold(
                String::with_capacity(32),
                |mut out, byte| {
                    out.push_str(&format!("{byte:02x}"));
                    out
                },
            );
            *blobs.files.entry(blob.clone()).or_default() += 1;
            blobs.by_path.insert(path.to_string(), blob);
        }
        blobs
    }

    /// The blob of the file at `path`, when the view holds it.
    pub fn blob(&self, path: &str) -> Option<&str> {
        self.by_path.get(path).map(String::as_str)
    }

    /// Each blob with the number of files that have it.
    pub fn counts(&self) -> &BTreeMap<String, u32> {
        &self.files
    }

    /// The paths of the view's files, sorted.
    pub fn paths(&self) -> impl Iterator<Item = &str> {
        self.by_path.keys().map(String::as_str)
    }

    /// Sets the blob of every site that has none and whose path the view
    /// holds. A site keeps a blob its map set (a live map hashes the current
    /// bytes, which the snapshot may not have seen yet) and a site in a file
    /// the view does not hold keeps what it had.
    pub fn fill(&self, edges: &mut [Edge]) {
        for site in edges.iter_mut().filter_map(|edge| edge.site.as_mut()) {
            if !site.blob.is_empty() {
                continue;
            }
            if let Some(blob) = self.by_path.get(&site.path) {
                site.blob.clone_from(blob);
            }
        }
    }
}

/// What a build does.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// How long one node may take (all its maps side by side).
    pub per_node: Duration,
    /// The most rows; a build that reaches it stops and says so.
    pub max_nodes: usize,
    /// Nodes to start from besides the view's files and chunks (the nodes of
    /// a map the view does not hold, such as symbols).
    pub roots: Vec<NodeId>,
}

impl Default for BuildOptions {
    fn default() -> Self {
        Self {
            per_node: Duration::from_secs(5),
            max_nodes: 2_000_000,
            roots: Vec::new(),
        }
    }
}

/// What one build or update did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BuildStats {
    /// Rows after the update.
    pub nodes: usize,
    /// Rows asked of the maps.
    pub expanded: usize,
    /// Rows taken over from before.
    pub reused: usize,
    /// Rows that came out incomplete (a map failed or missed its deadline).
    pub incomplete: usize,
    /// Rows with no known dependency (no path, no edge with a site in the
    /// view): they are asked again whenever any blob moves.
    pub undated: usize,
    /// Blobs of the view that were also in the previous state, new, gone.
    pub blobs_kept: usize,
    pub blobs_added: usize,
    pub blobs_removed: usize,
    /// Set when everything was asked again, and why.
    pub full_rebuild: Option<&'static str>,
    /// The `max_nodes` cap stopped the build.
    pub truncated: bool,
    pub elapsed_ms: f64,
}

#[derive(Debug, Clone, PartialEq)]
struct StoredCall {
    map: MapId,
    n_edges: usize,
    failure: Option<String>,
}

/// Everything known about the edges leaving one node.
#[derive(Debug, Clone)]
struct Row {
    /// Every edge the maps offered, as `OnTheFly::ask` returned them.
    raw: Vec<Edge>,
    /// `fuse(raw)`.
    fused: Vec<FusedEdge>,
    calls: Vec<StoredCall>,
    /// The blobs the row depends on; empty means no dependency is known.
    deps: BTreeSet<String>,
    complete: bool,
}

impl Row {
    async fn ask(
        source: &OnTheFly<'_, '_, '_>,
        node: &NodeId,
        per_node: Duration,
        blobs: &FileBlobs,
    ) -> Self {
        let budget = Budget {
            deadline: Instant::now() + per_node,
            max_external_calls: u32::MAX,
            max_nodes_touched: u64::MAX,
        };
        let (raw, calls) = source.ask(node, budget).await;
        let mut deps = BTreeSet::new();
        if let Some(blob) = node.path().and_then(|path| blobs.blob(path)) {
            deps.insert(blob.to_string());
        }
        let mut unresolved = false;
        for site in raw.iter().filter_map(|edge| edge.site.as_ref()) {
            if site.blob.is_empty() {
                unresolved = true;
            } else {
                deps.insert(site.blob.clone());
            }
        }
        if unresolved {
            deps.clear();
        }
        Self {
            fused: fuse(raw.clone()),
            complete: calls.iter().all(|call| call.failure.is_none()),
            calls: calls
                .into_iter()
                .map(|call| StoredCall {
                    map: call.map,
                    n_edges: call.n_edges,
                    failure: call.failure,
                })
                .collect(),
            raw,
            deps,
        }
    }
}

/// The union of the enabled maps' edges for every node of a snapshot.
#[derive(Debug, Clone, Default)]
pub struct MergedAdjacency {
    rows: BTreeMap<NodeId, Row>,
    /// Ids and versions of the maps the rows were built with.
    built: Option<Vec<(MapId, u32)>>,
    fingerprint: String,
    /// Files per blob of the view the rows were built from.
    blobs: BTreeMap<String, u32>,
    /// Kinds some built map accepts.
    kinds: BTreeSet<NodeKind>,
    /// Every registered map, built or not.
    known: BTreeSet<&'static str>,
    cards: BTreeMap<MapId, MapCard>,
    last: BuildStats,
}

impl MergedAdjacency {
    /// Builds the adjacency of `view` over the enabled maps of `maps`, from
    /// the view's files and chunks.
    pub async fn build(maps: &Registry<'_>, view: &SnapshotView<'_>) -> Self {
        Self::build_with(maps, view, &BuildOptions::default()).await
    }

    pub async fn build_with(
        maps: &Registry<'_>,
        view: &SnapshotView<'_>,
        options: &BuildOptions,
    ) -> Self {
        let mut adjacency = Self::default();
        adjacency.update(maps, view, options).await;
        adjacency
    }

    /// Brings the rows up to the snapshot `view` shows, asking the maps only
    /// about the nodes whose blobs changed (see the module notes), and drops
    /// the rows nothing reaches any more.
    pub async fn update(
        &mut self,
        maps: &Registry<'_>,
        view: &SnapshotView<'_>,
        options: &BuildOptions,
    ) -> BuildStats {
        let started = Instant::now();
        let source = OnTheFly::new(maps, view);
        let blobs = source.blobs();
        let enabled: Vec<_> = maps.entries.iter().filter(|e| e.enabled).collect();
        let built: Vec<(MapId, u32)> = enabled
            .iter()
            .map(|e| (e.map.describe().id, e.map.describe().version))
            .collect();
        let per_file = enabled
            .iter()
            .all(|e| e.map.describe().freshness == FreshnessModel::PerFile);
        let kinds: BTreeSet<NodeKind> = enabled
            .iter()
            .flat_map(|e| e.map.describe().accepts.iter().copied())
            .collect();

        let mut stats = BuildStats::default();
        let mut old = std::mem::take(&mut self.rows);
        if self
            .built
            .as_ref()
            .is_some_and(|previous| *previous != built)
        {
            stats.full_rebuild = Some("the map set changed");
        } else if !per_file && self.built.is_some() && self.fingerprint != view.snapshot.fingerprint
        {
            stats.full_rebuild = Some("the snapshot changed and a map is not per file");
        }
        if stats.full_rebuild.is_some() {
            old.clear();
        }
        let same_world =
            self.built.is_some() && self.blobs == *blobs.counts() && stats.full_rebuild.is_none();
        for (blob, count) in blobs.counts() {
            match self.blobs.get(blob) {
                Some(before) if before == count => stats.blobs_kept += 1,
                _ => stats.blobs_added += 1,
            }
        }
        stats.blobs_removed = self
            .blobs
            .iter()
            .filter(|(blob, count)| blobs.counts().get(*blob) != Some(count))
            .count();
        // A row that asked a map reading the live tree is never reused: its
        // answer can change with no change in the snapshot.
        let live: HashSet<MapId> = enabled
            .iter()
            .filter(|e| e.map.describe().freshness == FreshnessModel::Live)
            .map(|e| e.map.describe().id)
            .collect();
        let valid = |row: &Row| {
            row.complete
                && !row.calls.iter().any(|call| live.contains(call.map))
                && (same_world
                    || (!row.deps.is_empty()
                        && row.deps.iter().all(|blob| {
                            self.blobs.contains_key(blob)
                                && self.blobs.get(blob) == blobs.counts().get(blob)
                        })))
        };

        // The roots: the view's files and chunks, then the caller's.
        let roots: BTreeSet<NodeId> = blobs
            .paths()
            .filter_map(|path| NodeId::file(path).ok())
            .chain(view.ids().iter().cloned())
            .chain(options.roots.iter().cloned())
            .filter(|node| kinds.contains(&node.kind()))
            .collect();
        let mut seen: HashSet<NodeId> = roots.iter().cloned().collect();
        let mut queue: VecDeque<NodeId> = roots.into_iter().collect();
        let mut rows = BTreeMap::new();
        while let Some(node) = queue.pop_front() {
            if rows.len() >= options.max_nodes {
                stats.truncated = true;
                break;
            }
            let row = match old.remove(&node).filter(|row| valid(row)) {
                Some(row) => {
                    stats.reused += 1;
                    row
                }
                None => {
                    stats.expanded += 1;
                    Row::ask(&source, &node, options.per_node, blobs).await
                }
            };
            stats.incomplete += usize::from(!row.complete);
            stats.undated += usize::from(row.deps.is_empty());
            for edge in &row.raw {
                if kinds.contains(&edge.dst.kind()) && seen.insert(edge.dst.clone()) {
                    queue.push_back(edge.dst.clone());
                }
            }
            rows.insert(node, row);
        }

        self.known = maps.entries.iter().map(|e| e.map.describe().id).collect();
        self.cards = maps
            .entries
            .iter()
            .map(|e| (e.map.describe().id, e.map.card()))
            .collect();
        self.rows = rows;
        self.built = Some(built);
        self.fingerprint = view.snapshot.fingerprint.clone();
        self.blobs = blobs.counts().clone();
        self.kinds = kinds;
        stats.nodes = self.rows.len();
        stats.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.last = stats.clone();
        stats
    }

    /// What the last build or update did.
    pub fn stats(&self) -> &BuildStats {
        &self.last
    }

    /// Rows held.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Whether the adjacency holds a row for `node`.
    pub fn covers(&self, node: &NodeId) -> bool {
        self.rows.contains_key(node)
    }

    /// The nodes with a row, sorted.
    pub fn nodes(&self) -> impl Iterator<Item = &NodeId> {
        self.rows.keys()
    }

    /// Files per blob of the view the rows were built from.
    pub fn blobs(&self) -> &BTreeMap<String, u32> {
        &self.blobs
    }

    /// The node, map and note of every call that failed at build time.
    pub fn failures(&self) -> Vec<(&NodeId, MapId, &str)> {
        self.rows
            .iter()
            .flat_map(|(node, row)| {
                row.calls
                    .iter()
                    .filter_map(move |c| c.failure.as_deref().map(|why| (node, c.map, why)))
            })
            .collect()
    }

    /// Every row in a canonical text form, sorted by node: two adjacencies
    /// with equal canonical forms answer every lookup alike.
    pub fn canonical(&self) -> Vec<String> {
        self.rows
            .iter()
            .map(|(node, row)| {
                let edges: Vec<String> = row.fused.iter().map(canonical_fused).collect();
                let calls: Vec<String> = row
                    .calls
                    .iter()
                    .map(|c| format!("{}:{}:{}", c.map, c.n_edges, c.failure.is_some()))
                    .collect();
                format!(
                    "{node} => [{}] calls [{}]",
                    edges.join(" ; "),
                    calls.join(",")
                )
            })
            .collect()
    }

    fn accepts(&self, kind: NodeKind) -> bool {
        self.kinds.contains(&kind)
    }
}

/// An [`EdgeSource`] over a [`MergedAdjacency`], borrowed or shared
/// (`&MergedAdjacency`, `Arc<MergedAdjacency>`). Map switches are this
/// source's own.
pub struct PreMerged<A: Borrow<MergedAdjacency> + Send + Sync> {
    adjacency: A,
    off: HashSet<String>,
}

impl<A: Borrow<MergedAdjacency> + Send + Sync> PreMerged<A> {
    pub fn new(adjacency: A) -> Self {
        Self {
            adjacency,
            off: HashSet::new(),
        }
    }
}

#[crate::async_trait]
impl<A: Borrow<MergedAdjacency> + Send + Sync> EdgeSource for PreMerged<A> {
    async fn edges(&self, node: &NodeId, _budget: Budget) -> Expansion {
        let adjacency: &MergedAdjacency = self.adjacency.borrow();
        let Some(row) = adjacency.rows.get(node) else {
            return if adjacency.accepts(node.kind()) {
                Expansion {
                    edges: Vec::new(),
                    calls: vec![ExpandCall {
                        map: GAP_MAP,
                        n_edges: 0,
                        cost: Cost::default(),
                        failure: Some("not in the pre-merged adjacency".into()),
                    }],
                }
            } else {
                Expansion::default()
            };
        };
        let edges = if self.off.is_empty() {
            row.fused.clone()
        } else {
            fuse(
                row.raw
                    .iter()
                    .filter(|edge| !self.off.contains(edge.map))
                    .cloned()
                    .collect(),
            )
        };
        let calls = row
            .calls
            .iter()
            .filter(|call| !self.off.contains(call.map))
            .map(|call| ExpandCall {
                map: call.map,
                n_edges: call.n_edges,
                cost: Cost::default(),
                failure: call.failure.clone(),
            })
            .collect();
        Expansion { edges, calls }
    }

    fn card(&self, map: MapId) -> MapCard {
        let adjacency: &MergedAdjacency = self.adjacency.borrow();
        adjacency.cards.get(map).cloned().unwrap_or_default()
    }

    fn set_enabled(&mut self, id: &str, on: bool) -> bool {
        let adjacency: &MergedAdjacency = self.adjacency.borrow();
        if !adjacency.known.contains(id) {
            return false;
        }
        if on {
            self.off.remove(id);
        } else {
            self.off.insert(id.to_string());
        }
        true
    }
}
