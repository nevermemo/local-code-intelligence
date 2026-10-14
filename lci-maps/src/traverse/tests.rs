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
            StopPolicy {
                every_pops: 1,
                max_stops: 4,
            },
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
    StopPolicy {
        every_pops: pops,
        max_stops: 4,
    }
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
        t.handler(
            scripted(|_| Vec::new()),
            StopPolicy {
                every_pops: 5,
                max_stops: 3,
            },
        )
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
