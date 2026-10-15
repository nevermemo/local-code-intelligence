//! `search.lexical` and `search.vector`: edge kinds, witnesses and sites on an
//! in-memory index, own-chunk skipping, the embedding fallback and its
//! external call, caps, determinism, fail-open behaviour, and the S5
//! equivalence of the on-the-fly and pre-merged sources.

use super::*;
use crate::{
    Registry, SnapshotRef,
    audit::audit_edge_sources,
    kit_c3::check_expand,
    traverse::{
        merged::{BuildOptions, MergedAdjacency, PreMerged},
        source::{EdgeSource, OnTheFly},
    },
};
use lci_core::{
    chunk::Chunk,
    node::{SymbolKey, SymbolKind},
};
use std::{sync::atomic::AtomicBool, time::Duration};

const PER_NODE: Duration = Duration::from_secs(10);

/// (path, content hash, first line, last line, code, stored vector)
type Row = (
    &'static str,
    &'static str,
    u32,
    u32,
    &'static str,
    Option<[f32; 3]>,
);

/// alpha: the definition (chunk 1) and a doc comment (chunk 2); beta calls
/// it; gamma has the name in a comment; delta has only a longer name that
/// holds it; omega is unrelated and has no stored vector.
const ROWS: &[Row] = &[
    (
        "src/alpha.rs",
        "a1",
        1,
        5,
        "pub fn compute_total() -> u32 { 1 }",
        Some([1.0, 0.0, 0.0]),
    ),
    (
        "src/alpha.rs",
        "a2",
        6,
        9,
        "// alpha module docs",
        Some([0.0, 1.0, 0.0]),
    ),
    (
        "src/beta.rs",
        "b1",
        1,
        6,
        "use alpha; fn caller() { alpha::compute_total(); }",
        Some([0.9, 0.1, 0.0]),
    ),
    (
        "src/gamma.rs",
        "g1",
        1,
        3,
        "// see compute_total here",
        Some([0.8, 0.2, 0.0]),
    ),
    (
        "src/delta.rs",
        "d1",
        1,
        4,
        "fn recompute_totals() {}",
        Some([0.0, 0.0, 1.0]),
    ),
    ("src/omega.rs", "o1", 1, 2, "fn other() {}", None),
];

fn chunk_node(path: &str, hash: &str) -> NodeId {
    NodeId::chunk(path, hash, 0).unwrap()
}

fn hit_of(row: &Row, score: f32) -> ChunkHit {
    ChunkHit {
        node: chunk_node(row.0, row.1),
        path: row.0.into(),
        start_line: row.2,
        end_line: row.3,
        code: row.4.into(),
        score,
    }
}

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm = |v: &[f32]| v.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (norm(a) * norm(b))
}

/// The fixture index with switches and counters.
#[derive(Default)]
struct Mem {
    lexical_calls: AtomicU64,
    embed_calls: AtomicU64,
    nearest_calls: AtomicU64,
    fail_lexical: AtomicBool,
    fail_stored: AtomicBool,
    fail_embed: AtomicBool,
    fail_nearest: AtomicBool,
    /// The vector `embed` answers with.
    embedding: [f32; 3],
}

impl Mem {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            embedding: [0.0, 0.0, 1.0],
            ..Self::default()
        })
    }
}

fn down(flag: &AtomicBool) -> Result<(), String> {
    if flag.load(Ordering::Relaxed) {
        Err("it is down".into())
    } else {
        Ok(())
    }
}

#[crate::async_trait]
impl ChunkSearch for Mem {
    async fn lexical(&self, query: &str, limit: usize) -> Result<Vec<ChunkHit>, String> {
        self.lexical_calls.fetch_add(1, Ordering::Relaxed);
        down(&self.fail_lexical)?;
        let mut hits: Vec<ChunkHit> = ROWS
            .iter()
            .filter(|row| row.4.contains(query) || row.0.contains(query))
            .map(|row| hit_of(row, row.4.matches(query).count() as f32 + 1.0))
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
        hits.truncate(limit);
        Ok(hits)
    }

    async fn chunk_at(&self, path: &str, line: Option<u32>) -> Result<Option<ChunkHit>, String> {
        Ok(ROWS
            .iter()
            .find(|row| row.0 == path && line.is_none_or(|line| (row.2..=row.3).contains(&line)))
            .map(|row| hit_of(row, 0.0)))
    }

    async fn stored_vector(&self, chunk: &NodeId) -> Result<Option<Vec<f32>>, String> {
        down(&self.fail_stored)?;
        Ok(ROWS
            .iter()
            .find(|row| chunk_node(row.0, row.1) == *chunk)
            .and_then(|row| row.5)
            .map(|v| v.to_vec()))
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, String> {
        self.embed_calls.fetch_add(1, Ordering::Relaxed);
        down(&self.fail_embed)?;
        Ok(self.embedding.to_vec())
    }

    async fn nearest(&self, vector: &[f32], limit: usize) -> Result<Vec<ChunkHit>, String> {
        self.nearest_calls.fetch_add(1, Ordering::Relaxed);
        down(&self.fail_nearest)?;
        let mut hits: Vec<ChunkHit> = ROWS
            .iter()
            .filter_map(|row| Some(hit_of(row, cosine(vector, &row.5?))))
            .collect();
        hits.sort_by(|a, b| b.score.total_cmp(&a.score).then(a.node.cmp(&b.node)));
        hits.truncate(limit);
        Ok(hits)
    }

    async fn chunk_lines(&self) -> Result<Vec<(NodeId, (u32, u32))>, String> {
        Ok(ROWS
            .iter()
            .map(|row| (chunk_node(row.0, row.1), (row.2, row.3)))
            .collect())
    }
}

struct Decls(HashMap<NodeId, DeclSite>);

impl DeclLocator for Decls {
    fn site_of(&self, node: &NodeId) -> Option<DeclSite> {
        self.0.get(node).cloned()
    }
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

fn compute_total() -> NodeId {
    symbol("compute_total", "alpha")
}

fn decls() -> Arc<dyn DeclLocator> {
    Arc::new(Decls(
        [(
            compute_total(),
            DeclSite {
                path: "src/alpha.rs".into(),
                line: 0,
                span: (0, 4),
            },
        )]
        .into_iter()
        .collect(),
    ))
}

fn lexical(mem: &Arc<Mem>) -> LexicalChunks<'static> {
    LexicalChunks::new(mem.clone(), Some(decls()))
}

fn vector(mem: &Arc<Mem>) -> VectorChunks<'static> {
    VectorChunks::new(mem.clone(), Some(decls()))
}

fn view() -> SnapshotView<'static> {
    SnapshotView::new(SnapshotRef::default(), Vec::new())
}

fn budget(external_calls: u32) -> Budget {
    Budget {
        deadline: Instant::now() + PER_NODE,
        max_external_calls: external_calls,
        max_nodes_touched: u64::MAX,
    }
}

async fn expand(map: &dyn Map, node: &NodeId) -> Vec<Edge> {
    check_expand(map, node, PER_NODE, &view())
        .await
        .expect("C7")
}

fn dsts(edges: &[Edge]) -> Vec<NodeId> {
    edges.iter().map(|e| e.dst.clone()).collect()
}

fn file(path: &str) -> NodeId {
    NodeId::file(path).unwrap()
}

// ---- search.lexical -------------------------------------------------------------

#[tokio::test]
async fn lexical_from_a_symbol_offers_the_other_chunks_that_hold_the_name() {
    let mem = Mem::new();
    let map = lexical(&mem);
    let edges = expand(&map, &compute_total()).await;
    // beta and gamma hold the name whole; delta holds a longer word; alpha's
    // own definition chunk is skipped.
    let got = dsts(&edges);
    assert_eq!(
        got.iter().collect::<BTreeSet<_>>(),
        [
            chunk_node("src/beta.rs", "b1"),
            chunk_node("src/gamma.rs", "g1"),
            chunk_node("src/delta.rs", "d1"),
        ]
        .iter()
        .collect()
    );
    for edge in &edges {
        assert_eq!(edge.kind, EdgeKind::QueryMatch);
        assert_eq!(edge.map, SEARCH_LEXICAL_ID);
        assert_eq!(edge.src, compute_total());
        let site = edge.site.as_ref().unwrap();
        assert_eq!((site.line, site.col), (0, 0), "first line of the chunk");
        assert!(site.blob.is_empty());
        assert_eq!(
            site.path,
            match &edge.dst {
                NodeId::Chunk { path, .. } => path.as_str(),
                _ => unreachable!(),
            }
        );
    }
    let cost = map.expand(&compute_total(), budget(10), &view()).await.cost;
    assert_eq!(cost.external_calls, 0, "ripgrep is not an external call");
    let by = |hash: &str| {
        edges
            .iter()
            .find(|e| matches!(&e.dst, NodeId::Chunk { content_hash, .. } if content_hash == hash))
            .unwrap()
    };
    assert_eq!(by("b1").witness, WitnessClass::Exact);
    assert_eq!(by("g1").witness, WitnessClass::Exact);
    assert_eq!(by("d1").witness, WitnessClass::Text);
    assert!(by("b1").conf > by("d1").conf, "exact outranks text");
    assert!(by("g1").conf > by("d1").conf);
}

#[tokio::test]
async fn lexical_skips_the_nodes_own_chunks() {
    let mem = Mem::new();
    let map = lexical(&mem);
    // A file: all of its chunks. alpha.rs's second chunk holds the stem.
    let edges = expand(&map, &file("src/alpha.rs")).await;
    assert_eq!(dsts(&edges), vec![chunk_node("src/beta.rs", "b1")]);
    // A chunk: itself only; the file's other chunk may be offered.
    let edges = expand(&map, &chunk_node("src/alpha.rs", "a1")).await;
    assert_eq!(
        dsts(&edges).into_iter().collect::<BTreeSet<_>>(),
        [
            chunk_node("src/alpha.rs", "a2"),
            chunk_node("src/beta.rs", "b1")
        ]
        .into_iter()
        .collect()
    );
    // Without a locator the definition's chunk is not known to be its own.
    let blind = LexicalChunks::new(mem.clone(), None);
    let edges = expand(&blind, &compute_total()).await;
    assert!(dsts(&edges).contains(&chunk_node("src/alpha.rs", "a1")));
}

#[tokio::test]
async fn lexical_is_capped_by_k_and_deterministic() {
    let mem = Mem::new();
    let options = SearchMapOptions {
        k: 2,
        ..SearchMapOptions::default()
    };
    let map = LexicalChunks::with_options(mem.clone(), Some(decls()), options);
    let first = expand(&map, &compute_total()).await;
    assert_eq!(first.len(), 2);
    assert_eq!(shape(&first), shape(&expand(&map, &compute_total()).await));
    // k never exceeds the engine's expansion limit.
    let huge = LexicalChunks::with_options(
        mem.clone(),
        None,
        SearchMapOptions {
            k: usize::MAX,
            ..SearchMapOptions::default()
        },
    );
    assert_eq!(huge.options().k, EXPAND_LIMIT);
}

#[tokio::test]
async fn lexical_asks_nothing_for_names_that_are_noise() {
    let mem = Mem::new();
    let map = lexical(&mem);
    for node in [
        symbol("run", "alpha"),
        symbol("new", "alpha"),
        file("src/a.rs"),
        NodeId::Commit {
            sha: "c".repeat(40),
        },
    ] {
        assert!(expand(&map, &node).await.is_empty(), "{node}");
    }
    assert_eq!(mem.lexical_calls.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn lexical_fails_open_and_records_the_error() {
    let mem = Mem::new();
    mem.fail_lexical.store(true, Ordering::Relaxed);
    let map = lexical(&mem);
    assert!(expand(&map, &compute_total()).await.is_empty());
    assert_eq!(map.failures().count(), 1);
    assert!(
        map.failures()
            .first()
            .unwrap()
            .starts_with("search.lexical: ")
    );
    assert!(
        map.card().expand_p50_ms.is_none(),
        "a failed call is no cost sample"
    );

    // A seeded query says so: Failed when it found nothing.
    let seeds = [(compute_total(), 1.0)];
    let params = serde_json::Value::Null;
    let request = MapRequest {
        text: None,
        seeds: &seeds,
        limit: 10,
        budget: budget(u32::MAX),
        params: &params,
        as_of: None,
    };
    let response = map.query(&request, &view()).await;
    assert!(response.hits.is_empty());
    assert!(matches!(&response.status, MapStatus::Failed { warning } if warning.contains("down")));

    // Back up: it answers, and the status is Ok.
    mem.fail_lexical.store(false, Ordering::Relaxed);
    let response = map.query(&request, &view()).await;
    assert_eq!(response.status, MapStatus::Ok);
    assert_eq!(response.hits.len(), 3);
    assert_eq!(response.freshness.observed, Observed::LiveTree);
    assert!(map.card().expand_p50_ms.is_some());
}

#[test]
fn whole_identifiers_are_told_from_pieces() {
    assert!(holds_whole("a compute_total(b)", "compute_total"));
    assert!(holds_whole("compute_total", "compute_total"));
    assert!(!holds_whole("recompute_totals()", "compute_total"));
    assert!(!holds_whole("compute_total2", "compute_total"));
    assert!(holds_whole(
        "x_compute_total compute_total",
        "compute_total"
    ));
}

// ---- search.vector --------------------------------------------------------------

#[tokio::test]
async fn vector_uses_the_stored_embedding_and_makes_no_embed_call() {
    let mem = Mem::new();
    let map = vector(&mem);
    let edges = expand(&map, &compute_total()).await;
    assert_eq!(mem.embed_calls.load(Ordering::Relaxed), 0);
    assert_eq!(mem.nearest_calls.load(Ordering::Relaxed), 1);
    // Nearest first; alpha's own chunk (cosine 1) is skipped.
    assert_eq!(
        dsts(&edges)[..3],
        [
            chunk_node("src/beta.rs", "b1"),
            chunk_node("src/gamma.rs", "g1"),
            chunk_node("src/alpha.rs", "a2"),
        ]
    );
    assert!(!dsts(&edges).contains(&chunk_node("src/alpha.rs", "a1")));
    for edge in &edges {
        assert_eq!(edge.kind, EdgeKind::Near);
        assert_eq!(edge.map, SEARCH_VECTOR_ID);
        assert_eq!(edge.witness, WitnessClass::Heuristic);
        assert!(edge.site.as_ref().unwrap().blob.is_empty());
    }
    let cost = map.expand(&compute_total(), budget(10), &view()).await.cost;
    assert_eq!(cost.external_calls, 0, "a stored vector needs no embedding");
    // conf is the cosine in whole percent: 0.9 / sqrt(0.82) = 0.9939.
    assert_eq!(edges[0].conf, 99);
    assert_eq!(edges[1].conf, 97);
    assert_eq!(edges[2].conf, 0);
    assert_eq!(edges[0].site.as_ref().unwrap().path, "src/beta.rs");
}

#[tokio::test]
async fn vector_embeds_once_when_no_vector_is_stored_and_counts_the_call() {
    let mem = Mem::new();
    let map = vector(&mem);
    // omega's only chunk has no stored vector.
    let edges = expand(&map, &file("src/omega.rs")).await;
    assert_eq!(mem.embed_calls.load(Ordering::Relaxed), 1);
    assert!(!edges.is_empty());
    // One embedding call, reported once beside the edges.
    let again = map.expand(&file("src/omega.rs"), budget(10), &view()).await;
    assert_eq!(again.cost.external_calls, 1);
    // The fixture embeds to [0, 0, 1]: delta is the nearest.
    assert_eq!(edges[0].dst, chunk_node("src/delta.rs", "d1"));
    assert_eq!(edges[0].conf, 100);

    // A chunk node with no stored vector is embedded by its name; no budget
    // left for an external call: no edges and no call.
    let mem = Mem::new();
    let map = vector(&mem);
    let none = map
        .expand(&file("src/omega.rs"), budget(0), &view())
        .await
        .edges;
    assert!(none.is_empty());
    assert_eq!(mem.embed_calls.load(Ordering::Relaxed), 0);
    // The same budget does not stop a stored vector from being used.
    let some = map.expand(&compute_total(), budget(0), &view()).await.edges;
    assert!(!some.is_empty());
}

#[tokio::test]
async fn vector_embeds_the_name_of_a_node_the_index_does_not_hold() {
    let mem = Mem::new();
    let map = VectorChunks::new(mem.clone(), None);
    let edges = expand(&map, &compute_total()).await;
    assert_eq!(mem.embed_calls.load(Ordering::Relaxed), 1);
    assert!(!edges.is_empty());
    let again = map.expand(&compute_total(), budget(10), &view()).await;
    assert_eq!(again.cost.external_calls, 1);
    // Nothing to embed for a node that names nothing.
    let commit = NodeId::Commit {
        sha: "d".repeat(40),
    };
    assert!(expand(&map, &commit).await.is_empty());
    assert_eq!(
        mem.embed_calls.load(Ordering::Relaxed),
        2,
        "the commit embeds nothing"
    );
}

#[tokio::test]
async fn vector_from_a_chunk_skips_only_that_chunk() {
    let mem = Mem::new();
    let map = vector(&mem);
    let edges = expand(&map, &chunk_node("src/beta.rs", "b1")).await;
    let got = dsts(&edges);
    assert!(!got.contains(&chunk_node("src/beta.rs", "b1")));
    assert_eq!(got[0], chunk_node("src/alpha.rs", "a1"));
}

#[tokio::test]
async fn vector_is_capped_by_k() {
    let mem = Mem::new();
    let map = VectorChunks::with_options(
        mem.clone(),
        Some(decls()),
        SearchMapOptions {
            k: 2,
            ..SearchMapOptions::default()
        },
    );
    let edges = expand(&map, &compute_total()).await;
    assert_eq!(edges.len(), 2);
    assert_eq!(shape(&edges), shape(&expand(&map, &compute_total()).await));
}

#[tokio::test]
async fn vector_fails_open_when_any_step_is_down() {
    for step in ["stored", "embed", "nearest"] {
        let mem = Mem::new();
        let map = vector(&mem);
        let node = if step == "embed" {
            file("src/omega.rs")
        } else {
            compute_total()
        };
        let flag = match step {
            "stored" => &mem.fail_stored,
            "embed" => &mem.fail_embed,
            _ => &mem.fail_nearest,
        };
        flag.store(true, Ordering::Relaxed);
        assert!(expand(&map, &node).await.is_empty(), "{step}");
        assert_eq!(map.failures().count(), 1, "{step}");
        assert!(
            map.failures()
                .first()
                .unwrap()
                .starts_with("search.vector: "),
            "{step}"
        );
        let seeds = [(node.clone(), 1.0)];
        let params = serde_json::Value::Null;
        let response = map
            .query(
                &MapRequest {
                    text: None,
                    seeds: &seeds,
                    limit: 10,
                    budget: budget(u32::MAX),
                    params: &params,
                    as_of: None,
                },
                &view(),
            )
            .await;
        assert!(
            matches!(response.status, MapStatus::Failed { .. }),
            "{step}"
        );
        // Other seeds still answer: the status is Partial with the warning.
        flag.store(false, Ordering::Relaxed);
        let seeds = [(node.clone(), 1.0), (chunk_node("src/beta.rs", "b1"), 1.0)];
        mem.fail_nearest.store(false, Ordering::Relaxed);
        let ok = map
            .query(
                &MapRequest {
                    text: None,
                    seeds: &seeds,
                    limit: 10,
                    budget: budget(u32::MAX),
                    params: &params,
                    as_of: None,
                },
                &view(),
            )
            .await;
        assert_eq!(ok.status, MapStatus::Ok, "{step}");
        assert!(!ok.hits.is_empty());
    }
}

#[tokio::test]
async fn a_query_reports_the_embedding_call_it_made() {
    let mem = Mem::new();
    let map = vector(&mem);
    let seeds = [(file("src/omega.rs"), 1.0)];
    let params = serde_json::Value::Null;
    let response = map
        .query(
            &MapRequest {
                text: None,
                seeds: &seeds,
                limit: 3,
                budget: budget(5),
                params: &params,
                as_of: None,
            },
            &view(),
        )
        .await;
    assert_eq!(response.hits.len(), 3);
    assert_eq!(response.cost.external_calls, 1);
    assert_eq!(response.freshness.observed, Observed::Snapshot);
}

// ---- cards -------------------------------------------------------------------------

#[tokio::test]
async fn cards_report_the_measured_median_and_the_freshness_model() {
    let mem = Mem::new();
    let (lex, vec) = (lexical(&mem), vector(&mem));
    assert!(lex.card().expand_p50_ms.is_none());
    assert!(vec.card().expand_p50_ms.is_none());
    expand(&lex, &compute_total()).await;
    expand(&vec, &compute_total()).await;
    assert!(lex.card().expand_p50_ms.is_some());
    assert!(vec.card().expand_p50_ms.is_some());
    assert_eq!(lex.describe().freshness, FreshnessModel::Live);
    assert_eq!(vec.describe().freshness, FreshnessModel::PerFile);
    assert_eq!(lex.card().coverage, vec![NodeKind::Chunk]);
    assert!(lex.card().good_for.contains(&QuestionType::WhereTo));
    assert!(vec.card().good_for.contains(&QuestionType::WhereTo));
    assert_eq!(lex.describe().needs, vec![Need::Ripgrep]);
    assert_eq!(vec.describe().needs, vec![Need::Embedding]);
}

#[tokio::test]
async fn scip_decls_find_the_nodes_of_a_scip_graph() {
    // An empty map has no declarations.
    let decls = ScipDecls::default();
    assert!(decls.is_empty());
    assert_eq!(decls.len(), 0);
    assert!(decls.site_of(&compute_total()).is_none());
}

// ---- S5: pre-merged == on the fly ---------------------------------------------------

fn view_chunks() -> Vec<Chunk> {
    ROWS.iter()
        .map(|row| Chunk {
            relative_file_path: row.0.into(),
            language: "rust".into(),
            start_line: row.2,
            end_line: row.3,
            code: row.4.into(),
            content_hash: row.1.into(),
        })
        .collect()
}

async fn audit(map: Box<dyn Map + 'static>, id: MapId, roots: Vec<NodeId>) {
    let mut registry = Registry::new(60.0);
    registry.register(map, 1.0, 40).unwrap();
    let chunks = view_chunks();
    let view = SnapshotView::new(
        SnapshotRef {
            indexed_at: "t".into(),
            fingerprint: "f1".into(),
        },
        chunks.iter().collect(),
    );
    let adjacency = MergedAdjacency::build_with(
        &registry,
        &view,
        &BuildOptions {
            roots,
            ..BuildOptions::default()
        },
    )
    .await;
    // The build followed the edges to the chunks they name.
    assert!(adjacency.covers(&chunk_node("src/beta.rs", "b1")));
    let mut nodes: Vec<NodeId> = adjacency.nodes().cloned().collect();
    nodes.push(NodeId::Commit {
        sha: "b".repeat(40),
    });
    let mut fly = OnTheFly::new(&registry, &view);
    let mut merged = PreMerged::new(&adjacency);
    let report = audit_edge_sources(&fly, &merged, &nodes, PER_NODE)
        .await
        .expect("equal on every node");
    assert_eq!(report.nodes, nodes.len());
    assert!(report.edges >= 6, "{report:?}");
    assert!(fly.set_enabled(id, false) && merged.set_enabled(id, false));
    audit_edge_sources(&fly, &merged, &nodes, PER_NODE)
        .await
        .expect("equal with the map off");
}

#[tokio::test]
async fn lexical_pre_merged_equals_on_the_fly() {
    let mem = Mem::new();
    audit(
        Box::new(lexical(&mem)),
        SEARCH_LEXICAL_ID,
        vec![compute_total(), file("src/alpha.rs")],
    )
    .await;
}

#[tokio::test]
async fn vector_pre_merged_equals_on_the_fly() {
    let mem = Mem::new();
    audit(
        Box::new(vector(&mem)),
        SEARCH_VECTOR_ID,
        vec![compute_total(), chunk_node("src/beta.rs", "b1")],
    )
    .await;
}

/// What an edge says, without its timing.
fn shape(edges: &[Edge]) -> Vec<(NodeId, u8, Option<Site>)> {
    edges
        .iter()
        .map(|e| (e.dst.clone(), e.conf, e.site.clone()))
        .collect()
}
