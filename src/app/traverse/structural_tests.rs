//! Tests of the `grep.structural` wiring in the traversal entry point: the
//! key `[run_profile.grep] structural`, the note when it is off, and the
//! default set when it is on.

use super::maps::ripgrep_runs;
use super::*;

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

/// A tree with an empty SCIP index and three given file seeds.
struct Tree {
    dir: tempfile::TempDir,
    index: PathBuf,
    seeds: Vec<String>,
}

impl Tree {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let index = dir.path().join("index.scip");
        std::fs::write(&index, []).unwrap();
        let seeds = ["src/a.rs", "src/b.rs", "src/c.rs"]
            .map(|path| NodeId::file(path).unwrap().to_string())
            .to_vec();
        Self { dir, index, seeds }
    }
}

/// One pops-limited call over the tree under `[run_profile.grep] structural` as given.
async fn structural_run(tree: &Tree, structural: bool, maps: Option<&[&str]>) -> Value {
    let maps: Option<Vec<String>> = maps.map(|ids| ids.iter().map(|id| (*id).to_owned()).collect());
    let (budget, steer) = (
        BudgetArgs {
            pops: Some(1),
            ..BudgetArgs::default()
        },
        SteerArgs::default(),
    );
    let call = Call {
        scip_index: &tree.index,
        scip_root: Some(tree.dir.path()),
        question: "q",
        seeds: &tree.seeds,
        seed_search: None,
        top_k: 8,
        maps: maps.as_deref(),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
        lsp_sessions: None,
    };
    let profile = RunProfileConfig {
        grep: lci_core::config::profile::RunProfileGrep { structural },
        ..RunProfileConfig::default()
    };
    run(&limits(), &profile, &Dirs::under(tree.dir.path()), &call)
        .await
        .unwrap()
        .response
}

/// Whether the call's policy lists `grep.structural` as a running map.
fn structural_listed(response: &Value) -> bool {
    response["effective_policy"]["maps"]
        .as_array()
        .unwrap_or_else(|| panic!("no maps in {response}"))
        .iter()
        .any(|map| map["id"] == STRUCTURAL_ID && map["enabled"] == true)
}

#[tokio::test]
async fn naming_grep_structural_with_the_key_off_gives_the_note_and_no_edges() {
    assert!(OPT_IN_MAPS.contains(&STRUCTURAL_ID) && !MAPS.contains(&STRUCTURAL_ID));
    assert!(
        !lci_core::config::Config::default()
            .run_profile
            .grep
            .structural,
        "off by default"
    );
    let tree = Tree::new();
    // Key off, the map named: the note, and the answer is the one without it
    // (nothing registers; ripgrep is not started for it).
    let mut off = structural_run(&tree, false, Some(&["graph.scip", STRUCTURAL_ID])).await;
    let mut without = structural_run(&tree, false, Some(&["graph.scip"])).await;
    let notes = off["notes"].to_string();
    assert!(
        notes.contains("grep.structural is off ([run_profile.grep] structural = false)"),
        "{notes}"
    );
    assert!(!structural_listed(&off), "{off}");
    // Everything but the note and the wall time is the same.
    for answer in [&mut off, &mut without] {
        answer.as_object_mut().unwrap().remove("notes");
        answer["totals"]
            .as_object_mut()
            .unwrap()
            .remove("elapsed_ms");
    }
    assert_eq!(off, without);
    // Key off and no maps named: not in the default set, no note.
    let plain = structural_run(&tree, false, None).await;
    assert!(!structural_listed(&plain), "{plain}");
    assert!(!plain.to_string().contains("grep.structural"), "{plain}");
}

#[tokio::test]
async fn grep_structural_with_the_key_on_is_in_the_default_set_and_runs_when_named() {
    let ripgrep = lci_core::lexical::resolve_ripgrep_path("rg");
    if !ripgrep_runs(&ripgrep).await {
        eprintln!("skipped: ripgrep is not installed");
        return;
    }
    let tree = Tree::new();
    // Key on, no maps named, a tree root: the map joins the default set.
    let plain = structural_run(&tree, true, None).await;
    assert!(structural_listed(&plain), "{plain}");
    assert!(!plain.to_string().contains("is off"), "{plain}");
    // Named, it runs; the other maps are as they were.
    let named = structural_run(&tree, true, Some(&["graph.scip", STRUCTURAL_ID])).await;
    assert!(structural_listed(&named), "{named}");
    // Key on does not turn it on for a call that names other maps only.
    let other = structural_run(&tree, true, Some(&["graph.scip"])).await;
    assert!(!structural_listed(&other), "{other}");
}
