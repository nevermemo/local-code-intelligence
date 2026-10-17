//! The fitted tie-break: it orders results that tie at the same rank and
//! nothing else, and a handle refuses a file that is not the one it used.

use super::*;
use crate::traverse::{
    handle::{HandleError, Snapshot},
    tiebreak::{Facts, TieBreak},
    trace,
};

/// `top` is a call away from the seed (rank 1); `alpha`, `beta` and `gamma`
/// are containment hops (rank 0.5, tied); `delta` is one more hop (0.25).
fn graph() -> Registry<'static> {
    registry(vec![
        GraphMap::new("m1", &[("s", "top")]),
        GraphMap::new(
            "m2",
            &[
                ("s", "alpha"),
                ("s", "beta"),
                ("s", "gamma"),
                ("alpha", "delta"),
            ],
        )
        .kind(EdgeKind::Contains),
    ])
}

/// Depth is worth a lot (`delta` is the deepest), `overlap` a little.
fn tie_break(overlap: f64) -> TieBreak {
    TieBreak::from_text(&format!(
        r#"{{"version": 1, "features": {{"depth": 10.0, "overlap": {overlap}}}}}"#
    ))
    .unwrap()
}

async fn run(tie_break: Option<TieBreak>, pops: u32) -> TraversalResult {
    walk(&graph(), vec![seed("s")], budget(pops), |t| {
        t.question("beta").tie_break(tie_break).score_bound(false)
    })
    .await
}

fn names(result: &TraversalResult) -> Vec<String> {
    result
        .ranked()
        .into_iter()
        .map(|i| result.states[i].node.path().unwrap().to_string())
        .collect()
}

#[tokio::test]
async fn a_tie_break_orders_ties_only_and_leaves_the_walk_alone() {
    let plain = run(None, 20).await;
    assert_eq!(
        names(&plain),
        [
            "src/top.rs",
            "src/alpha.rs",
            "src/beta.rs",
            "src/gamma.rs",
            "src/delta.rs"
        ]
    );

    // `beta` matches the question; the tied three reorder, `delta` (deepest,
    // the most depth bonus) stays below them and `top` above.
    let broken = run(Some(tie_break(1.0)), 20).await;
    assert_eq!(
        names(&broken),
        [
            "src/top.rs",
            "src/beta.rs",
            "src/alpha.rs",
            "src/gamma.rs",
            "src/delta.rs"
        ]
    );
    // No weights, or equal scores: discovery order, as without a file.
    for tie in [
        tie_break(0.0),
        TieBreak::from_text(r#"{"version": 1, "features": {}}"#).unwrap(),
    ] {
        assert_eq!(names(&run(Some(tie), 20).await), names(&plain));
    }

    // The walk and its events are the same; only `ranked` differs.
    assert_eq!(broken.states, plain.states);
    assert_eq!(broken.popped, plain.popped);
    assert_eq!(broken.events, plain.events);
}

/// Without a tie-break the bound ends the walk once the `k`-th rank cannot be
/// beaten, ties included. With one, `alpha`, `beta` and `gamma` tie at the 2nd
/// rank (0.5) and a tied state could be the one it prefers: the walk goes on
/// past the pop where the plain walk stopped.
#[tokio::test]
async fn the_score_bound_waits_for_a_state_that_can_tie_the_kth_when_a_tie_break_is_in_use() {
    use crate::traverse::stop::StopReason;
    let bounded = |tie_break| async move {
        walk(&graph(), vec![seed("s")], budget(20), |t| {
            t.question("beta").tie_break(tie_break).top_k(2, 1)
        })
        .await
    };
    let plain = bounded(None).await;
    let broken = bounded(Some(tie_break(1.0))).await;
    assert_eq!(plain.stop_reason, StopReason::Bound);
    assert!(broken.totals.pops > plain.totals.pops);
    // Every state of the plain walk is in the longer one, with the same rank.
    assert!(broken.states.len() >= plain.states.len());
    assert!(broken.popped.starts_with(&plain.popped));
    // The tied states were expanded: the plain walk stopped before them.
    assert!(!popped_names(&plain).contains(&"src/gamma.rs".to_string()));
    assert!(popped_names(&broken).contains(&"src/gamma.rs".to_string()));
}

#[test]
fn a_handle_that_refuses_a_tie_break_says_so_on_one_line() {
    let error = HandleError::WrongTieBreak {
        handle: "h".into(),
        found: "aaa".into(),
        expected: "none".into(),
    };
    let text = error.to_string();
    assert!(!text.contains('\n') && !text.contains("  "), "{text:?}");
    assert!(text.contains("this call has none; give the same file"));
}

#[tokio::test]
async fn a_result_card_states_the_facts_the_tie_break_scores() {
    let result = run(None, 20).await;
    let beta = state(&result, "beta");
    let facts = result.facts(beta);
    assert_eq!(facts.overlap, 1.0);
    let mut card = trace::card_json(&crate::traverse::stop::Card::ranked(
        beta,
        result.ranked_potential(beta),
    ));
    for (key, value) in facts.card_fields() {
        card[key] = value;
    }
    assert_eq!(Facts::from_card(&card), Some(facts.clone()));
    // A card from before the tie-break lacks the extra fields.
    assert_eq!(
        Facts::from_card(&json!({"depth": 1, "n_maps": 1, "maps": []})),
        None
    );

    let names: Vec<_> = facts.features().into_iter().map(|(name, _)| name).collect();
    assert_eq!(
        names,
        [
            "depth",
            "n_maps",
            "maps_listed",
            "log_fanout",
            "overlap",
            "node:file",
            "witness:compiler",
            "via:m2"
        ]
    );
}

#[test]
fn a_file_is_read_checked_and_written_back() {
    for bad in [
        r#"{"version": 2, "features": {}}"#,
        r#"{"version": 1, "features": {"colour": 1.0}}"#,
        r#"{"version": 1, "features": {"witness:magic": 1.0}}"#,
        r#"{"version": 1, "features": {"node:xyz": 1.0}}"#,
        r#"{"version": 1, "features": {"depth": "high"}}"#,
        r#"{"version": 1, "features": {"depth": 1.0}, "holdout": [3]}"#,
        r#"{"version": 1}"#,
    ] {
        assert!(TieBreak::from_text(bad).is_err(), "{bad}");
    }
    let text = r#"{"version": 1, "fitted_on": "n", "holdout": ["a"],
        "features": {"depth": -0.5, "via:m1": 2.0, "witness:text": 1.0, "node:sym": 0.25}}"#;
    let tie = TieBreak::from_text(text).unwrap();
    assert_eq!(tie.hash(), trace::hash16(text.as_bytes()));
    // Written back, it reads as itself; its hash is that of the bytes written.
    let written = tie.to_json_string();
    let again = TieBreak::from_text(&written).unwrap();
    assert_eq!(
        (&again.features, &again.holdout),
        (&tie.features, &tie.holdout)
    );
    assert_eq!(again.hash(), trace::hash16(written.as_bytes()));
    assert_eq!(
        TieBreak::new(tie.features.clone(), tie.holdout.clone(), "n".into())
            .unwrap()
            .hash(),
        again.hash()
    );
}

#[tokio::test]
async fn a_handle_refuses_a_tie_break_that_is_not_the_one_it_was_made_with() {
    let first = run(Some(tie_break(1.0)), 1).await;
    let snapshot = first.snapshot.expect("the walk paused");
    assert_eq!(snapshot.tie_break.as_deref(), Some(tie_break(1.0).hash()));
    assert_eq!(Snapshot::from_json(&snapshot.to_json()).unwrap(), snapshot);

    snapshot.check_tie_break(Some(&tie_break(1.0))).unwrap();
    for (what, other) in [("the file changed", Some(tie_break(2.0))), ("none", None)] {
        let error = snapshot.check_tie_break(other.as_ref()).unwrap_err();
        assert!(matches!(error, HandleError::WrongTieBreak { .. }), "{what}");
    }
    // And the other way round: a handle made without one refuses a file.
    let plain = run(None, 1).await.snapshot.expect("the walk paused");
    plain.check_tie_break(None).unwrap();
    assert!(plain.check_tie_break(Some(&tie_break(1.0))).is_err());

    // A resume with the same file ranks as the straight run.
    let straight = run(Some(tie_break(1.0)), 20).await;
    let second = resume(&graph(), snapshot, budget(20), |t| {
        t.question("beta").tie_break(Some(tie_break(1.0)))
    })
    .await;
    assert_eq!(second.states, straight.states);
    assert_eq!(names(&second), names(&straight));
}
