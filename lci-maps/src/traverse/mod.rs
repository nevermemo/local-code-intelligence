//! The traversal engine of the multimap spine (slice S2; spine brief 1.3 to
//! 1.5): best-first search over path states, where one pop expands one state
//! through an [`EdgeSource`].
//!
//! A state's priority is the product of the hop values along its path (the
//! `walk.rs` rule); ties pop in discovery order. A node becomes a state once
//! and is expanded at most once, so a cycle cannot loop. Hop values come from
//! [`frontier::hop`]; the rules scorer prices cross-map hops through the map
//! card.
//!
//! Later arrivals at a node that has a state are not dropped:
//!
//! - A node that is not expanded and is reached by a route with a higher
//!   potential is relaxed: its potential, parent, `via`, witness, depth and
//!   fan-out become those of the stronger route, its queue priority is scaled
//!   by the same ratio (a handler's boost stays), and a `relax` event is
//!   traced. A parent may be a later state than the node. A queued node is
//!   only relaxed to a depth below `max_depth`. A node at `max_depth` (it is
//!   recorded but held back, "deferred") is queued by any route that brings it
//!   below `max_depth`, even a weaker one: that is the only way it can be
//!   expanded (the relax event says `unblocked`). The children of a node that
//!   was cut mid-expansion follow it: when it is relaxed their potentials and
//!   depths are re-derived from the new route (`rescaled` counts them), so they
//!   do not keep the old route's value. A route relaxes a node only if it beats
//!   the node's potential by a relative 1e-9.
//! - A node already popped keeps its state; the route is traced as an
//!   `alt_path` event, once per (node, offering state), also across a resume (a
//!   handle keeps the pairs). The node's own parent re-offering it (a retried
//!   expansion) is not one. An ancestor of the expanding node (a cycle) is
//!   neither.
//! - Corroboration: a [`State`] keeps the set of maps (and witness classes)
//!   whose edges reached it. An arrival from a map not in the set adds it. The
//!   maps that found the seeds ([`Seed::map`]) are start points, not evidence,
//!   and are never in the set: only maps that offered an edge count. Results
//!   rank by [`State::ranked_potential`]: the potential times
//!   `1 + boost * (n_maps - 1)` ([`DEFAULT_CORROBORATION_BOOST`]).
//!
//! Budget (all checked, none exceeded): `pops` before every pop, `deadline`
//! before every pop and inside every map call, `est_tokens` and
//! `external_calls` around every expansion. The maps share the external calls
//! left, in map order ([`source`]), so a map is not given calls another one
//! used. Once the allowance is spent the maps are given none: a map that needs
//! a call returns nothing, one that needs none (a structural map) goes on, and
//! the walk ends (`Complete` if the frontier ran dry) only when nothing is
//! left to expand; the `expand` events then say `external_spent`.
//!
//! An expansion prices and selects first: [`frontier::hop`] ranks every
//! candidate (fresh nodes and arrivals alike) and keeps the beam. Only then is
//! anything charged. Totals count adopted work only: a token per adopted hop
//! ([`TOKENS_PER_EDGE`]) plus the `est_tokens` the maps report for the call,
//! and the hops the remaining tokens afford are the best ranked ones (a
//! selected hop they cannot afford makes the pop `truncated`). The one thing
//! charged that was not adopted is an external call that ran: it was spent,
//! so it counts whatever becomes of the edges, even none. A map call's cost is
//! reported once per call ([`ExpandCall`]), not once per edge.
//!
//! A pop that was cut by a budget (nothing adopted, only some hops adopted,
//! or a map that timed out) is put back in the queue at its old place, so a
//! resume expands the node again and finds the rest. The calls ran, so a cut
//! pop charges them (tokens and external calls) and the hops it adopted, and
//! records what the calls cost as paid (a handle keeps it): the retry runs the
//! maps again, is given back the calls it paid for, and owes only what its
//! calls cost beyond that, so a pop pays for its calls once. A pop that
//! adopted nothing is also taken off the pop count.
//!
//! Arrivals outside the beam still corroborate: the maps and witness classes
//! of an edge into a node that has a state join its sets even when the beam
//! did not keep the edge, at no cost; such an edge never relaxes the node and
//! is not a hop. A hop out of a seed is the first step of the walk, never a
//! cross-map hop, whichever map found the seed.
//!
//! Budgets across calls: `pops`, `est_tokens` and `external_calls` are
//! absolute caps for the whole traversal (the totals persist in a handle);
//! `deadline` and `max_depth` belong to one call. A state at depth
//! `max_depth` is recorded but not expanded (the deepest state expanded is at
//! `max_depth - 1`), and a resume enforces its own call's limit on what an
//! earlier call queued. Only the outer agent can add to the caps, through a
//! [`steer::Steer`], within fixed limits
//! ([`handler::RAISE_MAX_POPS`], [`handler::RAISE_MAX_TOKENS`]); the addition
//! stays with the traversal. See [`steer`].
//!
//! Stops ([`StopPolicy`]: every N pops, a share of the budget, disagreements,
//! stagnation, a likely-complete estimate) hand a [`Checkpoint`] to a
//! [`Handler`], in practice a [`ladder::Ladder`]: rules first, then System
//! One, then the outer agent. Nothing a handler returns can raise a budget;
//! cleared (pruned) nodes are never revisited. When no rung is confident, or
//! a budget ends the call, the call ends with a [`TraversalResult::checkpoint`]
//! and a handle, and the next call carries the agent's steer.
//!
//! Fusion ([`Fusion`], `[traverse] fusion`): `boost` (the default) ranks as
//! above. `rrf` ranks by weighted reciprocal rank fusion instead, see
//! [`fusion`]: each evidence source (an independent map; the dependent maps
//! together; a seed's own map) ranks the nodes it delivered by the best
//! potential with which it delivered them, and a node scores the sum of
//! `weight / (rank + 2)` over the sources. The walk itself is the same in both
//! modes (only the ranking, the cards' `ranked_potential`, the order of the
//! results and the `start` event's `rank` object differ). The score bound
//! below is NOT valid under `rrf` (a node's fused score depends on the ranks
//! of all the others, so no frontier state has a bound on it): it is off, and
//! the walk ends by its budgets or when the frontier is dry. A handle keeps
//! the fusion it started with.
//!
//! Pass-through (`[traverse] pass_through`, off by default): popping a
//! connective node (a file, a commit or a config key: [`is_connective`]) is
//! free, so it does not count toward the pop budget. Its tokens and external
//! calls are charged as for any expansion, a free pop does not make the
//! children of the node free, and the free pops have the pop cap too: the walk
//! ends with the stop reason `pops` when either count reaches it, so a walk
//! pops at most twice the cap. The free pops are counted apart in
//! [`Totals::pass_through_pops`]. A handle keeps the setting.
//!
//! The score bound ([`Traversal::top_k`]) ends a walk early, with the stop
//! reason `bound`, once `k` results exist and nothing queued can lift a result
//! above the `k`-th; see [`stop`]. The `bound` event and the `end` event carry
//! it, and the result's [`TraversalResult::stop_reason`] is `Bound`. Every other end
//! has one too (`complete`, `pops`, `deadline`, `budget`, `steer`, or a stop
//! trigger), on the `end` event and on the result.
//!
//! The run is recorded as JSON events ([`TraversalResult::events`]), written as
//! JSONL by [`trace`]; a paused run leaves a [`handle::Snapshot`] to resume.

pub mod frontier;
pub mod fusion;
pub mod handle;
pub mod handler;
pub mod ladder;
pub mod merged;
pub mod priors;
pub mod rules;
pub mod source;
pub mod steer;
pub mod stop;
pub mod trace;

#[cfg(test)]
mod tests;

use crate::{Budget, MapId, edge::MapCard, edge::WitnessClass};
use frontier::{Candidate, Frontier, HopPrice, HopScorer, RulesScorer, ScoreCtx, hop};
use handler::{
    Action, BOOST_MAX, Checkpoint, Decision, Directive, FrontierItem, Handler, RAISE_MAX_POPS,
    RAISE_MAX_TOKENS, REDIRECT_MAX, RUNG_AGENT, RUNG_RULES, RUNG_SYSTEM_ONE, StopBudget,
    StopPolicy,
};
use lci_core::node::{NodeId, NodeKind};
use priors::HopPriors;
use serde_json::{Value, json};
use source::{EdgeSource, Expansion, FusedEdge};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    time::Instant,
};
use steer::Steer;
use stop::{
    ANSWER_K, CARD_K, CHECKPOINT_DISAGREEMENTS, Card, Disagreement, MAX_DISAGREEMENTS, MapStat,
    Pause, Regions, StopReason, Tally, is_structural, likely_complete,
};

pub use fusion::RRF_K;
/// How results are ranked (`[traverse] fusion`); see the module docs.
pub use lci_core::config::TraverseFusion as Fusion;

/// Tokens charged per adopted edge: roughly one ~12-token card line (a guess).
pub const TOKENS_PER_EDGE: u64 = 12;
/// The most frontier items a checkpoint carries.
pub const CHECKPOINT_FRONTIER: usize = 256;
/// The default share added to a node's rank for each map beyond the first
/// that reached it (see [`State::ranked_potential`]).
pub const DEFAULT_CORROBORATION_BOOST: f64 = 0.1;
/// A route relaxes a node only when it is worth more than the node's own by
/// this relative margin, so float noise in a rescaled subtree is not a route.
const RELAX_TOLERANCE: f64 = 1e-9;

#[derive(Debug, Clone, Copy)]
pub struct TraversalBudget {
    pub pops: u32,
    /// A state at this depth is recorded but not expanded.
    pub max_depth: u32,
    pub deadline: Instant,
    pub est_tokens: u64,
    pub external_calls: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraversalStatus {
    /// The frontier ran dry (or every target was reached) within budget.
    Complete,
    BudgetExhausted,
    /// A handler stopped the walk.
    Stopped,
    /// Targets were asked for and none was reached, whether the budget ran
    /// out or the maps had no more to offer.
    NoPathWithinBudget,
}

impl TraversalStatus {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Stopped => "stopped",
            Self::NoPathWithinBudget => "no_path_within_budget",
        }
    }
}

/// A start node, with the map that found it when a map did.
#[derive(Debug, Clone, PartialEq)]
pub struct Seed {
    pub node: NodeId,
    pub map: Option<MapId>,
}

impl From<NodeId> for Seed {
    fn from(node: NodeId) -> Self {
        Self { node, map: None }
    }
}

/// A discovered path state.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    pub node: NodeId,
    pub parent: Option<usize>,
    pub depth: u32,
    /// The map whose edge discovered the node.
    pub via: Option<MapId>,
    pub witness: Option<WitnessClass>,
    /// Product of the hop values along the path.
    pub potential: f64,
    /// Edges the discovering expansion offered.
    pub fanout: u32,
    /// The maps whose edges reached the node, by any route. A seed's own map
    /// (`Seed::map`) is not here.
    pub maps: BTreeSet<MapId>,
    /// How many of `maps` have a card that says `dependent` (derived from one
    /// shared graph): together they count as one map.
    pub dependent: usize,
    /// The witness classes of those edges.
    pub witnesses: BTreeSet<WitnessClass>,
    /// Under `rrf` fusion only (empty otherwise): the best potential with
    /// which each map delivered the node, a seed's own map included (at 1).
    /// All the maps of a fused edge share the potential of its route; an
    /// arrival outside the beam delivers at 0. When a stronger route rescales
    /// a subtree, the entries of its states scale with their potential.
    pub reach: BTreeMap<MapId, f64>,
}

impl State {
    /// How many independent maps reached the node: the dependent maps count
    /// once among them.
    pub fn n_maps(&self) -> usize {
        self.maps.len() - self.dependent + usize::from(self.dependent > 0)
    }

    /// `potential * (1 + boost * (n_maps - 1))`: the value results are ranked
    /// by. A node no map reached counts as one map.
    pub fn ranked_potential(&self, boost: f64) -> f64 {
        let extra = self.n_maps().saturating_sub(1) as f64;
        self.potential * (1.0 + boost * extra)
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub pops: u32,
    pub hops: u32,
    /// Hops whose discovering map is not the map that found the parent.
    pub cross_map_hops: u32,
    pub tokens: u64,
    pub external_calls: u32,
    /// Pops of connective nodes that did not count toward `pops` (pass-through
    /// only; see the module docs).
    pub pass_through_pops: u32,
    pub stops: u32,
    /// Directives applied at stops.
    pub steers: u32,
    pub elapsed_ms: f64,
}

#[derive(Debug, Clone)]
pub struct TraversalResult {
    pub status: TraversalStatus,
    pub states: Vec<State>,
    /// State indexes in pop order.
    pub popped: Vec<usize>,
    pub cleared: BTreeSet<NodeId>,
    pub totals: Totals,
    /// One JSON object per event, without any wall-clock value: equal runs
    /// give equal events.
    pub events: Vec<Value>,
    /// The state to resume from: set when the walk ended `Stopped` or
    /// `BudgetExhausted` with work left in the frontier.
    pub snapshot: Option<handle::Snapshot>,
    /// What the caller (the outer agent) is shown when the call ends with
    /// work left: set exactly when `snapshot` is, with the handle in it.
    pub checkpoint: Option<Checkpoint>,
    /// The corroboration boost the run ranked with.
    pub corroboration_boost: f64,
    /// How the run ranked its results.
    pub fusion: Fusion,
    /// Whether the run passed through connective nodes (see the module docs).
    pub pass_through: bool,
    /// Under `rrf` fusion: the fused score of every result, by node (empty
    /// otherwise).
    pub fused: HashMap<NodeId, f64>,
    /// Why the call ended, on every exit path: `Complete` (nothing left or
    /// every target reached), `Bound` (the score bound held), `Pops` or
    /// `Deadline` or `Budget` (a cap), `Steer`, or the trigger a handler's
    /// stop answered.
    pub stop_reason: StopReason,
    /// What each map did over the whole traversal (a resume carries the
    /// counts of the calls before it).
    pub maps: BTreeMap<MapId, MapStat>,
    /// Milliseconds each map spent answering expansions in this call. Wall
    /// clock: kept out of the events, the checkpoint and the handle.
    pub map_ms: BTreeMap<MapId, f64>,
}

impl TraversalResult {
    /// The rank of a state of this result: [`State::ranked_potential`] under
    /// `boost` fusion, the fused score under `rrf` (0 for a state that is not
    /// a result, such as a start node).
    pub fn ranked_potential(&self, state: &State) -> f64 {
        match self.fusion {
            Fusion::Boost => state.ranked_potential(self.corroboration_boost),
            Fusion::Rrf => self.fused.get(&state.node).copied().unwrap_or(0.0),
        }
    }

    /// The results, best first: the states reached by an edge that are not
    /// pruned, as indexes into `states`. Equal ranks keep discovery order
    /// under `boost` fusion and go by node id under `rrf`.
    pub fn ranked(&self) -> Vec<usize> {
        let mut results: Vec<usize> = (0..self.states.len())
            .filter(|&i| {
                self.states[i].parent.is_some() && !self.cleared.contains(&self.states[i].node)
            })
            .collect();
        results.sort_by(|&a, &b| {
            let (x, y) = (&self.states[a], &self.states[b]);
            self.ranked_potential(y)
                .total_cmp(&self.ranked_potential(x))
                .then_with(|| match self.fusion {
                    Fusion::Boost => a.cmp(&b),
                    Fusion::Rrf => x.node.cmp(&y.node),
                })
        });
        results
    }

    pub fn reached(&self, node: &NodeId) -> bool {
        self.states.iter().any(|s| s.node == *node)
    }

    /// The events as JSON lines.
    pub fn events_jsonl(&self) -> String {
        trace::jsonl(&self.events)
    }
}

/// A configured traversal; [`Traversal::run`] consumes it.
pub struct Traversal<'s> {
    source: Box<dyn EdgeSource + 's>,
    scorer: Box<dyn HopScorer + 's>,
    price: HopPrice,
    budget: TraversalBudget,
    handler: Option<Box<dyn Handler + 's>>,
    stops: StopPolicy,
    question: String,
    seeds: Vec<Seed>,
    targets: Vec<NodeId>,
    resume: Option<handle::Snapshot>,
    steer: Option<Steer>,
    plan: Option<trace::StartPlan>,
    boost: f64,
    fusion: Fusion,
    pass_through: bool,
    score_bound: bool,
    priors: HopPriors,
    /// `(k, n_maps)` of the score bound; see [`Traversal::top_k`].
    bound: Option<(usize, usize)>,
    identity: String,
}

impl<'s> Traversal<'s> {
    /// The rules scorer, default pricing, no handler, no question, no targets.
    pub fn new(source: impl EdgeSource + 's, seeds: Vec<Seed>, budget: TraversalBudget) -> Self {
        Self {
            source: Box::new(source),
            scorer: Box::new(RulesScorer),
            price: HopPrice::default(),
            budget,
            handler: None,
            stops: StopPolicy::default(),
            question: String::new(),
            seeds,
            targets: Vec::new(),
            resume: None,
            steer: None,
            plan: None,
            boost: DEFAULT_CORROBORATION_BOOST,
            fusion: Fusion::Boost,
            pass_through: false,
            score_bound: true,
            priors: HopPriors::default(),
            bound: None,
            identity: String::new(),
        }
    }

    /// Ends the walk with the stop reason `bound` once `k` results exist and
    /// nothing in the frontier can lift a result above the `k`-th (see
    /// [`stop`]). `n_maps` is how many registered maps expand nodes (seed-only
    /// maps do not count): the most a state can be corroborated by. Not kept
    /// in a handle; a resumed traversal passes it again. Without targets only:
    /// a walk with targets ends when they are reached. `k` of 0 switches the
    /// bound off.
    pub fn top_k(mut self, k: usize, n_maps: usize) -> Self {
        self.bound = (k > 0).then_some((k, n_maps));
        self
    }

    /// What the handle of this traversal is bound to (the workspace and the
    /// index it walks, as the caller fingerprints them). It is written into
    /// the snapshot; a resume with a different identity is the caller's to
    /// refuse, see [`handle::Snapshot::check_identity`]. A resumed traversal
    /// that is given none keeps the snapshot's.
    pub fn identity(mut self, identity: impl Into<String>) -> Self {
        self.identity = identity.into();
        self
    }

    /// The rank share added per corroborating map (default
    /// [`DEFAULT_CORROBORATION_BOOST`]); 0 ranks by potential alone. Not kept
    /// in a handle: a resumed traversal passes it again. A negative or
    /// non-finite value is 0.
    pub fn corroboration_boost(mut self, boost: f64) -> Self {
        self.boost = if boost.is_finite() {
            boost.max(0.0)
        } else {
            0.0
        };
        self
    }

    /// How results are ranked (default [`Fusion::Boost`]); see the module
    /// docs. A resumed traversal keeps the fusion its handle was made with.
    /// Under [`Fusion::Rrf`] the score bound is off.
    pub fn fusion(mut self, fusion: Fusion) -> Self {
        self.fusion = fusion;
        self
    }

    /// Pops of connective nodes (see [`is_connective`]) are free: they do not
    /// count toward the pop budget; the walk ends when they reach the pop
    /// cap. Tokens are charged as ever. A resumed traversal keeps the setting its handle was
    /// made with.
    pub fn pass_through(mut self, on: bool) -> Self {
        self.pass_through = on;
        self
    }

    /// Whether the score bound may end the walk (default true; see
    /// [`Traversal::top_k`]). A resumed traversal keeps the setting its handle
    /// was made with.
    pub fn score_bound(mut self, on: bool) -> Self {
        self.score_bound = on;
        self
    }

    /// The hop priors (default: the guessed [`frontier::kind_prior`] table).
    /// A handle keeps the question type and the content hash of a fitted
    /// table, not the table: the caller passes it again on a resume and checks
    /// it with [`handle::Snapshot::check_priors`].
    pub fn priors(mut self, priors: HopPriors) -> Self {
        self.priors = priors;
        self
    }

    /// What the caller knows of the run that the engine does not (maps with
    /// weights, steering policy, where the seeds came from): written into
    /// the `start` event. Without it the event has none of those fields.
    pub fn plan(mut self, plan: trace::StartPlan) -> Self {
        self.plan = Some(plan);
        self
    }

    /// The outer agent's answer to a returned checkpoint (rung 3), applied
    /// before the first pop. Meant for a resumed traversal.
    pub fn steer(mut self, steer: Steer) -> Self {
        self.steer = Some(steer);
        self
    }

    pub fn scorer(mut self, scorer: impl HopScorer + 's) -> Self {
        self.scorer = Box::new(scorer);
        self
    }

    pub fn price(mut self, price: HopPrice) -> Self {
        self.price = price;
        self
    }

    /// The question text; its tokens feed the overlap term of the scorer.
    pub fn question(mut self, question: impl Into<String>) -> Self {
        self.question = question.into();
        self
    }

    /// Nodes to reach; the walk is `Complete` once all are.
    pub fn targets(mut self, targets: Vec<NodeId>) -> Self {
        self.targets = targets;
        self
    }

    pub fn handler(mut self, handler: impl Handler + 's, stops: StopPolicy) -> Self {
        self.handler = Some(Box::new(handler));
        self.stops = stops;
        self
    }

    pub async fn run(self) -> TraversalResult {
        Run::new(self).run().await
    }
}

/// The dynamic state of one run.
struct Run<'s> {
    t: Traversal<'s>,
    query: Vec<String>,
    states: Vec<State>,
    frontier: Frontier,
    /// The state of each node that has one.
    state_of: HashMap<NodeId, usize>,
    /// Edges into each node seen so far, from expansions that finished or
    /// whose retry is not pending.
    in_seen: HashMap<NodeId, u32>,
    /// The destinations the latest expansion added to `in_seen`, so a pop
    /// that is put back can take them off again.
    counted: Vec<NodeId>,
    expanded: HashSet<usize>,
    popped: Vec<usize>,
    cleared: BTreeSet<NodeId>,
    weights: HashMap<String, f64>,
    cards: HashMap<MapId, MapCard>,
    totals: Totals,
    events: Vec<Value>,
    last_stop_at: Option<u32>,
    /// Map switches applied by handlers, in order (a handle replays them).
    switches: Vec<(String, bool)>,
    tally: Tally,
    /// Set when a stop ends the call: why, and the reason of the stop.
    pause: Option<(Pause, StopReason)>,
    /// The hash of the checkpoint the previous call returned.
    returned: Option<String>,
    /// `(state reached, state that offered the edge)` pairs already traced as
    /// `alt_path`, so a retried expansion does not repeat them (a handle keeps
    /// them, so neither does a resumed one).
    alts: HashSet<(usize, usize)>,
    /// States found at or beyond `max_depth` (or popped past the limit of a
    /// later call) that are not queued, and not expanded. A route that brings
    /// one below the limit queues it; a resume with a deeper limit does too.
    deferred: BTreeSet<usize>,
    /// States that were cut mid-expansion: not expanded, but with children.
    /// Their children follow them when a stronger route relaxes them.
    partial: HashSet<usize>,
    /// Per cut pop, what its calls were already charged: `(tokens, external
    /// calls)`. The retry owes only the difference.
    paid: BTreeMap<usize, (u64, u32)>,
    /// Milliseconds each map spent answering expansions in this call.
    map_ms: BTreeMap<MapId, f64>,
}

impl<'s> Run<'s> {
    fn new(t: Traversal<'s>) -> Self {
        let query = crate::walk::question_tokens(&t.question);
        Self {
            t,
            query,
            states: Vec::new(),
            frontier: Frontier::default(),
            state_of: HashMap::new(),
            in_seen: HashMap::new(),
            counted: Vec::new(),
            expanded: HashSet::new(),
            popped: Vec::new(),
            cleared: BTreeSet::new(),
            weights: HashMap::new(),
            cards: HashMap::new(),
            totals: Totals::default(),
            events: Vec::new(),
            last_stop_at: None,
            switches: Vec::new(),
            tally: Tally::default(),
            pause: None,
            returned: None,
            alts: HashSet::new(),
            deferred: BTreeSet::new(),
            partial: HashSet::new(),
            paid: BTreeMap::new(),
            map_ms: BTreeMap::new(),
        }
    }

    /// The pop cap: the caller's plus what the agent raised.
    fn pops_cap(&self) -> u32 {
        self.t.budget.pops.saturating_add(self.tally.raised_pops)
    }

    fn tokens_cap(&self) -> u64 {
        self.t
            .budget
            .est_tokens
            .saturating_add(self.tally.raised_tokens)
    }

    fn pops_left(&self) -> u32 {
        self.pops_cap().saturating_sub(self.totals.pops)
    }

    fn remaining_pops_pct(&self) -> u32 {
        let total = u64::from(self.pops_cap().max(1));
        let left = u64::from(self.pops_left());
        (left * 100 / total) as u32
    }

    fn tokens_left(&self) -> u64 {
        self.tokens_cap().saturating_sub(self.totals.tokens)
    }

    fn calls_left(&self) -> u32 {
        self.t
            .budget
            .external_calls
            .saturating_sub(self.totals.external_calls)
    }

    fn add_seed(&mut self, seed: &Seed, fanout: u32) {
        if self.cleared.contains(&seed.node) || self.state_of.contains_key(&seed.node) {
            return;
        }
        if let Some(map) = seed.map {
            self.tally
                .found
                .entry(map)
                .or_default()
                .insert(seed.node.clone());
        }
        let index = self.states.len();
        self.states.push(State {
            node: seed.node.clone(),
            parent: None,
            depth: 0,
            via: seed.map,
            witness: None,
            potential: 1.0,
            fanout,
            maps: BTreeSet::new(),
            dependent: 0,
            witnesses: BTreeSet::new(),
            reach: match (self.t.fusion, seed.map) {
                (Fusion::Rrf, Some(map)) => BTreeMap::from([(map, 1.0)]),
                _ => BTreeMap::new(),
            },
        });
        self.state_of.insert(seed.node.clone(), index);
        if self.t.budget.max_depth > 0 {
            self.frontier.push(index, 1.0);
        } else {
            self.deferred.insert(index);
        }
    }

    async fn run(mut self) -> TraversalResult {
        let started = Instant::now();
        if let Some(snapshot) = self.t.resume.take() {
            self.restore(snapshot);
        } else {
            let mut start = json!({
                "ev": "start",
                "question": self.t.question,
                "seeds": self.t.seeds.iter().map(|s| s.node.to_string()).collect::<Vec<_>>(),
                "budget": {
                    "pops": self.t.budget.pops,
                    "max_depth": self.t.budget.max_depth,
                    "est_tokens": self.t.budget.est_tokens,
                    "external_calls": self.t.budget.external_calls,
                },
                "scorer": self.t.scorer.label(),
                "stops": stops_json(&self.t.stops),
                "rank": rank_json(self.t.boost, self.t.fusion),
            });
            if let (true, Some(object)) = (self.t.pass_through, start.as_object_mut()) {
                object.insert("pass_through".into(), json!(true));
            }
            if let (false, Some(object)) = (self.t.score_bound, start.as_object_mut()) {
                object.insert("score_bound".into(), json!(false));
            }
            if let (Some(priors), Some(object)) = (self.t.priors.json(), start.as_object_mut()) {
                object.insert("priors".into(), priors);
            }
            if let (Some(plan), Some(object)) = (&self.t.plan, start.as_object_mut()) {
                for (key, value) in plan.fields(stops_json(&self.t.stops)) {
                    object.insert(key.to_owned(), value);
                }
            }
            self.events.push(start);
            let seeds = std::mem::take(&mut self.t.seeds);
            let fanout = seeds.len() as u32;
            for seed in &seeds {
                self.add_seed(seed, fanout);
            }
        }

        // The agent's answer to the checkpoint the last call returned.
        let steer_stop = match self.t.steer.take() {
            Some(steer) => self.steer(steer),
            None => false,
        };

        // Why the loop below ended, when no stop or steer says so.
        let mut ended = StopReason::Complete;
        let status = loop {
            if steer_stop {
                break TraversalStatus::Stopped;
            }
            if !self.t.targets.is_empty()
                && self.t.targets.iter().all(|t| self.state_of.contains_key(t))
            {
                break TraversalStatus::Complete;
            }
            if self.frontier.is_empty() {
                break self.end_status(false);
            }
            if let Some(bound) = self.bound_holds() {
                self.events.push(bound);
                ended = StopReason::Bound;
                break self.end_status(false);
            }
            let due = self.due();
            if !due.is_empty() && self.stop(&due) {
                break TraversalStatus::Stopped;
            }
            if self.frontier.is_empty() {
                continue;
            }
            // The external-call allowance is not checked here: once it is
            // spent the maps are given none and the walk goes on with the
            // ones that need none (see `expand`).
            let free_spent =
                self.t.pass_through && self.totals.pass_through_pops >= self.pops_cap();
            if free_spent || (self.totals.pops >= self.pops_cap() && !self.next_pop_is_free()) {
                ended = StopReason::Pops;
                break self.end_status(true);
            }
            if Instant::now() >= self.t.budget.deadline {
                ended = StopReason::Deadline;
                break self.end_status(true);
            }
            if self.tokens_left() == 0 {
                ended = StopReason::Budget;
                break self.end_status(true);
            }
            let item = self.frontier.pop().expect("a non-empty frontier pops");
            // An entry for a state that was expanded already is stale.
            if self.expanded.contains(&item.state)
                || self.cleared.contains(&self.states[item.state].node)
            {
                continue;
            }
            // This call's depth limit holds for what an earlier call queued.
            if self.states[item.state].depth >= self.t.budget.max_depth {
                self.deferred.insert(item.state);
                continue;
            }
            let free = self.is_free_pop(item.state);
            if free {
                self.totals.pass_through_pops += 1;
            } else {
                self.totals.pops += 1;
            }
            self.popped.push(item.state);
            self.expanded.insert(item.state);
            if let Some(adopted) = self.expand(item.state, free).await {
                // The node is not done: back in the queue at its old place,
                // for a resume to expand again.
                self.expanded.remove(&item.state);
                self.uncount();
                self.frontier.requeue(item);
                if !adopted {
                    if free {
                        self.totals.pass_through_pops -= 1;
                    } else {
                        self.totals.pops -= 1;
                    }
                    self.popped.pop();
                }
                ended = if Instant::now() >= self.t.budget.deadline {
                    StopReason::Deadline
                } else {
                    StopReason::Budget
                };
                break self.end_status(true);
            }
        };

        self.totals.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        let resumable = matches!(
            status,
            TraversalStatus::Stopped | TraversalStatus::BudgetExhausted
        ) && !self.frontier.is_empty();
        // The call ends with work left: build what goes back to the caller.
        let mut checkpoint = None;
        // A stop or a steer says why; otherwise the loop did.
        let (pause, stop_reason) = self.pause.take().unwrap_or((Pause::Budget, ended));
        if resumable {
            let cp = self.checkpoint(stop_reason);
            let hash = trace::checkpoint_hash(&cp);
            self.events.push(json!({
                "ev": "return", "reason": pause.as_str(), "stop": cp.stop,
                "checkpoint_hash": hash,
            }));
            self.returned = Some(hash);
            checkpoint = Some(cp);
        }
        let mut end = json!({
            "ev": "end",
            "status": status.as_str(),
            "answer_nodes": self.states.len(),
            "totals": {
                "pops": self.totals.pops,
                "hops": self.totals.hops,
                "cross_map_hops": self.totals.cross_map_hops,
                "tokens": self.totals.tokens,
                "external_calls": self.totals.external_calls,
                "stops": self.totals.stops,
                "steers": self.totals.steers,
            },
        });
        if let Some(object) = end.as_object_mut() {
            object.insert("stop_reason".into(), json!(stop_reason.as_str()));
            if self.t.pass_through {
                object["totals"]["pass_through_pops"] = json!(self.totals.pass_through_pops);
            }
        }
        self.events.push(end);
        let snapshot = resumable.then(|| self.snapshot());
        if let (Some(cp), Some(snapshot)) = (checkpoint.as_mut(), &snapshot) {
            cp.handle = Some(snapshot.handle());
        }
        let fused = self.fused_scores();
        let fused_by_node = fused
            .as_ref()
            .map(|scores| {
                self.states
                    .iter()
                    .zip(scores)
                    .filter(|(s, _)| s.parent.is_some() && !self.cleared.contains(&s.node))
                    .map(|(s, score)| (s.node.clone(), *score))
                    .collect()
            })
            .unwrap_or_default();
        TraversalResult {
            status,
            snapshot,
            checkpoint,
            states: self.states,
            popped: self.popped,
            cleared: self.cleared,
            totals: self.totals,
            events: self.events,
            corroboration_boost: self.t.boost,
            fusion: self.t.fusion,
            pass_through: self.t.pass_through,
            fused: fused_by_node,
            stop_reason,
            maps: self.tally.maps,
            map_ms: self.map_ms,
        }
    }

    /// Whether popping `state` is free: pass-through is on and the node is
    /// connective. Only the node itself is free; the children it adopts pay
    /// when their turn comes. (The loop has ended the walk before a free pop
    /// past the pop cap, so this need not check it; ending, rather than
    /// counting the pop, keeps a paused run a prefix of the straight run.)
    fn is_free_pop(&self, state: usize) -> bool {
        self.t.pass_through && is_connective(&self.states[state].node)
    }

    /// Whether the next item of the frontier would be a free pop, so the pop
    /// cap does not end the walk before it.
    fn next_pop_is_free(&mut self) -> bool {
        self.t.pass_through
            && self
                .frontier
                .peek()
                .is_some_and(|item| self.is_free_pop(item.state))
    }

    /// The `bound` event when the score bound holds (see [`stop`]): a result
    /// limit is set, no targets are asked for, at least `k` results exist and
    /// the best state in the frontier, at the most corroboration a state can
    /// have, cannot rank above the `k`-th result.
    fn bound_holds(&self) -> Option<Value> {
        let (k, n_maps) = self.t.bound?;
        // A fused score depends on the ranks of all the other results, so no
        // frontier state has a bound on it.
        if !self.t.score_bound || self.t.fusion == Fusion::Rrf || !self.t.targets.is_empty() {
            return None;
        }
        // The results: every state reached by an edge that is not pruned.
        let mut ranks: Vec<f64> = self
            .states
            .iter()
            .filter(|s| s.parent.is_some() && !self.cleared.contains(&s.node))
            .map(|s| s.ranked_potential(self.t.boost))
            .collect();
        if ranks.len() < k {
            return None;
        }
        let (_, kth, _) = ranks.select_nth_unstable_by(k - 1, |a, b| b.total_cmp(a));
        let kth = *kth;
        // Descendants of a state have at most its potential (a hop is worth
        // at most 1); a priority a handler scaled does not change that.
        let best = self
            .frontier
            .states()
            .map(|s| self.states[s].potential)
            .fold(f64::NEG_INFINITY, f64::max);
        // The most a state can be corroborated by: the maps now registered, or
        // the evidence a resumed state kept from maps that have left.
        let n_maps = self
            .states
            .iter()
            .map(State::n_maps)
            .fold(n_maps, usize::max);
        let factor = 1.0 + self.t.boost * n_maps.saturating_sub(1) as f64;
        (best * factor <= kth).then(|| {
            json!({
                "ev": "bound", "pop": self.totals.pops, "k": k, "kth": kth,
                "frontier_best": best, "max_factor": factor, "maps": n_maps,
                "frontier": self.frontier.len(),
            })
        })
    }

    fn end_status(&self, exhausted: bool) -> TraversalStatus {
        let none_reached = !self.t.targets.is_empty()
            && !self.t.targets.iter().any(|t| self.state_of.contains_key(t));
        if none_reached {
            TraversalStatus::NoPathWithinBudget
        } else if exhausted {
            TraversalStatus::BudgetExhausted
        } else {
            TraversalStatus::Complete
        }
    }

    fn map_card(&mut self, map: MapId) -> &MapCard {
        let source = &self.t.source;
        self.cards.entry(map).or_insert_with(|| source.card(map))
    }

    fn card(&mut self, map: MapId) -> MapCard {
        self.map_card(map).clone()
    }

    /// Expands one state. `Some` when a budget (or a timeout) cut the
    /// expansion short: true if some of its work was adopted, false if none.
    ///
    /// The order is: ask the maps; price and select every candidate with
    /// [`hop`]; only then charge. A fresh hop is adopted while the tokens
    /// left afford [`TOKENS_PER_EDGE`] for it, so what the beam ranks first is
    /// what the budget buys; a selected hop it cannot afford makes the pop
    /// `truncated`. Charges:
    ///
    /// - External calls are charged where they ran, adopted or not.
    /// - Tokens are charged for adopted work: [`TOKENS_PER_EDGE`] per hop, and
    ///   the maps' `est_tokens` once the pop is not cut. A cut pop (truncated
    ///   or timed out) charges its adopted hops only and is put back, so the
    ///   retry that completes it is the one that charges the calls' tokens: a
    ///   pop pays for its calls' tokens once. Its external calls are charged
    ///   each time they run, since each run spent them.
    async fn expand(&mut self, index: usize, free: bool) -> Option<bool> {
        let node = self.states[index].node.clone();
        let depth = self.states[index].depth;
        // What an earlier run of this pop already paid for its calls.
        let (paid_tokens, paid_calls) = self.paid.get(&index).copied().unwrap_or((0, 0));
        let calls_before = self.calls_left();
        let budget = Budget {
            deadline: self.t.budget.deadline,
            // The retry of a pop is given again the calls it paid for.
            max_external_calls: calls_before.saturating_add(paid_calls),
            max_nodes_touched: u64::MAX,
        };
        self.counted.clear();
        let Expansion { edges, calls } = self.t.source.edges(&node, budget).await;

        // Every call that ran is counted here, before any early exit, cut or
        // retried; the tokens of a call are counted once it completes.
        for call in &calls {
            *self.map_ms.entry(call.map).or_default() += call.ms;
            let stat = self.tally.maps.entry(call.map).or_default();
            stat.calls += 1;
            stat.failures += u32::from(call.failure.is_some());
        }
        let call_tokens: u64 = calls.iter().map(|c| c.cost.est_tokens).sum();
        let call_external: u32 = calls
            .iter()
            .map(|c| c.cost.external_calls)
            .fold(0, u32::saturating_add);
        // The calls ran: they are charged whatever becomes of the edges, but
        // a retry owes only what its earlier run did not pay.
        let owed_tokens = call_tokens.saturating_sub(paid_tokens);
        let owed_calls = call_external.saturating_sub(paid_calls);
        self.totals.external_calls = self.totals.external_calls.saturating_add(owed_calls);
        let call_report: Vec<Value> = calls
            .iter()
            .map(|c| {
                json!({
                    "map": c.map, "n_edges": c.n_edges,
                    "est_tokens": c.cost.est_tokens,
                    "external_calls": c.cost.external_calls,
                    "failure": c.failure,
                })
            })
            .collect();
        if owed_tokens > self.tokens_left() || owed_calls > calls_before {
            // The external calls that ran are paid; the tokens were not.
            if owed_calls > 0 {
                self.paid
                    .insert(index, (paid_tokens, call_external.max(paid_calls)));
            }
            let mut event = json!({
                "ev": "expand", "pop": self.totals.pops, "node": node.to_string(),
                "offered": edges.len(), "adopted": 0, "calls": call_report,
                "dropped": "budget",
            });
            if let (true, Some(object)) = (free, event.as_object_mut()) {
                object.insert("free".into(), json!(true));
            }
            self.events.push(event);
            return Some(false);
        }

        // Candidates: never a cleared node.
        let offered = edges
            .into_iter()
            .filter(|e| !self.cleared.contains(&e.best.dst))
            .collect::<Vec<_>>();
        let fanout = offered.len() as u32;
        for edge in &offered {
            *self.in_seen.entry(edge.best.dst.clone()).or_default() += 1;
            self.counted.push(edge.best.dst.clone());
            for map in &edge.maps {
                self.tally
                    .found
                    .entry(*map)
                    .or_default()
                    .insert(edge.best.dst.clone());
            }
        }
        self.note_disagreements(&node, &offered);
        // Edges to a node that has a state are arrivals, not discoveries; an
        // edge back to the expanding node or its own path is a cycle and is
        // neither.
        let mut kept: Vec<FusedEdge> = Vec::new();
        let mut target: Vec<Option<usize>> = Vec::new();
        for edge in offered {
            let state = self.state_of.get(&edge.best.dst).copied();
            if state.is_some_and(|s| self.on_path(index, s)) {
                continue;
            }
            kept.push(edge);
            target.push(state);
        }

        // Price and select first.
        let cards: Vec<MapCard> = kept.iter().map(|e| self.card(e.best.map)).collect();
        let cands: Vec<Candidate> = kept
            .iter()
            .zip(cards)
            .map(|(edge, card)| Candidate {
                edge,
                in_seen: self.in_seen.get(&edge.best.dst).copied().unwrap_or(0),
                card,
            })
            .collect();
        let ctx = ScoreCtx {
            query: &self.query,
            nodes_seen: self.states.len(),
            priors: &self.t.priors,
        };
        let steps = hop(self.t.scorer.as_mut(), &ctx, &cands, self.t.price);

        // Then adopt what the tokens afford, best first.
        let affordable = ((self.tokens_left() - owed_tokens) / TOKENS_PER_EDGE) as usize;
        let at = self.events.len();
        let parent = self.states[index].clone();
        let mut chosen = vec![false; kept.len()];
        let mut adopted = 0usize;
        let mut truncated = false;
        for (i, value) in steps {
            chosen[i] = true;
            let edge = &kept[i];
            let dst = &edge.best.dst;
            let map = edge.best.map;
            let value = value * self.weights.get(map).copied().unwrap_or(1.0);
            let potential = parent.potential * value;
            // A hop out of a seed is the first step of the walk, not a switch
            // between maps, whichever map found the seed.
            let cross = parent.parent.is_some() && parent.via != Some(map);
            if let Some(reached) = target[i] {
                self.arrive(index, &parent, reached, edge, potential, fanout, cross);
                continue;
            }
            if adopted >= affordable {
                truncated = true;
                continue;
            }
            adopted += 1;
            self.tally.maps.entry(map).or_default().edges += 1;
            let child = self.states.len();
            self.states.push(State {
                node: dst.clone(),
                parent: Some(index),
                depth: depth + 1,
                via: Some(map),
                witness: Some(edge.best.witness),
                potential,
                fanout,
                maps: BTreeSet::new(),
                dependent: 0,
                witnesses: BTreeSet::new(),
                reach: BTreeMap::new(),
            });
            self.add_evidence(child, edge, potential);
            self.state_of.insert(dst.clone(), child);
            self.totals.hops += 1;
            self.totals.cross_map_hops += u32::from(cross);
            self.events.push(json!({
                "ev": "hop", "src": node.to_string(), "dst": dst.to_string(),
                "kind": edge.best.kind.as_str(), "map": map, "maps": edge.maps,
                "witness": edge.best.witness.as_str(), "conf": edge.conf,
                "potential": potential, "cross_map": cross,
            }));
            if depth + 1 < self.t.budget.max_depth {
                self.frontier.push(child, potential);
            } else {
                self.deferred.insert(child);
            }
        }
        // An arrival outside the beam is still evidence: the maps that
        // offered it join the node's corroboration at no cost. It is not a
        // route, so it never relaxes the node and is not a hop.
        let mut corroborated = 0;
        for (i, edge) in kept.iter().enumerate() {
            if let (false, Some(reached)) = (chosen[i], target[i]) {
                self.add_evidence(reached, edge, 0.0);
                corroborated += 1;
            }
        }

        // A call that failed after the deadline left the node half-explored.
        let timed_out =
            calls.iter().any(|c| c.failure.is_some()) && Instant::now() >= self.t.budget.deadline;
        let cut = truncated || timed_out;

        // Now charge.
        let hop_tokens = adopted as u64 * TOKENS_PER_EDGE;
        // The calls ran, so they are paid now, cut or not; a cut pop is marked
        // so that its retry owes only what this run did not pay.
        let call_charge = owed_tokens;
        self.totals.tokens += call_charge + hop_tokens;
        if cut {
            self.paid.insert(
                index,
                (paid_tokens.max(call_tokens), paid_calls.max(call_external)),
            );
            if adopted > 0 {
                self.partial.insert(index);
            }
        } else {
            self.paid.remove(&index);
            self.partial.remove(&index);
        }
        if !cut {
            for call in &calls {
                self.tally.maps.entry(call.map).or_default().tokens += call.cost.est_tokens;
            }
        }
        let cost = self
            .tally
            .kind_cost
            .entry(node.kind().prefix().to_string())
            .or_default();
        cost.0 += u32::from(!cut);
        cost.1 += call_charge + hop_tokens;
        if adopted > 0 {
            self.tally.last_fresh_pop = self.totals.pops;
        }
        let mut event = json!({
            "ev": "expand", "pop": self.totals.pops, "node": node.to_string(),
            "offered": fanout, "adopted": adopted, "corroborated": corroborated,
            "calls": call_report, "tokens": call_charge + hop_tokens, "cut": cut,
        });
        if let Some(object) = event.as_object_mut() {
            if free {
                object.insert("free".into(), json!(true));
            }
            if (paid_tokens, paid_calls) != (0, 0) {
                object.insert("paid_before".into(), json!([paid_tokens, paid_calls]));
            }
            // The maps were given no external call for this expansion.
            if self.t.budget.external_calls > 0 && calls_before == 0 {
                object.insert("external_spent".into(), json!(true));
            }
        }
        self.events.insert(at, event);
        cut.then_some(adopted > 0 || timed_out)
    }

    /// Whether `target` is `from` or one of its ancestors.
    fn on_path(&self, from: usize, target: usize) -> bool {
        let mut at = Some(from);
        while let Some(i) = at {
            if i == target {
                return true;
            }
            at = self.states[i].parent;
        }
        false
    }

    /// The maps and witness classes of `edge` join the evidence of a state
    /// (an edge that was not followed still counts for the node); returns the
    /// maps that were new to it.
    ///
    /// Under `rrf` fusion the maps of the edge also deliver the node at
    /// `potential` (their best so far is kept): the potential of the route the
    /// edge is on, 0 for an arrival the beam did not keep.
    fn add_evidence(&mut self, index: usize, edge: &FusedEdge, potential: f64) -> Vec<MapId> {
        if self.t.fusion == Fusion::Rrf {
            for &map in &edge.maps {
                let reach = self.states[index].reach.entry(map).or_insert(potential);
                *reach = reach.max(potential);
            }
        }
        let mut new_maps = Vec::new();
        for &map in &edge.maps {
            if self.states[index].maps.contains(&map) {
                continue;
            }
            let dependent = self.map_card(map).dependent;
            let state = &mut self.states[index];
            state.maps.insert(map);
            new_maps.push(map);
            state.dependent += usize::from(dependent);
        }
        self.states[index]
            .witnesses
            .extend(edge.witnesses.iter().copied());
        new_maps
    }

    /// A route from the expanding state `from` (`parent`) to a node that
    /// already has a state (`reached`), at `potential`. The maps and witness
    /// classes of the edge join the node's corroboration.
    ///
    /// A node that is not expanded is relaxed onto the new route when the
    /// route is worth more, or when it brings a node that sits at the depth
    /// limit (so it is not queued) below the limit, however weak: that is the
    /// only way the node can be expanded. A queued node is never relaxed to
    /// the limit. The children of a node that was cut mid-expansion follow it
    /// (their potentials and depths are re-derived from the new route). Any
    /// other route is traced as an `alt_path`, once per (node, offering
    /// state), and a route that is the node's own parent is not one. The node
    /// is never expanded again.
    #[allow(clippy::too_many_arguments)]
    fn arrive(
        &mut self,
        from: usize,
        parent: &State,
        reached: usize,
        edge: &FusedEdge,
        potential: f64,
        fanout: u32,
        cross: bool,
    ) {
        let depth = parent.depth + 1;
        let max_depth = self.t.budget.max_depth;
        let popped = self.expanded.contains(&reached);
        let queued = self.frontier.contains(reached);
        let blocked = self.deferred.contains(&reached);
        let old_src = self.states[reached]
            .parent
            .map(|p| self.states[p].node.to_string());
        let new_maps = self.add_evidence(reached, edge, potential);
        let state = &mut self.states[reached];

        let old = state.potential;
        let own_parent = state.parent == Some(from);
        // The tolerance keeps a retried expansion, whose subtree was
        // rescaled, from reading its own route as a stronger one.
        let better = potential > old * (1.0 + RELAX_TOLERANCE);
        let unblock = blocked && depth < max_depth;
        // A queued node must stay below the depth limit.
        let stronger = !popped && (unblock || (better && (!queued || depth < max_depth)));
        let dst = state.node.to_string();
        if !stronger {
            if !own_parent && self.alts.insert((reached, from)) {
                self.events.push(json!({
                    "ev": "alt_path", "src": parent.node.to_string(), "dst": dst,
                    "kind": edge.best.kind.as_str(), "map": edge.best.map,
                    "maps": edge.maps, "witness": edge.best.witness.as_str(),
                    "potential": potential, "state_potential": old,
                    "popped": popped, "new_maps": new_maps,
                }));
            }
            return;
        }
        state.parent = Some(from);
        state.depth = depth;
        state.via = Some(edge.best.map);
        state.witness = Some(edge.best.witness);
        state.potential = potential;
        state.fanout = fanout;
        let mut event = json!({
            "ev": "relax", "src": parent.node.to_string(), "dst": dst,
            "kind": edge.best.kind.as_str(), "map": edge.best.map,
            "maps": edge.maps, "witness": edge.best.witness.as_str(),
            "old_potential": old, "potential": potential, "old_src": old_src,
            "depth": depth, "cross_map": cross, "new_maps": new_maps,
        });
        if unblock {
            self.deferred.remove(&reached);
            self.frontier.push(reached, potential);
        } else if queued {
            match self.frontier.priority_of(reached) {
                // A handler's boost or reorder stays: scale by the ratio.
                Some(priority) if priority != old && old > 0.0 => {
                    self.frontier.scale(reached, potential / old);
                }
                _ => {
                    self.frontier.set_priority(reached, potential);
                }
            }
        }
        let rescaled = if self.partial.contains(&reached) && old > 0.0 {
            self.rescale_below(reached, potential / old)
        } else {
            0
        };
        if let Some(object) = event.as_object_mut() {
            if unblock {
                object.insert("unblocked".into(), json!(true));
            }
            if rescaled > 0 {
                object.insert("rescaled".into(), json!(rescaled));
            }
        }
        self.events.push(event);
    }

    /// The states found through `root`, whose route has just changed: their
    /// potential is multiplied by `ratio` (a route's value is the product of
    /// its hops, and the hops below `root` did not change), their depth is
    /// re-derived from `root`'s, and the queue follows: a queued state keeps
    /// the share a handler gave it, one that now sits at the depth limit
    /// leaves the queue, one that no longer does joins it. Returns how many.
    fn rescale_below(&mut self, root: usize, ratio: f64) -> usize {
        let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, s) in self.states.iter().enumerate() {
            if let Some(p) = s.parent {
                children.entry(p).or_default().push(i);
            }
        }
        let max_depth = self.t.budget.max_depth;
        let mut work = vec![root];
        let mut count = 0;
        while let Some(at) = work.pop() {
            let depth = self.states[at].depth;
            for &child in children.get(&at).into_iter().flatten() {
                if child == root {
                    continue;
                }
                let state = &mut self.states[child];
                state.potential *= ratio;
                for reach in state.reach.values_mut() {
                    *reach *= ratio;
                }
                state.depth = depth + 1;
                let potential = state.potential;
                count += 1;
                if !self.expanded.contains(&child)
                    && !self.cleared.contains(&self.states[child].node)
                {
                    if self.states[child].depth >= max_depth {
                        self.frontier.remove(child);
                        self.deferred.insert(child);
                    } else if self.frontier.contains(child) {
                        self.frontier.scale(child, ratio);
                    } else if self.deferred.remove(&child) {
                        self.frontier.push(child, potential);
                    }
                }
                work.push(child);
            }
        }
        count
    }

    /// Takes the latest expansion's edges off `in_seen`: its pop is put back
    /// and the retry counts them again (an edge counts once per expansion
    /// that finished).
    fn uncount(&mut self) {
        for dst in std::mem::take(&mut self.counted) {
            if let Some(count) = self.in_seen.get_mut(&dst) {
                *count -= 1;
                if *count == 0 {
                    self.in_seen.remove(&dst);
                }
            }
        }
    }

    /// Records the nodes a fuzzy map offered that the structural maps, which
    /// answered this expansion, did not (see [`stop`]).
    fn note_disagreements(&mut self, src: &NodeId, offered: &[FusedEdge]) {
        let mut structural: Vec<MapId> = offered
            .iter()
            .flat_map(|e| e.offers.iter())
            .filter(|(_, w)| is_structural(*w))
            .map(|(map, _)| *map)
            .collect();
        structural.sort_unstable();
        structural.dedup();
        if structural.is_empty() {
            return;
        }
        for edge in offered {
            if edge.offers.iter().any(|(_, w)| is_structural(*w)) {
                continue;
            }
            let known = self
                .tally
                .disagreements
                .iter()
                .any(|d| d.src == *src && d.node == edge.best.dst);
            if known || self.tally.disagreements.len() >= MAX_DISAGREEMENTS {
                continue;
            }
            self.tally.disagreements.push(Disagreement {
                pop: self.totals.pops,
                src: src.clone(),
                node: edge.best.dst.clone(),
                offered_by: edge.maps.clone(),
                silent: structural.clone(),
            });
        }
    }

    /// The stop triggers that hold now; none when no stop may fire.
    fn due(&self) -> Vec<StopReason> {
        let policy = self.t.stops;
        if self.t.handler.is_none()
            || !policy.is_active()
            || self.totals.stops >= policy.max_stops
            || self.totals.pops == 0
            || self.last_stop_at == Some(self.totals.pops)
        {
            return Vec::new();
        }
        self.tally.triggers(
            &policy,
            self.totals.pops,
            self.pops_cap(),
            self.last_stop_at,
        )
    }

    /// The fused score of every state under `rrf` fusion (see [`fusion`]).
    fn fused_scores(&self) -> Option<Vec<f64>> {
        (self.t.fusion == Fusion::Rrf).then(|| {
            fusion::state_scores(
                &self.states,
                &self.cleared,
                self.t.source.as_ref(),
                &self.weights,
            )
        })
    }

    /// What results rank by for a state: `fused` is [`Run::fused_scores`].
    fn rank_of(&self, state: usize, fused: Option<&[f64]>) -> f64 {
        match fused {
            Some(scores) => scores[state],
            None => self.states[state].ranked_potential(self.t.boost),
        }
    }

    fn card_of(&self, state: usize, fused: Option<&[f64]>) -> Card {
        Card::ranked(&self.states[state], self.rank_of(state, fused))
    }

    fn checkpoint(&self, reason: StopReason) -> Checkpoint {
        let frontier = self
            .frontier
            .sorted()
            .into_iter()
            .take(CHECKPOINT_FRONTIER)
            .map(|item| {
                let state = &self.states[item.state];
                FrontierItem {
                    node: state.node.clone(),
                    via: state.via,
                    witness: state.witness,
                    score: item.priority,
                    potential: state.potential,
                    depth: state.depth,
                    fanout: state.fanout,
                    seen_before: self.in_seen.get(&state.node).copied().unwrap_or(0) > 1,
                    est_cost: self.tally.est_cost(&state.node),
                }
            })
            .collect();

        // Best rank first (potential, boosted by corroboration, or the fused
        // score); discovery order breaks ties under `boost`, node id under
        // `rrf`.
        let fused = self.fused_scores();
        let fused = fused.as_deref();
        let rank = |s: &usize| self.rank_of(*s, fused);
        let best_first = |a: &usize, b: &usize| {
            rank(b).total_cmp(&rank(a)).then_with(|| match fused {
                Some(_) => self.states[*a].node.cmp(&self.states[*b].node),
                None => a.cmp(b),
            })
        };
        let mut visited: Vec<usize> = self.expanded.iter().copied().collect();
        visited.sort_by(best_first);
        let visited_cards = visited
            .iter()
            .take(CARD_K)
            .map(|&s| self.card_of(s, fused))
            .collect();
        let mut answer: Vec<Card> = self
            .t
            .targets
            .iter()
            .filter_map(|t| self.state_of.get(t))
            .map(|&s| self.card_of(s, fused))
            .collect();
        answer.extend(
            visited
                .iter()
                .filter(|&&s| self.states[s].parent.is_some())
                .map(|&s| self.card_of(s, fused))
                .filter(|card| !answer.iter().any(|a| a.node == card.node))
                .take(ANSWER_K.saturating_sub(answer.len()))
                .collect::<Vec<_>>(),
        );
        answer.truncate(ANSWER_K);

        let estimate = likely_complete(&self.tally.found);
        let unknown = estimate
            .as_ref()
            .map(|lc| (lc.estimate - lc.found as f64).max(0.0).round() as usize);
        let regions = Regions::build(
            self.states.iter().enumerate().map(|(i, s)| {
                (
                    &s.node,
                    self.expanded.contains(&i),
                    self.cleared.contains(&s.node),
                )
            }),
            unknown,
        );
        let latest = self
            .tally
            .disagreements
            .len()
            .saturating_sub(CHECKPOINT_DISAGREEMENTS);
        Checkpoint {
            reason,
            handle: None,
            question: self.t.question.clone(),
            seeds: self
                .states
                .iter()
                .filter(|s| s.parent.is_none())
                .map(|s| s.node.clone())
                .collect(),
            pops: self.totals.pops,
            stop: self.totals.stops,
            budget_left_pct: self.remaining_pops_pct(),
            pops_left: self.pops_left(),
            tokens_left: self.tokens_left(),
            calls_left: self.calls_left(),
            answer,
            visited_cards,
            frontier,
            maps: self.tally.maps.iter().map(|(id, s)| (*id, *s)).collect(),
            regions,
            visited: self.expanded.len(),
            cleared: self.cleared.len(),
            disagreements: self.tally.disagreements[latest..].to_vec(),
            disagreements_total: self.tally.disagreements.len(),
            likely_complete: estimate,
        }
    }

    /// Runs the handler at a stop and applies its decision. True when the
    /// decision ends the call: a `stop` action, or no confident rung (the
    /// checkpoint then goes back to the outer agent).
    fn stop(&mut self, triggers: &[StopReason]) -> bool {
        let reason = triggers[0];
        self.last_stop_at = Some(self.totals.pops);
        self.totals.stops += 1;
        if triggers.contains(&StopReason::LikelyComplete) {
            self.tally.complete_fired = true;
        }
        let checkpoint = self.checkpoint(reason);
        let Some(handler) = self.t.handler.as_ref() else {
            return false;
        };
        let checkpoint_hash = trace::checkpoint_hash(&checkpoint);
        self.events.push(json!({
            "ev": "stop", "stop": checkpoint.stop, "pops": checkpoint.pops,
            "reason": reason.as_str(),
            "triggers": triggers.iter().map(|t| t.as_str()).collect::<Vec<_>>(),
            "checkpoint_hash": checkpoint_hash,
        }));
        let decision = handler.decide(&checkpoint, &StopBudget::default());
        // A handler at a stop is never the agent: rung 3 is only the steer.
        let rung = if decision.rung == 0 {
            handler.rung()
        } else {
            decision.rung
        }
        .clamp(RUNG_RULES, RUNG_SYSTEM_ONE);
        let handler_id = if decision.handler.is_empty() {
            handler.id().to_string()
        } else {
            decision.handler.clone()
        };
        for attempt in &decision.trail {
            self.events.push(steer_event(
                checkpoint.stop,
                attempt.rung,
                &attempt.handler,
                Some(&checkpoint_hash),
                attempt.confidence,
                attempt.truncated,
                &attempt.actions,
                0,
                "uncertain",
            ));
        }
        if decision.escalate {
            self.events.push(steer_event(
                checkpoint.stop,
                RUNG_AGENT,
                &handler_id,
                Some(&checkpoint_hash),
                0.0,
                false,
                &[],
                0,
                "returned",
            ));
            self.pause = Some((Pause::Escalated, reason));
            return true;
        }
        let (applied, stop) = self.apply(&decision, rung);
        self.totals.steers += applied;
        self.events.push(steer_event(
            checkpoint.stop,
            rung,
            &handler_id,
            Some(&checkpoint_hash),
            decision.confidence,
            decision.truncated,
            &decision.actions,
            applied,
            "applied",
        ));
        if stop {
            self.pause = Some((Pause::HandlerStop, reason));
        }
        stop
    }

    /// Applies the outer agent's steer (rung 3) before the first pop. True
    /// when it stops the walk.
    fn steer(&mut self, steer: Steer) -> bool {
        let decision = Decision {
            actions: steer.actions,
            confidence: 1.0,
            ..Decision::default()
        };
        let (applied, stop) = self.apply(&decision, RUNG_AGENT);
        self.totals.steers += applied;
        self.events.push(steer_event(
            self.totals.stops,
            RUNG_AGENT,
            "agent",
            self.returned.as_deref(),
            1.0,
            false,
            &decision.actions,
            applied,
            "applied",
        ));
        if stop {
            self.pause = Some((Pause::Steer, StopReason::Steer));
        }
        stop
    }

    /// Adds to the caps, within the fixed limits. True when anything grew.
    fn raise(&mut self, pops: u32, tokens: u64) -> bool {
        let before = (self.tally.raised_pops, self.tally.raised_tokens);
        self.tally.raised_pops = self
            .tally
            .raised_pops
            .saturating_add(pops)
            .min(RAISE_MAX_POPS);
        self.tally.raised_tokens = self
            .tally
            .raised_tokens
            .saturating_add(tokens)
            .min(RAISE_MAX_TOKENS);
        before != (self.tally.raised_pops, self.tally.raised_tokens)
    }

    /// Switches a map for this run; a switch that took effect is kept for a
    /// handle to replay.
    fn switch(&mut self, id: &str, on: bool) -> bool {
        let ok = self.t.source.set_enabled(id, on);
        if ok {
            self.switches.push((id.to_string(), on));
        }
        ok
    }

    /// Applies the directives of a decision made at `rung` under the
    /// engine's own limits. Returns how many took effect and whether a
    /// `Stop` was among them.
    fn apply(&mut self, decision: &Decision, rung: u8) -> (u32, bool) {
        let mut applied = 0;
        let mut stop = false;
        for directive in &decision.actions {
            let ok = match (&directive.node, &directive.action) {
                (Some(node), action) => self.apply_to_node(node, action),
                (None, Action::MapOn(id)) => self.switch(id, true),
                (None, Action::MapOff(id)) => self.switch(id, false),
                (None, Action::MapWeight(id, weight)) if weight.is_finite() => {
                    self.weights.insert(id.clone(), weight.clamp(0.0, 1.0));
                    true
                }
                (None, Action::Redirect(nodes)) => {
                    self.redirect(nodes);
                    true
                }
                (None, Action::Stop) => {
                    stop = true;
                    true
                }
                // Only the outer agent's steer can add to a cap.
                (None, Action::Budget { pops, tokens }) if rung == RUNG_AGENT => {
                    self.raise(*pops, *tokens)
                }
                (None, Action::Emit(note)) => {
                    let note: String = note.chars().take(handler::EMIT_MAX).collect();
                    self.events.push(json!({"ev": "emit", "note": note}));
                    true
                }
                _ => false,
            };
            applied += u32::from(ok);
        }
        (applied, stop)
    }

    fn apply_to_node(&mut self, node: &NodeId, action: &Action) -> bool {
        let Some(&state) = self.state_of.get(node) else {
            return false;
        };
        if !self.frontier.contains(state) {
            return false;
        }
        match action {
            Action::Continue => true,
            Action::Prune => {
                self.frontier.remove(state);
                self.cleared.insert(node.clone());
                true
            }
            Action::Expand => self.frontier.set_priority(state, f64::MAX),
            Action::Boost(factor) if factor.is_finite() => {
                self.frontier.scale(state, factor.clamp(0.0, BOOST_MAX))
            }
            _ => false,
        }
    }

    /// Replaces the frontier with `nodes` (at most [`REDIRECT_MAX`]):
    /// a node that was queued keeps its path, a new one starts a path; an
    /// expanded or cleared node is skipped. A node named twice counts once,
    /// and one at the depth limit stays out of the queue (it is deferred).
    fn redirect(&mut self, nodes: &[NodeId]) {
        self.frontier.clear();
        let mut seen = HashSet::new();
        let nodes: Vec<&NodeId> = nodes
            .iter()
            .filter(|node| seen.insert(*node))
            .take(REDIRECT_MAX)
            .collect();
        let fanout = nodes.len() as u32;
        for node in nodes {
            if self.cleared.contains(node) {
                continue;
            }
            match self.state_of.get(node).copied() {
                Some(state) if self.expanded.contains(&state) => {}
                Some(state) if self.states[state].depth >= self.t.budget.max_depth => {
                    self.deferred.insert(state);
                }
                Some(state) => {
                    let potential = self.states[state].potential;
                    self.deferred.remove(&state);
                    self.frontier.push(state, potential);
                }
                None => self.add_seed(&Seed::from(node.clone()), fanout),
            }
        }
    }
}

/// Whether a node only connects evidence and is not an answer in itself: a
/// file (`structure.contains`), a commit (`git.history`) or a config key
/// (`config.key`). Popping one is free under pass-through.
pub fn is_connective(node: &NodeId) -> bool {
    matches!(
        node.kind(),
        NodeKind::File | NodeKind::Commit | NodeKind::ConfigKey
    )
}

/// The `rank` object of the `start` and `resume` events. Under `boost` fusion
/// it is what it always was; `rrf` adds the fusion and its `k`.
pub(crate) fn rank_json(boost: f64, fusion: Fusion) -> Value {
    match fusion {
        Fusion::Boost => json!({"corroboration_boost": boost}),
        Fusion::Rrf => json!({
            "corroboration_boost": boost, "fusion": fusion.as_str(), "rrf_k": RRF_K,
        }),
    }
}

/// The stop policy as the trace records it.
pub(crate) fn stops_json(policy: &StopPolicy) -> Value {
    json!({
        "every_pops": policy.every_pops,
        "max_stops": policy.max_stops,
        "budget_share": policy.budget_share,
        "on_disagreement": policy.on_disagreement,
        "on_stagnation": policy.on_stagnation,
        "likely_complete": policy.likely_complete,
    })
}

/// One `steer` event: what a rung proposed at a stop and what became of it.
#[allow(clippy::too_many_arguments)]
fn steer_event(
    stop: u32,
    rung: u8,
    handler: &str,
    inputs_hash: Option<&str>,
    confidence: f32,
    truncated: bool,
    actions: &[Directive],
    applied: u32,
    outcome: &str,
) -> Value {
    json!({
        "ev": "steer", "stop": stop, "rung": rung, "handler": handler,
        "inputs_hash": inputs_hash,
        "confidence": confidence, "truncated": truncated,
        "actions": actions.iter().map(|d| json!({
            "node": d.node.as_ref().map(ToString::to_string),
            "action": d.action.name(),
        })).collect::<Vec<_>>(),
        "applied": applied,
        "outcome": outcome,
    })
}
