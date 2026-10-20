//! `probe.compile` on fixture cargo output (the patch is real, the cargo
//! answer is canned) and, ignored, on real cargo.

use super::*;
use crate::{
    SnapshotRef,
    kit_c3::check_edge_invariants,
    scip_graph::{RawDocument, RawOccurrence, ScipGraph},
    scratch::Scratch,
};
use serde_json::json;
use std::sync::atomic::{AtomicUsize, Ordering};

const LIB: &str = "pub fn helper(x: u32) -> u32 { x }\n\
\n\
pub fn caller() -> u32 { helper(1) }\n\
\n\
pub fn bystander() {}\n\
pub struct S;\n\
impl S {\n\
\x20   pub fn method(&self, y: u8) -> u8 { y }\n\
\x20   pub fn use_it(&self) -> u8 { self.method(2) }\n\
}\n";

const MANIFEST: &str = "[package]\nname = \"demo\"\nversion = \"0.1.0\"\nedition = \"2021\"\n";

fn sym(path: &str) -> String {
    format!("rust-analyzer cargo demo 0.1.0 {path}")
}

fn definition(line: u32, col: u32, symbol: &str, end_line: u32) -> RawOccurrence {
    RawOccurrence {
        range: (line, col, line, col + 4),
        symbol: symbol.to_string(),
        roles: 1,
        enclosing: Some((line, 0, end_line, 1)),
    }
}

/// The SCIP graph of [`LIB`]: five functions and two methods.
fn scip() -> Arc<ScipMap> {
    let docs = vec![RawDocument {
        path: "src/lib.rs".into(),
        occurrences: vec![
            definition(0, 7, &sym("lib/helper()."), 0),
            definition(2, 7, &sym("lib/caller()."), 2),
            definition(4, 7, &sym("lib/bystander()."), 4),
            definition(7, 11, &sym("lib/impl#[S]method()."), 7),
            definition(8, 11, &sym("lib/impl#[S]use_it()."), 8),
        ],
    }];
    Arc::new(ScipMap::with_default_ids(ScipGraph::build(&docs, None)))
}

fn node_named(scip: &ScipMap, name: &str) -> NodeId {
    let graph = scip.graph();
    let i = (0..graph.nodes.len())
        .find(|&i| graph.nodes[i].name.ends_with(name))
        .unwrap_or_else(|| panic!("no node {name}"));
    scip.node_id(i).clone()
}

/// A cargo `compiler-message` error at `file:line:col` (1-based).
fn error_at(dir: &Path, file: &str, line: u32, col: u32) -> String {
    let path = dir.join(file).to_string_lossy().replace('\\', "/");
    json!({"reason": "compiler-message", "target": {"name": "demo", "kind": ["lib"]},
        "message": {"level": "error", "message": "cannot find function",
        "code": {"code": "E0425"},
        "spans": [{"file_name": path, "line_start": line, "column_start": col,
                   "is_primary": true, "expansion": null}]}})
    .to_string()
}

/// Canned cargo: what it prints is a function of the scratch directory, and
/// it records the content of `src/lib.rs` as it found it.
type Lines = Box<dyn Fn(&Path) -> Vec<String> + Send + Sync>;

struct Canned {
    lines: Lines,
    saw: Mutex<Vec<String>>,
    calls: AtomicUsize,
    timed_out: bool,
}

impl Canned {
    fn new(lines: impl Fn(&Path) -> Vec<String> + Send + Sync + 'static) -> Arc<Self> {
        Arc::new(Self {
            lines: Box::new(lines),
            saw: Mutex::new(Vec::new()),
            calls: AtomicUsize::new(0),
            timed_out: false,
        })
    }
}

struct Handle(Arc<Canned>);

impl CargoRunner for Handle {
    fn check(&self, dir: &Path, _target: &Path, _timeout: Duration) -> CargoOutput {
        self.0.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.0.saw).push(std::fs::read_to_string(dir.join("src/lib.rs")).unwrap());
        let mut lines = (self.0.lines)(dir);
        lines.push(r#"{"reason":"build-finished","success":false}"#.into());
        CargoOutput {
            stdout: lines.join("\n"),
            stderr: String::new(),
            timed_out: self.0.timed_out,
            success: Some(false),
        }
    }
}

struct Fixture {
    _base: Scratch,
    real: PathBuf,
    config: ProbeConfig,
}

impl Fixture {
    fn new() -> Self {
        let base = Scratch::new("probe-map").unwrap();
        let real = base.path().join("real");
        std::fs::create_dir_all(real.join("src")).unwrap();
        std::fs::write(real.join("Cargo.toml"), MANIFEST).unwrap();
        std::fs::write(real.join("src/lib.rs"), LIB).unwrap();
        let config = ProbeConfig {
            root: real.clone(),
            scratch: base.path().join("probe/tree"),
            target_dir: base.path().join("probe/target"),
            kinds: vec![PatchKind::Rename],
            timeout: Duration::from_secs(30),
        };
        Self {
            _base: base,
            real,
            config,
        }
    }

    fn map(&self, canned: &Arc<Canned>) -> ProbeMap {
        ProbeMap::with_cargo(
            scip(),
            self.config.clone(),
            Box::new(Handle(canned.clone())),
        )
    }

    fn real_lib(&self) -> String {
        std::fs::read_to_string(self.real.join("src/lib.rs")).unwrap()
    }

    fn scratch_lib(&self) -> String {
        std::fs::read_to_string(self.config.scratch.join("src/lib.rs")).unwrap()
    }
}

const WAIT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------- patches

#[test]
fn rename_changes_only_the_name_at_the_definition() {
    let out = make_patch(PatchKind::Rename, LIB, 0, 7, "helper").unwrap();
    assert!(out.starts_with("pub fn helper_lci_probe(x: u32)"));
    assert!(out.contains("{ helper(1) }"), "the call is left to fail");
    assert_eq!(out.len(), LIB.len() + RENAME_SUFFIX.len());
}

#[test]
fn a_patch_refuses_a_position_that_does_not_hold_the_name() {
    let error = make_patch(PatchKind::Rename, LIB, 0, 8, "helper").unwrap_err();
    assert!(error.contains("not at 1:9"), "{error}");
    assert!(make_patch(PatchKind::Rename, LIB, 99, 0, "helper").is_err());
}

#[test]
fn signature_adds_a_parameter_in_front_of_the_others() {
    let out = make_patch(PatchKind::Signature, LIB, 0, 7, "helper").unwrap();
    assert!(
        out.starts_with("pub fn helper(__lci_probe: u8, x: u32) -> u32"),
        "{out}"
    );
}

#[test]
fn signature_goes_after_self_and_into_an_empty_list() {
    let method = make_patch(PatchKind::Signature, LIB, 7, 11, "method").unwrap();
    assert!(
        method.contains("pub fn method(&self, __lci_probe: u8, y: u8)"),
        "{method}"
    );
    let empty = make_patch(PatchKind::Signature, LIB, 4, 7, "bystander").unwrap();
    assert!(
        empty.contains("pub fn bystander(__lci_probe: u8) {}"),
        "{empty}"
    );
    for (header, want) in [
        ("fn f(&mut self) {}", "fn f(&mut self, __lci_probe: u8) {}"),
        (
            "fn f(self: Box<Self>) {}",
            "fn f(self: Box<Self>, __lci_probe: u8) {}",
        ),
        (
            "fn f(mut self, a: u8) {}",
            "fn f(mut self, __lci_probe: u8, a: u8) {}",
        ),
        (
            "fn f<'a>(&'a self) {}",
            "fn f<'a>(&'a self, __lci_probe: u8) {}",
        ),
        (
            "fn f<T: Fn() -> u8>(g: T) {}",
            "fn f<T: Fn() -> u8>(__lci_probe: u8, g: T) {}",
        ),
        (
            "fn f(\n    a: u8,\n) {}",
            "fn f(__lci_probe: u8, \n    a: u8,\n) {}",
        ),
        (
            "fn f(selfish: u8) {}",
            "fn f(__lci_probe: u8, selfish: u8) {}",
        ),
    ] {
        let got = make_patch(PatchKind::Signature, header, 0, 3, "f").unwrap();
        assert_eq!(got, want);
    }
    assert!(make_patch(PatchKind::Signature, "const f: u8 = 1;", 0, 6, "f").is_err());
}

// ---------------------------------------------------------------- the map

#[test]
fn an_error_site_in_a_unit_becomes_an_edge_to_that_unit() {
    let fixture = Fixture::new();
    let canned = Canned::new(|dir| vec![error_at(dir, "src/lib.rs", 3, 27)]);
    let map = fixture.map(&canned);
    let helper = node_named(&map.inner.scip, "helper");
    let found = map.inner.edges_for(&helper, WAIT).0;
    let (edges, cost) = (found.edges, found.cost);
    assert_eq!(edges.len(), 1, "{edges:?}");
    let edge = &edges[0];
    assert_eq!(edge.src, helper);
    assert_eq!(edge.dst, node_named(&map.inner.scip, "caller"));
    assert_eq!(edge.kind, EdgeKind::CompileErrorAt);
    assert_eq!(edge.witness, WitnessClass::Compiler);
    assert_eq!(edge.map, PROBE_COMPILE_ID);
    assert_eq!(edge.conf, 100);
    // 0-based, as the SCIP map's sites.
    assert_eq!(
        edge.site,
        Some(Site {
            path: "src/lib.rs".into(),
            blob: String::new(),
            line: 2,
            col: 26
        })
    );
    assert_eq!(cost.external_calls, 1);
    assert!(cost.elapsed_ms > 0.0);
    assert_eq!(
        check_edge_invariants(&helper, map.describe(), &edges, EXPAND_LIMIT),
        Ok(())
    );
    assert!(map.last_error().is_none());
}

#[test]
fn cargo_sees_the_patch_and_the_real_tree_and_the_scratch_are_left_unchanged() {
    let fixture = Fixture::new();
    let canned = Canned::new(|dir| vec![error_at(dir, "src/lib.rs", 3, 27)]);
    let map = fixture.map(&canned);
    let helper = node_named(&map.inner.scip, "helper");
    map.edges_blocking(&helper, WAIT);
    let saw = lock(&canned.saw).clone();
    assert_eq!(saw.len(), 1);
    assert!(
        saw[0].starts_with("pub fn helper_lci_probe(x: u32)"),
        "{}",
        saw[0]
    );
    // The real tree was never written; the scratch has the original back.
    assert_eq!(fixture.real_lib(), LIB);
    assert_eq!(fixture.scratch_lib(), LIB);
    assert!(!fixture.real.join("target").exists());
    // And so after a second call.
    map.edges_blocking(&helper, WAIT);
    assert_eq!(fixture.real_lib(), LIB);
    assert_eq!(fixture.scratch_lib(), LIB);
}

#[test]
fn a_site_outside_every_unit_goes_to_its_file_and_one_inside_the_patched_unit_is_dropped() {
    let fixture = Fixture::new();
    let canned = Canned::new(|dir| {
        vec![
            error_at(dir, "src/lib.rs", 6, 1), // `pub struct S;`, in no function
            error_at(dir, "src/lib.rs", 1, 30), // inside `helper` itself
            error_at(dir, "src/lib.rs", 9, 40), // inside `use_it`, the innermost
            error_at(dir, "src/lib.rs", 9, 41), // the same unit again
            error_at(dir, "../registry/dep.rs", 1, 1), // outside the workspace
        ]
    });
    let map = fixture.map(&canned);
    let helper = node_named(&map.inner.scip, "helper");
    let edges = map.edges_blocking(&helper, WAIT);
    let by_dst: Vec<(String, u8)> = edges.iter().map(|e| (e.dst.to_string(), e.conf)).collect();
    let use_it = node_named(&map.inner.scip, "use_it").to_string();
    assert_eq!(
        by_dst,
        [(use_it, 100), ("file:src/lib.rs".to_string(), 60)],
        "{edges:?}"
    );
    assert_eq!(map.runs()[0].sites, 4);
}

#[test]
fn a_timed_out_cargo_returns_what_it_printed_and_says_so() {
    let fixture = Fixture::new();
    let canned = Arc::new(Canned {
        lines: Box::new(|dir| vec![error_at(dir, "src/lib.rs", 3, 27)]),
        saw: Mutex::new(Vec::new()),
        calls: AtomicUsize::new(0),
        timed_out: true,
    });
    let map = fixture.map(&canned);
    let helper = node_named(&map.inner.scip, "helper");
    let edges = map.edges_blocking(&helper, WAIT);
    assert_eq!(edges.len(), 1);
    assert!(map.last_error().unwrap().contains("timed out"));
    assert!(map.runs()[0].timed_out);
    assert_eq!(fixture.scratch_lib(), LIB);
}

/// A cargo that stops inside `check` until the test lets it go, and says
/// when it got there.
struct Parked {
    entered: Mutex<std::sync::mpsc::Sender<()>>,
    release: Mutex<std::sync::mpsc::Receiver<()>>,
    calls: AtomicUsize,
}

impl CargoRunner for Parked {
    fn check(&self, _dir: &Path, _target: &Path, _timeout: Duration) -> CargoOutput {
        self.calls.fetch_add(1, Ordering::SeqCst);
        lock(&self.entered).send(()).unwrap();
        // Returns at once when the test has dropped its sender.
        let _ = lock(&self.release).recv();
        CargoOutput {
            stdout: r#"{"reason":"build-finished","success":true}"#.into(),
            stderr: String::new(),
            timed_out: false,
            success: Some(true),
        }
    }
}

struct SharedParked(Arc<Parked>);

impl CargoRunner for SharedParked {
    fn check(&self, dir: &Path, target: &Path, timeout: Duration) -> CargoOutput {
        self.0.check(dir, target, timeout)
    }
}

#[test]
fn a_probe_that_cannot_get_the_gate_in_its_budget_gives_up_instead_of_parking_a_thread() {
    let fixture = Fixture::new();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
    let parked = Arc::new(Parked {
        entered: Mutex::new(entered_tx),
        release: Mutex::new(release_rx),
        calls: AtomicUsize::new(0),
    });
    let map = Arc::new(ProbeMap::with_cargo(
        scip(),
        fixture.config.clone(),
        Box::new(SharedParked(parked.clone())),
    ));
    let helper = node_named(&map.inner.scip, "helper");

    // The first probe holds the gate inside its (parked) cargo run.
    let first = {
        let (map, helper) = (map.clone(), helper.clone());
        std::thread::spawn(move || map.edges_blocking(&helper, WAIT))
    };
    entered_rx
        .recv_timeout(WAIT)
        .expect("the first probe never reached cargo");

    // The second has a short budget. It must return when the budget ends,
    // not when the first probe's cargo run does.
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let second = {
        let (map, helper) = (map.clone(), helper.clone());
        std::thread::spawn(move || {
            let edges = map.edges_blocking(&helper, Duration::from_millis(100));
            done_tx.send(edges).unwrap();
        })
    };
    let returned = done_rx.recv_timeout(Duration::from_secs(10));
    let error = map.last_error();
    let second_calls = parked.calls.load(Ordering::SeqCst);

    // Let everything finish before asserting, so a failure cannot hang.
    drop(release_tx);
    first.join().unwrap();
    second.join().unwrap();

    let edges = returned.expect("the second probe stayed parked on the gate past its budget");
    assert!(edges.is_empty());
    assert_eq!(second_calls, 1, "the second probe must not have run cargo");
    assert!(
        error.is_some_and(|e| e.contains("another probe")),
        "the map must record the gate timeout as its last error"
    );
}

#[test]
fn nodes_the_graph_does_not_hold_and_other_kinds_have_no_edges() {
    let fixture = Fixture::new();
    let canned = Canned::new(|_| Vec::new());
    let map = fixture.map(&canned);
    assert!(
        map.edges_blocking(&NodeId::file("src/lib.rs").unwrap(), WAIT)
            .is_empty()
    );
    let unknown = NodeId::Symbol(lci_core::node::SymbolKey {
        language: "rust".into(),
        module_path: vec!["nope".into()],
        container: Vec::new(),
        name: "nope".into(),
        kind: lci_core::node::SymbolKind::Function,
        disambiguator: 0,
    });
    assert!(map.edges_blocking(&unknown, WAIT).is_empty());
    assert_eq!(canned.calls.load(Ordering::SeqCst), 0, "no cargo run");
    assert_eq!(map.describe().accepts, [NodeKind::Symbol]);
}

#[test]
fn a_diff_from_the_caller_is_applied_then_undone() {
    let fixture = Fixture::new();
    let canned = Canned::new(|dir| vec![error_at(dir, "src/lib.rs", 3, 27)]);
    let map = fixture.map(&canned);
    let helper = node_named(&map.inner.scip, "helper");
    let diff = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,1 @@\n\
-pub fn helper(x: u32) -> u32 { x }\n\
+pub fn helper(x: u32, extra: u32) -> u32 { x }\n";
    let edges = map.probe_diff(&helper, diff, WAIT);
    assert_eq!(edges.len(), 1);
    let saw = lock(&canned.saw).clone();
    assert!(
        saw[0].starts_with("pub fn helper(x: u32, extra: u32)"),
        "{}",
        saw[0]
    );
    assert_eq!(fixture.scratch_lib(), LIB);
    assert_eq!(fixture.real_lib(), LIB);
    assert_eq!(map.runs()[0].patch, "diff");
    // A diff that does not apply is a recorded failure, not a cargo run.
    let bad = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,1 +1,1 @@\n-nothing like it\n+x\n";
    assert!(map.probe_diff(&helper, bad, WAIT).is_empty());
    assert!(map.last_error().unwrap().contains("cannot apply"));
    assert_eq!(canned.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn every_configured_patch_is_one_cargo_run_and_the_best_edge_per_unit_wins() {
    let mut fixture = Fixture::new();
    fixture.config.kinds = vec![PatchKind::Rename, PatchKind::Signature];
    let canned = Canned::new(|dir| vec![error_at(dir, "src/lib.rs", 3, 27)]);
    let map = fixture.map(&canned);
    let helper = node_named(&map.inner.scip, "helper");
    let edges = map.edges_blocking(&helper, WAIT);
    assert_eq!(edges.len(), 1);
    assert_eq!(canned.calls.load(Ordering::SeqCst), 2);
    let saw = lock(&canned.saw).clone();
    assert!(saw[0].contains("helper_lci_probe"));
    assert!(saw[1].contains("__lci_probe: u8"));
    assert_eq!(map.runs().len(), 2);
}

#[test]
fn the_card_prices_a_probe_in_seconds_and_names_what_it_is_good_for() {
    let fixture = Fixture::new();
    let canned = Canned::new(|dir| vec![error_at(dir, "src/lib.rs", 3, 27)]);
    let map = fixture.map(&canned);
    let card = map.card();
    assert_eq!(card.expand_p50_ms, Some(PRIOR_EXPAND_MS));
    assert_eq!(card.good_for, [QuestionType::Impact]);
    assert_eq!(card.coverage, [NodeKind::Symbol, NodeKind::File]);
    assert_eq!(map.describe().freshness, FreshnessModel::Live);
    let helper = node_named(&map.inner.scip, "helper");
    map.edges_blocking(&helper, WAIT);
    let after = map.card().expand_p50_ms.unwrap();
    assert!(after > 0.0 && after < PRIOR_EXPAND_MS, "{after}");
}

#[tokio::test]
async fn expand_and_query_go_through_the_blocking_probe() {
    let fixture = Fixture::new();
    let canned = Canned::new(|dir| vec![error_at(dir, "src/lib.rs", 3, 27)]);
    let map = fixture.map(&canned);
    let helper = node_named(&map.inner.scip, "helper");
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
    let budget = Budget {
        deadline: Instant::now() + WAIT,
        max_external_calls: u32::MAX,
        max_nodes_touched: u64::MAX,
    };
    let edges = map.expand(&helper, budget, &view).await.edges;
    assert_eq!(edges.len(), 1);
    let seeds = [(helper.clone(), 1.0)];
    let params = serde_json::Value::Null;
    let response = map
        .query(
            &MapRequest {
                text: None,
                seeds: &seeds,
                limit: 10,
                budget,
                params: &params,
                as_of: None,
            },
            &view,
        )
        .await;
    assert_eq!(response.status, MapStatus::Ok);
    assert_eq!(response.freshness.observed, Observed::LiveTree);
    assert_eq!(response.hits.len(), 1);
    assert_eq!(response.hits[0].evidence[0].edge, EdgeKind::CompileErrorAt);
    assert_eq!(
        response.hits[0].evidence[0].detail.as_deref(),
        Some("src/lib.rs:3")
    );
}

/// A cargo that records the timeout each run was given.
struct Timed {
    timeouts: Mutex<Vec<Duration>>,
}

struct SharedTimed(Arc<Timed>);

impl CargoRunner for SharedTimed {
    fn check(&self, _dir: &Path, _target: &Path, timeout: Duration) -> CargoOutput {
        lock(&self.0.timeouts).push(timeout);
        CargoOutput {
            stdout: r#"{"reason":"build-finished","success":true}"#.into(),
            stderr: String::new(),
            timed_out: false,
            success: Some(true),
        }
    }
}

#[test]
fn a_probe_that_spent_its_budget_on_the_gate_does_not_copy_or_run_cargo() {
    let fixture = Fixture::new();
    let canned = Canned::new(|_| Vec::new());
    let map = fixture.map(&canned);
    let helper = node_named(&map.inner.scip, "helper");

    // No budget at all, and the gate is free: nothing may run.
    let edges = map.edges_blocking(&helper, Duration::ZERO);
    assert!(edges.is_empty());
    assert_eq!(canned.calls.load(Ordering::SeqCst), 0);
    assert!(!fixture.config.scratch.exists(), "the tree was copied");
    assert!(map.last_error().unwrap().contains("budget"));
}

#[test]
fn cargo_gets_only_the_budget_the_gate_wait_left() {
    let fixture = Fixture::new();
    let timed = Arc::new(Timed {
        timeouts: Mutex::new(Vec::new()),
    });
    let map = Arc::new(ProbeMap::with_cargo(
        scip(),
        fixture.config.clone(),
        Box::new(SharedTimed(timed.clone())),
    ));
    let helper = node_named(&map.inner.scip, "helper");

    // Another probe holds the gate for the first 2 s of this one's budget. The
    // hold starts only once the probe thread runs, and the 1 s margin covers a
    // loaded machine delaying the probe between that signal and its deadline.
    let budget = Duration::from_millis(5000);
    let held = lock(&map.inner.gate);
    let (started, running) = std::sync::mpsc::channel();
    let probe = {
        let (map, helper) = (map.clone(), helper.clone());
        std::thread::spawn(move || {
            started.send(()).unwrap();
            map.edges_blocking(&helper, budget)
        })
    };
    running.recv().unwrap();
    std::thread::sleep(Duration::from_millis(2000));
    drop(held);
    probe.join().unwrap();

    let timeouts = lock(&timed.timeouts).clone();
    assert_eq!(timeouts.len(), 1, "the probe should have run cargo once");
    assert!(
        timeouts[0] < Duration::from_millis(4000),
        "cargo was given {:?} of a {budget:?} budget after it waited about 2 s for the gate",
        timeouts[0]
    );
}

#[tokio::test]
async fn a_failed_probe_does_not_mark_the_next_response_partial() {
    let fixture = Fixture::new();
    let canned = Canned::new(|dir| vec![error_at(dir, "src/lib.rs", 3, 27)]);
    let map = fixture.map(&canned);
    let helper = node_named(&map.inner.scip, "helper");
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
    let seeds = [(helper.clone(), 1.0)];
    let params = serde_json::Value::Null;
    let ask = |deadline: Instant| {
        let request = MapRequest {
            text: None,
            seeds: &seeds,
            limit: 10,
            budget: Budget {
                deadline,
                max_external_calls: u32::MAX,
                max_nodes_touched: u64::MAX,
            },
            params: &params,
            as_of: None,
        };
        let (map, view) = (&map, &view);
        async move { map.query(&request, view).await }
    };

    // No budget: the probe does not run and the response says so.
    let starved = ask(Instant::now()).await;
    assert!(matches!(starved.status, MapStatus::Partial { .. }));
    assert!(starved.hits.is_empty());

    // The next call has its budget and runs cleanly: no leftover warning.
    let fed = ask(Instant::now() + WAIT).await;
    assert_eq!(fed.status, MapStatus::Ok);
    assert_eq!(fed.hits.len(), 1);
    assert_eq!(map.last_error(), None);
}

// ---------------------------------------------------------------- scratch

#[test]
fn the_scratch_is_synced_by_content_and_leaves_ignored_files_behind() {
    let base = Scratch::new("probe-sync").unwrap();
    let real = base.path().join("real");
    std::fs::create_dir_all(real.join("src")).unwrap();
    std::fs::create_dir_all(real.join("target")).unwrap();
    std::fs::write(real.join(".gitignore"), "/target/\n").unwrap();
    std::fs::write(real.join("src/lib.rs"), LIB).unwrap();
    std::fs::write(real.join("src/old.rs"), "// old\n").unwrap();
    std::fs::write(real.join("target/junk"), "x").unwrap();
    let git = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(&real)
            .args(args)
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}");
    };
    git(&["init", "-q"]);
    git(&["add", "-A"]);
    let scratch = base.path().join("scratch");
    // .gitignore, lib.rs, old.rs.
    assert_eq!(sync_scratch(&real, &scratch).unwrap(), 3);
    assert!(!scratch.join("target").exists());
    // Nothing changed: nothing written.
    assert_eq!(sync_scratch(&real, &scratch).unwrap(), 0);
    // One file edited, one removed, one added (untracked, not ignored).
    std::fs::write(real.join("src/lib.rs"), format!("{LIB}// more\n")).unwrap();
    std::fs::remove_file(real.join("src/old.rs")).unwrap();
    std::fs::write(real.join("src/new.rs"), "// new\n").unwrap();
    assert_eq!(sync_scratch(&real, &scratch).unwrap(), 3);
    assert!(!scratch.join("src/old.rs").exists());
    assert!(scratch.join("src/new.rs").exists());
    assert!(
        std::fs::read_to_string(scratch.join("src/lib.rs"))
            .unwrap()
            .ends_with("// more\n")
    );
}

#[test]
fn a_directory_that_is_not_a_repository_is_synced_without_target_and_git() {
    let base = Scratch::new("probe-sync-plain").unwrap();
    let real = base.path().join("real");
    std::fs::create_dir_all(real.join("src")).unwrap();
    std::fs::create_dir_all(real.join("target")).unwrap();
    std::fs::write(real.join("src/lib.rs"), LIB).unwrap();
    std::fs::write(real.join("target/junk"), "x").unwrap();
    let scratch = base.path().join("scratch");
    assert_eq!(sync_scratch(&real, &scratch).unwrap(), 1);
    assert!(!scratch.join("target").exists());
}

#[test]
fn a_process_that_outlives_its_timeout_is_killed() {
    let command = if cfg!(windows) {
        let mut c = Command::new("ping");
        c.args(["-n", "30", "127.0.0.1"]);
        c
    } else {
        let mut c = Command::new("sleep");
        c.arg("30");
        c
    };
    let started = Instant::now();
    let ran = run_limited(command, Duration::from_millis(300)).unwrap();
    assert!(ran.timed_out && ran.status.is_none());
    assert!(started.elapsed() < Duration::from_secs(10));
}

// ---------------------------------------------------------- real cargo

/// Needs `cargo` and a few seconds; run with `--ignored`.
#[test]
#[ignore = "runs real cargo check on a scratch crate"]
fn real_cargo_finds_the_caller_of_a_renamed_function() {
    let fixture = Fixture::new();
    let map = ProbeMap::new(scip(), fixture.config.clone());
    let helper = node_named(&map.inner.scip, "helper");
    let edges = map.edges_blocking(&helper, Duration::from_secs(120));
    let runs = map.runs();
    assert_eq!(
        runs[0].build_success,
        Some(false),
        "{runs:?} {:?}",
        map.last_error()
    );
    let dsts: Vec<String> = edges.iter().map(|e| e.dst.to_string()).collect();
    assert!(
        dsts.contains(&node_named(&map.inner.scip, "caller").to_string()),
        "{dsts:?}"
    );
    assert_eq!(fixture.real_lib(), LIB);
    assert_eq!(fixture.scratch_lib(), LIB);
    // The tree compiles again after the restore: a second probe of another
    // unit sees only that unit's callers.
    let edges = map.edges_blocking(
        &node_named(&map.inner.scip, "method"),
        Duration::from_secs(120),
    );
    let dsts: Vec<String> = edges.iter().map(|e| e.dst.to_string()).collect();
    assert!(
        dsts.contains(&node_named(&map.inner.scip, "use_it").to_string()),
        "{dsts:?}"
    );
    assert!(!dsts.contains(&node_named(&map.inner.scip, "caller").to_string()));
}
