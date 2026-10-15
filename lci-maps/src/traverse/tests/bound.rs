//! The score bound (stop reason `bound`) and the identity a handle is bound to.

use super::*;
use crate::traverse::handle::{HandleError, Snapshot};

async fn resume(
    reg: &Registry<'static>,
    snapshot: Snapshot,
    budget: TraversalBudget,
    configure: impl for<'t> FnOnce(Traversal<'t>) -> Traversal<'t>,
) -> TraversalResult {
    let view = view();
    configure(Traversal::resumed(
        OnTheFly::new(reg, &view),
        snapshot,
        budget,
    ))
    .run()
    .await
}

/// `parent` offers the nine names `dsts` (the ninth is above the beam gate,
/// hop value 1/9; the others are worth 1).
fn nine(map: &mut GraphMap, parent: &str, dsts: [&str; 9]) {
    map.edges
        .extend(dsts.iter().map(|d| (parent.to_string(), d.to_string())));
}

/// `s` offers `a`, `b`, `c` (value 1 each); `a` and `b` offer one more each.
fn small() -> GraphMap {
    GraphMap::new(
        "g",
        &[("s", "a"), ("s", "b"), ("s", "c"), ("a", "d"), ("b", "e")],
    )
}

fn bound_events(result: &TraversalResult) -> Vec<&Value> {
    result
        .events
        .iter()
        .filter(|e| e["ev"] == "bound")
        .collect()
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
    assert_eq!(full.stop_reason, None);

    // After the seed's pop three results sit at potential 1 and the queue
    // holds nothing above 1: the second best cannot be beaten.
    let result = walk(&reg, vec![seed("s")], budget(100), |t| t.top_k(2, 1)).await;
    assert_eq!(result.status, TraversalStatus::Complete);
    assert_eq!(result.stop_reason, Some(StopReason::Bound));
    assert_eq!(
        result.totals.pops, 1,
        "stopped well inside the budget of 100"
    );
    assert!(result.snapshot.is_none() && result.checkpoint.is_none());

    // The reason is in the trace: a `bound` event with the numbers, and the
    // `end` event.
    let bound = bound_events(&result);
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0]["pop"], 1);
    assert_eq!(bound[0]["k"], 2);
    assert!((bound[0]["kth"].as_f64().unwrap() - 1.0).abs() < 1e-12);
    assert!((bound[0]["frontier_best"].as_f64().unwrap() - 1.0).abs() < 1e-12);
    assert_eq!(bound[0]["maps"], 1);
    assert_eq!(bound[0]["frontier"], 3);
    assert_eq!(end_event(&result)["stop_reason"], "bound");
    assert_eq!(end_event(&result)["status"], "complete");
    // A walk that ended otherwise names no reason.
    assert!(end_event(&full).get("stop_reason").is_none());
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
    assert_eq!(result.stop_reason, Some(StopReason::Bound));
    assert_eq!(
        result.totals.pops, 9,
        "the seed and the eight strong states"
    );
    let bound = bound_events(&result);
    assert_eq!(bound.len(), 1);
    assert_eq!(
        bound[0]["pop"], 9,
        "not at pop 1, where a value-1 state was queued"
    );
    assert_eq!(bound[0]["frontier"], 1);
    assert!((bound[0]["kth"].as_f64().unwrap() - 1.0 / 9.0).abs() < 1e-12);
    assert!(!popped_names(&result).contains(&"src/p8.rs".to_string()));
}

#[tokio::test]
async fn q1_fewer_than_k_results_never_hold_the_bound() {
    let reg = registry(vec![small()]);
    let result = walk(&reg, vec![seed("s")], budget(100), |t| t.top_k(100, 1)).await;
    assert_eq!(result.status, TraversalStatus::Complete);
    assert_eq!(result.stop_reason, None);
    assert_eq!(result.totals.pops, 6);
    assert!(bound_events(&result).is_empty());
    // k of 0 switches it off.
    let off = walk(&reg, vec![seed("s")], budget(100), |t| t.top_k(0, 1)).await;
    assert_eq!(off.stop_reason, None);
    assert_eq!(off.totals.pops, 6);
}

#[tokio::test]
async fn q1_more_expanding_maps_raise_the_ceiling_a_queued_state_could_reach() {
    // The same walk as the first test, but two registered maps could each
    // corroborate a queued state: its potential 1 could rank 1.1, above the
    // 2nd result at 1.0, so the walk goes on to the end.
    let reg = registry(vec![small()]);
    let result = walk(&reg, vec![seed("s")], budget(100), |t| t.top_k(2, 2)).await;
    assert_eq!(result.stop_reason, None);
    assert_eq!(result.totals.pops, 6);
    assert!(bound_events(&result).is_empty());
}

#[tokio::test]
async fn q1_a_walk_with_targets_ends_when_they_are_reached_not_on_the_bound() {
    let reg = registry(vec![small()]);
    let result = walk(&reg, vec![seed("s")], budget(100), |t| {
        t.top_k(2, 1).targets(vec![node("e")])
    })
    .await;
    assert_eq!(result.stop_reason, None);
    assert!(result.reached(&node("e")));
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
