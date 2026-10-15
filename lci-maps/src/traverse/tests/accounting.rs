//! What a walk charges and when (slice F1): hops are priced and selected
//! before anything is charged, a map call costs once whatever it returns,
//! external calls are reserved before the maps run, a seed's first hop is not
//! cross-map, arrivals outside the beam still corroborate, and the frontier
//! does its work in logarithmic time.

use super::*;
use crate::traverse::frontier::Frontier;
use crate::walk::SplitMix;
use std::time::Instant as Clock;

fn expand_events(result: &TraversalResult) -> Vec<&Value> {
    result
        .events
        .iter()
        .filter(|e| e["ev"] == "expand")
        .collect()
}

async fn resume(
    reg: &Registry<'static>,
    snapshot: handle::Snapshot,
    budget: TraversalBudget,
) -> TraversalResult {
    let view = view();
    Traversal::resumed(OnTheFly::new(reg, &view), snapshot, budget)
        .run()
        .await
}

/// `a` offers `count` files `x00..`, then `zzz` last in offer order.
fn wide_with_zzz_last(count: usize) -> GraphMap {
    let mut map = GraphMap::new("g", &[]);
    map.edges = (0..count)
        .map(|i| ("a".to_string(), format!("x{i:02}")))
        .chain([("a".to_string(), "zzz".to_string())])
        .collect();
    map
}

// ---- R1: price and select first, charge after ---------------------------------

#[tokio::test]
async fn r1_the_beam_picks_before_the_tokens_do() {
    // 41 edges are offered and the tokens afford three hops. The question
    // names `zzz`, the last edge in offer order: it is the beam's first pick,
    // so it is one of the three adopted.
    let reg = registry(vec![wide_with_zzz_last(40)]);
    let mut limits = budget(100);
    limits.est_tokens = 3 * TOKENS_PER_EDGE;
    let result = walk(&reg, vec![seed("a")], limits, |t| t.question("zzz")).await;
    assert!(result.reached(&node("zzz")), "{:?}", hops(&result));
    assert_eq!(result.totals.hops, 3);
    assert_eq!(result.totals.tokens, 3 * TOKENS_PER_EDGE);
}

#[tokio::test]
async fn r1_the_expand_event_reports_adopted_and_offered_apart() {
    let reg = registry(vec![wide_with_zzz_last(40)]);
    let mut limits = budget(100);
    limits.est_tokens = 3 * TOKENS_PER_EDGE;
    let result = walk(&reg, vec![seed("a")], limits, |t| t).await;
    let events = expand_events(&result);
    assert_eq!(events[0]["offered"], 41);
    assert_eq!(events[0]["adopted"], 3);
}

#[tokio::test]
async fn r1_a_beam_that_fits_the_tokens_is_not_truncated() {
    // 41 edges are offered, the beam keeps 16, and 16 hops are affordable:
    // nothing was cut, so the pop is not put back.
    let reg = registry(vec![wide_with_zzz_last(40)]);
    let mut limits = budget(100);
    limits.est_tokens = 16 * TOKENS_PER_EDGE;
    let result = walk(&reg, vec![seed("a")], limits, |t| t).await;
    assert_eq!(result.totals.hops, 16);
    let snapshot = result.snapshot.expect("the walk has work left");
    assert!(
        snapshot.expanded.contains(&0),
        "the seed was finished, not put back: {:?}",
        snapshot.frontier
    );
}

#[tokio::test]
async fn r1_a_truncated_pop_charges_its_call_once() {
    let reg = registry(vec![
        GraphMap::new(
            "g",
            &[("a", "b"), ("a", "c"), ("a", "d"), ("a", "e"), ("a", "h")],
        )
        .cost(10, 0),
    ]);
    let tight = TraversalBudget {
        est_tokens: 30,
        ..budget(10)
    };
    let first = walk(&reg, vec![seed("a")], tight, |t| t).await;
    assert_eq!(first.status, TraversalStatus::BudgetExhausted);
    assert!(first.totals.tokens <= 30);
    let second = resume(&reg, first.snapshot.unwrap(), budget(10)).await;
    assert_eq!(second.status, TraversalStatus::Complete);
    // Six pops (a and its five children) cost one call each, plus five hops,
    // whatever the cut: the cut pop did not pay for its call twice.
    let straight = walk(&reg, vec![seed("a")], budget(10), |t| t).await;
    assert_eq!(straight.totals.tokens, 6 * 10 + 5 * TOKENS_PER_EDGE);
    assert_eq!(second.totals.tokens, straight.totals.tokens);
    assert_eq!(second.totals.hops, straight.totals.hops);
}

// ---- R2: a call costs once, whatever it returns -----------------------------

mod r2 {
    use super::*;
    use crate::search_maps::{ChunkHit, ChunkSearch, VectorChunks};

    /// An index with no stored vector and no chunk near anything: the vector
    /// map embeds once and finds nothing.
    struct Empty;

    #[crate::async_trait]
    impl ChunkSearch for Empty {
        async fn lexical(&self, _: &str, _: usize) -> Result<Vec<ChunkHit>, String> {
            Ok(Vec::new())
        }
        async fn chunk_at(&self, _: &str, _: Option<u32>) -> Result<Option<ChunkHit>, String> {
            Ok(None)
        }
        async fn stored_vector(&self, _: &NodeId) -> Result<Option<Vec<f32>>, String> {
            Ok(None)
        }
        async fn embed(&self, _: &str) -> Result<Vec<f32>, String> {
            Ok(vec![1.0, 0.0, 0.0])
        }
        async fn nearest(&self, _: &[f32], _: usize) -> Result<Vec<ChunkHit>, String> {
            Ok(Vec::new())
        }
        async fn chunk_lines(&self) -> Result<Vec<(NodeId, (u32, u32))>, String> {
            Ok(Vec::new())
        }
    }

    #[tokio::test]
    async fn r2_an_expansion_with_no_edges_still_costs_its_call() {
        let mut reg = Registry::new(60.0);
        reg.register(Box::new(VectorChunks::new(Arc::new(Empty), None)), 1.0, 40)
            .unwrap();
        let result = walk(&reg, vec![seed("a")], budget(10), |t| t).await;
        assert_eq!(result.totals.hops, 0);
        assert_eq!(result.totals.external_calls, 1, "the embedding call");
        let events = expand_events(&result);
        assert_eq!(events[0]["calls"][0]["external_calls"], 1);
    }
}

// ---- R3: external calls are reserved before the maps run ----------------------

#[tokio::test]
async fn r3_maps_share_the_allowance_in_map_order() {
    // Two maps each need a call and one is left: the first runs, the second
    // is given none. Both used to start with the same allowance. The walk
    // goes on without them (no more calls are possible), so it ends complete.
    let runs = Arc::new(AtomicU32::new(0));
    let reg = registry(vec![
        GraphMap::new("g1", &[("a", "b")]).cost(0, 1).counted(&runs),
        GraphMap::new("g2", &[("a", "c")]).cost(0, 1).counted(&runs),
    ]);
    let mut limits = budget(10);
    limits.external_calls = 1;
    let result = walk(&reg, vec![seed("a")], limits, |t| t).await;
    assert_eq!(runs.load(Ordering::Relaxed), 1, "calls that ran");
    assert_eq!(result.totals.external_calls, 1);
    assert!(result.reached(&node("b")), "the first map's edge is kept");
    assert!(!result.reached(&node("c")));
    assert_eq!(result.status, TraversalStatus::Complete);
}

#[tokio::test]
async fn r3_work_that_ran_is_charged_even_when_its_edges_are_rejected() {
    // A map that ignores its allowance spends two calls with one left.
    let reg = registry(vec![GraphMap::new("g", &[("a", "b")]).cost(0, 2)]);
    let mut limits = budget(10);
    limits.external_calls = 1;
    let result = walk(&reg, vec![seed("a")], limits, |t| t).await;
    assert_eq!(result.totals.external_calls, 2, "they ran");
    assert!(!result.reached(&node("b")), "over the allowance: rejected");
    assert_eq!(result.status, TraversalStatus::BudgetExhausted);
}

// ---- R4: a seed's first hop is not cross-map --------------------------------

#[tokio::test]
async fn r4_a_hop_from_a_seed_is_not_cross_map() {
    let reg = registry(vec![GraphMap::new("graph", &[("a", "b"), ("b", "c")])]);
    let from_lexical = Seed {
        node: node("a"),
        map: Some("lexical"),
    };
    let result = walk(&reg, vec![from_lexical], budget(10), |t| t).await;
    assert_eq!(result.totals.hops, 2);
    assert_eq!(result.totals.cross_map_hops, 0);
    assert!(
        result
            .events
            .iter()
            .filter(|e| e["ev"] == "hop")
            .all(|e| e["cross_map"] == false)
    );
}

// ---- R5: arrivals outside the beam still corroborate ---------------------------

#[tokio::test]
async fn r5_arrivals_beyond_the_beam_still_join_the_corroboration() {
    // `a` finds x0..x7 by m1. `c` offers the same eight by m2 at a low
    // confidence and sixteen new files by m3 at a high one: 24 candidates are
    // above the gate and the beam keeps the sixteen new ones. The eight
    // arrivals are outside it but m2 still reached them.
    let mut m1 = GraphMap::new("m1", &[]);
    m1.edges = (0..8).map(|i| ("a".to_string(), format!("x{i}"))).collect();
    let mut m2 = GraphMap::new("m2", &[]).conf(20);
    m2.edges = (0..8).map(|i| ("c".to_string(), format!("x{i}"))).collect();
    let mut m3 = GraphMap::new("m3", &[]).conf(90);
    m3.edges = (0..16)
        .map(|i| ("c".to_string(), format!("y{i}")))
        .collect();
    let reg = registry(vec![m1, m2, m3]);
    let result = walk(&reg, vec![seed("a"), seed("c")], budget(100), |t| t).await;
    for i in 0..8 {
        let state = result
            .states
            .iter()
            .find(|s| s.node == node(&format!("x{i}")))
            .unwrap();
        let maps: Vec<_> = state.maps.iter().copied().collect();
        assert_eq!(maps, ["m1", "m2"], "x{i}");
    }
    assert!(
        result.events.iter().all(|e| e["ev"] != "alt_path"),
        "no route is traced for them"
    );
}

// ---- R6: the frontier is a heap ---------------------------------------------

#[test]
fn r6_the_frontier_pops_in_logarithmic_time() {
    let n = 20_000usize;
    let mut rng = SplitMix(7);
    let mut frontier = Frontier::default();
    let mut expected = Vec::new();
    for state in 0..n {
        let priority = rng.next_f64();
        frontier.push(state, priority);
        expected.push((priority, state));
    }
    // Best first: higher priority, then earlier discovery.
    expected.sort_by(|a, b| b.0.total_cmp(&a.0).then(a.1.cmp(&b.1)));
    let started = Clock::now();
    let mut popped = Vec::new();
    while let Some(item) = frontier.pop() {
        popped.push(item.state);
    }
    let took = started.elapsed();
    assert_eq!(popped, expected.iter().map(|(_, s)| *s).collect::<Vec<_>>());
    // A linear scan per pop takes seconds here; a heap takes milliseconds.
    assert!(took < Duration::from_millis(1500), "{took:?}");
}

#[test]
fn r6_a_changed_priority_is_seen_by_the_next_pop() {
    let mut frontier = Frontier::default();
    for (state, priority) in [(0, 0.5), (1, 0.9), (2, 0.7)] {
        frontier.push(state, priority);
    }
    assert!(frontier.set_priority(0, 2.0));
    assert!(frontier.scale(2, 2.0));
    assert!(frontier.remove(1));
    assert!(!frontier.contains(1));
    assert_eq!(frontier.priority_of(2), Some(1.4));
    assert_eq!(frontier.len(), 2);
    let order: Vec<usize> = std::iter::from_fn(|| frontier.pop().map(|i| i.state)).collect();
    assert_eq!(order, [0, 2]);
    assert!(frontier.is_empty());
}

#[test]
fn r6_a_requeued_item_keeps_its_place() {
    let mut frontier = Frontier::default();
    frontier.push(0, 1.0);
    frontier.push(1, 1.0);
    let first = frontier.pop().unwrap();
    assert_eq!(first.state, 0);
    frontier.requeue(first);
    assert_eq!(frontier.pop().unwrap().state, 0, "its discovery number");
    assert_eq!(frontier.pop().unwrap().state, 1);
}
