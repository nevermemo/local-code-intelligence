//! `scip.impact`: the reverse closure as edges, ids, sites, the cap and its
//! order, fail-open on unknown nodes, and the S5 equivalence of the
//! on-the-fly and pre-merged sources.

use super::*;
use crate::{
    MapStatus, Registry, SnapshotRef,
    audit::audit_edge_sources,
    kit_c3::check_expand,
    scip_graph::{Range, RawDocument, RawOccurrence},
    traverse::{
        merged::{BuildOptions, MergedAdjacency, PreMerged},
        source::{EdgeSource, OnTheFly},
    },
};
use lci_core::chunk::Chunk;
use std::collections::BTreeSet;
use std::time::Duration;

fn sym(path: &str) -> String {
    format!("rust-analyzer cargo demo 0.1.0 {path}")
}

fn occ(range: Range, symbol: &str, roles: i32, enclosing: Option<Range>) -> RawOccurrence {
    RawOccurrence {
        range,
        symbol: symbol.to_string(),
        roles,
        enclosing,
    }
}

/// a.rs: `struct Foo` (line 0), `impl Foo { fn run() }` (1), `fn use_foo() {
/// Foo::run(); }` (2), `fn deep() { use_foo(); }` (3), `enum E { V(Foo) }`
/// (4). The same layout as the M1.4 index fixture.
fn demo() -> ImpactIndex {
    let foo = sym("a/Foo#");
    let run = sym("a/impl#[Foo]run().");
    let use_foo = sym("a/use_foo().");
    let deep = sym("a/deep().");
    let e = sym("a/E#");
    let v = sym("a/E#V#");
    let docs = vec![RawDocument {
        path: "a.rs".into(),
        occurrences: vec![
            occ((0, 7, 0, 10), &foo, 1, Some((0, 0, 0, 14))),
            occ((1, 14, 1, 17), &run, 1, Some((1, 11, 1, 30))),
            occ((2, 3, 2, 10), &use_foo, 1, Some((2, 0, 2, 40))),
            occ((2, 15, 2, 18), &foo, 0, None),
            occ((2, 20, 2, 23), &run, 0, None),
            occ((3, 3, 3, 7), &deep, 1, Some((3, 0, 3, 30))),
            occ((3, 12, 3, 19), &use_foo, 0, None),
            occ((4, 5, 4, 6), &e, 1, Some((4, 0, 4, 20))),
            occ((4, 9, 4, 10), &v, 1, Some((4, 9, 4, 15))),
            occ((4, 11, 4, 14), &foo, 0, None),
        ],
    }];
    let kinds: HashMap<String, Kind> = [
        (foo, Kind::Struct),
        (run, Kind::Method),
        (use_foo, Kind::Function),
        (deep, Kind::Function),
        (e, Kind::Enum),
        (v, Kind::EnumMember),
    ]
    .into_iter()
    .collect();
    ImpactIndex::build(docs, &kinds, None)
}

fn function(name: &str) -> NodeId {
    NodeId::Symbol(SymbolKey {
        language: "rust".into(),
        module_path: vec!["a".into()],
        container: Vec::new(),
        name: name.into(),
        kind: SymbolKind::Function,
        disambiguator: 0,
    })
}

fn method(name: &str) -> NodeId {
    NodeId::Symbol(SymbolKey {
        kind: SymbolKind::Method,
        ..match function(name) {
            NodeId::Symbol(key) => key,
            _ => unreachable!(),
        }
    })
}

/// The ids the SCIP graph would give the functions and methods.
fn graph_ids(symbol: &str) -> Option<NodeId> {
    match symbol {
        s if s == sym("a/use_foo().") => Some(function("use_foo")),
        s if s == sym("a/deep().") => Some(function("deep")),
        s if s == sym("a/impl#[Foo]run().") => Some(method("run")),
        _ => None,
    }
}

fn map() -> ImpactMap {
    ImpactMap::new(demo(), &graph_ids, ImpactOptions::default())
}

fn budget() -> Budget {
    Budget {
        deadline: Instant::now() + Duration::from_secs(10),
        max_external_calls: u32::MAX,
        max_nodes_touched: u64::MAX,
    }
}

fn view() -> SnapshotView<'static> {
    SnapshotView::new(SnapshotRef::default(), Vec::new())
}

fn site_of(edge: &Edge) -> (&str, u32, u32) {
    let site = edge.site.as_ref().unwrap();
    (&site.path, site.line, site.col)
}

async fn expand(map: &ImpactMap, node: &NodeId) -> Vec<Edge> {
    check_expand(map, node, Duration::from_secs(10), &view())
        .await
        .expect("C7")
}

#[tokio::test]
async fn a_type_reaches_the_units_that_use_it_and_not_the_variant() {
    let map = map();
    let foo = map.node_of(&sym("a/Foo#")).unwrap().clone();
    let edges = expand(&map, &foo).await;
    // use_foo (a resolved function id) and the enum E (an id made here); the
    // reference inside variant V belongs to E.
    let dsts: Vec<String> = edges.iter().map(|e| e.dst.to_string()).collect();
    assert_eq!(edges.len(), 2, "{dsts:?}");
    let use_foo = edges.iter().find(|e| e.dst == function("use_foo")).unwrap();
    let e = edges
        .iter()
        .find(|e| matches!(&e.dst, NodeId::Symbol(k) if k.name == "E"))
        .unwrap();
    assert_eq!(site_of(use_foo), ("a.rs", 2, 15));
    assert_eq!(site_of(e), ("a.rs", 4, 11));
    assert!(
        matches!(&e.dst, NodeId::Symbol(k) if k.kind == SymbolKind::Enum
        && k.module_path == rust_module_path("a.rs"))
    );
    for edge in &edges {
        assert_eq!(edge.kind, EdgeKind::CallsOrRefs);
        assert_eq!(edge.map, SCIP_IMPACT_ID);
        assert_eq!(edge.witness, WitnessClass::Compiler);
        assert_eq!(edge.conf, CONF_MAX);
        assert!(edge.site.as_ref().unwrap().blob.is_empty());
        assert_eq!(edge.src, foo);
    }
}

#[test]
fn every_node_of_the_map_has_the_site_of_its_definition() {
    let map = map();
    // A made id (the enum, which the graph does not hold) and a resolved one.
    let e = NodeId::Symbol(SymbolKey {
        language: "rust".into(),
        module_path: rust_module_path("a.rs"),
        container: Vec::new(),
        name: "E".into(),
        kind: SymbolKind::Enum,
        disambiguator: 0,
    });
    assert_eq!(map.site_of(&e), Some(("a.rs", 4, 5)));
    assert_eq!(map.site_of(&function("use_foo")), Some(("a.rs", 2, 3)));
    assert_eq!(map.site_of(&function("nowhere")), None);
}

#[test]
fn a_node_id_and_its_definition_map_both_ways() {
    let map = map();
    let node = function("use_foo");
    let def = map.def_of(&node).expect("use_foo has a definition");
    assert_eq!(map.index().defs[def].symbol, sym("a/use_foo()."));
    assert_eq!(map.node_of_def(def), Some(&node));
    assert_eq!(map.node_of(&sym("a/use_foo().")), Some(&node));
    assert_eq!(map.def_of(&function("nowhere")), None);
    assert_eq!(map.node_of_def(usize::MAX), None);
}

#[tokio::test]
async fn a_chain_of_callers_is_one_hop_at_a_time() {
    let map = map();
    let run = expand(&map, &method("run")).await;
    assert_eq!(run.len(), 1);
    assert_eq!(run[0].dst, function("use_foo"));
    assert_eq!(site_of(&run[0]), ("a.rs", 2, 20));
    let use_foo = expand(&map, &function("use_foo")).await;
    assert_eq!(use_foo.len(), 1);
    assert_eq!(use_foo[0].dst, function("deep"));
    assert!(expand(&map, &function("deep")).await.is_empty());
}

#[tokio::test]
async fn the_edges_are_the_closure_of_the_index() {
    let ix = demo();
    let want: HashMap<String, BTreeSet<String>> = ix
        .defs
        .iter()
        .map(|d| {
            (
                d.symbol.clone(),
                ix.closure(&d.symbol)
                    .into_iter()
                    .map(|u| ix.defs[u].symbol.clone())
                    .collect(),
            )
        })
        .collect();
    let map = ImpactMap::new(ix, &graph_ids, ImpactOptions::default());
    let mut compared = 0;
    for (symbol, users) in &want {
        let Some(node) = map.node_of(symbol).cloned() else {
            continue;
        };
        let got: BTreeSet<String> = expand(&map, &node)
            .await
            .iter()
            .map(|e| {
                let at = map.by_node[&e.dst];
                map.ix.defs[at].symbol.clone()
            })
            .collect();
        assert_eq!(&got, users, "closure of {symbol}");
        compared += 1;
    }
    assert!(compared >= 5, "{compared}");
}

#[tokio::test]
async fn a_recursive_call_makes_no_self_edge() {
    let rec = sym("a/rec().");
    let docs = vec![RawDocument {
        path: "a.rs".into(),
        occurrences: vec![
            occ((0, 3, 0, 6), &rec, 1, Some((0, 0, 0, 30))),
            occ((0, 12, 0, 15), &rec, 0, None),
        ],
    }];
    let kinds: HashMap<String, Kind> = [(rec.clone(), Kind::Function)].into_iter().collect();
    let ix = ImpactIndex::build(docs, &kinds, None);
    assert_eq!(ix.closure(&rec).len(), 1, "the index counts the self use");
    let map = ImpactMap::new(ix, &|_| None, ImpactOptions::default());
    let node = map.node_of(&rec).unwrap().clone();
    assert!(expand(&map, &node).await.is_empty());
}

/// `target` is used by `callers` functions, one each, in `a.rs`.
fn crowd(callers: usize) -> (ImpactIndex, String) {
    let target = sym("a/target().");
    let mut occurrences = vec![occ((0, 3, 0, 9), &target, 1, Some((0, 0, 0, 20)))];
    let mut kinds: HashMap<String, Kind> = [(target.clone(), Kind::Function)].into_iter().collect();
    for i in 0..callers {
        let name = sym(&format!("a/caller_{i:03}()."));
        let line = i as u32 + 1;
        occurrences.push(occ(
            (line, 3, line, 13),
            &name,
            1,
            Some((line, 0, line, 40)),
        ));
        occurrences.push(occ((line, 20, line, 26), &target, 0, None));
        kinds.insert(name, Kind::Function);
    }
    let docs = vec![RawDocument {
        path: "a.rs".into(),
        occurrences,
    }];
    (ImpactIndex::build(docs, &kinds, None), target)
}

#[tokio::test]
async fn the_cap_keeps_a_deterministic_prefix_in_edge_order() {
    let (ix, target) = crowd(100);
    let map = ImpactMap::new(ix, &|_| None, ImpactOptions::default());
    let node = map.node_of(&target).unwrap().clone();
    let first = expand(&map, &node).await;
    assert_eq!(first.len(), 64, "the default cap");
    assert_eq!(
        shape(&first),
        shape(&expand(&map, &node).await),
        "same answer twice"
    );

    // The full list, sorted: the cap is its prefix.
    let (mut all, total) = (map.all_edges(&node).0, 100);
    assert_eq!(all.len(), total);
    cap_edges(&mut all, usize::MAX);
    assert_eq!(shape(&first), shape(&all[..64]));

    // A smaller cap is a shorter prefix of the same order.
    let (ix, target) = crowd(100);
    let small = ImpactMap::new(ix, &|_| None, ImpactOptions { cap: 5 });
    let node = small.node_of(&target).unwrap().clone();
    let five = expand(&small, &node).await;
    assert_eq!(shape(&five), shape(&all[..5]));

    // `query` says what the cap left out.
    let seeds = [(node.clone(), 1.0)];
    let params = serde_json::Value::Null;
    let response = small
        .query(
            &MapRequest {
                text: None,
                seeds: &seeds,
                limit: 100,
                budget: budget(),
                params: &params,
                as_of: None,
            },
            &view(),
        )
        .await;
    assert_eq!(response.hits.len(), 5);
    assert!(matches!(
        &response.status,
        MapStatus::Partial { note: Some(note), .. } if note.contains("95 more")
    ));
}

#[test]
fn made_ids_are_distinct_for_units_with_one_name() {
    // Two structs called `Dup` in one file: distinct symbols, one key.
    let (one, two) = (sym("a/Dup#"), sym("a/b/Dup#"));
    let docs = vec![RawDocument {
        path: "a.rs".into(),
        occurrences: vec![
            occ((0, 7, 0, 10), &one, 1, Some((0, 0, 0, 14))),
            occ((1, 7, 1, 10), &two, 1, Some((1, 0, 1, 14))),
        ],
    }];
    let kinds: HashMap<String, Kind> = [(one.clone(), Kind::Struct), (two.clone(), Kind::Struct)]
        .into_iter()
        .collect();
    let map = ImpactMap::new(
        ImpactIndex::build(docs, &kinds, None),
        &|_| None,
        ImpactOptions::default(),
    );
    let (a, b) = (map.node_of(&one).unwrap(), map.node_of(&two).unwrap());
    assert_ne!(a, b);
    assert_eq!(map.stats().nodes, 2);
    assert_eq!(map.stats().resolved, 0);
}

// ---- fail-open --------------------------------------------------------------

#[tokio::test]
async fn unknown_and_foreign_nodes_give_no_edges() {
    let map = map();
    for node in [
        function("nowhere"),
        NodeId::file("a.rs").unwrap(),
        NodeId::Commit {
            sha: "a".repeat(40),
        },
    ] {
        assert!(map.expand(&node, budget(), &view()).await.edges.is_empty());
    }
    let empty = ImpactMap::empty(ImpactOptions::default());
    assert_eq!(empty.stats(), ImpactStats::default());
    assert!(
        empty
            .expand(&function("use_foo"), budget(), &view())
            .await
            .edges
            .is_empty()
    );
    assert!(!empty.log().is_empty());
}

#[tokio::test]
async fn a_query_without_edges_is_an_ok_answer() {
    let map = map();
    let seeds = [(function("deep"), 1.0)];
    let params = serde_json::Value::Null;
    let response = map
        .query(
            &MapRequest {
                text: None,
                seeds: &seeds,
                limit: 10,
                budget: budget(),
                params: &params,
                as_of: None,
            },
            &view(),
        )
        .await;
    assert!(response.hits.is_empty());
    assert_eq!(response.status, MapStatus::Ok);
}

#[test]
fn the_card_reports_the_measured_median() {
    let map = map();
    assert!(map.card().expand_p50_ms.is_none());
    for _ in 0..3 {
        map.edges_of(&function("use_foo"));
    }
    let card = map.card();
    assert!(card.expand_p50_ms.is_some());
    assert_eq!(card.coverage, vec![NodeKind::Symbol]);
    assert_eq!(
        card.good_for,
        vec![QuestionType::Impact, QuestionType::Flow]
    );
    assert_eq!(map.describe().freshness, FreshnessModel::PerFile);
}

// ---- S5: pre-merged == on the fly ---------------------------------------------

#[tokio::test]
async fn the_pre_merged_source_equals_the_on_the_fly_source() {
    let mut registry = Registry::new(60.0);
    registry.register(Box::new(map()), 1.0, 40).unwrap();
    let chunk = Chunk {
        relative_file_path: "a.rs".into(),
        language: "rust".into(),
        start_line: 1,
        end_line: 5,
        code: String::new(),
        content_hash: "h".into(),
    };
    let view = SnapshotView::new(
        SnapshotRef {
            indexed_at: "t".into(),
            fingerprint: "f1".into(),
        },
        vec![&chunk],
    );
    let roots = vec![
        method("run"),
        function("use_foo"),
        function("deep"),
        NodeId::Symbol(SymbolKey {
            language: "rust".into(),
            module_path: rust_module_path("a.rs"),
            container: Vec::new(),
            name: "Foo".into(),
            kind: SymbolKind::Struct,
            disambiguator: 0,
        }),
    ];
    let options = BuildOptions {
        roots,
        ..BuildOptions::default()
    };
    let adjacency = MergedAdjacency::build_with(&registry, &view, &options).await;
    assert!(adjacency.covers(&function("deep")), "followed an edge");

    let mut nodes: Vec<NodeId> = adjacency.nodes().cloned().collect();
    nodes.push(NodeId::file("a.rs").unwrap());
    let mut fly = OnTheFly::new(&registry, &view);
    let mut merged = PreMerged::new(&adjacency);
    let report = audit_edge_sources(&fly, &merged, &nodes, Duration::from_secs(10))
        .await
        .expect("equal on every node");
    assert!(report.edges >= 4, "{report:?}");
    assert!(fly.set_enabled(SCIP_IMPACT_ID, false));
    assert!(merged.set_enabled(SCIP_IMPACT_ID, false));
    audit_edge_sources(&fly, &merged, &nodes, Duration::from_secs(10))
        .await
        .expect("equal with the map off");
}

/// What an edge says, without its timing.
fn shape(edges: &[Edge]) -> Vec<(NodeId, u8, Option<Site>)> {
    edges
        .iter()
        .map(|e| (e.dst.clone(), e.conf, e.site.clone()))
        .collect()
}

// ---- the same index, one piece of evidence -----------------------------------------

#[tokio::test]
async fn graph_scip_and_scip_impact_over_one_index_corroborate_once() {
    use crate::{
        scip_graph::{ScipGraph, ScipMap},
        traverse::{Seed, Traversal, TraversalBudget},
    };
    // `ping` and `pong` call each other: from `ping`, graph.scip offers
    // `pong` (it calls it) and scip.impact offers `pong` (it uses `ping`).
    let (ping, pong) = (sym("a/ping()."), sym("a/pong()."));
    let docs = vec![RawDocument {
        path: "a.rs".into(),
        occurrences: vec![
            occ((0, 3, 0, 7), &ping, 1, Some((0, 0, 0, 30))),
            occ((0, 12, 0, 16), &pong, 0, None),
            occ((1, 3, 1, 7), &pong, 1, Some((1, 0, 1, 30))),
            occ((1, 12, 1, 16), &ping, 0, None),
        ],
    }];
    let kinds: HashMap<String, Kind> = [(ping.clone(), Kind::Function), (pong, Kind::Function)]
        .into_iter()
        .collect();
    let scip = ScipMap::with_default_ids(ScipGraph::build(&docs, None));
    let ids = |symbol: &str| {
        let graph = scip.graph();
        Some(scip.node_id(graph.node_index(symbol)?).clone())
    };
    let impact = ImpactMap::new(
        ImpactIndex::build(docs.clone(), &kinds, None),
        &ids,
        ImpactOptions::default(),
    );
    let start = ids(&ping).unwrap();
    let mut registry = Registry::new(60.0);
    registry.register(Box::new(scip), 1.0, 40).unwrap();
    registry.register(Box::new(impact), 1.0, 40).unwrap();
    assert_eq!(registry.independent_expanders(), 1);

    let view = view();
    let result = Traversal::new(
        OnTheFly::new(&registry, &view),
        vec![Seed::from(start)],
        TraversalBudget {
            pops: 10,
            max_depth: 3,
            deadline: Instant::now() + Duration::from_secs(10),
            est_tokens: 100_000,
            external_calls: 0,
        },
    )
    .run()
    .await;
    let reached = result
        .states
        .iter()
        .find(|s| s.parent.is_some())
        .expect("ping reaches pong");
    assert_eq!(
        reached.maps.iter().copied().collect::<Vec<_>>(),
        ["graph.scip", "scip.impact"]
    );
    assert_eq!(reached.n_maps(), 1, "one index, one piece of evidence");
}
