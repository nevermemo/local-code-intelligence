//! `grep.live`: edge kinds and witness classes on a small fixture tree, the
//! caps, the walk rules, freshness after an edit, and the S5 equivalence of
//! the on-the-fly and pre-merged sources.

use super::*;
use crate::{
    EdgeKind, Registry, SnapshotRef,
    audit::audit_edge_sources,
    kit_c3::check_expand,
    scip_graph::{RawDocument, RawOccurrence, ScipGraph},
    scratch::Scratch,
    traverse::{
        merged::{BuildOptions, MergedAdjacency, PreMerged},
        source::{EdgeSource, OnTheFly},
    },
};
use lci_core::{
    chunk::Chunk,
    node::{SymbolKey, SymbolKind},
};

fn write(root: &Path, path: &str, text: &str) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

/// alpha defines `compute_total`; beta calls it; gamma has the word in a
/// comment and `compute_total_cached` and `recompute_totals`; delta has only
/// `recompute_totals`. Lines (0-based) of interest:
/// beta 2 col 11 (call), gamma 0 col 7 (comment), delta 0 col 11 (substring).
fn fixture() -> Scratch {
    let scratch = Scratch::new("live-grep").unwrap();
    let root = scratch.path();
    write(
        root,
        "src/alpha.rs",
        "pub fn compute_total() -> u32 { 1 }\n",
    );
    write(
        root,
        "src/beta.rs",
        "use crate::alpha;\nfn caller() {\n    alpha::compute_total();\n}\n",
    );
    write(
        root,
        "src/gamma.rs",
        "// see compute_total in alpha\nfn recompute_totals() {}\nfn other() {\n    let x = compute_total_cached;\n}\n",
    );
    write(root, "src/delta.rs", "fn x() { recompute_totals(); }\n");
    scratch
}

fn symbol(name: &str, module: &str) -> NodeId {
    NodeId::Symbol(SymbolKey {
        language: "rust".into(),
        module_path: vec![module.into()],
        container: Vec::new(),
        name: name.into(),
        kind: SymbolKind::Function,
        disambiguator: 0,
    })
}

fn spans() -> Arc<ScipSpans> {
    let mut by_file: HashMap<String, Vec<(u32, u32, NodeId)>> = HashMap::new();
    by_file.insert(
        "src/alpha.rs".into(),
        vec![(0, 0, symbol("compute_total", "alpha"))],
    );
    by_file.insert("src/beta.rs".into(), vec![(1, 3, symbol("caller", "beta"))]);
    by_file.insert(
        "src/gamma.rs".into(),
        vec![
            (1, 1, symbol("recompute_totals", "gamma")),
            (2, 4, symbol("other", "gamma")),
        ],
    );
    Arc::new(ScipSpans { by_file })
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

fn file(path: &str) -> NodeId {
    NodeId::file(path).unwrap()
}

fn site_of(edge: &Edge) -> (&str, u32, u32) {
    let site = edge.site.as_ref().unwrap();
    (&site.path, site.line, site.col)
}

fn by_dst<'a>(edges: &'a [Edge], dst: &NodeId) -> &'a Edge {
    edges
        .iter()
        .find(|e| e.dst == *dst)
        .unwrap_or_else(|| panic!("no edge to {dst}: {edges:#?}"))
}

#[tokio::test]
async fn a_symbol_reaches_the_files_and_symbols_that_mention_it() {
    let tree = fixture();
    let map = LiveGrep::new(tree.path(), Some(spans()));
    let node = symbol("compute_total", "alpha");
    let edges = check_expand(&map, &node, Duration::from_secs(10), &view())
        .await
        .expect("C7");

    assert!(
        edges
            .iter()
            .all(|e| e.kind == EdgeKind::Mentions && e.map == GREP_LIVE_MAP_ID)
    );
    // Files: the call (exact), the comment (exact, wins over the substrings
    // of its file), the file with only a substring (text). Not alpha: the
    // only occurrence there is the symbol's own definition.
    let beta = by_dst(&edges, &file("src/beta.rs"));
    assert_eq!(
        (beta.witness, beta.conf, site_of(beta)),
        (WitnessClass::Exact, 60, ("src/beta.rs", 2, 11))
    );
    let gamma = by_dst(&edges, &file("src/gamma.rs"));
    assert_eq!(
        (gamma.witness, gamma.conf, site_of(gamma)),
        (WitnessClass::Exact, 60, ("src/gamma.rs", 0, 7))
    );
    let delta = by_dst(&edges, &file("src/delta.rs"));
    assert_eq!(
        (delta.witness, delta.conf, site_of(delta)),
        (WitnessClass::Text, 20, ("src/delta.rs", 0, 11))
    );
    assert!(edges.iter().all(|e| e.dst != file("src/alpha.rs")));

    // Symbols: the enclosing declarations at the occurrences. The comment
    // line is inside no symbol.
    let caller = by_dst(&edges, &symbol("caller", "beta"));
    assert_eq!(
        (caller.witness, site_of(caller)),
        (WitnessClass::Exact, ("src/beta.rs", 2, 11))
    );
    let other = by_dst(&edges, &symbol("other", "gamma"));
    assert_eq!(
        (other.witness, site_of(other)),
        (WitnessClass::Text, ("src/gamma.rs", 3, 12))
    );
    let recompute = by_dst(&edges, &symbol("recompute_totals", "gamma"));
    assert_eq!(recompute.witness, WitnessClass::Text);
    assert_eq!(edges.len(), 6, "{edges:#?}");
    // Exact before text, the confidence order the contract asks for.
    assert_eq!(edges[0].witness, WitnessClass::Exact);
    assert_eq!(edges.last().unwrap().witness, WitnessClass::Text);
    // Every site names the current bytes.
    assert!(
        edges
            .iter()
            .all(|e| e.site.as_ref().unwrap().blob.starts_with("live:"))
    );
}

#[tokio::test]
async fn without_a_locator_the_edges_go_to_files_only() {
    let tree = fixture();
    let map = LiveGrep::new(tree.path(), None);
    let edges = map
        .expand(&symbol("compute_total", "alpha"), budget(), &view())
        .await
        .edges;
    assert_eq!(edges.len(), 4, "{edges:#?}");
    assert!(edges.iter().all(|e| e.dst.kind() == NodeKind::File));
    // The definition's own file now counts: nothing says the line is the
    // symbol's own.
    assert!(edges.iter().any(|e| e.dst == file("src/alpha.rs")));
}

#[tokio::test]
async fn a_file_or_chunk_node_looks_for_its_stem_and_not_in_itself() {
    let tree = fixture();
    let map = LiveGrep::new(tree.path(), Some(spans()));
    let from_file = map
        .expand(&file("src/alpha.rs"), budget(), &view())
        .await
        .edges;
    // "alpha" appears in beta (use and call) and gamma (comment).
    let dsts: Vec<_> = from_file
        .iter()
        .filter(|e| e.dst.kind() == NodeKind::File)
        .map(|e| e.dst.to_string())
        .collect();
    assert_eq!(dsts.len(), 2, "{from_file:#?}");
    assert!(dsts.iter().all(|d| d != "file:src/alpha.rs"));
    let beta = by_dst(&from_file, &file("src/beta.rs"));
    assert_eq!(site_of(beta), ("src/beta.rs", 0, 11));
    assert_eq!(beta.conf, 65, "two exact occurrences in beta");

    let chunk = NodeId::chunk("src/alpha.rs", "h", 0).unwrap();
    let from_chunk = map.expand(&chunk, budget(), &view()).await.edges;
    assert_eq!(from_chunk.len(), from_file.len());
    assert!(from_chunk.iter().all(|e| e.src == chunk));
}

#[tokio::test]
async fn names_that_match_everywhere_and_nodes_without_a_name_make_no_edges() {
    let tree = fixture();
    write(tree.path(), "src/zeta.rs", "fn new() {} fn default() {}\n");
    let map = LiveGrep::new(tree.path(), Some(spans()));
    for name in ["new", "default", "ab"] {
        let edges = map
            .expand(&symbol(name, "zeta"), budget(), &view())
            .await
            .edges;
        assert!(edges.is_empty(), "{name}");
    }
    let commit = NodeId::Commit {
        sha: "a".repeat(40),
    };
    assert!(
        map.expand(&commit, budget(), &view())
            .await
            .edges
            .is_empty()
    );
    assert_eq!(map.log().len(), 0, "no scan ran for any of them");
}

#[test]
fn the_walk_skips_ignored_binary_large_hidden_and_target_files() {
    let scratch = Scratch::new("live-grep-walk").unwrap();
    let root = scratch.path();
    write(root, ".gitignore", "ignored/\n");
    write(root, "src/a.rs", "let marker_name = 1;\n");
    write(root, "ignored/b.rs", "marker_name\n");
    write(root, ".hidden/c.rs", "marker_name\n");
    write(root, "target/d.rs", "marker_name\n");
    write(root, "node_modules/e.js", "marker_name\n");
    write(root, "bin.dat", "marker_name\0\0\0");
    write(root, "big.txt", &format!("marker_name {}", "x".repeat(300)));
    std::fs::write(root.join("latin.txt"), b"marker_name \xff\xfe").unwrap();
    let options = GrepOptions {
        max_file_bytes: 200,
        ..GrepOptions::default()
    };
    let found = scan(
        root,
        "marker_name",
        &options,
        Instant::now() + Duration::from_secs(10),
    );
    let paths: Vec<_> = found.matches.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(paths, ["src/a.rs"]);
    assert_eq!(found.skipped_large, 1);
    assert_eq!(found.skipped_binary, 2, "NUL file and non-UTF-8 file");
    assert_eq!(found.stopped, None);
    // Whole identifier: the `let` line has the word between a space and a
    // space, col 4.
    assert_eq!((found.matches[0].line, found.matches[0].col), (0, 4));
    assert!(found.matches[0].exact);
}

#[test]
fn the_match_cap_keeps_exact_matches_first_and_stops_the_walk() {
    let scratch = Scratch::new("live-grep-cap").unwrap();
    let root = scratch.path();
    write(root, "a.txt", "xneedle_zz needle_z\nneedle_z_extra\n");
    write(root, "b.txt", &"needle_z\n".repeat(10));
    write(root, "c.txt", &"needle_z\n".repeat(10));
    let options = GrepOptions {
        max_matches: 4,
        ..GrepOptions::default()
    };
    let found = scan(
        root,
        "needle_z",
        &options,
        Instant::now() + Duration::from_secs(10),
    );
    assert_eq!(found.matches.len(), 4);
    assert!(found.matches.iter().all(|m| m.exact), "{:?}", found.matches);
    assert_eq!(found.stopped, Some("match cap"));
    // c.txt was never read: the walk stopped inside b.txt.
    assert!(found.matches.iter().all(|m| m.path != "c.txt"));
    // With room left, text matches fill it after the exact ones.
    let wide = GrepOptions {
        max_matches: 5,
        ..GrepOptions::default()
    };
    let tree2 = Scratch::new("live-grep-cap2").unwrap();
    write(
        tree2.path(),
        "a.txt",
        "xneedle_zz needle_z\nneedle_z_extra\n",
    );
    let found = scan(
        tree2.path(),
        "needle_z",
        &wide,
        Instant::now() + Duration::from_secs(10),
    );
    let classes: Vec<_> = found.matches.iter().map(|m| m.exact).collect();
    assert_eq!(classes, [true, false, false]);
    assert_eq!(found.stopped, None);
}

/// The default options with the lockfile rule switched on.
fn lockfile_options() -> GrepOptions {
    GrepOptions {
        skip_files: LOCKFILES.iter().map(|s| (*s).to_owned()).collect(),
        ..GrepOptions::default()
    }
}

fn scan_now(root: &Path, needle: &str, options: &GrepOptions) -> ScanResult {
    scan(
        root,
        needle,
        options,
        Instant::now() + Duration::from_secs(10),
    )
}

#[test]
fn no_file_is_skipped_by_name_unless_the_caller_asks() {
    assert!(GrepOptions::default().skip_files.is_empty());
    assert_eq!(LOCKFILES.len(), 11);
    let scratch = Scratch::new("live-grep-default-names").unwrap();
    let root = scratch.path();
    write(root, "Cargo.lock", "needle_z\n");
    write(root, "app/package-lock.json", "needle_z\n");
    let found = scan_now(root, "needle_z", &GrepOptions::default());
    let paths: Vec<_> = found.matches.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(paths, ["Cargo.lock", "app/package-lock.json"]);
    assert_eq!((found.files_read, found.skipped_by_name), (2, 0));
}

#[test]
fn a_lockfile_is_never_read_and_takes_no_match_slot() {
    let scratch = Scratch::new("live-grep-lockfile").unwrap();
    let root = scratch.path();
    // Both lockfiles sort before src/, so without the rule they would fill
    // the cap and the walk would stop before the real use.
    write(root, "Cargo.lock", &"name = \"needle_z\"\n".repeat(10));
    write(
        root,
        "app/package-lock.json",
        &"\"needle_z\": {}\n".repeat(10),
    );
    // Only the exact name is skipped.
    write(root, "app/Cargo.lock.md", "needle_z\n");
    write(root, "src/a.rs", "use needle_z;\n");
    let options = GrepOptions {
        max_matches: 4,
        ..lockfile_options()
    };
    let found = scan_now(root, "needle_z", &options);
    let paths: Vec<_> = found.matches.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(paths, ["app/Cargo.lock.md", "src/a.rs"]);
    assert_eq!((found.files_read, found.skipped_by_name), (2, 2));
    assert_eq!(found.stopped, None);
    // With the list emptied the lockfiles are read again and take the cap.
    let all = GrepOptions {
        skip_files: Vec::new(),
        ..options
    };
    let found = scan_now(root, "needle_z", &all);
    assert_eq!(found.matches.len(), 4);
    assert!(found.matches.iter().all(|m| m.path == "Cargo.lock"));
    assert_eq!(found.skipped_by_name, 0);
    assert_eq!(found.stopped, Some("match cap"));
}

#[test]
fn every_lockfile_name_is_skipped_at_the_root_and_nested() {
    let scratch = Scratch::new("live-grep-all-lockfiles").unwrap();
    let root = scratch.path();
    for name in LOCKFILES {
        write(root, name, "needle_z\n");
        write(root, &format!("packages/deep/{name}"), "needle_z\n");
    }
    write(root, "src/a.rs", "use needle_z;\n");
    let found = scan_now(root, "needle_z", &lockfile_options());
    let paths: Vec<_> = found.matches.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(paths, ["src/a.rs"]);
    assert_eq!(found.skipped_by_name, 2 * LOCKFILES.len() as u64);
    assert_eq!(found.files_read, 1);
    assert_eq!(found.stopped, None);
}

#[test]
fn authored_configs_beside_a_lockfile_are_still_scanned() {
    let scratch = Scratch::new("live-grep-configs").unwrap();
    let root = scratch.path();
    write(root, "js/package.json", "{ \"needle_z\": \"1\" }\n");
    write(root, "js/package-lock.json", "{ \"needle_z\": {} }\n");
    write(root, "py/pyproject.toml", "needle_z = \"1\"\n");
    write(root, "py/uv.lock", "name = \"needle_z\"\n");
    write(root, "rs/Cargo.toml", "needle_z = \"1\"\n");
    write(root, "rs/Cargo.lock", "name = \"needle_z\"\n");
    let found = scan_now(root, "needle_z", &lockfile_options());
    let paths: Vec<_> = found.matches.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(
        paths,
        ["js/package.json", "py/pyproject.toml", "rs/Cargo.toml"]
    );
    assert_eq!((found.files_read, found.skipped_by_name), (3, 3));
}

#[test]
fn a_name_that_differs_in_case_is_not_a_lockfile() {
    let scratch = Scratch::new("live-grep-lockfile-case").unwrap();
    let root = scratch.path();
    write(root, "cargo.lock", "needle_z\n");
    let found = scan_now(root, "needle_z", &lockfile_options());
    let paths: Vec<_> = found.matches.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(paths, ["cargo.lock"]);
    assert_eq!((found.files_read, found.skipped_by_name), (1, 0));
}

#[test]
fn a_directory_named_like_a_lockfile_is_still_walked() {
    let scratch = Scratch::new("live-grep-lockfile-dir").unwrap();
    let root = scratch.path();
    write(root, "uv.lock/inner.txt", "needle_z\n");
    let found = scan_now(root, "needle_z", &lockfile_options());
    let paths: Vec<_> = found.matches.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(paths, ["uv.lock/inner.txt"]);
    assert_eq!((found.files_read, found.skipped_by_name), (1, 0));
}

#[test]
fn a_skipped_lockfile_adds_nothing_to_files_read_or_bytes_read() {
    let scratch = Scratch::new("live-grep-lockfile-bytes").unwrap();
    let root = scratch.path();
    let source = "use needle_z;\n";
    // Few matches, so the default match cap does not end the scan in the lockfile.
    let lock = "name = \"needle_z\"\n".repeat(3);
    write(root, "Cargo.lock", &lock);
    write(root, "src/a.rs", source);
    let found = scan_now(root, "needle_z", &lockfile_options());
    assert_eq!(found.files_read, 1);
    assert_eq!(found.bytes_read, source.len() as u64);
    assert_eq!(found.skipped_by_name, 1);
    // Skipped for the name, not counted as large or binary.
    assert_eq!((found.skipped_large, found.skipped_binary), (0, 0));
    let found = scan_now(root, "needle_z", &GrepOptions::default());
    assert_eq!(found.files_read, 2);
    assert_eq!(found.bytes_read, (source.len() + lock.len()) as u64);
    assert_eq!(found.skipped_by_name, 0);
}

#[test]
fn a_crlf_file_gives_the_same_positions_as_an_lf_file() {
    let scratch = Scratch::new("live-grep-crlf").unwrap();
    let root = scratch.path();
    // The needle ends each line, so the `\r` is the character after it.
    write(root, "crlf.txt", "let a = needle_z\r\nlet b = needle_z\r\n");
    write(root, "lf.txt", "let a = needle_z\nlet b = needle_z\n");
    let found = scan_now(root, "needle_z", &lockfile_options());
    let positions = |path: &str| -> Vec<(u32, u32, bool)> {
        found
            .matches
            .iter()
            .filter(|m| m.path == path)
            .map(|m| (m.line, m.col, m.exact))
            .collect()
    };
    assert_eq!(positions("lf.txt"), [(0, 8, true), (1, 8, true)]);
    assert_eq!(positions("crlf.txt"), positions("lf.txt"));
}

#[test]
fn unicode_paths_and_content_around_a_skipped_lockfile_keep_their_columns() {
    let scratch = Scratch::new("live-grep-unicode").unwrap();
    let root = scratch.path();
    write(root, "日本語/Cargo.lock", "名前 = \"needle_z\"\n");
    // Byte column: `// héllo → ` is 14 bytes (é is 2, → is 3).
    write(root, "日本語/main.rs", "// héllo → needle_z\n");
    let found = scan_now(root, "needle_z", &lockfile_options());
    let rows: Vec<_> = found
        .matches
        .iter()
        .map(|m| (m.path.as_str(), m.line, m.col, m.exact))
        .collect();
    assert_eq!(rows, [("日本語/main.rs", 0, 14, true)]);
    assert_eq!((found.files_read, found.skipped_by_name), (1, 1));
}

#[test]
fn a_passed_deadline_stops_the_scan_and_says_so() {
    let tree = fixture();
    let found = scan(
        tree.path(),
        "compute_total",
        &GrepOptions::default(),
        Instant::now() - Duration::from_secs(1),
    );
    assert_eq!(found.stopped, Some("deadline"));
    assert!(found.matches.is_empty());
}

#[tokio::test]
async fn an_edit_after_construction_is_seen_by_the_next_call() {
    let tree = fixture();
    let map = LiveGrep::new(tree.path(), Some(spans()));
    let node = symbol("compute_total", "alpha");
    let before = map.expand(&node, budget(), &view()).await.edges;
    assert!(before.iter().all(|e| e.dst != file("src/epsilon.rs")));
    let beta_blob = by_dst(&before, &file("src/beta.rs"))
        .site
        .as_ref()
        .unwrap()
        .blob
        .clone();

    write(
        tree.path(),
        "src/epsilon.rs",
        "fn z() {\n    compute_total();\n}\n",
    );
    write(
        tree.path(),
        "src/beta.rs",
        "use crate::alpha;\nfn caller() {\n    alpha::compute_total();\n    compute_total();\n}\n",
    );
    let after = map.expand(&node, budget(), &view()).await.edges;
    let epsilon = by_dst(&after, &file("src/epsilon.rs"));
    assert_eq!(
        (epsilon.witness, site_of(epsilon)),
        (WitnessClass::Exact, ("src/epsilon.rs", 1, 4))
    );
    let beta = by_dst(&after, &file("src/beta.rs"));
    assert_eq!(beta.conf, 65, "the new occurrence counts");
    assert_ne!(
        beta.site.as_ref().unwrap().blob,
        beta_blob,
        "new bytes, new blob"
    );

    // And an occurrence that was removed is gone.
    write(tree.path(), "src/delta.rs", "fn x() {}\n");
    let gone = map.expand(&node, budget(), &view()).await.edges;
    assert!(gone.iter().all(|e| e.dst != file("src/delta.rs")));
}

#[tokio::test]
async fn query_names_the_same_destinations_and_reports_the_live_tree() {
    let tree = fixture();
    let map = LiveGrep::new(tree.path(), Some(spans()));
    let node = symbol("compute_total", "alpha");
    let edges = map.expand(&node, budget(), &view()).await.edges;
    let seeds = [(node.clone(), 1.0)];
    let params = serde_json::Value::Null;
    let mut request = MapRequest {
        text: None,
        seeds: &seeds,
        limit: 100,
        budget: budget(),
        params: &params,
        as_of: None,
    };
    let response = map.query(&request, &view()).await;
    let want: Vec<_> = edges.iter().map(|e| e.dst.clone()).collect();
    let got: Vec<_> = response.hits.iter().map(|h| h.node.clone()).collect();
    assert_eq!(got, want);
    assert_eq!(response.freshness.observed, Observed::LiveTree);
    assert_eq!(response.status, MapStatus::Ok);
    assert!(response.cost.bytes_read > 0);

    // A deadline that has passed is a partial answer, not a failure.
    request.budget.deadline = Instant::now() - Duration::from_secs(1);
    let late = map.query(&request, &view()).await;
    assert!(matches!(late.status, MapStatus::Partial { .. }), "{late:?}");
    assert!(late.hits.is_empty());
}

#[tokio::test]
async fn the_card_reports_a_measured_median_and_the_live_model() {
    let tree = fixture();
    let map = LiveGrep::new(tree.path(), None);
    assert!(map.card().expand_p50_ms.is_none(), "nothing measured yet");
    let p50 = map.calibrate(3).await.expect("measured");
    assert!(p50 >= 0.0);
    let card = map.card();
    assert_eq!(card.expand_p50_ms, Some(p50));
    assert_eq!(card.freshness_lag, Some(Duration::ZERO));
    assert_eq!(card.coverage, vec![NodeKind::File, NodeKind::Symbol]);
    assert_eq!(map.describe().freshness, FreshnessModel::Live);
    // Real calls add to the log.
    map.expand(&symbol("compute_total", "alpha"), budget(), &view())
        .await;
    assert_eq!(map.log().len(), 4);
}

#[test]
fn scip_spans_find_the_innermost_definition_around_a_line() {
    let occ = |range, symbol: &str, roles, enclosing| RawOccurrence {
        range,
        symbol: symbol.to_string(),
        roles,
        enclosing,
    };
    let s = |p: &str| format!("rust-analyzer cargo demo 0.1.0 {p}");
    let docs = vec![RawDocument {
        path: "a.rs".into(),
        occurrences: vec![
            occ((0, 3, 0, 8), &s("a/outer()."), 1, Some((0, 0, 9, 1))),
            occ(
                (2, 7, 2, 12),
                &s("a/outer()./inner()."),
                1,
                Some((2, 4, 4, 5)),
            ),
        ],
    }];
    let map = ScipMap::with_default_ids(ScipGraph::build(&docs, None));
    let spans = ScipSpans::from_scip(&map);
    assert_eq!(spans.files(), 1);
    let name = |line| {
        spans.symbol_at("a.rs", line).map(|id| match id {
            NodeId::Symbol(key) => key.name,
            other => other.to_string(),
        })
    };
    assert_eq!(name(1).as_deref(), Some("outer"));
    assert_eq!(name(3).as_deref(), Some("inner"));
    assert_eq!(name(9).as_deref(), Some("outer"));
    assert_eq!(name(12), None);
    assert!(spans.symbol_at("b.rs", 1).is_none());
}

// ---- S5: pre-merged == on the fly ---------------------------------------------

fn chunk(path: &str, hash: &str) -> Chunk {
    Chunk {
        relative_file_path: path.into(),
        language: "rust".into(),
        start_line: 1,
        end_line: 5,
        code: String::new(),
        content_hash: hash.into(),
    }
}

fn registry(root: &Path) -> Registry<'static> {
    let mut registry = Registry::new(60.0);
    registry
        .register(Box::new(LiveGrep::new(root, Some(spans()))), 1.0, 40)
        .unwrap();
    registry
}

fn snapshot_view(chunks: &[Chunk]) -> SnapshotView<'_> {
    SnapshotView::new(
        SnapshotRef {
            indexed_at: "t".into(),
            fingerprint: "f1".into(),
        },
        chunks.iter().collect(),
    )
}

const PER_NODE: Duration = Duration::from_secs(10);

#[tokio::test]
async fn the_pre_merged_source_equals_the_on_the_fly_source() {
    let tree = fixture();
    let reg = registry(tree.path());
    let chunks: Vec<Chunk> = ["alpha", "beta", "gamma", "delta"]
        .iter()
        .map(|n| chunk(&format!("src/{n}.rs"), n))
        .collect();
    let view = snapshot_view(&chunks);
    let roots = vec![symbol("compute_total", "alpha")];
    let adjacency = MergedAdjacency::build_with(
        &reg,
        &view,
        &BuildOptions {
            roots: roots.clone(),
            ..BuildOptions::default()
        },
    )
    .await;
    assert!(adjacency.covers(&symbol("compute_total", "alpha")));
    // The build followed the edges to the symbols they name.
    assert!(adjacency.covers(&symbol("caller", "beta")));

    let mut nodes: Vec<NodeId> = adjacency.nodes().cloned().collect();
    nodes.push(NodeId::Commit {
        sha: "b".repeat(40),
    });
    let mut fly = OnTheFly::new(&reg, &view);
    let mut merged = PreMerged::new(&adjacency);
    let report = audit_edge_sources(&fly, &merged, &nodes, PER_NODE)
        .await
        .expect("equal on every node");
    assert_eq!(report.nodes, nodes.len());
    assert!(report.edges >= 6, "{report:?}");
    // With the map switched off on both sides: empty on both.
    assert!(
        fly.set_enabled(GREP_LIVE_MAP_ID, false) && merged.set_enabled(GREP_LIVE_MAP_ID, false)
    );
    audit_edge_sources(&fly, &merged, &nodes, PER_NODE)
        .await
        .expect("equal with the map off");
}

#[tokio::test]
async fn an_update_asks_a_live_row_again_though_no_snapshot_blob_moved() {
    let tree = fixture();
    let reg = registry(tree.path());
    let chunks: Vec<Chunk> = ["alpha", "beta", "gamma", "delta"]
        .iter()
        .map(|n| chunk(&format!("src/{n}.rs"), n))
        .collect();
    let view = snapshot_view(&chunks);
    let options = BuildOptions {
        roots: vec![symbol("compute_total", "alpha")],
        ..BuildOptions::default()
    };
    let mut adjacency = MergedAdjacency::build_with(&reg, &view, &options).await;

    write(
        tree.path(),
        "src/epsilon.rs",
        "fn z() { compute_total(); }\n",
    );
    // The same snapshot: every chunk hash is as before.
    let stats = adjacency.update(&reg, &view, &options).await;
    assert_eq!(stats.reused, 0, "{stats:?}");
    assert!(stats.expanded > 0);

    let fly = OnTheFly::new(&reg, &view);
    let merged = PreMerged::new(&adjacency);
    let node = symbol("compute_total", "alpha");
    let got = merged.edges(&node, budget()).await;
    assert!(
        got.edges
            .iter()
            .any(|e| e.best.dst == file("src/epsilon.rs")),
        "the edit is in the row"
    );
    let nodes: Vec<NodeId> = adjacency.nodes().cloned().collect();
    audit_edge_sources(&fly, &merged, &nodes, PER_NODE)
        .await
        .expect("an updated adjacency equals the live source");
}

// ---------------------------------------------------------------- Corpus

/// Everything `Corpus::scan` must treat as `scan` does, in one tree.
fn corpus_tree(tag: &str) -> Scratch {
    let scratch = Scratch::new(tag).unwrap();
    let root = scratch.path();
    write(root, ".gitignore", "ignored/\n");
    write(
        root,
        "src/a.rs",
        "fn needle_z() {}\nlet x = xneedle_zz + needle_z;\n// needle_z_extra\n",
    );
    write(root, "src/b.rs", "call(needle_z)\r\nneedle_z\r\n");
    write(root, "docs/readme.md", "needle_z in prose, Needle_Z too\n");
    write(root, "Cargo.lock", "needle_z = 1\n");
    write(root, "sub/package-lock.json", "{\"needle_z\": 1}\n");
    write(root, "big.txt", &format!("needle_z {}", "x".repeat(400)));
    write(root, "empty.txt", "");
    write(root, "many.txt", &"needle_z\n".repeat(20));
    write(root, "ignored/i.rs", "needle_z\n");
    write(root, ".hidden/h.rs", "needle_z\n");
    write(root, "target/t.rs", "needle_z\n");
    write(root, "node_modules/n.js", "needle_z\n");
    write(root, "vendor/v.rs", "needle_z\n");
    write(root, "日本語/main.rs", "// héllo → needle_z\n");
    // The NUL lies past the probe, so the file is text.
    write(
        root,
        "late_nul.txt",
        &format!("{}\0 needle_z\n", "a".repeat(BINARY_PROBE + 10)),
    );
    std::fs::write(root.join("bin.dat"), b"needle_z\0\0").unwrap();
    std::fs::write(root.join("latin.txt"), b"needle_z \xff\xfe").unwrap();
    scratch
}

/// `Corpus` and `scan` agree on every field but the time.
fn assert_corpus_equals_scan(root: &Path, options: &GrepOptions, needles: &[&str]) {
    let corpus = Corpus::load(root, options);
    for needle in needles {
        let far = Instant::now() + Duration::from_secs(60);
        let disk = scan(root, needle, options, far);
        let stored = corpus.scan(needle, far);
        let label = format!("needle {needle:?}, options {options:?}");
        assert_eq!(stored.matches, disk.matches, "matches, {label}");
        assert_eq!(stored.files_read, disk.files_read, "files_read, {label}");
        assert_eq!(stored.bytes_read, disk.bytes_read, "bytes_read, {label}");
        assert_eq!(
            stored.skipped_binary, disk.skipped_binary,
            "binary, {label}"
        );
        assert_eq!(stored.skipped_large, disk.skipped_large, "large, {label}");
        assert_eq!(
            stored.skipped_by_name, disk.skipped_by_name,
            "name, {label}"
        );
        assert_eq!(stored.stopped, disk.stopped, "stopped, {label}");
    }
}

const CORPUS_NEEDLES: &[&str] = &[
    "needle_z",
    "needle_",
    "needle_z_extra",
    "Needle_Z",
    "NEEDLE_Z",
    "xneedle_zz",
    "x",
    "a",
    "no_such_token_anywhere",
    "",
    "日本語",
    "héllo",
];

#[test]
fn a_corpus_scan_equals_a_disk_scan_over_every_kind_of_file() {
    let tree = corpus_tree("corpus-equal");
    let root = tree.path();
    let lockfiles = LOCKFILES.iter().map(|s| (*s).to_owned()).collect();
    let variants = [
        ("defaults", GrepOptions::default()),
        (
            "lockfiles skipped",
            GrepOptions {
                skip_files: lockfiles,
                ..GrepOptions::default()
            },
        ),
        (
            "large files cut at 200 bytes",
            GrepOptions {
                max_file_bytes: 200,
                ..GrepOptions::default()
            },
        ),
        (
            "no match cap",
            GrepOptions {
                max_matches: usize::MAX,
                ..GrepOptions::default()
            },
        ),
        (
            "cap of 3",
            GrepOptions {
                max_matches: 3,
                ..GrepOptions::default()
            },
        ),
        (
            "cap of 1",
            GrepOptions {
                max_matches: 1,
                ..GrepOptions::default()
            },
        ),
        (
            "cap of 0",
            GrepOptions {
                max_matches: 0,
                ..GrepOptions::default()
            },
        ),
        (
            "other skip_dirs",
            GrepOptions {
                skip_dirs: vec!["vendor".into(), "docs".into()],
                ..GrepOptions::default()
            },
        ),
        (
            "no skip_dirs",
            GrepOptions {
                skip_dirs: Vec::new(),
                ..GrepOptions::default()
            },
        ),
        (
            "named file skipped",
            GrepOptions {
                skip_files: vec!["many.txt".into(), "a.rs".into()],
                ..GrepOptions::default()
            },
        ),
    ];
    for (name, options) in variants {
        println!("options: {name}");
        assert_corpus_equals_scan(root, &options, CORPUS_NEEDLES);
    }
}

#[test]
fn a_corpus_scan_stops_on_the_byte_cap_where_a_disk_scan_does() {
    let tree = corpus_tree("corpus-bytecap");
    let root = tree.path();
    let lockfiles: Vec<String> = LOCKFILES.iter().map(|s| (*s).to_owned()).collect();
    for max_scan_bytes in [0, 1, 40, 100, 300, 1000, 9000, 20_000, u64::MAX] {
        for skip_files in [Vec::new(), lockfiles.clone()] {
            let options = GrepOptions {
                max_scan_bytes,
                max_matches: usize::MAX,
                skip_files,
                ..GrepOptions::default()
            };
            assert_corpus_equals_scan(root, &options, CORPUS_NEEDLES);
        }
    }
    // The cap bites mid-walk, on both sides.
    let tight = GrepOptions {
        max_scan_bytes: 100,
        ..GrepOptions::default()
    };
    let far = Instant::now() + Duration::from_secs(60);
    assert_eq!(
        scan(root, "needle_z", &tight, far).stopped,
        Some("byte cap")
    );
    assert_eq!(
        Corpus::load(root, &tight).scan("needle_z", far).stopped,
        Some("byte cap")
    );
}

#[test]
fn a_corpus_counts_what_a_disk_scan_counts() {
    let tree = corpus_tree("corpus-counts");
    let options = GrepOptions {
        max_matches: usize::MAX,
        max_file_bytes: 200,
        skip_files: LOCKFILES.iter().map(|s| (*s).to_owned()).collect(),
        ..GrepOptions::default()
    };
    let corpus = Corpus::load(tree.path(), &options);
    let found = corpus.scan("needle_z", Instant::now() + Duration::from_secs(60));
    // Cargo.lock and package-lock.json by name; big.txt and late_nul.txt are
    // over 200 bytes; bin.dat and latin.txt are binary.
    assert_eq!(found.skipped_by_name, 2);
    assert_eq!(found.skipped_large, 2);
    assert_eq!(found.skipped_binary, 2);
    assert_eq!(found.stopped, None);
    assert!(found.matches.iter().all(|m| m.path != "Cargo.lock"));
    assert!(found.files_read >= 2 && found.bytes_read > 0);
    assert!(corpus.bytes() > 0 && corpus.files() > found.files_read as usize);
    assert_eq!(corpus.options().max_file_bytes, 200);
}

#[test]
fn a_corpus_keeps_the_bytes_of_its_load() {
    let tree = corpus_tree("corpus-stale");
    let root = tree.path();
    let options = GrepOptions::default();
    let corpus = Corpus::load(root, &options);
    let far = Instant::now() + Duration::from_secs(60);
    let before = corpus.scan("needle_z", far);
    write(root, "src/new.rs", "needle_z\n");
    std::fs::remove_file(root.join("src/a.rs")).unwrap();
    assert_eq!(corpus.scan("needle_z", far).matches, before.matches);
    assert_ne!(
        scan(root, "needle_z", &options, far).matches,
        before.matches
    );
    // A corpus loaded after the edit agrees with the disk again.
    assert_corpus_equals_scan(root, &options, &["needle_z"]);
}

#[test]
fn a_corpus_scan_past_its_deadline_stops_at_once() {
    let tree = fixture();
    let corpus = Corpus::load(tree.path(), &GrepOptions::default());
    let found = corpus.scan("compute_total", Instant::now());
    assert_eq!(found.stopped, Some("deadline"));
    assert!(found.matches.is_empty());
    assert_eq!(found.files_read, 0);
}
