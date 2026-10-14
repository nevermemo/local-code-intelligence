//! The pre-merged source (slice S5): equality with the on-the-fly source on
//! fixtures (the equivalence auditor), blobs in sites, and the incremental
//! update keyed by blob.

use super::*;
use crate::{
    audit::audit_edge_sources,
    traverse::merged::{BuildOptions, FileBlobs, GAP_MAP, MergedAdjacency, PreMerged},
};
use lci_core::chunk::Chunk;
use std::sync::Arc;

fn chunk(path: &str, hash: &str) -> Chunk {
    Chunk {
        relative_file_path: path.into(),
        language: "rust".into(),
        start_line: 1,
        end_line: 10,
        code: String::new(),
        content_hash: hash.into(),
    }
}

/// One chunk per file: `(name, content hash)`.
fn files(content: &[(&str, &str)]) -> Vec<Chunk> {
    content
        .iter()
        .map(|(name, hash)| chunk(&format!("src/{name}.rs"), hash))
        .collect()
}

fn view_of<'a>(chunks: &'a [Chunk], fingerprint: &str) -> SnapshotView<'a> {
    SnapshotView::new(
        SnapshotRef {
            indexed_at: "t".into(),
            fingerprint: fingerprint.into(),
        },
        chunks.iter().collect(),
    )
}

const PER_NODE: Duration = Duration::from_secs(5);

fn call_budget() -> Budget {
    Budget {
        deadline: Instant::now() + PER_NODE,
        max_external_calls: u32::MAX,
        max_nodes_touched: u64::MAX,
    }
}

async fn build(reg: &Registry<'static>, view: &SnapshotView<'_>) -> MergedAdjacency {
    MergedAdjacency::build(reg, view).await
}

fn name_of(node: &NodeId) -> String {
    node.path()
        .and_then(|p| p.strip_prefix("src/"))
        .and_then(|p| p.strip_suffix(".rs"))
        .unwrap_or("?")
        .to_string()
}

/// Two maps that overlap (a -> b is in both) and one that is witness text.
fn two_maps() -> Registry<'static> {
    registry(vec![
        GraphMap::new(
            "m1",
            &[("a", "b"), ("a", "c"), ("b", "d"), ("c", "d"), ("d", "a")],
        ),
        GraphMap::new("m2", &[("a", "b"), ("a", "x"), ("x", "y")])
            .conf(60)
            .witness(WitnessClass::Text),
    ])
}

// ---- equality with the on-the-fly source ------------------------------------

#[tokio::test]
async fn pre_merged_gives_the_edges_the_maps_give_on_the_fly() {
    let reg = two_maps();
    let chunks = files(&[("a", "h1"), ("b", "h2"), ("c", "h3"), ("d", "h4")]);
    let view = view_of(&chunks, "f1");
    let adjacency = build(&reg, &view).await;
    // x and y are not in the view: the build reached them through the edges.
    assert!(adjacency.covers(&node("x")) && adjacency.covers(&node("y")));

    let mut nodes: Vec<NodeId> = ["a", "b", "c", "d", "x", "y"].map(node).to_vec();
    nodes.extend(view.ids().iter().cloned()); // chunks: no map accepts them
    nodes.push(NodeId::Test {
        runner: lci_core::node::TestRunner::Cargo,
        name: "t".into(),
    });

    let mut fly = OnTheFly::new(&reg, &view);
    let mut merged = PreMerged::new(&adjacency);
    let report = audit_edge_sources(&fly, &merged, &nodes, PER_NODE)
        .await
        .expect("equal on every node");
    assert_eq!(report.nodes, nodes.len());
    assert!(report.edges >= 7, "{report:?}");

    // The same with a map switched off on both sides: the rest is fused again.
    assert!(fly.set_enabled("m2", false) && merged.set_enabled("m2", false));
    let report = audit_edge_sources(&fly, &merged, &nodes, PER_NODE)
        .await
        .expect("equal with m2 off");
    assert!(report.edges >= 5);
    assert!(!fly.set_enabled("nope", false) && !merged.set_enabled("nope", false));
    // And back on.
    assert!(fly.set_enabled("m2", true) && merged.set_enabled("m2", true));
    audit_edge_sources(&fly, &merged, &nodes, PER_NODE)
        .await
        .expect("equal with m2 back on");
}

#[tokio::test]
async fn a_map_the_registry_disabled_is_left_out_of_both_sources() {
    let mut reg = two_maps();
    assert!(reg.set_enabled("m2", false));
    let chunks = files(&[("a", "h1"), ("b", "h2")]);
    let view = view_of(&chunks, "f1");
    let adjacency = build(&reg, &view).await;
    let nodes = [node("a"), node("b"), node("d")];
    let mut fly = OnTheFly::new(&reg, &view);
    let mut merged = PreMerged::new(&adjacency);
    audit_edge_sources(&fly, &merged, &nodes, PER_NODE)
        .await
        .unwrap();
    // Known to both, off in the registry: switching it on changes nothing.
    assert!(fly.set_enabled("m2", true) && merged.set_enabled("m2", true));
    audit_edge_sources(&fly, &merged, &nodes, PER_NODE)
        .await
        .unwrap();
    let a = merged.edges(&node("a"), call_budget()).await;
    assert!(a.edges.iter().all(|e| e.maps == ["m1"]));
}

#[tokio::test]
async fn the_auditor_names_the_first_difference() {
    let chunks = files(&[("a", "h1"), ("b", "h2")]);
    let view = view_of(&chunks, "f1");
    let built = registry(vec![GraphMap::new("m1", &[("a", "b"), ("a", "c")])]);
    let adjacency = build(&built, &view).await;
    let merged = PreMerged::new(&adjacency);

    // The maps changed since the build: a -> c became weaker.
    let changed = registry(vec![
        GraphMap::new("m1", &[("a", "b"), ("a", "c")]).conf(70),
    ]);
    let fly = OnTheFly::new(&changed, &view);
    let failure = audit_edge_sources(&fly, &merged, &[node("b"), node("a")], PER_NODE)
        .await
        .expect_err("the sources differ");
    assert_eq!(failure.contract, "S5");
    assert!(failure.detail.contains("file:src/a.rs"), "{failure:?}");
    assert!(failure.detail.contains("conf=70"), "{failure:?}");

    // Same entries in another order is a failure too.
    struct Reversed<'m>(PreMerged<&'m MergedAdjacency>);
    #[crate::async_trait]
    impl EdgeSource for Reversed<'_> {
        async fn edges(&self, node: &NodeId, budget: Budget) -> Expansion {
            let mut expansion = self.0.edges(node, budget).await;
            expansion.edges.reverse();
            expansion
        }
        fn card(&self, map: MapId) -> crate::edge::MapCard {
            self.0.card(map)
        }
        fn set_enabled(&mut self, id: &str, on: bool) -> bool {
            self.0.set_enabled(id, on)
        }
    }
    let fly = OnTheFly::new(&built, &view);
    let reversed = Reversed(PreMerged::new(&adjacency));
    let failure = audit_edge_sources(&fly, &reversed, &[node("a")], PER_NODE)
        .await
        .expect_err("the order differs");
    assert!(failure.detail.contains("different order"), "{failure:?}");
}

#[tokio::test]
async fn a_walk_is_the_same_under_either_source() {
    let reg = two_maps();
    let chunks = files(&[("a", "h1"), ("b", "h2"), ("c", "h3"), ("d", "h4")]);
    let view = view_of(&chunks, "f1");
    let adjacency = Arc::new(build(&reg, &view).await);
    let off_at_each_stop = || scripted(|_| vec![act(None, Action::MapOff("m2".into()))]);

    let fly = Traversal::new(OnTheFly::new(&reg, &view), vec![seed("a")], budget(20))
        .question("a to y")
        .handler(off_at_each_stop(), every(2))
        .run()
        .await;
    let merged = Traversal::new(
        PreMerged::new(adjacency.clone()),
        vec![seed("a")],
        budget(20),
    )
    .question("a to y")
    .handler(off_at_each_stop(), every(2))
    .run()
    .await;
    assert!(fly.totals.hops > 0);
    assert_eq!(fly.events, merged.events);
    assert_eq!(fly.status, merged.status);

    // The adjacency is shared, not consumed.
    let again = Traversal::new(PreMerged::new(adjacency), vec![seed("a")], budget(20))
        .question("a to y")
        .handler(off_at_each_stop(), every(2))
        .run()
        .await;
    assert_eq!(again.events, merged.events);
}

// ---- blobs ---------------------------------------------------------------

#[tokio::test]
async fn sites_carry_a_content_hash_and_keep_the_path() {
    let chunks = files(&[("a", "same"), ("b", "same"), ("c", "other")]);
    let view = view_of(&chunks, "f1");
    let blobs = FileBlobs::of(&view);
    let (a, b, c) = (
        blobs.blob("src/a.rs").unwrap(),
        blobs.blob("src/b.rs").unwrap(),
        blobs.blob("src/c.rs").unwrap(),
    );
    assert_eq!(a, b, "equal content, equal blob");
    assert_ne!(a, c);
    assert_eq!(a.len(), 32);
    assert!(a.bytes().all(|byte| byte.is_ascii_hexdigit()));
    assert_eq!(blobs.counts().get(a), Some(&2));
    assert_eq!(blobs.counts().get(c), Some(&1));
    assert!(blobs.blob("src/nowhere.rs").is_none());

    // An edit moves the blob; so does moving the chunk's lines.
    let edited = files(&[("a", "changed"), ("b", "same"), ("c", "other")]);
    let edited_blobs = FileBlobs::of(&view_of(&edited, "f2"));
    assert_ne!(edited_blobs.blob("src/a.rs"), Some(a));
    assert_eq!(edited_blobs.blob("src/b.rs"), Some(a));
    let mut moved = files(&[("a", "same")]);
    moved[0].start_line = 5;
    assert_ne!(
        FileBlobs::of(&view_of(&moved, "f3")).blob("src/a.rs"),
        Some(a)
    );

    // The sources fill the blob and keep the path. A file the view does not
    // hold has no blob.
    let reg = registry(vec![
        GraphMap::new("m1", &[("a", "b")]).site("src/c.rs"),
        GraphMap::new("m2", &[("a", "x")]).site("src/gone.rs"),
    ]);
    let expansion = OnTheFly::new(&reg, &view)
        .edges(&node("a"), call_budget())
        .await;
    let site_of = |dst: &str| {
        expansion
            .edges
            .iter()
            .find(|e| e.best.dst == node(dst))
            .and_then(|e| e.best.site.clone())
            .unwrap()
    };
    assert_eq!(site_of("b").path, "src/c.rs");
    assert_eq!(site_of("b").blob, c);
    assert_eq!(site_of("x").path, "src/gone.rs");
    assert_eq!(site_of("x").blob, "");
}

// ---- the incremental update ------------------------------------------------

fn one_map(edges: &[(&str, &str)]) -> Registry<'static> {
    registry(vec![GraphMap::new("g", edges)])
}

fn names(adjacency: &MergedAdjacency) -> Vec<String> {
    adjacency.nodes().map(name_of).collect()
}

const WORLD: [(&str, &str); 4] = [("a", "h1"), ("b", "h2"), ("c", "h3"), ("d", "h4")];

#[tokio::test]
async fn an_update_asks_the_maps_only_about_the_files_whose_blob_changed() {
    let before = one_map(&[("a", "b"), ("b", "c"), ("c", "d"), ("a", "d")]);
    let chunks = files(&WORLD);
    let first = view_of(&chunks, "f1");
    let mut adjacency = build(&before, &first).await;
    assert_eq!(names(&adjacency), ["a", "b", "c", "d"]);
    assert_eq!(adjacency.stats().expanded, 4);
    assert_eq!(adjacency.stats().reused, 0);

    // c is edited, and its edges changed with it.
    let after = one_map(&[("a", "b"), ("b", "c"), ("c", "a"), ("a", "d")]);
    let edited = files(&[("a", "h1"), ("b", "h2"), ("c", "h3 edited"), ("d", "h4")]);
    let second = view_of(&edited, "f2");
    let stats = adjacency
        .update(&after, &second, &BuildOptions::default())
        .await;
    assert_eq!((stats.expanded, stats.reused), (1, 3), "{stats:?}");
    assert_eq!(
        (stats.blobs_kept, stats.blobs_added, stats.blobs_removed),
        (3, 1, 1)
    );
    assert_eq!(stats.full_rebuild, None);

    // It equals a build from nothing, row for row, and the auditor agrees.
    let fresh = build(&after, &second).await;
    assert_eq!(adjacency.canonical(), fresh.canonical());
    let nodes: Vec<NodeId> = adjacency.nodes().cloned().collect();
    audit_edge_sources(
        &PreMerged::new(&adjacency),
        &PreMerged::new(&fresh),
        &nodes,
        PER_NODE,
    )
    .await
    .unwrap();
    audit_edge_sources(
        &OnTheFly::new(&after, &second),
        &PreMerged::new(&adjacency),
        &nodes,
        PER_NODE,
    )
    .await
    .unwrap();

    // Nothing changed: nothing is asked, and nothing differs.
    let stats = adjacency
        .update(&after, &second, &BuildOptions::default())
        .await;
    assert_eq!((stats.expanded, stats.reused), (0, 4));
    assert_eq!(adjacency.canonical(), fresh.canonical());
}

#[tokio::test]
async fn a_row_is_reused_on_the_word_of_its_blobs_not_re_derived() {
    // a's file did not change, so the update trusts the maps' per-file
    // freshness: it keeps a's row although the map would answer otherwise.
    let before = one_map(&[("a", "b"), ("c", "d")]);
    let chunks = files(&WORLD);
    let mut adjacency = build(&before, &view_of(&chunks, "f1")).await;
    let lying = one_map(&[("a", "d"), ("c", "d")]);
    let edited = files(&[("a", "h1"), ("b", "h2"), ("c", "h3 edited"), ("d", "h4")]);
    adjacency
        .update(&lying, &view_of(&edited, "f2"), &BuildOptions::default())
        .await;
    let a = adjacency
        .canonical()
        .into_iter()
        .find(|row| row.starts_with("file:src/a.rs"))
        .unwrap();
    assert!(
        a.contains("file:src/b.rs") && !a.contains("-> file:src/d.rs"),
        "{a}"
    );
}

#[tokio::test]
async fn an_edge_site_in_another_file_ties_the_row_to_that_files_blob() {
    // Every edge was seen in c.rs, so a change to c asks again about every
    // node that has an edge, whichever file the node is in.
    let graph = || {
        registry(vec![
            GraphMap::new("g", &[("a", "b"), ("b", "d")]).site("src/c.rs"),
        ])
    };
    let chunks = files(&WORLD);
    let mut adjacency = build(&graph(), &view_of(&chunks, "f1")).await;
    let edited = files(&[("a", "h1"), ("b", "h2"), ("c", "h3 edited"), ("d", "h4")]);
    let stats = adjacency
        .update(&graph(), &view_of(&edited, "f2"), &BuildOptions::default())
        .await;
    // a and b have edges seen in c.rs; c is c's own; d is untouched.
    assert_eq!((stats.expanded, stats.reused), (3, 1), "{stats:?}");
    let fresh = build(&graph(), &view_of(&edited, "f2")).await;
    assert_eq!(adjacency.canonical(), fresh.canonical());
}

#[tokio::test]
async fn a_row_with_no_known_dependency_is_reused_only_when_nothing_changed() {
    // The site's file is not in the view, so the row has no blob to wait on.
    let graph = || registry(vec![GraphMap::new("g", &[("a", "b")]).site("src/gone.rs")]);
    let chunks = files(&WORLD);
    let mut adjacency = build(&graph(), &view_of(&chunks, "f1")).await;
    assert_eq!(
        adjacency.stats().undated,
        1,
        "only a's row has no dependency"
    );
    let same = adjacency
        .update(&graph(), &view_of(&chunks, "f1"), &BuildOptions::default())
        .await;
    assert_eq!(same.expanded, 0);
    let edited = files(&[("a", "h1"), ("b", "h2"), ("c", "h3"), ("d", "h4 edited")]);
    let changed = adjacency
        .update(&graph(), &view_of(&edited, "f2"), &BuildOptions::default())
        .await;
    // a's row (edge seen in a file the view lacks) and d's own; b and c wait
    // on nothing but themselves.
    assert_eq!(changed.expanded, 2, "{changed:?}");
}

#[tokio::test]
async fn a_new_map_set_or_a_moved_snapshot_with_a_global_map_asks_everything_again() {
    let chunks = files(&WORLD);
    let reg = one_map(&[("a", "b")]);
    let mut adjacency = build(&reg, &view_of(&chunks, "f1")).await;

    // A second map is registered.
    let more = two_maps();
    let stats = adjacency
        .update(&more, &view_of(&chunks, "f1"), &BuildOptions::default())
        .await;
    assert_eq!(stats.full_rebuild, Some("the map set changed"));
    assert_eq!(stats.reused, 0);
    assert!(stats.expanded >= 4);

    // A map whose version moved is a different map.
    let mut bumped = GraphMap::new("g", &[("a", "b")]);
    bumped.descriptor.version = 2;
    let stats = adjacency
        .update(
            &registry(vec![bumped]),
            &view_of(&chunks, "f1"),
            &BuildOptions::default(),
        )
        .await;
    assert_eq!(stats.full_rebuild, Some("the map set changed"));

    // A per-file map does not mind a new fingerprint when the files are equal.
    let reg = one_map(&[("a", "b")]);
    adjacency
        .update(&reg, &view_of(&chunks, "f1"), &BuildOptions::default())
        .await;
    let stats = adjacency
        .update(&reg, &view_of(&chunks, "f9"), &BuildOptions::default())
        .await;
    assert_eq!((stats.full_rebuild, stats.expanded), (None, 0));

    // A global map may answer otherwise for a file nobody touched.
    let mut global = GraphMap::new("g", &[("a", "b")]);
    global.descriptor.freshness = FreshnessModel::Global;
    let global = registry(vec![global]);
    adjacency
        .update(&global, &view_of(&chunks, "f9"), &BuildOptions::default())
        .await;
    let stats = adjacency
        .update(&global, &view_of(&chunks, "f10"), &BuildOptions::default())
        .await;
    assert_eq!(
        stats.full_rebuild,
        Some("the snapshot changed and a map is not per file")
    );
    assert_eq!(stats.reused, 0);
}

#[tokio::test]
async fn nodes_nothing_reaches_any_more_leave_the_adjacency() {
    let chunks = files(&WORLD);
    let mut adjacency = build(&one_map(&[("a", "b"), ("b", "x")]), &view_of(&chunks, "f1")).await;
    assert!(adjacency.covers(&node("x")));
    // b is edited and no longer reaches x; a's reused row still reaches b.
    let edited = files(&[("a", "h1"), ("b", "h2 edited"), ("c", "h3"), ("d", "h4")]);
    adjacency
        .update(
            &one_map(&[("a", "b")]),
            &view_of(&edited, "f2"),
            &BuildOptions::default(),
        )
        .await;
    assert!(!adjacency.covers(&node("x")));
    assert!(adjacency.covers(&node("b")));
}

// ---- failures and gaps -------------------------------------------------------

#[tokio::test]
async fn a_failed_call_leaves_an_incomplete_row_that_is_asked_again() {
    let chunks = files(&WORLD);
    let view = view_of(&chunks, "f1");
    let broken = registry(vec![
        GraphMap::new("good", &[("a", "b")]),
        GraphMap::new("bad", &[("a", "c")]).panics(),
    ]);
    let mut adjacency = build(&broken, &view).await;
    // Every file node asked the bad map and it panicked.
    assert_eq!(adjacency.stats().incomplete, 4);
    assert!(
        adjacency
            .failures()
            .iter()
            .all(|(_, map, why)| *map == "bad" && *why == "panicked")
    );
    let expansion = PreMerged::new(&adjacency)
        .edges(&node("a"), call_budget())
        .await;
    assert_eq!(expansion.edges.len(), 1, "the good map's edge is there");
    assert!(expansion.calls.iter().any(|c| c.failure.is_some()));

    // The map is back: nothing changed in the files, yet the rows are asked again.
    let healed = registry(vec![
        GraphMap::new("good", &[("a", "b")]),
        GraphMap::new("bad", &[("a", "c")]),
    ]);
    let stats = adjacency
        .update(&healed, &view, &BuildOptions::default())
        .await;
    assert_eq!(stats.incomplete, 0);
    assert!(stats.expanded >= 4, "{stats:?}");
    assert!(adjacency.failures().is_empty());
    audit_edge_sources(
        &OnTheFly::new(&healed, &view),
        &PreMerged::new(&adjacency),
        &["a", "b", "c", "d"].map(node),
        PER_NODE,
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn a_node_the_build_never_reached_says_so() {
    let chunks = files(&WORLD);
    let view = view_of(&chunks, "f1");
    let reg = one_map(&[("a", "b"), ("b", "c"), ("c", "d")]);

    // The cap: two rows, the rest is a visible gap, not silence.
    let options = BuildOptions {
        max_nodes: 2,
        ..BuildOptions::default()
    };
    let capped = MergedAdjacency::build_with(&reg, &view, &options).await;
    assert!(capped.stats().truncated);
    assert_eq!(capped.len(), 2);
    let missing = capped
        .nodes()
        .cloned()
        .chain(["a", "b", "c", "d"].map(node))
        .find(|n| !capped.covers(n))
        .unwrap();
    let source = PreMerged::new(&capped);
    let expansion = source.edges(&missing, call_budget()).await;
    assert!(expansion.edges.is_empty());
    assert_eq!(expansion.calls[0].map, GAP_MAP);
    assert!(expansion.calls[0].failure.is_some());

    // A kind no map accepts has no row and no gap, as on the fly.
    let literal = NodeId::StringLiteral {
        text_hash: "t".into(),
    };
    let expansion = source.edges(&literal, call_budget()).await;
    assert!(expansion.edges.is_empty() && expansion.calls.is_empty());

    // The caller's roots start a build from nodes the view does not hold.
    let options = BuildOptions {
        roots: vec![node("q")],
        ..BuildOptions::default()
    };
    let with_root = MergedAdjacency::build_with(&one_map(&[("q", "r")]), &view, &options).await;
    assert!(with_root.covers(&node("q")) && with_root.covers(&node("r")));
}
