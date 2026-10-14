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
//! - A node that is queued (not yet popped) and reached by a route with a
//!   higher potential is relaxed: its potential, parent, `via`, witness,
//!   depth and fan-out become those of the stronger route, its queue priority
//!   is scaled by the same ratio (a handler's boost stays), and a `relax`
//!   event is traced. A queued node is only relaxed to a depth below
//!   `max_depth`; a node that was never queued for its depth is queued when
//!   a route brings it below `max_depth`.
//! - A node already popped keeps its state; the route is traced as an
//!   `alt_path` event. An ancestor of the expanding node (a cycle) is neither.
//! - Corroboration: a [`State`] keeps the set of maps (and witness classes)
//!   whose edges reached it. An arrival from a map not in the set adds it. The
//!   maps that found the seeds ([`Seed::map`]) are start points, not evidence,
//!   and are never in the set: only maps that offered an edge count. Results
//!   rank by [`State::ranked_potential`]: the potential times
//!   `1 + boost * (n_maps - 1)` ([`DEFAULT_CORROBORATION_BOOST`]).
//!
//! Budget (all checked, none exceeded): `pops` before every pop, `deadline`
//! before every pop and inside every map call, `est_tokens` and
//! `external_calls` after every expansion. An expansion that would pass the
//! token or call budget is not adopted (a token budget that fits only some of
//! its edges keeps the best of them), and the walk ends `BudgetExhausted`.
//! Totals count adopted work only. A token is charged per adopted edge
//! ([`TOKENS_PER_EDGE`]) plus the `est_tokens` the maps report.
//!
//! A pop that was cut by a budget (nothing adopted, only some edges adopted,
//! or a map that timed out) is put back in the queue at its old place, so a
//! resume expands the node again and finds the rest. A pop that adopted
//! nothing is also taken off the pop count.
//!
//! Budgets across calls: `pops`, `est_tokens` and `external_calls` are
//! absolute caps for the whole traversal (the totals persist in a handle);
//! `deadline` and `max_depth` belong to one call. Only the outer agent can
//! add to the caps, through a [`steer::Steer`], within fixed limits
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
//! The run is recorded as JSON events ([`TraversalResult::events`]), written as
//! JSONL by [`trace`]; a paused run leaves a [`handle::Snapshot`] to resume.

pub mod frontier;
pub mod handle;
pub mod handler;
pub mod ladder;
pub mod merged;
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
    RAISE_MAX_TOKENS, REDIRECT_MAX, RUNG_AGENT, RUNG_RULES, RUNG_SYSTEM_ONE, RerankBy, StopBudget,
    StopPolicy,
};
use lci_core::node::NodeId;
use serde_json::{Value, json};
use source::{EdgeSource, Expansion, FusedEdge};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    time::Instant,
};
use steer::Steer;
use stop::{
    ANSWER_K, CARD_K, CHECKPOINT_DISAGREEMENTS, Card, Disagreement, MAX_DISAGREEMENTS, Pause,
    Regions, StopReason, Tally, is_structural, likely_complete,
};

/// Tokens charged per adopted edge: roughly one ~12-token card line (a guess).
pub const TOKENS_PER_EDGE: u64 = 12;
/// The most frontier items a checkpoint carries.
pub const CHECKPOINT_FRONTIER: usize = 256;
/// The default share added to a node's rank for each map beyond the first
/// that reached it (see [`State::ranked_potential`]).
pub const DEFAULT_CORROBORATION_BOOST: f64 = 0.1;

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
    /// The witness classes of those edges.
    pub witnesses: BTreeSet<WitnessClass>,
}

impl State {
    /// How many independent maps reached the node.
    pub fn n_maps(&self) -> usize {
        self.maps.len()
    }

    /// `potential * (1 + boost * (n_maps - 1))`: the value results are ranked
    /// by. A node no map reached counts as one map.
    pub fn ranked_potential(&self, boost: f64) -> f64 {
        let extra = self.maps.len().saturating_sub(1) as f64;
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
    pub stops: u32,
    /// Directives applied at stops.
    pub steers: u32,
    /// Expansions adopted, per map asked.
    pub per_map: BTreeMap<&'static str, u32>,
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
}

impl TraversalResult {
    /// The rank of a state of this result (see [`State::ranked_potential`]).
    pub fn ranked_potential(&self, state: &State) -> f64 {
        state.ranked_potential(self.corroboration_boost)
    }

    pub fn reached(&self, node: &NodeId) -> bool {
        self.states.iter().any(|s| s.node == *node)
    }

    /// The events as JSON lines.
    pub fn events_jsonl(&self) -> String {
        self.events
            .iter()
            .map(|e| format!("{e}\n"))
            .collect::<String>()
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
        }
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
    /// `alt_path`, so a retried expansion does not repeat them.
    alts: HashSet<(usize, usize)>,
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
            witnesses: BTreeSet::new(),
        });
        self.state_of.insert(seed.node.clone(), index);
        if self.t.budget.max_depth > 0 {
            self.frontier.push(index, 1.0);
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
                "rank": {"corroboration_boost": self.t.boost},
            });
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
            let due = self.due();
            if !due.is_empty() && self.stop(&due) {
                break TraversalStatus::Stopped;
            }
            if self.frontier.is_empty() {
                continue;
            }
            if self.totals.pops >= self.pops_cap()
                || Instant::now() >= self.t.budget.deadline
                || self.tokens_left() == 0
            {
                break self.end_status(true);
            }
            let item = self.frontier.pop().expect("a non-empty frontier pops");
            self.totals.pops += 1;
            self.popped.push(item.state);
            self.expanded.insert(item.state);
            if let Some(adopted) = self.expand(item.state).await {
                // The node is not done: back in the queue at its old place,
                // for a resume to expand again.
                self.expanded.remove(&item.state);
                self.uncount();
                self.frontier.requeue(item);
                if !adopted {
                    self.totals.pops -= 1;
                    self.popped.pop();
                }
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
        if resumable {
            let (pause, reason) = self
                .pause
                .take()
                .unwrap_or((Pause::Budget, StopReason::Budget));
            let cp = self.checkpoint(reason);
            let hash = trace::checkpoint_hash(&cp);
            self.events.push(json!({
                "ev": "return", "reason": pause.as_str(), "stop": cp.stop,
                "checkpoint_hash": hash,
            }));
            self.returned = Some(hash);
            checkpoint = Some(cp);
        }
        self.events.push(json!({
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
                "per_map": self.totals.per_map,
            },
        }));
        let snapshot = resumable.then(|| self.snapshot());
        if let (Some(cp), Some(snapshot)) = (checkpoint.as_mut(), &snapshot) {
            cp.handle = Some(snapshot.handle());
        }
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
        }
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

    fn card(&mut self, map: MapId) -> MapCard {
        let source = &self.t.source;
        self.cards
            .entry(map)
            .or_insert_with(|| source.card(map))
            .clone()
    }

    /// Expands one state. `Some` when a budget (or a timeout) cut the
    /// expansion short: true if some of its work was adopted, false if none.
    async fn expand(&mut self, index: usize) -> Option<bool> {
        let node = self.states[index].node.clone();
        let depth = self.states[index].depth;
        let budget = Budget {
            deadline: self.t.budget.deadline,
            max_external_calls: self.calls_left(),
            max_nodes_touched: u64::MAX,
        };
        self.counted.clear();
        let Expansion { edges, calls } = self.t.source.edges(&node, budget).await;

        let call_tokens: u64 = calls.iter().map(|c| c.cost.est_tokens).sum();
        let call_external: u32 = calls
            .iter()
            .map(|c| c.cost.external_calls)
            .fold(0, u32::saturating_add);
        let call_report: Vec<Value> = calls
            .iter()
            .map(|c| json!({"map": c.map, "n_edges": c.n_edges, "failure": c.failure}))
            .collect();
        if call_tokens > self.tokens_left() || call_external > self.calls_left() {
            self.events.push(json!({
                "ev": "expand", "pop": self.totals.pops, "node": node.to_string(),
                "n_edges": 0, "calls": call_report, "dropped": "budget",
            }));
            return Some(false);
        }

        // Candidates: never a cleared node or one that already has a state.
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
        let mut path = HashSet::new();
        let mut at = Some(index);
        while let Some(i) = at {
            path.insert(i);
            at = self.states[i].parent;
        }
        let affordable = ((self.tokens_left() - call_tokens) / TOKENS_PER_EDGE) as usize;
        let n_fresh = offered
            .iter()
            .filter(|e| !self.state_of.contains_key(&e.best.dst))
            .count();
        let truncated = n_fresh > affordable;
        // The edges to price, in offer order: the fresh ones the tokens
        // afford (`None`) and the arrivals (the state reached).
        let mut kept: Vec<FusedEdge> = Vec::new();
        let mut target: Vec<Option<usize>> = Vec::new();
        let mut fresh_taken = 0;
        for edge in offered {
            match self.state_of.get(&edge.best.dst).copied() {
                Some(state) if path.contains(&state) => {}
                Some(state) => {
                    kept.push(edge);
                    target.push(Some(state));
                }
                None if fresh_taken < affordable => {
                    fresh_taken += 1;
                    kept.push(edge);
                    target.push(None);
                }
                None => {}
            }
        }

        self.totals.tokens += call_tokens + fresh_taken as u64 * TOKENS_PER_EDGE;
        self.totals.external_calls = self.totals.external_calls.saturating_add(call_external);
        for call in &calls {
            *self.totals.per_map.entry(call.map).or_default() += 1;
            let stat = self.tally.maps.entry(call.map).or_default();
            stat.calls += 1;
            stat.failures += u32::from(call.failure.is_some());
            stat.tokens += call.cost.est_tokens;
        }
        for (edge, _) in kept.iter().zip(&target).filter(|(_, t)| t.is_none()) {
            self.tally.maps.entry(edge.best.map).or_default().edges += 1;
        }
        let spent = call_tokens + fresh_taken as u64 * TOKENS_PER_EDGE;
        let cost = self
            .tally
            .kind_cost
            .entry(node.kind().prefix().to_string())
            .or_default();
        cost.0 += 1;
        cost.1 += spent;
        if fresh_taken > 0 {
            self.tally.last_fresh_pop = self.totals.pops;
        }
        self.events.push(json!({
            "ev": "expand", "pop": self.totals.pops, "node": node.to_string(),
            "n_edges": fresh_taken, "calls": call_report,
            "tokens": call_tokens + fresh_taken as u64 * TOKENS_PER_EDGE,
        }));

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
        };
        let steps = hop(self.t.scorer.as_mut(), &ctx, &cands, self.t.price);

        let parent = self.states[index].clone();
        for (i, value) in steps {
            let edge = &kept[i];
            let dst = &edge.best.dst;
            let map = edge.best.map;
            let value = value * self.weights.get(map).copied().unwrap_or(1.0);
            let potential = parent.potential * value;
            let cross = parent.via.is_some_and(|via| via != map);
            if let Some(reached) = target[i] {
                self.arrive(index, &parent, reached, edge, potential, fanout, cross);
                continue;
            }
            let child = self.states.len();
            self.states.push(State {
                node: dst.clone(),
                parent: Some(index),
                depth: depth + 1,
                via: Some(map),
                witness: Some(edge.best.witness),
                potential,
                fanout,
                maps: edge.maps.iter().copied().collect(),
                witnesses: edge.witnesses.iter().copied().collect(),
            });
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
            }
        }
        // A call that failed after the deadline left the node half-explored.
        let timed_out =
            calls.iter().any(|c| c.failure.is_some()) && Instant::now() >= self.t.budget.deadline;
        (truncated || timed_out).then_some(true)
    }

    /// A route from the expanding state `from` (`parent`) to a node that
    /// already has a state (`reached`), at `potential`. The maps and witness
    /// classes of the edge join the node's corroboration. A node that is not
    /// popped and is reached at a higher potential is relaxed onto the new
    /// route; any other route is traced as an `alt_path`. The node is never
    /// expanded again.
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
        let old_src = self.states[reached]
            .parent
            .map(|p| self.states[p].node.to_string());
        let state = &mut self.states[reached];
        let new_maps: Vec<MapId> = edge
            .maps
            .iter()
            .copied()
            .filter(|map| state.maps.insert(*map))
            .collect();
        state.witnesses.extend(edge.witnesses.iter().copied());

        let old = state.potential;
        let old_depth = state.depth;
        // A queued node must stay below the depth limit.
        let stronger = !popped && potential > old && (!queued || depth < max_depth);
        let dst = state.node.to_string();
        if !stronger {
            if self.alts.insert((reached, from)) {
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
        self.events.push(json!({
            "ev": "relax", "src": parent.node.to_string(), "dst": dst,
            "kind": edge.best.kind.as_str(), "map": edge.best.map,
            "maps": edge.maps, "witness": edge.best.witness.as_str(),
            "old_potential": old, "potential": potential, "old_src": old_src,
            "depth": depth, "cross_map": cross, "new_maps": new_maps,
        }));
        if queued {
            match self.frontier.priority_of(reached) {
                // A handler's boost or reorder stays: scale by the ratio.
                Some(priority) if priority != old && old > 0.0 => {
                    self.frontier.scale(reached, potential / old);
                }
                _ => {
                    self.frontier.set_priority(reached, potential);
                }
            }
        } else if !popped && old_depth >= max_depth && depth < max_depth {
            // It was too deep to queue; the new route is not.
            self.frontier.push(reached, potential);
        }
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

    fn card_of(&self, state: usize) -> Card {
        let s = &self.states[state];
        Card {
            node: s.node.clone(),
            via: s.via,
            witness: s.witness,
            potential: s.potential,
            depth: s.depth,
            maps: s.maps.iter().copied().collect(),
        }
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

        // Best rank first (potential, boosted by corroboration); discovery
        // order breaks ties.
        let rank = |s: &usize| self.states[*s].ranked_potential(self.t.boost);
        let best_first = |a: &usize, b: &usize| rank(b).total_cmp(&rank(a)).then(a.cmp(b));
        let mut visited: Vec<usize> = self.expanded.iter().copied().collect();
        visited.sort_by(best_first);
        let visited_cards = visited
            .iter()
            .take(CARD_K)
            .map(|&s| self.card_of(s))
            .collect();
        let mut answer: Vec<Card> = self
            .t
            .targets
            .iter()
            .filter_map(|t| self.state_of.get(t))
            .map(|&s| self.card_of(s))
            .collect();
        answer.extend(
            visited
                .iter()
                .filter(|&&s| self.states[s].parent.is_some())
                .map(|&s| self.card_of(s))
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
                (None, Action::Rerank(by)) => {
                    let states = &self.states;
                    match by {
                        RerankBy::Witness => self
                            .frontier
                            .rerank(|s| states[s].witness.map_or(0u32, |w| w as u32 + 1)),
                        RerankBy::Depth => self.frontier.rerank(|s| states[s].depth),
                        RerankBy::Fanout => self.frontier.rerank(|s| states[s].fanout),
                    }
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
    /// expanded or cleared node is skipped.
    fn redirect(&mut self, nodes: &[NodeId]) {
        self.frontier.clear();
        let fanout = nodes.len().min(REDIRECT_MAX) as u32;
        for node in nodes.iter().take(REDIRECT_MAX) {
            if self.cleared.contains(node) {
                continue;
            }
            match self.state_of.get(node).copied() {
                Some(state) if self.expanded.contains(&state) => {}
                Some(state) => {
                    let potential = self.states[state].potential;
                    self.frontier.push(state, potential);
                }
                None => self.add_seed(&Seed::from(node.clone()), fanout),
            }
        }
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
