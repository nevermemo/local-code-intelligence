//! The frontier and the hop pricing of the traversal.
//!
//! [`Frontier`] pops the highest priority first and breaks ties by discovery
//! order, as `walk.rs` does. A state's priority starts as its path
//! potential, the product of the hop values along the path; a handler may
//! then scale it or reorder the queue (the values in the queue stay the same
//! set).
//!
//! [`hop`] is `walk::hop` over fused edges: a node with at most `gate`
//! offered edges is followed whole, each at its kind's [`kind_prior`] (1 for a
//! call, 0.5 for a containment hop, so structure does not tie with the call
//! graph); above the gate a [`HopScorer`] orders them, the best `beam` are
//! kept, and the edge at rank `r` gets `1 / (1 + r)` times its kind's
//! [`kind_prior`], so a rank-0 containment edge is worth 0.5 above the gate as
//! it is below it. An unscored arm follows every edge at 1. The rules scorer sees the edge's witness class, kind, the
//! fused confidence, the in-degree seen so far, the query overlap and the map
//! card's cost, so a hop into another map is priced like any other.

use crate::{
    EdgeKind,
    edge::{MapCard, WitnessClass},
    traverse::source::FusedEdge,
    walk::{SplitMix, W_BASE, W_INDEGREE_IDF, W_OVERLAP, tokenize},
};
use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet},
};

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

    /// The same queue entry: priorities compare by bits, so a NaN is itself.
    fn same(&self, other: &Self) -> bool {
        self.state == other.state
            && self.seq == other.seq
            && self.priority.to_bits() == other.priority.to_bits()
    }
}

/// A heap entry. [`BinaryHeap`] pops its greatest entry, so the best item
/// compares greatest.
#[derive(Debug, Clone, Copy)]
struct Entry(Item);

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for Entry {}

impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Entry {
    fn cmp(&self, other: &Self) -> Ordering {
        other.0.order(&self.0)
    }
}

/// A binary heap with lazy deletion: `push`, `pop` and the priority changes
/// are `O(log n)`. `live` holds the queued items by state (a state is queued
/// at most once; pushing a queued state does nothing); the heap may also hold
/// entries that are out of date, left by a priority change or a removal, and
/// `pop` skips them by checking them against `live`.
#[derive(Debug, Default)]
pub struct Frontier {
    live: HashMap<usize, Item>,
    heap: BinaryHeap<Entry>,
    next_seq: u64,
}

impl Frontier {
    pub fn push(&mut self, state: usize, priority: f64) {
        if self.live.contains_key(&state) {
            return;
        }
        let item = Item {
            state,
            priority,
            seq: self.next_seq,
        };
        self.next_seq += 1;
        self.live.insert(state, item);
        self.heap.push(Entry(item));
    }

    /// Puts a popped item back at its old place (same priority and
    /// discovery number).
    pub fn requeue(&mut self, item: Item) {
        self.live.insert(item.state, item);
        self.heap.push(Entry(item));
    }

    pub fn pop(&mut self) -> Option<Item> {
        while let Some(Entry(item)) = self.heap.pop() {
            if self.live.get(&item.state).is_some_and(|l| l.same(&item)) {
                self.live.remove(&item.state);
                return Some(item);
            }
        }
        None
    }

    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    /// The queued states, in no particular order.
    pub fn states(&self) -> impl Iterator<Item = usize> + '_ {
        self.live.keys().copied()
    }

    /// The queue as `(state, priority, discovery number)` in discovery
    /// order, and the next discovery number: all a handle needs to rebuild it.
    pub fn parts(&self) -> (Vec<(usize, f64, u64)>, u64) {
        let mut items: Vec<_> = self
            .live
            .values()
            .map(|item| (item.state, item.priority, item.seq))
            .collect();
        items.sort_by_key(|&(_, _, seq)| seq);
        (items, self.next_seq)
    }

    /// The inverse of [`Frontier::parts`].
    pub fn from_parts(items: &[(usize, f64, u64)], next_seq: u64) -> Self {
        let mut frontier = Self {
            next_seq,
            ..Self::default()
        };
        for &(state, priority, seq) in items {
            frontier.live.entry(state).or_insert(Item {
                state,
                priority,
                seq,
            });
        }
        frontier.rebuild();
        frontier
    }

    /// A heap of exactly the live items.
    fn rebuild(&mut self) {
        self.heap = self.live.values().map(|item| Entry(*item)).collect();
    }

    /// Puts a changed item in the heap; the old entry is now out of date.
    fn reinsert(&mut self, item: Item) {
        self.heap.push(Entry(item));
        // Many changes would leave the heap mostly dead entries.
        if self.heap.len() > 4 * self.live.len() + 64 {
            self.rebuild();
        }
    }

    pub fn clear(&mut self) {
        self.live.clear();
        self.heap.clear();
    }

    /// The items best first.
    pub fn sorted(&self) -> Vec<Item> {
        let mut items: Vec<Item> = self.live.values().copied().collect();
        items.sort_by(Item::order);
        items
    }

    pub fn remove(&mut self, state: usize) -> bool {
        self.live.remove(&state).is_some()
    }

    pub fn contains(&self, state: usize) -> bool {
        self.live.contains_key(&state)
    }

    /// Sets the priority of a queued state; false when it is not queued.
    pub fn set_priority(&mut self, state: usize, priority: f64) -> bool {
        match self.live.get_mut(&state) {
            Some(item) => {
                item.priority = priority;
                let item = *item;
                self.reinsert(item);
                true
            }
            None => false,
        }
    }

    /// The priority of a queued state.
    pub fn priority_of(&self, state: usize) -> Option<f64> {
        self.live.get(&state).map(|item| item.priority)
    }

    pub fn scale(&mut self, state: usize, factor: f64) -> bool {
        match self.priority_of(state) {
            Some(priority) => self.set_priority(state, (priority * factor).min(f64::MAX)),
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
            if let Some(item) = self.live.get_mut(&current[i].state) {
                (item.priority, item.seq) = slots[rank];
            }
        }
        self.rebuild();
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
        EdgeKind::Contains
        | EdgeKind::DefinedIn
        | EdgeKind::QueryMatch
        | EdgeKind::Touched
        | EdgeKind::Cochange => 0.5,
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

/// One step: `(index into cands, hop value)` for the edges to follow. The
/// value is the kind prior at or below the gate and `kind_prior / (1 + rank)`
/// above it; the unscored arm gives every edge 1.
pub fn hop(
    scorer: &mut dyn HopScorer,
    ctx: &ScoreCtx,
    cands: &[Candidate],
    price: HopPrice,
) -> Vec<(usize, f64)> {
    if scorer.unscored() {
        return (0..cands.len()).map(|i| (i, 1.0)).collect();
    }
    if cands.len() <= price.gate {
        return cands
            .iter()
            .enumerate()
            .map(|(i, c)| (i, kind_prior(c.edge.best.kind)))
            .collect();
    }
    let scores = scorer.score(ctx, cands);
    let mut order: Vec<usize> = (0..cands.len()).collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    order
        .into_iter()
        .take(price.beam)
        .enumerate()
        .map(|(rank, i)| (i, kind_prior(cands[i].edge.best.kind) / (1.0 + rank as f64)))
        .collect()
}
