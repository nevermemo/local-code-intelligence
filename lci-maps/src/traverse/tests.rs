//! Engine tests: budgets, determinism, fail-open, cross-map hops, statuses,
//! fusion, and the effect of every handler action on the walk.

use super::*;
use crate::{
    Contribution, Cost, EdgeKind, FreshnessModel, Map, MapDescriptor, MapRequest, MapResponse,
    MapStatus, Registry, SnapshotRef, SnapshotView, SourceLabel,
    edge::{Edge, WitnessClass},
    traverse::{
        frontier::{BfsScorer, RandomScorer},
        handler::{Action, Directive},
        rules::RuleList,
        source::{OnTheFly, fuse},
    },
};
use lci_core::node::NodeKind;
use std::time::Duration;

fn node(name: &str) -> NodeId {
    match name.strip_prefix("tests/") {
        Some(rest) => NodeId::file(format!("tests/{rest}.rs")),
        None => NodeId::file(format!("src/{name}.rs")),
    }
    .unwrap()
}

struct GraphMap {
    descriptor: MapDescriptor,
    edges: Vec<(String, String)>,
    conf: u8,
    witness: WitnessClass,
    delay: Duration,
    panics: bool,
    cost: Cost,
}

impl GraphMap {
    fn new(id: MapId, edges: &[(&str, &str)]) -> Self {
        Self {
            descriptor: MapDescriptor {
                id,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::File],
                emits: vec![NodeKind::File],
                freshness: FreshnessModel::PerFile,
                source_labels: vec![SourceLabel::Scip],
                needs: Vec::new(),
            },
            edges: edges
                .iter()
                .map(|(a, b)| (a.to_string(), b.to_string()))
                .collect(),
            conf: 80,
            witness: WitnessClass::Compiler,
            delay: Duration::ZERO,
            panics: false,
            cost: Cost::default(),
        }
    }

    fn conf(mut self, conf: u8) -> Self {
        self.conf = conf;
        self
    }

    fn delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn panics(mut self) -> Self {
        self.panics = true;
        self
    }

    fn cost(mut self, est_tokens: u64, external_calls: u32) -> Self {
        self.cost.est_tokens = est_tokens;
        self.cost.external_calls = external_calls;
        self
    }

    fn witness(mut self, witness: WitnessClass) -> Self {
        self.witness = witness;
        self
    }

    fn accepts(mut self, kinds: Vec<NodeKind>) -> Self {
        self.descriptor.accepts = kinds;
        self
    }
}

#[crate::async_trait]
impl Map for GraphMap {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, _request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        MapResponse::empty(&self.descriptor, view, MapStatus::Ok)
    }

    async fn expand(&self, from: &NodeId, _budget: Budget, _view: &SnapshotView<'_>) -> Vec<Edge> {
        assert!(!self.panics, "this map panics on purpose");
        if !self.delay.is_zero() {
            tokio::time::sleep(self.delay).await;
        }
        self.edges
            .iter()
            .filter(|(a, _)| node(a) == *from)
            .map(|(a, b)| Edge {
                src: node(a),
                dst: node(b),
                kind: EdgeKind::CallsOrRefs,
                map: self.descriptor.id,
                witness: self.witness,
                conf: self.conf,
                site: None,
                cost: self.cost,
            })
            .collect()
    }
}

fn registry(maps: Vec<GraphMap>) -> Registry<'static> {
    let mut registry = Registry::new(60.0);
    for map in maps {
        registry.register(Box::new(map), 1.0, 40).unwrap();
    }
    registry
}

fn view() -> SnapshotView<'static> {
    SnapshotView::new(SnapshotRef::default(), Vec::new())
}

fn budget(pops: u32) -> TraversalBudget {
    TraversalBudget {
        pops,
        max_depth: 20,
        deadline: Instant::now() + Duration::from_secs(60),
        est_tokens: u64::MAX / 2,
        external_calls: u32::MAX / 2,
    }
}

fn seed(name: &str) -> Seed {
    Seed::from(node(name))
}

async fn walk(
    registry: &Registry<'static>,
    seeds: Vec<Seed>,
    budget: TraversalBudget,
    configure: impl for<'t> FnOnce(Traversal<'t>) -> Traversal<'t>,
) -> TraversalResult {
    let view = view();
    let traversal = Traversal::new(OnTheFly::new(registry, &view), seeds, budget);
    configure(traversal).run().await
}

/// `n` nodes `n0..`, each with edges to the next two: a wide, deep graph.
fn lattice(n: usize) -> Vec<(String, String)> {
    (0..n)
        .flat_map(|i| (1..=2).map(move |d| (format!("n{i}"), format!("n{}", i + d))))
        .collect()
}

fn lattice_map(id: MapId, n: usize) -> GraphMap {
    let mut map = GraphMap::new(id, &[]);
    map.edges = lattice(n);
    map
}

fn popped_names(result: &TraversalResult) -> Vec<String> {
    result
        .popped
        .iter()
        .map(|&i| result.states[i].node.path().unwrap().to_string())
        .collect()
}

fn hops(result: &TraversalResult) -> Vec<(String, String)> {
    result
        .events
        .iter()
        .filter(|e| e["ev"] == "hop")
        .map(|e| {
            (
                e["src"].as_str().unwrap().to_string(),
                e["dst"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

// ---- budgets -------------------------------------------------------------

#[tokio::test]
async fn the_pop_budget_is_never_exceeded() {
    let reg = registry(vec![lattice_map("g", 60)]);
    for pops in [0, 1, 7, 30] {
        let result = walk(&reg, vec![seed("n0")], budget(pops), |t| t).await;
        assert_eq!(result.totals.pops, pops);
        assert_eq!(result.popped.len(), pops as usize);
        assert_eq!(result.status, TraversalStatus::BudgetExhausted);
    }
}

#[tokio::test]
async fn the_deadline_is_never_exceeded() {
    let reg = registry(vec![
        GraphMap::new("slow", &[("a", "b"), ("b", "c"), ("c", "d"), ("d", "e")])
            .delay(Duration::from_millis(60)),
    ]);
    let mut limits = budget(100);
    let limit = Duration::from_millis(100);
    limits.deadline = Instant::now() + limit;
    let started = Instant::now();
    let result = walk(&reg, vec![seed("a")], limits, |t| t).await;
    // The second expansion is cut at the deadline, not waited for.
    assert!(started.elapsed() < limit + Duration::from_millis(150));
    assert_eq!(result.status, TraversalStatus::BudgetExhausted);
    assert_eq!(result.totals.pops, 2);
    let expands: Vec<_> = result
        .events
        .iter()
        .filter(|e| e["ev"] == "expand")
        .collect();
    assert_eq!(
        expands[1]["calls"][0]["failure"],
        "did not answer before the deadline"
    );
}

#[tokio::test]
async fn an_expired_deadline_pops_nothing() {
    let reg = registry(vec![lattice_map("g", 10)]);
    let mut limits = budget(100);
    limits.deadline = Instant::now();
    let result = walk(&reg, vec![seed("n0")], limits, |t| t).await;
    assert_eq!(result.totals.pops, 0);
    assert_eq!(result.status, TraversalStatus::BudgetExhausted);
}

#[tokio::test]
async fn the_token_budget_is_never_exceeded() {
    // Four fresh nodes per expansion at 20 tokens per call and 12 per edge.
    let wide: Vec<(String, String)> = (0..40)
        .flat_map(|i| (0..4).map(move |k| (format!("n{i}"), format!("n{}", i * 4 + k + 1))))
        .collect();
    let mut map = GraphMap::new("g", &[]).cost(20, 0);
    map.edges = wide;
    let reg = registry(vec![map]);
    for limit in [0u64, 19, 44, 68, 200, 500] {
        let mut limits = budget(1000);
        limits.est_tokens = limit;
        let result = walk(&reg, vec![seed("n0")], limits, |t| t).await;
        assert!(
            result.totals.tokens <= limit,
            "{} > {limit}",
            result.totals.tokens
        );
        assert_eq!(result.status, TraversalStatus::BudgetExhausted, "{limit}");
    }
    // 44 tokens: the call (20) and two of the four edges (24).
    let mut limits = budget(1000);
    limits.est_tokens = 44;
    let result = walk(&reg, vec![seed("n0")], limits, |t| t).await;
    assert_eq!((result.totals.tokens, result.totals.hops), (44, 2));
    // 19 tokens: the call alone does not fit, so nothing is adopted.
    let mut limits = budget(1000);
    limits.est_tokens = 19;
    let result = walk(&reg, vec![seed("n0")], limits, |t| t).await;
    assert_eq!((result.totals.tokens, result.totals.hops), (0, 0));
}

#[tokio::test]
async fn the_external_call_budget_is_never_exceeded() {
    let reg = registry(vec![lattice_map("g", 30).cost(0, 1)]);
    let mut limits = budget(1000);
    limits.external_calls = 3;
    let result = walk(&reg, vec![seed("n0")], limits, |t| t).await;
    assert_eq!(result.totals.external_calls, 3);
    assert_eq!(result.status, TraversalStatus::BudgetExhausted);
}

#[tokio::test]
async fn max_depth_records_but_does_not_expand() {
    let reg = registry(vec![GraphMap::new(
        "g",
        &[("a", "b"), ("b", "c"), ("c", "d")],
    )]);
    let mut limits = budget(100);
    limits.max_depth = 2;
    let result = walk(&reg, vec![seed("a")], limits, |t| t).await;
    assert!(result.reached(&node("c")));
    assert!(!result.reached(&node("d")));
    assert_eq!(result.status, TraversalStatus::Complete);
}

// ---- determinism ---------------------------------------------------------

#[tokio::test]
async fn equal_runs_give_equal_events() {
    let reg = registry(vec![lattice_map("g", 40)]);
    let price = HopPrice { gate: 1, beam: 2 };
    for seed_value in [1u64, 2, 3] {
        let run = || {
            walk(&reg, vec![seed("n0")], budget(25), move |t| {
                t.price(price).scorer(RandomScorer::new(seed_value))
            })
        };
        let (a, b) = (run().await, run().await);
        assert_eq!(a.events, b.events, "seed {seed_value}");
        assert_eq!(a.popped, b.popped);
    }
    // The default scorer is deterministic too.
    let run = || walk(&reg, vec![seed("n0")], budget(25), move |t| t.price(price));
    assert_eq!(run().await.events, run().await.events);
}

#[tokio::test]
async fn the_seed_of_the_random_arm_changes_the_walk() {
    let reg = registry(vec![lattice_map("g", 40)]);
    let price = HopPrice { gate: 1, beam: 2 };
    let mut walks = std::collections::HashSet::new();
    for seed_value in 0..8u64 {
        let result = walk(&reg, vec![seed("n0")], budget(25), move |t| {
            t.price(price).scorer(RandomScorer::new(seed_value))
        })
        .await;
        walks.insert(hops(&result));
    }
    assert!(walks.len() > 1);
}

#[tokio::test]
async fn events_are_json_lines_from_start_to_end() {
    let reg = registry(vec![GraphMap::new("g", &[("a", "b")])]);
    let result = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.question("what does a call")
    })
    .await;
    let lines = result.events_jsonl();
    let parsed: Vec<Value> = lines
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(parsed.first().unwrap()["ev"], "start");
    assert_eq!(parsed.first().unwrap()["question"], "what does a call");
    assert_eq!(parsed.last().unwrap()["ev"], "end");
    assert_eq!(parsed.last().unwrap()["status"], "complete");
    assert_eq!(parsed.last().unwrap()["totals"]["hops"], 1);
}

// ---- fail-open and cross-map hops -----------------------------------------

#[tokio::test]
async fn a_failing_map_does_not_stop_the_walk() {
    let reg = registry(vec![
        GraphMap::new("aaa.broken", &[("a", "x")]).panics(),
        GraphMap::new("zzz.good", &[("a", "b"), ("b", "c")]),
    ]);
    let result = walk(&reg, vec![seed("a")], budget(10), |t| t).await;
    assert_eq!(result.status, TraversalStatus::Complete);
    assert!(result.reached(&node("c")));
    assert!(!result.reached(&node("x")));
    let first = result.events.iter().find(|e| e["ev"] == "expand").unwrap();
    assert_eq!(first["calls"][0]["map"], "aaa.broken");
    assert_eq!(first["calls"][0]["failure"], "panicked");
    assert_eq!(first["calls"][1]["failure"], Value::Null);
}

#[tokio::test]
async fn a_cross_map_hop_is_recorded() {
    // a -> b by m1, b -> c by m2.
    let reg = registry(vec![
        GraphMap::new("m1", &[("a", "b")]),
        GraphMap::new("m2", &[("b", "c")]),
    ]);
    let result = walk(&reg, vec![seed("a")], budget(10), |t| t).await;
    assert_eq!(result.totals.hops, 2);
    assert_eq!(result.totals.cross_map_hops, 1);
    let flags: Vec<_> = result
        .events
        .iter()
        .filter(|e| e["ev"] == "hop")
        .map(|e| {
            (
                e["map"].as_str().unwrap().to_string(),
                e["cross_map"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(flags, [("m1".to_string(), false), ("m2".to_string(), true)]);
    let c = result.states.iter().find(|s| s.node == node("c")).unwrap();
    assert_eq!(c.via, Some("m2"));
}

#[tokio::test]
async fn a_seed_found_by_another_map_makes_the_first_hop_cross_map() {
    let reg = registry(vec![GraphMap::new("graph", &[("a", "b")])]);
    let from_lexical = Seed {
        node: node("a"),
        map: Some("lexical"),
    };
    let result = walk(&reg, vec![from_lexical], budget(10), |t| t).await;
    assert_eq!(result.totals.cross_map_hops, 1);
}

#[tokio::test]
async fn maps_are_asked_only_about_kinds_they_accept_and_while_enabled() {
    let mut reg = registry(vec![
        GraphMap::new("files", &[("a", "b")]),
        GraphMap::new("symbols", &[("a", "c")]).accepts(vec![NodeKind::Symbol]),
        GraphMap::new("off", &[("a", "d")]),
    ]);
    assert!(reg.set_enabled("off", false));
    let result = walk(&reg, vec![seed("a")], budget(10), |t| t).await;
    assert!(result.reached(&node("b")));
    assert!(!result.reached(&node("c")) && !result.reached(&node("d")));
    assert_eq!(
        result.totals.per_map.keys().copied().collect::<Vec<_>>(),
        ["files"]
    );
}

// ---- fusion ----------------------------------------------------------------

fn edge(src: &str, dst: &str, map: MapId, witness: WitnessClass, conf: u8) -> Edge {
    Edge {
        src: node(src),
        dst: node(dst),
        kind: EdgeKind::CallsOrRefs,
        map,
        witness,
        conf,
        site: None,
        cost: Cost::default(),
    }
}

#[test]
fn fusion_is_noisy_or_over_maps_with_a_provenance_union() {
    let fused = fuse(vec![
        edge("a", "b", "m1", WitnessClass::Compiler, 60),
        edge("a", "b", "m2", WitnessClass::Text, 50),
        // A second edge of the same map does not count twice.
        edge("a", "b", "m1", WitnessClass::Compiler, 10),
        edge("a", "c", "m1", WitnessClass::Compiler, 90),
    ]);
    assert_eq!(fused.len(), 2);
    assert_eq!(fused[0].best.dst, node("c"));
    let b = &fused[1];
    assert_eq!(b.conf, 80);
    assert_eq!(b.maps, ["m1", "m2"]);
    assert_eq!(b.witnesses, [WitnessClass::Compiler, WitnessClass::Text]);
    assert_eq!(b.best.map, "m1");
}

#[test]
fn fusion_order_is_total() {
    let edges = vec![
        edge("a", "z", "m1", WitnessClass::Compiler, 70),
        edge("a", "y", "m1", WitnessClass::Compiler, 70),
        edge("a", "x", "m2", WitnessClass::Compiler, 70),
    ];
    let mut reversed = edges.clone();
    reversed.reverse();
    assert_eq!(fuse(edges), fuse(reversed));
}

#[tokio::test]
async fn two_maps_agreeing_on_an_edge_make_one_hop() {
    let reg = registry(vec![
        GraphMap::new("m1", &[("a", "b")]).conf(60),
        GraphMap::new("m2", &[("a", "b")]).conf(50),
    ]);
    let result = walk(&reg, vec![seed("a")], budget(10), |t| t).await;
    assert_eq!(result.totals.hops, 1);
    let hop = result.events.iter().find(|e| e["ev"] == "hop").unwrap();
    assert_eq!(hop["conf"], 80);
    assert_eq!(hop["maps"], serde_json::json!(["m1", "m2"]));
}

// ---- statuses --------------------------------------------------------------

#[tokio::test]
async fn statuses() {
    let reg = registry(vec![GraphMap::new("g", &[("a", "b"), ("b", "c")])]);
    // The frontier runs dry.
    let done = walk(&reg, vec![seed("a")], budget(10), |t| t).await;
    assert_eq!(done.status, TraversalStatus::Complete);
    // The target is reached; the walk ends before the frontier is empty.
    let hit = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.targets(vec![node("b")])
    })
    .await;
    assert_eq!(hit.status, TraversalStatus::Complete);
    assert_eq!(hit.totals.pops, 1);
    // The target is out of reach, with or without budget to spare.
    let lost = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.targets(vec![node("zzz")])
    })
    .await;
    assert_eq!(lost.status, TraversalStatus::NoPathWithinBudget);
    let short = walk(&reg, vec![seed("a")], budget(1), |t| {
        t.targets(vec![node("c")])
    })
    .await;
    assert_eq!(short.status, TraversalStatus::NoPathWithinBudget);
    // Budget out, but a target was reached.
    let part = walk(&reg, vec![seed("a")], budget(1), |t| {
        t.targets(vec![node("b"), node("c")])
    })
    .await;
    assert_eq!(part.status, TraversalStatus::BudgetExhausted);
    // A handler stops the walk.
    let stopped = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.handler(
            scripted(|_| vec![act(None, Action::Stop)]),
            StopPolicy::every(1),
        )
    })
    .await;
    assert_eq!(stopped.status, TraversalStatus::Stopped);
    assert_eq!(stopped.totals.pops, 1);
}

// ---- stops and handler actions ---------------------------------------------

struct ScriptedHandler<F>(F);

fn scripted<F: Fn(&Checkpoint) -> Vec<Directive> + Send + Sync>(f: F) -> ScriptedHandler<F> {
    ScriptedHandler(f)
}

impl<F: Fn(&Checkpoint) -> Vec<Directive> + Send + Sync> Handler for ScriptedHandler<F> {
    fn id(&self) -> &str {
        "scripted"
    }

    fn decide(&self, checkpoint: &Checkpoint, _budget: &StopBudget) -> Decision {
        Decision {
            actions: (self.0)(checkpoint),
            confidence: 1.0,
            ..Decision::default()
        }
    }
}

fn act(node: Option<&NodeId>, action: Action) -> Directive {
    Directive {
        node: node.cloned(),
        action,
    }
}

fn every(pops: u32) -> StopPolicy {
    StopPolicy::every(pops)
}

/// a -> b, c, d (potential 1 each: below the gate).
fn fan() -> Registry<'static> {
    registry(vec![GraphMap::new(
        "g",
        &[
            ("a", "b"),
            ("a", "c"),
            ("a", "d"),
            ("b", "e"),
            ("c", "f"),
            ("d", "h"),
        ],
    )])
}

#[tokio::test]
async fn without_a_handler_the_order_is_discovery_order() {
    let result = walk(&fan(), vec![seed("a")], budget(10), |t| t).await;
    assert_eq!(
        popped_names(&result),
        [
            "src/a.rs", "src/b.rs", "src/c.rs", "src/d.rs", "src/e.rs", "src/f.rs", "src/h.rs"
        ]
    );
}

#[tokio::test]
async fn expand_boost_and_rerank_reorder_the_frontier() {
    let expand = walk(&fan(), vec![seed("a")], budget(3), |t| {
        t.handler(
            scripted(|_| vec![act(Some(&node("d")), Action::Expand)]),
            every(1),
        )
    })
    .await;
    assert_eq!(popped_names(&expand)[1], "src/d.rs");

    let boost = walk(&fan(), vec![seed("a")], budget(3), |t| {
        t.handler(
            scripted(|_| vec![act(Some(&node("c")), Action::Boost(4.0))]),
            every(1),
        )
    })
    .await;
    assert_eq!(popped_names(&boost)[1], "src/c.rs");

    // Boosting below 1 sends a node to the back.
    let demote = walk(&fan(), vec![seed("a")], budget(3), |t| {
        t.handler(
            scripted(|_| vec![act(Some(&node("b")), Action::Boost(0.5))]),
            every(1),
        )
    })
    .await;
    assert_eq!(&popped_names(&demote)[1..], ["src/c.rs", "src/d.rs"]);
}

#[tokio::test]
async fn rerank_by_witness_puts_the_strongest_class_first() {
    // b is offered by a text map, c by a compiler-grade one: b is found first.
    let reg = registry(vec![
        GraphMap::new("m1", &[("a", "b")]).witness(WitnessClass::Text),
        GraphMap::new("m2", &[("a", "c")]),
    ]);
    let plain = walk(&reg, vec![seed("a")], budget(2), |t| t).await;
    assert_eq!(popped_names(&plain)[1], "src/b.rs");
    let result = walk(&reg, vec![seed("a")], budget(2), |t| {
        t.handler(
            scripted(|_| vec![act(None, Action::Rerank(RerankBy::Witness))]),
            every(1),
        )
    })
    .await;
    assert_eq!(popped_names(&result)[1], "src/c.rs");
}

#[test]
fn rerank_permutes_the_priorities_it_was_given() {
    let mut frontier = Frontier::default();
    for (state, priority) in [(0, 0.9), (1, 0.5), (2, 0.1)] {
        frontier.push(state, priority);
    }
    // Largest state number first.
    frontier.rerank(std::cmp::Reverse);
    let order: Vec<(usize, f64)> = frontier
        .sorted()
        .iter()
        .map(|i| (i.state, i.priority))
        .collect();
    assert_eq!(order, [(2, 0.9), (1, 0.5), (0, 0.1)]);
}

#[tokio::test]
async fn an_out_of_range_boost_is_clamped_by_the_engine() {
    let result = walk(&fan(), vec![seed("a")], budget(2), |t| {
        t.handler(
            scripted(|_| {
                vec![
                    act(Some(&node("b")), Action::Boost(1e12)),
                    act(Some(&node("c")), Action::Boost(f64::NAN)),
                ]
            }),
            every(1),
        )
    })
    .await;
    // b is boosted (to the cap), the NaN boost is ignored.
    assert_eq!(result.totals.steers, 1);
    assert_eq!(popped_names(&result)[1], "src/b.rs");
}

#[tokio::test]
async fn prune_clears_a_node_for_good() {
    // b is pruned at the first stop; c reaches it again, and it stays out.
    let reg = registry(vec![GraphMap::new(
        "g",
        &[("a", "b"), ("a", "c"), ("c", "b"), ("b", "z")],
    )]);
    let rules = RuleList::from_json(&serde_json::json!([
        {"when": {"path_glob": "src/b.rs"}, "then": "prune"}
    ]))
    .unwrap();
    let result = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.handler(rules, every(1))
    })
    .await;
    let names = popped_names(&result);
    assert!(!names.contains(&"src/b.rs".to_string()), "{names:?}");
    assert!(!result.reached(&node("z")));
    assert!(result.cleared.contains(&node("b")));
    assert_eq!(result.status, TraversalStatus::Complete);
    // No hop into b after the stop.
    let steer_at = result
        .events
        .iter()
        .position(|e| e["ev"] == "steer")
        .unwrap();
    assert!(
        result.events[steer_at..]
            .iter()
            .all(|e| !(e["ev"] == "hop" && e["dst"] == "file:src/b.rs"))
    );
}

#[tokio::test]
async fn the_demo_rule_prunes_a_test_directory() {
    let reg = registry(vec![GraphMap::new(
        "g",
        &[("a", "b"), ("a", "tests/t"), ("tests/t", "u")],
    )]);
    let rules = RuleList::from_json(&serde_json::json!([
        {"when": {"path_glob": "tests/**"}, "then": "prune"}
    ]))
    .unwrap();
    let result = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.handler(rules, every(1))
    })
    .await;
    assert!(result.reached(&node("b")));
    assert!(result.reached(&node("tests/t")), "discovered, then cleared");
    assert!(result.cleared.contains(&node("tests/t")));
    assert!(!result.reached(&node("u")), "never expanded");
}

#[tokio::test]
async fn map_switches_and_weights_act_on_later_expansions() {
    let reg = registry(vec![
        GraphMap::new("m1", &[("a", "b"), ("b", "c")]),
        GraphMap::new("m2", &[("a", "x"), ("x", "y")]),
    ]);
    // m2 off at every stop: x is found (before the first stop), y is not.
    let result = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.handler(
            scripted(|_| {
                vec![
                    act(None, Action::MapOff("m2".into())),
                    act(None, Action::MapOff("nope".into())),
                ]
            }),
            every(1),
        )
    })
    .await;
    assert!(result.reached(&node("x")) && result.reached(&node("c")));
    assert!(!result.reached(&node("y")));
    let steer = result.events.iter().find(|e| e["ev"] == "steer").unwrap();
    assert_eq!(steer["applied"], 1, "an unknown map is not applied");

    // Off at the first stop, on again at the second: x is expanded with m2.
    let result = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.handler(
            scripted(|cp| {
                if cp.stop == 1 {
                    vec![act(None, Action::MapOff("m2".into()))]
                } else {
                    vec![act(None, Action::MapOn("m2".into()))]
                }
            }),
            every(1),
        )
    })
    .await;
    assert!(result.reached(&node("y")));

    // A weight above 1 is clamped to 1: nothing changes.
    let result = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.handler(
            scripted(|_| vec![act(None, Action::MapWeight("m1".into(), 5.0))]),
            every(1),
        )
    })
    .await;
    let plain = walk(&reg, vec![seed("a")], budget(10), |t| t).await;
    assert_eq!(popped_names(&result), popped_names(&plain));

    // A weight of 0 prices m1's later hops at nothing.
    let result = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.handler(
            scripted(|_| vec![act(None, Action::MapWeight("m1".into(), 0.0))]),
            every(1),
        )
    })
    .await;
    let c = result.states.iter().find(|s| s.node == node("c")).unwrap();
    assert_eq!(c.potential, 0.0);
}

#[tokio::test]
async fn a_map_the_caller_disabled_cannot_be_switched_on() {
    let mut reg = registry(vec![
        GraphMap::new("m1", &[("a", "b")]),
        GraphMap::new("m2", &[("a", "x"), ("b", "y")]),
    ]);
    reg.set_enabled("m2", false);
    let result = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.handler(
            scripted(|_| vec![act(None, Action::MapOn("m2".into()))]),
            every(1),
        )
    })
    .await;
    assert!(!result.reached(&node("x")));
}

#[tokio::test]
async fn redirect_replaces_the_frontier() {
    let reg = registry(vec![GraphMap::new(
        "g",
        &[("a", "b"), ("a", "c"), ("c", "d"), ("q", "r")],
    )]);
    let result = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.handler(
            scripted(|cp| {
                if cp.stop == 1 {
                    vec![act(None, Action::Redirect(vec![node("q"), node("c")]))]
                } else {
                    Vec::new()
                }
            }),
            every(1),
        )
    })
    .await;
    // b was dropped from the frontier and never expanded; q and c were.
    let names = popped_names(&result);
    assert!(!names.contains(&"src/b.rs".to_string()));
    assert!(result.reached(&node("r")) && result.reached(&node("d")));
    // q, the new seed, starts a path at depth 0.
    let q = result.states.iter().find(|s| s.node == node("q")).unwrap();
    assert_eq!((q.depth, q.parent), (0, None));
}

#[tokio::test]
async fn emit_and_directives_for_unknown_nodes() {
    let result = walk(&fan(), vec![seed("a")], budget(3), |t| {
        t.handler(
            scripted(|_| {
                vec![
                    act(None, Action::Emit("looking at b".into())),
                    // Not in the frontier: ignored.
                    act(Some(&node("zzz")), Action::Prune),
                    // The seed was already expanded: ignored.
                    act(Some(&node("a")), Action::Prune),
                ]
            }),
            every(1),
        )
    })
    .await;
    assert!(result.cleared.is_empty());
    assert!(
        result
            .events
            .iter()
            .any(|e| e["ev"] == "emit" && e["note"] == "looking at b")
    );
    let steer = result.events.iter().find(|e| e["ev"] == "steer").unwrap();
    assert_eq!(steer["applied"], 1);
    assert_eq!(steer["actions"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn stops_fire_every_n_pops_and_at_most_max_stops() {
    let reg = registry(vec![lattice_map("g", 60)]);
    let result = walk(&reg, vec![seed("n0")], budget(50), |t| {
        t.handler(scripted(|_| Vec::new()), StopPolicy::every(5).max_stops(3))
    })
    .await;
    let stops: Vec<u64> = result
        .events
        .iter()
        .filter(|e| e["ev"] == "steer")
        .map(|e| e["stop"].as_u64().unwrap())
        .collect();
    assert_eq!(stops, [1, 2, 3]);
    assert_eq!(result.totals.stops, 3);
    // The stops come after pops 5, 10 and 15.
    let pops_at_steer: Vec<u64> = {
        let mut pops = 0;
        let mut out = Vec::new();
        for e in &result.events {
            match e["ev"].as_str().unwrap() {
                "expand" => pops = e["pop"].as_u64().unwrap(),
                "steer" => out.push(pops),
                _ => {}
            }
        }
        out
    };
    assert_eq!(pops_at_steer, [5, 10, 15]);

    // No handler or no interval: no stops.
    let none = walk(&reg, vec![seed("n0")], budget(20), |t| {
        t.handler(scripted(|_| Vec::new()), every(0))
    })
    .await;
    assert_eq!(none.totals.stops, 0);
}

#[tokio::test]
async fn the_checkpoint_shows_the_frontier_and_the_budget() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = seen.clone();
    let result = walk(&fan(), vec![seed("a")], budget(10), |t| {
        t.handler(
            scripted(move |cp| {
                sink.lock().unwrap().push(cp.clone());
                Vec::new()
            }),
            every(1),
        )
    })
    .await;
    assert_eq!(result.status, TraversalStatus::Complete);
    let checkpoints = seen.lock().unwrap();
    let first = &checkpoints[0];
    assert_eq!((first.pops, first.stop, first.budget_left_pct), (1, 1, 90));
    let nodes: Vec<_> = first.frontier.iter().map(|i| i.node.to_string()).collect();
    assert_eq!(nodes, ["file:src/b.rs", "file:src/c.rs", "file:src/d.rs"]);
    assert_eq!(first.frontier[0].via, Some("g"));
    assert_eq!(first.frontier[0].witness, Some(WitnessClass::Compiler));
    assert_eq!((first.frontier[0].depth, first.frontier[0].fanout), (1, 3));
    assert_eq!(first.visited, 1);
}

#[tokio::test]
async fn no_handler_can_raise_a_budget() {
    // Every action the enum has, from a handler that asks for as much as it can.
    let reg = registry(vec![lattice_map("g", 60)]);
    let result = walk(&reg, vec![seed("n0")], budget(12), |t| {
        t.handler(
            scripted(|cp| {
                let mut out = vec![
                    act(None, Action::MapWeight("g".into(), 1e9)),
                    act(None, Action::Rerank(RerankBy::Witness)),
                    act(None, Action::Emit("x".repeat(10_000))),
                ];
                for item in &cp.frontier {
                    out.push(act(Some(&item.node), Action::Boost(1e300)));
                    out.push(act(Some(&item.node), Action::Expand));
                }
                out
            }),
            every(1),
        )
    })
    .await;
    assert_eq!(result.totals.pops, 12);
    assert_eq!(result.status, TraversalStatus::BudgetExhausted);
    let emit = result.events.iter().find(|e| e["ev"] == "emit").unwrap();
    assert_eq!(emit["note"].as_str().unwrap().len(), handler::EMIT_MAX);
}

#[tokio::test]
async fn a_rule_list_drives_the_engine() {
    let reg = registry(vec![
        GraphMap::new("m1", &[("a", "b"), ("a", "c")]),
        GraphMap::new("m2", &[("b", "d")]),
    ]);
    let rules = RuleList::from_json(&serde_json::json!([
        {"when": {"depth": {"ge": 1}}, "then": "continue"},
        {"when": {"budget_left": {"lt": 95}}, "then": {"map_off": "m2"}},
        {"when": {"budget_left": {"lt": 5}}, "then": "stop"}
    ]))
    .unwrap();
    let result = walk(&reg, vec![seed("a")], budget(10), |t| {
        t.handler(rules, every(1))
    })
    .await;
    // m2 was switched off after the first pop: d is never found.
    assert!(!result.reached(&node("d")));
    assert!(result.totals.steers >= 1);
}

#[tokio::test]
async fn the_unscored_arm_follows_everything_in_offer_order() {
    let reg = registry(vec![lattice_map("g", 20)]);
    let result = walk(&reg, vec![seed("n0")], budget(5), |t| {
        t.scorer(BfsScorer).price(HopPrice { gate: 0, beam: 1 })
    })
    .await;
    // Gate and beam are ignored: every edge of every popped node is followed.
    assert_eq!(result.totals.hops, 6);
}

#[test]
fn hop_pricing_gate_and_beam() {
    let fused: Vec<FusedEdge> = fuse(
        (0..5)
            .map(|i| {
                edge(
                    "a",
                    &format!("n{i}"),
                    "g",
                    WitnessClass::Compiler,
                    90 - 10 * i as u8,
                )
            })
            .collect(),
    );
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
    // Within the gate: everything at value 1.
    let steps = hop(
        &mut RulesScorer,
        &ctx,
        &cands,
        HopPrice { gate: 5, beam: 2 },
    );
    assert_eq!(steps.len(), 5);
    assert!(steps.iter().all(|(_, v)| *v == 1.0));
    // Above it: the best `beam`, valued 1/(1+rank). Higher confidence wins.
    let steps = hop(
        &mut RulesScorer,
        &ctx,
        &cands,
        HopPrice { gate: 2, beam: 3 },
    );
    assert_eq!(steps, [(0, 1.0), (1, 0.5), (2, 1.0 / 3.0)]);
}

#[test]
fn a_slower_or_weaker_map_prices_its_hops_lower() {
    let strong = fuse(vec![edge("a", "b", "fast", WitnessClass::Compiler, 80)]);
    let weak_witness = fuse(vec![edge("a", "c", "fuzzy", WitnessClass::Text, 80)]);
    let fast = MapCard::default();
    let slow = MapCard {
        expand_p50_ms: Some(400.0),
        ..MapCard::default()
    };
    let ctx = ScoreCtx {
        query: &[],
        nodes_seen: 10,
    };
    let score = |edge: &FusedEdge, card: &MapCard| {
        let cands = [Candidate {
            edge,
            in_seen: 0,
            card: card.clone(),
        }];
        RulesScorer.score(&ctx, &cands)[0]
    };
    let base = score(&strong[0], &fast);
    assert!(score(&strong[0], &slow) < base);
    assert!(score(&weak_witness[0], &fast) < base);
}

// ---- trace files and handles (slice S3) ------------------------------------

mod persist {
    use super::*;
    use crate::traverse::handle::{HandleError, HandleStore, Snapshot};
    use crate::traverse::trace::{self, TraceWriter};
    use std::{collections::BTreeSet, path::PathBuf};

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lci-s3-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// Prunes the worst queued node at every stop.
    fn prune_last(cp: &Checkpoint) -> Vec<Directive> {
        cp.frontier
            .last()
            .map(|item| act(Some(&item.node), Action::Prune))
            .into_iter()
            .collect()
    }

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

    async fn pruning_walk(pops: u32) -> TraversalResult {
        let reg = registry(vec![lattice_map("g", 60)]);
        walk(&reg, vec![seed("n0")], budget(pops), |t| {
            t.question("how does n0 reach n9")
                .handler(scripted(prune_last), every(3))
        })
        .await
    }

    /// The events without the call markers and without what depends on the
    /// call's own budget (a checkpoint shows the budget left).
    fn without_marks(events: &[Value]) -> Vec<Value> {
        events
            .iter()
            .filter(|e| {
                !["start", "end", "resume", "stop", "return"].contains(&e["ev"].as_str().unwrap())
            })
            .cloned()
            .map(|mut e| {
                if let Some(obj) = e.as_object_mut() {
                    obj.remove("inputs_hash");
                }
                e
            })
            .collect()
    }

    fn stop_hashes(result: &TraversalResult) -> Vec<String> {
        result
            .events
            .iter()
            .filter(|e| e["ev"] == "stop")
            .map(|e| e["checkpoint_hash"].as_str().unwrap().to_string())
            .collect()
    }

    #[tokio::test]
    async fn a_trace_writes_and_reads_back() {
        let result = pruning_walk(9).await;
        let dir = scratch("roundtrip");
        let path = trace::write_trace(&dir, &result).unwrap();
        assert_eq!(path.extension().unwrap(), "jsonl");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            result.events_jsonl()
        );
        assert_eq!(trace::read_trace(&path).unwrap(), result.events);
        // A resumable run is named by its handle.
        let handle = result.snapshot.as_ref().unwrap().handle();
        assert_eq!(path.file_stem().unwrap().to_str().unwrap(), handle);

        // The writer appends line by line; a bad line is an error naming it.
        let mut writer = TraceWriter::create(dir.join("w.jsonl")).unwrap();
        writer.append(&result.events[0]).unwrap();
        writer.append_all(&result.events[1..3]).unwrap();
        let written = writer.finish().unwrap();
        assert_eq!(trace::read_trace(&written).unwrap(), result.events[..3]);
        std::fs::write(&written, "{}\nnot json\n").unwrap();
        let err = trace::read_trace(&written).unwrap_err();
        assert!(err.to_string().contains("line 2"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stops_and_steers_cite_the_checkpoint_hash() {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = seen.clone();
        let reg = registry(vec![lattice_map("g", 60)]);
        let result = walk(&reg, vec![seed("n0")], budget(12), |t| {
            t.question("how does n0 reach n9").handler(
                scripted(move |cp| {
                    sink.lock().unwrap().push(trace::checkpoint_hash(cp));
                    prune_last(cp)
                }),
                every(3),
            )
        })
        .await;
        let stops = stop_hashes(&result);
        let steers: Vec<String> = result
            .events
            .iter()
            .filter(|e| e["ev"] == "steer")
            .map(|e| e["inputs_hash"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(stops.len(), 4);
        assert_eq!(stops, steers);
        assert_eq!(stops, *seen.lock().unwrap());
        // Different checkpoints, different hashes; the same run, the same.
        let distinct: BTreeSet<&String> = stops.iter().collect();
        assert_eq!(distinct.len(), stops.len());
        assert_eq!(stops, stop_hashes(&pruning_walk(12).await));
    }

    #[tokio::test]
    async fn equal_runs_write_byte_identical_traces_and_handles() {
        let (a, b) = (pruning_walk(8).await, pruning_walk(8).await);
        let (dir_a, dir_b) = (scratch("bytes-a"), scratch("bytes-b"));
        let path_a = trace::write_trace(&dir_a, &a).unwrap();
        let path_b = trace::write_trace(&dir_b, &b).unwrap();
        assert_eq!(path_a.file_name(), path_b.file_name());
        assert_eq!(
            std::fs::read(&path_a).unwrap(),
            std::fs::read(&path_b).unwrap()
        );
        assert_eq!(trace::trace_hash(&a.events), trace::trace_hash(&b.events));
        let (sa, sb) = (a.snapshot.clone().unwrap(), b.snapshot.unwrap());
        assert_eq!(sa.handle(), sb.handle());
        assert_eq!(sa.to_json().to_string(), sb.to_json().to_string());

        // And so do two resumed runs.
        let reg = registry(vec![lattice_map("g", 60)]);
        let ra = resume(&reg, sa, budget(20), |t| {
            t.handler(scripted(prune_last), every(3))
        })
        .await;
        let rb = resume(&reg, sb, budget(20), |t| {
            t.handler(scripted(prune_last), every(3))
        })
        .await;
        assert_eq!(ra.events_jsonl(), rb.events_jsonl());
        // A different run has a different trace.
        let other = pruning_walk(9).await;
        assert_ne!(
            trace::trace_hash(&other.events),
            trace::trace_hash(&a.events)
        );
        let _ = std::fs::remove_dir_all(&dir_a);
        let _ = std::fs::remove_dir_all(&dir_b);
    }

    #[tokio::test]
    async fn a_resume_continues_at_the_same_pop_count_and_matches_a_straight_run() {
        let straight = pruning_walk(12).await;

        let first = pruning_walk(6).await;
        assert_eq!(first.status, TraversalStatus::BudgetExhausted);
        let snapshot = first.snapshot.clone().expect("a paused run has a snapshot");
        assert_eq!(snapshot.pops(), 6);

        let reg = registry(vec![lattice_map("g", 60)]);
        let second = resume(&reg, snapshot, budget(12), |t| {
            t.handler(scripted(prune_last), every(3))
        })
        .await;
        let resumed = second
            .events
            .iter()
            .find(|e| e["ev"] == "resume")
            .expect("the resume is in the trace");
        assert_eq!(resumed["pops"], 6);
        assert_eq!(second.events[..first.events.len()], first.events[..]);
        assert_eq!(second.totals.pops, 12);

        assert_eq!(second.status, straight.status);
        assert_eq!(second.popped, straight.popped);
        assert_eq!(second.states, straight.states);
        assert_eq!(second.cleared, straight.cleared);
        let (a, b) = (&second.totals, &straight.totals);
        assert_eq!(
            (
                a.pops,
                a.hops,
                a.cross_map_hops,
                a.tokens,
                a.stops,
                a.steers
            ),
            (
                b.pops,
                b.hops,
                b.cross_map_hops,
                b.tokens,
                b.stops,
                b.steers
            )
        );
        assert_eq!(a.per_map, b.per_map);
        assert_eq!(
            without_marks(&second.events),
            without_marks(&straight.events)
        );
        // The stop at pop 6 fired once, in the first call. The first call's
        // stops saw a budget of 6 pops, so their checkpoints (and hashes)
        // differ from the straight run's; the stops of the second call saw
        // the same budget and the same state.
        let (hs, hr) = (stop_hashes(&second), stop_hashes(&straight));
        assert_eq!((hs.len(), hr.len()), (4, 4));
        assert_ne!(hs[..2], hr[..2]);
        assert_eq!(hs[2..], hr[2..]);
    }

    #[tokio::test]
    async fn a_handler_stop_returns_a_handle_that_resumes_with_the_switches_kept() {
        let reg = registry(vec![
            GraphMap::new("m1", &[("a", "b"), ("b", "c"), ("c", "d")]),
            GraphMap::new("m2", &[("a", "x"), ("x", "y")]),
        ]);
        fn steer(cp: &Checkpoint) -> Vec<Directive> {
            if cp.stop == 1 {
                vec![
                    act(None, Action::MapOff("m2".into())),
                    act(None, Action::MapWeight("m1".into(), 0.5)),
                ]
            } else {
                Vec::new()
            }
        }
        let straight = walk(&reg, vec![seed("a")], budget(10), |t| {
            t.handler(scripted(steer), every(1))
        })
        .await;
        assert!(!straight.reached(&node("y")));

        let first = walk(&reg, vec![seed("a")], budget(10), |t| {
            t.handler(
                scripted(|cp| {
                    let mut out = steer(cp);
                    out.push(act(None, Action::Stop));
                    out
                }),
                every(1),
            )
        })
        .await;
        assert_eq!(first.status, TraversalStatus::Stopped);
        let dir = scratch("store");
        let handle = HandleStore::in_dir(&dir)
            .put(first.snapshot.as_ref().unwrap())
            .unwrap();

        // A fresh store reads the file, as a new process would.
        let snapshot = HandleStore::in_dir(&dir).get(&handle).unwrap();
        assert_eq!(&snapshot, first.snapshot.as_ref().unwrap());
        let second = resume(&reg, snapshot, budget(10), |t| t).await;
        assert_eq!(second.states, straight.states);
        assert_eq!(second.popped, straight.popped);
        assert!(!second.reached(&node("y")), "the m2 switch survives");
        let c = second.states.iter().find(|s| s.node == node("c")).unwrap();
        assert!(c.potential < 1.0, "the m1 weight survives");
        assert_eq!(second.status, TraversalStatus::Complete);
        assert!(second.snapshot.is_none(), "a finished walk has no handle");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn the_handle_store_checks_what_it_reads() {
        let first = pruning_walk(6).await;
        let snapshot = first.snapshot.unwrap();
        let dir = scratch("checks");

        let memory = HandleStore::memory();
        let handle = memory.put(&snapshot).unwrap();
        assert_eq!(handle, snapshot.handle());
        assert_eq!(memory.put(&snapshot).unwrap(), handle);
        assert_eq!(memory.get(&handle).unwrap(), snapshot);
        assert!(matches!(
            memory.get("h0000000000000000"),
            Err(HandleError::Unknown(_))
        ));
        for bad in ["../x", "", "hABCDEF0123456789", "h123", "x0000000000000000"] {
            assert!(
                matches!(memory.get(bad), Err(HandleError::BadHandle(_))),
                "{bad}"
            );
        }

        HandleStore::in_dir(&dir).put(&snapshot).unwrap();
        let file = dir.join(format!("{handle}.json"));
        let text = std::fs::read_to_string(&file).unwrap();
        assert_eq!(HandleStore::in_dir(&dir).get(&handle).unwrap(), snapshot);

        // Edited content no longer hashes to its name.
        let edited = text.replacen("\"depth\":1", "\"depth\":2", 1);
        assert_ne!(edited, text);
        std::fs::write(&file, edited).unwrap();
        assert!(matches!(
            HandleStore::in_dir(&dir).get(&handle),
            Err(HandleError::Corrupt(_))
        ));
        // A state naming a parent that does not precede it is refused.
        let mut value: Value = serde_json::from_str(&text).unwrap();
        value["states"][1]["parent"] = 7.into();
        std::fs::write(&file, value.to_string()).unwrap();
        assert!(matches!(
            HandleStore::in_dir(&dir).get(&handle),
            Err(HandleError::Corrupt(_))
        ));
        std::fs::write(&file, "{ torn").unwrap();
        assert!(matches!(
            HandleStore::in_dir(&dir).get(&handle),
            Err(HandleError::Corrupt(_))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn only_a_walk_with_work_left_leaves_a_handle() {
        let reg = registry(vec![GraphMap::new("g", &[("a", "b")])]);
        let done = walk(&reg, vec![seed("a")], budget(10), |t| t).await;
        assert_eq!(done.status, TraversalStatus::Complete);
        assert!(done.snapshot.is_none());
        let cut = walk(&reg, vec![seed("a")], budget(1), |t| t).await;
        assert_eq!(cut.status, TraversalStatus::BudgetExhausted);
        assert!(cut.snapshot.is_some());
    }
}

// ---- slice S4: stops, the ladder, rung 3, the steer continuation ---------------

mod steering {
    use super::*;
    use crate::traverse::{
        handle::{HandleStore, Snapshot},
        handler::{RAISE_MAX_POPS, RAISE_MAX_TOKENS},
        ladder::{Ladder, LadderError},
        steer::{MAX_STEER_ACTIONS, Steer, SteerError},
        stop::{self, CARD_CHARS, Card},
        trace,
    };
    use serde_json::json;
    use std::{
        collections::{BTreeMap, BTreeSet},
        path::PathBuf,
        sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
    };

    /// A handler on a chosen rung with a canned answer; counts its calls.
    struct Rung {
        id: &'static str,
        rung: u8,
        confidence: f32,
        actions: Vec<Directive>,
        calls: Arc<AtomicUsize>,
    }

    impl Handler for Rung {
        fn id(&self) -> &str {
            self.id
        }

        fn rung(&self) -> u8 {
            self.rung
        }

        fn decide(&self, _checkpoint: &Checkpoint, _budget: &StopBudget) -> Decision {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Decision {
                actions: self.actions.clone(),
                confidence: self.confidence,
                ..Decision::default()
            }
        }
    }

    fn rung(
        id: &'static str,
        rung: u8,
        confidence: f32,
        actions: Vec<Directive>,
    ) -> (Rung, Arc<AtomicUsize>) {
        let calls = Arc::new(AtomicUsize::new(0));
        let handler = Rung {
            id,
            rung,
            confidence,
            actions,
            calls: calls.clone(),
        };
        (handler, calls)
    }

    /// A System One stand-in: rung 2, as S8 will fill it.
    fn system_one_double(confidence: f32, actions: Vec<Directive>) -> (Rung, Arc<AtomicUsize>) {
        rung("system_one_double", 2, confidence, actions)
    }

    fn no_rules() -> RuleList {
        RuleList::new(Vec::new()).unwrap()
    }

    /// Matches every frontier node and changes nothing: rung 1 is sure.
    fn sure_rules() -> RuleList {
        RuleList::from_json(&json!([{"when": {"depth": {"ge": 0}}, "then": "continue"}])).unwrap()
    }

    fn ladder(tau: f32, rungs: Vec<Box<dyn Handler>>) -> Ladder {
        Ladder::new(tau, rungs).unwrap()
    }

    fn of<'a>(result: &'a TraversalResult, ev: &str) -> Vec<&'a Value> {
        result.events.iter().filter(|e| e["ev"] == ev).collect()
    }

    fn texts(events: &[&Value], key: &str) -> Vec<String> {
        events
            .iter()
            .map(|e| match &e[key] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect()
    }

    /// `(pops, reason)` of every stop.
    fn stops_of(result: &TraversalResult) -> Vec<(u64, String)> {
        of(result, "stop")
            .iter()
            .map(|e| {
                (
                    e["pops"].as_u64().unwrap(),
                    e["reason"].as_str().unwrap().to_string(),
                )
            })
            .collect()
    }

    async fn continue_from(
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

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lci-s4-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn noop() -> impl Handler {
        scripted(|_| Vec::new())
    }

    /// Runs a handler and keeps every checkpoint it saw.
    struct Observe<H>(H, Arc<Mutex<Vec<Checkpoint>>>);

    impl<H: Handler> Handler for Observe<H> {
        fn id(&self) -> &str {
            self.0.id()
        }

        fn decide(&self, checkpoint: &Checkpoint, budget: &StopBudget) -> Decision {
            self.1.lock().unwrap().push(checkpoint.clone());
            self.0.decide(checkpoint, budget)
        }
    }

    type Seen = Arc<Mutex<Vec<Checkpoint>>>;

    fn recorder() -> (Seen, Seen) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        (seen.clone(), seen)
    }

    // ---- triggers ------------------------------------------------------------

    #[tokio::test]
    async fn every_pops_fires_with_its_reason() {
        let reg = registry(vec![lattice_map("g", 60)]);
        let result = walk(&reg, vec![seed("n0")], budget(20), |t| {
            t.handler(noop(), every(4).max_stops(2))
        })
        .await;
        assert_eq!(
            stops_of(&result),
            [(4, "every_pops".to_string()), (8, "every_pops".to_string())]
        );
    }

    #[tokio::test]
    async fn budget_share_fires_at_each_share_of_the_pop_budget() {
        let reg = registry(vec![lattice_map("g", 60)]);
        let policy = StopPolicy::default().budget_share(25).max_stops(10);
        let result = walk(&reg, vec![seed("n0")], budget(20), |t| {
            t.handler(noop(), policy)
        })
        .await;
        let share = "budget_share".to_string();
        let stops = stops_of(&result);
        assert_eq!(stops.len(), 4, "{stops:?}");
        assert_eq!(stops[0], (5, share.clone()));
        assert_eq!(stops[1], (10, share.clone()));
        assert_eq!(stops[2], (15, share));
    }

    #[tokio::test]
    async fn disagreement_fires_when_a_fuzzy_map_offers_what_the_structural_map_does_not() {
        let (seen, sink) = recorder();
        let reg = registry(vec![
            GraphMap::new("s", &[("a", "b")]),
            GraphMap::new("f", &[("a", "b"), ("a", "x")]).witness(WitnessClass::Vector),
        ]);
        let result = walk(&reg, vec![seed("a")], budget(10), |t| {
            t.handler(
                Observe(noop(), sink),
                StopPolicy::default().on_disagreement(1),
            )
        })
        .await;
        assert_eq!(stops_of(&result), [(1, "disagreement".to_string())]);
        let cp = seen.lock().unwrap()[0].clone();
        assert_eq!(cp.disagreements_total, 1);
        let d = &cp.disagreements[0];
        assert_eq!((d.node.clone(), d.src.clone()), (node("x"), node("a")));
        assert_eq!(d.offered_by, ["f"]);
        assert_eq!(d.silent, ["s"]);
        assert_eq!(d.pop, 1);
    }

    #[tokio::test]
    async fn without_a_structural_answer_nothing_disagrees() {
        let reg = registry(vec![
            GraphMap::new("f", &[("a", "b"), ("a", "x")]).witness(WitnessClass::Vector),
        ]);
        let result = walk(&reg, vec![seed("a")], budget(10), |t| {
            t.handler(noop(), StopPolicy::default().on_disagreement(1))
        })
        .await;
        assert!(stops_of(&result).is_empty());
    }

    #[tokio::test]
    async fn stagnation_fires_after_pops_that_found_nothing_new() {
        let reg = registry(vec![GraphMap::new(
            "g",
            &[
                ("a", "b"),
                ("a", "c"),
                ("a", "d"),
                ("b", "a"),
                ("c", "a"),
                ("d", "a"),
            ],
        )]);
        let result = walk(&reg, vec![seed("a")], budget(10), |t| {
            t.handler(noop(), StopPolicy::default().on_stagnation(2))
        })
        .await;
        assert_eq!(stops_of(&result), [(3, "stagnation".to_string())]);
    }

    #[tokio::test]
    async fn likely_complete_fires_once_when_two_maps_agree_on_what_they_found() {
        let edges = [("a", "b"), ("a", "c"), ("a", "d"), ("a", "e")];
        let reg = registry(vec![
            GraphMap::new("m1", &edges),
            GraphMap::new("m2", &edges),
        ]);
        let result = walk(&reg, vec![seed("a")], budget(10), |t| {
            t.handler(noop(), StopPolicy::default().likely_complete(90))
        })
        .await;
        assert_eq!(stops_of(&result), [(1, "likely_complete".to_string())]);

        let apart = registry(vec![
            GraphMap::new("m1", &[("a", "b"), ("a", "c")]),
            GraphMap::new("m2", &[("a", "d"), ("a", "e")]),
        ]);
        let none = walk(&apart, vec![seed("a")], budget(10), |t| {
            t.handler(noop(), StopPolicy::default().likely_complete(1))
        })
        .await;
        assert!(stops_of(&none).is_empty());
    }

    #[tokio::test]
    async fn several_triggers_at_once_make_one_stop_and_max_stops_bounds_them_all() {
        let reg = registry(vec![lattice_map("g", 60)]);
        let policy = StopPolicy::every(5).budget_share(25).max_stops(10);
        let result = walk(&reg, vec![seed("n0")], budget(20), |t| {
            t.handler(noop(), policy)
        })
        .await;
        let stops = of(&result, "stop");
        assert_eq!(stops[0]["pops"], 5);
        assert_eq!(stops[0]["reason"], "every_pops");
        assert_eq!(stops[0]["triggers"], json!(["every_pops", "budget_share"]));

        let capped = walk(&reg, vec![seed("n0")], budget(20), |t| {
            t.handler(noop(), StopPolicy::every(5).budget_share(25).max_stops(2))
        })
        .await;
        assert_eq!(capped.totals.stops, 2);
    }

    #[tokio::test]
    async fn a_policy_with_no_trigger_never_stops() {
        let reg = registry(vec![lattice_map("g", 60)]);
        let result = walk(&reg, vec![seed("n0")], budget(10), |t| {
            t.handler(noop(), StopPolicy::default())
        })
        .await;
        assert_eq!(result.totals.stops, 0);
        assert!(!StopPolicy::default().is_active());
    }

    // ---- the checkpoint payload ------------------------------------------------

    #[tokio::test]
    async fn the_checkpoint_carries_cards_regions_costs_and_the_rest() {
        let (seen, sink) = recorder();
        let reg = registry(vec![
            GraphMap::new("g", &[("a", "b"), ("a", "c"), ("a", "tests/t"), ("b", "e")]).cost(5, 0),
        ]);
        walk(&reg, vec![seed("a")], budget(10), |t| {
            t.question("how does a reach e")
                .handler(Observe(noop(), sink), every(2))
        })
        .await;
        let cp = seen.lock().unwrap()[0].clone();
        assert_eq!(cp.reason, stop::StopReason::EveryPops);
        assert_eq!(cp.question, "how does a reach e");
        assert_eq!(cp.seeds, [node("a")]);
        assert_eq!((cp.pops, cp.stop, cp.pops_left), (2, 1, 8));
        assert!(
            cp.handle.is_none(),
            "the handle belongs to a returned checkpoint"
        );

        let visited: Vec<_> = cp.visited_cards.iter().map(|c| c.node.clone()).collect();
        assert_eq!(visited, [node("a"), node("b")]);
        assert_eq!(cp.visited_cards[1].via, Some("g"));
        assert_eq!(cp.visited_cards[1].witness, Some(WitnessClass::Compiler));
        let answer: Vec<_> = cp.answer.iter().map(|c| c.node.clone()).collect();
        assert!(!answer.contains(&node("a")), "the seed is not an answer");
        assert!(answer.contains(&node("b")));

        assert_eq!(cp.frontier.len(), 3);
        assert!(
            cp.frontier
                .iter()
                .all(|i| i.est_cost > 0 && i.potential > 0.0)
        );
        let (id, stat) = &cp.maps[0];
        assert_eq!(*id, "g");
        assert_eq!((stat.calls, stat.failures, stat.edges), (2, 0, 4));
        assert_eq!(stat.tokens, 10);
        assert_eq!(
            (cp.regions.explored, cp.regions.seen, cp.regions.cleared),
            (2, 3, 0)
        );
        assert_eq!(cp.regions.unknown, None);
        assert!(
            cp.regions
                .rows
                .iter()
                .any(|r| r.path == "tests" && r.seen == 1)
        );
        assert_eq!(cp.disagreements_total, 0);
        assert!(cp.likely_complete.is_none());
        assert_eq!((cp.visited, cp.cleared), (2, 0));
    }

    #[tokio::test]
    async fn cleared_nodes_show_in_the_regions() {
        let (seen, sink) = recorder();
        let reg = registry(vec![GraphMap::new(
            "g",
            &[("a", "b"), ("b", "c"), ("a", "tests/t"), ("a", "tests/u")],
        )]);
        let rules = RuleList::from_json(&json!([
            {"when": {"path_glob": "tests/**"}, "then": "prune"}
        ]))
        .unwrap();
        walk(&reg, vec![seed("a")], budget(10), |t| {
            t.handler(Observe(rules, sink), every(1))
        })
        .await;
        let later = seen.lock().unwrap();
        let cp = later.last().unwrap();
        assert_eq!(cp.cleared, 2);
        assert_eq!(cp.regions.cleared, 2);
    }

    #[tokio::test]
    async fn two_maps_re_finds_make_the_likely_complete_estimate() {
        let (seen, sink) = recorder();
        let reg = registry(vec![
            GraphMap::new("m1", &[("a", "b"), ("a", "c"), ("a", "d"), ("a", "e")]),
            GraphMap::new("m2", &[("a", "d"), ("a", "e"), ("a", "f"), ("a", "h")]),
        ]);
        walk(&reg, vec![seed("a")], budget(10), |t| {
            t.handler(Observe(noop(), sink), every(1).max_stops(1))
        })
        .await;
        let cp = seen.lock().unwrap()[0].clone();
        let lc = cp.likely_complete.expect("the maps share nodes");
        assert_eq!(lc.maps, ["m1", "m2"]);
        assert_eq!((lc.refound, lc.found), (2, 6));
        assert!(lc.fraction > 0.0 && lc.fraction <= 1.0);
    }

    #[test]
    fn the_estimate_is_lincoln_petersen_over_the_two_largest_finders() {
        let set = |names: &[&str]| -> BTreeSet<NodeId> { names.iter().map(|n| node(n)).collect() };
        let mut found: BTreeMap<MapId, BTreeSet<NodeId>> = BTreeMap::new();
        assert!(stop::likely_complete(&found).is_none());
        found.insert(
            "a",
            set(&["1", "2", "3", "4", "5", "6", "7", "8", "9", "10"]),
        );
        assert!(
            stop::likely_complete(&found).is_none(),
            "one map is not a recapture"
        );
        found.insert(
            "b",
            set(&["6", "7", "8", "9", "10", "11", "12", "13", "14", "15"]),
        );
        let lc = stop::likely_complete(&found).unwrap();
        assert_eq!((lc.n1, lc.n2, lc.refound, lc.found), (10, 10, 5, 15));
        assert_eq!((lc.estimate, lc.fraction), (20.0, 0.75));
        found.insert("c", set(&["1", "20"]));
        assert_eq!(stop::likely_complete(&found).unwrap().maps, ["a", "b"]);
        let same: BTreeMap<MapId, BTreeSet<NodeId>> =
            [("x", set(&["1", "2"])), ("y", set(&["1", "2"]))]
                .into_iter()
                .collect();
        assert_eq!(stop::likely_complete(&same).unwrap().fraction, 1.0);
        let apart: BTreeMap<MapId, BTreeSet<NodeId>> = [("x", set(&["1"])), ("y", set(&["2"]))]
            .into_iter()
            .collect();
        assert!(stop::likely_complete(&apart).is_none());
    }

    #[test]
    fn a_card_is_one_short_line() {
        let card = Card {
            node: node("a"),
            via: Some("scip"),
            witness: Some(WitnessClass::Compiler),
            potential: 0.5,
            depth: 2,
        };
        assert_eq!(card.text(), "file:src/a.rs | scip/compiler | p0.50 d2");
        let long = Card {
            node: NodeId::file(format!("src/{}/deep.rs", "very_long_directory/".repeat(30)))
                .unwrap(),
            ..card
        };
        let text = long.text();
        assert!(text.chars().count() <= CARD_CHARS, "{text}");
        assert!(text.starts_with('~') && text.ends_with("deep.rs | scip/compiler | p0.50 d2"));
    }

    #[tokio::test]
    async fn equal_runs_give_equal_checkpoint_hashes() {
        let run = || async {
            let reg = registry(vec![
                GraphMap::new("s", &[("a", "b"), ("b", "c")]),
                GraphMap::new("f", &[("a", "b"), ("a", "x")]).witness(WitnessClass::Vector),
            ]);
            walk(&reg, vec![seed("a")], budget(10), |t| {
                t.question("q").handler(noop(), every(1))
            })
            .await
        };
        let (a, b) = (run().await, run().await);
        let hashes = |r: &TraversalResult| texts(&of(r, "stop"), "checkpoint_hash");
        assert!(!hashes(&a).is_empty());
        assert_eq!(hashes(&a), hashes(&b));
        assert_eq!(a.events_jsonl(), b.events_jsonl());
        let distinct: BTreeSet<_> = hashes(&a).into_iter().collect();
        assert_eq!(distinct.len(), hashes(&a).len());
    }

    // ---- the ladder ----------------------------------------------------------------

    #[test]
    fn a_ladder_checks_tau_and_the_order_of_its_rungs() {
        let rules = || -> Box<dyn Handler> { Box::new(no_rules()) };
        let one = || -> Box<dyn Handler> { Box::new(system_one_double(1.0, Vec::new()).0) };
        let agent = || -> Box<dyn Handler> { Box::new(rung("liar", 3, 1.0, Vec::new()).0) };
        assert!(Ladder::new(0.8, vec![rules(), one()]).is_ok());
        assert!(Ladder::new(0.8, Vec::new()).is_ok());
        assert_eq!(
            Ladder::new(1.5, vec![rules()]).err(),
            Some(LadderError::BadTau)
        );
        assert_eq!(
            Ladder::new(f32::NAN, vec![rules()]).err(),
            Some(LadderError::BadTau)
        );
        assert_eq!(
            Ladder::new(0.8, vec![one(), rules()]).err(),
            Some(LadderError::BadRungs(vec![2, 1]))
        );
        assert_eq!(
            Ladder::new(0.8, vec![rules(), rules()]).err(),
            Some(LadderError::BadRungs(vec![1, 1]))
        );
        assert_eq!(
            Ladder::new(0.8, vec![agent()]).err(),
            Some(LadderError::BadRungs(vec![3]))
        );
    }

    #[tokio::test]
    async fn a_rule_list_that_matches_nothing_escalates_in_order_to_the_agent() {
        let (double, double_calls) = system_one_double(0.3, Vec::new());
        let rungs: Vec<Box<dyn Handler>> = vec![Box::new(no_rules()), Box::new(double)];
        let result = walk(&fan(), vec![seed("a")], budget(10), |t| {
            t.handler(ladder(0.8, rungs), every(1))
        })
        .await;
        assert_eq!(result.status, TraversalStatus::Stopped);
        assert_eq!(result.totals.pops, 1);
        assert_eq!(double_calls.load(Ordering::SeqCst), 1);

        let steers = of(&result, "steer");
        let order: Vec<(u64, &str, &str)> = steers
            .iter()
            .map(|e| {
                (
                    e["rung"].as_u64().unwrap(),
                    e["handler"].as_str().unwrap(),
                    e["outcome"].as_str().unwrap(),
                )
            })
            .collect();
        assert_eq!(
            order,
            [
                (1, "rules", "uncertain"),
                (2, "system_one_double", "uncertain"),
                (3, "agent", "returned"),
            ]
        );
        let checkpoint = result
            .checkpoint
            .as_ref()
            .expect("rung 3 returns a checkpoint");
        let hash = trace::checkpoint_hash(checkpoint);
        assert_eq!(texts(&steers, "inputs_hash"), [hash.as_str(); 3]);
        assert_eq!(
            texts(&of(&result, "stop"), "checkpoint_hash"),
            [hash.as_str()]
        );
        let ret = of(&result, "return");
        assert_eq!(ret.len(), 1);
        assert_eq!(ret[0]["reason"], "escalated");
        assert_eq!(ret[0]["checkpoint_hash"], hash.as_str());
        assert_eq!(result.totals.steers, 0, "nothing was applied");
        let snapshot = result.snapshot.as_ref().unwrap();
        assert_eq!(
            checkpoint.handle.as_deref(),
            Some(snapshot.handle().as_str())
        );
        assert_eq!(snapshot.returned.as_deref(), Some(hash.as_str()));
    }

    #[tokio::test]
    async fn a_confident_rule_stops_the_escalation() {
        let (double, double_calls) = system_one_double(1.0, Vec::new());
        let rungs: Vec<Box<dyn Handler>> = vec![Box::new(sure_rules()), Box::new(double)];
        let result = walk(&fan(), vec![seed("a")], budget(10), |t| {
            t.handler(ladder(0.8, rungs), every(1))
        })
        .await;
        assert_eq!(result.status, TraversalStatus::Complete);
        assert_eq!(
            double_calls.load(Ordering::SeqCst),
            0,
            "rung 2 was never asked"
        );
        let steers = of(&result, "steer");
        assert!(!steers.is_empty());
        assert!(
            steers
                .iter()
                .all(|e| e["rung"] == 1 && e["outcome"] == "applied")
        );
        assert!(result.checkpoint.is_none() && result.snapshot.is_none());
        assert!(of(&result, "return").is_empty());
    }

    #[tokio::test]
    async fn a_confident_system_one_decides_when_the_rules_are_unsure() {
        let (double, _) = system_one_double(0.9, vec![act(Some(&node("d")), Action::Prune)]);
        let rungs: Vec<Box<dyn Handler>> = vec![Box::new(no_rules()), Box::new(double)];
        let result = walk(&fan(), vec![seed("a")], budget(10), |t| {
            t.handler(ladder(0.8, rungs), every(1).max_stops(1))
        })
        .await;
        assert_eq!(result.status, TraversalStatus::Complete);
        assert!(result.cleared.contains(&node("d")));
        assert!(result.reached(&node("e")) && !result.reached(&node("h")));
        let steers = of(&result, "steer");
        assert_eq!(texts(&steers, "outcome"), ["uncertain", "applied"]);
        assert_eq!(steers[0]["rung"], 1);
        assert_eq!(steers[1]["rung"], 2);
        assert_eq!(steers[1]["applied"], 1);
        assert_eq!(result.totals.steers, 1);
    }

    #[tokio::test]
    async fn the_proposals_of_an_unsure_rung_are_not_applied() {
        let (double, _) = system_one_double(0.1, vec![act(Some(&node("d")), Action::Prune)]);
        let rungs: Vec<Box<dyn Handler>> = vec![Box::new(no_rules()), Box::new(double)];
        let result = walk(&fan(), vec![seed("a")], budget(10), |t| {
            t.handler(ladder(0.8, rungs), every(1))
        })
        .await;
        assert_eq!(result.status, TraversalStatus::Stopped);
        assert!(result.cleared.is_empty());
        let steers = of(&result, "steer");
        assert_eq!(steers[1]["actions"][0]["action"], "prune");
        assert_eq!(steers[1]["applied"], 0);
    }

    #[tokio::test]
    async fn rules_that_contradict_each_other_are_unsure() {
        let rules = RuleList::from_json(&json!([
            {"when": {"path_glob": "src/d.rs"}, "then": "prune"},
            {"when": {"kind": "file"}, "then": {"boost": 2.0}}
        ]))
        .unwrap();
        let (double, double_calls) = system_one_double(0.95, Vec::new());
        let rungs: Vec<Box<dyn Handler>> = vec![Box::new(rules), Box::new(double)];
        let result = walk(&fan(), vec![seed("a")], budget(10), |t| {
            t.handler(ladder(0.8, rungs), every(1).max_stops(1))
        })
        .await;
        assert_eq!(double_calls.load(Ordering::SeqCst), 1);
        assert!(result.cleared.is_empty(), "rung 1's prune was not applied");
        let steers = of(&result, "steer");
        assert_eq!(texts(&steers, "outcome"), ["uncertain", "applied"]);
        assert_eq!(steers[0]["rung"], 1);
    }

    #[tokio::test]
    async fn the_agent_is_asked_only_when_the_rungs_are_unsure_or_a_budget_is_hit() {
        let sure = ladder(0.8, vec![Box::new(sure_rules())]);
        let done = walk(&fan(), vec![seed("a")], budget(20), |t| {
            t.handler(sure, every(2))
        })
        .await;
        assert_eq!(done.status, TraversalStatus::Complete);
        assert!(done.checkpoint.is_none());
        assert!(of(&done, "steer").iter().all(|e| e["rung"] != 3));

        let sure = ladder(0.8, vec![Box::new(sure_rules())]);
        let cut = walk(&fan(), vec![seed("a")], budget(3), |t| {
            t.handler(sure, every(100))
        })
        .await;
        assert_eq!(cut.status, TraversalStatus::BudgetExhausted);
        let checkpoint = cut.checkpoint.as_ref().expect("a budget hit is returned");
        assert_eq!(checkpoint.reason, stop::StopReason::Budget);
        assert_eq!(checkpoint.pops, 3);
        assert_eq!(of(&cut, "return")[0]["reason"], "budget");
        assert!(of(&cut, "steer").is_empty(), "no rung was consulted");
        assert!(of(&cut, "stop").is_empty());
        assert_eq!(
            checkpoint.handle.as_deref(),
            Some(cut.snapshot.as_ref().unwrap().handle().as_str())
        );
    }

    // ---- rung 3: the steer continuation ----------------------------------------

    #[tokio::test]
    async fn rung_three_round_trips_through_the_handle_with_a_steer() {
        let reg = fan();
        let dir = scratch("roundtrip");
        let first = walk(&reg, vec![seed("a")], budget(10), |t| {
            t.question("how does a reach h")
                .handler(ladder(0.8, vec![Box::new(no_rules())]), every(1))
        })
        .await;
        assert_eq!(first.status, TraversalStatus::Stopped);
        let checkpoint = first.checkpoint.clone().unwrap();
        let queued: Vec<_> = checkpoint.frontier.iter().map(|i| i.node.clone()).collect();
        assert_eq!(queued, [node("b"), node("c"), node("d")]);
        let handle = checkpoint.handle.clone().unwrap();
        assert_eq!(
            HandleStore::in_dir(&dir)
                .put(first.snapshot.as_ref().unwrap())
                .unwrap(),
            handle
        );

        let steer = Steer::new(vec![
            act(Some(&node("b")), Action::Prune),
            act(Some(&node("d")), Action::Expand),
        ]);
        let snapshot = HandleStore::in_dir(&dir).get(&handle).unwrap();
        let second = continue_from(&reg, snapshot, budget(10), |t| {
            t.handler(ladder(0.8, vec![Box::new(sure_rules())]), every(1))
                .steer(steer)
        })
        .await;
        assert_eq!(second.events[..first.events.len()], first.events[..]);
        assert_eq!(second.status, TraversalStatus::Complete);

        let order = popped_names(&second);
        assert_eq!(&order[..2], ["src/a.rs", "src/d.rs"]);
        assert!(!order.contains(&"src/b.rs".to_string()));
        assert!(second.cleared.contains(&node("b")) && !second.reached(&node("e")));
        assert!(second.reached(&node("h")));

        let hash = trace::checkpoint_hash(&checkpoint);
        let mine: Vec<&Value> = of(&second, "steer")
            .into_iter()
            .filter(|e| e["rung"] == 3 && e["outcome"] == "applied")
            .collect();
        assert_eq!(mine.len(), 1);
        assert_eq!(mine[0]["handler"], "agent");
        assert_eq!(mine[0]["inputs_hash"], hash.as_str());
        assert_eq!(mine[0]["applied"], 2);
        let resume_at = second
            .events
            .iter()
            .position(|e| e["ev"] == "resume")
            .unwrap();
        let steer_at = second.events.iter().position(|e| e == mine[0]).unwrap();
        assert!(resume_at < steer_at);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn a_steer_can_stop_the_walk_and_leaves_a_new_handle() {
        let reg = fan();
        let first = walk(&reg, vec![seed("a")], budget(10), |t| {
            t.handler(ladder(0.8, vec![Box::new(no_rules())]), every(1))
        })
        .await;
        let second = continue_from(&reg, first.snapshot.clone().unwrap(), budget(10), |t| {
            t.steer(Steer::new(vec![act(None, Action::Stop)]))
        })
        .await;
        assert_eq!(second.status, TraversalStatus::Stopped);
        assert_eq!(second.totals.pops, 1);
        assert_eq!(
            second.checkpoint.as_ref().unwrap().reason,
            stop::StopReason::Steer
        );
        assert_eq!(of(&second, "return").last().unwrap()["reason"], "steer");
        assert_ne!(
            second.snapshot.unwrap().handle(),
            first.snapshot.unwrap().handle()
        );
    }

    #[tokio::test]
    async fn a_steer_redirects_and_switches_maps_like_any_rung() {
        let reg = registry(vec![
            GraphMap::new("m1", &[("a", "b"), ("b", "c")]),
            GraphMap::new("m2", &[("a", "x"), ("x", "y")]),
        ]);
        let first = walk(&reg, vec![seed("a")], budget(10), |t| {
            t.handler(ladder(0.8, Vec::new()), every(1))
        })
        .await;
        assert_eq!(first.status, TraversalStatus::Stopped);
        let steer = Steer::new(vec![
            act(None, Action::MapOff("m2".into())),
            act(None, Action::Redirect(vec![node("b")])),
        ]);
        let second = continue_from(&reg, first.snapshot.unwrap(), budget(10), |t| {
            t.steer(steer)
        })
        .await;
        assert!(second.reached(&node("c")), "redirected to b");
        assert!(
            !second.reached(&node("y")),
            "x was dropped from the frontier"
        );
        assert_eq!(second.status, TraversalStatus::Complete);
    }

    #[test]
    fn a_steer_parses_from_json_under_the_rule_list_bounds() {
        let ok = Steer::from_json(&json!({"actions": [
            {"node": "file:src/a.rs", "action": "prune"},
            {"node": "file:src/b.rs", "action": {"boost": 2.0}},
            {"action": {"map_off": "vector"}},
            {"action": {"rerank": "depth"}},
            {"action": {"redirect": ["file:src/c.rs"]}},
            {"action": {"budget": {"pops": 20, "tokens": 2000}}},
            {"action": {"budget": {"pops": 5}}},
            {"action": "stop"}
        ]}))
        .unwrap();
        let names: Vec<_> = ok.actions.iter().map(|d| d.action.name()).collect();
        assert_eq!(
            names,
            [
                "prune", "boost", "map_off", "rerank", "redirect", "budget", "budget", "stop"
            ]
        );
        assert_eq!(ok.actions[0].node, Some(node("a")));
        assert_eq!(
            ok.actions[5].action,
            Action::Budget {
                pops: 20,
                tokens: 2000
            }
        );
        assert_eq!(ok.actions[6].action, Action::Budget { pops: 5, tokens: 0 });

        let bad = |value: Value| Steer::from_json(&value).unwrap_err();
        assert_eq!(bad(json!([])), SteerError::NotAnObject);
        let many = vec![json!({"action": "stop"}); MAX_STEER_ACTIONS + 1];
        assert!(matches!(
            bad(json!({"actions": many})),
            SteerError::TooManyActions(_)
        ));
        for value in [
            json!({"action": "prune"}),
            json!({"node": "file:src/a.rs", "action": {"map_off": "m"}}),
            json!({"node": "file:src/a.rs", "action": "dance"}),
            json!({"node": "nonsense", "action": "prune"}),
            json!({"action": {"budget": {}}}),
            json!({"action": {"budget": {"pops": -1}}}),
        ] {
            assert!(
                matches!(
                    bad(json!({"actions": [value.clone()]})),
                    SteerError::Invalid { .. }
                ),
                "{value}"
            );
        }
        assert!(
            RuleList::from_json(&json!([
                {"when": {"depth": {"ge": 0}}, "then": {"budget": {"pops": 5}}}
            ]))
            .is_err()
        );
    }

    // ---- budgets across calls (carry-over 2) -------------------------------------

    #[tokio::test]
    async fn caps_are_absolute_across_calls_and_only_the_agent_raises_them() {
        let reg = registry(vec![lattice_map("g", 400)]);
        let first = walk(&reg, vec![seed("n0")], budget(4), |t| t).await;
        assert_eq!(
            (first.status, first.totals.pops),
            (TraversalStatus::BudgetExhausted, 4)
        );
        let snapshot = first.snapshot.unwrap();

        for cap in [4, 2, 0] {
            let same = continue_from(&reg, snapshot.clone(), budget(cap), |t| t).await;
            assert_eq!(
                (same.status, same.totals.pops),
                (TraversalStatus::BudgetExhausted, 4)
            );
            assert!(same.snapshot.is_some());
        }
        let more = continue_from(&reg, snapshot.clone(), budget(9), |t| t).await;
        assert_eq!(more.totals.pops, 9);

        let raise = |pops| Steer::new(vec![act(None, Action::Budget { pops, tokens: 0 })]);
        let plus3 = continue_from(&reg, snapshot.clone(), budget(4), |t| t.steer(raise(3))).await;
        assert_eq!(plus3.totals.pops, 7);
        assert_eq!(plus3.snapshot.as_ref().unwrap().tally.raised_pops, 3);
        let again = continue_from(&reg, plus3.snapshot.clone().unwrap(), budget(4), |t| t).await;
        assert_eq!(again.totals.pops, 7, "the raise stays with the traversal");
        let plus6 = continue_from(&reg, plus3.snapshot.unwrap(), budget(4), |t| {
            t.steer(raise(3))
        })
        .await;
        assert_eq!(plus6.totals.pops, 10);
    }

    #[tokio::test]
    async fn the_raise_is_capped_and_the_other_rungs_cannot_raise_at_all() {
        let reg = registry(vec![lattice_map("g", 400)]);
        let deep = |pops| TraversalBudget {
            max_depth: 1000,
            ..budget(pops)
        };
        let first = walk(&reg, vec![seed("n0")], deep(4), |t| t).await;
        let huge = Steer::new(vec![act(
            None,
            Action::Budget {
                pops: u32::MAX,
                tokens: u64::MAX,
            },
        )]);
        let capped = continue_from(&reg, first.snapshot.unwrap(), deep(4), |t| t.steer(huge)).await;
        assert_eq!(capped.totals.pops, 4 + RAISE_MAX_POPS);
        let raised = capped.snapshot.unwrap();
        assert_eq!(
            (raised.tally.raised_pops, raised.tally.raised_tokens),
            (RAISE_MAX_POPS, RAISE_MAX_TOKENS)
        );

        struct Greedy(u8);
        impl Handler for Greedy {
            fn id(&self) -> &str {
                "greedy"
            }
            fn decide(&self, _: &Checkpoint, _: &StopBudget) -> Decision {
                Decision {
                    actions: vec![act(
                        None,
                        Action::Budget {
                            pops: 50,
                            tokens: 5000,
                        },
                    )],
                    confidence: 1.0,
                    rung: self.0,
                    ..Decision::default()
                }
            }
        }
        for claimed in [0, 1, 2, 3] {
            let result = walk(&reg, vec![seed("n0")], budget(6), |t| {
                t.handler(Greedy(claimed), every(1))
            })
            .await;
            assert_eq!(result.totals.pops, 6, "claimed rung {claimed}");
            assert_eq!(result.totals.steers, 0);
            assert_eq!(result.snapshot.unwrap().tally.raised_pops, 0);
        }
    }

    #[tokio::test]
    async fn the_agent_can_raise_the_token_cap_within_its_limit() {
        let reg = registry(vec![lattice_map("g", 3000)]);
        let tight = TraversalBudget {
            est_tokens: 60,
            ..budget(5000)
        };
        let first = walk(&reg, vec![seed("n0")], tight, |t| t).await;
        assert_eq!(first.status, TraversalStatus::BudgetExhausted);
        assert!(first.totals.tokens <= 60);
        let snapshot = first.snapshot.unwrap();
        let none = continue_from(&reg, snapshot.clone(), tight, |t| t).await;
        assert_eq!(
            none.totals.tokens, first.totals.tokens,
            "no budget, no work"
        );

        let plenty = Steer::new(vec![act(
            None,
            Action::Budget {
                pops: 0,
                tokens: 1_000_000_000,
            },
        )]);
        let more = continue_from(&reg, snapshot, tight, |t| t.steer(plenty)).await;
        assert!(more.totals.pops > first.totals.pops);
        assert!(more.totals.tokens > 60 && more.totals.tokens <= 60 + RAISE_MAX_TOKENS);
    }

    // ---- a pop cut by a budget is requeued (carry-over 1) ------------------------

    #[tokio::test]
    async fn a_pop_cut_by_the_token_cap_is_requeued_and_a_resume_finishes_it() {
        let reg = registry(vec![
            GraphMap::new(
                "g",
                &[("a", "b"), ("a", "c"), ("a", "d"), ("a", "e"), ("a", "h")],
            )
            .cost(10, 0),
        ]);
        // The cost of the call is 10 and each edge adds its own; cap at 30.
        let tight = TraversalBudget {
            est_tokens: 30,
            ..budget(10)
        };
        let first = walk(&reg, vec![seed("a")], tight, |t| t).await;
        assert_eq!(first.status, TraversalStatus::BudgetExhausted);
        let reached = first.states.len();
        assert!(reached < 6, "the pop was cut, only some edges adopted");
        let snapshot = first.snapshot.unwrap();
        let queued: Vec<_> = snapshot
            .frontier
            .iter()
            .map(|(s, _, _)| snapshot.states[*s].node.clone())
            .collect();
        assert!(queued.contains(&node("a")), "{queued:?}");
        assert!(snapshot.expanded.is_empty());

        let second = continue_from(&reg, snapshot, budget(10), |t| t).await;
        assert_eq!(second.status, TraversalStatus::Complete);
        for name in ["b", "c", "d", "e", "h"] {
            assert!(second.reached(&node(name)), "{name}");
        }
    }

    #[tokio::test]
    async fn a_pop_that_adopted_nothing_is_not_counted_and_is_requeued() {
        let reg = registry(vec![GraphMap::new("g", &[("a", "b")]).cost(100, 0)]);
        let tight = TraversalBudget {
            est_tokens: 50,
            ..budget(10)
        };
        let first = walk(&reg, vec![seed("a")], tight, |t| t).await;
        assert_eq!(first.status, TraversalStatus::BudgetExhausted);
        assert_eq!(first.totals.pops, 0);
        assert!(first.popped.is_empty());
        let snapshot = first.snapshot.clone().unwrap();
        assert_eq!(snapshot.frontier.len(), 1);
        assert!(snapshot.expanded.is_empty());

        let roomy = TraversalBudget {
            est_tokens: 1000,
            ..budget(10)
        };
        let second = continue_from(&reg, snapshot, roomy, |t| t).await;
        assert_eq!(second.totals.pops, 2);
        assert!(second.reached(&node("b")));
        assert_eq!(second.status, TraversalStatus::Complete);
    }
}
