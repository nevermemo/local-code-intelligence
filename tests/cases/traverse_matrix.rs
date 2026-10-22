//! The traverse output matrix (K39a, commit 0): one `traverse::run` call per
//! cell on the tree and SCIP index of `freshness.rs`, each cell's response, trace
//! and handle file compared with a golden under `tests/golden/traverse-matrix/`.
//! It pins what a refactor of the map selection and registration of
//! `src/app/traverse/mod.rs` must not change: which maps a call wants and the
//! notes that say so, the order the maps register in, the errors a bad call
//! gets (with the map lists they print), and the walk each map set produces.
//!
//! Normalised before the comparison, and only these: the handles (`handle`,
//! `checkpoint.handle`, the `resume` event's `handle`: hashes of a snapshot
//! that carries timings and temp paths), `trace.file`, the handle file's
//! `identity`, the timings (every `elapsed_ms` and `ms`, and the build time in
//! the `config.key` note), the "expand p50"
//! notes (a measured cost), git's error text in the `exact.pickaxe` note (it
//! differs between git versions), `trace.hash` of a resumed response (its trace
//! names the handle it resumed; its lines are pinned instead) and the temp
//! root (`<tmp>`). Everything else is exact. The actual text of every cell
//! goes to `test-results/traverse-matrix/` for diffing; set `BLESS_GOLDEN=1`
//! to rewrite the goldens (only the commit that changes an output may do so,
//! and its message names why).
//!
//! No model service and no `App`: `seed_search` is `None`, so the start nodes
//! come from explicit seeds or from the stand-in that matches symbol names.
//! `lsp_ready` uses the documents fake language server; `lsp_down` uses
//! binaries that do not exist (every other server's path is missing too, so
//! the cells do not depend on what is installed).
use super::freshness::{COMBINE, RUN, SPEAK, write_scip, write_tree};
use super::*;
use local_code_intelligence::{
    app::traverse::{self, BudgetArgs, Call, Dirs, SteerArgs},
    config::{RunProfileConfig, TraverseConfig},
};
use std::path::PathBuf;

/// One cell's world: the tree, the SCIP file outside it, the data directory.
struct Cell {
    temp: tempfile::TempDir,
    workspace: PathBuf,
    scip: PathBuf,
}

fn cell() -> Cell {
    let temp = tempfile::tempdir().unwrap();
    let tree = temp.path().join("tree");
    write_tree(&tree, 0, false);
    let workspace = dunce::canonicalize(&tree).unwrap();
    let scip = temp.path().join("scip/index.scip");
    // The tree is written before the SCIP file, never after (see
    // `freshness.rs`).
    write_scip(&workspace, &scip);
    Cell {
        temp,
        workspace,
        scip,
    }
}

impl Cell {
    fn dirs(&self, trajectory_candidates: usize) -> Dirs {
        Dirs {
            trajectory_candidates,
            ..Dirs::under(&self.temp.path().join("data"))
        }
    }

    /// The strings the temp root can be spelled as in an answer.
    fn roots(&self) -> Vec<String> {
        let mut roots = vec![
            dunce::canonicalize(self.temp.path())
                .unwrap()
                .to_string_lossy()
                .into_owned(),
            self.temp.path().to_string_lossy().into_owned(),
        ];
        roots.sort_by_key(|root| std::cmp::Reverse(root.len()));
        roots.dedup();
        roots
    }
}

/// What a cell asks. `rootless` leaves the tree out of the call.
#[derive(Default)]
struct Spec {
    question: &'static str,
    seeds: Vec<String>,
    maps: Option<Vec<String>>,
    pops: Option<u32>,
    policy: Option<&'static str>,
    handle: Option<String>,
    rootless: bool,
    lsp: Option<Config>,
    lsp_sessions: Option<std::sync::Arc<lci_core::lsp::Manager>>,
    trajectory: usize,
}

fn maps(ids: &[&str]) -> Option<Vec<String>> {
    Some(ids.iter().map(|id| (*id).to_owned()).collect())
}

fn seeds(nodes: &[&str]) -> Vec<String> {
    nodes.iter().map(|node| (*node).to_owned()).collect()
}

fn limits() -> TraverseConfig {
    TraverseConfig {
        enabled: true,
        exact_seeds: false,
        deadline_ms: 60_000,
        ..Default::default()
    }
}

/// The run profile every cell starts from: the opt-in seed maps and
/// `grep.structural` off whatever the shipped default is, so a cell that
/// wants one switches it on.
fn profile() -> RunProfileConfig {
    let mut profile = RunProfileConfig::default();
    profile.seeds.bm25 = false;
    profile.seeds.literal = false;
    profile.seeds.repo_map = false;
    profile.grep.structural = false;
    profile
}

async fn ask(
    cell: &Cell,
    spec: &Spec,
    profile: &RunProfileConfig,
) -> anyhow::Result<traverse::Report> {
    let budget = BudgetArgs {
        pops: spec.pops,
        ..Default::default()
    };
    let steer = SteerArgs {
        policy: spec.policy.map(str::to_owned),
        ..Default::default()
    };
    let call = Call {
        scip_index: &cell.scip,
        scip_root: (!spec.rootless).then_some(cell.workspace.as_path()),
        question: spec.question,
        seeds: &spec.seeds,
        seed_search: None,
        top_k: 10,
        maps: spec.maps.as_deref(),
        budget: &budget,
        steer: &steer,
        handle: spec.handle.as_deref(),
        lsp: spec.lsp.as_ref(),
        lsp_sessions: spec.lsp_sessions.clone(),
    };
    traverse::run(&limits(), profile, &cell.dirs(spec.trajectory), &call).await
}

// ---- the normaliser ---------------------------------------------------------

/// Replaces the temp root in every string (and turns the backslashes of such a
/// path into slashes), and drops the notes that carry a measured cost.
fn scrub(value: &mut Value, roots: &[String]) {
    match value {
        Value::String(text) => {
            let mut changed = false;
            for root in roots {
                if text.contains(root.as_str()) {
                    *text = text.replace(root.as_str(), "<tmp>");
                    changed = true;
                }
            }
            if changed {
                *text = text.replace('\\', "/");
            }
            // `config.key: ... built in 1172 ms`: a measured time.
            if text.starts_with("config.key: ")
                && let Some(at) = text.find("built in ")
                && let Some(end) = text[at..].find(" ms")
            {
                text.replace_range(at + "built in ".len()..at + end, "<n>");
            }
            // `exact.pickaxe: ... git exited with ...)`: git's own error text,
            // which differs between git versions and platforms.
            if text.starts_with("exact.pickaxe: ")
                && let Some(at) = text.find("git exited with ")
                && text.ends_with(')')
            {
                let end = text.len() - 1;
                text.replace_range(at + "git exited with ".len()..end, "<git error>");
            }
        }
        Value::Array(items) => {
            items.retain(|item| !item.as_str().is_some_and(|t| t.contains("expand p50")));
            for item in items {
                scrub(item, roots);
            }
        }
        Value::Object(map) => {
            for field in map.values_mut() {
                scrub(field, roots);
            }
        }
        _ => {}
    }
}

/// Removes `path` (object keys) from `value` when it is there.
fn remove(value: &mut Value, path: &[&str]) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut at = value;
    for key in parents {
        match at.get_mut(*key) {
            Some(next) => at = next,
            None => return,
        }
    }
    if let Some(object) = at.as_object_mut() {
        object.remove(*last);
    }
}

/// The timings of a response or a handle file, wherever they sit.
fn drop_timings(value: &mut Value) {
    match value {
        Value::Object(map) => {
            map.remove("elapsed_ms");
            map.remove("ms");
            for field in map.values_mut() {
                drop_timings(field);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(drop_timings),
        _ => {}
    }
}

fn pretty(value: &Value) -> String {
    serde_json::to_string_pretty(value).unwrap() + "\n"
}

/// The response as pinned.
fn response_text(cell: &Cell, report: &traverse::Report) -> String {
    let mut value = report.response.clone();
    remove(&mut value, &["handle"]);
    remove(&mut value, &["checkpoint", "handle"]);
    remove(&mut value, &["trace", "file"]);
    // A resumed trace names the handle it resumed (a hash of a snapshot with
    // timings and temp paths), so its own hash moves with it; its lines are
    // pinned instead.
    if trace_text(cell, report).is_some_and(|trace| trace.contains("\"ev\":\"resume\"")) {
        remove(&mut value, &["trace", "hash"]);
    }
    drop_timings(&mut value);
    scrub(&mut value, &cell.roots());
    pretty(&value)
}

/// The trace file as pinned: one compact JSON object per line.
fn trace_text(cell: &Cell, report: &traverse::Report) -> Option<String> {
    let path = report.trace.as_ref()?;
    let text = std::fs::read_to_string(path).unwrap();
    let mut out = String::new();
    for line in text.lines().filter(|line| !line.is_empty()) {
        let mut event: Value = serde_json::from_str(line).unwrap();
        scrub(&mut event, &cell.roots());
        if event["ev"] == "resume" {
            event["handle"] = json!("<handle>");
        }
        out.push_str(&serde_json::to_string(&event).unwrap());
        out.push('\n');
    }
    Some(out)
}

/// The handle file as pinned (its identity is a hash of the temp paths).
fn handle_text(cell: &Cell, report: &traverse::Report) -> Option<String> {
    let handle = report.response["handle"].as_str()?;
    let path = cell.dirs(0).handles.join(format!("{handle}.json"));
    let mut value: Value = serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
    remove(&mut value, &["identity"]);
    drop_timings(&mut value);
    scrub(&mut value, &cell.roots());
    Some(pretty(&value))
}

// ---- the goldens ------------------------------------------------------------

fn manifest(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
}

/// Compares `actual` with `tests/golden/traverse-matrix/<name>.<suffix>`. An
/// artifact the call did not make (`None`) has no golden either.
fn check(name: &str, suffix: &str, actual: Option<String>) {
    let file = format!("{name}.{suffix}");
    let golden = manifest("tests/golden/traverse-matrix").join(&file);
    let bless = std::env::var_os("BLESS_GOLDEN").is_some();
    let Some(actual) = actual else {
        if bless {
            let _ = std::fs::remove_file(&golden);
        }
        assert!(
            !golden.exists(),
            "{file} is pinned, but the call made no such artifact"
        );
        return;
    };
    let out = manifest("test-results/traverse-matrix");
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join(&file), &actual).unwrap();
    if bless {
        std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
        std::fs::write(&golden, &actual).unwrap();
    }
    let expected = std::fs::read_to_string(&golden)
        .unwrap_or_else(|error| panic!("{}: {error}; run with BLESS_GOLDEN=1", golden.display()))
        .replace("\r\n", "\n");
    assert_eq!(actual, expected, "{file} changed");
}

/// Pins an answer: its response, its trace and its handle file.
fn pin(name: &str, cell: &Cell, report: &traverse::Report) {
    check(name, "json", Some(response_text(cell, report)));
    check(name, "trace.jsonl", trace_text(cell, report));
    check(name, "handle.json", handle_text(cell, report));
    check(name, "err.txt", None);
}

/// Pins an error: its text (the whole chain).
fn pin_error(name: &str, error: &anyhow::Error) {
    check(name, "json", None);
    check(name, "trace.jsonl", None);
    check(name, "handle.json", None);
    check(name, "err.txt", Some(format!("{error:#}\n")));
}

fn notes(report: &traverse::Report) -> String {
    report.response["notes"].to_string()
}

// ---- the cells --------------------------------------------------------------

/// A first call that stops at its budget: a handle and a checkpoint.
#[tokio::test]
async fn fresh() {
    let cell = cell();
    let spec = Spec {
        question: "Combine",
        seeds: seeds(&[COMBINE, RUN]),
        maps: maps(&["graph.scip", "structure.contains"]),
        pops: Some(2),
        ..Default::default()
    };
    let report = ask(&cell, &spec, &profile()).await.unwrap();
    assert!(report.response["handle"].is_string(), "{}", report.response);
    pin("fresh", &cell, &report);
}

/// The continuation of `fresh`'s handle.
#[tokio::test]
async fn resumed() {
    let cell = cell();
    let first = Spec {
        question: "Combine",
        seeds: seeds(&[COMBINE, RUN]),
        maps: maps(&["graph.scip", "structure.contains"]),
        pops: Some(2),
        ..Default::default()
    };
    let report = ask(&cell, &first, &profile()).await.unwrap();
    let handle = report.response["handle"].as_str().unwrap().to_owned();
    let second = Spec {
        handle: Some(handle),
        ..first
    };
    let report = ask(&cell, &second, &profile()).await.unwrap();
    pin("resumed", &cell, &report);
}

/// The record output: `fresh` with the `pop` events of the trajectory.
#[tokio::test]
async fn trajectory() {
    let cell = cell();
    let spec = Spec {
        question: "Combine",
        seeds: seeds(&[COMBINE, RUN]),
        maps: maps(&["graph.scip", "structure.contains"]),
        pops: Some(2),
        trajectory: lci_maps::traverse::TRAJECTORY_CANDIDATES,
        ..Default::default()
    };
    let report = ask(&cell, &spec, &profile()).await.unwrap();
    let trace = trace_text(&cell, &report).unwrap();
    assert!(trace.contains("\"ev\":\"pop\""), "{trace}");
    pin("trajectory", &cell, &report);
}

/// `seeds.bm25`, `seeds.literal`, `seeds.repo_map` and `grep.structural` named
/// while their keys are off: one note each, in this order, and none of them
/// registers (no ripgrep is needed).
#[tokio::test]
async fn optin_off() {
    let cell = cell();
    let spec = Spec {
        question: "Combine",
        maps: maps(&[
            "graph.scip",
            "seeds.bm25",
            "seeds.literal",
            "seeds.repo_map",
            "grep.structural",
        ]),
        pops: Some(2),
        ..Default::default()
    };
    let report = ask(&cell, &spec, &profile()).await.unwrap();
    let text = notes(&report);
    for map in [
        "seeds.bm25",
        "seeds.literal",
        "seeds.repo_map",
        "grep.structural",
    ] {
        assert!(text.contains(&format!("{map} is off")), "{text}");
    }
    pin("optin_off", &cell, &report);
}

/// `seeds.bm25` on, no maps named: it joins the default set and seeds.
#[tokio::test]
async fn optin_on() {
    let cell = cell();
    let spec = Spec {
        question: "Combine",
        pops: Some(2),
        ..Default::default()
    };
    let mut profile = profile();
    profile.seeds.bm25 = true;
    let report = ask(&cell, &spec, &profile).await.unwrap();
    let text = report.response.to_string();
    assert!(text.contains("seeds.bm25"), "{text}");
    pin("optin_on", &cell, &report);
}

/// `seeds.repo_map` on, no maps named: the same for the repo map.
#[tokio::test]
async fn optin_repo_map_on() {
    let cell = cell();
    let spec = Spec {
        question: "Combine",
        pops: Some(2),
        ..Default::default()
    };
    let mut profile = profile();
    profile.seeds.repo_map = true;
    let report = ask(&cell, &spec, &profile).await.unwrap();
    let text = report.response.to_string();
    assert!(text.contains("seeds.repo_map"), "{text}");
    pin("optin_repo_map_on", &cell, &report);
}

/// `grep.structural` named while its key is off: the note, nothing registered,
/// and no `rg` needed.
#[tokio::test]
async fn structural_named_off() {
    let cell = cell();
    let spec = Spec {
        question: "Combine",
        seeds: seeds(&[COMBINE]),
        maps: maps(&["graph.scip", "grep.structural"]),
        pops: Some(2),
        ..Default::default()
    };
    let report = ask(&cell, &spec, &profile()).await.unwrap();
    assert!(notes(&report).contains("grep.structural is off"));
    pin("structural_named_off", &cell, &report);
}

/// A map that reads the tree, with no tree: the error.
#[tokio::test]
async fn needs_root() {
    let cell = cell();
    let spec = Spec {
        question: "Combine",
        seeds: seeds(&[COMBINE]),
        maps: maps(&["scip.impact"]),
        rootless: true,
        ..Default::default()
    };
    let error = ask(&cell, &spec, &profile()).await.err().unwrap();
    pin_error("needs_root", &error);
}

/// An unknown map: the error lists `MAPS` and `OPT_IN_MAPS` in order, so a map
/// dropped from either list or moved in it fails here.
#[tokio::test]
async fn unknown_map() {
    let cell = cell();
    let spec = Spec {
        question: "Combine",
        seeds: seeds(&[COMBINE]),
        maps: maps(&["nope"]),
        ..Default::default()
    };
    let error = ask(&cell, &spec, &profile()).await.err().unwrap();
    let text = format!("{error:#}");
    for map in traverse::MAPS.iter().chain(&traverse::OPT_IN_MAPS) {
        assert!(text.contains(map), "{text}");
    }
    pin_error("unknown_map", &error);
}

/// A policy whose extra maps read the tree, with no tree: the skipped-maps
/// note and the path without a first stage.
#[tokio::test]
async fn policy_without_root() {
    let cell = cell();
    let spec = Spec {
        question: "Combine",
        policy: Some("impact"),
        rootless: true,
        ..Default::default()
    };
    let report = ask(&cell, &spec, &profile()).await.unwrap();
    assert!(
        notes(&report).contains("were left out"),
        "{}",
        notes(&report)
    );
    pin("policy_without_root", &cell, &report);
}

/// A trait the question names by a path (`shapes::Speak`; a one-segment name
/// matches nothing) joins the start nodes with `scip.implements` on: the
/// implements note comes before the trait-seed note.
#[tokio::test]
async fn trait_seed() {
    let cell = cell();
    let spec = Spec {
        question: "`shapes::Speak`",
        maps: maps(&["graph.scip", "scip.implements"]),
        pops: Some(2),
        ..Default::default()
    };
    let report = ask(&cell, &spec, &profile()).await.unwrap();
    let seeds = report.response["trait_seeds"].as_u64();
    assert!(seeds >= Some(1), "{}", report.response);
    let notes = report.response["notes"].as_array().unwrap();
    let position = |needle: &str| {
        notes
            .iter()
            .position(|note| note.as_str().is_some_and(|text| text.contains(needle)))
            .unwrap_or_else(|| panic!("no note with {needle:?}: {notes:?}"))
    };
    assert!(
        position("scip.implements: ") < position("start nodes are the traits"),
        "{notes:?}"
    );
    pin("trait_seed", &cell, &report);
}

/// Given start nodes that are traits (`scip.implements` on): they are the
/// `given_traits` the walk is told about, and no trait-seed note is added.
#[tokio::test]
async fn trait_seed_given() {
    let cell = cell();
    let spec = Spec {
        question: "`shapes::Speak`",
        seeds: seeds(&[SPEAK]),
        maps: maps(&["graph.scip", "scip.implements"]),
        pops: Some(2),
        ..Default::default()
    };
    let report = ask(&cell, &spec, &profile()).await.unwrap();
    assert_eq!(report.response["trait_seeds"], 0, "{}", report.response);
    assert!(
        !notes(&report).contains("start nodes are the traits"),
        "{}",
        notes(&report)
    );
    pin("trait_seed_given", &cell, &report);
}

/// Six maps that read the tree, the SCIP file and the TOML (no ripgrep, no
/// model): the registration order in `effective_policy.maps` and the notes
/// each one adds, in call order.
#[tokio::test]
async fn registration_order() {
    let cell = cell();
    // An empty repository of its own: `exact.pickaxe` asks git, which must not
    // find a repository around the temp directory instead.
    let init = std::process::Command::new("git")
        .args(["init", "-q", "--initial-branch=matrix"])
        .current_dir(&cell.workspace)
        .output()
        .unwrap();
    assert!(init.status.success(), "{init:?}");
    let spec = Spec {
        question: "`Combine`",
        seeds: seeds(&[COMBINE]),
        maps: maps(&[
            "graph.scip",
            "config.key",
            "scip.impact",
            "tests.tested_by",
            "exact.closure",
            "exact.pickaxe",
        ]),
        pops: Some(2),
        ..Default::default()
    };
    let report = ask(&cell, &spec, &profile()).await.unwrap();
    pin("registration_order", &cell, &report);
}

/// A named policy and a root: the policy's extra maps join the default set.
#[tokio::test]
async fn policy_with_root() {
    let cell = cell();
    let spec = Spec {
        question: "`Combine`",
        seeds: seeds(&[COMBINE]),
        policy: Some("impact"),
        ..Default::default()
    };
    let report = ask(&cell, &spec, &profile()).await.unwrap();
    pin("policy_with_root", &cell, &report);
}

/// The settings of the `lsp.live` cells: no other language server than the one
/// a cell names can start, whatever is installed on the machine (the Rust
/// server is always enabled, its path a command looked up on `PATH`).
fn lsp_base() -> Config {
    Config {
        rust_analyzer_path: "definitely-missing-rust-analyzer".into(),
        readiness_timeout_seconds: LOADED_BUDGET_SECONDS,
        ..Config::default()
    }
}

/// `lsp.live` with no language-server binary at all: unavailable, and the
/// answer is not labelled complete.
#[tokio::test]
async fn lsp_down() {
    let cell = cell();
    let mut config = lsp_base();
    config.csharp.path = Some("lci-no-such-language-server".to_owned());
    let spec = Spec {
        question: "Combine",
        seeds: seeds(&[COMBINE]),
        maps: maps(&["graph.scip", "lsp.live"]),
        pops: Some(2),
        lsp: Some(config),
        ..Default::default()
    };
    let mut profile = profile();
    profile.traverse.answer_labels = true;
    let report = ask(&cell, &spec, &profile).await.unwrap();
    assert!(
        notes(&report).contains("lsp.live is unavailable"),
        "{}",
        notes(&report)
    );
    pin("lsp_down", &cell, &report);
}

/// `lsp.live` in the default set: no maps named, a tree and a manager that
/// outlives the call (`Call.lsp_sessions`, as the service passes it) put the
/// map after the four default ones (`[run_profile.lsp] live`, K11b-3). The
/// language servers are missing, so the map is unavailable: its note, and the
/// answer is not labelled complete.
#[tokio::test]
async fn lsp_default() {
    let cell = cell();
    let config = lsp_base();
    let spec = Spec {
        question: "Combine",
        seeds: seeds(&[COMBINE]),
        pops: Some(2),
        lsp_sessions: Some(std::sync::Arc::new(lci_core::lsp::Manager::new(&config))),
        lsp: Some(config),
        ..Default::default()
    };
    let mut profile = profile();
    profile.traverse.answer_labels = true;
    let report = ask(&cell, &spec, &profile).await.unwrap();
    assert!(
        notes(&report).contains("lsp.live is unavailable"),
        "{}",
        notes(&report)
    );
    pin("lsp_default", &cell, &report);
}

/// `lsp.live` with the documents fake: the edges come from the server. The
/// cell fails loudly, instead of pinning a race, when the server was not ready
/// or could not start.
#[tokio::test]
async fn lsp_ready() {
    let cell = cell();
    let config = with_fake_documents_server(
        lsp_base(),
        &cell.temp.path().join("lsp-walk"),
        &[],
        LOADED_BUDGET_SECONDS,
    );
    let spec = Spec {
        question: "Combine",
        seeds: seeds(&[COMBINE]),
        maps: maps(&["graph.scip", "lsp.live"]),
        pops: Some(4),
        lsp: Some(config),
        ..Default::default()
    };
    let mut profile = profile();
    profile.traverse.answer_labels = true;
    let report = ask(&cell, &spec, &profile).await.unwrap();
    let text = notes(&report);
    assert!(!text.contains("when the walk ended"), "{text}");
    assert!(!text.contains("unavailable"), "{text}");
    pin("lsp_ready", &cell, &report);
}

/// Nothing to start from: the early answer.
#[tokio::test]
async fn no_seeds() {
    let cell = cell();
    let spec = Spec {
        question: "zzz-nothing-here",
        ..Default::default()
    };
    let report = ask(&cell, &spec, &profile()).await.unwrap();
    assert!(report.trace.is_none());
    pin("no_seeds", &cell, &report);
}
