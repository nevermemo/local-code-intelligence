//! Weighted reciprocal rank fusion of the results (`[traverse] fusion = "rrf"`;
//! the Qdrant-style alternative to the corroboration boost).
//!
//! An evidence source is a map that delivered a node, with every dependent map
//! (a card that says `dependent`: structure.contains, graph.scip, scip.impact,
//! all derived from one graph) forming ONE source. A seed's own map is a source
//! too. Each source ranks the nodes it delivered by the best potential with
//! which it delivered them (ties by node id), rank starting at 0, and
//!
//! `score(node) = sum over sources of weight / (rank + RRF_K)`
//!
//! where the weight is the map weight (the steering policy's `map_weight`,
//! 1 by default; the largest in a dependent group). A node no source delivered
//! scores 0. Equal scores order by node id.
//!
//! Only the results take part: the states reached by an edge that are not
//! pruned. A start node is not a result, so the seed map's list holds a start
//! node only when a stronger route made it one.

use super::{State, source::EdgeSource};
use crate::MapId;
use lci_core::node::NodeId;
use std::collections::{BTreeMap, BTreeSet, HashMap};

/// Qdrant's default `k` of `1 / (rank + k)`.
pub const RRF_K: f64 = 2.0;

/// An evidence source: one independent map, or all the dependent ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Source {
    Map(MapId),
    Dependent,
}

/// One node's deliveries: the map, its weight, and the best potential with
/// which the map delivered the node.
pub struct Item<'a> {
    pub node: &'a NodeId,
    pub deliveries: Vec<(MapId, f64, f64)>,
}

/// The fused scores of `items`, in item order. `dependent` says whether a map
/// is one of the maps that count as one source.
pub fn rrf(items: &[Item<'_>], dependent: impl Fn(MapId) -> bool) -> Vec<f64> {
    // Per source: the weight, and each node's best potential.
    let mut weights: BTreeMap<Source, f64> = BTreeMap::new();
    let mut lists: BTreeMap<Source, Vec<(usize, f64)>> = BTreeMap::new();
    for (at, item) in items.iter().enumerate() {
        let mut best: BTreeMap<Source, f64> = BTreeMap::new();
        for &(map, weight, potential) in &item.deliveries {
            let source = if dependent(map) {
                Source::Dependent
            } else {
                Source::Map(map)
            };
            let entry = best.entry(source).or_insert(f64::NEG_INFINITY);
            *entry = entry.max(potential);
            let w = weights.entry(source).or_insert(f64::NEG_INFINITY);
            *w = w.max(weight);
        }
        for (source, potential) in best {
            lists.entry(source).or_default().push((at, potential));
        }
    }
    let mut scores = vec![0.0; items.len()];
    for (source, mut list) in lists {
        list.sort_by(|a, b| {
            b.1.total_cmp(&a.1)
                .then_with(|| items[a.0].node.cmp(items[b.0].node))
        });
        let weight = weights[&source];
        for (rank, (at, _)) in list.into_iter().enumerate() {
            scores[at] += weight / (rank as f64 + RRF_K);
        }
    }
    scores
}

/// The fused score of every state, parallel to `states`: a result (a state
/// with a parent that is not pruned) gets its score, any other 0.
pub(super) fn state_scores(
    states: &[State],
    cleared: &BTreeSet<NodeId>,
    source: &dyn EdgeSource,
    weights: &HashMap<String, f64>,
) -> Vec<f64> {
    let results: Vec<usize> = (0..states.len())
        .filter(|&i| states[i].parent.is_some() && !cleared.contains(&states[i].node))
        .collect();
    let mut dependent: BTreeMap<MapId, bool> = BTreeMap::new();
    for &i in &results {
        for &map in states[i].reach.keys() {
            dependent
                .entry(map)
                .or_insert_with(|| source.card(map).dependent);
        }
    }
    let items: Vec<Item<'_>> = results
        .iter()
        .map(|&i| Item {
            node: &states[i].node,
            deliveries: states[i]
                .reach
                .iter()
                .map(|(&map, &potential)| {
                    (map, weights.get(map).copied().unwrap_or(1.0), potential)
                })
                .collect(),
        })
        .collect();
    let fused = rrf(&items, |map| dependent.get(map).copied().unwrap_or(false));
    let mut scores = vec![0.0; states.len()];
    for (&i, score) in results.iter().zip(fused) {
        scores[i] = score;
    }
    scores
}
