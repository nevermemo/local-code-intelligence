//! `tests.tested_by`: the two test-unit rules, the edge shape, the refactor
//! guard on `scip.impact`, the cap and the seeded query.

use super::*;
use crate::{
    MapStatus, SnapshotRef,
    edge::sort_edges,
    impact::ImpactIndex,
    kit_c3::check_expand,
    scip_graph::{Range, RawDocument, RawOccurrence},
    scip_impact::{ImpactOptions, SCIP_IMPACT_ID},
};
use lci_core::node::{SymbolKey, SymbolKind};
use scip::types::symbol_information::Kind;
use std::{collections::HashMap, time::Duration};

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

/// `src/a.rs`: `foo` (line 0), `use_foo` using `foo` (1), `tests::inline_t`
/// (2, inside an inline `tests` module) using `foo`; `tests/a_test.rs`:
/// `t_foo` using `foo`.
fn demo() -> ImpactIndex {
    let foo = sym("a/foo().");
    let use_foo = sym("a/use_foo().");
    let inline_t = sym("a/tests/inline_t().");
    let t_foo = sym("a_test/t_foo().");
    let docs = vec![
        RawDocument {
            path: "src/a.rs".into(),
            occurrences: vec![
                occ((0, 3, 0, 6), &foo, 1, Some((0, 0, 0, 12))),
                occ((1, 3, 1, 10), &use_foo, 1, Some((1, 0, 1, 25))),
                occ((1, 15, 1, 18), &foo, 0, None),
                occ((2, 15, 2, 23), &inline_t, 1, Some((2, 12, 2, 36))),
                occ((2, 28, 2, 31), &foo, 0, None),
            ],
        },
        RawDocument {
            path: "tests/a_test.rs".into(),
            occurrences: vec![
                occ((0, 3, 0, 8), &t_foo, 1, Some((0, 0, 0, 20))),
                occ((0, 12, 0, 15), &foo, 0, None),
            ],
        },
    ];
    let kinds: HashMap<String, Kind> = [foo, use_foo, inline_t, t_foo]
        .into_iter()
        .map(|s| (s, Kind::Function))
        .collect();
    ImpactIndex::build(docs, &kinds, None)
}

fn impact(ix: ImpactIndex) -> Arc<ImpactMap> {
    Arc::new(ImpactMap::new(ix, &|_| None, ImpactOptions::default()))
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

async fn expand(map: &dyn Map, node: &NodeId) -> Vec<Edge> {
    check_expand(map, node, Duration::from_secs(10), &view())
        .await
        .expect("C7")
}

fn name_of(node: &NodeId) -> &str {
    match node {
        NodeId::Symbol(key) => &key.name,
        other => panic!("not a symbol: {other}"),
    }
}

fn names(edges: &[Edge]) -> Vec<&str> {
    let mut names: Vec<&str> = edges.iter().map(|e| name_of(&e.dst)).collect();
    names.sort_unstable();
    names
}

#[tokio::test]
async fn a_symbol_expands_to_the_test_units_that_reference_it_and_nothing_else() {
    let shared = impact(demo());
    let map = TestedByMap::new(shared.clone());
    let foo = shared.node_of(&sym("a/foo().")).unwrap().clone();
    let edges = expand(&map, &foo).await;
    assert_eq!(names(&edges), vec!["inline_t", "t_foo"]);
    for edge in &edges {
        assert_eq!(edge.kind, EdgeKind::Tests);
        assert_eq!(edge.map, TESTED_BY_ID);
        assert_eq!(edge.witness, WitnessClass::Compiler);
        assert_eq!(edge.conf, CONF_MAX);
        assert_eq!(edge.src, foo);
        let site = edge.site.as_ref().unwrap();
        assert!(site.blob.is_empty());
        match name_of(&edge.dst) {
            "t_foo" => assert_eq!(
                (site.path.as_str(), site.line, site.col),
                ("tests/a_test.rs", 0, 12)
            ),
            "inline_t" => assert_eq!(
                (site.path.as_str(), site.line, site.col),
                ("src/a.rs", 2, 28)
            ),
            other => panic!("{other}"),
        }
    }
    assert_eq!(map.test_units(), 2, "inline_t by chain, t_foo by path");
    // `use_foo` is a plain user, not a test, and uses nothing a test unit holds.
    let use_foo = shared.node_of(&sym("a/use_foo().")).unwrap().clone();
    assert!(expand(&map, &use_foo).await.is_empty());
    // A node the map does not hold gives nothing.
    let stranger = NodeId::Symbol(SymbolKey {
        language: "rust".into(),
        module_path: Vec::new(),
        container: Vec::new(),
        name: "nowhere".into(),
        kind: SymbolKind::Function,
        disambiguator: 0,
    });
    assert!(expand(&map, &stranger).await.is_empty());
}

#[tokio::test]
async fn the_scip_impact_edges_are_unchanged_by_the_split() {
    let shared = impact(demo());
    let foo = shared.node_of(&sym("a/foo().")).unwrap().clone();
    let edges = expand(shared.as_ref(), &foo).await;
    assert_eq!(names(&edges), vec!["inline_t", "t_foo", "use_foo"]);
    for edge in &edges {
        assert_eq!(edge.kind, EdgeKind::CallsOrRefs);
        assert_eq!(edge.map, SCIP_IMPACT_ID);
        assert_eq!(edge.witness, WitnessClass::Compiler);
        assert_eq!(edge.conf, CONF_MAX);
        assert_eq!(edge.src, foo);
        assert!(edge.site.as_ref().unwrap().blob.is_empty());
    }
    let use_foo = edges.iter().find(|e| name_of(&e.dst) == "use_foo").unwrap();
    let site = use_foo.site.as_ref().unwrap();
    assert_eq!(
        (site.path.as_str(), site.line, site.col),
        ("src/a.rs", 1, 15)
    );
}

/// `target` is used by `callers` functions in `tests/crowd.rs`, one each.
fn crowd(callers: usize) -> (ImpactIndex, String) {
    let target = sym("a/target().");
    let mut occurrences = Vec::new();
    let mut kinds: HashMap<String, Kind> = [(target.clone(), Kind::Function)].into_iter().collect();
    for i in 0..callers {
        let name = sym(&format!("crowd/t_{i:03}()."));
        let line = i as u32;
        occurrences.push(occ((line, 3, line, 8), &name, 1, Some((line, 0, line, 40))));
        occurrences.push(occ((line, 20, line, 26), &target, 0, None));
        kinds.insert(name, Kind::Function);
    }
    let docs = vec![
        RawDocument {
            path: "src/a.rs".into(),
            occurrences: vec![occ((0, 3, 0, 9), &target, 1, Some((0, 0, 0, 20)))],
        },
        RawDocument {
            path: "tests/crowd.rs".into(),
            occurrences,
        },
    ];
    (ImpactIndex::build(docs, &kinds, None), target)
}

#[tokio::test]
async fn edges_are_sorted_and_capped_at_the_expand_limit() {
    let (ix, target) = crowd(EXPAND_LIMIT + 5);
    let shared = impact(ix);
    let map = TestedByMap::new(shared.clone());
    let node = shared.node_of(&target).unwrap().clone();
    let edges = expand(&map, &node).await;
    assert_eq!(edges.len(), EXPAND_LIMIT);
    let (mut all, total) = (map.all_edges(&node).0, EXPAND_LIMIT + 5);
    assert_eq!(all.len(), total);
    sort_edges(&mut all);
    assert_eq!(edges, all[..EXPAND_LIMIT]);

    let seeds = [(node, 1.0)];
    let params = serde_json::Value::Null;
    let response = map
        .query(
            &MapRequest {
                text: None,
                seeds: &seeds,
                limit: usize::MAX,
                budget: budget(),
                params: &params,
                as_of: None,
            },
            &view(),
        )
        .await;
    assert_eq!(response.hits.len(), EXPAND_LIMIT);
    assert!(matches!(
        &response.status,
        MapStatus::Partial { note: Some(note), .. } if note.starts_with("5 more")
    ));
}

#[tokio::test]
async fn query_offers_one_hit_per_test_unit_with_scip_evidence() {
    let foo = sym("a/foo().");
    let bar = sym("a/bar().");
    let t_both = sym("a_test/t_both().");
    let docs = vec![
        RawDocument {
            path: "src/a.rs".into(),
            occurrences: vec![
                occ((0, 3, 0, 6), &foo, 1, Some((0, 0, 0, 12))),
                occ((1, 3, 1, 6), &bar, 1, Some((1, 0, 1, 12))),
            ],
        },
        RawDocument {
            path: "tests/a_test.rs".into(),
            occurrences: vec![
                occ((0, 3, 0, 9), &t_both, 1, Some((0, 0, 0, 30))),
                occ((0, 12, 0, 15), &foo, 0, None),
                occ((0, 20, 0, 23), &bar, 0, None),
            ],
        },
    ];
    let kinds: HashMap<String, Kind> = [foo.clone(), bar.clone(), t_both]
        .into_iter()
        .map(|s| (s, Kind::Function))
        .collect();
    let shared = impact(ImpactIndex::build(docs, &kinds, None));
    let map = TestedByMap::new(shared.clone());
    let foo = shared.node_of(&foo).unwrap().clone();
    let bar = shared.node_of(&bar).unwrap().clone();
    let seeds = [(foo.clone(), 1.0), (bar.clone(), 0.5)];
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
    assert_eq!(response.hits.len(), 1, "{:?}", response.hits);
    let hit = &response.hits[0];
    assert_eq!(hit.rank, 1);
    assert_eq!(name_of(&hit.node), "t_both");
    let evidence = &hit.evidence[0];
    assert!(
        evidence
            .via
            .as_ref()
            .is_some_and(|via| *via == foo || *via == bar)
    );
    assert_eq!(evidence.edge, EdgeKind::Tests);
    assert_eq!(evidence.label, SourceLabel::Scip);
    assert!(
        evidence
            .detail
            .as_deref()
            .is_some_and(|d| d.starts_with("tests the seed at tests/a_test.rs:1"))
    );
}

// ---- pins: the seeded `query` output -------------------------------------------------

async fn ask(map: &TestedByMap, seeds: &[(NodeId, f32)], limit: usize) -> MapResponse {
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
    let (ix, target) = crowd(EXPAND_LIMIT + 5);
    let shared = impact(ix);
    let map = TestedByMap::new(shared.clone());
    let node = shared.node_of(&target).unwrap().clone();
    let response = ask(&map, &[(node.clone(), 1.0)], 3).await;
    assert_eq!(
        response.status,
        MapStatus::Partial {
            note: Some("5 more test units than the cap of 256 were not offered".to_string()),
            warning: None,
        }
    );
    assert_eq!(response.hits.len(), 3);
    let hit = &response.hits[0];
    assert_eq!(hit.rank, 1);
    assert_eq!(hit.score, Some(1.0));
    assert_eq!(response.hits[2].rank, 3);
    assert_eq!(name_of(&hit.node), "t_000");
    assert_eq!(hit.evidence.len(), 1);
    let evidence = &hit.evidence[0];
    assert_eq!(evidence.via, Some(node));
    assert_eq!(evidence.edge, EdgeKind::Tests);
    assert_eq!(evidence.label, SourceLabel::Scip);
    assert_eq!(
        evidence.detail.as_deref(),
        Some("tests the seed at tests/crowd.rs:1")
    );
}

#[tokio::test]
async fn query_dedupes_a_shared_destination_and_truncates_to_the_limit() {
    let foo = sym("a/foo().");
    let bar = sym("a/bar().");
    let t_both = sym("a_test/t_both().");
    let t_foo = sym("a_test/t_foo().");
    let docs = vec![
        RawDocument {
            path: "src/a.rs".into(),
            occurrences: vec![
                occ((0, 3, 0, 6), &foo, 1, Some((0, 0, 0, 12))),
                occ((1, 3, 1, 6), &bar, 1, Some((1, 0, 1, 12))),
            ],
        },
        RawDocument {
            path: "tests/a_test.rs".into(),
            occurrences: vec![
                occ((0, 3, 0, 9), &t_both, 1, Some((0, 0, 0, 30))),
                occ((0, 12, 0, 15), &foo, 0, None),
                occ((0, 20, 0, 23), &bar, 0, None),
                occ((1, 3, 1, 9), &t_foo, 1, Some((1, 0, 1, 30))),
                occ((1, 12, 1, 15), &foo, 0, None),
            ],
        },
    ];
    let kinds: HashMap<String, Kind> = [foo.clone(), bar.clone(), t_both, t_foo]
        .into_iter()
        .map(|s| (s, Kind::Function))
        .collect();
    let shared = impact(ImpactIndex::build(docs, &kinds, None));
    let map = TestedByMap::new(shared.clone());
    let foo = shared.node_of(&foo).unwrap().clone();
    let bar = shared.node_of(&bar).unwrap().clone();
    // Both seeds reach t_both (foo at tests/a_test.rs:1 col 12, bar at col 20);
    // only foo reaches t_foo.
    let seeds = [(foo.clone(), 1.0), (bar, 1.0)];
    let all = ask(&map, &seeds, 10).await;
    assert_eq!(all.status, MapStatus::Ok);
    assert_eq!(all.hits.len(), 2, "{:?}", all.hits);
    assert_eq!(
        all.hits.iter().map(|h| h.rank).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let both = all
        .hits
        .iter()
        .find(|h| name_of(&h.node) == "t_both")
        .expect("t_both is offered once");
    // The first edge in sort order survives: the one with the smaller site.
    assert_eq!(both.evidence[0].via, Some(foo));
    assert_eq!(
        both.evidence[0].detail.as_deref(),
        Some("tests the seed at tests/a_test.rs:1")
    );

    let one = ask(&map, &seeds, 1).await;
    assert_eq!(one.hits.len(), 1);
    assert_eq!(one.hits[0].rank, 1);
    assert_eq!(one.hits[0].node, all.hits[0].node);
    assert_eq!(one.status, MapStatus::Ok);
}

#[test]
fn the_two_rules_and_their_limits() {
    let chain = |names: &[&str]| Chain {
        names: names.iter().map(|s| (*s).to_owned()).collect(),
        owner: None,
    };
    // Rule 1: the path.
    assert!(is_test_unit("tests/a_test.rs", None));
    assert!(is_test_unit("src/a/tests.rs", Some(&chain(&["a", "f"]))));
    assert!(!is_test_unit("src/a.rs", Some(&chain(&["a", "f"]))));
    assert!(!is_test_unit("src/a.rs", None));
    // Rule 2: a `tests` or `test` segment before the unit's own name.
    assert!(is_test_unit("src/a.rs", Some(&chain(&["a", "tests", "t"]))));
    assert!(is_test_unit(
        "src/a.rs",
        Some(&chain(&["a", "test", "Fixture", "t"]))
    ));
    // A bare `test.rs` is a source file: its own module is not a test module.
    assert!(!is_test_unit(
        "xtask/src/test.rs",
        Some(&chain(&["test", "run"]))
    ));
    assert!(!is_test_unit(
        "xtask/src/test.rs",
        Some(&chain(&["xtask", "test", "run"]))
    ));
    // But a `tests` module inside it, or a `test` module in another file, is.
    assert!(is_test_unit(
        "xtask/src/test.rs",
        Some(&chain(&["test", "tests", "t"]))
    ));
    assert!(is_test_unit("src/b.rs", Some(&chain(&["test", "run"]))));
    // The unit's own name alone does not make it a test.
    assert!(!is_test_unit("src/a.rs", Some(&chain(&["a", "tests"]))));
    assert!(!is_test_unit("src/a.rs", Some(&chain(&["tests"]))));
}
