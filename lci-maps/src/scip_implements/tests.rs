//! `scip.implements`: the three edge shapes, their ids and sites, the facts
//! dropped for lack of a node id, the cap, and fail-open on unknown nodes.

use super::*;
use crate::{
    MapStatus, SnapshotRef,
    kit_c3::check_expand,
    scip_graph::{DirSource, Range, RawDocument, RawOccurrence, SourceText},
};
use lci_core::node::{SymbolKey, SymbolKind};
use std::time::Duration;

fn sym(path: &str) -> String {
    format!("rust-analyzer cargo demo 0.1.0 {path}")
}

fn def(range: Range, symbol: &str, enclosing: Range) -> RawOccurrence {
    RawOccurrence {
        range,
        symbol: symbol.to_string(),
        roles: 1,
        enclosing: Some(enclosing),
    }
}

fn reference(range: Range, symbol: &str) -> RawOccurrence {
    RawOccurrence {
        range,
        symbol: symbol.to_string(),
        roles: 0,
        enclosing: None,
    }
}

struct One(Vec<u8>);

impl SourceText for One {
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        (path == "a.rs").then(|| self.0.clone())
    }
}

/// a.rs: trait `Tr` with method `m`, structs `A` and `B`, `impl Tr for A`,
/// `impl Tr for B`, and `impl Default for A` (a trait the index lacks).
const SRC: &str = "pub trait Tr {\n    fn m(&self);\n}\npub struct A;\npub struct B;\nimpl Tr for A {\n    fn m(&self) {}\n}\nimpl Tr for B {\n    fn m(&self) {}\n}\nimpl Default for A {}\n";

fn graph() -> ScipGraph {
    ScipGraph::build(
        &graph_documents(),
        Some(&One(SRC.as_bytes().to_vec()) as &dyn SourceText),
    )
}

fn graph_documents() -> Vec<RawDocument> {
    let default = "rust-analyzer cargo core 0.0 default/Default#";
    vec![RawDocument {
        path: "a.rs".into(),
        occurrences: vec![
            def((0, 10, 0, 12), &sym("a/Tr#"), (0, 0, 2, 1)),
            def((1, 7, 1, 8), &sym("a/Tr#m()."), (1, 4, 1, 16)),
            def((3, 11, 3, 12), &sym("a/A#"), (3, 0, 3, 13)),
            def((4, 11, 4, 12), &sym("a/B#"), (4, 0, 4, 13)),
            reference((5, 5, 5, 7), &sym("a/Tr#")),
            reference((5, 12, 5, 13), &sym("a/A#")),
            def((6, 7, 6, 8), &sym("a/impl#[A][Tr]m()."), (6, 4, 6, 18)),
            reference((8, 5, 8, 7), &sym("a/Tr#")),
            reference((8, 12, 8, 13), &sym("a/B#")),
            def((9, 7, 9, 8), &sym("a/impl#[B][Tr]m()."), (9, 4, 9, 18)),
            reference((11, 5, 11, 12), default),
            reference((11, 17, 11, 18), &sym("a/A#")),
        ],
    }]
}

fn id(name: &str, container: &[&str], kind: SymbolKind) -> NodeId {
    NodeId::Symbol(SymbolKey {
        language: "rust".into(),
        module_path: vec!["a".into()],
        container: container.iter().map(|c| (*c).to_string()).collect(),
        name: name.into(),
        kind,
        disambiguator: 0,
    })
}

fn tr() -> NodeId {
    id("Tr", &[], SymbolKind::Trait)
}
fn a() -> NodeId {
    id("A", &[], SymbolKind::Struct)
}
fn b() -> NodeId {
    id("B", &[], SymbolKind::Struct)
}
fn decl() -> NodeId {
    id("m", &["Tr"], SymbolKind::Method)
}
fn member_a() -> NodeId {
    id("m", &["A"], SymbolKind::Method)
}
fn member_b() -> NodeId {
    id("m", &["B"], SymbolKind::Method)
}

/// The ids the other maps would give; the external `Default` has none.
fn resolve(symbol: &str) -> Option<NodeId> {
    match symbol {
        s if s == sym("a/Tr#") => Some(tr()),
        s if s == sym("a/A#") => Some(a()),
        s if s == sym("a/B#") => Some(b()),
        s if s == sym("a/Tr#m().") => Some(decl()),
        s if s == sym("a/impl#[A][Tr]m().") => Some(member_a()),
        s if s == sym("a/impl#[B][Tr]m().") => Some(member_b()),
        _ => None,
    }
}

fn map(cap: usize) -> ImplementsMap {
    ImplementsMap::new(&graph(), &resolve, ImplementsOptions { cap })
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

async fn expand(map: &ImplementsMap, node: &NodeId) -> Vec<Edge> {
    check_expand(map, node, Duration::from_secs(10), &view())
        .await
        .expect("C7")
}

fn site_of(edge: &Edge) -> (&str, u32, u32) {
    let site = edge.site.as_ref().unwrap();
    (&site.path, site.line, site.col)
}

#[tokio::test]
async fn a_type_reaches_its_traits_and_a_trait_its_types() {
    let map = map(64);
    let from_a = expand(&map, &a()).await;
    // `Default` has no id: only `Tr` is offered.
    assert_eq!(from_a.len(), 1);
    assert_eq!(from_a[0].dst, tr());
    assert_eq!(site_of(&from_a[0]), ("a.rs", 5, 5));
    let from_tr = expand(&map, &tr()).await;
    let dsts: Vec<_> = from_tr.iter().map(|e| e.dst.clone()).collect();
    assert_eq!(dsts.len(), 2);
    assert!(dsts.contains(&a()) && dsts.contains(&b()));
    for edge in from_a.iter().chain(&from_tr) {
        assert_eq!(edge.kind, EdgeKind::Implements);
        assert_eq!(edge.map, SCIP_IMPLEMENTS_ID);
        assert_eq!(edge.witness, WitnessClass::Compiler);
        assert_eq!(edge.conf, CONF_MAX);
        assert!(edge.site.as_ref().unwrap().blob.is_empty());
    }
}

#[tokio::test]
async fn a_trait_method_reaches_the_members_that_implement_it() {
    let map = map(64);
    let edges = expand(&map, &decl()).await;
    let dsts: Vec<_> = edges.iter().map(|e| e.dst.clone()).collect();
    assert_eq!(dsts.len(), 2);
    assert!(dsts.contains(&member_a()) && dsts.contains(&member_b()));
    assert!(edges.iter().all(|e| e.src == decl()));
    let to_a = edges.iter().find(|e| e.dst == member_a()).unwrap();
    assert_eq!(site_of(to_a), ("a.rs", 5, 5));
    // The member side is `graph.scip`'s: no edge leaves a member here.
    assert!(expand(&map, &member_a()).await.is_empty());
}

#[test]
fn the_stats_count_pairs_and_the_facts_without_an_id() {
    let stats = map(64).stats();
    assert_eq!(
        stats,
        ImplementsStats {
            facts: 3,
            type_trait: 2,
            methods: 2,
            dropped: 1
        }
    );
    assert_eq!(
        map(64).describe().freshness,
        FreshnessModel::PerFile,
        "read once, like graph.scip"
    );
}

#[tokio::test]
async fn the_cap_keeps_a_prefix_and_query_says_what_it_left_out() {
    let map = map(1);
    let node = tr();
    assert_eq!(expand(&map, &node).await.len(), 1);
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
    assert_eq!(response.hits.len(), 1);
    assert!(matches!(
        &response.status,
        MapStatus::Partial { note: Some(note), .. } if note.contains("1 more")
    ));
    assert_eq!(response.hits[0].evidence[0].edge, EdgeKind::Implements);
}

#[tokio::test]
async fn query_agrees_with_expand_when_nothing_is_capped() {
    let map = map(64);
    let seeds = [(tr(), 1.0)];
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
    assert_eq!(response.status, MapStatus::Ok);
    let hits: Vec<_> = response.hits.iter().map(|h| h.node.clone()).collect();
    let edges: Vec<_> = expand(&map, &tr())
        .await
        .into_iter()
        .map(|e| e.dst)
        .collect();
    assert_eq!(hits, edges);
}

#[tokio::test]
async fn unknown_nodes_and_an_empty_map_give_no_edges() {
    let map = map(64);
    for node in [
        id("nowhere", &[], SymbolKind::Struct),
        NodeId::file("a.rs").unwrap(),
    ] {
        assert!(map.expand(&node, budget(), &view()).await.edges.is_empty());
    }
    let empty = ImplementsMap::empty(ImplementsOptions::default());
    assert_eq!(empty.stats(), ImplementsStats::default());
    assert!(
        empty
            .expand(&tr(), budget(), &view())
            .await
            .edges
            .is_empty()
    );
}

#[test]
fn the_card_is_dependent_and_good_for_impact_and_flow() {
    let map = map(64);
    assert!(map.card().expand_p50_ms.is_none());
    for _ in 0..3 {
        map.edges_of(&tr());
    }
    let card = map.card();
    assert!(card.expand_p50_ms.is_some());
    assert!(card.dependent);
    assert_eq!(card.coverage, vec![NodeKind::Symbol]);
    assert_eq!(
        card.good_for,
        vec![QuestionType::Impact, QuestionType::Flow]
    );
}

// ---- pins: the seeded `query` output -------------------------------------------------

async fn ask(map: &ImplementsMap, seeds: &[(NodeId, f32)], limit: usize) -> MapResponse {
    let params = serde_json::Value::Null;
    map.query(
        &MapRequest {
            text: None,
            seeds,
            limit,
            budget: budget(),
            params: &params,
            as_of: None,
        },
        &view(),
    )
    .await
}

#[tokio::test]
async fn query_pins_the_partial_note_and_the_first_hit() {
    let map = map(1);
    let response = ask(&map, &[(tr(), 1.0)], 10).await;
    assert_eq!(
        response.status,
        MapStatus::Partial {
            note: Some("1 more implementations than the cap of 1 were not offered".to_string()),
            warning: None,
        }
    );
    assert_eq!(response.hits.len(), 1);
    let hit = &response.hits[0];
    assert_eq!(hit.rank, 1);
    assert_eq!(hit.score, Some(1.0));
    assert_eq!(hit.node, a());
    assert_eq!(hit.evidence.len(), 1);
    let evidence = &hit.evidence[0];
    assert_eq!(evidence.via, Some(tr()));
    assert_eq!(evidence.edge, EdgeKind::Implements);
    assert_eq!(evidence.label, crate::SourceLabel::Scip);
    assert_eq!(evidence.detail.as_deref(), Some("implements at a.rs:6"));
}

#[tokio::test]
async fn query_dedupes_a_shared_destination_and_truncates_to_the_limit() {
    let map = map(64);
    // A and B both reach `Tr` (A at a.rs:6, B at a.rs:9): one hit, from A.
    let both = ask(&map, &[(a(), 1.0), (b(), 1.0)], 10).await;
    assert_eq!(both.status, MapStatus::Ok);
    assert_eq!(both.hits.len(), 1, "{:?}", both.hits);
    assert_eq!(both.hits[0].node, tr());
    assert_eq!(both.hits[0].rank, 1);
    assert_eq!(both.hits[0].evidence[0].via, Some(a()));
    assert_eq!(
        both.hits[0].evidence[0].detail.as_deref(),
        Some("implements at a.rs:6")
    );

    // With `Tr` as a third seed the edges to A and B join it: three hits, cut to one.
    let seeds = [(a(), 1.0), (b(), 1.0), (tr(), 1.0)];
    let all = ask(&map, &seeds, 10).await;
    assert_eq!(all.hits.len(), 3, "{:?}", all.hits);
    let one = ask(&map, &seeds, 1).await;
    assert_eq!(one.hits.len(), 1);
    assert_eq!(one.hits[0].rank, 1);
    assert_eq!(one.hits[0].node, all.hits[0].node);
    assert_eq!(one.status, MapStatus::Ok);
}

#[test]
fn without_source_the_map_has_no_edges() {
    let docs = vec![RawDocument {
        path: "a.rs".into(),
        occurrences: vec![reference((5, 5, 5, 7), &sym("a/Tr#"))],
    }];
    let graph = ScipGraph::build(&docs, None);
    let map = ImplementsMap::new(&graph, &resolve, ImplementsOptions::default());
    assert_eq!(map.stats(), ImplementsStats::default());
    // `DirSource` on a missing directory behaves the same.
    let missing = DirSource("no-such-dir".into());
    let graph = ScipGraph::build(&docs, Some(&missing));
    assert_eq!(graph.report.impl_no_source, 1);
}

fn names(paths: &[&str]) -> Vec<Vec<String>> {
    paths
        .iter()
        .map(|path| path.split("::").map(str::to_owned).collect())
        .collect()
}

fn found(map: &ImplementsMap, paths: &[&str]) -> Vec<(NodeId, bool)> {
    found_max(map, paths, 6)
}

fn found_max(map: &ImplementsMap, paths: &[&str], max: usize) -> Vec<(NodeId, bool)> {
    map.find(&names(paths), max)
        .into_iter()
        .map(|f| (f.target.node.clone(), f.exact))
        .collect()
}

#[test]
fn the_targets_are_the_traits_and_trait_methods_that_have_edges() {
    let map = map(64);
    let targets: Vec<_> = map
        .targets()
        .iter()
        .map(|t| (t.node.clone(), t.kind))
        .collect();
    // `Default` has no id, the members and types are not targets.
    assert_eq!(
        targets,
        vec![(tr(), TargetKind::Trait), (decl(), TargetKind::Method)]
    );
    assert!(
        ImplementsMap::empty(ImplementsOptions::default())
            .targets()
            .is_empty()
    );
}

#[test]
fn a_trait_path_names_the_trait_and_a_trait_method_path_the_declaration() {
    let map = map(64);
    // The crate name, a module, or both qualify the trait.
    for path in ["demo::Tr", "a::Tr", "demo::a::Tr"] {
        assert_eq!(found(&map, &[path]), vec![(tr(), true)], "{path}");
    }
    // `Trait::method`, with the crate and module in front or not.
    for path in ["Tr::m", "a::Tr::m", "demo::Tr::m"] {
        assert_eq!(found(&map, &[path]), vec![(decl(), true)], "{path}");
    }
    // Both written: strict matches, in the order the question gave them.
    assert_eq!(
        found(&map, &["demo::Tr", "Tr::m"]),
        vec![(tr(), true), (decl(), true)]
    );
}

#[test]
fn a_bare_word_or_a_path_of_another_name_names_nothing() {
    let map = map(64);
    // One segment is prose; a type, a member and an unknown trait are no target.
    for path in ["Tr", "m", "demo::A", "demo::Default", "Tr::x"] {
        assert!(found(&map, &[path]).is_empty(), "{path}");
    }
    // The qualifiers must come in the order of the symbol (out of order, the
    // only trait of that name is still named, loosely).
    assert_eq!(found(&map, &["a::demo::Tr"]), vec![(tr(), false)]);
    assert!(map.find(&[], 6).is_empty());
}

#[test]
fn a_re_exported_path_names_the_one_target_of_that_name_but_not_a_strict_match() {
    let map = map(64);
    // `other` is not on the symbol's path, and only one `Tr` has edges.
    assert_eq!(found(&map, &["other::Tr"]), vec![(tr(), false)]);
    // A strict match drops the loose one of the same name.
    assert_eq!(found(&map, &["other::Tr", "demo::Tr"]), vec![(tr(), true)]);
}

#[test]
fn a_method_path_needs_the_trait_that_declares_it() {
    // `Other::m` has a `m`, but not under a trait named `Other`.
    assert!(found(&map(64), &["Other::m"]).is_empty());
}

/// b.rs: a second trait `Tr` with method `m`, implemented by `C`.
const SRC_B: &str =
    "pub trait Tr {\n    fn m(&self);\n}\npub struct C;\nimpl Tr for C {\n    fn m(&self) {}\n}\n";

struct Two;

impl SourceText for Two {
    fn read(&self, path: &str) -> Option<Vec<u8>> {
        match path {
            "a.rs" => Some(SRC.as_bytes().to_vec()),
            "b.rs" => Some(SRC_B.as_bytes().to_vec()),
            _ => None,
        }
    }
}

fn in_b(name: &str, container: &[&str], kind: SymbolKind) -> NodeId {
    NodeId::Symbol(SymbolKey {
        language: "rust".into(),
        module_path: vec!["b".into()],
        container: container.iter().map(|c| (*c).to_string()).collect(),
        name: name.into(),
        kind,
        disambiguator: 0,
    })
}

fn tr_b() -> NodeId {
    in_b("Tr", &[], SymbolKind::Trait)
}
fn decl_b() -> NodeId {
    in_b("m", &["Tr"], SymbolKind::Method)
}

/// Two traits named `Tr` (in modules `a` and `b` of crate `demo`).
fn twin_map() -> ImplementsMap {
    let mut docs = graph_documents();
    docs.push(RawDocument {
        path: "b.rs".into(),
        occurrences: vec![
            def((0, 10, 0, 12), &sym("b/Tr#"), (0, 0, 2, 1)),
            def((1, 7, 1, 8), &sym("b/Tr#m()."), (1, 4, 1, 16)),
            def((3, 11, 3, 12), &sym("b/C#"), (3, 0, 3, 13)),
            reference((4, 5, 4, 7), &sym("b/Tr#")),
            reference((4, 12, 4, 13), &sym("b/C#")),
            def((5, 7, 5, 8), &sym("b/impl#[C][Tr]m()."), (5, 4, 5, 18)),
        ],
    });
    let graph = ScipGraph::build(&docs, Some(&Two as &dyn SourceText));
    let resolve_both = |symbol: &str| match symbol {
        s if s == sym("b/Tr#") => Some(tr_b()),
        s if s == sym("b/C#") => Some(in_b("C", &[], SymbolKind::Struct)),
        s if s == sym("b/Tr#m().") => Some(decl_b()),
        s if s == sym("b/impl#[C][Tr]m().") => Some(in_b("m", &["C"], SymbolKind::Method)),
        other => resolve(other),
    };
    ImplementsMap::new(&graph, &resolve_both, ImplementsOptions { cap: 64 })
}

#[test]
fn two_traits_of_one_name_are_told_apart_by_the_qualifiers_or_ranked() {
    let map = twin_map();
    // A qualifier that is on one path picks that trait, strictly.
    assert_eq!(found(&map, &["b::Tr"]), vec![(tr_b(), true)]);
    assert_eq!(found(&map, &["demo::a::Tr"]), vec![(tr(), true)]);
    // `demo` is on both paths: both, strictly, in symbol order.
    assert_eq!(
        found(&map, &["demo::Tr"]),
        vec![(tr(), true), (tr_b(), true)]
    );
    // No strict match: every trait of the name, loosely, the best score first
    // (`a` is on the path of `a::Tr` only).
    assert_eq!(
        found(&map, &["x::a::Tr"]),
        vec![(tr(), false), (tr_b(), false)]
    );
    assert_eq!(
        found(&map, &["x::b::Tr"]),
        vec![(tr_b(), false), (tr(), false)]
    );
    // The cap counts targets.
    assert_eq!(found_max(&map, &["x::Tr"], 1).len(), 1);
    assert_eq!(found_max(&map, &["x::Tr"], 6).len(), 2);
}

#[test]
fn a_method_path_needs_only_the_trait_segment_and_ranks_the_rest() {
    let map = twin_map();
    // Unknown qualifiers in front of `Tr::m` do not matter (both methods).
    assert_eq!(
        found(&map, &["bevy::prelude::Tr::m"]),
        vec![(decl(), true), (decl_b(), true)]
    );
    // A qualifier that is on one path keeps only that method.
    assert_eq!(found(&map, &["b::Tr::m"]), vec![(decl_b(), true)]);
    assert_eq!(found(&map, &["bevy::a::Tr::m"]), vec![(decl(), true)]);
    assert_eq!(found_max(&map, &["Tr::m"], 1).len(), 1);
    // The trait segment itself must match.
    assert!(found(&map, &["b::Other::m"]).is_empty());
}
