//! Where a traversal gets edges (spine brief 1.3).
//!
//! [`OnTheFly`] asks every enabled map of a [`Registry`] whose `accepts`
//! holds the node's kind, with the registry's fail-open rules (a panic or a
//! missed deadline becomes a note on the call, never an error), and fuses the
//! edges that reach the same destination: noisy-OR of the best confidence per
//! map, provenance (maps and witness classes) as a union.
//!
//! External calls are reserved before the maps run. With a finite allowance
//! the maps are asked one after the other in registry order, each with what
//! the maps before it left (`Budget::max_external_calls`), so the allowance
//! is shared deterministically and the calls that ran never pass it; with no
//! limit ([`u32::MAX`]) they are asked side by side. Each call reports its
//! cost once ([`ExpandCall::cost`]), whatever it returned.
//!
//! The pre-merged source ([`super::merged::PreMerged`]) answers from a
//! materialised union built with this same fusion.

use super::merged::FileBlobs;
use crate::{
    Budget, Cost, EXPAND_LIMIT, MapId, Registry, SnapshotView,
    edge::{CONF_MAX, Edge, MapCard, WitnessClass, sort_edges},
};
use futures::FutureExt;
use lci_core::node::NodeId;
use std::{
    cmp::Reverse,
    collections::{BTreeMap, HashSet},
    panic::AssertUnwindSafe,
    sync::OnceLock,
    time::Instant,
};

/// Edges to one destination from one or more maps.
#[derive(Debug, Clone, PartialEq)]
pub struct FusedEdge {
    /// The highest-confidence edge of the group.
    pub best: Edge,
    /// Noisy-OR of the best confidence per map, whole percent.
    pub conf: u8,
    /// Every map that offered the destination, sorted.
    pub maps: Vec<MapId>,
    /// Every witness class among them, strongest first.
    pub witnesses: Vec<WitnessClass>,
    /// Each offering map with the witness class of its best edge, in `maps`
    /// order (the pairing `maps` and `witnesses` lose).
    pub offers: Vec<(MapId, WitnessClass)>,
}

/// Groups `edges` by destination and fuses each group. Sorted by fused
/// confidence (high first), then destination. A total order.
pub fn fuse(edges: Vec<Edge>) -> Vec<FusedEdge> {
    let mut groups: BTreeMap<NodeId, Vec<Edge>> = BTreeMap::new();
    for edge in edges {
        groups.entry(edge.dst.clone()).or_default().push(edge);
    }
    let mut fused: Vec<FusedEdge> = groups
        .into_values()
        .map(|mut group| {
            sort_edges(&mut group);
            // Best confidence per map: the first of each, as sorted.
            let mut seen = HashSet::new();
            let mut miss = 1.0f64;
            for edge in group.iter().filter(|e| seen.insert(e.map)) {
                miss *= 1.0 - f64::from(edge.conf) / f64::from(CONF_MAX);
            }
            let mut maps: Vec<MapId> = group.iter().map(|e| e.map).collect();
            maps.sort_unstable();
            maps.dedup();
            let mut witnesses: Vec<WitnessClass> = group.iter().map(|e| e.witness).collect();
            witnesses.sort_unstable();
            witnesses.dedup();
            let mut offers: Vec<(MapId, WitnessClass)> = Vec::new();
            for edge in &group {
                if !offers.iter().any(|(map, _)| *map == edge.map) {
                    offers.push((edge.map, edge.witness));
                }
            }
            offers.sort_unstable_by_key(|(map, _)| *map);
            FusedEdge {
                offers,
                conf: (f64::from(CONF_MAX) * (1.0 - miss)).round() as u8,
                best: group.swap_remove(0),
                maps,
                witnesses,
            }
        })
        .collect();
    // Stable: equal confidences keep destination order.
    fused.sort_by_key(|f| Reverse(f.conf));
    fused
}

/// One map's part in an expansion.
#[derive(Debug, Clone)]
pub struct ExpandCall {
    pub map: MapId,
    pub n_edges: usize,
    /// What the call cost, once, however many edges it returned (none when
    /// it did not answer).
    pub cost: Cost,
    /// Set when the call panicked or missed the deadline; the walk goes on.
    pub failure: Option<String>,
    /// Wall-clock time the call took, in milliseconds (0 from a pre-merged
    /// adjacency, which makes no call). Only reported: it is left out of the
    /// comparison, so equal expansions are equal whatever the machine did.
    pub ms: f64,
}

impl PartialEq for ExpandCall {
    fn eq(&self, other: &Self) -> bool {
        self.map == other.map
            && self.n_edges == other.n_edges
            && self.cost == other.cost
            && self.failure == other.failure
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Expansion {
    pub edges: Vec<FusedEdge>,
    /// In registry (fusion) order.
    pub calls: Vec<ExpandCall>,
}

/// The expansion cost each map is priced with when a traversal ranks hops: a
/// recorded estimate, so that the ranking does not follow a map's running p50
/// as the machine gets loaded, and so that both edge sources can use the same
/// numbers.
///
/// [`OnTheFly::new`] records the table once, from the maps' cards, and keeps
/// it for its lifetime. [`MergedAdjacency`](super::merged::MergedAdjacency)
/// records one at every build or update and
/// [`PreMerged`](super::merged::PreMerged) prices with it. To compare the two
/// sources, or to rank by a table fixed beforehand, give the on-the-fly source
/// the adjacency's table with [`OnTheFly::with_costs`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostTable {
    p50_ms: BTreeMap<MapId, Option<f64>>,
}

impl CostTable {
    /// What the maps of `registry` report as their p50 now.
    pub fn record(registry: &Registry<'_>) -> Self {
        Self::of_cards(registry.entries.iter().map(|entry| {
            let map = &entry.map;
            (map.describe().id, map.card())
        }))
    }

    /// The p50 of each card.
    pub fn of_cards(cards: impl IntoIterator<Item = (MapId, MapCard)>) -> Self {
        Self {
            p50_ms: cards
                .into_iter()
                .map(|(id, card)| (id, card.expand_p50_ms))
                .collect(),
        }
    }

    /// The recorded p50 of `map`; `None` when it has none or is unknown.
    pub fn p50_ms(&self, map: &str) -> Option<f64> {
        self.p50_ms.get(map).copied().flatten()
    }

    /// Whether the table has an entry for `map` (even an empty one).
    pub fn knows(&self, map: &str) -> bool {
        self.p50_ms.contains_key(map)
    }

    /// `card` with its p50 replaced by the recorded one, when the table knows
    /// the map.
    pub fn priced(&self, map: &str, mut card: MapCard) -> MapCard {
        if self.knows(map) {
            card.expand_p50_ms = self.p50_ms(map);
        }
        card
    }
}

#[crate::async_trait]
pub trait EdgeSource: Send + Sync {
    /// The edges leaving `node`, fused, within `budget`.
    async fn edges(&self, node: &NodeId, budget: Budget) -> Expansion;

    /// What the map says about itself, for hop pricing.
    fn card(&self, map: MapId) -> MapCard;

    /// Switches a map off or back on for this source only; a map the
    /// caller disabled in the registry stays disabled. False when no such
    /// map is known.
    fn set_enabled(&mut self, id: &str, on: bool) -> bool;
}

/// Asks the registry's maps at every expansion.
pub struct OnTheFly<'r, 'a, 'v> {
    registry: &'r Registry<'a>,
    view: &'r SnapshotView<'v>,
    off: HashSet<String>,
    /// Content hashes of the view's files, for the sites of the edges.
    blobs: OnceLock<FileBlobs>,
    /// What hops are priced with (see [`CostTable`]).
    costs: CostTable,
}

impl<'r, 'a, 'v> OnTheFly<'r, 'a, 'v> {
    /// A source that prices hops with the maps' p50 as of now.
    pub fn new(registry: &'r Registry<'a>, view: &'r SnapshotView<'v>) -> Self {
        Self {
            registry,
            view,
            off: HashSet::new(),
            blobs: OnceLock::new(),
            costs: CostTable::record(registry),
        }
    }

    /// Prices hops with `costs` instead of the maps' p50 as of now, so that
    /// this source ranks as another one (a pre-merged adjacency) does.
    pub fn with_costs(mut self, costs: CostTable) -> Self {
        self.costs = costs;
        self
    }

    /// The table hops are priced with.
    pub fn costs(&self) -> &CostTable {
        &self.costs
    }

    /// The blob of each file of the view.
    pub fn blobs(&self) -> &FileBlobs {
        self.blobs.get_or_init(|| FileBlobs::of(self.view))
    }

    /// What the enabled maps say about `node`, before fusion: every edge
    /// (sanitised, sites with their blobs) in registry order, and the calls
    /// made. [`EdgeSource::edges`] fuses exactly these.
    pub async fn ask(&self, node: &NodeId, budget: Budget) -> (Vec<Edge>, Vec<ExpandCall>) {
        let kind = node.kind();
        let eligible: Vec<_> = self
            .registry
            .entries
            .iter()
            .filter(|entry| {
                let descriptor = entry.map.describe();
                entry.enabled
                    && !self.off.contains(descriptor.id)
                    && descriptor.accepts.contains(&kind)
            })
            .collect();
        let answers = if budget.max_external_calls == u32::MAX {
            // No limit to share: side by side.
            futures::future::join_all(eligible.iter().map(|entry| self.call(entry, node, budget)))
                .await
        } else {
            // A finite allowance is shared in registry order: each map gets
            // what the maps before it left.
            let mut left = budget.max_external_calls;
            let mut answers = Vec::with_capacity(eligible.len());
            for entry in eligible {
                let share = Budget {
                    max_external_calls: left,
                    ..budget
                };
                let answer = self.call(entry, node, share).await;
                left = left.saturating_sub(answer.0.cost.external_calls);
                answers.push(answer);
            }
            answers
        };
        let mut all = Vec::new();
        let mut report = Vec::new();
        for (call, edges) in answers {
            report.push(call);
            all.extend(edges);
        }
        (all, report)
    }

    /// One map's answer: the call as reported (id, cost, time, the note when
    /// it did not answer) and its sanitised edges.
    async fn call(
        &self,
        entry: &crate::Entry<'_>,
        node: &NodeId,
        budget: Budget,
    ) -> (ExpandCall, Vec<Edge>) {
        let id = entry.map.describe().id;
        let started = Instant::now();
        let report = |edges: &[Edge], cost: Cost, failure: Option<String>| ExpandCall {
            map: id,
            n_edges: edges.len(),
            cost,
            failure,
            ms: started.elapsed().as_secs_f64() * 1000.0,
        };
        let remaining = budget.deadline.saturating_duration_since(Instant::now());
        let answer = tokio::time::timeout(
            remaining,
            AssertUnwindSafe(entry.map.expand(node, budget, self.view)).catch_unwind(),
        )
        .await;
        match answer {
            Err(_) => (
                report(
                    &[],
                    Cost::default(),
                    Some("did not answer before the deadline".into()),
                ),
                Vec::new(),
            ),
            Ok(Err(_)) => (
                report(&[], Cost::default(), Some("panicked".to_string())),
                Vec::new(),
            ),
            Ok(Ok(expanded)) => {
                // A map is trusted with its edges, not with their bookkeeping.
                let mut edges = expanded.edges;
                edges.retain(|edge| edge.dst != *node);
                for edge in &mut edges {
                    edge.map = id;
                    edge.conf = edge.conf.min(CONF_MAX);
                }
                edges.truncate(EXPAND_LIMIT);
                self.blobs().fill(&mut edges);
                (report(&edges, expanded.cost, None), edges)
            }
        }
    }
}

#[crate::async_trait]
impl EdgeSource for OnTheFly<'_, '_, '_> {
    async fn edges(&self, node: &NodeId, budget: Budget) -> Expansion {
        let (all, calls) = self.ask(node, budget).await;
        Expansion {
            edges: fuse(all),
            calls,
        }
    }

    fn card(&self, map: MapId) -> MapCard {
        self.registry
            .entries
            .iter()
            .find(|entry| entry.map.describe().id == map)
            .map(|entry| self.costs.priced(map, entry.map.card()))
            .unwrap_or_default()
    }

    fn set_enabled(&mut self, id: &str, on: bool) -> bool {
        if !self
            .registry
            .entries
            .iter()
            .any(|entry| entry.map.describe().id == id)
        {
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
