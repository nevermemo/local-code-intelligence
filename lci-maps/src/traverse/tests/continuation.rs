//! Continuation and depth semantics (slice F2): relaxing, depth blocking,
//! duplicate frontier entries, what a handle holds, and what a cut pop has
//! already paid.

use super::*;
use crate::traverse::{handle::Snapshot, steer::Steer};
use serde_json::json;

// ---- R7a: a relaxed state may have a parent that comes later ------------------

/// `s1` offers nine files with `zx` last (hop value 1/9); `s2` offers `y`,
/// which offers `zx` at value 1 and so relaxes it: its parent `y` is a state
/// created after it.
fn relax_graph() -> GraphMap {
    let mut map = GraphMap::new("g", &[("s2", "y"), ("y", "zx")]);
    nine(
        &mut map,
        "s1",
        ["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "zx"],
    );
    map
}

#[tokio::test]
async fn r7a_a_relaxed_state_with_a_later_parent_still_makes_a_readable_handle() {
    let reg = registry(vec![relax_graph()]);
    let result = walk(&reg, vec![seed("s1"), seed("s2")], budget(4), |t| t).await;
    let (index, zx) = state_of(&result, "zx");
    assert!(
        zx.parent.unwrap() > index,
        "the relax gave it a later parent"
    );
    assert_eq!(events_of(&result, "relax").len(), 1);
    let snapshot = result.snapshot.expect("work is left");
    let back = Snapshot::from_json(&snapshot.to_json()).expect("the handle reads back");
    assert_eq!(back, snapshot);
}

// ---- R7b: a relaxed node's children follow it ---------------------------------

/// `s` offers nine files: `ay` first (rank 0, value 1), `x` fifth (rank 4,
/// value 1/5). `ay` offers `x`; `x` offers `k1..k4`.
fn follow_graph() -> GraphMap {
    let mut map = GraphMap::new(
        "g",
        &[
            ("ay", "x"),
            ("x", "k1"),
            ("x", "k2"),
            ("x", "k3"),
            ("x", "k4"),
        ],
    );
    nine(
        &mut map,
        "s",
        ["ay", "b0", "b1", "b2", "x", "z0", "z1", "z2", "z3"],
    );
    map
}

#[tokio::test]
async fn r7b_the_children_of_a_cut_node_follow_it_when_it_is_relaxed() {
    let reg = registry(vec![follow_graph()]);
    // Call 1: `x` is boosted so it pops second, before `ay`, and the tokens
    // afford two of its four children: the pop is cut and put back.
    let mut tight = budget(100);
    tight.est_tokens = 9 * TOKENS_PER_EDGE + 2 * TOKENS_PER_EDGE;
    let x = node("x");
    let first = walk(&reg, vec![seed("s")], tight, |t| {
        t.handler(
            scripted(move |cp| {
                if cp.stop == 1 {
                    vec![act(Some(&x), Action::Boost(8.0))]
                } else {
                    Vec::new()
                }
            }),
            every(1),
        )
    })
    .await;
    assert_eq!(first.status, TraversalStatus::BudgetExhausted);
    let (_, k1) = state_of(&first, "k1");
    assert!((k1.potential - 0.2).abs() < 1e-12, "{}", k1.potential);
    let snapshot = first.snapshot.unwrap();

    // Call 2: the agent expands `ay` and then `k1` before `x` is retried. `ay`
    // reaches `x` at value 1 and relaxes it; `k1` is popped under it.
    let steer = Steer::new(vec![
        act(Some(&node("ay")), Action::Expand),
        act(Some(&node("k1")), Action::Expand),
    ]);
    let second = resume(&reg, snapshot, budget(100), |t| t.steer(steer)).await;
    assert_eq!(second.status, TraversalStatus::Complete);
    let (_, x_state) = state_of(&second, "x");
    assert!((x_state.potential - 1.0).abs() < 1e-12);
    assert_eq!(x_state.depth, 2);
    for name in ["k1", "k2", "k3", "k4"] {
        let (_, k) = state_of(&second, name);
        assert!(
            (k.potential - 1.0).abs() < 1e-9,
            "{name} keeps the potential of the route through x: {}",
            k.potential
        );
        assert_eq!(k.depth, 3, "{name}");
    }
    let relax = events_of(&second, "relax");
    assert_eq!(relax.len(), 1, "{relax:?}");
    assert_eq!(relax[0]["dst"], node("x").to_string());
    assert_eq!(relax[0]["rescaled"], 2, "k1 and k2 had been adopted");
}

// ---- R7c: alt_path events survive a resume ---------------------------------------

#[tokio::test]
async fn r7c_a_resume_does_not_repeat_an_alt_path() {
    // `q` offers the popped `p` (an alt_path) and three fresh nodes; the
    // tokens afford one, so the pop is cut after the alt_path was traced.
    let reg = registry(vec![GraphMap::new(
        "g",
        &[
            ("s", "p"),
            ("s", "q"),
            ("q", "k1"),
            ("q", "k2"),
            ("q", "k3"),
            ("q", "p"),
        ],
    )]);
    let mut tight = budget(100);
    tight.est_tokens = 3 * TOKENS_PER_EDGE;
    let first = walk(&reg, vec![seed("s")], tight, |t| t).await;
    assert_eq!(first.status, TraversalStatus::BudgetExhausted);
    let snapshot = first.snapshot.clone().unwrap();
    let second = resume(&reg, snapshot, budget(100), |t| t).await;
    assert_eq!(second.status, TraversalStatus::Complete);
    let to = |result: &TraversalResult, dst: &str| {
        events_of(result, "alt_path")
            .into_iter()
            .filter(|e| e["dst"] == node(dst).to_string())
            .count()
    };
    assert_eq!(to(&first, "p") + to(&second, "p"), 1, "p, once");
    // The retry's own route to its first child is not an alternative.
    assert_eq!(to(&first, "k1") + to(&second, "k1"), 0);
}

// ---- R8: depth ----------------------------------------------------------------------

/// `a` offers `aa` (rank 0, value 1) and `zb` (rank 8, value 1/9) among nine;
/// `aa -> n1 -> zq` reaches `zq` at depth 3, which is `max_depth`; `zb -> zq`
/// is a shallower (depth 2) but weaker route; `zq -> w`.
fn blocked_graph() -> GraphMap {
    let mut map = GraphMap::new(
        "g",
        &[("aa", "n1"), ("n1", "zq"), ("zb", "zq"), ("zq", "w")],
    );
    nine(
        &mut map,
        "a",
        ["aa", "f0", "f1", "f2", "f3", "f4", "f5", "f6", "zb"],
    );
    map
}

#[tokio::test]
async fn r8_a_shallower_but_weaker_route_queues_a_depth_blocked_node() {
    let reg = registry(vec![blocked_graph()]);
    let mut limits = budget(100);
    limits.max_depth = 3;
    let result = walk(&reg, vec![seed("a")], limits, |t| t).await;
    let (_, zq) = state_of(&result, "zq");
    assert_eq!((zq.depth, zq.via), (2, Some("g")));
    assert!(zq.potential < 0.2, "the weaker route: {}", zq.potential);
    assert!(
        result.reached(&node("w")),
        "zq was expanded: {:?}",
        popped_names(&result)
    );
    let relax = events_of(&result, "relax");
    assert_eq!(relax.len(), 1);
    assert_eq!(relax[0]["unblocked"], true);
}

#[tokio::test]
async fn r8_a_resume_enforces_the_depth_of_its_own_call() {
    let reg = registry(vec![GraphMap::new(
        "g",
        &[("a", "b"), ("b", "c"), ("c", "d")],
    )]);
    let first = walk(&reg, vec![seed("a")], budget(2), |t| t).await;
    assert!(first.reached(&node("c")));
    let snapshot = first.snapshot.unwrap();
    // `c` is queued at depth 2; this call allows no state at depth 2 to expand.
    let mut shallow = budget(100);
    shallow.max_depth = 2;
    let second = resume(&reg, snapshot, shallow, |t| t).await;
    assert!(!second.reached(&node("d")));
    assert_eq!(second.totals.pops, 2, "the blocked entry is not a pop");
    assert_eq!(second.status, TraversalStatus::Complete);
}

#[tokio::test]
async fn r8_a_resume_with_a_deeper_limit_expands_the_states_the_first_call_blocked() {
    let reg = registry(vec![GraphMap::new(
        "g",
        &[("a", "b"), ("a", "x"), ("b", "c"), ("c", "d")],
    )]);
    let mut shallow = budget(2);
    shallow.max_depth = 2;
    let first = walk(&reg, vec![seed("a")], shallow, |t| t).await;
    assert!(first.reached(&node("c")) && !first.reached(&node("d")));
    let snapshot = first.snapshot.unwrap();
    let mut deeper = budget(100);
    deeper.max_depth = 4;
    let second = resume(&reg, snapshot, deeper, |t| t).await;
    assert!(second.reached(&node("d")), "c was queued by the new limit");
}

// ---- R9: duplicates and expanded entries in the frontier --------------------------------

async fn cut_snapshot() -> (Registry<'static>, Snapshot) {
    let reg = registry(vec![GraphMap::new("g", &[("a", "b"), ("a", "c")])]);
    let first = walk(&reg, vec![seed("a")], budget(1), |t| t).await;
    let snapshot = first.snapshot.unwrap();
    (reg, snapshot)
}

#[tokio::test]
async fn r7a_r9_a_handle_with_a_parent_cycle_a_parent_out_of_range_or_a_bad_queue_is_refused() {
    let (_, snapshot) = cut_snapshot().await;
    let base = snapshot.to_json();
    // States 1 and 2 are the children of state 0.
    let parents = |one: Value, two: Value| {
        let mut value = base.clone();
        value["states"][1]["parent"] = one;
        value["states"][2]["parent"] = two;
        value
    };
    let queued_twice = {
        let mut value = base.clone();
        let first = value["frontier"][0].clone();
        value["frontier"].as_array_mut().unwrap().push(first);
        value
    };
    let queued_expanded = {
        let mut value = base.clone();
        let queued = value["frontier"][0][0].clone();
        value["expanded"].as_array_mut().unwrap().push(queued);
        value
    };
    for (what, value, wanted) in [
        ("a parent cycle", parents(json!(2), json!(1)), "cycle"),
        (
            "a state its own parent",
            parents(json!(1), json!(0)),
            "cycle",
        ),
        (
            "a parent out of range",
            parents(json!(99), json!(0)),
            "range",
        ),
        ("a state queued twice", queued_twice, "twice"),
        ("an expanded state queued", queued_expanded, "expanded"),
    ] {
        let err = Snapshot::from_json(&value).unwrap_err();
        assert!(err.contains(wanted), "{what}: {err}");
    }
    // A parent that comes later is fine when it leads to a root.
    assert!(Snapshot::from_json(&parents(json!(2), json!(0))).is_ok());
}

#[tokio::test]
async fn r9_a_pop_skips_a_queue_entry_that_is_already_expanded() {
    let (reg, mut snapshot) = cut_snapshot().await;
    // Built by hand, past the checks of a stored handle: the seed (state 0)
    // was expanded and is queued again.
    assert!(snapshot.expanded.contains(&0));
    let seq = snapshot.next_seq;
    snapshot.frontier.push((0, 1.0, seq));
    snapshot.next_seq += 1;
    let second = resume(&reg, snapshot, budget(100), |t| t).await;
    let expansions = second.popped.iter().filter(|&&s| s == 0).count();
    assert_eq!(
        expansions, 1,
        "the seed was expanded once: {:?}",
        second.popped
    );
    assert_eq!(second.totals.pops, 3);
}

#[tokio::test]
async fn r9_redirect_targets_are_counted_once() {
    let (reg, snapshot) = cut_snapshot().await;
    let z = node("z");
    let steer = Steer::new(vec![act(
        None,
        Action::Redirect(vec![z.clone(), z.clone()]),
    )]);
    let second = resume(&reg, snapshot, budget(100), |t| t.steer(steer)).await;
    let (_, state) = state_of(&second, "z");
    assert_eq!(state.fanout, 1);
    assert_eq!(
        second
            .popped
            .iter()
            .filter(|&&s| second.states[s].node == z)
            .count(),
        1
    );
}

// ---- R10: what a handle holds ---------------------------------------------------------

#[tokio::test]
async fn r10_a_handle_does_not_hold_the_event_log() {
    let (reg, snapshot) = cut_snapshot().await;
    assert!(snapshot.to_json().get("events").is_none());
    // The resumed call writes its own trace, from its `resume` event.
    let second = resume(&reg, snapshot.clone(), budget(100), |t| t).await;
    assert_eq!(second.events[0]["ev"], "resume");
    assert_eq!(second.events[0]["handle"], snapshot.handle());
    assert!(events_of(&second, "start").is_empty());
}

// ---- F1 follow-ups ---------------------------------------------------------------------

#[tokio::test]
async fn f1_a_spent_external_allowance_skips_external_maps_and_structure_goes_on() {
    let runs = Arc::new(AtomicU32::new(0));
    let reg = registry(vec![
        GraphMap::new("g1", &[("a", "b"), ("b", "c"), ("c", "d")]),
        GraphMap::new("g2", &[("a", "x"), ("b", "y"), ("c", "z")])
            .cost(0, 1)
            .counted(&runs),
    ]);
    let mut limits = budget(100);
    limits.external_calls = 1;
    let result = walk(&reg, vec![seed("a")], limits, |t| t).await;
    assert_eq!(
        runs.load(Ordering::Relaxed),
        1,
        "one call, at the first pop"
    );
    assert_eq!(result.totals.external_calls, 1);
    assert!(result.reached(&node("x")));
    assert!(result.reached(&node("d")), "the structural map went on");
    assert!(!result.reached(&node("y")) && !result.reached(&node("z")));
    assert_eq!(result.status, TraversalStatus::Complete);
    assert!(
        result.events.iter().any(|e| e["external_spent"] == true),
        "the trace says the maps were given no call"
    );
}

#[tokio::test]
async fn f1_a_cut_pop_is_requeued_and_its_retry_does_not_pay_its_calls_again() {
    // Each expansion costs 10 tokens and one external call. The first call
    // cuts the seed's pop after one hop; the retry runs the call again but
    // owes nothing for it.
    let reg = registry(vec![
        GraphMap::new(
            "g",
            &[("a", "b"), ("a", "c"), ("a", "d"), ("a", "e"), ("a", "h")],
        )
        .cost(10, 1),
    ]);
    let tight = TraversalBudget {
        est_tokens: 30,
        ..budget(10)
    };
    let first = walk(&reg, vec![seed("a")], tight, |t| t).await;
    assert_eq!(first.status, TraversalStatus::BudgetExhausted);
    assert_eq!(first.totals.external_calls, 1);
    assert_eq!(
        (
            first.totals.physical_map_calls,
            first.totals.physical_external_calls
        ),
        (1, 1)
    );
    let snapshot = first.snapshot.unwrap();
    // The handle says what the cut pop already paid: the seed (state 0), 10
    // tokens, 1 call; and it reads back.
    assert_eq!(snapshot.paid, [(0, 10, 1)]);
    // The cut pop is requeued, not expanded.
    assert!(snapshot.expanded.is_empty());
    assert!(
        snapshot
            .frontier
            .iter()
            .any(|(s, _, _)| snapshot.states[*s].node == node("a")),
        "the cut seed is queued again"
    );
    let back = Snapshot::from_json(&snapshot.to_json()).unwrap();
    assert_eq!(back.paid, snapshot.paid);
    let second = resume(&reg, snapshot, budget(10), |t| t).await;
    let retry = events_of(&second, "expand")[0];
    assert_eq!(retry["paid_before"], json!([10, 1]));
    let straight = walk(&reg, vec![seed("a")], budget(10), |t| t).await;
    assert_eq!(straight.totals.external_calls, 6);
    assert_eq!(
        second.totals.external_calls, 6,
        "a, b, c, d, e, h: one call each"
    );
    // The physical work counts the retry: the seed's call ran twice.
    assert_eq!(
        (
            straight.totals.physical_map_calls,
            straight.totals.physical_external_calls
        ),
        (6, 6)
    );
    assert_eq!(
        (
            second.totals.physical_map_calls,
            second.totals.physical_external_calls
        ),
        (7, 7),
        "the first call's one plus the retry and the five other pops"
    );
    let end = &events_of(&second, "end")[0]["totals"];
    assert_eq!(end["physical_map_calls"], 7);
    assert_eq!(end["physical_external_calls"], 7);
    assert_eq!(end["external_calls"], 6);
    assert_eq!(second.totals.tokens, straight.totals.tokens);
    assert_eq!(second.totals.hops, straight.totals.hops);
    // Six pops (a and its five children) cost one call each, plus five hops,
    // whatever the cut.
    assert_eq!(straight.totals.tokens, 6 * 10 + 5 * TOKENS_PER_EDGE);
    for name in ["b", "c", "d", "e", "h"] {
        assert!(second.reached(&node(name)), "{name}");
    }
    assert!(second.snapshot.is_none());
}

// ---- R1b: what a resume must repeat -----------------------------------------------------

#[tokio::test]
async fn a_handle_refuses_another_corroboration_boost_and_warns_when_unchecked() {
    use crate::traverse::handle::HandleError;
    let (reg, snapshot) = cut_snapshot().await;
    assert_eq!(snapshot.boost, DEFAULT_CORROBORATION_BOOST);
    snapshot.check_boost(DEFAULT_CORROBORATION_BOOST).unwrap();
    // A value that is sanitised to the handle's boost matches it.
    let zero = walk(&reg, vec![seed("a")], budget(1), |t| {
        t.corroboration_boost(0.0)
    })
    .await
    .snapshot
    .unwrap();
    assert_eq!(zero.boost, 0.0);
    zero.check_boost(-1.0).unwrap();
    zero.check_boost(f64::NAN).unwrap();
    let error = snapshot.check_boost(0.5).unwrap_err();
    assert!(
        matches!(error, HandleError::WrongBoost { found, expected, .. } if found == 0.1 && expected == 0.5),
        "{error:?}"
    );
    let text = error.to_string();
    assert!(text.contains("0.5") && text.contains("0.1"), "{text}");

    // A resume that gave no boost, or the handle's, has nothing to say; one
    // that gave another and was not checked ranks with the handle's, and says so.
    for given in [None, Some(DEFAULT_CORROBORATION_BOOST)] {
        let second = resume(&reg, snapshot.clone(), budget(100), |t| match given {
            Some(boost) => t.corroboration_boost(boost),
            None => t,
        })
        .await;
        assert!(second.warnings.is_empty(), "{:?}", second.warnings);
    }
    let second = resume(&reg, snapshot.clone(), budget(100), |t| {
        t.corroboration_boost(0.5)
    })
    .await;
    assert_eq!(second.warnings.len(), 1, "{:?}", second.warnings);
    assert!(
        second.warnings[0].contains("0.5") && second.warnings[0].contains("0.1"),
        "{:?}",
        second.warnings
    );
}

#[tokio::test]
async fn a_handle_refuses_another_score_bound_k() {
    use crate::traverse::handle::HandleError;
    let reg = registry(vec![GraphMap::new("g", &[("a", "b"), ("a", "c")])]);
    let bounded = walk(&reg, vec![seed("a")], budget(1), |t| t.top_k(3, 1))
        .await
        .snapshot
        .unwrap();
    assert_eq!(bounded.top_k, 3);
    bounded.check_top_k(3).unwrap();
    assert_eq!(Snapshot::from_json(&bounded.to_json()).unwrap(), bounded);
    let error = bounded.check_top_k(5).unwrap_err();
    assert!(matches!(error, HandleError::WrongTopK { .. }), "{error:?}");
    let text = bounded.check_top_k(0).unwrap_err().to_string();
    assert!(text.contains('3') && text.contains("off"), "{text}");
    // The bound being off is a setting too.
    let (_, unbounded) = cut_snapshot().await;
    assert_eq!(unbounded.top_k, 0);
    unbounded.check_top_k(0).unwrap();
    assert!(unbounded.check_top_k(3).is_err());
}

#[tokio::test]
async fn a_switch_for_a_map_the_resume_does_not_register_is_named() {
    let both = registry(vec![
        GraphMap::new("m1", &[("a", "b"), ("b", "c")]),
        GraphMap::new("m2", &[("a", "x"), ("x", "y")]),
    ]);
    let first = walk(&both, vec![seed("a")], budget(10), |t| {
        t.handler(
            scripted(|cp| {
                if cp.stop == 1 {
                    vec![
                        act(None, Action::MapOff("m2".into())),
                        act(None, Action::Stop),
                    ]
                } else {
                    Vec::new()
                }
            }),
            every(1),
        )
    })
    .await;
    let snapshot = first.snapshot.expect("a stop leaves a handle");
    assert_eq!(snapshot.switches, [("m2".to_owned(), false)]);

    // The same maps: nothing to say.
    let same = resume(&both, snapshot.clone(), budget(10), |t| t).await;
    assert!(same.warnings.is_empty(), "{:?}", same.warnings);
    // A registry without m2: the switch has no effect, and the call says so.
    let only_m1 = registry(vec![GraphMap::new("m1", &[("a", "b"), ("b", "c")])]);
    let second = resume(&only_m1, snapshot.clone(), budget(10), |t| t).await;
    assert_eq!(second.warnings.len(), 1, "{:?}", second.warnings);
    assert!(
        second.warnings[0].contains("m2") && !second.warnings[0].contains("m1"),
        "{:?}",
        second.warnings
    );
}

#[tokio::test]
async fn a_version_10_handle_round_trips_its_new_fields() {
    let reg = registry(vec![
        GraphMap::new("g", &[("a", "b"), ("a", "c")]).cost(0, 1),
    ]);
    let first = walk(&reg, vec![seed("a")], budget(1), |t| t.top_k(3, 1)).await;
    let snapshot = first.snapshot.unwrap();
    let json = snapshot.to_json();
    assert_eq!(json["version"], 10);
    assert_eq!(json["top_k"], 3);
    for key in [
        "rerank_failure_reasons",
        "rerank_skipped",
        "physical_map_calls",
        "physical_external_calls",
    ] {
        assert!(json["totals"].get(key).is_some(), "{key}");
    }
    assert_eq!(Snapshot::from_json(&json).unwrap(), snapshot);
    // A handle of the version before is refused, not guessed at.
    let mut old = json.clone();
    old["version"] = json!(9);
    assert!(Snapshot::from_json(&old).unwrap_err().contains("version"));
    // The new totals are required.
    let mut missing = json.clone();
    missing["totals"]
        .as_object_mut()
        .unwrap()
        .remove("physical_map_calls");
    assert!(Snapshot::from_json(&missing).is_err());
}
