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
//!
//! `[traverse] fusion = "log_odds"` uses the same sources and weights but
//! scores a node
//!
//! `score(node) = sum over sources of weight * clamp(logit(p), -CAP, CAP)`
//!
//! with `p` the best potential the source delivered the node with, and 0 when
//! the source delivered other results but not this node, or delivered it only at
//! potential 0 (outside the beam), which counts as silence: a silent source costs
//! `weight * CAP` ([`LOG_ODDS_CAP`], 3). Prior odds are 1, so a potential of
//! 0.5 is neutral, and no run of low-potential deliveries can move a node past
//! `weight * CAP` per source. Equal scores order by node id.

use super::{Fusion, State, source::EdgeSource};
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

/// Per source: its weight, and the nodes it delivered.
type Sources = (BTreeMap<Source, f64>, BTreeMap<Source, Vec<(usize, f64)>>);

/// The evidence sources of `items`: per source the weight (the largest of its
/// maps), and the nodes it delivered as `(item index, best potential)` in item
/// order. `dependent` says whether a map is one of the maps that count as one
/// source.
fn sources(items: &[Item<'_>], dependent: impl Fn(MapId) -> bool) -> Sources {
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
    (weights, lists)
}

/// The fused scores of `items`, in item order. `dependent` says whether a map
/// is one of the maps that count as one source.
pub fn rrf(items: &[Item<'_>], dependent: impl Fn(MapId) -> bool) -> Vec<f64> {
    let (weights, lists) = sources(items, dependent);
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

/// The most log-odds one source can add (or, when it is silent, cost) per
/// unit of weight: a potential between about 0.047 and 0.953 moves a source,
/// anything beyond it counts as this.
pub const LOG_ODDS_CAP: f64 = 3.0;

/// `ln(p / (1 - p))` with `p` held inside `(1e-6, 1 - 1e-6)`, then clamped to
/// `-LOG_ODDS_CAP..=LOG_ODDS_CAP`.
fn capped_logit(p: f64) -> f64 {
    let p = p.clamp(1e-6, 1.0 - 1e-6);
    (p / (1.0 - p)).ln().clamp(-LOG_ODDS_CAP, LOG_ODDS_CAP)
}

/// The capped log-odds scores of `items`, in item order:
/// `sum over sources of weight * capped_logit(p)`, `p` the best potential with
/// which the source delivered the node and 0 when it did not (so a silent
/// source costs `weight * LOG_ODDS_CAP`). Sources are those of [`rrf`]; a
/// source counts for every node once it has delivered any of them.
///
/// The map weight enters twice: a hop's potential already carries it, and the
/// term is multiplied by it. So a map at weight 0.5 or less never adds a
/// positive term on a first hop; its deliveries only cost less than silence.
pub fn log_odds(items: &[Item<'_>], dependent: impl Fn(MapId) -> bool) -> Vec<f64> {
    let (weights, lists) = sources(items, dependent);
    let mut scores = vec![0.0; items.len()];
    for (source, list) in lists {
        let weight = weights[&source];
        let mut potentials = vec![0.0; items.len()];
        for (at, potential) in list {
            potentials[at] = potential;
        }
        for (score, potential) in scores.iter_mut().zip(potentials) {
            *score += weight * capped_logit(potential);
        }
    }
    scores
}

/// The fused score of every state, parallel to `states`: a result (a state
/// with a parent that is not pruned) gets its score, any other 0. `fusion`
/// is `rrf` or `log_odds`.
pub(super) fn state_scores(
    fusion: Fusion,
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
    let is_dependent = |map: MapId| dependent.get(map).copied().unwrap_or(false);
    let fused = match fusion {
        Fusion::LogOdds => log_odds(&items, is_dependent),
        _ => rrf(&items, is_dependent),
    };
    let mut scores = vec![0.0; states.len()];
    for (&i, score) in results.iter().zip(fused) {
        scores[i] = score;
    }
    scores
}
