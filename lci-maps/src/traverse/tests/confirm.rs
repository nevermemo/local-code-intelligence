//! The confirm frontier: a confirmer decides the `grep.live` text hops an
//! expansion kept (confirmed: relabelled a server answer; refuted: dropped;
//! unknown: unchanged), within the pop's external calls and the walk's
//! deadline, and the walk stays resumable.

use super::*;
use crate::traverse::{
    confirm::{Confirmation, FrontierConfirmer, Proposal, Verdict},
    handle::{HandleError, Snapshot},
};
use std::{collections::HashMap, sync::Mutex};

/// What a confirmer was given in one call: the proposals and `max_calls`.
type Call = (Vec<Proposal>, u32);

#[derive(Clone, Copy, PartialEq)]
enum Reply {
    /// One verdict per proposal, asking at most `max_calls` of them.
    Verdicts,
    Error,
    /// One verdict too many.
    WrongLength,
    Hang,
    /// Honest verdicts, but claims 99 calls.
    OverClaim,
}

/// Decides a proposal by its destination (`default` when unlisted) and keeps
/// every call it gets.
#[derive(Clone)]
struct Stub {
    verdicts: HashMap<NodeId, Verdict>,
    default: Verdict,
    reply: Reply,
    identity: &'static str,
    seen: Arc<Mutex<Vec<Call>>>,
}

impl Stub {
    fn new(verdicts: &[(&str, Verdict)]) -> Self {
        Self {
            verdicts: verdicts.iter().map(|&(n, v)| (node(n), v)).collect(),
            default: Verdict::Unknown,
            reply: Reply::Verdicts,
            identity: "stub-a",
            seen: Arc::default(),
        }
    }

    fn all(verdict: Verdict) -> Self {
        Self {
            default: verdict,
            ..Self::new(&[])
        }
    }

    fn replying(mut self, reply: Reply) -> Self {
        self.reply = reply;
        self
    }

    fn calls(&self) -> Vec<Call> {
        self.seen.lock().unwrap().clone()
    }
}

#[crate::async_trait]
impl FrontierConfirmer for Stub {
    fn identity(&self) -> String {
        self.identity.to_owned()
    }

    async fn confirm(
        &self,
        proposals: &[Proposal],
        max_calls: u32,
    ) -> Result<Confirmation, String> {
        self.seen
            .lock()
            .unwrap()
            .push((proposals.to_vec(), max_calls));
        let mut used = 0;
        let verdicts: Vec<Verdict> = proposals
            .iter()
            .map(|p| {
                if used >= max_calls {
                    return Verdict::Unknown;
                }
                used += 1;
                self.verdicts.get(&p.dst).copied().unwrap_or(self.default)
            })
            .collect();
        match self.reply {
            Reply::Verdicts => Ok(Confirmation {
                verdicts,
                calls: used,
            }),
            Reply::OverClaim => Ok(Confirmation {
                verdicts,
                calls: 99,
            }),
            Reply::Error => Err("the server is down".into()),
            Reply::WrongLength => Ok(Confirmation {
                verdicts: vec![Verdict::Confirmed; proposals.len() + 1],
                calls: 1,
            }),
            Reply::Hang => {
                std::future::pending::<()>().await;
                unreachable!("a hung confirmer never answers")
            }
        }
    }
}

/// A `grep.live` stub: a text proposal for every edge, seen in `src/s.rs`.
fn grep(edges: &[(&str, &str)]) -> GraphMap {
    GraphMap::new("grep.live", edges)
        .kind(EdgeKind::Mentions)
        .witness(WitnessClass::Exact)
        .site("src/s.rs")
}

async fn walk_with(
    reg: &Registry<'static>,
    seeds: Vec<Seed>,
    limits: TraversalBudget,
    confirm: Option<(&Stub, usize)>,
) -> TraversalResult {
    walk(reg, seeds, limits, |t| match confirm {
        Some((stub, top_k)) => t.confirm_frontier(Arc::new(stub.clone()), top_k),
        None => t,
    })
    .await
}

fn totals_of(result: &TraversalResult) -> Totals {
    Totals {
        elapsed_ms: 0.0,
        ..result.totals.clone()
    }
}

/// The `confirm` field of the `expand` event of the pop of `name`.
fn confirm_event<'r>(result: &'r TraversalResult, name: &str) -> &'r Value {
    events_of(result, "expand")
        .into_iter()
        .find(|e| e["node"] == node(name).to_string())
        .unwrap_or_else(|| panic!("no expand event for {name}"))
        .get("confirm")
        .unwrap_or_else(|| panic!("the expand of {name} has no confirm field"))
}

#[tokio::test]
async fn a_confirmed_mention_keeps_its_hop_and_gains_the_lsp_witness() {
    let reg = registry(vec![grep(&[("s", "a")])]);
    let stub = Stub::new(&[("a", Verdict::Confirmed)]);
    let plain = walk_with(&reg, vec![seed("s")], budget(20), None).await;
    let result = walk_with(&reg, vec![seed("s")], budget(20), Some((&stub, 5))).await;

    // The hop is still taken, at the value it had.
    assert_eq!(hops(&result), hops(&plain));
    assert_eq!(state(&result, "a").potential, state(&plain, "a").potential);
    // It is now a server answer: the witness, the hop event and the evidence.
    assert_eq!(state(&plain, "a").witness, Some(WitnessClass::Exact));
    assert_eq!(state(&result, "a").witness, Some(WitnessClass::Lsp));
    assert!(state(&result, "a").witnesses.contains(&WitnessClass::Lsp));
    let hop = &events_of(&result, "hop")[0];
    assert_eq!(hop["witness"], "lsp");
    assert!(hop["conf"].as_u64().unwrap() >= 90, "{hop}");
    // The counts, the charge and the event.
    let totals = totals_of(&result);
    assert_eq!(totals.confirm_calls, 1);
    assert_eq!(totals.confirm_proposals, 1);
    assert_eq!(totals.confirm_confirmed, 1);
    assert_eq!((totals.confirm_refuted, totals.confirm_unknown), (0, 0));
    assert_eq!(totals.confirm_failures, 0);
    assert_eq!(totals.external_calls, 1, "the call is charged to the pop");
    let event = confirm_event(&result, "s");
    assert_eq!(event["confirmed"], 1);
    assert_eq!(event["calls"], 1);
    // It was asked about the hop's own site.
    let seen = stub.calls();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].0[0].src, node("s"));
    assert_eq!(seen[0].0[0].dst, node("a"));
    assert_eq!(seen[0].0[0].site.path, "src/s.rs");
}

#[tokio::test]
async fn a_refuted_mention_is_dropped_and_still_corroborates_at_zero() {
    // `s` offers `a` (which is also a start node, so it has a state), `x` and
    // `b`; the server refutes `a` and `x`. `b` is the other candidate.
    let reg = registry(vec![grep(&[
        ("s", "a"),
        ("s", "x"),
        ("s", "b"),
        ("b", "d"),
    ])]);
    let stub = Stub::new(&[("a", Verdict::Refuted), ("x", Verdict::Refuted)]);
    let seeds = || vec![seed("s"), seed("a")];
    let plain = walk_with(&reg, seeds(), budget(20), None).await;
    let result = walk_with(&reg, seeds(), budget(20), Some((&stub, 5))).await;

    // A refuted hop is not a hop, and a node only it reached is not reached.
    let sorted = |mut hops: Vec<(String, String)>| {
        hops.sort();
        hops
    };
    let edge = |from: &str, to: &str| (node(from).to_string(), node(to).to_string());
    assert_eq!(
        sorted(hops(&plain)),
        sorted(vec![edge("s", "x"), edge("s", "b"), edge("b", "d")]),
        "the plain walk follows x and b"
    );
    assert_eq!(
        sorted(hops(&result)),
        sorted(vec![edge("s", "b"), edge("b", "d")])
    );
    assert!(plain.reached(&node("x")) && !result.reached(&node("x")));
    assert!(!popped_names(&result).contains(&"x".to_owned()));
    assert_eq!(result.totals.confirm_refuted, 2);
    assert_eq!(result.totals.confirm_confirmed, 0);
    // The refuted arrival at the start node `a` still counts as evidence
    // there, at no value: its route is the start's own.
    let a = state(&result, "a");
    assert!(a.maps.contains(&"grep.live"), "{:?}", a.maps);
    assert_eq!(a.parent, None);
    assert_eq!(a.potential, state(&plain, "a").potential);
    assert_eq!(a.witnesses, state(&plain, "a").witnesses);
    assert_eq!(confirm_event(&result, "s")["refuted"], 2);
}

#[tokio::test]
async fn an_unknown_verdict_and_a_failed_call_leave_the_hop_as_it_was() {
    let reg = registry(vec![grep(&[("s", "a"), ("s", "b"), ("a", "c")])]);
    let seeds = || vec![seed("s")];
    let plain = walk_with(&reg, seeds(), budget(20), None).await;

    // (what, the stub, failures counted)
    let cases = [
        // The server knows nothing there.
        ("unknown", Stub::all(Verdict::Unknown), 0),
        // The call errs.
        (
            "an error",
            Stub::all(Verdict::Confirmed).replying(Reply::Error),
            2,
        ),
        // One verdict too many: none of it is used.
        (
            "the wrong count",
            Stub::all(Verdict::Refuted).replying(Reply::WrongLength),
            2,
        ),
    ];
    for (what, stub, failures) in cases {
        let result = walk_with(&reg, seeds(), budget(20), Some((&stub, 5))).await;
        assert_eq!(hops(&result), hops(&plain), "{what}");
        assert_eq!(
            result.states, plain.states,
            "{what}: the states are the plain walk's"
        );
        assert_eq!(result.totals.confirm_confirmed, 0, "{what}");
        assert_eq!(result.totals.confirm_refuted, 0, "{what}");
        assert_eq!(result.totals.confirm_failures, failures, "{what}");
        assert_eq!(
            result.totals.confirm_calls, 2,
            "{what}: s and a had proposals"
        );
        assert_eq!(result.status, plain.status, "{what}");
        assert_eq!(result.stop_reason, plain.stop_reason, "{what}");
        assert_eq!(
            result.warnings.is_empty(),
            failures == 0,
            "{what}: {:?}",
            result.warnings
        );
    }
}

/// A confirmer that never answers is cut at the walk's deadline: the hops are
/// as they were and the cut is no failure of the confirmer. The tokio clock is
/// paused, so the cut comes as soon as the runtime is idle, whatever the load
/// of the machine; the walk's own deadline (a real clock, 60 s here) is never
/// near, so nothing else ends the walk first.
#[tokio::test(start_paused = true)]
async fn a_confirmer_the_deadline_cuts_changes_nothing() {
    let reg = registry(vec![grep(&[("s", "a"), ("s", "b")])]);
    let stub = Stub::all(Verdict::Refuted).replying(Reply::Hang);
    let hung = walk_with(&reg, vec![seed("s")], budget(20), Some((&stub, 5))).await;
    assert!(hung.reached(&node("a")) && hung.reached(&node("b")));
    assert_eq!(hung.totals.confirm_failures, 0);
    assert_eq!(hung.totals.confirm_refuted, 0);
    assert_eq!(hung.totals.confirm_unknown, 2);
    assert_eq!(confirm_event(&hung, "s")["failed"], "deadline");
    assert_eq!(state(&hung, "a").witness, Some(WitnessClass::Exact));
}

#[tokio::test]
async fn confirmation_stops_at_top_k_and_at_the_external_call_cap() {
    let names: Vec<String> = (1..=12).map(|i| format!("n{i}")).collect();
    let edges: Vec<(&str, &str)> = names.iter().map(|n| ("s", n.as_str())).collect();
    let reg = registry(vec![grep(&edges)]);
    let seeds = || vec![seed("s")];

    // Twelve text hops are kept; five are proposed, the five worth most.
    let stub = Stub::all(Verdict::Confirmed);
    let result = walk_with(&reg, seeds(), budget(30), Some((&stub, 5))).await;
    let calls = stub.calls();
    assert_eq!(calls.len(), 1, "one call for the expansion");
    assert_eq!(calls[0].0.len(), 5);
    assert_eq!(result.totals.confirm_proposals, 5);
    assert_eq!(result.totals.confirm_confirmed, 5);
    assert_eq!(result.totals.external_calls, 5);
    let proposed: Vec<&NodeId> = calls[0].0.iter().map(|p| &p.dst).collect();
    let values: Vec<f64> = events_of(&result, "hop")
        .iter()
        .filter(|h| proposed.iter().any(|p| h["dst"] == p.to_string()))
        .map(|h| h["potential"].as_f64().unwrap())
        .collect();
    let best_unproposed = events_of(&result, "hop")
        .iter()
        .filter(|h| !proposed.iter().any(|p| h["dst"] == p.to_string()))
        .map(|h| h["potential"].as_f64().unwrap())
        .fold(0.0, f64::max);
    assert!(
        values.iter().all(|v| *v >= best_unproposed),
        "the proposals are the best hops: {values:?} vs {best_unproposed}"
    );

    // Two external calls left: the confirmer is given two, uses two, and the
    // pop is charged two.
    let mut limits = budget(30);
    limits.external_calls = 2;
    let stub = Stub::all(Verdict::Confirmed);
    let capped = walk_with(&reg, seeds(), limits, Some((&stub, 5))).await;
    let calls = stub.calls();
    assert_eq!((calls[0].0.len(), calls[0].1), (5, 2));
    assert_eq!(capped.totals.confirm_confirmed, 2);
    assert_eq!(capped.totals.confirm_unknown, 3);
    assert_eq!(capped.totals.external_calls, 2);
    assert_eq!(capped.totals.physical_external_calls, 2);
    assert_eq!(confirm_event(&capped, "s")["calls"], 2);
    assert_eq!(
        capped.states.len(),
        13,
        "the walk took every hop all the same"
    );

    // A confirmer that claims more calls than it was given is charged what it
    // was given: the cap holds.
    let stub = Stub::all(Verdict::Confirmed).replying(Reply::OverClaim);
    let mut limits = budget(30);
    limits.external_calls = 2;
    let over = walk_with(&reg, seeds(), limits, Some((&stub, 5))).await;
    assert_eq!(over.totals.external_calls, 2);

    // No call left: no call is made, and it is no failure.
    let stub = Stub::all(Verdict::Confirmed);
    let mut limits = budget(30);
    limits.external_calls = 0;
    let none = walk_with(&reg, seeds(), limits, Some((&stub, 5))).await;
    assert!(stub.calls().is_empty());
    assert_eq!(none.totals.confirm_skipped, 1);
    assert_eq!(none.totals.confirm_calls, 0);
    assert_eq!(none.totals.confirm_failures, 0);
    assert_eq!(none.totals.external_calls, 0);
    assert_eq!(confirm_event(&none, "s")["skipped"], "external_calls");
    assert_eq!(none.states.len(), 13);
}

#[tokio::test]
async fn compiler_and_lsp_witnessed_hops_are_never_proposed() {
    // `a` is only a text proposal. `b` is a compiler-grade edge, `c` a server
    // answer. `d` and `e` are offered as text, and also (at a lower
    // confidence, so the text edge stays the best one) by the compiler and by
    // a server: the best edge is `grep.live`'s, but something stands behind it.
    let reg = registry(vec![
        grep(&[("s", "a"), ("s", "d"), ("s", "e")]),
        GraphMap::new("g", &[("s", "b")]),
        GraphMap::new("l", &[("s", "c")]).witness(WitnessClass::Lsp),
        GraphMap::new("g2", &[("s", "d")]).conf(50),
        GraphMap::new("l2", &[("s", "e")])
            .witness(WitnessClass::Lsp)
            .conf(50),
    ]);
    let stub = Stub::all(Verdict::Confirmed);
    let result = walk_with(&reg, vec![seed("s")], budget(20), Some((&stub, 5))).await;
    let calls = stub.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        calls[0].0.iter().map(|p| p.dst.clone()).collect::<Vec<_>>(),
        vec![node("a")],
        "only the hop with nothing but a text witness is proposed"
    );
    assert_eq!(result.totals.confirm_proposals, 1);
    assert_eq!(state(&result, "a").witness, Some(WitnessClass::Lsp));
    assert_eq!(state(&result, "b").witness, Some(WitnessClass::Compiler));
    for name in ["d", "e"] {
        assert_eq!(
            state(&result, name).witness,
            Some(WitnessClass::Exact),
            "{name}: the best edge is the text one and is not relabelled"
        );
    }
    assert!(
        state(&result, "d")
            .witnesses
            .contains(&WitnessClass::Compiler)
    );

    // Not attached, the confirmer is never called and nothing of the frontier
    // is in the totals, the events or the handle.
    let stub = Stub::all(Verdict::Refuted);
    let plain = walk(&reg, vec![seed("s")], budget(2), |t| t).await;
    assert!(stub.calls().is_empty());
    assert_eq!(plain.confirm_top_k, None);
    assert_eq!(plain.totals.confirm_calls, 0);
    assert_eq!(plain.totals.confirm_proposals, 0);
    assert!(
        plain
            .events
            .iter()
            .all(|e| e.get("confirm").is_none() && e["ev"] != "confirm"),
        "no event carries a confirm field"
    );
    let end = events_of(&plain, "end")[0];
    assert!(end["totals"].get("confirm_calls").is_none());
    let json = plain.snapshot.expect("the pop cap left work").to_json();
    assert!(json.get("confirm").is_none());
    assert!(json["totals"].get("confirm_calls").is_none());
}

#[tokio::test]
async fn a_handle_refuses_a_resume_under_the_other_confirm_value() {
    let reg = registry(vec![grep(&[("s", "a"), ("s", "b"), ("a", "c")])]);
    let stub = Stub::new(&[("a", Verdict::Confirmed), ("c", Verdict::Refuted)]);
    let first = walk_with(&reg, vec![seed("s")], budget(2), Some((&stub, 5))).await;
    let snapshot = first.snapshot.expect("the pop cap left work");
    assert_eq!(snapshot.confirm, Some((5, "stub-a".to_owned())));
    assert_eq!(snapshot.totals.confirm_confirmed, 1);
    assert_eq!(Snapshot::from_json(&snapshot.to_json()).unwrap(), snapshot);
    assert_eq!(snapshot.to_json()["confirm"]["top_k"], 5);

    snapshot.check_confirm(Some((5, "stub-a"))).unwrap();
    for (what, other) in [
        ("off", None),
        ("another top_k", Some((6, "stub-a"))),
        ("another confirmer", Some((5, "stub-b"))),
    ] {
        let error = snapshot.check_confirm(other).unwrap_err();
        assert!(matches!(error, HandleError::WrongConfirm { .. }), "{what}");
        assert!(error.to_string().contains("confirm frontier"), "{what}");
    }
    // A handle made without the frontier refuses it, and the other way round.
    let off = walk_with(&reg, vec![seed("s")], budget(2), None)
        .await
        .snapshot
        .expect("work left");
    assert_eq!(off.confirm, None);
    off.check_confirm(None).unwrap();
    assert!(off.check_confirm(Some((5, "stub-a"))).is_err());
    // The handle is the same whether or not the walk had the frontier off
    // before the key existed: no confirm key in a default handle.
    assert!(off.to_json().get("confirm").is_none());

    // A resume under the same setting is the straight run.
    let straight = walk_with(&reg, vec![seed("s")], budget(20), Some((&stub, 5))).await;
    let second = resume(&reg, snapshot, budget(20), |t| {
        t.confirm_frontier(Arc::new(stub.clone()), 5)
    })
    .await;
    assert_eq!(second.states, straight.states);
    assert_eq!(second.popped, straight.popped);
    assert_eq!(totals_of(&second), totals_of(&straight));
    assert_eq!(events_of(&second, "resume")[0]["confirm"]["top_k"], 5);
}
