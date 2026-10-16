//! The in-request edge bandit (`Traversal::edge_bandit`): off it changes
//! nothing, on it moves the queue toward the kind of edge that finds results
//! and reports its tries, hits and weights. The signals: `topk` (the walk's own
//! top `k`), `rerank` (a rescorer of its own, counted and capped), and the
//! baselines `fixed` and `random`.

use super::*;
use crate::traverse::{
    bandit::{Options, Signal, WEIGHT_MAX, WEIGHT_MIN, random_weight},
    handle::{HandleError, Snapshot},
    rescore::{FrontierRescorer, RerankFailure, RescoreError},
    stop::StopReason,
};
use std::{collections::HashMap, sync::Mutex};

/// The events with the bandit's additions (a hop's `priority`, the end event's
/// `totals.bandit`) taken out.
fn without_bandit_fields(events: &[Value]) -> Vec<Value> {
    events
        .iter()
        .cloned()
        .map(|mut event| {
            if let Some(object) = event.as_object_mut() {
                object.remove("priority");
            }
            if let Some(totals) = event.get_mut("totals").and_then(Value::as_object_mut) {
                totals.remove("bandit");
            }
            event
        })
        .collect()
}

fn potentials(result: &TraversalResult) -> Vec<(String, f64)> {
    result
        .states
        .iter()
        .map(|s| (s.node.to_string(), s.potential))
        .collect()
}

/// The hop event to the node called `name`.
fn hop_to<'r>(result: &'r TraversalResult, name: &str) -> &'r Value {
    events_of(result, "hop")
        .into_iter()
        .find(|e| e["dst"].as_str().unwrap().ends_with(&format!("{name}.rs")))
        .unwrap_or_else(|| panic!("no hop to {name}"))
}

/// Where `name` was popped.
fn pop_index(result: &TraversalResult, name: &str) -> usize {
    popped_names(result)
        .iter()
        .position(|n| n.ends_with(&format!("{name}.rs")))
        .unwrap_or_else(|| panic!("{name} not popped in {:?}", popped_names(result)))
}

/// Rates a node by name (1 when unlisted) and keeps the nodes of each call.
#[derive(Clone)]
struct Rater {
    scores: HashMap<NodeId, f64>,
    fail: bool,
    calls: Arc<Mutex<Vec<Vec<NodeId>>>>,
    identity: &'static str,
}

impl Rater {
    fn new(scores: &[(&str, f64)]) -> Self {
        Self {
            scores: scores.iter().map(|&(n, s)| (node(n), s)).collect(),
            fail: false,
            calls: Arc::default(),
            identity: "rater-a",
        }
    }

    fn failing(mut self) -> Self {
        self.fail = true;
        self
    }

    fn calls(&self) -> Vec<Vec<NodeId>> {
        self.calls.lock().unwrap().clone()
    }
}

#[crate::async_trait]
impl FrontierRescorer for Rater {
    fn identity(&self) -> String {
        self.identity.to_owned()
    }

    async fn relevance(&self, _question: &str, nodes: &[NodeId]) -> Result<Vec<f64>, RescoreError> {
        self.calls.lock().unwrap().push(nodes.to_vec());
        if self.fail {
            return Err(RescoreError::new(RerankFailure::Other, "the rater is down"));
        }
        Ok(nodes
            .iter()
            .map(|n| self.scores.get(n).copied().unwrap_or(1.0))
            .collect())
    }
}

fn rerank_options(every: u32, sample: u32) -> Options {
    Options {
        every,
        sample,
        ..Options::default()
    }
}

#[tokio::test]
async fn off_the_walk_and_its_trace_are_as_before() {
    let reg = registry(vec![lattice_map("g", 12)]);
    let plain = walk(&reg, vec![seed("n0")], budget(30), |t| t.top_k(4, 1)).await;
    let off = walk(&reg, vec![seed("n0")], budget(30), |t| {
        t.top_k(4, 1).edge_bandit(None)
    })
    .await;
    assert_eq!(plain.events, off.events);
    assert_eq!(plain.popped, off.popped);
    assert_eq!(potentials(&plain), potentials(&off));
    assert!(off.bandit.is_none());
    for event in &off.events {
        assert!(event.get("priority").is_none(), "{event}");
    }
    let end = off.events.last().unwrap();
    assert!(end["totals"].get("bandit").is_none());
    assert!(end["totals"].get("bandit_calls").is_none());
    assert_eq!(end["totals"], plain.events.last().unwrap()["totals"]);
    assert_eq!(off.totals.bandit_calls, 0);
}

#[tokio::test]
async fn a_bandit_of_rate_zero_orders_the_walk_as_off_and_still_reports() {
    let reg = registry(vec![lattice_map("g", 12)]);
    let off = walk(&reg, vec![seed("n0")], budget(30), |t| {
        t.top_k(4, 1).score_bound(false)
    })
    .await;
    let flat = walk(&reg, vec![seed("n0")], budget(30), |t| {
        t.top_k(4, 1).score_bound(false).edge_bandit(Some(0.0))
    })
    .await;
    assert_eq!(off.popped, flat.popped);
    assert_eq!(potentials(&off), potentials(&flat));
    assert_eq!(without_bandit_fields(&flat.events), off.events);
    // The hops carry their priority, equal to the potential at weight 1.
    for hop in events_of(&flat, "hop") {
        assert_eq!(hop["priority"], hop["potential"], "{hop}");
    }
    let report = flat.bandit.as_ref().expect("the bandit was on");
    assert_eq!(report.rate, 0.0);
    // Nothing is recorded while fewer than `k` (4) results exist, so the first
    // four adoptions are not tries.
    let tries: u32 = report.kinds.values().map(|k| k.tries).sum();
    assert!(flat.totals.hops > 4, "{}", flat.totals.hops);
    assert_eq!(tries, flat.totals.hops - 4);
    assert!(report.kinds.values().all(|k| k.weight == 1.0));
    assert_eq!(
        flat.events.last().unwrap()["totals"]["bandit"],
        report.to_json()
    );
}

/// Seed `s` reaches `a` through map `p` (kind `Contains`) and `b` through map
/// `q` (kind `Cochange`), both worth .5. `a` offers three more nodes through
/// `p`, `b` one through `q`.
fn two_kinds() -> Registry<'static> {
    registry(vec![
        GraphMap::new("p", &[("s", "a"), ("a", "a1"), ("a", "a2"), ("a", "a3")])
            .kind(EdgeKind::Contains),
        GraphMap::new("q", &[("s", "b"), ("b", "b1")]).kind(EdgeKind::Cochange),
    ])
}

#[tokio::test]
async fn topk_records_nothing_until_the_list_is_full() {
    let reg = two_kinds();
    let off = walk(&reg, vec![seed("s")], budget(30), |t| {
        t.top_k(3, 2).score_bound(false)
    })
    .await;
    let on = walk(&reg, vec![seed("s")], budget(30), |t| {
        t.top_k(3, 2).score_bound(false).edge_bandit(Some(1.0))
    })
    .await;
    // Same graph, same potentials: only the queue can differ.
    let mut a = potentials(&off);
    let mut b = potentials(&on);
    a.sort_by(|x, y| x.0.cmp(&y.0));
    b.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(a, b, "the bandit changes no potential");

    // `s` adopts `a` and `b` (2 results), `a1` fills the top 3 (.5, .5, .25):
    // none of the three is a try. `a2`, `a3` and `b1` are worth .25, which does
    // not beat the 3rd (a tie goes to the earlier result): three misses.
    let report = on.bandit.as_ref().expect("the bandit was on");
    let contains = report.kinds[&EdgeKind::Contains];
    let cochange = report.kinds[&EdgeKind::Cochange];
    assert_eq!((contains.tries, contains.hits), (2, 0));
    assert_eq!((cochange.tries, cochange.hits), (1, 0));
    assert_eq!(on.totals.hops, 6);
    for w in [contains.weight, cochange.weight] {
        assert!((WEIGHT_MIN..=WEIGHT_MAX).contains(&w));
    }
    // `b1` is queued after `a`'s misses: Contains has 0 of 2 (smoothed 1/4) and
    // is the only kind tried, Cochange has no evidence (1/2), so Cochange
    // weighs (1/2) / (1/4) = 2, which is the cap.
    let b1 = hop_to(&on, "b1");
    assert_eq!(b1["potential"], 0.25);
    assert_eq!(b1["priority"], 0.5);
    // The first adoptions had no stats to weigh by.
    assert_eq!(hop_to(&on, "a")["priority"], hop_to(&on, "a")["potential"]);
}

#[tokio::test]
async fn topk_counts_a_hit_for_a_node_that_outranks_the_kth() {
    // `c` (a call, worth 1) and `a` (containment, worth .5) fill the top 2.
    // `c` offers two calls worth 1: the first beats the 2nd result (.5), the
    // second only ties it. `a` offers two containments worth .25: misses.
    let reg = registry(vec![
        GraphMap::new("p", &[("s", "a"), ("a", "a1"), ("a", "a2")]).kind(EdgeKind::Contains),
        GraphMap::new("q", &[("s", "c"), ("c", "c1"), ("c", "c2")]).kind(EdgeKind::CallsOrRefs),
    ]);
    let on = walk(&reg, vec![seed("s")], budget(30), |t| {
        t.top_k(2, 2).score_bound(false).edge_bandit(Some(1.0))
    })
    .await;
    let report = on.bandit.as_ref().unwrap();
    let calls = report.kinds[&EdgeKind::CallsOrRefs];
    let contains = report.kinds[&EdgeKind::Contains];
    assert_eq!((calls.tries, calls.hits), (2, 1));
    assert_eq!((contains.tries, contains.hits), (2, 0));
    assert!(calls.weight > contains.weight);
}

#[tokio::test]
async fn the_queue_moves_toward_the_kind_the_rater_likes() {
    // `s` offers `a` (containment, .5) and `b` (a call, 1). The rater likes
    // `a` and `x`, dislikes `b` and `y`. `b` offers `x` (containment, worth .5)
    // and `y` (a call, worth 1): off, the call is queued above the containment.
    let reg = registry(vec![
        GraphMap::new("p", &[("s", "a"), ("b", "x")]).kind(EdgeKind::Contains),
        GraphMap::new("q", &[("s", "b"), ("b", "y")]).kind(EdgeKind::CallsOrRefs),
    ]);
    let rater = Rater::new(&[("a", 0.9), ("b", 0.1), ("x", 0.9), ("y", 0.1)]);
    let off = walk(&reg, vec![seed("s")], budget(30), |t| t).await;
    let on = walk(&reg, vec![seed("s")], budget(30), |t| {
        t.question("which file")
            .edge_bandit(Some(2.0))
            .bandit_signal(Signal::Rerank)
            .bandit_options(rerank_options(1, 8))
            .bandit_rescorer(rater.clone())
    })
    .await;
    assert!(
        pop_index(&off, "y") < pop_index(&off, "x"),
        "{:?}",
        popped_names(&off)
    );
    assert!(
        pop_index(&on, "x") < pop_index(&on, "y"),
        "{:?}",
        popped_names(&on)
    );

    // The rater rated a, b after `s` (2 docs) and x, y after `b`.
    let report = on.bandit.as_ref().unwrap();
    let contains = report.kinds[&EdgeKind::Contains];
    let calls = report.kinds[&EdgeKind::CallsOrRefs];
    assert_eq!((contains.tries, contains.hits), (2, 2));
    assert_eq!((calls.tries, calls.hits), (2, 0));
    assert_eq!(on.totals.bandit_calls as usize, rater.calls().len());
    assert_eq!(on.totals.bandit_docs, 4);

    // x and y were queued with the stats after the first round: Contains 1 of
    // 1 (2/3), Calls 0 of 1 (1/3), mean 1/2, squared (rate 2): x weighs 16/9
    // and y 4/9, which the lower clamp lifts to 1/2.
    let (x, y) = (hop_to(&on, "x"), hop_to(&on, "y"));
    assert!((x["priority"].as_f64().unwrap() - 0.5 * (4.0 / 3.0_f64).powi(2)).abs() < 1e-12);
    assert_eq!(y["priority"], 0.5, "{y}");
    // Potentials, hence results' scores, are as off.
    let mut a = potentials(&off);
    let mut b = potentials(&on);
    a.sort_by(|x, y| x.0.cmp(&y.0));
    b.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(a, b);
}

#[tokio::test]
async fn a_relaxed_node_keeps_the_weight_of_the_first_route() {
    // `s` offers `a` and `b` (containment, .5) and `c` (cochange, .5); the
    // rater likes the containments. `a` offers `x` (containment, .25), queued
    // at its weight; then `b` reaches `x` with a call worth .5: the relax scales
    // the priority by the ratio, so the weight stays and `x` (.5 x weight)
    // pops before `c` (.5), which was queued earlier at weight 1.
    let reg = registry(vec![
        GraphMap::new("p", &[("s", "a"), ("s", "b"), ("a", "x")]).kind(EdgeKind::Contains),
        GraphMap::new("q", &[("s", "c")]).kind(EdgeKind::Cochange),
        GraphMap::new("r", &[("b", "x")]).kind(EdgeKind::CallsOrRefs),
    ]);
    let rater = Rater::new(&[("a", 0.9), ("b", 0.9), ("c", 0.1)]);
    let on = walk(&reg, vec![seed("s")], budget(30), |t| {
        t.question("which file")
            .edge_bandit(Some(1.0))
            .bandit_signal(Signal::Rerank)
            .bandit_options(rerank_options(1, 8))
            .bandit_rescorer(rater.clone())
    })
    .await;
    let off = walk(&reg, vec![seed("s")], budget(30), |t| t).await;
    let relax = |r: &TraversalResult| {
        events_of(r, "relax")
            .into_iter()
            .find(|e| e["dst"].as_str().unwrap().ends_with("x.rs"))
            .cloned()
    };
    assert!(relax(&off).is_some(), "off: the relax happens");
    let relaxed = relax(&on).expect("on: x is relaxed by the call from b");
    assert_eq!(relaxed["old_potential"], 0.25);
    assert_eq!(relaxed["potential"], 0.5);
    // Off, x ties c at .5 and c was queued first.
    assert!(pop_index(&off, "c") < pop_index(&off, "x"));
    // On, x keeps the containment weight (above 1) and pops first.
    assert!(
        pop_index(&on, "x") < pop_index(&on, "c"),
        "{:?}",
        popped_names(&on)
    );
}

/// Two trees, one of containments and one of calls.
fn trees() -> Registry<'static> {
    registry(vec![
        GraphMap::new(
            "p",
            &[
                ("s", "pa"),
                ("s", "pb"),
                ("s", "pc"),
                ("pa", "pa1"),
                ("pa", "pa2"),
                ("pb", "pb1"),
                ("pb", "pb2"),
                ("pc", "pc1"),
                ("pc", "pc2"),
            ],
        )
        .kind(EdgeKind::Contains),
        GraphMap::new(
            "q",
            &[
                ("s", "qa"),
                ("s", "qb"),
                ("qa", "qa1"),
                ("qa", "qa2"),
                ("qa", "qa3"),
                ("qb", "qb1"),
                ("qb", "qb2"),
                ("qb", "qb3"),
            ],
        )
        .kind(EdgeKind::CallsOrRefs),
    ])
}

#[tokio::test]
async fn the_score_bound_stops_the_same_walk_with_the_bandit_on() {
    let reg = trees();
    let k = 9;
    let ranks = |r: &TraversalResult| -> Vec<f64> {
        r.ranked()
            .into_iter()
            .take(k)
            .map(|i| r.states[i].ranked_potential(r.corroboration_boost))
            .collect()
    };
    let off = walk(&reg, vec![seed("s")], budget(40), |t| t.top_k(k, 1)).await;
    assert_eq!(off.stop_reason, StopReason::Bound, "{:?}", off.stop_reason);

    let mut fixed = Options::default();
    fixed.fixed.insert(EdgeKind::Contains, 2.0);
    fixed.fixed.insert(EdgeKind::CallsOrRefs, 0.5);
    let arms: Vec<(&str, Signal, Options)> = vec![
        ("fixed", Signal::Fixed, fixed),
        (
            "random",
            Signal::Random,
            Options {
                seed: 7,
                ..Options::default()
            },
        ),
        ("topk", Signal::Topk, Options::default()),
    ];
    let mut reordered = false;
    for (name, signal, options) in arms {
        let on = walk(&reg, vec![seed("s")], budget(40), |t| {
            t.top_k(k, 1)
                .edge_bandit(Some(2.0))
                .bandit_signal(signal)
                .bandit_options(options)
        })
        .await;
        // The bound is computed from potentials, which the weights do not
        // touch. A reordered walk may need more pops before the bound holds
        // (or run out of work first), but it never stops while a queued state
        // could still enter the top results, and it ends with the same ones.
        assert!(
            matches!(on.stop_reason, StopReason::Bound | StopReason::Complete),
            "{name}: {:?}",
            on.stop_reason
        );
        for bound in events_of(&on, "bound") {
            let (best, kth) = (
                bound["frontier_best"].as_f64().unwrap(),
                bound["kth"].as_f64().unwrap(),
            );
            assert!(best <= kth, "{name}: {bound}");
        }
        assert_eq!(ranks(&on), ranks(&off), "{name}");
        reordered |= popped_names(&on) != popped_names(&off);
    }
    assert!(reordered, "at least one arm changed the pop order");
}

#[tokio::test]
async fn fixed_weights_move_the_queue_and_keep_no_stats() {
    let reg = registry(vec![
        GraphMap::new("p", &[("s", "a"), ("a", "a1")]).kind(EdgeKind::Contains),
        GraphMap::new("q", &[("s", "b"), ("b", "b1")]).kind(EdgeKind::Cochange),
    ]);
    let mut options = Options::default();
    options.fixed.insert(EdgeKind::Contains, 2.0);
    options.fixed.insert(EdgeKind::Cochange, 0.5);
    let off = walk(&reg, vec![seed("s")], budget(30), |t| t).await;
    let on = walk(&reg, vec![seed("s")], budget(30), |t| {
        t.edge_bandit(Some(1.0))
            .bandit_signal(Signal::Fixed)
            .bandit_options(options.clone())
    })
    .await;
    assert_eq!(popped_names(&off).len(), popped_names(&on).len());
    // Off: s a b a1 b1. On: a (1.0) and a1 (.5) come before b (.25).
    assert!(pop_index(&off, "b") < pop_index(&off, "a1"));
    assert!(
        pop_index(&on, "a1") < pop_index(&on, "b"),
        "{:?}",
        popped_names(&on)
    );
    assert_eq!(hop_to(&on, "a")["priority"], 1.0);
    assert_eq!(hop_to(&on, "b")["priority"], 0.25);
    assert_eq!(hop_to(&on, "a1")["priority"], 0.5);
    assert_eq!(hop_to(&on, "b1")["priority"], 0.125);
    let report = on.bandit.as_ref().unwrap();
    assert_eq!(report.signal, Signal::Fixed);
    assert!(report.kinds.values().all(|k| (k.tries, k.hits) == (0, 0)));
    assert_eq!(report.kinds[&EdgeKind::Contains].weight, 2.0);
    assert_eq!(report.kinds[&EdgeKind::Cochange].weight, 0.5);
    assert_eq!(on.totals.bandit_calls, 0);
    // Deterministic.
    let again = walk(&reg, vec![seed("s")], budget(30), |t| {
        t.edge_bandit(Some(1.0))
            .bandit_signal(Signal::Fixed)
            .bandit_options(options.clone())
    })
    .await;
    assert_eq!(on.events, again.events);
}

#[tokio::test]
async fn random_weights_are_deterministic_by_seed() {
    let reg = trees();
    let run = |seed_value: u64| {
        let reg = &reg;
        async move {
            walk(reg, vec![seed("s")], budget(40), |t| {
                t.edge_bandit(Some(1.0))
                    .bandit_signal(Signal::Random)
                    .bandit_options(Options {
                        seed: seed_value,
                        ..Options::default()
                    })
            })
            .await
        }
    };
    let (one, again, other) = (run(1).await, run(1).await, run(2).await);
    assert_eq!(one.events, again.events);
    assert_eq!(one.popped, again.popped);
    assert_ne!(one.events, other.events);
    let report = one.bandit.as_ref().unwrap();
    assert_eq!(report.signal, Signal::Random);
    for (kind, r) in &report.kinds {
        assert_eq!(r.weight, random_weight(1, *kind));
        assert!((WEIGHT_MIN..WEIGHT_MAX).contains(&r.weight));
        assert_eq!((r.tries, r.hits), (0, 0));
    }
    // Each hop's priority is its potential times its kind's weight.
    for hop in events_of(&one, "hop") {
        let kind = EdgeKind::from_name(hop["kind"].as_str().unwrap()).unwrap();
        let expected = hop["potential"].as_f64().unwrap() * random_weight(1, kind);
        assert!((hop["priority"].as_f64().unwrap() - expected).abs() < 1e-12);
    }
    // No weight changes a potential.
    let mut a = potentials(&one);
    let mut b = potentials(&other);
    a.sort_by(|x, y| x.0.cmp(&y.0));
    b.sort_by(|x, y| x.0.cmp(&y.0));
    assert_eq!(a, b);
}

#[tokio::test]
async fn the_rerank_signal_counts_its_calls_and_docs() {
    let reg = registry(vec![lattice_map("g", 12)]);
    let rate = |every: u32, sample: u32| {
        let rater = Rater::new(&[]);
        (rater, every, sample)
    };
    let (rater, every, sample) = rate(1, 2);
    let r = walk(&reg, vec![seed("n0")], budget(30), |t| {
        t.question("q")
            .edge_bandit(Some(1.0))
            .bandit_signal(Signal::Rerank)
            .bandit_options(rerank_options(every, sample))
            .bandit_rescorer(rater.clone())
    })
    .await;
    let calls = rater.calls();
    assert!(!calls.is_empty());
    assert!(
        calls.iter().all(|c| !c.is_empty() && c.len() <= 2),
        "{calls:?}"
    );
    assert_eq!(r.totals.bandit_calls as usize, calls.len());
    assert_eq!(
        r.totals.bandit_docs as usize,
        calls.iter().map(Vec::len).sum::<usize>()
    );
    assert_eq!(events_of(&r, "bandit").len(), calls.len());
    let report = r.bandit.as_ref().unwrap();
    // Every rated node scored 1: a hit.
    let tries: u32 = report.kinds.values().map(|k| k.tries).sum();
    let hits: u32 = report.kinds.values().map(|k| k.hits).sum();
    assert_eq!(tries, r.totals.bandit_docs);
    assert_eq!(hits, tries);
    let end = r.events.last().unwrap();
    assert_eq!(end["totals"]["bandit_calls"], r.totals.bandit_calls);
    assert_eq!(end["totals"]["bandit_docs"], r.totals.bandit_docs);
    assert_eq!(end["totals"]["bandit"]["failures"], 0);
    assert_eq!(r.totals.rerank_calls, 0, "the frontier reranker is off");
    // The walk's order does not depend on the rescorer when all hit alike.

    // A round every second expansion makes fewer calls.
    let (slow, every, sample) = rate(2, 2);
    let r2 = walk(&reg, vec![seed("n0")], budget(30), |t| {
        t.question("q")
            .edge_bandit(Some(1.0))
            .bandit_signal(Signal::Rerank)
            .bandit_options(rerank_options(every, sample))
            .bandit_rescorer(slow.clone())
    })
    .await;
    assert!(r2.totals.bandit_calls > 0);
    assert!(r2.totals.bandit_calls < r.totals.bandit_calls);
}

#[tokio::test]
async fn the_rerank_signal_respects_model_calls_together_with_the_frontier() {
    let reg = registry(vec![lattice_map("g", 12)]);
    // Alone: the cap of 2 allows two rating calls, the later rounds are skipped.
    let rater = Rater::new(&[]);
    let capped = walk(&reg, vec![seed("n0")], budget(30), |t| {
        t.question("q")
            .edge_bandit(Some(1.0))
            .bandit_signal(Signal::Rerank)
            .bandit_options(rerank_options(1, 2))
            .bandit_rescorer(rater.clone())
            .model_calls(Some(2))
    })
    .await;
    assert_eq!(capped.totals.bandit_calls, 2);
    assert_eq!(rater.calls().len(), 2);
    let report = capped.bandit.as_ref().unwrap();
    assert!(report.skipped > 0);
    assert!(
        capped
            .warnings
            .iter()
            .any(|w| w.contains("edge_bandit") && w.contains("model_calls")),
        "{:?}",
        capped.warnings
    );

    // With the frontier reranker too, the cap counts both.
    let frontier = Rater::new(&[]);
    let bandit = Rater::new(&[]);
    let both = walk(&reg, vec![seed("n0")], budget(30), |t| {
        t.question("q")
            .rerank_frontier(frontier.clone(), 0.5)
            .edge_bandit(Some(1.0))
            .bandit_signal(Signal::Rerank)
            .bandit_options(rerank_options(1, 2))
            .bandit_rescorer(bandit.clone())
            .model_calls(Some(5))
    })
    .await;
    assert_eq!(both.totals.rerank_calls + both.totals.bandit_calls, 5);
    assert_eq!(
        frontier.calls().len() + bandit.calls().len(),
        5,
        "no call beyond the cap"
    );
    assert!(both.totals.rerank_calls > 0 && both.totals.bandit_calls > 0);
}

#[tokio::test]
async fn a_failed_rating_call_records_nothing_and_is_counted() {
    let reg = registry(vec![lattice_map("g", 12)]);
    let off = walk(&reg, vec![seed("n0")], budget(30), |t| t).await;
    let rater = Rater::new(&[]).failing();
    let on = walk(&reg, vec![seed("n0")], budget(30), |t| {
        t.question("q")
            .edge_bandit(Some(1.0))
            .bandit_signal(Signal::Rerank)
            .bandit_options(rerank_options(1, 2))
            .bandit_rescorer(rater.clone())
    })
    .await;
    // No stats, so every weight is 1 and the walk is the plain one.
    assert_eq!(on.popped, off.popped);
    let report = on.bandit.as_ref().unwrap();
    assert!(report.kinds.is_empty());
    assert_eq!(report.failures, on.totals.bandit_calls);
    assert!(on.totals.bandit_calls > 0);
    assert!(
        on.warnings
            .iter()
            .any(|w| w.contains("rating calls failed")),
        "{:?}",
        on.warnings
    );
    assert!(events_of(&on, "bandit").iter().all(|e| e["failed"] == true));
}

#[tokio::test]
async fn the_rerank_signal_without_a_rescorer_says_so_and_changes_nothing() {
    let reg = registry(vec![lattice_map("g", 12)]);
    let off = walk(&reg, vec![seed("n0")], budget(30), |t| t).await;
    let on = walk(&reg, vec![seed("n0")], budget(30), |t| {
        t.edge_bandit(Some(1.0)).bandit_signal(Signal::Rerank)
    })
    .await;
    assert_eq!(on.popped, off.popped);
    assert_eq!(on.totals.bandit_calls, 0);
    assert!(
        on.warnings.iter().any(|w| w.contains("no rescorer")),
        "{:?}",
        on.warnings
    );
}

#[tokio::test]
async fn a_handle_keeps_the_bandit_settings_and_refuses_others() {
    let reg = registry(vec![lattice_map("g", 12)]);
    let first = walk(&reg, vec![seed("n0")], budget(3), |t| {
        t.edge_bandit(Some(1.0))
    })
    .await;
    let snapshot = first.snapshot.expect("the pop cap left work");
    assert_eq!(snapshot.bandit.as_deref(), Some("topk;rate=1"));
    assert_eq!(Snapshot::from_json(&snapshot.to_json()).unwrap(), snapshot);
    snapshot.check_bandit(Some("topk;rate=1")).unwrap();
    for (what, other) in [
        ("off", None),
        ("another rate", Some("topk;rate=2")),
        ("another signal", Some("random;seed=0")),
    ] {
        let error = snapshot.check_bandit(other).unwrap_err();
        assert!(matches!(error, HandleError::WrongBandit { .. }), "{what}");
        assert!(error.to_string().contains("edge bandit"), "{error}");
    }

    // A handle made without the bandit refuses it, and is what it was.
    let plain = walk(&reg, vec![seed("n0")], budget(3), |t| t)
        .await
        .snapshot
        .expect("work left");
    assert_eq!(plain.bandit, None);
    assert!(plain.to_json().get("bandit").is_none());
    plain.check_bandit(None).unwrap();
    assert!(plain.check_bandit(Some("topk;rate=1")).is_err());

    // The signal's own settings and the rescorer's identity are part of it.
    let rerank = walk(&reg, vec![seed("n0")], budget(3), |t| {
        t.question("q")
            .edge_bandit(Some(1.0))
            .bandit_signal(Signal::Rerank)
            .bandit_options(rerank_options(2, 3))
            .bandit_rescorer(Rater::new(&[]))
    })
    .await
    .snapshot
    .expect("work left");
    let text = rerank.bandit.clone().unwrap();
    assert!(text.starts_with("rerank;"), "{text}");
    assert!(
        text.contains("every=2") && text.contains("sample=3"),
        "{text}"
    );
    assert!(text.contains("rescorer=rater-a"), "{text}");

    // The bandit calls persist, so a resume counts on from them.
    assert!(rerank.totals.bandit_calls > 0);
    let back = Snapshot::from_json(&rerank.to_json()).unwrap();
    assert_eq!(back.totals.bandit_calls, rerank.totals.bandit_calls);
    assert_eq!(back.totals.bandit_docs, rerank.totals.bandit_docs);

    // A matching resume carries on with a new bandit (no stats).
    let second = resume(&reg, snapshot, budget(30), |t| t.edge_bandit(Some(1.0))).await;
    assert!(second.totals.pops > 3);
    assert!(second.bandit.is_some());
}

#[tokio::test]
async fn the_fingerprint_of_a_traversal_matches_the_settings_text() {
    let reg = registry(vec![lattice_map("g", 4)]);
    let view = view();
    let t = Traversal::new(OnTheFly::new(&reg, &view), vec![seed("n0")], budget(3));
    assert_eq!(t.bandit_fingerprint(), None);
    let t = t.edge_bandit(Some(3.0));
    assert_eq!(t.bandit_fingerprint().as_deref(), Some("topk;rate=3"));
    // A rate out of range is the default.
    let t = t.edge_bandit(Some(9.0));
    assert_eq!(t.bandit_fingerprint().as_deref(), Some("topk;rate=1"));
}

#[tokio::test]
async fn a_state_queued_later_gets_the_weight_of_the_kind_that_found_it() {
    // `x` is first found by `b` at the depth limit (deferred, worth .5), then
    // by `c` one level up (worth .35): that unblocks it, and it is queued at its
    // potential times the weight of the kind of edge from `c` (cochange, 2).
    // `e` is queued at .5. Off, `x` (.35) pops after `e`; on, it (.7) pops first.
    let reg = registry(vec![
        GraphMap::new("calls", &[("s", "a"), ("a", "b")]).kind(EdgeKind::CallsOrRefs),
        GraphMap::new("holds", &[("b", "x"), ("s", "e")]).kind(EdgeKind::Contains),
        GraphMap::new("tests", &[("s", "c")]).kind(EdgeKind::Tests),
        GraphMap::new("co", &[("c", "x")]).kind(EdgeKind::Cochange),
    ]);
    let shallow = TraversalBudget {
        max_depth: 3,
        ..budget(30)
    };
    let off = walk(&reg, vec![seed("s")], shallow, |t| t).await;
    let mut options = Options::default();
    options.fixed.insert(EdgeKind::Cochange, 2.0);
    let on = walk(&reg, vec![seed("s")], shallow, |t| {
        t.edge_bandit(Some(1.0))
            .bandit_signal(Signal::Fixed)
            .bandit_options(options.clone())
    })
    .await;
    for r in [&off, &on] {
        let unblocked = events_of(r, "relax")
            .into_iter()
            .any(|e| e["dst"].as_str().unwrap().ends_with("x.rs") && e["unblocked"] == true);
        assert!(unblocked, "x is unblocked: {:?}", popped_names(r));
    }
    assert!(
        pop_index(&off, "e") < pop_index(&off, "x"),
        "{:?}",
        popped_names(&off)
    );
    assert!(
        pop_index(&on, "x") < pop_index(&on, "e"),
        "{:?}",
        popped_names(&on)
    );
}

/// The nodes the first rating call of a walk over a star of eight children
/// rated, for `seed`, with a sample of three.
async fn first_sample(seed_of_draw: u64) -> Vec<NodeId> {
    let edges: Vec<(String, String)> = (0..8).map(|i| ("s".to_owned(), format!("c{i}"))).collect();
    let edges: Vec<(&str, &str)> = edges
        .iter()
        .map(|(a, b)| (a.as_str(), b.as_str()))
        .collect();
    let reg = registry(vec![GraphMap::new("star", &edges).kind(EdgeKind::Contains)]);
    let rater = Rater::new(&[]);
    walk(&reg, vec![seed("s")], budget(30), |t| {
        t.question("q")
            .edge_bandit(Some(1.0))
            .bandit_signal(Signal::Rerank)
            .bandit_options(Options {
                seed: seed_of_draw,
                ..rerank_options(1, 3)
            })
            .bandit_rescorer(rater.clone())
    })
    .await;
    rater.calls().remove(0)
}

#[tokio::test]
async fn a_round_samples_a_seeded_draw_not_the_first_nodes_adopted() {
    let first_adopted: Vec<NodeId> = ["c0", "c1", "c2"].iter().map(|n| node(n)).collect();
    let mut draws = Vec::new();
    for seed in 0..12 {
        let sample = first_sample(seed).await;
        assert_eq!(sample.len(), 3, "seed {seed}: {sample:?}");
        // The same seed draws the same nodes.
        assert_eq!(sample, first_sample(seed).await, "seed {seed}");
        draws.push(sample);
    }
    assert!(
        draws.iter().any(|d| *d != first_adopted),
        "every seed rated the first three adopted: {draws:?}"
    );
    assert!(
        draws.iter().any(|d| *d != draws[0]),
        "the seed changes nothing: {draws:?}"
    );
}
