//! When a traversal stops, and what a stop shows (spine brief 1.5).
//!
//! A [`StopPolicy`] registers the triggers that make the engine pause and
//! consult its handler (the steering ladder, [`super::ladder`]):
//!
//! | trigger | fires when |
//! | --- | --- |
//! | `every_pops` | the pop count is a multiple of N |
//! | `budget_share` | a multiple of P percent of the pop budget lies between the last stop and now |
//! | `on_disagreement` | at least N disagreements were recorded since the last stop |
//! | `on_stagnation` | the last N pops found no new node |
//! | `likely_complete` | the [`likely_complete`] estimate reaches P percent (once per traversal) |
//!
//! A zero switches a trigger off. Every trigger is also bounded by
//! `max_stops` (stops over the whole traversal, resumes included), and no
//! stop fires twice at one pop count. A budget hit is not a trigger: it ends
//! the call and returns the checkpoint to the agent (rung 3) whatever the
//! policy says.
//!
//! The score bound is not a trigger either and consults no handler: when a
//! result limit `k` is given ([`super::Traversal::top_k`]), the walk ends with
//! the stop reason `bound` (status `complete`, no handle) once at least `k`
//! results exist and neither a state still in the frontier nor a result below
//! the top `k` can rise above the `k`-th one. The bound is
//!
//! `max(best frontier potential, best potential below the top k) * (1 + boost * (n_maps - 1)) <= ranked potential of the k-th result`
//!
//! with `<` instead of `<=` when a tie-break orders tied results (a state
//! that can only tie the `k`-th could be the one the tie-break prefers).
//! `n_maps` is the most a state can be corroborated by: the registered maps
//! that expand nodes, enabled or not (the caller passes their count with `k`,
//! [`super::Traversal::top_k`], and their ids, [`super::Traversal::expanders`];
//! seed-only maps are not counted), and for each state the union of those with
//! the maps whose evidence it kept, so a resumed state that kept evidence from
//! maps that have left still has room to gain from the maps registered now. A hop value is at most 1, so no state found from the
//! frontier has a potential above the frontier's best; ranks only rise by
//! corroboration, at most by the factor above. A result below the top `k`
//! can gain maps from a later arrival, so its potential counts too; the
//! frontier need not hold it. The bound fixes the top-`k` set, not the order
//! inside it: a later arrival can still corroborate a result in the top `k`
//! and reorder them. The `bound` event of the trace gives the numbers.
//!
//! The rest of the module is the payload of a stop: [`Card`]s (one line of
//! about 40 tokens per node), [`MapStat`]s, [`Regions`], [`Disagreement`]s and
//! the [`LikelyComplete`] estimate, and the [`Tally`] of facts the engine
//! keeps to build them (it is part of a handle).
//!
//! *Disagreement* here is the "unconfirmed" kind: a node that a fuzzy map
//! (text, vector, heuristic, agent) offered while a structural map (compiler,
//! language server, exact table) answered the same expansion and did not
//! offer it. The converse (a structural edge no fuzzy map confirms) says
//! nothing, as fuzzy maps return a top-k only.
//!
//! The *likely-complete estimate* is the Lincoln-Petersen mark-recapture
//! estimate over the two maps that found the most nodes: with `n1` and `n2`
//! nodes found by the two and `m` found by both (the re-finds), the number of
//! findable nodes is estimated as `n1 * n2 / m`, and the fraction found is
//! the union over that estimate. It assumes the two maps sample
//! independently, which maps over one code base do not fully; read it as a
//! hint, not a bound. No estimate exists before the two maps share a node.

use super::{State, rules::node_path};
use crate::{MapId, edge::WitnessClass};
use lci_core::node::NodeId;
use std::collections::{BTreeMap, BTreeSet};

/// Cards in a checkpoint's visited list.
pub const CARD_K: usize = 12;
/// Cards in a checkpoint's answer list.
pub const ANSWER_K: usize = 8;
/// A card's text in characters: about 40 tokens at four characters each.
pub const CARD_CHARS: usize = 160;
/// Region rows in a checkpoint.
pub const REGION_ROWS: usize = 8;
/// Disagreements a checkpoint lists.
pub const CHECKPOINT_DISAGREEMENTS: usize = 8;
/// Disagreements a traversal keeps; later ones are not recorded.
pub const MAX_DISAGREEMENTS: usize = 256;

/// Why a checkpoint was built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopReason {
    #[default]
    EveryPops,
    BudgetShare,
    Disagreement,
    Stagnation,
    LikelyComplete,
    /// The call ended on the token or external-call budget.
    Budget,
    /// The call ended on the pop cap.
    Pops,
    /// The call ended on its deadline.
    Deadline,
    /// The agent's steer ended the walk.
    Steer,
    /// The score bound held: nothing left in the frontier can reach the
    /// ranks of the results already found (see the module docs). This ends
    /// the call as `complete`, with no handle.
    Bound,
    /// Nothing was left to expand (or every target was reached): the walk
    /// ran to its end within budget.
    Complete,
}

impl StopReason {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::EveryPops => "every_pops",
            Self::BudgetShare => "budget_share",
            Self::Disagreement => "disagreement",
            Self::Stagnation => "stagnation",
            Self::LikelyComplete => "likely_complete",
            Self::Budget => "budget",
            Self::Pops => "pops",
            Self::Deadline => "deadline",
            Self::Steer => "steer",
            Self::Bound => "bound",
            Self::Complete => "complete",
        }
    }
}

/// Why a call ended with work left, handing the checkpoint to the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pause {
    /// The ladder found no confident rung: rung 3, the outer agent.
    Escalated,
    /// A handler applied `stop`.
    HandlerStop,
    /// The pops, tokens, calls or deadline ran out.
    Budget,
    /// The agent's steer ended the walk.
    Steer,
}

impl Pause {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Escalated => "escalated",
            Self::HandlerStop => "handler_stop",
            Self::Budget => "budget",
            Self::Steer => "steer",
        }
    }
}

/// When a stop fires; see the module docs for each trigger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopPolicy {
    pub every_pops: u32,
    /// Stops over the whole traversal.
    pub max_stops: u32,
    /// Percent of the pop budget between share stops (25: at 25, 50, 75, ..).
    pub budget_share: u32,
    /// New disagreements since the last stop that fire a stop.
    pub on_disagreement: u32,
    /// Pops without a new node that fire a stop.
    pub on_stagnation: u32,
    /// Estimated percent found that fires a stop.
    pub likely_complete: u32,
}

impl Default for StopPolicy {
    fn default() -> Self {
        Self {
            every_pops: 0,
            max_stops: 4,
            budget_share: 0,
            on_disagreement: 0,
            on_stagnation: 0,
            likely_complete: 0,
        }
    }
}

impl StopPolicy {
    /// A stop every `pops` pops, nothing else.
    pub fn every(pops: u32) -> Self {
        Self {
            every_pops: pops,
            ..Self::default()
        }
    }

    pub fn max_stops(mut self, n: u32) -> Self {
        self.max_stops = n;
        self
    }

    pub fn budget_share(mut self, percent: u32) -> Self {
        self.budget_share = percent;
        self
    }

    pub fn on_disagreement(mut self, n: u32) -> Self {
        self.on_disagreement = n;
        self
    }

    pub fn on_stagnation(mut self, pops: u32) -> Self {
        self.on_stagnation = pops;
        self
    }

    pub fn likely_complete(mut self, percent: u32) -> Self {
        self.likely_complete = percent;
        self
    }

    /// Whether any trigger is registered.
    pub fn is_active(&self) -> bool {
        self.every_pops > 0
            || self.budget_share > 0
            || self.on_disagreement > 0
            || self.on_stagnation > 0
            || self.likely_complete > 0
    }
}

/// A node in one line.
#[derive(Debug, Clone, PartialEq)]
pub struct Card {
    pub node: NodeId,
    /// The map whose edge found the node; `None` for a seed without one.
    pub via: Option<MapId>,
    pub witness: Option<WitnessClass>,
    /// The path potential.
    pub potential: f64,
    /// The value results rank by: the potential, lifted by the corroboration
    /// boost for each independent map beyond the first
    /// ([`State::ranked_potential`]); under `rrf` fusion the fused score.
    pub ranked_potential: f64,
    pub depth: u32,
    /// The maps whose edges reached the node, sorted (seed maps are not
    /// counted; see `State::maps`).
    pub maps: Vec<MapId>,
    /// How many of them are independent evidence ([`State::n_maps`]): maps
    /// derived from the same graph count once.
    pub n_maps: usize,
}

impl Card {
    /// The card of a state, ranked with `boost`.
    pub fn of(state: &State, boost: f64) -> Self {
        Self::ranked(state, state.ranked_potential(boost))
    }

    /// The card of a state with the rank the traversal gave it (the fused
    /// score under `rrf` fusion).
    pub fn ranked(state: &State, ranked_potential: f64) -> Self {
        Self {
            node: state.node.clone(),
            via: state.via,
            witness: state.witness,
            potential: state.potential,
            ranked_potential,
            depth: state.depth,
            maps: state.maps.iter().copied().collect(),
            n_maps: state.n_maps(),
        }
    }

    /// `node | map/witness | p0.50 d2`, at most [`CARD_CHARS`] characters; a
    /// long node id loses its start.
    pub fn text(&self) -> String {
        let how = match (self.via, self.witness) {
            (Some(map), Some(w)) => format!("{map}/{}", w.as_str()),
            (Some(map), None) => map.to_string(),
            (None, _) => "seed".to_string(),
        };
        let tail = format!(" | {how} | p{:.2} d{}", self.potential, self.depth);
        let node = self.node.to_string();
        let room = CARD_CHARS.saturating_sub(tail.chars().count());
        let node = if node.chars().count() <= room {
            node
        } else {
            let keep = room.saturating_sub(1);
            let skip = node.chars().count() - keep;
            format!("~{}", node.chars().skip(skip).collect::<String>())
        };
        let mut text = format!("{node}{tail}");
        if text.chars().count() > CARD_CHARS {
            text = text.chars().take(CARD_CHARS).collect();
        }
        text
    }
}

/// What one map has done in a traversal.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MapStat {
    /// Calls made to the map, each one counted whatever came of it (cut by a
    /// budget, retried by a resume, failed).
    pub calls: u32,
    pub failures: u32,
    /// Edges adopted whose best offer was this map's.
    pub edges: u32,
    /// The estimated tokens of its calls, counted once the expansion that made
    /// them completes.
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RegionRow {
    /// The first two path segments of the nodes it groups.
    pub path: String,
    pub explored: usize,
    pub seen: usize,
    pub cleared: usize,
}

/// The state of the search space: expanded, found but not expanded, pruned,
/// and (when an estimate exists) still unknown.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Regions {
    pub explored: usize,
    pub seen: usize,
    /// Estimated nodes not yet found: the estimate less the found.
    pub unknown: Option<usize>,
    pub cleared: usize,
    /// The largest regions by node count, at most [`REGION_ROWS`].
    pub rows: Vec<RegionRow>,
}

impl Regions {
    /// Groups `(node, explored, cleared)` by the first two directories of the path.
    pub fn build<'a>(
        nodes: impl Iterator<Item = (&'a NodeId, bool, bool)>,
        unknown: Option<usize>,
    ) -> Self {
        let mut regions = Self {
            unknown,
            ..Self::default()
        };
        let mut rows: BTreeMap<String, RegionRow> = BTreeMap::new();
        for (node, explored, cleared) in nodes {
            if explored {
                regions.explored += 1;
            } else if cleared {
                regions.cleared += 1;
            } else {
                regions.seen += 1;
            }
            let Some(path) = node_path(node) else {
                continue;
            };
            let dirs: Vec<&str> = path.split('/').collect();
            let dirs = &dirs[..dirs.len().saturating_sub(1)];
            let prefix: Vec<&str> = dirs.iter().copied().take(2).collect();
            let key = if prefix.is_empty() {
                ".".to_string()
            } else {
                prefix.join("/")
            };
            let row = rows.entry(key.clone()).or_insert_with(|| RegionRow {
                path: key,
                explored: 0,
                seen: 0,
                cleared: 0,
            });
            if explored {
                row.explored += 1;
            } else if cleared {
                row.cleared += 1;
            } else {
                row.seen += 1;
            }
        }
        let mut rows: Vec<RegionRow> = rows.into_values().collect();
        rows.sort_by(|a, b| {
            (b.explored + b.seen + b.cleared)
                .cmp(&(a.explored + a.seen + a.cleared))
                .then_with(|| a.path.cmp(&b.path))
        });
        rows.truncate(REGION_ROWS);
        regions.rows = rows;
        regions
    }
}

/// A node a fuzzy map offered that the structural maps, which answered the
/// same expansion, did not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Disagreement {
    /// The pop at which it was seen.
    pub pop: u32,
    pub src: NodeId,
    pub node: NodeId,
    pub offered_by: Vec<MapId>,
    /// The structural maps that answered and did not offer it.
    pub silent: Vec<MapId>,
}

/// The mark-recapture estimate; see the module docs.
#[derive(Debug, Clone, PartialEq)]
pub struct LikelyComplete {
    /// The two maps, the larger finder first.
    pub maps: [MapId; 2],
    pub n1: usize,
    pub n2: usize,
    /// Nodes both found.
    pub refound: usize,
    /// Nodes either found.
    pub found: usize,
    /// Estimated findable nodes, `n1 * n2 / refound`.
    pub estimate: f64,
    /// `found / estimate`, at most 1.
    pub fraction: f64,
}

/// The estimate over what each map has found. None until the two largest
/// finders share a node.
pub fn likely_complete(found: &BTreeMap<MapId, BTreeSet<NodeId>>) -> Option<LikelyComplete> {
    let mut by_size: Vec<(&MapId, &BTreeSet<NodeId>)> =
        found.iter().filter(|(_, set)| !set.is_empty()).collect();
    // Largest first; the map id breaks ties.
    by_size.sort_by(|a, b| b.1.len().cmp(&a.1.len()).then(a.0.cmp(b.0)));
    let [(first, a), (second, b), ..] = by_size.as_slice() else {
        return None;
    };
    let refound = a.intersection(b).count();
    if refound == 0 {
        return None;
    }
    let found = a.union(b).count();
    let estimate = (a.len() as f64 * b.len() as f64) / refound as f64;
    Some(LikelyComplete {
        maps: [**first, **second],
        n1: a.len(),
        n2: b.len(),
        refound,
        found,
        estimate,
        fraction: (found as f64 / estimate).min(1.0),
    })
}

/// Whether a witness class is structural (compiler, language server, exact).
pub fn is_structural(witness: WitnessClass) -> bool {
    witness <= WitnessClass::Exact
}

/// The facts the engine keeps for stops and checkpoints. All of it is part
/// of a handle.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Tally {
    /// The nodes each map has offered (a seed counts for the map that found
    /// it), including the ones offered again: the re-finds.
    pub found: BTreeMap<MapId, BTreeSet<NodeId>>,
    pub maps: BTreeMap<MapId, MapStat>,
    /// Expansions and tokens spent per node kind (prefix), for the cost
    /// estimate of a frontier node.
    pub kind_cost: BTreeMap<String, (u32, u64)>,
    pub disagreements: Vec<Disagreement>,
    /// The latest pop that found a new node (0: the seeds).
    pub last_fresh_pop: u32,
    pub complete_fired: bool,
    /// Budget raised by the agent, over the whole traversal.
    pub raised_pops: u32,
    pub raised_tokens: u64,
}

impl Tally {
    /// The cost of expanding a node of this kind: the mean so far, else 0.
    pub fn est_cost(&self, node: &NodeId) -> u64 {
        match self.kind_cost.get(node.kind().prefix()) {
            Some(&(n, tokens)) if n > 0 => tokens.div_ceil(u64::from(n)),
            _ => 0,
        }
    }

    pub fn disagreements_since(&self, pop: u32) -> usize {
        self.disagreements.iter().filter(|d| d.pop > pop).count()
    }

    /// The triggers of `policy` that hold now, in the order of the table in
    /// the module docs. The caller has checked that a stop may fire at all.
    pub fn triggers(
        &self,
        policy: &StopPolicy,
        pops: u32,
        pops_cap: u32,
        last_stop_at: Option<u32>,
    ) -> Vec<StopReason> {
        let last = last_stop_at.unwrap_or(0);
        let mut out = Vec::new();
        if policy.every_pops > 0 && pops.is_multiple_of(policy.every_pops) {
            out.push(StopReason::EveryPops);
        }
        if policy.budget_share > 0 {
            // A boundary every `share` percent of the cap, in whole pops.
            let width = u64::from(pops_cap) * u64::from(policy.budget_share);
            let bucket = |p: u32| (u64::from(p) * 100).checked_div(width).unwrap_or(0);
            if bucket(pops) > bucket(last) {
                out.push(StopReason::BudgetShare);
            }
        }
        if policy.on_disagreement > 0
            && self.disagreements_since(last) >= policy.on_disagreement as usize
        {
            out.push(StopReason::Disagreement);
        }
        if policy.on_stagnation > 0 {
            let window = self.last_fresh_pop.saturating_add(policy.on_stagnation);
            if pops >= window && last < window {
                out.push(StopReason::Stagnation);
            }
        }
        if policy.likely_complete > 0
            && !self.complete_fired
            && likely_complete(&self.found)
                .is_some_and(|lc| lc.fraction * 100.0 >= f64::from(policy.likely_complete))
        {
            out.push(StopReason::LikelyComplete);
        }
        out
    }
}
