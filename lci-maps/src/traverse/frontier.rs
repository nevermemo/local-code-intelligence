//! The frontier and the hop pricing of the traversal.
//!
//! [`Frontier`] pops the highest priority first and breaks ties by discovery
//! order, as `walk.rs` does. A state's priority starts as its path
//! potential, the product of the hop values along the path; a handler may
//! then scale it or reorder the queue (the values in the queue stay the same
//! set).
//!
//! [`hop`] is `walk::hop` over fused edges: a node with at most `gate`
//! offered edges is followed whole at value 1; above the gate a [`HopScorer`]
//! orders them, the best `beam` are kept, and the edge at rank `r` gets
//! `1 / (1 + r)`. The rules scorer sees the edge's witness class, kind, the
//! fused confidence, the in-degree seen so far, the query overlap and the map
//! card's cost, so a hop into another map is priced like any other.

use crate::{
    EdgeKind,
    edge::{MapCard, WitnessClass},
    traverse::source::FusedEdge,
    walk::{SplitMix, W_BASE, W_INDEGREE_IDF, W_OVERLAP, tokenize},
};
use std::{cmp::Ordering, collections::HashSet};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Item {
    pub state: usize,
    pub priority: f64,
    seq: u64,
}

impl Item {
    /// Best first: higher priority, then earlier discovery.
    fn order(&self, other: &Self) -> Ordering {
        other
            .priority
            .total_cmp(&self.priority)
            .then(self.seq.cmp(&other.seq))
    }
}

#[derive(Debug, Default)]
pub struct Frontier {
    items: Vec<Item>,
    next_seq: u64,
}

impl Frontier {
    pub fn push(&mut self, state: usize, priority: f64) {
        self.items.push(Item {
            state,
            priority,
            seq: self.next_seq,
        });
        self.next_seq += 1;
    }

    pub fn pop(&mut self) -> Option<Item> {
        let best = (0..self.items.len()).min_by(|&a, &b| self.items[a].order(&self.items[b]))?;
        Some(self.items.swap_remove(best))
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// The items best first.
    pub fn sorted(&self) -> Vec<Item> {
        let mut items = self.items.clone();
        items.sort_by(Item::order);
        items
    }

    pub fn remove(&mut self, state: usize) -> bool {
        match self.items.iter().position(|item| item.state == state) {
            Some(at) => {
                self.items.swap_remove(at);
                true
            }
            None => false,
        }
    }

    pub fn contains(&self, state: usize) -> bool {
        self.items.iter().any(|item| item.state == state)
    }

    /// Sets the priority of a queued state; false when it is not queued.
    pub fn set_priority(&mut self, state: usize, priority: f64) -> bool {
        match self.items.iter_mut().find(|item| item.state == state) {
            Some(item) => {
                item.priority = priority;
                true
            }
            None => false,
        }
    }

    pub fn scale(&mut self, state: usize, factor: f64) -> bool {
        match self.items.iter().find(|item| item.state == state) {
            Some(item) => self.set_priority(state, (item.priority * factor).min(f64::MAX)),
            None => false,
        }
    }

    /// Hands the existing queue positions (priority and discovery number)
    /// out again in the order of `key` (smaller first; equal keys keep the
    /// current order). The set of positions does not change.
    pub fn rerank<K: Ord>(&mut self, key: impl Fn(usize) -> K) {
        let current = self.sorted();
        let slots: Vec<(f64, u64)> = current
            .iter()
            .map(|item| (item.priority, item.seq))
            .collect();
        let mut order: Vec<usize> = (0..current.len()).collect();
        order.sort_by_key(|&i| key(current[i].state));
        for (rank, &i) in order.iter().enumerate() {
            if let Some(item) = self
                .items
                .iter_mut()
                .find(|it| it.state == current[i].state)
            {
                (item.priority, item.seq) = slots[rank];
            }
        }
    }
}

/// How many edges are followed whole, and how many are kept above that.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HopPrice {
    pub gate: usize,
    pub beam: usize,
}

impl Default for HopPrice {
    fn default() -> Self {
        Self { gate: 8, beam: 16 }
    }
}

/// One offered edge as the scorer sees it.
#[derive(Debug, Clone)]
pub struct Candidate<'a> {
    pub edge: &'a FusedEdge,
    /// How many edges into the destination have been seen so far.
    pub in_seen: u32,
    /// Card of the map that offered the best edge.
    pub card: MapCard,
}

pub struct ScoreCtx<'a> {
    /// Question tokens ([`crate::walk::question_tokens`]).
    pub query: &'a [String],
    /// States discovered so far.
    pub nodes_seen: usize,
}

/// An ordering arm. Scores are higher-is-better; ties keep offer order.
pub trait HopScorer: Send {
    fn label(&self) -> &'static str;

    /// True for an arm that follows every edge in offer order and ignores
    /// the gate and the beam.
    fn unscored(&self) -> bool {
        false
    }

    fn score(&mut self, ctx: &ScoreCtx, cands: &[Candidate]) -> Vec<f64>;
}

/// Prior of a witness class (a guess, not measured; text is low by M1.2).
pub fn witness_prior(witness: WitnessClass) -> f64 {
    match witness {
        WitnessClass::Compiler => 1.0,
        WitnessClass::Lsp => 0.9,
        WitnessClass::Exact => 0.9,
        WitnessClass::Text => 0.3,
        WitnessClass::Vector => 0.3,
        WitnessClass::Heuristic => 0.4,
        WitnessClass::Agent => 0.5,
    }
}

/// Prior of an edge kind (a guess; calls and contains are `walk::kind_prior`).
pub fn kind_prior(kind: EdgeKind) -> f64 {
    match kind {
        EdgeKind::CallsOrRefs | EdgeKind::CompileErrorAt => 1.0,
        EdgeKind::Implements => 0.9,
        EdgeKind::Imports | EdgeKind::ReadsKey => 0.8,
        EdgeKind::Tests => 0.7,
        EdgeKind::Contains | EdgeKind::QueryMatch | EdgeKind::Touched | EdgeKind::Cochange => 0.5,
        EdgeKind::Mentions => 0.4,
        EdgeKind::Near | EdgeKind::Prior => 0.3,
    }
}

/// An expand that takes this long halves the price of the hop (a guess).
pub const COST_REF_MS: f64 = 100.0;

/// Share of the query tokens that the node's id tokens contain; 0 to 1.
fn overlap(query: &[String], candidate: &Candidate) -> f64 {
    if query.is_empty() {
        return 0.0;
    }
    let tokens: HashSet<String> = tokenize(&candidate.edge.best.dst.to_string())
        .into_iter()
        .collect();
    query.iter().filter(|t| tokens.contains(*t)).count() as f64 / query.len() as f64
}

/// `witness * kind * conf * cost * (W_BASE + W_INDEGREE_IDF * idf + W_OVERLAP * overlap)`
/// with the weights of `walk.rs`; the idf is over the in-degree seen so far.
pub struct RulesScorer;

impl HopScorer for RulesScorer {
    fn label(&self) -> &'static str {
        "rules"
    }

    fn score(&mut self, ctx: &ScoreCtx, cands: &[Candidate]) -> Vec<f64> {
        let n = ctx.nodes_seen as f64 + 1.0;
        cands
            .iter()
            .map(|c| {
                let idf = ((n / (f64::from(c.in_seen) + 1.0)).ln() / n.ln().max(f64::MIN_POSITIVE))
                    .clamp(0.0, 1.0);
                let cost = 1.0 / (1.0 + c.card.expand_p50_ms.unwrap_or(0.0).max(0.0) / COST_REF_MS);
                witness_prior(c.edge.best.witness)
                    * kind_prior(c.edge.best.kind)
                    * (f64::from(c.edge.conf) / 100.0)
                    * cost
                    * (W_BASE + W_INDEGREE_IDF * idf + W_OVERLAP * overlap(ctx.query, c))
            })
            .collect()
    }
}

/// Control: a seeded random order.
pub struct RandomScorer {
    rng: SplitMix,
}

impl RandomScorer {
    pub fn new(seed: u64) -> Self {
        Self {
            rng: SplitMix(seed),
        }
    }
}

impl HopScorer for RandomScorer {
    fn label(&self) -> &'static str {
        "random"
    }

    fn score(&mut self, _ctx: &ScoreCtx, cands: &[Candidate]) -> Vec<f64> {
        cands.iter().map(|_| self.rng.next_f64()).collect()
    }
}

/// Control: unscored, edges in offer order.
pub struct BfsScorer;

impl HopScorer for BfsScorer {
    fn label(&self) -> &'static str {
        "bfs"
    }

    fn unscored(&self) -> bool {
        true
    }

    fn score(&mut self, _ctx: &ScoreCtx, cands: &[Candidate]) -> Vec<f64> {
        cands.iter().map(|_| 0.0).collect()
    }
}

/// One step: `(index into cands, hop value)` for the edges to follow.
pub fn hop(
    scorer: &mut dyn HopScorer,
    ctx: &ScoreCtx,
    cands: &[Candidate],
    price: HopPrice,
) -> Vec<(usize, f64)> {
    if scorer.unscored() || cands.len() <= price.gate {
        return (0..cands.len()).map(|i| (i, 1.0)).collect();
    }
    let scores = scorer.score(ctx, cands);
    let mut order: Vec<usize> = (0..cands.len()).collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    order
        .into_iter()
        .take(price.beam)
        .enumerate()
        .map(|(rank, i)| (i, 1.0 / (1.0 + rank as f64)))
        .collect()
}
