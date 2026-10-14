//! Where a traversal gets edges (spine brief 1.3).
//!
//! [`OnTheFly`] asks every enabled map of a [`Registry`] whose `accepts`
//! holds the node's kind, side by side, with the registry's fail-open rules
//! (a panic or a missed deadline becomes a note on the call, never an error),
//! and fuses the edges that reach the same destination: noisy-OR of the best
//! confidence per map, provenance (maps and witness classes) as a union.
//! The pre-merged source comes in a later slice.

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
#[derive(Debug, Clone, PartialEq)]
pub struct ExpandCall {
    pub map: MapId,
    pub n_edges: usize,
    pub cost: Cost,
    /// Set when the call panicked or missed the deadline; the walk goes on.
    pub failure: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Expansion {
    pub edges: Vec<FusedEdge>,
    /// In registry (fusion) order.
    pub calls: Vec<ExpandCall>,
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
}

impl<'r, 'a, 'v> OnTheFly<'r, 'a, 'v> {
    pub fn new(registry: &'r Registry<'a>, view: &'r SnapshotView<'v>) -> Self {
        Self {
            registry,
            view,
            off: HashSet::new(),
        }
    }
}

#[crate::async_trait]
impl EdgeSource for OnTheFly<'_, '_, '_> {
    async fn edges(&self, node: &NodeId, budget: Budget) -> Expansion {
        let kind = node.kind();
        let calls = self
            .registry
            .entries
            .iter()
            .filter(|entry| {
                let descriptor = entry.map.describe();
                entry.enabled
                    && !self.off.contains(descriptor.id)
                    && descriptor.accepts.contains(&kind)
            })
            .map(|entry| async move {
                let id = entry.map.describe().id;
                let remaining = budget.deadline.saturating_duration_since(Instant::now());
                let answer = tokio::time::timeout(
                    remaining,
                    AssertUnwindSafe(entry.map.expand(node, budget, self.view)).catch_unwind(),
                )
                .await;
                match answer {
                    Err(_) => (
                        id,
                        Vec::new(),
                        Some("did not answer before the deadline".into()),
                    ),
                    Ok(Err(_)) => (id, Vec::new(), Some("panicked".to_string())),
                    Ok(Ok(mut edges)) => {
                        // A map is trusted with its edges, not with their
                        // bookkeeping.
                        edges.retain(|edge| edge.dst != *node);
                        for edge in &mut edges {
                            edge.map = id;
                            edge.conf = edge.conf.min(CONF_MAX);
                        }
                        edges.truncate(EXPAND_LIMIT);
                        (id, edges, None)
                    }
                }
            });
        let mut all = Vec::new();
        let mut report = Vec::new();
        for (map, edges, failure) in futures::future::join_all(calls).await {
            report.push(ExpandCall {
                map,
                n_edges: edges.len(),
                cost: edges.first().map(|edge| edge.cost).unwrap_or_default(),
                failure,
            });
            all.extend(edges);
        }
        Expansion {
            edges: fuse(all),
            calls: report,
        }
    }

    fn card(&self, map: MapId) -> MapCard {
        self.registry
            .entries
            .iter()
            .find(|entry| entry.map.describe().id == map)
            .map(|entry| entry.map.card())
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
