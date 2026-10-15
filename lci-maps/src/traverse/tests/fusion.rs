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

async fn rrf_walk(
    reg: &Registry<'static>,
    pops: u32,
    configure: impl for<'t> FnOnce(Traversal<'t>) -> Traversal<'t>,
) -> TraversalResult {
    walk(reg, vec![seed("s")], budget(pops), |t| {
        configure(t.price(RANKED).fusion(Fusion::Rrf))
    })
    .await
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
