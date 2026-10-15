//! The containment map `structure.contains` (slice S6c): each edge kind on a
//! small graph, a walk from a chunk that holds no symbol, and the S5
//! equivalence auditor over the pre-merged and the on-the-fly source.

use super::*;
use crate::{
    audit::audit_edge_sources,
    contains::{CONTAINS_MAP_ID, ContainsMap},
    kit_c3::check_expand,
    scip_graph::{RawDocument, RawOccurrence, SCIP_MAP_ID, ScipGraph, ScipMap},
    traverse::merged::{MergedAdjacency, PreMerged},
};
use lci_core::chunk::Chunk;
use std::collections::BTreeSet;

fn sym(path: &str) -> String {
    format!("rust-analyzer cargo demo 0.1.0 {path}")
}

fn occ(
    range: (u32, u32, u32, u32),
    symbol: &str,
    roles: i32,
    enclosing: Option<(u32, u32, u32, u32)>,
) -> RawOccurrence {
    RawOccurrence {
        range,
        symbol: sym(symbol),
        roles,
        enclosing,
    }
}

/// `src/a.rs` (0-based lines):
///
/// ```text
/// 0  use std::fmt;
/// 1
/// 2
/// 3  fn alpha() {          alpha: lines 3..7
/// 4      fn inner() {}     inner: lines 4..5, nested in alpha
/// 5
/// 6      beta();           reference to b.rs
/// 7  }
/// 8
/// 9  impl T { fn method() {} }   T::method: lines 9..11
/// ```
///
/// `src/b.rs`: `fn beta()` on lines 2..4 calling `gamma()`, `fn gamma()` on
/// lines 6..7.
fn documents() -> Vec<RawDocument> {
    vec![
        RawDocument {
            path: "src/a.rs".into(),
            occurrences: vec![
                occ((3, 3, 3, 8), "a/alpha().", 1, Some((3, 0, 7, 1))),
                occ((4, 7, 4, 12), "a/alpha()./inner().", 1, Some((4, 4, 5, 5))),
                occ((6, 4, 6, 8), "b/beta().", 0, None),
                occ((9, 7, 9, 13), "a/impl#[T]method().", 1, Some((9, 4, 11, 5))),
            ],
        },
        RawDocument {
            path: "src/b.rs".into(),
            occurrences: vec![
                occ((2, 3, 2, 7), "b/beta().", 1, Some((2, 0, 4, 1))),
                occ((3, 4, 3, 9), "b/gamma().", 0, None),
                occ((6, 3, 6, 8), "b/gamma().", 1, Some((6, 0, 7, 1))),
            ],
        },
    ]
}

fn demo_scip() -> ScipMap {
    ScipMap::with_default_ids(ScipGraph::build(&documents(), None))
}

fn symbol(scip: &ScipMap, name: &str) -> NodeId {
    let at = scip.graph().node_index(&sym(name)).expect(name);
    scip.node_id(at).clone()
}

fn chunk_node(path: &str, hash: &str) -> NodeId {
    NodeId::chunk(path, hash, 0).unwrap()
}

fn call_budget() -> Budget {
    Budget {
        deadline: Instant::now() + Duration::from_secs(5),
        max_external_calls: u32::MAX,
        max_nodes_touched: u64::MAX,
    }
}

async fn expand(map: &ContainsMap, from: &NodeId) -> Vec<Edge> {
    check_expand(map, from, Duration::from_secs(5), &view())
        .await
        .expect("C7")
}

fn dsts(edges: &[Edge]) -> BTreeSet<NodeId> {
    edges.iter().map(|e| e.dst.clone()).collect()
}

fn set(nodes: &[&NodeId]) -> BTreeSet<NodeId> {
    nodes.iter().map(|n| (*n).clone()).collect()
}

// ---- edge kinds ------------------------------------------------------------

#[tokio::test]
async fn a_file_contains_its_symbols_top_level_first_then_members() {
    let scip = demo_scip();
    let map = ContainsMap::new(&scip);
    let (alpha, inner, method) = (
        symbol(&scip, "a/alpha()."),
        symbol(&scip, "a/alpha()./inner()."),
        symbol(&scip, "a/impl#[T]method()."),
    );
    let file = NodeId::file("src/a.rs").unwrap();
    let edges = expand(&map, &file).await;
    assert_eq!(dsts(&edges), set(&[&alpha, &inner, &method]));
    for edge in &edges {
        assert_eq!(edge.src, file);
        assert_eq!(edge.kind, EdgeKind::Contains);
        assert_eq!(edge.map, CONTAINS_MAP_ID);
        assert_eq!((edge.witness, edge.conf), (WitnessClass::Exact, 100));
    }
    // The site is the definition of the symbol the edge reaches.
    let at = |dst: &NodeId| edges.iter().find(|e| e.dst == *dst).unwrap().site.clone();
    let site = |line, col| {
        Some(crate::edge::Site {
            path: "src/a.rs".into(),
            blob: String::new(),
            line,
            col,
        })
    };
    assert_eq!(at(&alpha), site(3, 3));
    assert_eq!(at(&inner), site(4, 7));
    assert_eq!(at(&method), site(9, 7));
    // The listing order the edge cap keeps: the free function, then members
    // (nested function and method) in definition order.
    assert_eq!(
        map.symbols_of("src/a.rs"),
        [&alpha, &inner, &method],
        "top-level first, then members"
    );
    // A file the graph does not know has no symbols.
    assert!(
        expand(&map, &NodeId::file("src/zzz.rs").unwrap())
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn a_chunk_contains_the_symbols_its_lines_overlap_from_its_own_lines_or_the_snapshot_view() {
    let scip = demo_scip();
    let (alpha, inner, method) = (
        symbol(&scip, "a/alpha()."),
        symbol(&scip, "a/alpha()./inner()."),
        symbol(&scip, "a/impl#[T]method()."),
    );
    let (header, body, tail, other) = (
        chunk_node("src/a.rs", "h1"),
        chunk_node("src/a.rs", "h2"),
        chunk_node("src/a.rs", "h3"),
        chunk_node("src/a.rs", "h4"),
    );
    // Lines are 1-based in chunks: the header is lines 1-3 (0-based 0..2),
    // the body is lines 5-6 (0-based 4..5), the tail is line 11 (0-based 10).
    let map = ContainsMap::new(&scip).with_chunk_lines([
        (header.clone(), (1, 3)),
        (body.clone(), (5, 6)),
        (tail.clone(), (11, 11)),
    ]);
    // The body overlaps alpha and the function nested in it.
    let edges = expand(&map, &body).await;
    assert_eq!(dsts(&edges), set(&[&alpha, &inner]));
    assert!(edges.iter().all(|e| e.kind == EdgeKind::Contains
        && e.witness == WitnessClass::Exact
        && e.conf == 100));
    // The tail overlaps the last line of the method's span only.
    assert_eq!(dsts(&expand(&map, &tail).await), set(&[&method]));
    // The header overlaps no symbol: it falls through to the whole file.
    assert_eq!(
        dsts(&expand(&map, &header).await),
        set(&[&alpha, &inner, &method])
    );
    // So does a chunk whose lines nobody knows.
    assert_eq!(
        dsts(&expand(&map, &other).await),
        set(&[&alpha, &inner, &method])
    );
    // A chunk of a file the graph does not know has no edges.
    assert!(
        expand(&map, &chunk_node("src/zzz.rs", "h1"))
            .await
            .is_empty()
    );

    // The lines come from the snapshot view when it holds the chunk.
    let chunks = [
        file_chunk("src/a.rs", 1, 3, "h1"),
        file_chunk("src/a.rs", 10, 12, "h2"),
    ];
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let (header, tail) = (view.ids()[0].clone(), view.ids()[1].clone());
    let map = ContainsMap::new(&scip);
    let edges = map.expand(&tail, call_budget(), &view).await.edges;
    assert_eq!(dsts(&edges), set(&[&method]));
    let edges = map.expand(&header, call_budget(), &view).await.edges;
    assert_eq!(dsts(&edges), set(&[&alpha, &inner, &method]));
}

#[tokio::test]
async fn a_symbol_is_defined_in_its_file_and_its_enclosing_function() {
    let scip = demo_scip();
    let map = ContainsMap::new(&scip);
    let (alpha, inner, method) = (
        symbol(&scip, "a/alpha()."),
        symbol(&scip, "a/alpha()./inner()."),
        symbol(&scip, "a/impl#[T]method()."),
    );
    let file = NodeId::file("src/a.rs").unwrap();

    let edges = expand(&map, &inner).await;
    assert_eq!(dsts(&edges), set(&[&file, &alpha]));
    for edge in &edges {
        assert_eq!(edge.src, inner);
        assert_eq!(edge.kind, EdgeKind::DefinedIn);
        assert_eq!(edge.kind.as_str(), "defined_in");
        assert_eq!((edge.witness, edge.conf), (WitnessClass::Exact, 100));
    }
    // To its file, the site is the symbol's own definition; to the function
    // around it, that function's.
    let site_of = |dst: &NodeId| {
        let site = edges
            .iter()
            .find(|e| e.dst == *dst)
            .and_then(|e| e.site.clone())
            .unwrap();
        (site.path, site.line, site.col)
    };
    assert_eq!(site_of(&file), ("src/a.rs".to_string(), 4, 7));
    assert_eq!(site_of(&alpha), ("src/a.rs".to_string(), 3, 3));

    // Top-level functions and methods are defined in their file alone (the
    // graph holds no node for an impl or a type).
    assert_eq!(dsts(&expand(&map, &alpha).await), set(&[&file]));
    assert_eq!(dsts(&expand(&map, &method).await), set(&[&file]));

    // Nodes of other kinds, and symbols the graph does not hold, have no edges.
    let commit = NodeId::Commit {
        sha: "a".repeat(40),
    };
    assert!(expand(&map, &commit).await.is_empty());
    let other = ScipMap::with_default_ids(ScipGraph::build(
        &[RawDocument {
            path: "src/x.rs".into(),
            occurrences: vec![occ((0, 3, 0, 4), "x/stranger().", 1, Some((0, 0, 1, 1)))],
        }],
        None,
    ));
    assert!(
        expand(&map, &symbol(&other, "x/stranger()."))
            .await
            .is_empty()
    );
}

#[tokio::test]
async fn the_query_names_the_neighbours_expand_gives() {
    let scip = demo_scip();
    let map = ContainsMap::new(&scip);
    let from = symbol(&scip, "a/alpha()./inner().");
    let want: Vec<NodeId> = expand(&map, &from)
        .await
        .into_iter()
        .map(|e| e.dst)
        .collect();
    let seeds = [(from.clone(), 1.0)];
    let params = serde_json::Value::Null;
    let response = map
        .query(
            &MapRequest {
                text: None,
                seeds: &seeds,
                limit: 10,
                budget: call_budget(),
                params: &params,
                as_of: None,
            },
            &view(),
        )
        .await;
    let got: Vec<NodeId> = response.hits.iter().map(|h| h.node.clone()).collect();
    assert_eq!(got, want);
    assert!(
        response
            .hits
            .iter()
            .all(|h| h.evidence[0].edge == EdgeKind::DefinedIn)
    );
}

#[tokio::test]
async fn the_edge_cap_keeps_the_first_symbols_of_the_listing() {
    // 300 functions in one file: the cap keeps the first 256 by definition.
    let occurrences = (0..300u32)
        .map(|i| {
            occ(
                (i * 2, 3, i * 2, 6),
                &format!("big/f{i:03}()."),
                1,
                Some((i * 2, 0, i * 2 + 1, 1)),
            )
        })
        .collect();
    let scip = ScipMap::with_default_ids(ScipGraph::build(
        &[RawDocument {
            path: "src/big.rs".into(),
            occurrences,
        }],
        None,
    ));
    let map = ContainsMap::new(&scip);
    let edges = expand(&map, &NodeId::file("src/big.rs").unwrap()).await;
    assert_eq!(edges.len(), crate::EXPAND_LIMIT);
    let kept = dsts(&edges);
    let first = map.symbols_of("src/big.rs");
    assert_eq!(first.len(), 300);
    assert!(
        first[..crate::EXPAND_LIMIT]
            .iter()
            .all(|n| kept.contains(*n))
    );
    assert!(!kept.contains(first[299]));
}

// ---- a walk ----------------------------------------------------------------

fn registry_of(scip: ScipMap, contains: Option<ContainsMap>) -> Registry<'static> {
    let mut registry = Registry::new(60.0);
    if let Some(contains) = contains {
        registry.register(Box::new(contains), 1.0, 40).unwrap();
    }
    registry.register(Box::new(scip), 1.0, 40).unwrap();
    registry
}

fn visited(result: &TraversalResult) -> BTreeSet<NodeId> {
    result.states.iter().map(|s| s.node.clone()).collect()
}

#[tokio::test]
async fn a_chunk_in_an_import_header_reaches_the_files_functions_and_then_scip_neighbours() {
    let scip = demo_scip();
    let (alpha, beta, gamma) = (
        symbol(&scip, "a/alpha()."),
        symbol(&scip, "b/beta()."),
        symbol(&scip, "b/gamma()."),
    );
    let header = chunk_node("src/a.rs", "h1");
    let contains = ContainsMap::new(&scip).with_chunk_lines([(header.clone(), (1, 2))]);

    // Without the containment map nothing leaves the chunk (S6b's q04, q06).
    let alone = registry_of(demo_scip(), None);
    let result = walk(&alone, vec![Seed::from(header.clone())], budget(20), |t| t).await;
    assert_eq!(visited(&result), set(&[&header]));
    assert_eq!(result.totals.pops, 1);

    // With it: the file's functions, then what they call.
    let reg = registry_of(scip, Some(contains));
    let result = walk(&reg, vec![Seed::from(header.clone())], budget(20), |t| t).await;
    let seen = visited(&result);
    for node in [&alpha, &beta, &gamma] {
        assert!(seen.contains(node), "{node} not reached: {seen:?}");
    }
    let state = |node: &NodeId| result.states.iter().find(|s| s.node == *node).unwrap();
    assert_eq!(state(&alpha).via, Some(CONTAINS_MAP_ID));
    assert_eq!(state(&alpha).witness, Some(WitnessClass::Exact));
    // alpha calls beta (graph.scip), beta calls gamma.
    assert_eq!(state(&beta).via, Some(SCIP_MAP_ID));
    assert_eq!(state(&beta).witness, Some(WitnessClass::Compiler));
    assert_eq!(state(&gamma).via, Some(SCIP_MAP_ID));
    assert!(result.totals.cross_map_hops >= 1);
}

#[tokio::test]
async fn graph_scip_and_structure_contains_corroborate_a_node_once() {
    let scip = demo_scip();
    let (beta, gamma) = (symbol(&scip, "b/beta()."), symbol(&scip, "b/gamma()."));
    let reg = registry_of(demo_scip(), Some(ContainsMap::new(&scip)));
    assert_eq!(reg.independent_expanders(), 1);
    // The file holds beta and gamma; beta calls gamma: both maps reach gamma.
    let file = NodeId::file("src/b.rs").unwrap();
    let result = walk(&reg, vec![Seed::from(file)], budget(20), |t| t).await;
    let state = |node: &NodeId| result.states.iter().find(|s| s.node == *node).unwrap();
    assert!(state(&beta).n_maps() == 1);
    let gamma = state(&gamma);
    assert_eq!(
        gamma.maps.iter().copied().collect::<Vec<_>>(),
        [SCIP_MAP_ID, CONTAINS_MAP_ID]
    );
    assert_eq!(gamma.n_maps(), 1, "two maps over one index are one map");
    assert_eq!(result.ranked_potential(gamma), gamma.potential);
}

// ---- equivalence of the two sources ----------------------------------------

fn file_chunk(path: &str, start: u32, end: u32, hash: &str) -> Chunk {
    Chunk {
        relative_file_path: path.into(),
        language: "rust".into(),
        start_line: start,
        end_line: end,
        code: String::new(),
        content_hash: hash.into(),
    }
}

#[tokio::test]
async fn the_pre_merged_source_gives_the_edges_the_map_gives_on_the_fly() {
    let scip = demo_scip();
    let mut nodes: Vec<NodeId> = (0..scip.graph().nodes.len())
        .map(|i| scip.node_id(i).clone())
        .collect();
    nodes.extend(["src/a.rs", "src/b.rs", "src/zzz.rs"].map(|p| NodeId::file(p).unwrap()));
    let reg = registry_of(demo_scip(), Some(ContainsMap::new(&scip)));
    let chunks = [
        file_chunk("src/a.rs", 1, 3, "h1"),
        file_chunk("src/a.rs", 4, 8, "h2"),
        file_chunk("src/a.rs", 10, 12, "h3"),
        file_chunk("src/b.rs", 1, 2, "h4"),
        file_chunk("src/b.rs", 3, 5, "h5"),
        file_chunk("src/zzz.rs", 1, 4, "h6"),
    ];
    let view = SnapshotView::new(
        SnapshotRef {
            indexed_at: "t".into(),
            fingerprint: "f1".into(),
        },
        chunks.iter().collect(),
    );
    nodes.extend(view.ids().iter().cloned());

    let adjacency = MergedAdjacency::build(&reg, &view).await;
    let mut fly = OnTheFly::new(&reg, &view);
    let mut merged = PreMerged::new(&adjacency);
    let report = audit_edge_sources(&fly, &merged, &nodes, Duration::from_secs(5))
        .await
        .expect("equal on every node");
    assert_eq!(report.nodes, nodes.len());
    assert!(report.edges >= 20, "{report:?}");

    // The same with the containment map off on both sides, and back on.
    assert!(fly.set_enabled(CONTAINS_MAP_ID, false) && merged.set_enabled(CONTAINS_MAP_ID, false));
    let without = audit_edge_sources(&fly, &merged, &nodes, Duration::from_secs(5))
        .await
        .expect("equal with structure.contains off");
    assert!(without.edges < report.edges);
    assert!(fly.set_enabled(CONTAINS_MAP_ID, true) && merged.set_enabled(CONTAINS_MAP_ID, true));
    audit_edge_sources(&fly, &merged, &nodes, Duration::from_secs(5))
        .await
        .expect("equal with structure.contains back on");

    // The sites carry the blob of the file they are in.
    let from_chunk = merged.edges(&view.ids()[0], call_budget()).await;
    assert!(!from_chunk.edges.is_empty());
    assert!(
        from_chunk
            .edges
            .iter()
            .all(|e| e.best.site.as_ref().is_some_and(|s| !s.blob.is_empty()))
    );
}

// ---- hop pricing of structural edges (S6d) ----------------------------------

fn kinded(kind: EdgeKind, dst: &str) -> Edge {
    Edge {
        kind,
        ..edge("a", dst, "m", WitnessClass::Exact, 100)
    }
}

#[test]
fn a_hop_under_the_gate_is_valued_at_its_kind_prior() {
    let fused = fuse(vec![
        kinded(EdgeKind::CallsOrRefs, "calls"),
        kinded(EdgeKind::Contains, "contains"),
        kinded(EdgeKind::DefinedIn, "defined"),
    ]);
    let cands = candidates(&fused);
    let ctx = score_ctx();
    let value_of = |dst: &str| {
        let at = fused.iter().position(|f| f.best.dst == node(dst)).unwrap();
        let steps = hop(&mut RulesScorer, &ctx, &cands, HopPrice::default());
        assert_eq!(steps.len(), 3);
        steps.iter().find(|(i, _)| *i == at).unwrap().1
    };
    assert_eq!(value_of("calls"), 1.0);
    assert_eq!(value_of("contains"), 0.5);
    assert_eq!(value_of("defined"), 0.5);
    // The unscored control keeps value 1 for every kind.
    let steps = hop(&mut BfsScorer, &ctx, &cands, HopPrice::default());
    assert!(steps.iter().all(|(_, v)| *v == 1.0));
}

#[tokio::test]
async fn a_call_neighbour_outranks_the_file_and_its_siblings_which_a_walk_reaches_by_climbing() {
    let scip = demo_scip();
    let (alpha, beta) = (symbol(&scip, "a/alpha()."), symbol(&scip, "b/beta()."));
    let method = symbol(&scip, "a/impl#[T]method().");
    let file = NodeId::file("src/a.rs").unwrap();
    let reg = registry_of(demo_scip(), Some(ContainsMap::new(&scip)));
    let result = walk(&reg, vec![Seed::from(alpha.clone())], budget(20), |t| t).await;
    let potential = |node: &NodeId| {
        result
            .states
            .iter()
            .find(|s| s.node == *node)
            .unwrap_or_else(|| panic!("{node} not reached"))
            .potential
    };
    // alpha calls beta (value 1); alpha -> its file is DefinedIn (0.5), and
    // the sibling is one Contains hop further (0.25).
    assert_eq!(potential(&beta), 1.0);
    assert_eq!(potential(&file), 0.5);
    assert_eq!(potential(&method), 0.25);
    // Ranked by potential, as the tool does: the call neighbour comes first.
    let mut ranked: Vec<&State> = result.states.iter().filter(|s| s.node != alpha).collect();
    ranked.sort_by(|a, b| b.potential.total_cmp(&a.potential));
    assert_eq!(ranked[0].node, beta);
    // Every state reached through the containment map ranks below it.
    let structural: Vec<&&State> = ranked
        .iter()
        .filter(|s| s.via == Some(CONTAINS_MAP_ID))
        .collect();
    assert!(!structural.is_empty());
    assert!(structural.iter().all(|s| s.potential <= 0.5));

    // A walk can climb from a nested function to the file and down to its
    // siblings.
    let inner = symbol(&scip, "a/alpha()./inner().");
    let result = walk(&reg, vec![Seed::from(inner)], budget(20), |t| t).await;
    let seen = visited(&result);
    assert!(seen.contains(&file) && seen.contains(&method), "{seen:?}");
}
