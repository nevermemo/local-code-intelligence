//! Re-arrival at a node that has a state (slice S6e): relax, `alt_path`,
//! corroboration and the rank it feeds, and the kind prior above the gate.

use super::*;

fn events<'a>(result: &'a TraversalResult, ev: &str) -> Vec<&'a Value> {
    result.events.iter().filter(|e| e["ev"] == ev).collect()
}

fn state<'a>(result: &'a TraversalResult, name: &str) -> &'a State {
    let wanted = node(name);
    result
        .states
        .iter()
        .find(|s| s.node == wanted)
        .unwrap_or_else(|| panic!("{name} not reached"))
}

/// Everything priced by rank: no edge is followed whole.
const RANKED: HopPrice = HopPrice { gate: 0, beam: 16 };

/// `s` offers `a` (best), `b` and `x` (worst) in that rank order; `a` then
/// leads to `x`, so `x` is found first by the weakest route and again by the
/// strongest before it is popped.
fn weak_then_strong() -> Registry<'static> {
    registry(vec![
        GraphMap::new("m1", &[("s", "a"), ("a", "x")]).conf(90),
        GraphMap::new("m2", &[("s", "b")]).conf(80),
        GraphMap::new("m3", &[("s", "x")]).conf(70),
    ])
}

#[tokio::test]
async fn a_node_found_by_a_weak_route_takes_the_strong_one_before_it_pops() {
    let reg = weak_then_strong();
    let result = walk(&reg, vec![seed("s")], budget(20), |t| t.price(RANKED)).await;
    let x = state(&result, "x");
    // s -> x alone is rank 2 of 3 (1/3); s -> a -> x is 1 * 1.
    assert_eq!(x.potential, 1.0);
    assert_eq!(x.depth, 2);
    assert_eq!(x.via, Some("m1"));
    assert_eq!(
        x.parent,
        Some(
            result
                .states
                .iter()
                .position(|s| s.node == node("a"))
                .unwrap()
        )
    );
    // It popped at its new place: right after a, before b (1/2).
    assert_eq!(
        popped_names(&result),
        ["src/s.rs", "src/a.rs", "src/x.rs", "src/b.rs"]
    );
    let relax = events(&result, "relax");
    assert_eq!(relax.len(), 1);
    assert_eq!(relax[0]["src"], "file:src/a.rs");
    assert_eq!(relax[0]["dst"], "file:src/x.rs");
    assert_eq!(relax[0]["old_src"], "file:src/s.rs");
    assert_eq!(relax[0]["potential"], 1.0);
    assert!((relax[0]["old_potential"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-12);
    // The relax is not a new hop.
    assert_eq!(result.totals.hops, 3);
    assert!(events(&result, "alt_path").is_empty());
}

#[tokio::test]
async fn a_queued_node_is_not_relaxed_past_the_depth_limit() {
    // s -> x (weak, depth 1); s -> a -> b -> x would be strong, depth 3.
    let reg = registry(vec![
        GraphMap::new("m1", &[("s", "a"), ("a", "b"), ("b", "x")]).conf(90),
        GraphMap::new("m3", &[("s", "x")]).conf(70),
    ]);
    let run = |max_depth: u32| {
        let reg = &reg;
        async move {
            let mut limits = budget(20);
            limits.max_depth = max_depth;
            walk(reg, vec![seed("s")], limits, |t| t.price(RANKED)).await
        }
    };
    let deep = run(3).await;
    assert_eq!(state(&deep, "x").depth, 1, "depth 3 is the limit: x stays");
    assert!(events(&deep, "relax").is_empty());
    assert_eq!(events(&deep, "alt_path").len(), 1);
    let roomy = run(4).await;
    assert_eq!(state(&roomy, "x").depth, 3);
    assert_eq!(events(&roomy, "relax").len(), 1);
    for result in [&deep, &roomy] {
        // Nothing is expanded at or past the limit.
        let limit = if std::ptr::eq(result, &deep) { 3 } else { 4 };
        assert!(
            result
                .popped
                .iter()
                .all(|&i| result.states[i].depth < limit)
        );
    }
}

#[tokio::test]
async fn a_popped_node_is_never_expanded_again() {
    // Every node has two ways in; the walk meets each of them again.
    let reg = registry(vec![lattice_map("g", 40)]);
    let result = walk(&reg, vec![seed("n0")], budget(100), |t| t).await;
    assert_eq!(result.status, TraversalStatus::Complete);
    let expands = events(&result, "expand");
    let mut nodes: Vec<&str> = expands
        .iter()
        .map(|e| e["node"].as_str().unwrap())
        .collect();
    assert_eq!(nodes.len(), result.totals.pops as usize);
    nodes.sort_unstable();
    nodes.dedup();
    assert_eq!(nodes.len(), expands.len(), "a node was expanded twice");
    let mut popped = result.popped.clone();
    popped.sort_unstable();
    popped.dedup();
    assert_eq!(popped.len(), result.popped.len());
    // The second ways in were seen, as a path to a popped node or a relax.
    assert!(!events(&result, "alt_path").is_empty());
}

#[tokio::test]
async fn a_cycle_is_neither_a_relax_nor_an_alt_path() {
    let reg = registry(vec![GraphMap::new(
        "g",
        &[("a", "b"), ("b", "c"), ("c", "a"), ("b", "b")],
    )]);
    let result = walk(&reg, vec![seed("a")], budget(20), |t| t).await;
    assert_eq!(result.status, TraversalStatus::Complete);
    assert_eq!(result.totals.pops, 3);
    assert!(events(&result, "relax").is_empty());
    assert!(events(&result, "alt_path").is_empty());
    assert!(result.states.iter().all(|s| s.n_maps() <= 1));
}

/// `s` reaches `a` by m1 and `b` by m2; `a` and `b` both reach `z`, `a` also
/// reaches `y`. `z` has two maps, `y` one, at the same potential.
fn two_maps_one_node() -> Registry<'static> {
    registry(vec![
        GraphMap::new("m1", &[("s", "a"), ("a", "z"), ("a", "y")]),
        GraphMap::new("m2", &[("s", "b"), ("b", "z")]),
    ])
}

#[tokio::test]
async fn two_maps_reaching_a_node_are_both_recorded_and_outrank_one() {
    let reg = two_maps_one_node();
    let result = walk(&reg, vec![seed("s")], budget(20), |t| t).await;
    let (z, y) = (state(&result, "z"), state(&result, "y"));
    assert_eq!(z.potential, y.potential);
    assert_eq!(z.maps.iter().copied().collect::<Vec<_>>(), ["m1", "m2"]);
    assert_eq!(y.maps.iter().copied().collect::<Vec<_>>(), ["m1"]);
    assert_eq!(z.via, Some("m1"), "the first route keeps the node");
    assert_eq!((z.n_maps(), y.n_maps()), (2, 1));
    assert!(result.ranked_potential(z) > result.ranked_potential(y));
    assert!((result.ranked_potential(z) - z.potential * 1.1).abs() < 1e-12);
    assert_eq!(result.corroboration_boost, DEFAULT_CORROBORATION_BOOST);
    // The other route is traced with the map it added.
    let alt = events(&result, "alt_path");
    let to_z: Vec<_> = alt.iter().filter(|e| e["dst"] == "file:src/z.rs").collect();
    assert_eq!(to_z.len(), 1);
    assert_eq!(to_z[0]["src"], "file:src/b.rs");
    assert_eq!(to_z[0]["new_maps"], json!(["m2"]));
    // A second arrival of a map the node has adds nothing.
    assert_eq!(z.witnesses.len(), 1);
    // The checkpoint cards carry the maps.
    let cards = [card_json_of(&result, "z"), card_json_of(&result, "y")];
    assert_eq!(cards[0]["n_maps"], 2);
    assert_eq!(cards[0]["maps"], json!(["m1", "m2"]));
    assert_eq!(cards[1]["n_maps"], 1);
}

fn card_json_of(result: &TraversalResult, name: &str) -> Value {
    let s = state(result, name);
    trace::card_json(&stop::Card {
        node: s.node.clone(),
        via: s.via,
        witness: s.witness,
        potential: s.potential,
        depth: s.depth,
        maps: s.maps.iter().copied().collect(),
    })
}

#[tokio::test]
async fn a_boost_of_zero_ranks_by_potential_alone() {
    let reg = two_maps_one_node();
    let default = walk(&reg, vec![seed("s")], budget(20), |t| t).await;
    let flat = walk(&reg, vec![seed("s")], budget(20), |t| {
        t.corroboration_boost(0.0)
    })
    .await;
    // The walk itself is the same; only the rank differs.
    assert_eq!(default.states, flat.states);
    assert_eq!(default.popped, flat.popped);
    let order = |result: &TraversalResult, by: &dyn Fn(&State) -> f64| {
        let mut ranked: Vec<usize> = (0..result.states.len()).collect();
        ranked.sort_by(|&a, &b| {
            by(&result.states[b])
                .total_cmp(&by(&result.states[a]))
                .then(a.cmp(&b))
        });
        ranked
    };
    let by_potential = order(&flat, &|s| s.potential);
    assert_eq!(order(&flat, &|s| flat.ranked_potential(s)), by_potential);
    assert!(
        flat.states
            .iter()
            .all(|s| flat.ranked_potential(s) == s.potential)
    );
    // The boost moved z above y, which tie on potential.
    let position = |order: &[usize], name: &str| {
        let wanted = node(name);
        order
            .iter()
            .position(|&i| flat.states[i].node == wanted)
            .unwrap()
    };
    let boosted = order(&default, &|s| default.ranked_potential(s));
    assert!(position(&by_potential, "y") < position(&by_potential, "z"));
    assert!(position(&boosted, "z") < position(&boosted, "y"));
    // The start event names the setting.
    assert_eq!(flat.events[0]["rank"]["corroboration_boost"], 0.0);
    assert_eq!(default.events[0]["rank"]["corroboration_boost"], 0.1);
    // A bad value is no boost.
    let bad = walk(&reg, vec![seed("s")], budget(20), |t| {
        t.corroboration_boost(f64::NAN)
    })
    .await;
    assert_eq!(bad.corroboration_boost, 0.0);
}

#[tokio::test]
async fn the_map_that_found_a_seed_is_not_evidence() {
    // Two seeds, both found by a seed map; the first leads to the second.
    let reg = registry(vec![GraphMap::new("m1", &[("s1", "s2"), ("s1", "c")])]);
    let seeds = vec![
        Seed {
            node: node("s1"),
            map: Some("seeds.hybrid"),
        },
        Seed {
            node: node("s2"),
            map: Some("seeds.hybrid"),
        },
    ];
    let result = walk(&reg, seeds, budget(20), |t| t).await;
    assert!(state(&result, "s1").maps.is_empty());
    assert_eq!(state(&result, "s1").via, Some("seeds.hybrid"));
    // A real map reaching a seed counts; the seed's own map does not.
    let s2 = state(&result, "s2");
    assert_eq!(s2.maps.iter().copied().collect::<Vec<_>>(), ["m1"]);
    assert_eq!(s2.via, Some("seeds.hybrid"));
    assert!(
        result
            .states
            .iter()
            .all(|s| !s.maps.contains("seeds.hybrid"))
    );
    // A seed that nothing reached ranks at its potential.
    assert_eq!(result.ranked_potential(state(&result, "s1")), 1.0);
}

#[tokio::test]
async fn budgets_and_determinism_hold_with_relax_and_corroboration() {
    let reg = registry(vec![lattice_map("g", 60), lattice_map("h", 60)]);
    let price = HopPrice { gate: 1, beam: 3 };
    for pops in [0, 1, 7, 25] {
        let run = || async { walk(&reg, vec![seed("n0")], budget(pops), |t| t.price(price)).await };
        let (a, b) = (run().await, run().await);
        assert!(a.totals.pops <= pops && a.popped.len() <= pops as usize);
        assert_eq!(a.events_jsonl(), b.events_jsonl());
        let mut seen = a.popped.clone();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), a.popped.len());
    }
    // Every node has two ways in, so the walk traced the second one.
    let run = walk(&reg, vec![seed("n0")], budget(25), |t| t.price(price)).await;
    assert!(!events(&run, "relax").is_empty() || !events(&run, "alt_path").is_empty());
    assert!(run.states.iter().skip(1).all(|s| s.n_maps() == 2));
}

#[tokio::test]
async fn corroboration_survives_a_handle() {
    let reg = two_maps_one_node();
    let result = walk(&reg, vec![seed("s")], budget(3), |t| t).await;
    let snapshot = result.snapshot.expect("a budget stop leaves a snapshot");
    let again = handle::Snapshot::from_json(&snapshot.to_json()).unwrap();
    assert_eq!(again, snapshot);
    assert!(again.states.iter().any(|s| s.n_maps() > 0));
    // A handle from before corroboration existed reads with no maps.
    let mut old = snapshot.to_json();
    for s in old["states"].as_array_mut().unwrap() {
        let s = s.as_object_mut().unwrap();
        s.remove("maps");
        s.remove("witnesses");
    }
    let old = handle::Snapshot::from_json(&old).unwrap();
    assert!(
        old.states
            .iter()
            .all(|s| s.maps.is_empty() && s.witnesses.is_empty())
    );
}

fn kinded_edge(kind: EdgeKind, dst: &str) -> Edge {
    Edge {
        kind,
        ..edge("a", dst, "m", WitnessClass::Exact, 100)
    }
}

#[test]
fn above_the_gate_a_hop_is_the_rank_value_times_its_kind_prior() {
    // Four edges of one kind each, offered by one map, above a gate of 2.
    let fused = fuse(vec![
        kinded_edge(EdgeKind::CallsOrRefs, "call"),
        kinded_edge(EdgeKind::Contains, "contains"),
        kinded_edge(EdgeKind::Mentions, "mentions"),
        kinded_edge(EdgeKind::DefinedIn, "defined"),
    ]);
    let cands: Vec<Candidate> = fused
        .iter()
        .map(|edge| Candidate {
            edge,
            in_seen: 0,
            card: MapCard::default(),
        })
        .collect();
    let ctx = ScoreCtx {
        query: &[],
        nodes_seen: 10,
    };
    let price = HopPrice { gate: 2, beam: 4 };
    let steps = hop(&mut RulesScorer, &ctx, &cands, price);
    assert_eq!(steps.len(), 4);
    // The rules scorer multiplies by the kind prior, so the order is the
    // priors': call, then the two 0.5 kinds (offer order), then mentions.
    let rank_of = |dst: &str| {
        let at = fused.iter().position(|f| f.best.dst == node(dst)).unwrap();
        steps.iter().position(|(i, _)| *i == at).unwrap()
    };
    assert_eq!(rank_of("call"), 0);
    let value_of = |dst: &str| {
        let at = fused.iter().position(|f| f.best.dst == node(dst)).unwrap();
        steps.iter().find(|(i, _)| *i == at).unwrap().1
    };
    assert_eq!(value_of("call"), 1.0);
    // A rank-1 containment edge: 0.5 * 1/2.
    let second = steps[1];
    assert_eq!(second.1, 0.5 * 0.5);
    // The weakest kind at rank 3: 0.4 * 1/4.
    assert_eq!(value_of("mentions"), 0.4 / 4.0);
    // A rank-0 containment edge is 0.5 above the gate, as under it.
    let contains_only = fuse(vec![
        kinded_edge(EdgeKind::Contains, "c1"),
        kinded_edge(EdgeKind::Contains, "c2"),
        kinded_edge(EdgeKind::Contains, "c3"),
    ]);
    let cands: Vec<Candidate> = contains_only
        .iter()
        .map(|edge| Candidate {
            edge,
            in_seen: 0,
            card: MapCard::default(),
        })
        .collect();
    let steps = hop(
        &mut RulesScorer,
        &ctx,
        &cands,
        HopPrice { gate: 1, beam: 3 },
    );
    assert_eq!(
        steps.iter().map(|s| s.1).collect::<Vec<_>>(),
        [0.5, 0.25, 0.5 / 3.0]
    );
    // The unscored arm keeps 1 for every edge.
    let steps = hop(&mut BfsScorer, &ctx, &cands, HopPrice { gate: 1, beam: 3 });
    assert!(steps.iter().all(|s| s.1 == 1.0));
}
