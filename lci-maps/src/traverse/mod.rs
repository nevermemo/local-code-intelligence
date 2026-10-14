//! The traversal engine of the multimap spine (slice S2; spine brief 1.3 to
//! 1.5): best-first search over path states, where one pop expands one state
//! through an [`EdgeSource`].
//!
//! A state's priority is the product of the hop values along its path (the
//! `walk.rs` rule); ties pop in discovery order. A node becomes a state once:
//! the first edge that reaches it decides its path. Hop values come from
//! [`frontier::hop`]; the rules scorer prices cross-map hops through the map
//! card.
//!
//! Budget (all checked, none exceeded): `pops` before every pop, `deadline`
//! before every pop and inside every map call, `est_tokens` and
//! `external_calls` after every expansion. An expansion that would pass the
//! token or call budget is not adopted (a token budget that fits only some of
//! its edges keeps the best of them), and the walk ends `BudgetExhausted`.
//! Totals count adopted work only. A token is charged per adopted edge
//! ([`TOKENS_PER_EDGE`]) plus the `est_tokens` the maps report.
//!
//! After every `every_pops` pops ([`StopPolicy`]) a [`Handler`] sees a
//! [`Checkpoint`] and its directives are applied. Nothing a handler returns
//! can raise a budget; cleared (pruned) nodes are never revisited.
//!
//! The run is recorded as JSON events ([`TraversalResult::events`]); the
//! trace writer and handles are the next slice.

pub mod frontier;
pub mod handler;
pub mod rules;
pub mod source;

#[cfg(test)]
mod tests;

use crate::{Budget, MapId, edge::MapCard, edge::WitnessClass};
use frontier::{Candidate, Frontier, HopPrice, HopScorer, RulesScorer, ScoreCtx, hop};
use handler::{
    Action, BOOST_MAX, Checkpoint, Decision, FrontierItem, Handler, REDIRECT_MAX, RerankBy,
    StopBudget, StopPolicy,
};
use lci_core::node::NodeId;
use serde_json::{Value, json};
use source::{EdgeSource, Expansion, FusedEdge};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    time::Instant,
};

/// Tokens charged per adopted edge: roughly one ~12-token card line (a guess).
pub const TOKENS_PER_EDGE: u64 = 12;
/// The most frontier items a checkpoint carries.
pub const CHECKPOINT_FRONTIER: usize = 256;

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
}

impl TraversalResult {
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
        }
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
    /// Edges into each node seen so far.
    in_seen: HashMap<NodeId, u32>,
    expanded: HashSet<usize>,
    popped: Vec<usize>,
    cleared: BTreeSet<NodeId>,
    weights: HashMap<String, f64>,
    cards: HashMap<MapId, MapCard>,
    totals: Totals,
    events: Vec<Value>,
    last_stop_at: Option<u32>,
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
            expanded: HashSet::new(),
            popped: Vec::new(),
            cleared: BTreeSet::new(),
            weights: HashMap::new(),
            cards: HashMap::new(),
            totals: Totals::default(),
            events: Vec::new(),
            last_stop_at: None,
        }
    }

    fn remaining_pops_pct(&self) -> u32 {
        let total = u64::from(self.t.budget.pops.max(1));
        let left = total.saturating_sub(u64::from(self.totals.pops));
        (left * 100 / total) as u32
    }

    fn tokens_left(&self) -> u64 {
        self.t.budget.est_tokens.saturating_sub(self.totals.tokens)
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
        let index = self.states.len();
        self.states.push(State {
            node: seed.node.clone(),
            parent: None,
            depth: 0,
            via: seed.map,
            witness: None,
            potential: 1.0,
            fanout,
        });
        self.state_of.insert(seed.node.clone(), index);
        if self.t.budget.max_depth > 0 {
            self.frontier.push(index, 1.0);
        }
    }

    async fn run(mut self) -> TraversalResult {
        let started = Instant::now();
        self.events.push(json!({
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
            "stops": {
                "every_pops": self.t.stops.every_pops,
                "max_stops": self.t.stops.max_stops,
            },
        }));
        let seeds = std::mem::take(&mut self.t.seeds);
        let fanout = seeds.len() as u32;
        for seed in &seeds {
            self.add_seed(seed, fanout);
        }

        let status = loop {
            if !self.t.targets.is_empty()
                && self.t.targets.iter().all(|t| self.state_of.contains_key(t))
            {
                break TraversalStatus::Complete;
            }
            if self.frontier.is_empty() {
                break self.end_status(false);
            }
            if self.stop_due() && self.stop() {
                break TraversalStatus::Stopped;
            }
            if self.frontier.is_empty() {
                continue;
            }
            if self.totals.pops >= self.t.budget.pops
                || Instant::now() >= self.t.budget.deadline
                || self.tokens_left() == 0
            {
                break self.end_status(true);
            }
            let item = self.frontier.pop().expect("a non-empty frontier pops");
            self.totals.pops += 1;
            self.popped.push(item.state);
            self.expanded.insert(item.state);
            if self.expand(item.state).await {
                break self.end_status(true);
            }
        };

        self.totals.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
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
        TraversalResult {
            status,
            states: self.states,
            popped: self.popped,
            cleared: self.cleared,
            totals: self.totals,
            events: self.events,
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

    /// Expands one state. True when the budget stopped the expansion.
    async fn expand(&mut self, index: usize) -> bool {
        let node = self.states[index].node.clone();
        let depth = self.states[index].depth;
        let budget = Budget {
            deadline: self.t.budget.deadline,
            max_external_calls: self.calls_left(),
            max_nodes_touched: u64::MAX,
        };
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
            return true;
        }

        // Candidates: never a cleared node or one that already has a state.
        let offered = edges
            .into_iter()
            .filter(|e| !self.cleared.contains(&e.best.dst))
            .collect::<Vec<_>>();
        let fanout = offered.len() as u32;
        for edge in &offered {
            *self.in_seen.entry(edge.best.dst.clone()).or_default() += 1;
        }
        let fresh: Vec<FusedEdge> = offered
            .into_iter()
            .filter(|e| !self.state_of.contains_key(&e.best.dst))
            .collect();
        let affordable = ((self.tokens_left() - call_tokens) / TOKENS_PER_EDGE) as usize;
        let truncated = fresh.len() > affordable;
        let kept: Vec<FusedEdge> = fresh.into_iter().take(affordable).collect();

        self.totals.tokens += call_tokens + kept.len() as u64 * TOKENS_PER_EDGE;
        self.totals.external_calls = self.totals.external_calls.saturating_add(call_external);
        for call in &calls {
            *self.totals.per_map.entry(call.map).or_default() += 1;
        }
        self.events.push(json!({
            "ev": "expand", "pop": self.totals.pops, "node": node.to_string(),
            "n_edges": kept.len(), "calls": call_report,
            "tokens": call_tokens + kept.len() as u64 * TOKENS_PER_EDGE,
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
            let child = self.states.len();
            let cross = parent.via.is_some_and(|via| via != map);
            self.states.push(State {
                node: dst.clone(),
                parent: Some(index),
                depth: depth + 1,
                via: Some(map),
                witness: Some(edge.best.witness),
                potential,
                fanout,
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
        truncated || timed_out
    }

    fn stop_due(&self) -> bool {
        let policy = self.t.stops;
        self.t.handler.is_some()
            && policy.every_pops > 0
            && self.totals.stops < policy.max_stops
            && self.totals.pops > 0
            && self.totals.pops.is_multiple_of(policy.every_pops)
            && self.last_stop_at != Some(self.totals.pops)
    }

    fn checkpoint(&self) -> Checkpoint {
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
                    depth: state.depth,
                    fanout: state.fanout,
                    seen_before: self.in_seen.get(&state.node).copied().unwrap_or(0) > 1,
                }
            })
            .collect();
        Checkpoint {
            pops: self.totals.pops,
            stop: self.totals.stops,
            budget_left_pct: self.remaining_pops_pct(),
            tokens_left: self.tokens_left(),
            frontier,
            visited: self.expanded.len(),
            cleared: self.cleared.len(),
        }
    }

    /// Runs the handler at a stop and applies its decision. True when the
    /// decision stops the walk.
    fn stop(&mut self) -> bool {
        self.last_stop_at = Some(self.totals.pops);
        self.totals.stops += 1;
        let checkpoint = self.checkpoint();
        let Some(handler) = self.t.handler.as_ref() else {
            return false;
        };
        let decision = handler.decide(&checkpoint, &StopBudget::default());
        let handler_id = handler.id().to_string();
        let (applied, stop) = self.apply(&decision);
        self.totals.steers += applied;
        self.events.push(json!({
            "ev": "steer", "stop": checkpoint.stop, "handler": handler_id,
            "confidence": decision.confidence, "truncated": decision.truncated,
            "actions": decision.actions.iter().map(|d| json!({
                "node": d.node.as_ref().map(ToString::to_string),
                "action": d.action.name(),
            })).collect::<Vec<_>>(),
            "applied": applied,
        }));
        stop
    }

    /// Applies the directives under the engine's own limits. Returns how
    /// many took effect and whether a `Stop` was among them.
    fn apply(&mut self, decision: &Decision) -> (u32, bool) {
        let mut applied = 0;
        let mut stop = false;
        for directive in &decision.actions {
            let ok = match (&directive.node, &directive.action) {
                (Some(node), action) => self.apply_to_node(node, action),
                (None, Action::MapOn(id)) => self.t.source.set_enabled(id, true),
                (None, Action::MapOff(id)) => self.t.source.set_enabled(id, false),
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
