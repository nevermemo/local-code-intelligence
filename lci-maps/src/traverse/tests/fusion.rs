//! Weighted reciprocal rank fusion of the results (slice Q2): the arithmetic
//! of [`fusion::rrf`], what the engine feeds it, and that `boost` fusion is
//! left exactly as it was.

use super::*;
use crate::traverse::{
    fusion::{self, Item},
    handle::Snapshot,
};

/// Everything priced by rank: no edge is followed whole.
const RANKED: HopPrice = HopPrice { gate: 0, beam: 16 };

fn names(result: &TraversalResult) -> Vec<String> {
    result
        .ranked()
        .into_iter()
        .map(|i| result.states[i].node.path().unwrap().to_string())
        .collect()
}

fn scores(result: &TraversalResult) -> Vec<(String, f64)> {
    result
        .ranked()
        .into_iter()
        .map(|i| {
            let s = &result.states[i];
            (
                s.node.path().unwrap().to_string(),
                result.ranked_potential(s),
            )
        })
        .collect()
}

async fn fusion_walk(
    reg: &Registry<'static>,
    pops: u32,
    fusion: Fusion,
    configure: impl for<'t> FnOnce(Traversal<'t>) -> Traversal<'t>,
) -> TraversalResult {
    walk(reg, vec![seed("s")], budget(pops), |t| {
        configure(t.price(RANKED).fusion(fusion))
    })
    .await
}

async fn rrf_walk(
    reg: &Registry<'static>,
    pops: u32,
    configure: impl for<'t> FnOnce(Traversal<'t>) -> Traversal<'t>,
) -> TraversalResult {
    fusion_walk(reg, pops, Fusion::Rrf, configure).await
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-12
}

// ---- the arithmetic -------------------------------------------------------------------

fn items<'a>(nodes: &'a [NodeId], rows: &[&[(MapId, f64, f64)]]) -> Vec<Item<'a>> {
    nodes
        .iter()
        .zip(rows)
        .map(|(node, deliveries)| Item {
            node,
            deliveries: deliveries.to_vec(),
        })
        .collect()
}

#[test]
fn rrf_with_one_source_is_that_sources_order_with_k_of_two() {
    let nodes = [node("c"), node("a"), node("b")];
    // Potentials 0.2, 0.9, 0.5: order a, b, c.
    let rows: [&[(MapId, f64, f64)]; 3] =
        [&[("m", 1.0, 0.2)], &[("m", 1.0, 0.9)], &[("m", 1.0, 0.5)]];
    let scores = fusion::rrf(&items(&nodes, &rows), |_| false);
    // c is rank 2, a rank 0, b rank 1; k = 2.
    assert_eq!(scores, [1.0 / 4.0, 1.0 / 2.0, 1.0 / 3.0]);
    assert_eq!(fusion::RRF_K, 2.0);
}

#[test]
fn rrf_ties_in_a_source_go_by_node_id_and_empty_nodes_score_zero() {
    let nodes = [node("b"), node("a"), node("z")];
    let rows: [&[(MapId, f64, f64)]; 3] = [&[("m", 1.0, 0.5)], &[("m", 1.0, 0.5)], &[]];
    let scores = fusion::rrf(&items(&nodes, &rows), |_| false);
    assert_eq!(scores, [1.0 / 3.0, 1.0 / 2.0, 0.0]);
}

#[test]
fn rrf_a_node_in_two_independent_sources_outranks_a_node_in_one_at_the_same_rank() {
    let nodes = [node("both"), node("one")];
    let rows: [&[(MapId, f64, f64)]; 2] =
        [&[("m1", 1.0, 0.8), ("m2", 1.0, 0.8)], &[("m3", 1.0, 0.8)]];
    let scores = fusion::rrf(&items(&nodes, &rows), |_| false);
    // Rank 0 in m1 and m2 against rank 0 in m3.
    assert_eq!(scores, [1.0, 0.5]);
}

#[test]
fn rrf_a_flooding_source_of_low_weight_cannot_out_vote_two_agreeing_sources() {
    // `t` is rank 3 in each of two agreeing sources behind three decoys each;
    // a flooding source puts its own node `f0` first.
    let nodes: Vec<NodeId> = ["t", "a1", "a2", "a3", "b1", "b2", "b3", "f0", "f1", "f2"]
        .iter()
        .map(|n| node(n))
        .collect();
    let rows_for = |flood: f64| -> Vec<Vec<(MapId, f64, f64)>> {
        vec![
            vec![("A", 1.0, 0.1), ("B", 1.0, 0.1)],
            vec![("A", 1.0, 0.9)],
            vec![("A", 1.0, 0.8)],
            vec![("A", 1.0, 0.7)],
            vec![("B", 1.0, 0.9)],
            vec![("B", 1.0, 0.8)],
            vec![("B", 1.0, 0.7)],
            vec![("F", flood, 0.9)],
            vec![("F", flood, 0.8)],
            vec![("F", flood, 0.7)],
        ]
    };
    let rank = |flood: f64| {
        let rows = rows_for(flood);
        let slices: Vec<&[(MapId, f64, f64)]> = rows.iter().map(Vec::as_slice).collect();
        let s = fusion::rrf(&items(&nodes, &slices), |_| false);
        // t: 2 * 1/5; f0 (rank 0 of F): flood / 2.
        assert!(close(s[0], 0.4));
        assert!(close(s[7], flood / 2.0));
        s[7] > s[0]
    };
    assert!(rank(1.0), "with a weight of 1 the flood wins");
    assert!(!rank(0.2), "with a weight of 0.2 it does not");
}

#[test]
fn rrf_dependent_maps_are_one_source_with_the_largest_weight() {
    let nodes = [node("z"), node("y")];
    let rows: [&[(MapId, f64, f64)]; 2] =
        [&[("d1", 0.5, 0.9), ("d2", 1.0, 0.4)], &[("i", 1.0, 0.9)]];
    let scores = fusion::rrf(&items(&nodes, &rows), |m| m.starts_with('d'));
    // z: one source (best potential 0.9, weight max(0.5, 1) = 1), rank 0; y
    // is rank 0 of its own.
    assert_eq!(scores, [0.5, 0.5]);
}

// ---- what the engine feeds it ---------------------------------------------------------

#[tokio::test]
async fn one_source_ranks_by_its_potentials() {
    let reg = registry(vec![GraphMap::new(
        "m1",
        &[("s", "c"), ("s", "a"), ("s", "b")],
    )]);
    let result = rrf_walk(&reg, 20, |t| t).await;
    // The map offers a, b, c in node order: potentials 1, 1/2, 1/3.
    assert_eq!(names(&result), ["src/a.rs", "src/b.rs", "src/c.rs"]);
    let scores: Vec<f64> = scores(&result).into_iter().map(|(_, s)| s).collect();
    assert_eq!(scores, [1.0 / 2.0, 1.0 / 3.0, 1.0 / 4.0]);
    // The cards carry the fused score.
    let card = stop::Card::ranked(
        state(&result, "b"),
        result.ranked_potential(state(&result, "b")),
    );
    assert_eq!(trace::card_json(&card)["ranked_potential"], 1.0 / 3.0);
    // The start node is not a result and ranks 0.
    assert_eq!(result.ranked_potential(state(&result, "s")), 0.0);
    // The order is the boost order here (one source, no corroboration).
    let boost = walk(&reg, vec![seed("s")], budget(20), |t| t.price(RANKED)).await;
    let boosted: Vec<String> = boost
        .ranked()
        .into_iter()
        .map(|i| boost.states[i].node.path().unwrap().to_string())
        .collect();
    assert_eq!(names(&result), boosted);
}

#[tokio::test]
async fn a_node_reached_by_two_independent_sources_outranks_one_reached_by_one() {
    let reg = registry(vec![
        GraphMap::new("m1", &[("s", "z")]),
        GraphMap::new("m2", &[("s", "z")]),
        GraphMap::new("m3", &[("s", "y")]),
    ]);
    let result = rrf_walk(&reg, 20, |t| t).await;
    // z is rank 0 in m1 and in m2; y is rank 0 in m3.
    let got = scores(&result);
    assert_eq!(got[0].0, "src/z.rs");
    assert!(close(got[0].1, 1.0), "{got:?}");
    assert_eq!(got[1].0, "src/y.rs");
    assert!(close(got[1].1, 0.5), "{got:?}");
}

#[tokio::test]
async fn dependent_maps_count_once_in_the_fusion() {
    let reg = registry(vec![
        GraphMap::new("m1", &[("s", "z")]).dependent(),
        GraphMap::new("m2", &[("s", "z")]).dependent(),
        GraphMap::new("m3", &[("s", "y")]),
    ]);
    let result = rrf_walk(&reg, 20, |t| t).await;
    let got = scores(&result);
    // One dependent source (rank 0) and one independent source (rank 0)
    // tie; the node id breaks it.
    assert!(close(got[0].1, 0.5) && close(got[1].1, 0.5), "{got:?}");
    assert_eq!(names(&result), ["src/y.rs", "src/z.rs"]);
    // The same maps, independent, give z both votes.
    let apart = registry(vec![
        GraphMap::new("m1", &[("s", "z")]),
        GraphMap::new("m2", &[("s", "z")]),
        GraphMap::new("m3", &[("s", "y")]),
    ]);
    let apart = rrf_walk(&apart, 20, |t| t).await;
    assert_eq!(names(&apart), ["src/z.rs", "src/y.rs"]);
}

#[tokio::test]
async fn a_map_weight_from_the_policy_scales_the_sources_votes() {
    // `a` comes from F, `z` from M: equal at weight 1, a first by node id.
    let reg = registry(vec![
        GraphMap::new("F", &[("s", "a")]),
        GraphMap::new("M", &[("s", "z")]),
    ]);
    let plain = rrf_walk(&reg, 20, |t| t).await;
    assert_eq!(names(&plain), ["src/a.rs", "src/z.rs"]);
    let weighted = rrf_walk(&reg, 20, |t| {
        t.handler(
            scripted(|_| vec![act(None, Action::MapWeight("F".to_string(), 0.2))]),
            every(1),
        )
    })
    .await;
    let got = scores(&weighted);
    assert_eq!(got[0].0, "src/z.rs", "{got:?}");
    assert!(close(got[0].1, 0.5) && close(got[1].1, 0.1), "{got:?}");
}

#[tokio::test]
async fn the_walk_is_the_same_under_both_fusions_and_the_start_event_records_it() {
    let reg = two_maps_one_node_for_fusion();
    let boost = walk(&reg, vec![seed("s")], budget(20), |t| t).await;
    let rrf = walk(&reg, vec![seed("s")], budget(20), |t| t.fusion(Fusion::Rrf)).await;
    let shape = |r: &TraversalResult| {
        r.states
            .iter()
            .map(|s| {
                (
                    s.node.clone(),
                    s.parent,
                    s.depth,
                    s.potential,
                    s.maps.clone(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&boost), shape(&rrf));
    assert_eq!(boost.popped, rrf.popped);
    assert_eq!(boost.totals.hops, rrf.totals.hops);
    // The default is exactly what it was: no new key in the start event.
    assert_eq!(boost.events[0]["rank"], json!({"corroboration_boost": 0.1}));
    assert_eq!(boost.fusion, Fusion::Boost);
    assert!(boost.fused.is_empty());
    assert!(boost.states.iter().all(|s| s.reach.is_empty()));
    assert_eq!(
        rrf.events[0]["rank"],
        json!({"corroboration_boost": 0.1, "fusion": "rrf", "rrf_k": 2.0})
    );
    // Asking for the default explicitly changes nothing, event for event.
    let explicit = walk(&reg, vec![seed("s")], budget(20), |t| {
        t.fusion(Fusion::Boost)
    })
    .await;
    assert_eq!(boost.events, explicit.events);
    assert_eq!(boost.states, explicit.states);
}

fn two_maps_one_node_for_fusion() -> Registry<'static> {
    registry(vec![
        GraphMap::new("m1", &[("s", "a"), ("a", "z"), ("a", "y")]),
        GraphMap::new("m2", &[("s", "b"), ("b", "z")]),
    ])
}

#[tokio::test]
async fn a_seeds_own_map_is_a_source_and_the_start_node_is_no_result() {
    let reg = registry(vec![GraphMap::new("m1", &[("s1", "a")])]);
    let seeds = vec![Seed {
        node: node("s1"),
        map: Some("seeds.hybrid"),
    }];
    let result = walk(&reg, seeds.clone(), budget(20), |t| t.fusion(Fusion::Rrf)).await;
    assert_eq!(
        state(&result, "s1").reach.get("seeds.hybrid"),
        Some(&1.0),
        "the seed map delivered the start node"
    );
    assert_eq!(names(&result), ["src/a.rs"]);
    assert_eq!(
        state(&result, "a")
            .reach
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        ["m1"]
    );
    let boost = walk(&reg, seeds, budget(20), |t| t).await;
    assert!(boost.states.iter().all(|s| s.reach.is_empty()));
}

#[tokio::test]
async fn the_score_bound_is_off_under_rrf() {
    let reg = registry(vec![GraphMap::new(
        "g",
        &[("s", "a"), ("s", "b"), ("s", "c"), ("a", "d"), ("b", "e")],
    )]);
    let bound = walk(&reg, vec![seed("s")], budget(100), |t| t.top_k(2, 1)).await;
    assert_eq!(bound.stop_reason, StopReason::Bound);
    let rrf = walk(&reg, vec![seed("s")], budget(100), |t| {
        t.top_k(2, 1).fusion(Fusion::Rrf)
    })
    .await;
    assert_eq!(rrf.stop_reason, StopReason::Complete);
    assert_eq!(rrf.totals.pops, 6, "every node is expanded");
    assert!(events_of(&rrf, "bound").is_empty());
}

#[tokio::test]
async fn a_handle_keeps_the_fusion_it_started_with() {
    let reg = two_maps_one_node_for_fusion();
    let straight = walk(&reg, vec![seed("s")], budget(20), |t| t.fusion(Fusion::Rrf)).await;
    let first = walk(&reg, vec![seed("s")], budget(2), |t| t.fusion(Fusion::Rrf)).await;
    let snapshot = first.snapshot.expect("the walk paused");
    assert_eq!(snapshot.fusion, Fusion::Rrf);
    let json = snapshot.to_json();
    assert_eq!(json["fusion"], "rrf");
    assert!(
        json["states"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.get("reach").is_some())
    );
    assert_eq!(Snapshot::from_json(&json).unwrap(), snapshot);
    // The resume is made with the default fusion and still ranks by rrf.
    let second = resume(&reg, snapshot, budget(20), |t| t).await;
    assert_eq!(second.fusion, Fusion::Rrf);
    assert_eq!(events_of(&second, "resume")[0]["rank"]["fusion"], "rrf");
    assert_eq!(scores(&second), scores(&straight));
    assert_eq!(second.states, straight.states);

    // A boost handle has neither key, as before.
    let boosted = walk(&reg, vec![seed("s")], budget(2), |t| t).await;
    let snapshot = boosted.snapshot.expect("the walk paused");
    let json = snapshot.to_json();
    assert!(json.get("fusion").is_none());
    assert!(
        json["states"]
            .as_array()
            .unwrap()
            .iter()
            .all(|s| s.get("reach").is_none())
    );
    assert_eq!(Snapshot::from_json(&json).unwrap().fusion, Fusion::Boost);
}

#[tokio::test]
async fn rrf_runs_are_deterministic() {
    let reg = two_maps_one_node_for_fusion();
    let a = rrf_walk(&reg, 20, |t| t).await;
    let b = rrf_walk(&reg, 20, |t| t).await;
    assert_eq!(a.events, b.events);
    assert_eq!(scores(&a), scores(&b));
}

// ---- capped log-odds (`fusion = "log_odds"`) --------------------------------------------

/// `ln(p / (1 - p))`, the formula written out for the expected numbers.
fn logit(p: f64) -> f64 {
    (p / (1.0 - p)).ln()
}

const CAP: f64 = fusion::LOG_ODDS_CAP;

#[test]
fn log_odds_one_source_ranks_by_potential_and_the_cap_bounds_each_term() {
    assert_eq!(CAP, 3.0);
    let nodes = [node("a"), node("b"), node("c"), node("d"), node("e")];
    // Potentials 0.9, 0.5, 0.1, 0.99 (logit 4.6, capped to 3), 0 (capped to -3).
    let rows: [&[(MapId, f64, f64)]; 5] = [
        &[("m", 1.0, 0.9)],
        &[("m", 1.0, 0.5)],
        &[("m", 1.0, 0.1)],
        &[("m", 1.0, 0.99)],
        &[("m", 1.0, 0.0)],
    ];
    let got = fusion::log_odds(&items(&nodes, &rows), |_| false);
    let want = [logit(0.9), 0.0, logit(0.1), CAP, -CAP];
    for (g, w) in got.iter().zip(want) {
        assert!(close(*g, w), "{got:?} against {want:?}");
    }
    // Monotone in the potential: more potential never scores less.
    let mut last = f64::NEG_INFINITY;
    for p in [0.0, 1e-9, 1e-3, 0.05, 0.2, 0.5, 0.8, 0.95, 0.999, 1.0] {
        let nodes = [node("x")];
        let rows: [&[(MapId, f64, f64)]; 1] = [&[("m", 1.0, p)]];
        let s = fusion::log_odds(&items(&nodes, &rows), |_| false)[0];
        assert!(s >= last, "p {p}: {s} fell below {last}");
        assert!((-CAP..=CAP).contains(&s), "p {p}: {s} beyond the cap");
        last = s;
    }
    // The weight scales the term, and the cap with it.
    let rows: [&[(MapId, f64, f64)]; 1] = [&[("m", 0.2, 0.99)]];
    let nodes = [node("x")];
    assert!(close(
        fusion::log_odds(&items(&nodes, &rows), |_| false)[0],
        0.2 * CAP
    ));
}

#[test]
fn log_odds_a_silent_source_costs_the_cap() {
    // `x` is delivered by A only, `y` by B only, both at 0.5 (logit 0).
    let nodes = [node("x"), node("y"), node("both")];
    let rows: [&[(MapId, f64, f64)]; 3] = [
        &[("A", 1.0, 0.5)],
        &[("B", 1.0, 0.5)],
        &[("A", 1.0, 0.5), ("B", 1.0, 0.5)],
    ];
    let got = fusion::log_odds(&items(&nodes, &rows), |_| false);
    assert!(close(got[0], -CAP) && close(got[1], -CAP), "{got:?}");
    assert!(close(got[2], 0.0), "{got:?}");
    // The silence costs the source's weight times the cap.
    let rows: [&[(MapId, f64, f64)]; 3] = [
        &[("A", 1.0, 0.5)],
        &[("B", 0.5, 0.5)],
        &[("A", 1.0, 0.5), ("B", 0.5, 0.5)],
    ];
    let got = fusion::log_odds(&items(&nodes, &rows), |_| false);
    assert!(close(got[0], -0.5 * CAP), "{got:?}");
    assert!(close(got[1], -CAP), "{got:?}");

    // One source at 0.9 with one silent beats two at 0.25 (the plan's example).
    let nodes = [node("strong"), node("weak")];
    let rows: [&[(MapId, f64, f64)]; 2] =
        [&[("A", 1.0, 0.9)], &[("A", 1.0, 0.25), ("B", 1.0, 0.25)]];
    let got = fusion::log_odds(&items(&nodes, &rows), |_| false);
    assert!(close(got[0], logit(0.9) - CAP), "{got:?}");
    assert!(close(got[1], 2.0 * logit(0.25)), "{got:?}");
    assert!(got[0] > got[1]);
}

#[test]
fn log_odds_two_agreeing_sources_outrank_one_at_the_same_potential() {
    let nodes = [node("one"), node("two")];
    let rows: [&[(MapId, f64, f64)]; 2] = [&[("A", 1.0, 0.8)], &[("A", 1.0, 0.8), ("B", 1.0, 0.8)]];
    let got = fusion::log_odds(&items(&nodes, &rows), |_| false);
    assert!(close(got[0], logit(0.8) - CAP), "{got:?}");
    assert!(close(got[1], 2.0 * logit(0.8)), "{got:?}");
    assert!(got[1] > got[0]);
}

#[test]
fn log_odds_a_flood_of_low_weight_deliveries_is_capped() {
    // `t` is delivered by A and B at 0.6 (and by F at 0.1); twenty nodes are
    // delivered only by the flooding source F (weight 0.1) at a high potential.
    let mut nodes: Vec<NodeId> = vec![node("t")];
    nodes.extend((0..20).map(|i| node(&format!("f{i:02}"))));
    let mut rows: Vec<Vec<(MapId, f64, f64)>> =
        vec![vec![("A", 1.0, 0.6), ("B", 1.0, 0.6), ("F", 0.1, 0.1)]];
    rows.extend((0..20).map(|_| vec![("F", 0.1, 0.99)]));
    let slices: Vec<&[(MapId, f64, f64)]> = rows.iter().map(Vec::as_slice).collect();
    let got = fusion::log_odds(&items(&nodes, &slices), |_| false);
    assert!(
        close(got[0], 2.0 * logit(0.6) + 0.1 * logit(0.1)),
        "{got:?}"
    );
    // Each flood node: F's term at the cap, and A and B silent.
    for score in &got[1..] {
        assert!(close(*score, 0.1 * CAP - 2.0 * CAP), "{got:?}");
    }
    assert!(got[1..].iter().all(|s| *s < got[0]), "{got:?}");
}

#[test]
fn log_odds_dependent_maps_are_one_source_with_the_largest_weight() {
    let nodes = [node("z"), node("y"), node("w")];
    let rows: [&[(MapId, f64, f64)]; 3] = [
        &[("d1", 0.5, 0.9), ("d2", 1.0, 0.4)],
        &[("i", 1.0, 0.9)],
        &[("i", 1.0, 0.5)],
    ];
    let got = fusion::log_odds(&items(&nodes, &rows), |m| m.starts_with('d'));
    // z: the dependent source once, at its best potential 0.9 and weight 1;
    // `i` is silent on it. y: `i` at 0.9, the dependent source silent.
    assert!(close(got[0], logit(0.9) - CAP), "{got:?}");
    assert!(close(got[1], logit(0.9) - CAP), "{got:?}");
    // w: `i` at 0.5 (0), the dependent source silent at weight 1, not 0.5.
    assert!(close(got[2], -CAP), "{got:?}");
}

#[test]
fn log_odds_equal_inputs_give_bit_equal_scores() {
    let rows: [&[(MapId, f64, f64)]; 2] = [&[("A", 1.0, 0.7)], &[("B", 1.0, 0.7)]];
    let nodes = [node("a"), node("b")];
    let got = fusion::log_odds(&items(&nodes, &rows), |_| false);
    assert_eq!(got[0].to_bits(), got[1].to_bits(), "{got:?}");
}

#[tokio::test]
async fn the_walk_ranks_by_log_odds_when_selected() {
    let reg = registry(vec![GraphMap::new(
        "m1",
        &[("s", "c"), ("s", "a"), ("s", "b")],
    )]);
    let result = fusion_walk(&reg, 20, Fusion::LogOdds, |t| t).await;
    assert_eq!(result.fusion, Fusion::LogOdds);
    // The map offers a, b, c in node order: potentials 1, 1/2, 1/3.
    assert_eq!(names(&result), ["src/a.rs", "src/b.rs", "src/c.rs"]);
    let got: Vec<f64> = scores(&result).into_iter().map(|(_, s)| s).collect();
    let want = [CAP, 0.0, logit(1.0 / 3.0)];
    for (g, w) in got.iter().zip(want) {
        assert!(close(*g, w), "{got:?} against {want:?}");
    }
    // The fused map holds every result and not the start node.
    assert_eq!(result.fused.len(), 3);
    assert!(!result.fused.contains_key(&node("s")));
    // The cards carry the fused score, and the start event names the cap.
    let b = state(&result, "b");
    let card = stop::Card::ranked(b, result.ranked_potential(b));
    assert!(close(
        trace::card_json(&card)["ranked_potential"]
            .as_f64()
            .unwrap(),
        0.0
    ));
    assert_eq!(
        result.events[0]["rank"],
        json!({"corroboration_boost": 0.1, "fusion": "log_odds", "cap": 3.0})
    );
    // The default keeps its own key and no fused map.
    let boost = walk(&reg, vec![seed("s")], budget(20), |t| t.price(RANKED)).await;
    assert_eq!(boost.events[0]["rank"], json!({"corroboration_boost": 0.1}));
    assert!(boost.fused.is_empty());
}

#[tokio::test]
async fn log_odds_orders_equal_scores_by_node_id_and_counts_maps_as_sources() {
    // `b` and `a` are each offered by one map at the same potential, each
    // silent in the other map: equal scores, ordered by node id.
    let reg = registry(vec![
        GraphMap::new("m1", &[("s", "b")]),
        GraphMap::new("m2", &[("s", "a")]),
    ]);
    // The default price follows every edge whole (potential 1), so the two tie.
    let result = walk(&reg, vec![seed("s")], budget(20), |t| {
        t.fusion(Fusion::LogOdds)
    })
    .await;
    let got = scores(&result);
    assert_eq!(names(&result), ["src/a.rs", "src/b.rs"]);
    assert!(close(got[0].1, got[1].1), "{got:?}");
    assert!(close(got[0].1, CAP - CAP), "{got:?}");
    // A node two independent maps deliver outranks one delivered by one map.
    let reg = registry(vec![
        GraphMap::new("m1", &[("s", "z")]),
        GraphMap::new("m2", &[("s", "z")]),
        GraphMap::new("m3", &[("s", "y")]),
    ]);
    let result = fusion_walk(&reg, 20, Fusion::LogOdds, |t| t).await;
    assert_eq!(names(&result), ["src/z.rs", "src/y.rs"]);
}

#[tokio::test]
async fn the_score_bound_is_off_under_log_odds_and_the_walk_is_the_same() {
    let reg = registry(vec![GraphMap::new(
        "g",
        &[("s", "a"), ("s", "b"), ("s", "c"), ("a", "d"), ("b", "e")],
    )]);
    let log_odds = walk(&reg, vec![seed("s")], budget(100), |t| {
        t.top_k(2, 1).fusion(Fusion::LogOdds)
    })
    .await;
    assert_eq!(log_odds.stop_reason, StopReason::Complete);
    assert_eq!(log_odds.totals.pops, 6, "every node is expanded");
    assert!(events_of(&log_odds, "bound").is_empty());
    // The same states as `boost` reaches.
    let reg = two_maps_one_node_for_fusion();
    let shape = |r: &TraversalResult| {
        r.states
            .iter()
            .map(|s| (s.node.clone(), s.parent, s.depth, s.potential))
            .collect::<Vec<_>>()
    };
    let boost = walk(&reg, vec![seed("s")], budget(20), |t| t).await;
    let log_odds = walk(&reg, vec![seed("s")], budget(20), |t| {
        t.fusion(Fusion::LogOdds)
    })
    .await;
    assert_eq!(shape(&boost), shape(&log_odds));
    assert_eq!(boost.popped, log_odds.popped);
}

#[tokio::test]
async fn a_handle_keeps_the_log_odds_fusion_it_started_with() {
    let reg = two_maps_one_node_for_fusion();
    let straight = walk(&reg, vec![seed("s")], budget(20), |t| {
        t.fusion(Fusion::LogOdds)
    })
    .await;
    let first = walk(&reg, vec![seed("s")], budget(2), |t| {
        t.fusion(Fusion::LogOdds)
    })
    .await;
    let snapshot = first.snapshot.expect("the walk paused");
    assert_eq!(snapshot.fusion, Fusion::LogOdds);
    let json = snapshot.to_json();
    assert_eq!(json["fusion"], "log_odds");
    assert!(
        json["states"]
            .as_array()
            .unwrap()
            .iter()
            .any(|s| s.get("reach").is_some())
    );
    assert_eq!(Snapshot::from_json(&json).unwrap(), snapshot);
    // The resume is made with the default fusion and still ranks by log-odds.
    let second = resume(&reg, snapshot, budget(20), |t| t).await;
    assert_eq!(second.fusion, Fusion::LogOdds);
    assert_eq!(
        events_of(&second, "resume")[0]["rank"]["fusion"],
        "log_odds"
    );
    assert_eq!(scores(&second), scores(&straight));
    assert_eq!(second.states, straight.states);
}
