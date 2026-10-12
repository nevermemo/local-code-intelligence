use super::*;
use crate::{
    Budget, Registry,
    kit::{FixedMap, Fixture, check_determinism, check_fail_open},
    scratch::Scratch,
};
use lci_core::chunk::{Scan, scan_incremental};
use std::{path::Path, time::Duration};

const LEXICAL: &str = "\
pub struct LexicalMap;

pub trait Map {
    fn query(&self) -> u8;
}

pub trait Other {
    fn query(&self) -> u8;
}

impl Map for LexicalMap {
    fn query(&self) -> u8 { 1 }
}

impl Other for LexicalMap {
    fn query(&self) -> u8 { 2 }
}

pub const QUERY: u8 = 3;
";

fn write(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

struct Workspace {
    root: Scratch,
    data: Scratch,
    scan: Scan,
    snapshot: SnapshotRef,
}

impl Workspace {
    fn new(files: &[(&str, &str)]) -> Self {
        let root = Scratch::new("symdefs-ws").unwrap();
        for (name, content) in files {
            write(root.path(), name, content);
        }
        let scan = scan_incremental(root.path(), &HashMap::new()).unwrap();
        let snapshot = SnapshotRef {
            indexed_at: "t1".into(),
            fingerprint: scan.fingerprint.clone(),
        };
        Self {
            root,
            data: Scratch::new("symdefs-data").unwrap(),
            scan,
            snapshot,
        }
    }

    fn store(&self) -> BundleStore {
        BundleStore::new(self.data.path(), &"ab".repeat(32))
    }

    fn target(&self) -> Target<'_> {
        Target {
            snapshot: &self.snapshot,
            root: self.root.path(),
            files: &self.scan.files,
        }
    }

    fn built(&self) -> SymDefs {
        let mut map = SymDefs::new(self.store());
        map.build(&self.target()).unwrap();
        map
    }

    fn view(&self) -> SnapshotView<'_> {
        SnapshotView::new(self.snapshot.clone(), self.scan.chunks.iter().collect())
    }
}

fn request<'r>(text: &'r str, limit: usize, params: &'r serde_json::Value) -> MapRequest<'r> {
    MapRequest {
        text: Some(text),
        seeds: &[],
        limit,
        budget: Budget {
            deadline: Instant::now() + Duration::from_secs(5),
            max_external_calls: 0,
            max_nodes_touched: u64::MAX,
        },
        params,
        as_of: None,
    }
}

fn details(response: &MapResponse) -> Vec<String> {
    response
        .hits
        .iter()
        .map(|hit| hit.evidence[0].detail.clone().unwrap())
        .collect()
}

#[tokio::test]
async fn exact_names_rank_before_case_insensitive_ones_and_impl_headers_keep_methods_apart() {
    let workspace = Workspace::new(&[("src/maps/lexical.rs", LEXICAL)]);
    let map = workspace.built();
    let view = workspace.view();
    let params = serde_json::Value::Null;
    let response = map
        .query(&request("where is query?", 40, &params), &view)
        .await;
    assert_eq!(response.status, MapStatus::Ok);
    assert_eq!(
        details(&response),
        [
            "sym:rust:maps::lexical::impl Map for LexicalMap::query/method",
            "sym:rust:maps::lexical::impl Other for LexicalMap::query/method",
            "sym:rust:maps::lexical::trait Map::query/method",
            "sym:rust:maps::lexical::trait Other::query/method",
            "sym:rust:maps::lexical::QUERY/const",
        ]
    );
    let ranks: Vec<u32> = response.hits.iter().map(|hit| hit.rank).collect();
    assert_eq!(ranks, [1, 2, 3, 4, 5]);
    for hit in &response.hits {
        assert!(view.contains(&hit.node), "{}", hit.node);
        assert_eq!(hit.score, None);
        assert_eq!(hit.evidence.len(), 1);
        assert_eq!(hit.evidence[0].label, SourceLabel::Tags);
        assert_eq!(hit.evidence[0].edge, EdgeKind::QueryMatch);
        // The head chunk holds the declaration it is listed for.
        let chunk = view.chunk(&hit.node).unwrap();
        assert!(chunk.code.contains("query") || chunk.code.contains("QUERY"));
    }
    // Each method is its own chunk's head: impl Map's and impl Other's
    // methods are two hits, not one.
    let distinct: HashSet<&NodeId> = response.hits.iter().map(|hit| &hit.node).collect();
    assert_eq!(distinct.len(), 5);
    assert_eq!(response.freshness.model, FreshnessModel::PerFile);
    assert_eq!(response.freshness.snapshot, workspace.snapshot);
    assert!(!response.freshness.stale);

    // The limit cuts the list; a query naming nothing answers nothing.
    let two = map.query(&request("query", 2, &params), &view).await;
    assert_eq!(two.hits, response.hits[..2]);
    let none = map
        .query(&request("nothing here", 40, &params), &view)
        .await;
    assert_eq!(none.status, MapStatus::Ok);
    assert!(none.hits.is_empty());
}

fn chunk(path: &str, start_line: u32, end_line: u32, hash: &str) -> Chunk {
    Chunk {
        relative_file_path: path.into(),
        language: "rust".into(),
        start_line,
        end_line,
        code: format!("code {hash}"),
        content_hash: hash.into(),
    }
}

fn stored(name: &str, kind: &str, start_line: u32) -> StoredDeclaration {
    StoredDeclaration {
        inline_modules: vec![],
        container: vec![],
        name: name.into(),
        kind: kind.into(),
        disambiguator: 0,
        start_line,
        end_line: start_line,
    }
}

#[test]
fn the_head_chunk_holds_the_first_line_and_a_chunk_is_listed_once() {
    let chunks = vec![
        chunk("src/a.rs", 1, 10, "h1"),
        chunk("src/a.rs", 10, 20, "h2"),
        chunk("src/a.rs", 22, 30, "h1"),
    ];
    let files: HashMap<String, CachedFile> = [(
        "src/a.rs".to_owned(),
        CachedFile {
            language: "rust".into(),
            adapter_version: "v".into(),
            content_hash: "file".into(),
            chunks: chunks.clone(),
        },
    )]
    .into();
    let payloads: BTreeMap<String, Vec<StoredDeclaration>> = [(
        "src/a.rs".to_owned(),
        vec![
            stored("alpha", "fn", 5),
            // Line 10 is in both of the first two chunks: the one starting
            // later holds it.
            stored("beta", "fn", 10),
            stored("Beta", "struct", 12),
            // In no chunk.
            stored("beta", "const", 21),
            stored("gamma", "fn", 25),
            stored("delta", "nonsense-kind", 25),
        ],
    )]
    .into();
    let index = SymIndex::join(SnapshotRef::default(), &files, &payloads);
    let heads: Vec<String> = index
        .entries()
        .iter()
        .map(|entry| {
            format!(
                "{} {}",
                entry.name,
                entry.head.as_ref().map_or("-".into(), |h| h.to_string())
            )
        })
        .collect();
    // Sorted by symbol; the unknown kind is dropped.
    assert_eq!(
        heads,
        [
            "Beta chunk:src/a.rs@h2#0",
            "alpha chunk:src/a.rs@h1#0",
            "beta -",
            "beta chunk:src/a.rs@h2#0",
            "gamma chunk:src/a.rs@h1#1",
        ]
    );
    assert_eq!(
        index.canonical(),
        [
            "sym:rust:a::Beta/struct\tchunk:src/a.rs@h2#0",
            "sym:rust:a::alpha/fn\tchunk:src/a.rs@h1#0",
            "sym:rust:a::beta/const\t-",
            "sym:rust:a::beta/fn\tchunk:src/a.rs@h2#0",
            "sym:rust:a::gamma/fn\tchunk:src/a.rs@h1#1",
        ]
    );
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    // "beta" names three declarations: the fn (exact) and the struct
    // (case-insensitive) share chunk h2, listed once at the fn's rank; the
    // const has no head chunk and is left out.
    let hits = answer(&index, "beta gamma", 40, &view);
    let listed: Vec<(String, u32)> = hits
        .iter()
        .map(|hit| (hit.node.to_string(), hit.rank))
        .collect();
    assert_eq!(
        listed,
        [
            ("chunk:src/a.rs@h2#0".to_owned(), 1),
            ("chunk:src/a.rs@h1#1".to_owned(), 2),
        ]
    );
    assert_eq!(
        hits[0].evidence[0].detail.as_deref(),
        Some("sym:rust:a::beta/fn")
    );
    // A chunk outside the view is never listed.
    let narrow = SnapshotView::new(SnapshotRef::default(), vec![&chunks[0]]);
    assert!(answer(&index, "beta gamma", 40, &narrow).is_empty());
}

#[tokio::test]
async fn a_map_built_for_another_snapshot_says_it_is_stale_and_one_never_built_fails() {
    let workspace = Workspace::new(&[("src/maps/lexical.rs", LEXICAL)]);
    let map = workspace.built();
    let newer = SnapshotRef {
        indexed_at: "t2".into(),
        fingerprint: "other".into(),
    };
    let view = SnapshotView::new(newer, workspace.scan.chunks.iter().collect());
    let params = serde_json::Value::Null;
    let response = map.query(&request("query", 40, &params), &view).await;
    assert_eq!(response.status, MapStatus::Ok);
    assert!(response.freshness.stale);
    assert_eq!(response.freshness.snapshot, workspace.snapshot);
    assert_eq!(response.hits.len(), 5);

    let never = SymDefs::new(workspace.store());
    let response = never.query(&request("query", 40, &params), &view).await;
    assert!(
        matches!(response.status, MapStatus::Failed { .. }),
        "{response:?}"
    );
    assert!(response.hits.is_empty());
}

#[tokio::test]
async fn sym_defs_passes_contracts_c1_and_c6_and_fuses_in_a_registry() {
    let workspace = Workspace::new(&[
        ("src/maps/lexical.rs", LEXICAL),
        ("src/other.rs", "pub fn unrelated() {}\n"),
    ]);
    let view = workspace.view();
    let control_hits: Vec<MapHit> = view
        .ids()
        .iter()
        .enumerate()
        .map(|(rank, node)| MapHit {
            node: node.clone(),
            rank: rank as u32 + 1,
            score: None,
            evidence: vec![],
        })
        .collect();
    let control = || -> Box<dyn Map> {
        Box::new(FixedMap {
            descriptor: MapDescriptor {
                id: "control",
                ..descriptor()
            },
            hits: control_hits.clone(),
        })
    };
    let subject = || -> Box<dyn Map> { Box::new(workspace.built()) };
    let fixture = Fixture {
        view: &view,
        text: "query LexicalMap",
        budget: Duration::from_millis(500),
        slack: Duration::from_millis(500),
        rrf_k: 60.0,
    };
    check_fail_open(&subject, &control, &fixture).await.unwrap();
    check_determinism(&subject, &control, &fixture)
        .await
        .unwrap();

    // Registered beside another rank list, its hits fuse with weight 1.
    let mut registry = Registry::new(60.0);
    registry.register(subject(), 1.0, 40).unwrap();
    registry.register(control(), 1.0, 40).unwrap();
    assert_eq!(registry.ids(), ["control", ID]);
    let run = registry
        .run(Some("LexicalMap"), Duration::from_secs(5), &view)
        .await;
    let call = run.calls.iter().find(|call| call.id == ID).unwrap();
    assert_eq!(call.response.status, MapStatus::Ok);
    assert_eq!(call.response.hits.len(), 1);
    let top = &call.response.hits[0].node;
    let fused = run.fused.iter().find(|fused| &fused.node == top).unwrap();
    assert_eq!(fused.parts.len(), 2);
    assert!(
        fused
            .parts
            .iter()
            .any(|part| part.map == ID && part.rank == Some(1))
    );
    assert!(run.warnings().is_empty() && run.notes().is_empty());
}
