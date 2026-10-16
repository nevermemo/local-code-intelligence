//! The recipes over an in-memory SCIP index: anchors and their stops, the
//! caps, ring arithmetic, path order, the key joins and `lci_compose`.

use super::*;
use lci_maps::{
    config_key::ConfigKeyOptions,
    impact::Decl,
    scip_graph::{RawDocument, RawOccurrence, ScipGraph},
    scip_impact::ImpactOptions,
};
use scip::types::symbol_information::Kind;

const EXAMPLE: &str = "[server]\nport = 1\nhost = \"x\"\n\n[client]\nport = 2\n";

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
        symbol: symbol.to_owned(),
        roles,
        enclosing,
    }
}

/// `src/a.rs`: `struct Foo` (0), `impl Foo { fn run }` (1), `use_foo` calls
/// both (2), `deep` calls `use_foo` (3), `top` calls `deep` (4), the field
/// `Config.port` (5), `read_port` and `set_port` use it (7, 8).
/// `tests/t.rs`: `test_run` calls `run` and `read_port`. `src/b.rs`: a free
/// `fn run`.
fn demo_docs() -> Vec<RawDocument> {
    let (foo, run, use_foo, deep, top) = (
        sym("a/Foo#"),
        sym("a/impl#[Foo]run()."),
        sym("a/use_foo()."),
        sym("a/deep()."),
        sym("a/top()."),
    );
    let (port, read_port, set_port) = (
        sym("a/Config#port."),
        sym("a/read_port()."),
        sym("a/set_port()."),
    );
    vec![
        RawDocument {
            path: "src/a.rs".into(),
            occurrences: vec![
                occ((0, 7, 0, 10), &foo, 1, Some((0, 0, 0, 14))),
                occ((1, 14, 1, 17), &run, 1, Some((1, 11, 1, 30))),
                occ((2, 3, 2, 10), &use_foo, 1, Some((2, 0, 2, 40))),
                occ((2, 15, 2, 18), &foo, 0, None),
                occ((2, 20, 2, 23), &run, 0, None),
                occ((3, 3, 3, 7), &deep, 1, Some((3, 0, 3, 30))),
                occ((3, 12, 3, 19), &use_foo, 0, None),
                occ((4, 3, 4, 6), &top, 1, Some((4, 0, 4, 30))),
                occ((4, 12, 4, 16), &deep, 0, None),
                occ((5, 2, 5, 6), &port, 1, Some((5, 2, 5, 12))),
                occ((7, 3, 7, 12), &read_port, 1, Some((7, 0, 7, 30))),
                occ((7, 15, 7, 19), &port, 0, None),
                occ((8, 3, 8, 11), &set_port, 1, Some((8, 0, 8, 30))),
                occ((8, 15, 8, 19), &port, 0, None),
            ],
        },
        RawDocument {
            path: "tests/t.rs".into(),
            occurrences: vec![
                occ((0, 3, 0, 11), &sym("t/test_run()."), 1, Some((0, 0, 0, 40))),
                occ((0, 15, 0, 18), &run, 0, None),
                occ((0, 20, 0, 29), &read_port, 0, None),
            ],
        },
        RawDocument {
            path: "src/b.rs".into(),
            occurrences: vec![occ((0, 3, 0, 6), &sym("b/run()."), 1, Some((0, 0, 0, 10)))],
        },
    ]
}

fn demo_kinds() -> HashMap<String, Kind> {
    [
        ("a/Foo#", Kind::Struct),
        ("a/impl#[Foo]run().", Kind::Method),
        ("a/use_foo().", Kind::Function),
        ("a/deep().", Kind::Function),
        ("a/top().", Kind::Function),
        ("a/Config#port.", Kind::Field),
        ("a/read_port().", Kind::Function),
        ("a/set_port().", Kind::Function),
        ("t/test_run().", Kind::Function),
        ("b/run().", Kind::Function),
    ]
    .into_iter()
    .map(|(symbol, kind)| (sym(symbol), kind))
    .collect()
}

fn decl(name: &str, line: u32, symbol: &str, path: &str) -> Decl {
    Decl {
        path: path.into(),
        line,
        name: name.into(),
        symbol: sym(symbol),
    }
}

fn key_edge(key: &str, decl: Decl) -> reads_key::Edge {
    reads_key::Edge {
        key: key.into(),
        decl,
        w1: true,
        w2: false,
        l: false,
        roles: Default::default(),
        in_def_file: false,
        outside_scip: false,
    }
}

fn sources_of(
    docs: Vec<RawDocument>,
    kinds: &HashMap<String, Kind>,
    edges: &[reads_key::Edge],
) -> Sources {
    let scip = Arc::new(ScipMap::with_default_ids(ScipGraph::build(&docs, None)));
    let ix = ImpactIndex::build(docs, kinds, None);
    let node = |symbol: &str| {
        scip.graph()
            .node_index(symbol)
            .map(|node| scip.node_id(node).clone())
    };
    let impact = Arc::new(ImpactMap::new(ix, &node, ImpactOptions::default()));
    let keys = ConfigKeyMap::new(
        edges,
        &|decl: &Decl| node(&decl.symbol),
        ConfigKeyOptions::default(),
    );
    Sources {
        scip: scip.clone(),
        impact,
        keys: Some(Arc::new(keys)),
        key_ids: key_ids_of(EXAMPLE),
    }
}

fn demo() -> Sources {
    let edges = [
        key_edge(
            "server.port",
            decl("read_port", 8, "a/read_port().", "src/a.rs"),
        ),
        key_edge(
            "server.port",
            decl("set_port", 9, "a/set_port().", "src/a.rs"),
        ),
    ];
    sources_of(demo_docs(), &demo_kinds(), &edges)
}

fn names(value: &Value, group: &str) -> Vec<String> {
    value["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["group"] == group)
        .map(|row| row["name"].as_str().unwrap().to_owned())
        .collect()
}

fn started() -> Instant {
    Instant::now()
}

fn rings(symbol: &str, rings: Option<u32>) -> ImpactRingsArgs {
    ImpactRingsArgs {
        workspace_path: String::new(),
        symbol: symbol.into(),
        rings,
    }
}

#[test]
fn rings_count_the_declarations_that_reference_the_anchor_and_those_that_reference_them() {
    let sources = demo();
    let answer = impact_rings(&sources, &rings("Foo::run", None), started()).unwrap();
    assert_eq!(answer["recipe"], "impact_rings");
    assert_eq!(answer["stop"]["reason"], "complete");
    assert_eq!(answer["anchor"]["how"], "exact");
    assert_eq!(names(&answer, "ring_1"), ["use_foo", "test_run"]);
    assert_eq!(names(&answer, "ring_2"), ["deep"]);
    let tests: Vec<&Value> = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["role"] == "test")
        .collect();
    assert_eq!(tests.len(), 1);
    assert_eq!(tests[0]["name"], "test_run");
    // One closure for ring 1, one expansion per ring-1 declaration for ring 2.
    assert_eq!(answer["counts"]["operator_calls"], 3);
    assert_eq!(answer["steps"].as_array().unwrap().len(), 3);

    let one = impact_rings(&sources, &rings("Foo::run", Some(1)), started()).unwrap();
    assert!(names(&one, "ring_2").is_empty());
    assert_eq!(one["counts"]["operator_calls"], 1);
}

#[test]
fn a_type_name_anchors_and_a_node_id_given_back_resolves_exactly() {
    let sources = demo();
    let answer = impact_rings(&sources, &rings("Foo", Some(1)), started()).unwrap();
    assert_eq!(answer["anchor"]["kind"], "struct");
    assert_eq!(names(&answer, "ring_1"), ["use_foo"]);

    let field = impact_rings(&sources, &rings("Config::port", Some(1)), started()).unwrap();
    assert_eq!(names(&field, "ring_1"), ["read_port", "set_port"]);

    let id = answer["anchor"]["resolved"].as_str().unwrap().to_owned();
    assert!(id.starts_with("sym:"), "{id}");
    let by_id = impact_rings(&sources, &rings(&id, Some(1)), started()).unwrap();
    assert_eq!(by_id["anchor"]["how"], "id");
    assert_eq!(names(&by_id, "ring_1"), ["use_foo"]);
}

#[test]
fn a_name_that_matches_several_declarations_stops_with_the_candidates() {
    let sources = demo();
    let answer = impact_rings(&sources, &rings("run", None), started()).unwrap();
    assert_eq!(answer["stop"]["reason"], "ambiguous_anchor");
    assert_eq!(answer["stop"]["at"], "s1");
    assert_eq!(answer["stop"]["partial"], true);
    assert!(answer["anchor"]["resolved"].is_null());
    let candidates = answer["anchor"]["candidates"].as_array().unwrap();
    assert_eq!(candidates.len(), 2);
    let kinds: Vec<&str> = candidates
        .iter()
        .map(|c| c["kind"].as_str().unwrap())
        .collect();
    assert!(
        kinds.contains(&"method") && kinds.contains(&"function"),
        "{kinds:?}"
    );
    assert!(answer["results"].as_array().unwrap().is_empty());
    assert_eq!(answer["counts"]["operator_calls"], 0);
    // The qualified name tells the two apart, and so does the node id.
    let qualified: Vec<&str> = candidates
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert!(
        qualified.contains(&"a::Foo::run") && qualified.contains(&"b::run"),
        "{qualified:?}"
    );
}

#[test]
fn an_unknown_name_and_a_declaration_nothing_references_stop_as_empty() {
    let sources = demo();
    let unknown = impact_rings(&sources, &rings("Nothing::here", None), started()).unwrap();
    assert_eq!(unknown["stop"]["reason"], "empty");
    assert!(
        unknown["anchor"]["why"]
            .as_str()
            .unwrap()
            .contains("no declaration")
    );

    let unreferenced = impact_rings(&sources, &rings("top", None), started()).unwrap();
    assert_eq!(unreferenced["stop"]["reason"], "empty");
    assert_eq!(unreferenced["stop"]["at"], "s2");
    assert_eq!(unreferenced["counts"]["operator_calls"], 1);

    assert!(impact_rings(&sources, &rings("Foo", Some(3)), started()).is_err());
}

#[test]
fn ring_one_is_cut_at_its_cap_and_says_what_it_did_not_compute() {
    let mut occurrences = vec![occ((0, 7, 0, 10), &sym("a/Foo#"), 1, Some((0, 0, 0, 14)))];
    let mut kinds = HashMap::from([(sym("a/Foo#"), Kind::Struct)]);
    for i in 0..(CAP_RING1 as u32 + 5) {
        let name = format!("a/user{i}().");
        let line = i + 1;
        occurrences.push(occ(
            (line, 3, line, 8),
            &sym(&name),
            1,
            Some((line, 0, line, 30)),
        ));
        occurrences.push(occ((line, 12, line, 15), &sym("a/Foo#"), 0, None));
        kinds.insert(sym(&name), Kind::Function);
    }
    let docs = vec![RawDocument {
        path: "src/a.rs".into(),
        occurrences,
    }];
    let sources = sources_of(docs, &kinds, &[]);
    let answer = impact_rings(&sources, &rings("Foo", None), started()).unwrap();
    assert_eq!(answer["stop"]["reason"], "fanout_cap");
    assert_eq!(answer["stop"]["partial"], true);
    assert_eq!(names(&answer, "ring_1").len(), CAP_RING1);
    assert!(names(&answer, "ring_2").is_empty());
    assert!(answer["stop"]["next"].is_null());
    assert_eq!(answer["counts"]["cards_seen"], CAP_RING1 + 5);
}

fn path_args(from: &str, to: &str, max_depth: Option<u32>) -> FlowPathArgs {
    FlowPathArgs {
        workspace_path: String::new(),
        from: from.into(),
        to: to.into(),
        max_depth,
    }
}

#[test]
fn a_path_lists_the_functions_between_the_ends_in_call_order() {
    let sources = demo();
    let answer = flow_path(&sources, &path_args("top", "Foo::run", None), started()).unwrap();
    assert_eq!(answer["stop"]["reason"], "complete");
    assert_eq!(answer["path_length"], 3);
    assert_eq!(
        names(&answer, "path"),
        ["top", "deep", "use_foo", "Foo::run"]
    );
    let depths: Vec<u64> = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["depth"].as_u64().unwrap())
        .collect();
    assert_eq!(depths, [0, 1, 2, 3]);
    assert_eq!(answer["counts"]["operator_calls"], 2);
}

#[test]
fn no_path_within_the_depth_asks_for_a_deeper_search_and_reports_the_frontiers() {
    let sources = demo();
    let short = flow_path(&sources, &path_args("top", "Foo::run", Some(2)), started()).unwrap();
    assert_eq!(short["stop"]["reason"], "empty");
    assert_eq!(short["stop"]["next"]["recipe"], "flow_path");
    assert_eq!(short["stop"]["next"]["args"]["max_depth"], 4);
    assert!(
        short["stop"]["unexplored"][0]["forward_frontier"]
            .as_u64()
            .unwrap()
            >= 3
    );

    // Calls only go down: the reverse direction has no path at any depth.
    let reversed = flow_path(&sources, &path_args("Foo::run", "top", Some(8)), started()).unwrap();
    assert_eq!(reversed["stop"]["reason"], "empty");
    assert!(reversed["stop"]["next"].is_null());

    // A type is not a node of the call graph.
    let typed = flow_path(&sources, &path_args("Foo", "top", None), started()).unwrap();
    assert_eq!(typed["stop"]["reason"], "empty");
    assert!(
        typed["anchor"]["why"]
            .as_str()
            .unwrap()
            .contains("call graph")
    );

    let ambiguous = flow_path(&sources, &path_args("run", "top", None), started()).unwrap();
    assert_eq!(ambiguous["stop"]["reason"], "ambiguous_anchor");
    assert!(flow_path(&sources, &path_args("top", "run", Some(9)), started()).is_err());
}

fn readers(key: &str, include_tests: bool) -> ConfigReadersArgs {
    ConfigReadersArgs {
        workspace_path: String::new(),
        key: key.into(),
        include_tests,
    }
}

#[tokio::test]
async fn readers_of_a_key_come_with_the_tests_that_reference_them() {
    let sources = demo();
    let answer = config_readers(&sources, &readers("server.port", true), started())
        .await
        .unwrap();
    assert_eq!(answer["stop"]["reason"], "complete");
    assert_eq!(answer["anchor"]["resolved"], "cfg:toml:server.port");
    assert_eq!(names(&answer, "readers"), ["read_port", "set_port"]);
    assert_eq!(names(&answer, "tests"), ["test_run"]);
    let test = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["group"] == "tests")
        .unwrap();
    assert_eq!(test["role"], "test");
    assert_eq!(test["note"], "references read_port");
    // The key lookup, then one expansion per reader.
    assert_eq!(answer["counts"]["operator_calls"], 3);

    let bare = config_readers(&sources, &readers("cfg:toml:server.port", false), started())
        .await
        .unwrap();
    assert!(names(&bare, "tests").is_empty());
    assert_eq!(bare["counts"]["operator_calls"], 1);
}

#[tokio::test]
async fn a_key_name_that_two_sections_share_is_ambiguous_and_an_unknown_key_is_empty() {
    let sources = demo();
    let both = config_readers(&sources, &readers("port", true), started())
        .await
        .unwrap();
    assert_eq!(both["stop"]["reason"], "ambiguous_anchor");
    assert_eq!(
        both["anchor"]["candidates"],
        json!(["server.port", "client.port"])
    );

    let host = config_readers(&sources, &readers("host", true), started())
        .await
        .unwrap();
    assert_eq!(host["anchor"]["resolved"], "cfg:toml:server.host");
    assert_eq!(host["stop"]["reason"], "empty");
    assert_eq!(host["stop"]["at"], "s1");

    let unknown = config_readers(&sources, &readers("server", true), started())
        .await
        .unwrap();
    assert_eq!(unknown["stop"]["reason"], "empty");
    assert_eq!(
        unknown["anchor"]["candidates"],
        json!(["server.port", "server.host"])
    );
}

#[tokio::test]
async fn neighbours_join_a_declaration_to_its_keys_their_other_readers_and_its_callers() {
    let sources = demo();
    let args = ConfigNeighboursArgs {
        workspace_path: String::new(),
        symbol: "read_port".into(),
    };
    let answer = config_neighbours(&sources, &args, started()).await.unwrap();
    assert_eq!(answer["stop"]["reason"], "complete");
    assert_eq!(names(&answer, "keys"), ["server.port"]);
    assert_eq!(names(&answer, "other_readers"), ["set_port"]);
    assert_eq!(names(&answer, "callers"), ["test_run"]);
    let other = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["group"] == "other_readers")
        .unwrap();
    assert_eq!(other["note"], "reads server.port");
    // Keys of the declaration, readers of its one key, callers.
    assert_eq!(answer["counts"]["operator_calls"], 3);

    let none = ConfigNeighboursArgs {
        symbol: "top".into(),
        ..args
    };
    let empty = config_neighbours(&sources, &none, started()).await.unwrap();
    assert_eq!(empty["stop"]["reason"], "empty");
    assert_eq!(empty["stop"]["next"]["recipe"], "impact_rings");
    assert_eq!(empty["stop"]["next"]["args"]["symbol"], "top");
}

fn compose_of(steps: Value) -> ComposeArgs {
    serde_json::from_value(json!({"workspace_path": "", "steps": steps})).unwrap()
}

#[tokio::test]
async fn compose_runs_references_between_steps() {
    let sources = demo();
    let args = compose_of(json!([
        {"id": "s1", "op": "seeds.scip_names", "in": {"names": ["Foo::run"]}},
        {"id": "s2", "op": "exact.closure", "in": {"symbols": "$s1.symbols"}},
        {"id": "s3", "op": "pick", "in": {"from": "$s2", "role": "test"}},
        {"id": "s4", "op": "set.minus", "in": {"of": ["$s2.symbols", "$s3.symbols"]}},
    ]));
    let answer = compose::run(&sources, &args, started()).await.unwrap();
    assert_eq!(answer["recipe"], "lci_compose");
    assert_eq!(answer["stop"]["reason"], "complete");
    assert_eq!(names(&answer, "s4"), ["use_foo"]);
    assert_eq!(answer["resolved"]["s2"]["n"], 2);
    assert_eq!(
        answer["resolved"]["s3"]["listed"][0],
        "test_run tests/t.rs:1"
    );
    // One name lookup, one closure; pick and set operators are plan-local.
    assert_eq!(answer["counts"]["operator_calls"], 2);
    assert_eq!(answer["steps"].as_array().unwrap().len(), 4);
}

#[tokio::test]
async fn compose_walks_impact_and_callees_with_their_depths_and_joins_keys() {
    let sources = demo();
    let impact = compose_of(json!([
        {"id": "a", "op": "scip.impact", "in": {"symbols": ["Foo::run"], "depth": 2}},
    ]));
    let answer = compose::run(&sources, &impact, started()).await.unwrap();
    let rows = answer["results"].as_array().unwrap();
    let listed: Vec<(&str, u64)> = rows
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r["depth"].as_u64().unwrap()))
        .collect();
    assert_eq!(listed, [("use_foo", 1), ("test_run", 1), ("deep", 2)]);

    let callees = compose_of(json!([
        {"id": "a", "op": "graph.scip", "in": {"symbols": "top", "depth": 3}},
    ]));
    let answer = compose::run(&sources, &callees, started()).await.unwrap();
    assert_eq!(names(&answer, "a"), ["deep", "use_foo", "Foo::run"]);

    let keys = compose_of(json!([
        {"id": "k", "op": "config.key", "in": {"keys": ["server.port"]}},
        {"id": "back", "op": "config.key", "in": {"symbols": "$k.symbols"}},
        {"id": "u", "op": "set.union", "in": {"of": ["$back.keys", "$back.keys"]}},
    ]));
    let answer = compose::run(&sources, &keys, started()).await.unwrap();
    assert_eq!(names(&answer, "u"), ["server.port"]);
    assert_eq!(answer["resolved"]["k"]["n"], 2);
    assert_eq!(answer["counts"]["operator_calls"], 3);
}

#[tokio::test]
async fn compose_stops_on_an_ambiguous_name_and_rejects_a_malformed_request() {
    let sources = demo();
    let ambiguous = compose_of(json!([
        {"id": "s1", "op": "seeds.scip_names", "in": {"names": ["Foo"]}},
        {"id": "s2", "op": "exact.closure", "in": {"symbols": ["run"]}},
    ]));
    let answer = compose::run(&sources, &ambiguous, started()).await.unwrap();
    assert_eq!(answer["stop"]["reason"], "ambiguous_anchor");
    assert_eq!(answer["stop"]["at"], "s2");
    assert_eq!(answer["anchor"]["candidates"].as_array().unwrap().len(), 2);
    assert_eq!(answer["resolved"]["s1"]["n"], 1);

    for steps in [
        json!([]),
        json!([{"id": "s1", "op": "nope", "in": {}}]),
        json!([{"id": "s1", "op": "exact.closure", "in": {"symbols": "$s9"}}]),
        json!([{"id": "s1", "op": "exact.closure", "in": {"depth": 2, "symbols": ["Foo"]}}]),
        json!([{"id": "s1", "op": "seeds.scip_names", "in": {"names": ["Foo"]}},
               {"id": "s2", "op": "exact.closure", "in": {"symbols": "$s1.files"}}]),
        json!([{"id": "s1", "op": "scip.impact", "in": {"symbols": ["Foo"], "depth": 9}}]),
        json!([{"id": "s1", "op": "seeds.scip_names", "in": {"names": ["Foo"]}},
               {"id": "s1", "op": "seeds.scip_names", "in": {"names": ["Foo"]}}]),
    ] {
        assert!(
            compose::run(&sources, &compose_of(steps.clone()), started())
                .await
                .is_err(),
            "{steps}"
        );
    }
    let seven: Vec<Value> = (0..7)
        .map(|i| json!({"id": format!("s{i}"), "op": "seeds.scip_names", "in": {"names": ["Foo"]}}))
        .collect();
    assert!(
        compose::run(&sources, &compose_of(Value::Array(seven)), started())
            .await
            .is_err()
    );
}

#[test]
fn requests_parse_by_name_and_refuse_unknown_fields() {
    let ok = Request::parse(
        "impact_rings",
        &json!({"workspace_path": "w", "symbol": "Foo"}),
    )
    .unwrap();
    assert_eq!(ok.name(), "impact_rings");
    assert!(!ok.needs_keys());
    let readers = Request::parse(
        "config_readers",
        &json!({"workspace_path": "w", "key": "k"}),
    )
    .unwrap();
    assert!(readers.needs_keys());
    assert!(matches!(readers, Request::ConfigReaders(ref a) if a.include_tests));
    assert!(
        Request::parse(
            "impact_rings",
            &json!({"workspace_path": "w", "symbol": "Foo", "ring": 1})
        )
        .is_err()
    );
    assert!(Request::parse("impact_rings", &json!({"symbol": "Foo"})).is_err());
    let error = Request::parse("history_of", &json!({}))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("config_readers") && error.contains("lci_compose"),
        "{error}"
    );
}

#[test]
fn roles_follow_the_path_and_globs_match_names() {
    assert_eq!(role_of("tests/t.rs"), "test");
    assert_eq!(role_of("src/app/recipes/tests.rs"), "test");
    assert_eq!(role_of("lci-maps/tests/a/b.rs"), "test");
    assert_eq!(role_of("src/foo_test.rs"), "test");
    assert_eq!(role_of("src/contests.rs"), "prod");
    assert_eq!(role_of("src/app/recipes.rs"), "prod");
    assert!(compose::glob_match("src/*.rs", "src/a.rs"));
    assert!(compose::glob_match("*run*", "Foo::run"));
    assert!(compose::glob_match("a?c", "abc"));
    assert!(!compose::glob_match("src/*.rs", "src/a.toml"));
    assert!(!compose::glob_match("a?c", "ac"));
    assert!(compose::glob_match("*", ""));
}

/// A tree with an `index.scip` written to disk (kinds included): `alpha`
/// references `Foo`, `beta` calls `alpha`.
struct Tree {
    dir: tempfile::TempDir,
}

impl Tree {
    #[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
    fn new() -> Self {
        use scip::types::{Document, Occurrence, SymbolInformation};
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "struct Foo;\nfn alpha() { Foo }\nfn beta() { alpha() }\n",
        )
        .unwrap();
        let occurrence = |range: &[i32], symbol: &str, roles: i32, enclosing: &[i32]| {
            let mut o = Occurrence::default();
            o.range = range.to_vec();
            o.symbol = sym(symbol);
            o.symbol_roles = roles;
            o.enclosing_range = enclosing.to_vec();
            o
        };
        let info = |symbol: &str, kind: Kind| {
            let mut i = SymbolInformation::default();
            i.symbol = sym(symbol);
            i.kind = kind.into();
            i
        };
        let mut document = Document::default();
        document.relative_path = "a.rs".to_owned();
        document.occurrences = vec![
            occurrence(&[0, 7, 10], "a/Foo#", 1, &[0, 0, 0, 11]),
            occurrence(&[1, 3, 8], "a/alpha().", 1, &[1, 0, 1, 27]),
            occurrence(&[1, 21, 24], "a/Foo#", 0, &[]),
            occurrence(&[2, 3, 7], "a/beta().", 1, &[2, 0, 2, 25]),
            occurrence(&[2, 12, 17], "a/alpha().", 0, &[]),
        ];
        document.symbols = vec![
            info("a/Foo#", Kind::Struct),
            info("a/alpha().", Kind::Function),
            info("a/beta().", Kind::Function),
        ];
        let mut message = scip::types::Index::default();
        message.documents = vec![document];
        scip::write_message_to_file(dir.path().join("index.scip"), message).unwrap();
        Self { dir }
    }

    async fn ask(&self, request: Value, flow_judge: bool) -> Value {
        let (name, args) = match request.as_object().unwrap().iter().next() {
            Some((name, args)) => (name.clone(), args.clone()),
            None => unreachable!(),
        };
        let mut args = args;
        args["workspace_path"] = json!(self.dir.path());
        let request = Request::parse(&name, &args).unwrap();
        let limits = TraverseConfig::default();
        let dirs = Dirs::under(&self.dir.path().join("data"));
        let index = self.dir.path().join("index.scip");
        let ctx = Ctx {
            limits: &limits,
            dirs: &dirs,
            scip_index: &index,
            root: self.dir.path(),
            flow_judge,
        };
        run(&ctx, &request).await.unwrap()
    }
}

#[tokio::test]
async fn recipes_run_over_an_index_file_and_the_walk_goes_through_the_traversal() {
    let tree = Tree::new();
    let rings = tree
        .ask(json!({"impact_rings": {"symbol": "Foo"}}), false)
        .await;
    assert_eq!(names(&rings, "ring_1"), ["alpha"], "{rings}");
    assert_eq!(names(&rings, "ring_2"), ["beta"], "{rings}");

    let path = tree
        .ask(json!({"flow_path": {"from": "beta", "to": "alpha"}}), false)
        .await;
    assert_eq!(names(&path, "path"), ["beta", "alpha"], "{path}");

    let walk = tree
        .ask(json!({"flow_from": {"entry": "beta", "k": 5}}), false)
        .await;
    assert_eq!(names(&walk, "walk"), ["alpha", "file:a.rs"], "{walk}");
    assert_eq!(walk["stop"]["reason"], "complete", "{walk}");
    assert!(
        walk["counts"]["operator_calls"].as_u64().unwrap() >= 1,
        "{walk}"
    );
    assert_eq!(walk["counts"]["judge_calls"], 0);
    assert!(
        walk["stop"]["unexplored"].as_array().unwrap().is_empty(),
        "{walk}"
    );
    assert_eq!(walk["steps"][1]["params"]["beam"], 64);

    // The judge flag is a switch with nothing behind it yet, and says so.
    let judged = tree
        .ask(json!({"flow_from": {"entry": "beta"}}), true)
        .await;
    assert_eq!(names(&judged, "walk"), names(&walk, "walk"));
    assert!(
        judged["stop"]["unexplored"][0]["flow_judge"]
            .as_str()
            .unwrap()
            .contains("no judge"),
        "{judged}"
    );
    assert_eq!(judged["counts"]["judge_calls"], 0);

    let typed = tree
        .ask(json!({"flow_from": {"entry": "Foo"}}), false)
        .await;
    assert_eq!(typed["stop"]["reason"], "empty");
}
