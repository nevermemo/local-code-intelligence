//! Learned hop priors in the engine: the table orders pops, the guess stays
//! what it was, and a handle refuses a table that is not the one it used.

use super::*;
use crate::traverse::{handle::HandleError, handle::Snapshot, priors::HopPriors};

/// `m2` rates a containment hop far above `m1`'s call, the opposite of the
/// guess (1 for a call, 0.5 for containment).
fn table(contains: f64) -> String {
    format!(
        r#"{{"version": 1, "holdout": [],
        "priors": {{"flow": {{"m1": {{"calls_or_refs": 0.02}}, "m2": {{"contains": {contains}}}}}}}}}"#
    )
}

fn two_kinds() -> Registry<'static> {
    registry(vec![
        GraphMap::new("m1", &[("s", "a"), ("a", "c")]),
        GraphMap::new("m2", &[("s", "b"), ("b", "d")]).kind(EdgeKind::Contains),
    ])
}

fn priors(question_type: &str, contains: f64) -> HopPriors {
    HopPriors::from_text(&table(contains), question_type).unwrap()
}

async fn run(reg: &Registry<'static>, pops: u32, priors: HopPriors) -> TraversalResult {
    walk(reg, vec![seed("s")], budget(pops), |t| t.priors(priors)).await
}

#[tokio::test]
async fn a_fitted_table_reorders_the_pops_and_an_unknown_type_is_the_guess() {
    let reg = two_kinds();
    let guessed = walk(&reg, vec![seed("s")], budget(20), |t| t).await;
    assert_eq!(
        popped_names(&guessed),
        ["src/s.rs", "src/a.rs", "src/c.rs", "src/b.rs", "src/d.rs"]
    );
    // Normalised, the containment hop is 1 and the call 0.02 / 0.18.
    let learned = run(&reg, 20, priors("flow", 0.18)).await;
    assert_eq!(
        popped_names(&learned),
        ["src/s.rs", "src/b.rs", "src/d.rs", "src/a.rs", "src/c.rs"]
    );
    assert_eq!(state(&learned, "b").potential, 1.0);
    assert!((state(&learned, "a").potential - 0.02 / 0.18).abs() < 1e-12);
    assert_eq!(
        events_of(&learned, "start")[0]["priors"]["question_type"],
        "flow"
    );

    // A type the table does not hold prices every hop at the guess; only the
    // `start` event, which names the table, differs.
    let other = run(&reg, 20, priors("history", 0.18)).await;
    assert_eq!(other.states, guessed.states);
    assert_eq!(other.popped, guessed.popped);
    assert_eq!(other.events[1..], guessed.events[1..]);
    assert!(events_of(&guessed, "start")[0].get("priors").is_none());
}

#[tokio::test]
async fn a_handle_refuses_priors_that_are_not_the_ones_it_was_made_with() {
    let reg = two_kinds();
    let first = run(&reg, 2, priors("flow", 0.18)).await;
    let snapshot = first.snapshot.expect("the walk paused");
    assert_eq!(snapshot.priors.as_ref().map(|p| p.0.as_str()), Some("flow"));
    assert_eq!(Snapshot::from_json(&snapshot.to_json()).unwrap(), snapshot);

    snapshot.check_priors(&priors("flow", 0.18)).unwrap();
    for (what, other) in [
        ("the table file changed", priors("flow", 0.19)),
        ("another question type", priors("impact", 0.18)),
        ("the guess", HopPriors::default()),
    ] {
        let error = snapshot.check_priors(&other).unwrap_err();
        assert!(matches!(error, HandleError::WrongPriors { .. }), "{what}");
    }
    // And the other way round: a guessed handle refuses a table.
    let guessed = walk(&reg, vec![seed("s")], budget(2), |t| t).await;
    let guessed = guessed.snapshot.expect("the walk paused");
    guessed.check_priors(&HopPriors::default()).unwrap();
    assert!(guessed.check_priors(&priors("flow", 0.18)).is_err());

    // A resume with the same table prices as the straight run, and says so.
    let straight = run(&reg, 20, priors("flow", 0.18)).await;
    let second = resume(&reg, snapshot, budget(20), |t| {
        t.priors(priors("flow", 0.18))
    })
    .await;
    assert_eq!(second.states, straight.states);
    assert_eq!(
        events_of(&second, "resume")[0]["priors"]["question_type"],
        "flow"
    );
}
