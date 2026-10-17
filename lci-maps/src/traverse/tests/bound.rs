//! The score bound (stop reason `bound`), the stop reason every other exit
//! names, and the identity a handle is bound to.

use super::*;
use crate::traverse::handle::{HandleError, Snapshot};

/// `s` offers `a`, `b`, `c` (value 1 each); `a` and `b` offer one more each.
fn small() -> GraphMap {
    GraphMap::new(
        "g",
        &[("s", "a"), ("s", "b"), ("s", "c"), ("a", "d"), ("b", "e")],
    )
}

fn end_event(result: &TraversalResult) -> &Value {
    result.events.last().expect("a run ends with an event")
}

// ---- Q1: the score bound ------------------------------------------------------------

#[tokio::test]
async fn q1_the_walk_stops_with_bound_before_its_budget_when_nothing_can_beat_the_kth() {
    let reg = registry(vec![small()]);
    let full = walk(&reg, vec![seed("s")], budget(100), |t| t).await;
    assert_eq!(
        full.totals.pops, 6,
        "without a bound every node is expanded"
    );
    assert_eq!(full.stop_reason, StopReason::Complete);

    // After the seed's pop three results sit at potential 1 and the queue
    // holds nothing above 1: the second best cannot be beaten.
    let result = walk(&reg, vec![seed("s")], budget(100), |t| t.top_k(2, 1)).await;
    assert_eq!(result.status, TraversalStatus::Complete);
    assert_eq!(result.stop_reason, StopReason::Bound);
    assert_eq!(
        result.totals.pops, 1,
        "stopped well inside the budget of 100"
    );
    assert!(result.snapshot.is_none() && result.checkpoint.is_none());

    // The reason is in the trace: a `bound` event with the numbers, and the
    // `end` event.
    let bound = events_of(&result, "bound");
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0]["pop"], 1);
    assert_eq!(bound[0]["k"], 2);
    assert!((bound[0]["kth"].as_f64().unwrap() - 1.0).abs() < 1e-12);
    assert!((bound[0]["frontier_best"].as_f64().unwrap() - 1.0).abs() < 1e-12);
    assert!((bound[0]["below_k_best"].as_f64().unwrap() - 1.0).abs() < 1e-12);
    assert_eq!(bound[0]["maps"], 1);
    assert_eq!(bound[0]["frontier"], 3);
    assert_eq!(end_event(&result)["stop_reason"], "bound");
    assert_eq!(end_event(&result)["status"], "complete");
    // A walk that ended otherwise names its own reason.
    assert_eq!(end_event(&full)["stop_reason"], "complete");
}

#[tokio::test]
async fn q1_the_bound_does_not_hold_while_the_queue_can_beat_the_kth() {
    // `s` offers nine files: eight at value 1 and the ninth, past the beam
    // gate, at 1/9. With k = 9 the ninth result is the 9th best (1/9), so
    // the bound cannot hold while any value-1 state is queued: the eight
    // are popped first, and only the weak one is left out.
    let mut map = GraphMap::new("g", &[]);
    nine(
        &mut map,
        "s",
        ["p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8"],
    );
    let reg = registry(vec![map]);
    let result = walk(&reg, vec![seed("s")], budget(100), |t| t.top_k(9, 1)).await;
    assert_eq!(result.stop_reason, StopReason::Bound);
    assert_eq!(
        result.totals.pops, 9,
        "the seed and the eight strong states"
    );
    let bound = events_of(&result, "bound");
    assert_eq!(bound.len(), 1);
    assert_eq!(
        bound[0]["pop"], 9,
        "not at pop 1, where a value-1 state was queued"
    );
    assert_eq!(bound[0]["frontier"], 1);
    assert!(
        bound[0]["below_k_best"].is_null(),
        "exactly k results: none below"
    );
    assert!((bound[0]["kth"].as_f64().unwrap() - 1.0 / 9.0).abs() < 1e-12);
    assert!(!popped_names(&result).contains(&"src/p8.rs".to_string()));
}

#[tokio::test]
async fn q1_the_bound_holds_only_with_k_results_and_no_state_that_could_beat_the_kth() {
    let reg = registry(vec![small()]);
    // Fewer than k results never hold it; a k of 0 switches it off; with two
    // expanding maps a queued state of potential 1 could rank 1.1, above the
    // 2nd result at 1.0, so the walk goes on to the end. With the bound
    // switched off the walk runs on though it would hold.
    for (k, maps, on) in [(100, 1, true), (0, 1, true), (2, 2, true), (2, 1, false)] {
        let result = walk(&reg, vec![seed("s")], budget(100), |t| {
            t.top_k(k, maps).score_bound(on)
        })
        .await;
        assert_eq!(
            result.status,
            TraversalStatus::Complete,
            "k {k}, maps {maps}"
        );
        assert_eq!(
            result.stop_reason,
            StopReason::Complete,
            "k {k}, maps {maps}"
        );
        assert_eq!(result.totals.pops, 6, "k {k}, maps {maps}");
        assert!(events_of(&result, "bound").is_empty(), "k {k}, maps {maps}");
    }
    // A walk with targets ends when they are reached, not on the bound.
    let result = walk(&reg, vec![seed("s")], budget(100), |t| {
        t.top_k(2, 1).targets(vec![node("e")])
    })
    .await;
    assert_eq!(result.stop_reason, StopReason::Complete);
    assert!(result.reached(&node("e")));
}

#[tokio::test]
async fn q1_a_result_below_k_that_a_queued_expansion_will_corroborate_keeps_the_bound_off() {
    // Every hop is worth 0.8, the boost 0.5, two maps expand. `x` and `a` are
    // the top two (0.8); `z` and `c` sit below at 0.64 and `w` is queued at
    // 0.512. The queue alone cannot beat the 2nd (0.512 * 1.5 < 0.8), but the
    // expansion of `w` is where map `b` reaches `z`, which lifts it to
    // 0.64 * 1.5 = 0.96. The bound must wait for that.
    let reg = registry(vec![
        GraphMap::new(
            "a",
            &[("s", "x"), ("s", "a"), ("a", "z"), ("a", "c"), ("c", "w")],
        )
        .kind(EdgeKind::Imports),
        GraphMap::new("b", &[("w", "z")]).kind(EdgeKind::Imports),
    ]);
    let result = walk(&reg, vec![seed("s")], budget(100), |t| {
        t.corroboration_boost(0.5).top_k(2, 2)
    })
    .await;
    assert_eq!(result.stop_reason, StopReason::Complete);
    assert!(popped_names(&result).contains(&"src/w.rs".to_string()));
    assert!(events_of(&result, "bound").is_empty());
    let top: Vec<_> = result.ranked()[..2]
        .iter()
        .map(|&i| result.states[i].node.clone())
        .collect();
    assert!(top.contains(&node("z")), "{top:?}");
}

#[tokio::test]
async fn q1_with_a_tie_break_a_state_that_can_tie_the_kth_keeps_the_bound_off() {
    use crate::traverse::tiebreak::TieBreak;
    // `a` and `b` both rank 1 (every hop is worth 1). After `s` is popped the
    // queue holds `a` at 1 and one result exists: without a tie-break a state
    // that can only tie the 1st changes nothing, so the walk stops there.
    let reg = registry(vec![GraphMap::new("g", &[("s", "a"), ("a", "b")])]);
    let plain = walk(&reg, vec![seed("s")], budget(100), |t| t.top_k(1, 1)).await;
    assert_eq!(plain.stop_reason, StopReason::Bound);
    assert!(!plain.reached(&node("b")));

    // With a tie-break that prefers depth, `b` (tied with `a`) is the one it
    // prefers: the walk must reach it before it can stop.
    let depth = TieBreak::from_text(r#"{"version": 1, "features": {"depth": 10.0}}"#).unwrap();
    let result = walk(&reg, vec![seed("s")], budget(100), |t| {
        t.tie_break(Some(depth)).top_k(1, 1)
    })
    .await;
    assert!(result.reached(&node("b")));
    assert!(popped_names(&result).contains(&"src/a.rs".to_string()));
    assert!(events_of(&result, "bound").is_empty());
    let top = result.ranked()[0];
    assert_eq!(result.states[top].node, node("b"));
}

#[tokio::test]
async fn q1_with_a_tie_break_the_bound_still_holds_when_nothing_can_reach_the_kth() {
    use crate::traverse::tiebreak::TieBreak;
    // `s` offers `a` (a call, 1) and `b`, `c` (containment, 0.5). With k = 1
    // the 1st result is `a` at 1; once `a` is popped the queue holds `b` and
    // `c` at 0.5 and the best result below the 1st is 0.5, strictly less.
    let reg = registry(vec![
        GraphMap::new("m1", &[("s", "a")]),
        GraphMap::new("m2", &[("s", "b"), ("s", "c")]).kind(EdgeKind::Contains),
    ]);
    let depth = TieBreak::from_text(r#"{"version": 1, "features": {"depth": 10.0}}"#).unwrap();
    let result = walk(&reg, vec![seed("s")], budget(100), |t| {
        t.tie_break(Some(depth)).top_k(1, 1)
    })
    .await;
    assert_eq!(result.stop_reason, StopReason::Bound);
    assert_eq!(popped_names(&result), ["src/s.rs", "src/a.rs"]);
    let top = result.ranked()[0];
    assert_eq!(result.states[top].node, node("a"));
}

#[tokio::test]
async fn q1_a_resumed_state_counts_corroboration_from_the_maps_now_registered() {
    // Map `a` found `x` and `y` before the pause. The resume registers only
    // `b`, which offers `y` from `x`. A state with evidence from `a` alone can
    // gain `b`: two maps, not one, so `y` (potential 1) can rank 1.5 and the
    // bound must not hold at potential 1 before `x` is expanded.
    let first_map = registry(vec![GraphMap::new("a", &[("s", "x"), ("s", "y")])]);
    let first = walk(&first_map, vec![seed("s")], budget(1), |t| t).await;
    let snapshot = first.snapshot.expect("the walk paused");

    let second_map = registry(vec![GraphMap::new("b", &[("x", "y")])]);
    let result = resume(&second_map, snapshot, budget(100), |t| {
        t.corroboration_boost(0.5)
            .top_k(1, second_map.independent_expanders())
            .expanders(second_map.expanding_maps())
    })
    .await;
    assert_eq!(result.stop_reason, StopReason::Bound);
    let bound = events_of(&result, "bound");
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0]["maps"], 2, "{}", bound[0]);
    // Not at the start of the resume: `x` was expanded, where `b` reached `y`.
    assert!(popped_names(&result).contains(&"src/x.rs".to_string()));
    let top = result.ranked()[0];
    assert_eq!(result.states[top].node, node("y"));
    assert_eq!(result.states[top].n_maps(), 2);
}

// ---- every exit names its reason and the maps that ran --------------------------------

fn reason_of(result: &TraversalResult) -> (StopReason, &str) {
    (
        result.stop_reason,
        end_event(result)["stop_reason"].as_str().unwrap(),
    )
}

#[tokio::test]
async fn every_exit_path_names_its_stop_reason() {
    let chain = registry(vec![GraphMap::new("g", &[("a", "b"), ("b", "c")])]);
    let lattice = registry(vec![lattice_map("g", 60)]);

    let done = walk(&chain, vec![seed("a")], budget(10), |t| t).await;
    assert_eq!(reason_of(&done), (StopReason::Complete, "complete"));

    let capped = walk(&lattice, vec![seed("n0")], budget(3), |t| t).await;
    assert_eq!(reason_of(&capped), (StopReason::Pops, "pops"));
    assert_eq!(capped.checkpoint.as_ref().unwrap().reason, StopReason::Pops);

    let mut late = budget(50);
    late.deadline = Instant::now();
    let timed_out = walk(&lattice, vec![seed("n0")], late, |t| t).await;
    assert_eq!(reason_of(&timed_out), (StopReason::Deadline, "deadline"));
    assert_eq!(
        (timed_out.totals.pops, timed_out.status),
        (0, TraversalStatus::BudgetExhausted)
    );

    let mut dry = budget(50);
    dry.est_tokens = 0;
    let no_tokens = walk(&lattice, vec![seed("n0")], dry, |t| t).await;
    assert_eq!(reason_of(&no_tokens), (StopReason::Budget, "budget"));

    // Tokens for two of the seed's edges only: the expansion is cut.
    let mut tight = budget(50);
    tight.est_tokens = 2 * TOKENS_PER_EDGE;
    let cut = walk(&lattice, vec![seed("n0")], tight, |t| t).await;
    assert_eq!(reason_of(&cut), (StopReason::Budget, "budget"));

    let first = walk(&lattice, vec![seed("n0")], budget(3), |t| t).await;
    let steered = resume(&lattice, first.snapshot.unwrap(), budget(10), |t| {
        t.steer(Steer::new(vec![act(None, Action::Stop)]))
    })
    .await;
    assert_eq!(reason_of(&steered), (StopReason::Steer, "steer"));

    let stopped = walk(&lattice, vec![seed("n0")], budget(10), |t| {
        t.handler(scripted(|_| vec![act(None, Action::Stop)]), every(2))
    })
    .await;
    assert_eq!(reason_of(&stopped), (StopReason::EveryPops, "every_pops"));

    let bounded = walk(
        &registry(vec![small()]),
        vec![seed("s")],
        budget(100),
        |t| t.top_k(2, 1),
    )
    .await;
    assert_eq!(reason_of(&bounded), (StopReason::Bound, "bound"));
}

#[tokio::test]
async fn every_exit_path_reports_the_expansions_and_time_of_each_map() {
    let reg = registry(vec![
        GraphMap::new("slow", &[("n0", "n1"), ("n1", "n2")]).delay(Duration::from_millis(5)),
        GraphMap::new("fast", &[("n0", "n3")]),
    ]);
    let done = walk(&reg, vec![seed("n0")], budget(10), |t| t).await;
    let capped = walk(&reg, vec![seed("n0")], budget(1), |t| t).await;
    let mut late = budget(10);
    late.deadline = Instant::now();
    let timed_out = walk(&reg, vec![seed("n0")], late, |t| t).await;

    for (result, pops) in [(&done, 4), (&capped, 1)] {
        assert_eq!(result.maps["slow"].calls, pops, "{:?}", result.status);
        assert_eq!(result.maps["fast"].calls, pops);
        assert!(result.map_ms["slow"] >= 5.0 * f64::from(pops));
        assert!(result.map_ms["fast"] < result.map_ms["slow"]);
    }
    // No expansion ran, so no map did anything.
    assert!(timed_out.maps.is_empty() && timed_out.map_ms.is_empty());
    // The times are wall clock: they are in no event and not in the handle.
    assert!(!done.events_jsonl().contains("\"ms\""));
    let handle = capped.snapshot.as_ref().unwrap().to_json().to_string();
    assert!(!handle.contains("map_ms") && !handle.contains("\"ms\""));
}

// ---- R10: a handle is bound to its workspace and index -----------------------------

#[tokio::test]
async fn r10_a_handle_names_the_identity_it_was_made_for() {
    let reg = registry(vec![small()]);
    let first = walk(&reg, vec![seed("s")], budget(1), |t| t.identity("w1+i1")).await;
    let snapshot = first.snapshot.expect("work is left");
    assert_eq!(snapshot.identity(), "w1+i1");
    assert!(snapshot.check_identity("w1+i1").is_ok());
    assert!(
        snapshot.check_identity("").is_ok(),
        "a caller with none asks nothing"
    );

    // The identity is part of the handle: another one is another handle, and
    // it survives the JSON round trip.
    let back = Snapshot::from_json(&snapshot.to_json()).unwrap();
    assert_eq!(back.identity(), "w1+i1");
    let mut other = snapshot.clone();
    other.identity = "w2+i1".into();
    assert_ne!(other.handle(), snapshot.handle());

    // A different workspace, a different index, or a handle made with none
    // is refused with an error that says what happened.
    for expected in ["w2+i1", "w1+i2"] {
        let err = snapshot.check_identity(expected).unwrap_err();
        assert!(matches!(err, HandleError::WrongIdentity { .. }), "{err}");
        let text = err.to_string();
        assert!(text.contains("another workspace or index"), "{text}");
        assert!(text.contains(&snapshot.handle()), "{text}");
        assert!(text.contains("w1+i1") && text.contains(expected), "{text}");
    }
    let mut anonymous = snapshot.clone();
    anonymous.identity = String::new();
    let err = anonymous.check_identity("w1+i1").unwrap_err();
    assert!(err.to_string().contains("none"), "{err}");

    // A resume keeps the identity, and writes it into its own handle.
    let second = resume(&reg, snapshot, budget(1), |t| t).await;
    let again = second.snapshot.expect("work is left");
    assert_eq!(again.identity(), "w1+i1");
}
