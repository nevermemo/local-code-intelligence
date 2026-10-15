//! `config.key`: both edge directions, the witness classes and the text
//! switch, declaration resolution, sites, and the S5 equivalence of the
//! on-the-fly and pre-merged sources.

use super::*;
use crate::{
    EdgeKind, Registry, SnapshotRef,
    audit::audit_edge_sources,
    kit_c3::check_expand,
    traverse::{
        merged::{BuildOptions, MergedAdjacency, PreMerged},
        source::{EdgeSource, OnTheFly},
    },
};
use lci_core::{
    chunk::Chunk,
    node::{ConfigNamespace, SymbolKey, SymbolKind},
};
use std::{collections::BTreeSet, time::Duration};

fn decl(path: &str, line: u32, name: &str, symbol: &str) -> Decl {
    Decl {
        path: path.into(),
        line,
        name: name.into(),
        symbol: format!("rust-analyzer cargo demo 0.1.0 {symbol}"),
    }
}

fn raw(key: &str, decl: Decl, w1: bool, w2: bool, l: bool) -> reads_key::Edge {
    reads_key::Edge {
        key: key.into(),
        decl,
        w1,
        w2,
        l,
        roles: BTreeSet::new(),
        in_def_file: false,
        outside_scip: false,
    }
}

fn key(name: &str) -> NodeId {
    NodeId::ConfigKey {
        namespace: ConfigNamespace::Toml,
        key: name.into(),
    }
}

fn function(module: &str, name: &str) -> NodeId {
    NodeId::Symbol(SymbolKey {
        language: "rust".into(),
        module_path: vec![module.into()],
        container: Vec::new(),
        name: name.into(),
        kind: SymbolKind::Function,
        disambiguator: 0,
    })
}

/// `channels.lsp` is read by `read_lsp` (both witnesses) and `set_lsp` (SCIP
/// only), and named by `text_reader` (text only). `index.freshness` is also
/// read by `read_lsp`.
fn extracted() -> Vec<reads_key::Edge> {
    let read = decl("src/a.rs", 10, "read_lsp", "a/read_lsp().");
    vec![
        raw("channels.lsp", read.clone(), true, true, false),
        raw(
            "channels.lsp",
            decl("src/a.rs", 20, "set_lsp", "a/set_lsp()."),
            true,
            false,
            false,
        ),
        raw(
            "channels.lsp",
            decl("src/b.rs", 5, "text_reader", "b/text_reader()."),
            false,
            true,
            false,
        ),
        raw("index.freshness", read, true, false, false),
    ]
}

/// What the SCIP map would call `read_lsp`: a method, not the function the
/// extractor guesses.
fn resolve(decl: &Decl) -> Option<NodeId> {
    decl.symbol.ends_with("a/read_lsp().").then(|| {
        NodeId::Symbol(SymbolKey {
            kind: SymbolKind::Method,
            ..match function("a", "read_lsp") {
                NodeId::Symbol(key) => key,
                _ => unreachable!(),
            }
        })
    })
}

fn method_read_lsp() -> NodeId {
    resolve(&decl("src/a.rs", 10, "read_lsp", "a/read_lsp().")).unwrap()
}

fn budget() -> Budget {
    Budget {
        deadline: std::time::Instant::now() + Duration::from_secs(10),
        max_external_calls: u32::MAX,
        max_nodes_touched: u64::MAX,
    }
}

fn view() -> SnapshotView<'static> {
    SnapshotView::new(SnapshotRef::default(), Vec::new())
}

fn map(text_edges: bool) -> ConfigKeyMap {
    ConfigKeyMap::new(&extracted(), &resolve, ConfigKeyOptions { text_edges })
}

#[tokio::test]
async fn a_key_reaches_the_declarations_that_read_it_with_exact_witness_only() {
    let map = map(false);
    let node = key("channels.lsp");
    let edges = check_expand(&map, &node, Duration::from_secs(10), &view())
        .await
        .expect("C7");
    assert!(
        edges
            .iter()
            .all(|e| e.kind == EdgeKind::ReadsKey && e.map == CONFIG_KEY_MAP_ID)
    );
    assert!(edges.iter().all(|e| e.witness == WitnessClass::Exact));
    let got: Vec<_> = edges.iter().map(|e| (e.dst.clone(), e.conf)).collect();
    // Sorted by confidence: both witnesses 100, SCIP only 80. The resolver
    // gave `read_lsp` the id the SCIP map uses; `set_lsp` keeps the guess.
    assert_eq!(
        got,
        [(method_read_lsp(), 100), (function("a", "set_lsp"), 80)]
    );
    // The site is the declaration's name line, counted from 0.
    let site = edges[0].site.as_ref().unwrap();
    assert_eq!(
        (site.path.as_str(), site.line, site.col),
        ("src/a.rs", 9, 0)
    );
    assert!(site.blob.is_empty(), "an edge source fills the blob");
}

#[tokio::test]
async fn a_declaration_reaches_the_keys_it_reads() {
    let map = map(false);
    let edges = check_expand(&map, &method_read_lsp(), Duration::from_secs(10), &view())
        .await
        .expect("C7");
    let got: Vec<_> = edges.iter().map(|e| (e.dst.clone(), e.conf)).collect();
    assert_eq!(
        got,
        [(key("channels.lsp"), 100), (key("index.freshness"), 80)]
    );
    assert!(edges.iter().all(|e| e.kind == EdgeKind::ReadsKey));
    // The same declaration site as the forward edge.
    assert_eq!(edges[0].site.as_ref().unwrap().line, 9);
    // A declaration only text names has no edge by default.
    let text_reader = function("b", "text_reader");
    assert!(
        map.expand(&text_reader, budget(), &view())
            .await
            .edges
            .is_empty()
    );
}

#[tokio::test]
async fn text_edges_are_off_by_default_and_a_switch_turns_them_on() {
    let node = key("channels.lsp");
    let off = map(false).expand(&node, budget(), &view()).await.edges;
    assert_eq!(off.len(), 2);

    let on_map = map(true);
    let on = on_map.expand(&node, budget(), &view()).await.edges;
    assert_eq!(on.len(), 3);
    let text = on.iter().find(|e| e.witness == WitnessClass::Text).unwrap();
    assert_eq!(
        (text.dst.clone(), text.conf),
        (function("b", "text_reader"), 35)
    );
    assert_eq!(
        on_map
            .expand(&function("b", "text_reader"), budget(), &view())
            .await
            .edges
            .len(),
        1,
        "the reverse edge too"
    );

    // The same switch per request, whichever the map was built with.
    let hits = async |params: serde_json::Value, map: &ConfigKeyMap| {
        let seeds = [(key("channels.lsp"), 1.0)];
        let request = MapRequest {
            text: None,
            seeds: &seeds,
            limit: 10,
            budget: budget(),
            params: &params,
            as_of: None,
        };
        map.query(&request, &view()).await.hits.len()
    };
    assert_eq!(hits(serde_json::Value::Null, &map(false)).await, 2);
    assert_eq!(
        hits(serde_json::json!({"text_edges": true}), &map(false)).await,
        3
    );
    assert_eq!(
        hits(serde_json::json!({"text_edges": false}), &map(true)).await,
        2
    );
}

#[tokio::test]
async fn unknown_nodes_have_no_edges_and_the_build_is_counted() {
    let map = map(false);
    assert!(
        map.expand(&key("nope"), budget(), &view())
            .await
            .edges
            .is_empty()
    );
    assert!(
        map.expand(&NodeId::file("src/a.rs").unwrap(), budget(), &view())
            .await
            .edges
            .is_empty()
    );
    let stats = map.stats();
    assert_eq!((stats.exact, stats.text), (3, 1));
    assert_eq!(map.nodes(), 5, "2 keys and 3 declarations have edges");
    assert!(ConfigKeyMap::empty(ConfigKeyOptions::default()).nodes() == 0);
    assert_eq!(map.describe().freshness, FreshnessModel::Global);
    let card = map.card();
    assert_eq!(card.coverage, vec![NodeKind::ConfigKey, NodeKind::Symbol]);
    assert!(card.expand_p50_ms.is_some(), "expand was measured");
}

#[tokio::test]
async fn query_names_the_same_destinations_as_expand() {
    let map = map(false);
    let node = key("channels.lsp");
    let edges = map.expand(&node, budget(), &view()).await.edges;
    let seeds = [(node, 1.0)];
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
    let want: Vec<_> = edges.iter().map(|e| e.dst.clone()).collect();
    let got: Vec<_> = response.hits.iter().map(|h| h.node.clone()).collect();
    assert_eq!(got, want);
    assert_eq!(response.hits[0].evidence[0].edge, EdgeKind::ReadsKey);
}

// ---- S5: pre-merged == on the fly ---------------------------------------------

#[tokio::test]
async fn the_pre_merged_source_equals_the_on_the_fly_source() {
    for text_edges in [false, true] {
        let mut registry = Registry::new(60.0);
        registry
            .register(Box::new(map(text_edges)), 1.0, 40)
            .unwrap();
        let chunk = Chunk {
            relative_file_path: "src/a.rs".into(),
            language: "rust".into(),
            start_line: 1,
            end_line: 30,
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
        // Nothing in the view is a key or a symbol: the roots are the nodes
        // the map knows.
        let options = BuildOptions {
            roots: vec![
                key("channels.lsp"),
                key("index.freshness"),
                method_read_lsp(),
            ],
            ..BuildOptions::default()
        };
        let adjacency = MergedAdjacency::build_with(&registry, &view, &options).await;
        assert!(
            adjacency.covers(&function("a", "set_lsp")),
            "followed an edge"
        );

        let mut nodes: Vec<NodeId> = adjacency.nodes().cloned().collect();
        nodes.push(NodeId::file("src/a.rs").unwrap());
        let mut fly = OnTheFly::new(&registry, &view);
        let mut merged = PreMerged::new(&adjacency);
        let report = audit_edge_sources(&fly, &merged, &nodes, Duration::from_secs(10))
            .await
            .expect("equal on every node");
        assert!(report.edges >= 6, "{report:?}");
        assert!(fly.set_enabled(CONFIG_KEY_MAP_ID, false));
        assert!(merged.set_enabled(CONFIG_KEY_MAP_ID, false));
        audit_edge_sources(&fly, &merged, &nodes, Duration::from_secs(10))
            .await
            .expect("equal with the map off");
    }
}
