//! `docs.mentions`: edges only to documentation files, labelled `Doc`, on a
//! small fixture tree; the stem of a file node; short and common names; the
//! query's labels and freshness; an edit after construction; the card.

use super::*;
use crate::{
    EdgeKind, MapStatus, Observed, SnapshotRef,
    edge::{Edge, WitnessClass},
    kit_c3::check_expand,
    scratch::Scratch,
};
use lci_core::node::{SymbolKey, SymbolKind};
use std::{path::Path, time::Instant};

fn write(root: &Path, path: &str, text: &str) {
    let full = root.join(path);
    std::fs::create_dir_all(full.parent().unwrap()).unwrap();
    std::fs::write(full, text).unwrap();
}

/// `compute_total` is defined in `src/a.rs` and named in a source comment,
/// a text file and the example config (none of them documentation). Two
/// documents mention it: `README.md` (whole word, line 1 col 6) and
/// `docs/guide.md` (only inside `compute_total_cached`, line 0 col 4).
fn fixture() -> Scratch {
    let scratch = Scratch::new("docs-mentions").unwrap();
    let root = scratch.path();
    write(
        root,
        "src/a.rs",
        "// compute_total\npub fn compute_total() {}\n",
    );
    write(root, "README.md", "Intro.\nCall `compute_total` to sum.\n");
    write(root, "docs/guide.md", "Use compute_total_cached here.\n");
    write(root, "notes.txt", "compute_total\n");
    write(root, "config.example.toml", "compute_total = 1\n");
    scratch
}

fn symbol(name: &str) -> NodeId {
    NodeId::Symbol(SymbolKey {
        language: "rust".into(),
        module_path: vec!["a".into()],
        container: Vec::new(),
        name: name.into(),
        kind: SymbolKind::Function,
        disambiguator: 0,
    })
}

fn file(path: &str) -> NodeId {
    NodeId::file(path).unwrap()
}

fn budget() -> Budget {
    Budget {
        deadline: Instant::now() + Duration::from_secs(3600),
        max_external_calls: u32::MAX,
        max_nodes_touched: u64::MAX,
    }
}

fn view() -> SnapshotView<'static> {
    SnapshotView::new(SnapshotRef::default(), Vec::new())
}

fn by_dst<'a>(edges: &'a [Edge], dst: &NodeId) -> &'a Edge {
    edges
        .iter()
        .find(|e| e.dst == *dst)
        .unwrap_or_else(|| panic!("no edge to {dst}: {edges:#?}"))
}

fn site_of(edge: &Edge) -> (&str, u32, u32) {
    let site = edge.site.as_ref().unwrap();
    (&site.path, site.line, site.col)
}

#[tokio::test]
async fn a_symbol_reaches_the_documents_that_mention_it_and_no_source_file() {
    let tree = fixture();
    let map = DocsMentions::patient(tree.path());
    let node = symbol("compute_total");
    let edges = check_expand(&map, &node, Duration::from_secs(10), &view())
        .await
        .expect("C7");

    assert_eq!(edges.len(), 2, "{edges:#?}");
    assert!(edges.iter().all(|e| {
        e.kind == EdgeKind::Mentions
            && e.map == DOCS_MENTIONS_ID
            && e.dst.kind() == NodeKind::File
            && e.dst
                .path()
                .is_some_and(|p| lci_core::path::has_extension_ignore_case(p, "md"))
    }));
    let readme = by_dst(&edges, &file("README.md"));
    assert_eq!(
        (readme.witness, readme.conf, site_of(readme)),
        (WitnessClass::Exact, 60, ("README.md", 1, 6))
    );
    assert!(readme.site.as_ref().unwrap().blob.starts_with("live:"));
    let guide = by_dst(&edges, &file("docs/guide.md"));
    assert_eq!(
        (guide.witness, guide.conf, site_of(guide)),
        (WitnessClass::Text, 20, ("docs/guide.md", 0, 4))
    );
    for other in ["src/a.rs", "notes.txt", "config.example.toml"] {
        assert!(edges.iter().all(|e| e.dst != file(other)), "{other}");
    }
    // Exact before text.
    assert_eq!(edges[0].dst, file("README.md"));
}

#[tokio::test]
async fn a_file_node_looks_for_its_stem_in_the_documents() {
    let tree = Scratch::new("docs-mentions-stem").unwrap();
    let root = tree.path();
    write(root, "src/parser.rs", "// the parser\npub fn run() {}\n");
    write(root, "README.md", "The `parser` turns text into a tree.\n");
    write(root, "docs/parser.md", "The parser, in detail.\n");
    let map = DocsMentions::patient(root);

    let from_source = map
        .expand(&file("src/parser.rs"), budget(), &view())
        .await
        .edges;
    let dsts: Vec<_> = from_source.iter().map(|e| e.dst.to_string()).collect();
    assert_eq!(dsts, ["file:README.md", "file:docs/parser.md"]);

    // A document seed does not point at itself.
    let from_doc = map
        .expand(&file("docs/parser.md"), budget(), &view())
        .await
        .edges;
    let dsts: Vec<_> = from_doc.iter().map(|e| e.dst.to_string()).collect();
    assert_eq!(dsts, ["file:README.md"]);

    let chunk = NodeId::chunk("src/parser.rs", "h", 0).unwrap();
    let from_chunk = map.expand(&chunk, budget(), &view()).await.edges;
    assert_eq!(from_chunk.len(), from_source.len());
    assert!(from_chunk.iter().all(|e| e.src == chunk));
}

#[tokio::test]
async fn short_and_common_names_make_no_edges() {
    let tree = Scratch::new("docs-mentions-short").unwrap();
    write(
        tree.path(),
        "README.md",
        "The lsp channel and the config file.\n",
    );
    let map = DocsMentions::patient(tree.path());
    for name in ["lsp", "config"] {
        let edges = map.expand(&symbol(name), budget(), &view()).await.edges;
        assert!(edges.is_empty(), "{name}");
    }
    assert_eq!(map.log().len(), 0, "no scan ran for either");
}

#[tokio::test]
async fn query_labels_evidence_doc_and_reports_the_live_tree() {
    let tree = fixture();
    write(
        tree.path(),
        "README.md",
        "Call `compute_total` and `other_name`.\n",
    );
    write(tree.path(), "docs/guide.md", "Use compute_total here.\n");
    write(tree.path(), "docs/more.md", "Only other_name here.\n");
    let map = DocsMentions::patient(tree.path());
    let seeds = [(symbol("compute_total"), 1.0), (symbol("other_name"), 1.0)];
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
    assert_eq!(response.status, MapStatus::Ok);
    assert_eq!(response.freshness.observed, Observed::LiveTree);
    assert!(response.cost.bytes_read > 0);
    // README is named by both seeds and appears once.
    let mut got: Vec<_> = response.hits.iter().map(|h| h.node.to_string()).collect();
    got.sort();
    assert_eq!(
        got,
        ["file:README.md", "file:docs/guide.md", "file:docs/more.md"]
    );
    assert!(
        response
            .hits
            .iter()
            .flat_map(|h| &h.evidence)
            .all(|e| e.label == SourceLabel::Doc && e.edge == EdgeKind::Mentions)
    );

    request.limit = 2;
    let limited = map.query(&request, &view()).await;
    assert_eq!(limited.hits.len(), 2);

    // A deadline that has passed is a partial answer, not a failure.
    request.budget.deadline = Instant::now() - Duration::from_secs(1);
    let late = map.query(&request, &view()).await;
    assert!(matches!(late.status, MapStatus::Partial { .. }), "{late:?}");
}

#[tokio::test]
async fn an_edit_after_construction_is_seen() {
    let tree = fixture();
    let map = DocsMentions::patient(tree.path());
    let node = symbol("compute_total");
    let before = map.expand(&node, budget(), &view()).await.edges;
    assert!(before.iter().all(|e| e.dst != file("docs/new.md")));
    let blob = by_dst(&before, &file("README.md"))
        .site
        .as_ref()
        .unwrap()
        .blob
        .clone();

    write(tree.path(), "docs/new.md", "Also `compute_total`.\n");
    write(
        tree.path(),
        "README.md",
        "Intro.\nCall `compute_total` to sum.\nAnd compute_total again.\n",
    );
    let after = map.expand(&node, budget(), &view()).await.edges;
    let new = by_dst(&after, &file("docs/new.md"));
    assert_eq!(
        (new.witness, site_of(new)),
        (WitnessClass::Exact, ("docs/new.md", 0, 6))
    );
    let readme = by_dst(&after, &file("README.md"));
    assert_eq!(readme.conf, 65, "the new occurrence counts");
    assert_ne!(readme.site.as_ref().unwrap().blob, blob, "new bytes");
}

#[tokio::test]
async fn the_card_reports_file_coverage_and_a_measured_median() {
    let tree = fixture();
    let map = DocsMentions::patient(tree.path());
    assert!(map.card().expand_p50_ms.is_none(), "nothing measured yet");
    let p50 = map.calibrate(1).await.expect("measured");
    assert!(p50 >= 0.0);
    let card = map.card();
    assert_eq!(card.expand_p50_ms, Some(p50));
    assert_eq!(card.coverage, vec![NodeKind::File]);
    assert_eq!(card.freshness_lag, Some(Duration::ZERO));
    assert!(!card.dependent);
    assert_eq!(
        card.good_for,
        vec![QuestionType::WhereTo, QuestionType::Impact]
    );
    let descriptor = map.describe();
    assert_eq!(descriptor.id, "docs.mentions");
    assert_eq!(descriptor.freshness, FreshnessModel::Live);
    assert_eq!(descriptor.emits, vec![NodeKind::File]);
    assert_eq!(descriptor.source_labels, vec![SourceLabel::Doc]);
    assert_eq!(map.log().len(), 1);
}
