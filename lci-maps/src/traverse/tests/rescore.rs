//! The reranker frontier: a rescorer rates the kept hops of an expansion once,
//! each value is multiplied by `floor + (1 - floor) * s`, a failed call changes
//! nothing, and the walk stays sound and resumable.

use super::*;
use crate::traverse::{
    handle::{HandleError, Snapshot},
    rescore::{FrontierRescorer, RerankFailure, RescoreError, factor},
};
use std::{collections::HashMap, sync::Mutex};

#[derive(Clone, Copy, PartialEq)]
enum Answer {
    Scores,
    Error,
    TooFew,
    NotANumber,
    Hang,
    /// An error the rescorer itself classifies.
    Failing(RerankFailure),
}

/// Rates a node by name (1 when unlisted) and keeps the nodes of each call.
#[derive(Clone)]
struct Mock {
    scores: HashMap<NodeId, f64>,
    answer: Answer,
    calls: Arc<Mutex<Vec<Vec<NodeId>>>>,
    identity: &'static str,
}

impl Mock {
    fn new(scores: &[(&str, f64)]) -> Self {
        Self {
            scores: scores.iter().map(|&(n, s)| (node(n), s)).collect(),
            answer: Answer::Scores,
            calls: Arc::default(),
            identity: "mock-a",
        }
    }

    fn answering(mut self, answer: Answer) -> Self {
        self.answer = answer;
        self
    }

    fn identified(mut self, identity: &'static str) -> Self {
        self.identity = identity;
        self
    }

    fn calls(&self) -> Vec<Vec<NodeId>> {
        self.calls.lock().unwrap().clone()
    }
}

#[crate::async_trait]
impl FrontierRescorer for Mock {
    fn identity(&self) -> String {
        self.identity.to_owned()
    }

    async fn relevance(&self, question: &str, nodes: &[NodeId]) -> Result<Vec<f64>, RescoreError> {
        assert_eq!(question, "which file");
        self.calls.lock().unwrap().push(nodes.to_vec());
        let mut scores: Vec<f64> = nodes
            .iter()
            .map(|n| self.scores.get(n).copied().unwrap_or(1.0))
            .collect();
        match self.answer {
            Answer::Scores => Ok(scores),
            Answer::Error => Err("the reranker is down".into()),
            Answer::Failing(reason) => Err(RescoreError::new(reason, "the reranker failed")),
            Answer::TooFew => {
                scores.pop();
                Ok(scores)
            }
            Answer::NotANumber => Ok(vec![f64::NAN; nodes.len()]),
            Answer::Hang => {
                tokio::time::sleep(Duration::from_secs(600)).await;
                Ok(scores)
            }
        }
    }
}

/// `s` offers `a` and `b`; `a` offers `c`, `b` offers `d`. Without a rescorer
/// the pops go `s a b c d`.
fn two_branches() -> Registry<'static> {
    registry(vec![GraphMap::new(
        "m1",
        &[("s", "a"), ("s", "b"), ("a", "c"), ("b", "d")],
    )])
}

async fn walk_with(
    reg: &Registry<'static>,
    pops: u32,
    rerank: Option<(Mock, f64)>,
) -> TraversalResult {
    walk(reg, vec![seed("s")], budget(pops), |t| {
        let t = t.question("which file");
        match rerank {
            Some((mock, floor)) => t.rerank_frontier(mock, floor),
            None => t,
        }
    })
    .await
}

fn totals_of(result: &TraversalResult) -> Totals {
    Totals {
        elapsed_ms: 0.0,
        ..result.totals.clone()
    }
}

#[test]
fn the_factor_runs_from_the_floor_to_one() {
    for (floor, s, want) in [
        (0.25, 0.0, 0.25),
        (0.25, 0.5, 0.625),
        (0.25, 1.0, 1.0),
        (0.0, 0.0, 0.0),
        (1.0, 0.0, 1.0),
        // Out-of-range relevance and floors are clamped: never above 1.
        (0.25, 5.0, 1.0),
        (0.25, -3.0, 0.25),
        (7.0, 0.5, 1.0),
        (-1.0, 0.5, 0.5),
    ] {
        assert_eq!(factor(floor, s), want, "floor {floor}, s {s}");
    }
}

#[tokio::test]
async fn off_by_default_and_a_floor_of_one_is_the_plain_walk() {
    let reg = two_branches();
    let plain = walk_with(&reg, 20, None).await;
    assert_eq!(
        popped_names(&plain),
        ["src/s.rs", "src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs"]
    );
    assert_eq!(plain.rerank_floor, None);
    assert_eq!(
        (plain.totals.rerank_calls, plain.totals.rerank_docs),
        (0, 0)
    );
    assert!(
        events_of(&plain, "expand")
            .iter()
            .all(|e| e.get("rerank").is_none())
    );
    assert!(
        events_of(&plain, "end")[0]["totals"]
            .get("rerank_calls")
            .is_none()
    );

    // The mock hates `a`, but a floor of 1 multiplies by 1: same walk, and the
    // calls are still made and counted.
    let mock = Mock::new(&[("a", 0.0)]);
    let identity = walk_with(&reg, 20, Some((mock.clone(), 1.0))).await;
    assert_eq!(identity.states, plain.states);
    assert_eq!(identity.popped, plain.popped);
    assert_eq!(hops(&identity), hops(&plain));
    assert_eq!(identity.totals.rerank_calls, 3);
    assert_eq!(mock.calls().len(), 3);
}

#[tokio::test]
async fn kept_hops_are_multiplied_and_the_order_follows() {
    let reg = two_branches();
    let mock = Mock::new(&[("a", 0.0), ("b", 0.5)]);
    let result = walk_with(&reg, 20, Some((mock, 0.25))).await;
    assert_eq!(state(&result, "a").potential, 0.25);
    assert_eq!(state(&result, "b").potential, 0.625);
    // `c` and `d` are rated 1: they keep the potential of their parents.
    assert_eq!(state(&result, "c").potential, 0.25);
    assert_eq!(state(&result, "d").potential, 0.625);
    // `b` is now the better branch, so it pops first.
    assert_eq!(
        popped_names(&result),
        ["src/s.rs", "src/b.rs", "src/d.rs", "src/a.rs", "src/c.rs"]
    );
    assert_eq!(result.rerank_floor, Some(0.25));
    assert_eq!(events_of(&result, "start")[0]["rerank_floor"], 0.25);
}

#[tokio::test]
async fn one_call_per_expansion_that_keeps_a_hop_counts_the_kept_nodes() {
    let reg = two_branches();
    let mock = Mock::new(&[("a", 0.0)]);
    let result = walk_with(&reg, 20, Some((mock.clone(), 0.25))).await;
    // Pops: s (a, b), b (d), d (nothing, no call), a (c), c (no call).
    assert_eq!(
        mock.calls(),
        [vec![node("a"), node("b")], vec![node("d")], vec![node("c")],]
    );
    assert_eq!(
        (
            result.totals.rerank_calls,
            result.totals.rerank_docs,
            result.totals.rerank_failures
        ),
        (3, 4, 0)
    );
    let docs: Vec<Option<u64>> = events_of(&result, "expand")
        .iter()
        .map(|e| e["rerank"]["docs"].as_u64())
        .collect();
    assert_eq!(docs, [Some(2), Some(1), None, Some(1), None]);
    let end = &events_of(&result, "end")[0]["totals"];
    assert_eq!(
        (end["rerank_calls"].as_u64(), end["rerank_docs"].as_u64()),
        (Some(3), Some(4))
    );

    // Only the hops the beam kept are rated: 20 are offered, the beam keeps 16.
    let mut wide = GraphMap::new("m1", &[]);
    wide.edges = (0..20)
        .map(|i| ("s".to_owned(), format!("x{i:02}")))
        .collect();
    let mock = Mock::new(&[]);
    let result = walk_with(&registry(vec![wide]), 1, Some((mock.clone(), 0.25))).await;
    assert_eq!(mock.calls()[0].len(), 16);
    assert_eq!(result.totals.rerank_docs, 16);
}

#[tokio::test]
async fn a_failed_call_keeps_factor_one_is_counted_and_the_walk_completes() {
    let reg = two_branches();
    let plain = walk_with(&reg, 20, None).await;
    // Every failure keeps its reason: the rescorer's own, or the engine's for a
    // wrong count or a score that is not a number.
    for (answer, reason) in [
        (Answer::Error, RerankFailure::Other),
        (Answer::TooFew, RerankFailure::CountMismatch),
        (Answer::NotANumber, RerankFailure::Malformed),
        (
            Answer::Failing(RerankFailure::Timeout),
            RerankFailure::Timeout,
        ),
        (
            Answer::Failing(RerankFailure::Status),
            RerankFailure::Status,
        ),
        (
            Answer::Failing(RerankFailure::Unreachable),
            RerankFailure::Unreachable,
        ),
    ] {
        let mock = Mock::new(&[("a", 0.0)]).answering(answer);
        let result = walk_with(&reg, 20, Some((mock, 0.25))).await;
        assert_eq!(result.status, TraversalStatus::Complete);
        assert_eq!(result.states, plain.states);
        assert_eq!(result.popped, plain.popped);
        assert_eq!(
            (result.totals.rerank_calls, result.totals.rerank_failures),
            (3, 3)
        );
        assert_eq!(
            result.totals.rerank_failure_reasons,
            std::collections::BTreeMap::from([(reason, 3)]),
            "{reason}"
        );
        // `too few` for a one-node call is an empty answer: also a failure.
        assert_eq!(events_of(&result, "expand")[0]["rerank"]["failed"], true);
        assert_eq!(
            events_of(&result, "expand")[0]["rerank"]["reason"],
            reason.as_str()
        );
        let end = &events_of(&result, "end")[0]["totals"];
        assert_eq!(end["rerank_failure_reasons"][reason.as_str()], 3);
        // A response warning, whatever the reason.
        assert_eq!(result.warnings.len(), 1, "{:?}", result.warnings);
        assert!(
            result.warnings[0].contains("3 of 3 reranker calls failed")
                && result.warnings[0].contains(&format!("{reason} 3")),
            "{:?}",
            result.warnings
        );
    }
    // No failure, no warning.
    let fine = walk_with(&reg, 20, Some((Mock::new(&[]), 0.25))).await;
    assert!(fine.warnings.is_empty(), "{:?}", fine.warnings);
    assert!(fine.totals.rerank_failure_reasons.is_empty());

    // A rescorer that never answers is cut at the walk's deadline.
    let mut limits = budget(20);
    limits.deadline = Instant::now() + Duration::from_millis(100);
    let mock = Mock::new(&[]).answering(Answer::Hang);
    let hung = walk(&reg, vec![seed("s")], limits, |t| {
        t.question("which file").rerank_frontier(mock, 0.25)
    })
    .await;
    assert_eq!(hung.totals.rerank_failures, 1);
    assert_eq!(
        hung.totals
            .rerank_failure_reasons
            .get(&RerankFailure::Deadline),
        Some(&1),
        "the walk's deadline cut the call"
    );
    assert!(
        hung.rerank_ms >= 50.0 && hung.rerank_ms < 5_000.0,
        "{}",
        hung.rerank_ms
    );
    assert!(hung.reached(&node("a")) && hung.reached(&node("b")));
}

#[tokio::test]
async fn the_bound_stays_sound_whatever_the_rescorer_says() {
    // Out-of-range answers are clamped, so no hop is worth more than 1 and no
    // state more than its parent; the early stop still fixes the top set.
    let scores: Vec<(String, f64)> = (0..14)
        .map(|i| (format!("n{i}"), [5.0, -3.0, 0.2, 0.9, 0.0, 1.0, 0.5][i % 7]))
        .collect();
    let scores: Vec<(&str, f64)> = scores.iter().map(|(n, s)| (n.as_str(), *s)).collect();
    let reg = registry(vec![lattice_map("g", 14)]);
    for floor in [0.0, 0.25, 1.0] {
        let run = |bound: bool| {
            let mock = Mock::new(&scores);
            let reg = &reg;
            async move {
                walk(reg, vec![seed("n0")], budget(100), |t| {
                    t.question("which file")
                        .rerank_frontier(mock, floor)
                        .top_k(3, 1)
                        .score_bound(bound)
                })
                .await
            }
        };
        let (bounded, full) = (run(true).await, run(false).await);
        for result in [&bounded, &full] {
            for s in result.states.iter().filter(|s| s.parent.is_some()) {
                let parent = &result.states[s.parent.unwrap()];
                assert!(s.potential <= parent.potential, "floor {floor}");
            }
        }
        let top = |r: &TraversalResult| -> BTreeSet<NodeId> {
            r.ranked()
                .into_iter()
                .take(3)
                .map(|i| r.states[i].node.clone())
                .collect()
        };
        assert_eq!(top(&bounded), top(&full), "floor {floor}");
        if floor == 1.0 {
            assert_eq!(
                bounded.stop_reason,
                crate::traverse::stop::StopReason::Bound
            );
        }
    }
}

#[tokio::test]
async fn a_handle_keeps_the_floor_refuses_another_and_resumes_as_the_straight_run() {
    let reg = two_branches();
    let mock = Mock::new(&[("a", 0.0), ("b", 0.5)]);
    let first = walk_with(&reg, 2, Some((mock.clone(), 0.25))).await;
    let snapshot = first.snapshot.expect("the pop cap left work");
    assert_eq!(snapshot.rerank_floor, Some(0.25));
    assert_eq!(snapshot.totals.rerank_calls, 2);
    assert_eq!(Snapshot::from_json(&snapshot.to_json()).unwrap(), snapshot);

    assert_eq!(snapshot.rerank_identity.as_deref(), Some("mock-a"));
    snapshot.check_rerank(Some((0.25, "mock-a"))).unwrap();
    for (what, other) in [
        ("off", None),
        ("another floor", Some((0.5, "mock-a"))),
        ("another reranker", Some((0.25, "mock-b"))),
    ] {
        let error = snapshot.check_rerank(other).unwrap_err();
        assert!(matches!(error, HandleError::WrongRerank { .. }), "{what}");
    }
    // The handle keeps the identity the rescorer reports, and refuses the
    // reranker it was not made with.
    let other = Mock::new(&[]).identified("mock-b");
    let made_by_b = walk_with(&reg, 2, Some((other, 0.25)))
        .await
        .snapshot
        .expect("work left");
    assert_eq!(made_by_b.rerank_identity.as_deref(), Some("mock-b"));
    made_by_b.check_rerank(Some((0.25, "mock-b"))).unwrap();
    assert!(made_by_b.check_rerank(Some((0.25, "mock-a"))).is_err());
    // A handle made without the frontier refuses it, and the other way round.
    let off = walk_with(&reg, 2, None).await.snapshot.expect("work left");
    assert_eq!(off.rerank_floor, None);
    assert_eq!(off.rerank_identity, None);
    off.check_rerank(None).unwrap();
    assert!(off.check_rerank(Some((0.25, "mock-a"))).is_err());

    let straight = walk_with(&reg, 20, Some((mock.clone(), 0.25))).await;
    let second = resume(&reg, snapshot, budget(20), |t| {
        t.rerank_frontier(mock.clone(), 0.25)
    })
    .await;
    assert_eq!(second.states, straight.states);
    assert_eq!(second.popped, straight.popped);
    assert_eq!(totals_of(&second), totals_of(&straight));
    assert_eq!(events_of(&second, "resume")[0]["rerank_floor"], 0.25);
}

/// The `model_calls` cap stops the rating, not the walk.
#[tokio::test]
async fn the_model_calls_cap_stops_rating_but_not_the_walk() {
    let reg = two_branches();
    let plain = walk_with(&reg, 20, None).await;
    let mock = Mock::new(&[("a", 0.0)]);
    let capped = walk(&reg, vec![seed("s")], budget(20), |t| {
        t.question("which file")
            .rerank_frontier(mock.clone(), 0.25)
            .model_calls(Some(2))
    })
    .await;
    assert_eq!(capped.status, TraversalStatus::Complete);
    // The walk reaches every state the plain walk does (the order differs:
    // `a` was rated down before the cap was reached).
    let sorted = |r: &TraversalResult| {
        let mut p = r.popped.clone();
        p.sort_unstable();
        p
    };
    assert_eq!(sorted(&capped), sorted(&plain), "the walk goes on");
    assert_eq!(mock.calls().len(), 2, "no call past the cap");
    assert_eq!(
        (
            capped.totals.rerank_calls,
            capped.totals.rerank_failures,
            capped.totals.rerank_skipped
        ),
        (2, 0, 1)
    );
    let skipped = events_of(&capped, "expand")
        .iter()
        .filter(|e| e["rerank"]["skipped"] == "model_calls")
        .count();
    assert_eq!(skipped, 1);
    assert_eq!(events_of(&capped, "end")[0]["totals"]["rerank_skipped"], 1);
    assert_eq!(capped.warnings.len(), 1, "{:?}", capped.warnings);
    assert!(
        capped.warnings[0].contains("model_calls cap of 2")
            && capped.warnings[0].contains("1 expansions were not rated"),
        "{:?}",
        capped.warnings
    );

    // No cap by default, and a cap that is never reached warns of nothing.
    for cap in [None, Some(3), Some(100)] {
        let mock = Mock::new(&[]);
        let result = walk(&reg, vec![seed("s")], budget(20), |t| {
            t.question("which file")
                .rerank_frontier(mock.clone(), 0.25)
                .model_calls(cap)
        })
        .await;
        assert_eq!(
            (result.totals.rerank_calls, result.totals.rerank_skipped),
            (3, 0),
            "{cap:?}"
        );
        assert!(result.warnings.is_empty(), "{:?}", result.warnings);
    }
}

/// The cap counts calls over the whole traversal, a resume included.
#[tokio::test]
async fn the_model_calls_cap_is_absolute_across_a_resume() {
    let reg = two_branches();
    let mock = Mock::new(&[("a", 0.0)]);
    let first = walk(&reg, vec![seed("s")], budget(2), |t| {
        t.question("which file")
            .rerank_frontier(mock.clone(), 0.25)
            .model_calls(Some(2))
    })
    .await;
    assert_eq!(first.totals.rerank_calls, 2, "s and a were rated");
    assert!(first.warnings.is_empty(), "the cap was not hit yet");
    let snapshot = first.snapshot.expect("the pop cap left work");
    let second = resume(&reg, snapshot, budget(20), |t| {
        t.rerank_frontier(mock.clone(), 0.25).model_calls(Some(2))
    })
    .await;
    assert_eq!(second.status, TraversalStatus::Complete);
    assert_eq!(
        (second.totals.rerank_calls, second.totals.rerank_skipped),
        (2, 1),
        "the second walk starts at the first's two calls"
    );
    assert_eq!(mock.calls().len(), 2);
    let straight = walk(&reg, vec![seed("s")], budget(20), |t| {
        t.question("which file")
            .rerank_frontier(Mock::new(&[("a", 0.0)]), 0.25)
            .model_calls(Some(2))
    })
    .await;
    assert_eq!(second.states, straight.states);
    assert_eq!(totals_of(&second), totals_of(&straight));
}

/// The failure counts by reason persist in a handle and keep adding up.
#[tokio::test]
async fn failure_reasons_persist_in_a_handle() {
    let reg = two_branches();
    let mock = Mock::new(&[]).answering(Answer::Failing(RerankFailure::Unreachable));
    let first = walk_with(&reg, 2, Some((mock.clone(), 0.25))).await;
    let snapshot = first.snapshot.expect("work left");
    assert_eq!(
        snapshot.totals.rerank_failure_reasons,
        std::collections::BTreeMap::from([(RerankFailure::Unreachable, 2)])
    );
    assert_eq!(Snapshot::from_json(&snapshot.to_json()).unwrap(), snapshot);
    let second = resume(&reg, snapshot, budget(20), |t| {
        t.rerank_frontier(mock.clone(), 0.25)
    })
    .await;
    assert_eq!(
        second.totals.rerank_failure_reasons,
        std::collections::BTreeMap::from([(RerankFailure::Unreachable, 3)])
    );
    assert!(
        second.warnings[0].contains("3 of 3 reranker calls failed"),
        "{:?}",
        second.warnings
    );
}
