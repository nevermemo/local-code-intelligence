//! `lsp.live` against a fake language server (an in-process [`LspBackend`]);
//! no real server runs here.

use super::*;
use crate::{SnapshotRef, kit_c3::check_edge_invariants};
use lci_core::node::{SymbolKey, SymbolKind};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

fn sym(name: &str) -> NodeId {
    NodeId::Symbol(SymbolKey {
        language: "rust".into(),
        module_path: vec!["m".into()],
        container: Vec::new(),
        name: name.into(),
        kind: SymbolKind::Function,
        disambiguator: 0,
    })
}

fn place(path: &str, line: u32, col: u32) -> Place {
    Place {
        path: path.into(),
        line,
        col,
    }
}

fn file(path: &str) -> NodeId {
    NodeId::file(path).unwrap()
}

/// The index of the fixture: `a` at src/a.rs:1:3 (lines 1-4), `b` at
/// src/b.rs:10:3 (lines 10-12), `c` at src/c.rs:5:3 (lines 4-9), `x` at
/// src/x.rs:0:3, `y` at src/y.rs:0:3.
struct Table(Vec<(NodeId, Place, (u32, u32))>);

impl SymbolPlaces for Table {
    fn place_of(&self, node: &NodeId) -> Option<Place> {
        self.0.iter().find(|(n, ..)| n == node).map(|e| e.1.clone())
    }

    fn defined_at(&self, at: &Place) -> Option<NodeId> {
        self.0.iter().find(|e| e.1 == *at).map(|e| e.0.clone())
    }

    fn defined_in(&self, path: &str, lines: (u32, u32), name: &str) -> Option<NodeId> {
        self.0
            .iter()
            .find(|(id, p, _)| {
                p.path == path
                    && (lines.0..=lines.1).contains(&p.line)
                    && matches!(id, NodeId::Symbol(key) if key.name == name)
            })
            .map(|e| e.0.clone())
    }

    fn enclosing(&self, path: &str, line: u32) -> Option<NodeId> {
        self.0
            .iter()
            .find(|(_, p, (from, to))| p.path == path && (*from..=*to).contains(&line))
            .map(|e| e.0.clone())
    }
}

fn table() -> Arc<Table> {
    let row = |name: &str, path, line, col, span| (sym(name), place(path, line, col), span);
    Arc::new(Table(vec![
        row("a", "src/a.rs", 1, 3, (1, 4)),
        row("b", "src/b.rs", 10, 3, (10, 12)),
        row("c", "src/c.rs", 5, 3, (4, 9)),
        row("x", "src/x.rs", 0, 3, (0, 2)),
        row("y", "src/y.rs", 0, 3, (0, 2)),
    ]))
}

/// What the fake server answers, by the place or path asked about. It counts
/// the requests and notes when it is dropped (the server's process is the
/// real backend's to end then).
#[derive(Default)]
struct Fake {
    definitions: HashMap<Place, Vec<Place>>,
    references: HashMap<Place, Vec<Place>>,
    members: HashMap<String, Vec<Member>>,
    delay: Duration,
    requests: Arc<AtomicU32>,
    dropped: SetOnDrop,
}

impl Fake {
    async fn answer<T: Clone + Default>(&self, found: Option<&T>) -> Result<T, String> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        Ok(found.cloned().unwrap_or_default())
    }
}

/// Sets its flag when dropped.
#[derive(Default)]
struct SetOnDrop(Arc<AtomicBool>);

impl Drop for SetOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[crate::async_trait]
impl LspBackend for Fake {
    async fn definition(&self, at: &Place) -> Result<Vec<Place>, String> {
        self.answer(self.definitions.get(at)).await
    }

    async fn references(&self, at: &Place) -> Result<Vec<Place>, String> {
        self.answer(self.references.get(at)).await
    }

    async fn document_symbols(&self, path: &str) -> Result<Vec<Member>, String> {
        self.answer(self.members.get(path)).await
    }
}

fn fake() -> Fake {
    let member = |name: &str, from, to| Member {
        name: name.into(),
        place: place("src/a.rs", from, 0),
        end_line: to,
    };
    Fake {
        // `x` is defined at `b`'s name and also, once more, at itself.
        definitions: HashMap::from([(
            place("src/x.rs", 0, 3),
            vec![place("src/b.rs", 10, 3), place("src/x.rs", 0, 3)],
        )]),
        // `y` is used twice inside `c` and once outside any symbol.
        references: HashMap::from([(
            place("src/y.rs", 0, 3),
            vec![
                place("src/c.rs", 7, 4),
                place("src/c.rs", 6, 8),
                place("src/z.rs", 2, 0),
            ],
        )]),
        // `a` is a member the index knows, listed by its whole item (a line of
        // attributes above its name line 1); `S` (a struct) it does not.
        members: HashMap::from([(
            "src/a.rs".to_owned(),
            vec![member("a", 0, 4), member("S", 7, 8)],
        )]),
        ..Fake::default()
    }
}

fn budget(deadline: Duration, calls: u32) -> Budget {
    Budget {
        deadline: Instant::now() + deadline,
        max_external_calls: calls,
        max_nodes_touched: u64::MAX,
    }
}

fn view() -> SnapshotView<'static> {
    SnapshotView::new(SnapshotRef::default(), Vec::new())
}

fn map_over(fake: Fake) -> LspLive {
    LspLive::new(Box::new(fake), table())
}

/// (kind, destination, site path, site line)
type Row = (EdgeKind, NodeId, String, u32);

#[tokio::test]
async fn definition_references_and_document_symbols_become_exact_edges() {
    let cases: [(&str, NodeId, u32, Vec<Row>); 3] = [
        (
            "definition, not the symbol itself",
            sym("x"),
            2,
            vec![(EdgeKind::CallsOrRefs, sym("b"), "src/b.rs".into(), 10)],
        ),
        (
            "references: the first one in each user, a file when no symbol holds it",
            sym("y"),
            2,
            vec![
                (EdgeKind::CallsOrRefs, sym("c"), "src/c.rs".into(), 6),
                (
                    EdgeKind::CallsOrRefs,
                    file("src/z.rs"),
                    "src/z.rs".into(),
                    2,
                ),
            ],
        ),
        (
            "document symbols the index defines",
            file("src/a.rs"),
            1,
            vec![(EdgeKind::Contains, sym("a"), "src/a.rs".into(), 0)],
        ),
    ];
    for (label, seed, calls, expected) in cases {
        let map = map_over(fake());
        let found = map
            .expand(&seed, budget(Duration::from_secs(5), 10), &view())
            .await;
        check_edge_invariants(&seed, map.describe(), &found.edges, EXPAND_LIMIT).unwrap();
        // The edges come sorted (all confidences are equal): by destination.
        let got: Vec<Row> = found
            .edges
            .iter()
            .map(|edge| {
                assert_eq!(edge.witness, WitnessClass::Exact, "{label}");
                assert_eq!((edge.map, edge.conf), (LSP_LIVE_ID, CONF_MAX), "{label}");
                let site = edge.site.as_ref().unwrap();
                (edge.kind, edge.dst.clone(), site.path.clone(), site.line)
            })
            .collect();
        let mut want = expected;
        want.sort_by(|a, b| a.1.cmp(&b.1));
        assert_eq!(got, want, "{label}");
        assert_eq!(found.cost.external_calls, calls, "{label}");
        assert_eq!(found.cost.nodes_touched, want.len() as u64, "{label}");
        assert!(map.last_error().is_none(), "{label}");
    }
}

#[tokio::test]
async fn a_call_that_outlasts_its_deadline_ends_with_the_note_and_no_edges() {
    let mut slow = fake();
    slow.delay = Duration::from_secs(5);
    let requests = slow.requests.clone();
    let map = map_over(slow);
    let started = Instant::now();
    let found = map
        .expand(&sym("x"), budget(Duration::from_millis(50), 10), &view())
        .await;
    assert!(started.elapsed() < Duration::from_secs(2));
    assert!(found.edges.is_empty());
    assert_eq!(found.cost.external_calls, 1, "the request was made");
    assert_eq!(map.last_error().as_deref(), Some(DEADLINE_NOTE));

    // A deadline already past: nothing is asked.
    let before = requests.load(Ordering::SeqCst);
    let found = map
        .expand(&sym("x"), budget(Duration::ZERO, 10), &view())
        .await;
    assert!(found.edges.is_empty() && found.cost.external_calls == 0);
    assert_eq!(requests.load(Ordering::SeqCst), before);

    // No external call left: nothing is asked either.
    let found = map
        .expand(&sym("x"), budget(Duration::from_secs(5), 0), &view())
        .await;
    assert!(found.cost.external_calls == 0);
    assert_eq!(requests.load(Ordering::SeqCst), before);
}

#[tokio::test]
async fn the_server_is_asked_only_when_expanding_and_lives_until_the_map_drops() {
    let backend = fake();
    let (requests, dropped) = (backend.requests.clone(), backend.dropped.0.clone());
    let map = map_over(backend);
    assert_eq!(requests.load(Ordering::SeqCst), 0, "nothing asked at build");
    for _ in 0..2 {
        map.expand(&sym("x"), budget(Duration::from_secs(5), 10), &view())
            .await;
    }
    assert_eq!(requests.load(Ordering::SeqCst), 4, "one backend, reused");
    assert!(!dropped.load(Ordering::SeqCst));
    drop(map);
    assert!(
        dropped.load(Ordering::SeqCst),
        "the backend goes with the map"
    );
}

#[tokio::test]
async fn a_missing_server_binary_makes_the_map_unavailable_before_any_walk() {
    let config = Config {
        rust_analyzer_path: "lci-no-such-language-server".into(),
        ..Config::default()
    };
    let workspace = Workspace {
        id: "test".into(),
        path: std::env::temp_dir(),
    };
    let error = ManagerBackend::open(workspace, &config)
        .await
        .err()
        .expect("no server can be found");
    assert!(
        error.contains("no language server is available")
            && error.contains("rust-analyzer")
            && error.contains("lci-no-such-language-server"),
        "{error}"
    );
}
