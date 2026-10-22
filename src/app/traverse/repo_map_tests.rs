//! Tests of the `seeds.repo_map` wiring in the traversal entry point: the key
//! `[run_profile.seeds] repo_map`, the note when it is off, the default set when
//! it is on, and the start nodes it adds after the other sources' (K18a; the
//! algorithm's own tests are in `repo_map.rs`).

use super::seeds::join_source;
use super::*;
use lci_core::config::profile::ScipMode;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The walk alone, as `tests.rs` runs it.
fn limits() -> TraverseConfig {
    TraverseConfig {
        enabled: true,
        first_stage: TraverseFirstStage::Off,
        answer_seeds: false,
        exact_seeds: false,
        ..TraverseConfig::default()
    }
}

/// `a.rs`: the three functions the question names (`parse_alpha`,
/// `parse_beta`, `parse_gamma`), the hub they all call (`read_frame`, whose
/// name holds none of the question's words) and ten functions that call
/// nothing, each on a line of its own.
const FUNCTIONS: [&str; 14] = [
    "parse_alpha",
    "parse_beta",
    "parse_gamma",
    "read_frame",
    "zeta_one",
    "kilo_three",
    "lima_four",
    "mike_five",
    "nova_six",
    "oscar_seven",
    "papa_eight",
    "quebec_nine",
    "romeo_ten",
    "sierra_eleven",
];
const QUESTION: &str = "parse alpha beta gamma";

fn symbol(name: &str) -> String {
    format!("rust-analyzer cargo demo 0.1.0 a/{name}().")
}

/// The tree with a SCIP index over `a.rs`.
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn tree() -> (tempfile::TempDir, PathBuf) {
    use scip::types::{Document, Index, Occurrence};
    let dir = tempfile::tempdir().unwrap();
    let source: String = FUNCTIONS.iter().map(|_| "fn f() { f(); }\n").collect();
    std::fs::write(dir.path().join("a.rs"), source).unwrap();
    let occurrence = |range: &[i32], symbol: &str, roles: i32, enclosing: &[i32]| {
        let mut o = Occurrence::default();
        o.range = range.to_vec();
        o.symbol = symbol.to_owned();
        o.symbol_roles = roles;
        o.enclosing_range = enclosing.to_vec();
        o
    };
    let mut document = Document::default();
    document.relative_path = "a.rs".to_owned();
    for (i, name) in FUNCTIONS.iter().enumerate() {
        let line = i as i32;
        document.occurrences.push(occurrence(
            &[line, 3, 8],
            &symbol(name),
            1,
            &[line, 0, line, 15],
        ));
        if i < 3 {
            document
                .occurrences
                .push(occurrence(&[line, 9, 15], &symbol("read_frame"), 0, &[]));
        }
    }
    let mut message = Index::default();
    message.documents = vec![document];
    let index = dir.path().join("index.scip");
    scip::write_message_to_file(&index, message).unwrap();
    (dir, index)
}

async fn loaded_tree(tree: &(tempfile::TempDir, PathBuf)) -> Arc<Loaded> {
    load(&tree.1, Some(tree.0.path()), ScipMode::Authoritative)
        .await
        .unwrap()
}

/// The node id of function `name` of the tree, as the responses print it.
fn id_of(loaded: &Loaded, name: &str) -> String {
    let at = loaded.scip.graph().node_index(&symbol(name)).unwrap();
    loaded.scip.node_id(at).to_string()
}

fn hit(lines: (u32, u32)) -> SeedHit {
    SeedHit {
        path: "a.rs".into(),
        start_line: lines.0,
        end_line: lines.1,
        content_hash: "abc123".into(),
        ordinal: None,
        rank: 1,
        retrievers: vec!["semantic".into(), "lexical".into()],
        semantic_rank: Some(1),
        lexical_rank: None,
        lsp_rank: None,
        fusion_score: 0.25,
        reranker_score: Some(0.75),
    }
}

/// A search with a canned answer that counts its calls.
struct Canned {
    answer: Vec<SeedHit>,
    calls: AtomicUsize,
}

#[lci_maps::async_trait]
impl SeedSearch for Canned {
    async fn search(&self, _question: &str, _k: usize) -> Result<Vec<SeedHit>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(self.answer.clone())
    }
}

/// The hybrid search finds `zeta_one` (line 5 of `a.rs`, 1-based).
fn zeta_search() -> Canned {
    Canned {
        answer: vec![hit((5, 5))],
        calls: AtomicUsize::new(0),
    }
}

async fn seeds_in(
    loaded: &Loaded,
    search: Option<&dyn SeedSearch>,
    question: &str,
    wanted: &[MapId],
) -> Seeds {
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: Path::new("unused"),
        scip_root: None,
        question,
        seeds: &[],
        seed_search: search,
        top_k: 8,
        maps: None,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    find_seeds(&limits(), &call, loaded, wanted).await.unwrap()
}

fn symbols(found: &Seeds) -> Vec<&str> {
    found
        .details
        .iter()
        .map(|detail| detail["symbol"].as_str().unwrap_or("-"))
        .collect()
}

#[tokio::test]
async fn a_hub_the_question_does_not_name_becomes_a_seed() {
    let tree = tree();
    let loaded = loaded_tree(&tree).await;
    // BM25 alone ranks the three functions whose names hold the words, and
    // not the hub they call.
    let bm25 = seeds_in(&loaded, None, QUESTION, &[SEEDS_BM25]).await;
    assert_eq!(bm25.nodes.len(), 3, "{:?}", symbols(&bm25));
    assert!(
        !bm25
            .nodes
            .iter()
            .any(|seed| seed.node.to_string() == id_of(&loaded, "read_frame"))
    );
    // The PageRank from those three puts the hub first.
    let found = seeds_in(&loaded, None, QUESTION, &[SEEDS_REPO_MAP]).await;
    assert_eq!(found.source, "repo_map");
    assert_eq!(found.nodes.len(), 4, "{:?}", symbols(&found));
    assert_eq!(
        found.nodes[0].node.to_string(),
        id_of(&loaded, "read_frame")
    );
    assert!(
        found
            .nodes
            .iter()
            .all(|seed| seed.map == Some(SEEDS_REPO_MAP))
    );
    assert_eq!(found.details.len(), found.nodes.len());
    let detail = &found.details[0];
    assert_eq!(detail["how"], "repo_map");
    assert_eq!(detail["kind"], "symbol");
    assert_eq!(detail["retrievers"], json!(["repo_map"]));
    assert_eq!(detail["score_kind"], "pagerank");
    assert_eq!(detail["node"], found.nodes[0].node.to_string());
    assert!(detail["symbol"].as_str().unwrap().ends_with("read_frame"));
    assert_eq!(detail["personalised"], 3);
    assert!((1..=30).contains(&detail["iterations"].as_u64().unwrap()));
    let scores: Vec<f64> = found
        .details
        .iter()
        .map(|d| d["score"].as_f64().unwrap())
        .collect();
    assert!(
        scores.windows(2).all(|pair| pair[0] >= pair[1]),
        "{scores:?}"
    );
    assert!(scores.iter().all(|score| *score > 0.0));
    // The same question again gives the same nodes and scores.
    let again = seeds_in(&loaded, None, QUESTION, &[SEEDS_REPO_MAP]).await;
    assert_eq!(again.details, found.details);
}

#[tokio::test]
async fn repo_map_seeds_come_after_the_hybrid_and_bm25_ones_and_dedup() {
    let tree = tree();
    let loaded = loaded_tree(&tree).await;
    let hybrid = zeta_search();
    let with = |extra: &[MapId]| -> Vec<MapId> {
        MAPS.iter().copied().chain(extra.iter().copied()).collect()
    };

    // The key's map not wanted: the edge lists are never built.
    let plain = seeds_in(&loaded, Some(&hybrid), QUESTION, &with(&[SEEDS_BM25])).await;
    assert_eq!(plain.source, "hybrid+bm25");
    assert!(!loaded.repo_map_built());

    // Hybrid, bm25, repo_map: in that order. The callers `seeds.bm25` gave are
    // not added again; the hub is.
    let all = seeds_in(
        &loaded,
        Some(&hybrid),
        QUESTION,
        &with(&[SEEDS_BM25, SEEDS_REPO_MAP]),
    )
    .await;
    assert!(loaded.repo_map_built());
    assert_eq!(all.source, "hybrid+bm25+repo_map");
    assert_eq!(
        all.nodes.len(),
        plain.nodes.len() + 1,
        "{:?}",
        symbols(&all)
    );
    assert_eq!(all.details.len(), all.nodes.len());
    for (node, earlier) in all.nodes.iter().zip(&plain.nodes) {
        assert_eq!((&node.node, node.map), (&earlier.node, earlier.map));
    }
    let last = all.nodes.last().unwrap();
    assert_eq!(last.map, Some(SEEDS_REPO_MAP));
    assert_eq!(last.node.to_string(), id_of(&loaded, "read_frame"));
    let distinct: HashSet<&NodeId> = all.nodes.iter().map(|seed| &seed.node).collect();
    assert_eq!(distinct.len(), all.nodes.len(), "no duplicates");

    // Without bm25 in the call the callers are the PageRank's to add.
    let no_bm25 = seeds_in(&loaded, Some(&hybrid), QUESTION, &with(&[SEEDS_REPO_MAP])).await;
    assert_eq!(no_bm25.source, "hybrid+repo_map");
    assert_eq!(no_bm25.nodes[0].map, Some(SEEDS_HYBRID));
    assert_eq!(
        no_bm25.nodes[1].node.to_string(),
        id_of(&loaded, "read_frame")
    );
    assert_eq!(no_bm25.nodes.len(), 1 + 4);

    // A node the hybrid search gave is not added again, and the source stays.
    let hub = Canned {
        answer: vec![hit((4, 4))],
        calls: AtomicUsize::new(0),
    };
    let hub_given = seeds_in(&loaded, Some(&hub), "parse alpha", &with(&[SEEDS_REPO_MAP])).await;
    let hub_id = id_of(&loaded, "read_frame");
    assert_eq!(
        hub_given
            .nodes
            .iter()
            .filter(|seed| seed.node.to_string() == hub_id)
            .count(),
        1
    );
    assert_eq!(hub_given.nodes[0].map, Some(SEEDS_HYBRID));

    // The hybrid search found nothing or failed: bm25 then repo_map are the
    // start nodes, the reason stays visible, and the stand-in does not run.
    let failing = NoSearch("the embedder is down".into());
    let down = seeds_in(
        &loaded,
        Some(&failing),
        QUESTION,
        &with(&[SEEDS_BM25, SEEDS_REPO_MAP]),
    )
    .await;
    assert_eq!(down.source, "bm25+repo_map");
    assert_eq!(down.nodes.len(), 4);
    assert!(
        down.fallback
            .as_deref()
            .unwrap()
            .contains("embedder is down")
    );
    // repo_map alone answered: its own source, and the reason says so.
    let alone = seeds_in(
        &loaded,
        Some(&failing),
        QUESTION,
        &[SEEDS_HYBRID, SEEDS_REPO_MAP],
    )
    .await;
    assert_eq!(alone.source, "repo_map");
    assert!(
        alone
            .fallback
            .as_deref()
            .unwrap()
            .contains("embedder is down")
    );
    assert!(
        alone
            .notes
            .iter()
            .any(|note| note.starts_with("seed_source is repo_map")),
        "{:?}",
        alone.notes
    );
    // Only the map wanted: the search is not asked at all.
    let calls = hybrid.calls.load(Ordering::SeqCst);
    let map_alone = seeds_in(
        &loaded,
        Some(&hybrid),
        QUESTION,
        &[SEEDS_REPO_MAP, SCIP_MAP_ID],
    )
    .await;
    assert_eq!(map_alone.source, "repo_map");
    assert!(map_alone.fallback.is_none() && map_alone.notes.is_empty());
    assert_eq!(hybrid.calls.load(Ordering::SeqCst), calls);
}

#[test]
fn the_source_forms_are_the_closed_list_in_source_order() {
    let forms = [
        ("", "repo_map", "repo_map"),
        ("hybrid", "repo_map", "hybrid+repo_map"),
        ("bm25", "repo_map", "bm25+repo_map"),
        ("hybrid+bm25", "repo_map", "hybrid+bm25+repo_map"),
        ("", "literal", "literal"),
        ("hybrid", "literal", "hybrid+literal"),
        ("bm25", "literal", "bm25+literal"),
        ("hybrid+bm25", "literal", "hybrid+bm25+literal"),
        ("repo_map", "literal", "repo_map+literal"),
        ("hybrid+repo_map", "literal", "hybrid+repo_map+literal"),
        ("bm25+repo_map", "literal", "bm25+repo_map+literal"),
        (
            "hybrid+bm25+repo_map",
            "literal",
            "hybrid+bm25+repo_map+literal",
        ),
    ];
    for (current, part, joined) in forms {
        assert_eq!(join_source(current, part), joined, "{current} + {part}");
    }
}

#[tokio::test]
async fn an_empty_graph_or_no_bm25_match_gives_no_seeds_and_a_note() {
    // No graph (`scip.mode = off`): no seeds, a note that says why.
    let empty = Loaded::empty(ScipMode::Off, "graph.scip: empty".to_owned());
    let found = seeds_in(&empty, None, QUESTION, &[SEEDS_MAP, SEEDS_REPO_MAP]).await;
    assert!(found.nodes.is_empty(), "{:?}", found.details);
    assert_ne!(found.source, "repo_map");
    assert!(
        found
            .notes
            .iter()
            .any(|note| note.starts_with("seeds.repo_map: the SCIP graph is empty")),
        "{:?}",
        found.notes
    );

    // A graph, and a question none of whose words any entity holds: the
    // personalisation is empty, and a global hub is not an answer.
    let tree = tree();
    let loaded = loaded_tree(&tree).await;
    let none = seeds_in(&loaded, None, "zebra", &[SEEDS_REPO_MAP]).await;
    assert!(none.nodes.is_empty() && none.details.is_empty());
    assert_eq!(none.source, "none");
    assert!(
        none.notes
            .iter()
            .any(|note| note.starts_with("seeds.repo_map: no entity matches")),
        "{:?}",
        none.notes
    );
    // The stand-in still answers when it is wanted and the map gave nothing.
    let stand_in = seeds_in(&loaded, None, "zebra", &[SEEDS_MAP, SEEDS_REPO_MAP]).await;
    assert_eq!(stand_in.source, "name_tokens");
    assert!(stand_in.nodes.is_empty());

    // An empty question runs nothing and says nothing.
    let blank = seeds_in(&loaded, None, "  ", &[SEEDS_REPO_MAP]).await;
    assert!(blank.nodes.is_empty() && blank.notes.is_empty());
}

/// One call over the tree under `[run_profile.seeds] repo_map` as given.
async fn repo_map_run(
    tree: &(tempfile::TempDir, PathBuf),
    repo_map: bool,
    maps: Option<&[&str]>,
    root: bool,
) -> Value {
    let data = tempfile::tempdir().unwrap();
    let profile = RunProfileConfig {
        seeds: lci_core::config::RunProfileSeeds {
            repo_map,
            ..Default::default()
        },
        ..Default::default()
    };
    let maps: Option<Vec<String>> = maps.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect());
    let (budget, steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: &tree.1,
        scip_root: root.then(|| tree.0.path()),
        question: QUESTION,
        seeds: &[],
        seed_search: None,
        top_k: 8,
        maps: maps.as_deref(),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    run(&limits(), &profile, &Dirs::under(data.path()), &call)
        .await
        .unwrap()
        .response
}

/// Whether the call's policy lists `seeds.repo_map` as a running map.
fn repo_map_listed(response: &Value) -> bool {
    response["effective_policy"]["maps"]
        .as_array()
        .unwrap_or_else(|| panic!("no maps in {response}"))
        .iter()
        .any(|map| map["id"] == SEEDS_REPO_MAP && map["enabled"] == true)
}

#[tokio::test]
async fn naming_seeds_repo_map_with_the_key_off_gives_the_note() {
    assert!(OPT_IN_MAPS.contains(&SEEDS_REPO_MAP) && !MAPS.contains(&SEEDS_REPO_MAP));
    assert!(
        !lci_core::config::Config::default()
            .run_profile
            .seeds
            .repo_map,
        "off by default"
    );
    let tree = tree();
    // Key off, the map named: the note, and the answer is the one without it.
    let named = Some(&["seeds.repo_map", "graph.scip"][..]);
    let mut off = repo_map_run(&tree, false, named, true).await;
    let mut without = repo_map_run(&tree, false, Some(&["graph.scip"]), true).await;
    let notes = off["notes"].to_string();
    assert!(
        notes.contains("seeds.repo_map is off ([run_profile.seeds] repo_map = false)"),
        "{notes}"
    );
    assert!(!repo_map_listed(&off), "{off}");
    assert_eq!(off["seeds"], json!([]));
    assert_eq!(off["seed_source"], without["seed_source"]);
    for answer in [&mut off, &mut without] {
        answer.as_object_mut().unwrap().remove("notes");
        if let Some(totals) = answer.get_mut("totals").and_then(Value::as_object_mut) {
            totals.remove("elapsed_ms");
        }
    }
    assert_eq!(off, without);
    // Key off and no maps named: not in the default set, no note.
    let plain = repo_map_run(&tree, false, None, true).await;
    assert!(!repo_map_listed(&plain), "{plain}");
    assert!(!plain.to_string().contains("repo_map"), "{plain}");
}

#[tokio::test]
async fn seeds_repo_map_with_the_key_on_is_in_the_default_set_and_needs_no_root() {
    let tree = tree();
    let loaded = loaded_tree(&tree).await;
    // Key on, no maps named: the map is in the default set and seeds the hub
    // (no search here, so it is the only source that answers).
    let on = repo_map_run(&tree, true, None, true).await;
    assert!(repo_map_listed(&on), "{on}");
    assert!(!on.to_string().contains("is off"), "{on}");
    assert_eq!(on["seed_source"], "repo_map");
    assert_eq!(on["seeds"][0], id_of(&loaded, "read_frame"));
    assert_eq!(on["seed_hits"][0]["how"], "repo_map");
    // Named, it runs, and it needs no tree root.
    let named = Some(&["seeds.repo_map", "graph.scip"][..]);
    for root in [true, false] {
        let named_on = repo_map_run(&tree, true, named, root).await;
        assert!(repo_map_listed(&named_on), "{named_on}");
        assert_eq!(named_on["seed_source"], "repo_map", "root {root}");
        assert_eq!(named_on["seeds"], on["seeds"], "root {root}");
    }
    // Key on does not turn it on for a call that names other maps only.
    let other = repo_map_run(&tree, true, Some(&["graph.scip"]), true).await;
    assert!(!repo_map_listed(&other), "{other}");
}
