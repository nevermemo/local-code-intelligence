//! `sym.defs`, the first stored map (slice S3): the declarations of the
//! snapshot, fed from the path-free declaration bundles
//! (`lci_core::bundle`). A query names identifiers; the map returns the
//! chunks that define declarations with those names.
//!
//! Hits are projected to chunks before fusion (decision M-4): a symbol's
//! hit is the chunk holding the first line of its declaration ("head
//! chunk"), and the evidence names the symbol. Registered for tests and the
//! contract kit only; no production search runs it.

use crate::{
    Contribution, EdgeKind, Evidence, FreshnessModel, Map, MapDescriptor, MapHit, MapRequest,
    MapResponse, MapStatus, SnapshotRef, SnapshotView, SourceLabel,
    fresh::{Refreshable, Target},
};
use lci_core::{
    bundle::{BundleStore, Declarations, Producer, StoredDeclaration, sync},
    chunk::Chunk,
    manifest::CachedFile,
    node::{NodeId, NodeKind, chunk_ids},
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Instant,
};

/// The map's id.
pub const ID: &str = "sym.defs";
/// Longest evidence detail (a symbol's canonical string), in bytes.
const MAX_DETAIL: usize = 256;

/// One declaration of the snapshot.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Entry {
    pub symbol: NodeId,
    /// The chunk holding the declaration's first line, if any chunk does.
    pub head: Option<NodeId>,
    pub name: String,
}

/// The joined data of one snapshot: every declaration with its symbol
/// NodeId (module path from the file's current path) and its head chunk.
#[derive(Debug, Clone, PartialEq)]
pub struct SymIndex {
    pub snapshot: SnapshotRef,
    /// Sorted.
    entries: Vec<Entry>,
    /// Lower-case name to entry positions.
    by_name: HashMap<String, Vec<usize>>,
}

/// The chunk whose lines hold `line`: the one starting last at or before
/// it, the first in source order on a tie. `None` when no chunk holds it.
fn head_chunk<'c>(chunks: &'c [Chunk], ids: &'c [NodeId], line: u32) -> Option<&'c NodeId> {
    let mut best: Option<usize> = None;
    for (index, chunk) in chunks.iter().enumerate() {
        if chunk.start_line <= line
            && line <= chunk.end_line
            && best.is_none_or(|b| chunk.start_line > chunks[b].start_line)
        {
            best = Some(index);
        }
    }
    best.map(|index| &ids[index])
}

impl SymIndex {
    /// Joins `payloads` (per path) with the snapshot's files: symbol keys
    /// take their module path from the path the file has now, head chunks
    /// come from the file's chunk list.
    pub fn join(
        snapshot: SnapshotRef,
        files: &HashMap<String, CachedFile>,
        payloads: &BTreeMap<String, Vec<StoredDeclaration>>,
    ) -> Self {
        let mut entries = Vec::new();
        for (path, stored) in payloads {
            let Some(file) = files.get(path) else {
                continue;
            };
            let ids = chunk_ids(&file.chunks);
            for declaration in stored.iter().filter_map(StoredDeclaration::declaration) {
                entries.push(Entry {
                    symbol: declaration.node_id(path),
                    head: head_chunk(&file.chunks, &ids, declaration.start_line).cloned(),
                    name: declaration.name,
                });
            }
        }
        entries.sort();
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        for (position, entry) in entries.iter().enumerate() {
            by_name
                .entry(entry.name.to_lowercase())
                .or_default()
                .push(position);
        }
        Self {
            snapshot,
            entries,
            by_name,
        }
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// The canonical serialisation contract C4 compares: one line per
    /// declaration, `<symbol>\t<head chunk or ->`, sorted.
    pub fn canonical(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .entries
            .iter()
            .map(|entry| match &entry.head {
                Some(head) => format!("{}\t{head}", entry.symbol),
                None => format!("{}\t-", entry.symbol),
            })
            .collect();
        lines.sort();
        lines
    }
}

/// Identifier tokens of a query, in order, each once.
fn tokens(text: &str) -> Vec<&str> {
    let mut seen = HashSet::new();
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|token| !token.is_empty() && seen.insert(*token))
        .collect()
}

pub fn descriptor() -> MapDescriptor {
    MapDescriptor {
        id: ID,
        version: 1,
        contribution: Contribution::RankList,
        accepts_text: true,
        accepts: Vec::new(),
        emits: vec![NodeKind::Chunk],
        freshness: FreshnessModel::PerFile,
        source_labels: vec![SourceLabel::Tags],
        needs: Vec::new(),
    }
}

/// The ranked answer of `index` to `text`, projected onto the chunks of
/// `view`: declarations named exactly as a query token first, then those
/// that match one ignoring case; by symbol within each class. A chunk is
/// listed once, at its best declaration.
pub fn answer(index: &SymIndex, text: &str, limit: usize, view: &SnapshotView<'_>) -> Vec<MapHit> {
    let tokens = tokens(text);
    let mut matched: Vec<(u8, &Entry)> = Vec::new();
    let mut taken = HashSet::new();
    for token in &tokens {
        for &position in index
            .by_name
            .get(&token.to_lowercase())
            .into_iter()
            .flatten()
        {
            if taken.insert(position) {
                let entry = &index.entries[position];
                let class = if tokens.contains(&entry.name.as_str()) {
                    0
                } else {
                    1
                };
                matched.push((class, entry));
            }
        }
    }
    matched.sort();
    let mut hits: Vec<MapHit> = Vec::new();
    let mut listed = HashSet::new();
    for (_, entry) in matched {
        if hits.len() == limit {
            break;
        }
        let Some(head) = &entry.head else {
            continue;
        };
        if !view.contains(head) || !listed.insert(head) {
            continue;
        }
        let mut detail = entry.symbol.to_string();
        if detail.len() > MAX_DETAIL {
            let mut end = MAX_DETAIL;
            while !detail.is_char_boundary(end) {
                end -= 1;
            }
            detail.truncate(end);
        }
        hits.push(MapHit {
            node: head.clone(),
            rank: hits.len() as u32 + 1,
            score: None,
            evidence: vec![Evidence {
                via: None,
                edge: EdgeKind::QueryMatch,
                label: SourceLabel::Tags,
                detail: Some(detail),
            }],
        });
    }
    hits
}

/// `sym.defs` over a workspace's bundle store. `P` is the declaration
/// producer: [`Declarations`] in production; the contract kit's tests pass
/// deliberately broken ones.
pub struct SymDefs<P: Producer<Payload = Vec<StoredDeclaration>> = Declarations> {
    descriptor: MapDescriptor,
    store: BundleStore,
    producer: P,
    data: Option<SymIndex>,
    /// What the last refresh found: (reused, extracted) bundles.
    pub last_sync: Option<(usize, usize)>,
    /// Why the last build could not delete unused bundles, if it could not.
    pub cleanup_error: Option<String>,
}

impl SymDefs<Declarations> {
    pub fn new(store: BundleStore) -> Self {
        Self::with_producer(store, Declarations)
    }
}

impl<P: Producer<Payload = Vec<StoredDeclaration>>> SymDefs<P> {
    pub fn with_producer(store: BundleStore, producer: P) -> Self {
        Self {
            descriptor: descriptor(),
            store,
            producer,
            data: None,
            last_sync: None,
            cleanup_error: None,
        }
    }

    /// The same map with another descriptor version (a version bump).
    pub fn with_version(mut self, version: u32) -> Self {
        self.descriptor.version = version;
        self
    }

    pub fn data(&self) -> Option<&SymIndex> {
        self.data.as_ref()
    }

    /// Builds the data of `target` from the bundles, extracting the missing
    /// ones; the previous data is replaced only when everything succeeded.
    /// Bundles no file of `target` uses are deleted afterwards.
    pub fn build(&mut self, target: &Target<'_>) -> Result<(), String> {
        let synced = sync(&self.store, &self.producer, target.root, target.files)
            .map_err(|error| format!("{error:#}"))?;
        self.data = Some(SymIndex::join(
            target.snapshot.clone(),
            target.files,
            &synced.payloads,
        ));
        self.last_sync = Some((synced.reused, synced.extracted));
        // A failed clean-up keeps the new data: unused bundles cost disk,
        // not correctness, and the next build retries.
        self.cleanup_error = self
            .store
            .retain(&synced.keys)
            .err()
            .map(|error| format!("{error:#}"));
        Ok(())
    }
}

impl<P: Producer<Payload = Vec<StoredDeclaration>>> Refreshable for SymDefs<P> {
    fn data_snapshot(&self) -> Option<&SnapshotRef> {
        self.data.as_ref().map(|data| &data.snapshot)
    }

    fn refresh(&mut self, target: &Target<'_>) -> Result<(), String> {
        self.build(target)
    }
}

#[async_trait::async_trait]
impl<P: Producer<Payload = Vec<StoredDeclaration>>> Map for SymDefs<P> {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let started = Instant::now();
        let Some(data) = &self.data else {
            return MapResponse::empty(
                &self.descriptor,
                view,
                MapStatus::Failed {
                    warning: format!("map {ID} has no data for this workspace yet"),
                },
            );
        };
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        if let Some(text) = request.text {
            response.hits = answer(data, text, request.limit, view);
        }
        response.freshness.snapshot = data.snapshot.clone();
        response.freshness.stale = data.snapshot != view.snapshot;
        response.cost.nodes_touched = data.entries.len() as u64;
        response.cost.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        response
    }
}

#[cfg(test)]
mod tests;
