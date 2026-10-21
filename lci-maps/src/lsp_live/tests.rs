//! `lsp.live` against a fake language server (an in-process [`LspBackend`]);
//! no real server runs here.

use super::*;
use crate::{SnapshotRef, kit_c3::check_edge_invariants};
use lci_core::config::Config;
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

    fn nodes_in(&self, path: &str) -> Vec<(u32, NodeId)> {
        self.0
            .iter()
            .filter(|(_, p, _)| p.path == path)
            .map(|(id, p, _)| (p.line, id.clone()))
            .collect()
    }
}

fn table_rows() -> Table {
    let row = |name: &str, path, line, col, span| (sym(name), place(path, line, col), span);
    Table(vec![
        row("a", "src/a.rs", 1, 3, (1, 4)),
        row("b", "src/b.rs", 10, 3, (10, 12)),
        row("c", "src/c.rs", 5, 3, (4, 9)),
        row("x", "src/x.rs", 0, 3, (0, 2)),
        row("y", "src/y.rs", 0, 3, (0, 2)),
    ])
}

fn table() -> Arc<Table> {
    Arc::new(table_rows())
}

/// What the fake server answers, by the place or path asked about. It counts
/// the requests and notes when it is dropped (the server's process is the
/// real backend's to end then).
#[derive(Default)]
struct Fake {
    definitions: HashMap<Place, Vec<Place>>,
    references: HashMap<Place, Vec<Place>>,
    implementations: HashMap<Place, Vec<Place>>,
    /// `Some`: every implementation request fails with this text.
    implementation_error: Option<String>,
    /// Per place of a callable: its callers and its callees. A prepare request
    /// answers one item at the asked place when the key exists, else nothing.
    hierarchy: HashMap<Place, (Vec<CallLink>, Vec<CallLink>)>,
    /// `Some((method, text))`: that call-hierarchy request fails with the text.
    hierarchy_error: Option<(&'static str, String)>,
    members: HashMap<String, Vec<Member>>,
    /// `Some`: every outline request fails with this text.
    outline_error: Option<String>,
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

/// The server's own item of the callable named at `at`: the map must send it
/// back as it got it.
fn raw_item(at: &Place) -> serde_json::Value {
    serde_json::json!({"name": at.path, "data": {"line": at.line, "col": at.col}})
}

fn call_item(path: &str, line: u32, col: u32) -> CallItem {
    let place = place(path, line, col);
    CallItem {
        raw: raw_item(&place),
        place,
    }
}

fn link(item: CallItem, sites: &[Place]) -> CallLink {
    CallLink {
        item,
        sites: sites.to_vec(),
    }
}

impl Fake {
    /// One call-hierarchy request: counted, delayed, and refused when
    /// `hierarchy_error` names it.
    async fn hierarchy_request(&self, method: &str) -> Result<(), String> {
        self.answer::<()>(None).await?;
        match &self.hierarchy_error {
            Some((failing, error)) if *failing == method => Err(error.clone()),
            _ => Ok(()),
        }
    }

    fn calls_of(&self, item: &CallItem) -> Result<&(Vec<CallLink>, Vec<CallLink>), String> {
        if item.raw != raw_item(&item.place) {
            return Err("the item was not sent back as it was given".to_owned());
        }
        self.hierarchy
            .get(&item.place)
            .ok_or_else(|| "no such item".to_owned())
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

    async fn implementation(&self, at: &Place) -> Result<Vec<Place>, String> {
        let answer = self.answer(self.implementations.get(at)).await;
        match &self.implementation_error {
            Some(error) => Err(error.clone()),
            None => answer,
        }
    }

    async fn prepare_call_hierarchy(&self, at: &Place) -> Result<Vec<CallItem>, String> {
        self.hierarchy_request("textDocument/prepareCallHierarchy")
            .await?;
        Ok(self
            .hierarchy
            .contains_key(at)
            .then(|| CallItem {
                place: at.clone(),
                raw: raw_item(at),
            })
            .into_iter()
            .collect())
    }

    async fn incoming_calls(&self, item: &CallItem) -> Result<Vec<CallLink>, String> {
        self.hierarchy_request("callHierarchy/incomingCalls")
            .await?;
        Ok(self.calls_of(item)?.0.clone())
    }

    async fn outgoing_calls(&self, item: &CallItem) -> Result<Vec<CallLink>, String> {
        self.hierarchy_request("callHierarchy/outgoingCalls")
            .await?;
        Ok(self.calls_of(item)?.1.clone())
    }

    async fn document_symbols(&self, path: &str) -> Result<Vec<Member>, String> {
        let answer = self.answer(self.members.get(path)).await;
        match &self.outline_error {
            Some(error) => Err(error.clone()),
            None => answer,
        }
    }
}

fn fake() -> Fake {
    let member = |name: &str, from, to, kind| Member {
        name: name.into(),
        place: place("src/a.rs", from, 0),
        end_line: to,
        kind: Some(kind),
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
            vec![member("a", 0, 4, 12), member("S", 7, 8, 23)],
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

/// `x` is implemented at `b`'s name, inside `c` (not at its name), in a file
/// the index has no symbol for, and, once more, at `x` itself.
fn fake_with_implementations() -> Fake {
    Fake {
        implementations: HashMap::from([(
            place("src/x.rs", 0, 3),
            vec![
                place("src/b.rs", 10, 3),
                place("src/c.rs", 6, 1),
                place("src/z.rs", 0, 0),
                place("src/x.rs", 0, 3),
            ],
        )]),
        ..Fake::default()
    }
}

#[tokio::test]
async fn the_implementation_request_is_third_and_gated_by_the_key_and_the_budget() {
    let implements = |map: &LspLive, found: &Expanded| -> Vec<(NodeId, String, u32, u32)> {
        check_edge_invariants(&sym("x"), map.describe(), &found.edges, EXPAND_LIMIT).unwrap();
        found
            .edges
            .iter()
            .filter(|edge| edge.kind == EdgeKind::Implements)
            .map(|edge| {
                assert_eq!(edge.src, sym("x"));
                assert_eq!(edge.witness, WitnessClass::Exact);
                assert_eq!((edge.map, edge.conf), (LSP_LIVE_ID, CONF_MAX));
                let site = edge.site.as_ref().unwrap();
                (edge.dst.clone(), site.path.clone(), site.line, site.col)
            })
            .collect()
    };
    let wide = || budget(Duration::from_secs(5), 10);

    // Key on: three requests, one edge per destination, the node itself left
    // out, the destination of a place no symbol is defined at the enclosing
    // symbol, else the file.
    let backend = fake_with_implementations();
    let requests = backend.requests.clone();
    let map = map_over(backend).implementations(true);
    let found = map.expand(&sym("x"), wide(), &view()).await;
    let mut want = vec![
        (sym("b"), "src/b.rs".to_owned(), 10, 3),
        (sym("c"), "src/c.rs".to_owned(), 6, 1),
        (file("src/z.rs"), "src/z.rs".to_owned(), 0, 0),
    ];
    want.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(implements(&map, &found), want);
    assert_eq!(found.edges.len(), 3, "the other two requests found nothing");
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert_eq!(found.cost.external_calls, 3);
    assert!(map.last_error().is_none());

    // Key off (the default): today's two requests and no Implements edge.
    let backend = fake_with_implementations();
    let requests = backend.requests.clone();
    let map = map_over(backend);
    let found = map.expand(&sym("x"), wide(), &view()).await;
    assert!(implements(&map, &found).is_empty());
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(found.cost.external_calls, 2);

    // Key on with room for two calls: the third is not made.
    let backend = fake_with_implementations();
    let requests = backend.requests.clone();
    let map = map_over(backend).implementations(true);
    let found = map
        .expand(&sym("x"), budget(Duration::from_secs(5), 2), &view())
        .await;
    assert!(implements(&map, &found).is_empty());
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(found.cost.external_calls, 2);

    // A file node asks for its symbols only.
    let backend = fake_with_implementations();
    let requests = backend.requests.clone();
    let map = map_over(backend).implementations(true);
    map.expand(&file("src/x.rs"), wide(), &view()).await;
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_implementation_error_keeps_the_other_edges() {
    let mut backend = fake();
    backend.implementation_error = Some("no implementation provider".to_owned());
    let requests = backend.requests.clone();
    let map = map_over(backend).implementations(true);
    let found = map
        .expand(&sym("x"), budget(Duration::from_secs(5), 10), &view())
        .await;
    // `fake()` defines `x` at `b`'s name: that edge stands.
    let got: Vec<(EdgeKind, NodeId)> = found
        .edges
        .iter()
        .map(|edge| (edge.kind, edge.dst.clone()))
        .collect();
    assert_eq!(got, [(EdgeKind::CallsOrRefs, sym("b"))]);
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert_eq!(found.cost.external_calls, 3);
    let error = map.last_error().expect("the failure is recorded");
    assert!(
        error.starts_with("textDocument/implementation: ")
            && error.contains("no implementation provider"),
        "{error}"
    );
}

/// `x`: called by `b` (at src/b.rs:11:2) and by something in a file the index
/// has no symbol for; it calls `c` (at src/x.rs:1:4) and, once, itself.
fn fake_with_hierarchy() -> Fake {
    Fake {
        hierarchy: HashMap::from([(
            place("src/x.rs", 0, 3),
            (
                vec![
                    link(
                        call_item("src/b.rs", 10, 3),
                        &[place("src/b.rs", 11, 9), place("src/b.rs", 11, 2)],
                    ),
                    link(call_item("src/z.rs", 0, 0), &[]),
                ],
                vec![
                    link(call_item("src/c.rs", 5, 3), &[place("src/x.rs", 1, 4)]),
                    link(call_item("src/x.rs", 0, 3), &[place("src/x.rs", 1, 1)]),
                ],
            ),
        )]),
        ..Fake::default()
    }
}

fn rows(map: &LspLive, seed: &NodeId, found: &Expanded) -> Vec<Row> {
    check_edge_invariants(seed, map.describe(), &found.edges, EXPAND_LIMIT).unwrap();
    found
        .edges
        .iter()
        .map(|edge| {
            assert_eq!(edge.src, *seed);
            assert_eq!(edge.witness, WitnessClass::Exact);
            assert_eq!((edge.map, edge.conf), (LSP_LIVE_ID, CONF_MAX));
            let site = edge.site.as_ref().unwrap();
            (edge.kind, edge.dst.clone(), site.path.clone(), site.line)
        })
        .collect()
}

#[tokio::test]
async fn call_hierarchy_edges_follow_the_fakes_items_and_sites() {
    let wide = || budget(Duration::from_secs(5), 10);

    // Key on: five requests (definition, references, prepare, incoming,
    // outgoing), `CallsOrRefs` edges only, a caller's and a callee's site the
    // call (the smallest place of its calls), a destination with no symbol its
    // file at the item's place, the node itself left out.
    let backend = fake_with_hierarchy();
    let requests = backend.requests.clone();
    let map = map_over(backend).call_hierarchy(true);
    let found = map.expand(&sym("x"), wide(), &view()).await;
    let mut want: Vec<Row> = vec![
        (EdgeKind::CallsOrRefs, sym("b"), "src/b.rs".into(), 11),
        (
            EdgeKind::CallsOrRefs,
            file("src/z.rs"),
            "src/z.rs".into(),
            0,
        ),
        (EdgeKind::CallsOrRefs, sym("c"), "src/x.rs".into(), 1),
    ];
    want.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(rows(&map, &sym("x"), &found), want);
    assert_eq!(requests.load(Ordering::SeqCst), 5);
    assert_eq!(found.cost.external_calls, 5);
    assert_eq!(found.cost.nodes_touched, 3);
    assert!(map.last_error().is_none());

    // Key off (the default): today's two requests and no hierarchy edge.
    let backend = fake_with_hierarchy();
    let requests = backend.requests.clone();
    let map = map_over(backend);
    let found = map.expand(&sym("x"), wide(), &view()).await;
    assert!(found.edges.is_empty());
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    assert_eq!(found.cost.external_calls, 2);

    // Both optional keys on: the implementation request comes first, then the
    // hierarchy's three.
    let backend = fake_with_hierarchy();
    let requests = backend.requests.clone();
    let map = map_over(backend).implementations(true).call_hierarchy(true);
    let found = map.expand(&sym("x"), wide(), &view()).await;
    assert_eq!(requests.load(Ordering::SeqCst), 6);
    assert_eq!(found.cost.external_calls, 6);
    assert_eq!(found.edges.len(), 3);
}

#[tokio::test]
async fn a_destination_the_references_named_gets_no_second_edge() {
    // `fake()`'s `y` is used inside `c` (first at src/c.rs:6) and in a file
    // with no symbol; the hierarchy names `c` once more, and `a` besides.
    let mut backend = fake();
    backend.hierarchy = HashMap::from([(
        place("src/y.rs", 0, 3),
        (
            vec![
                link(call_item("src/c.rs", 5, 3), &[place("src/c.rs", 8, 0)]),
                link(call_item("src/a.rs", 1, 3), &[place("src/a.rs", 2, 0)]),
            ],
            vec![link(
                call_item("src/c.rs", 5, 3),
                &[place("src/y.rs", 1, 0)],
            )],
        ),
    )]);
    let map = map_over(backend).call_hierarchy(true);
    let found = map
        .expand(&sym("y"), budget(Duration::from_secs(5), 10), &view())
        .await;
    let mut want: Vec<Row> = vec![
        (EdgeKind::CallsOrRefs, sym("c"), "src/c.rs".into(), 6),
        (
            EdgeKind::CallsOrRefs,
            file("src/z.rs"),
            "src/z.rs".into(),
            2,
        ),
        (EdgeKind::CallsOrRefs, sym("a"), "src/a.rs".into(), 2),
    ];
    want.sort_by(|a, b| a.1.cmp(&b.1));
    assert_eq!(rows(&map, &sym("y"), &found), want);
    assert_eq!(found.cost.nodes_touched, found.edges.len() as u64);
    assert_eq!(found.cost.external_calls, 5);
}

#[tokio::test]
async fn call_hierarchy_is_gated_by_the_budget_and_stops_after_an_empty_prepare() {
    // Room for three calls: definition, references, prepare. Neither the
    // callers nor the callees are asked.
    let backend = fake_with_hierarchy();
    let requests = backend.requests.clone();
    let map = map_over(backend).call_hierarchy(true);
    let found = map
        .expand(&sym("x"), budget(Duration::from_secs(5), 3), &view())
        .await;
    assert!(found.edges.is_empty());
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert_eq!(found.cost.external_calls, 3);
    assert!(map.last_error().is_none());

    // Room for four: the callers are asked, the callees are not.
    let backend = fake_with_hierarchy();
    let requests = backend.requests.clone();
    let map = map_over(backend).call_hierarchy(true);
    let found = map
        .expand(&sym("x"), budget(Duration::from_secs(5), 4), &view())
        .await;
    assert_eq!(requests.load(Ordering::SeqCst), 4);
    assert_eq!(found.edges.len(), 2, "the callers only");

    // A node the server has no item for: prepare is empty, nothing more is
    // asked, nothing fails.
    let backend = fake_with_hierarchy();
    let requests = backend.requests.clone();
    let map = map_over(backend).call_hierarchy(true);
    let found = map
        .expand(&sym("y"), budget(Duration::from_secs(5), 10), &view())
        .await;
    assert!(found.edges.is_empty());
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert_eq!(found.cost.external_calls, 3);
    assert!(map.last_error().is_none());

    // A file node asks for its symbols only.
    let backend = fake_with_hierarchy();
    let requests = backend.requests.clone();
    let map = map_over(backend).call_hierarchy(true);
    map.expand(
        &file("src/x.rs"),
        budget(Duration::from_secs(5), 10),
        &view(),
    )
    .await;
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_call_hierarchy_error_keeps_the_other_edges() {
    let with = |method: &'static str| {
        // `fake()` defines `x` at `b`'s name (a definition edge to `b`).
        let mut backend = fake();
        backend.hierarchy = fake_with_hierarchy().hierarchy;
        backend.hierarchy_error = Some((method, "no provider".to_owned()));
        backend
    };
    let wide = || budget(Duration::from_secs(5), 10);
    let destinations = |found: &Expanded| -> Vec<NodeId> {
        found.edges.iter().map(|edge| edge.dst.clone()).collect()
    };

    // The callers fail: the definition edge stands, the callees are still asked.
    let backend = with("callHierarchy/incomingCalls");
    let requests = backend.requests.clone();
    let map = map_over(backend).call_hierarchy(true);
    let found = map.expand(&sym("x"), wide(), &view()).await;
    let mut want = vec![sym("b"), sym("c")];
    want.sort();
    assert_eq!(destinations(&found), want);
    assert_eq!(requests.load(Ordering::SeqCst), 5);
    assert_eq!(found.cost.external_calls, 5);
    let error = map.last_error().expect("the failure is recorded");
    assert!(
        error.starts_with("callHierarchy/incomingCalls: ") && error.contains("no provider"),
        "{error}"
    );

    // The callees fail: the definition edge and the callers' edges stand (the
    // definition's `b` is also a caller: still one edge to it).
    let backend = with("callHierarchy/outgoingCalls");
    let map = map_over(backend).call_hierarchy(true);
    let found = map.expand(&sym("x"), wide(), &view()).await;
    assert_eq!(found.cost.external_calls, 5);
    let mut want = vec![sym("b"), file("src/z.rs")];
    want.sort();
    assert_eq!(destinations(&found), want);
    assert!(
        map.last_error()
            .unwrap()
            .starts_with("callHierarchy/outgoingCalls: ")
    );

    // Prepare fails (a server without call hierarchy answers -32601 here):
    // the definition edge stands and nothing more is asked.
    let backend = with("textDocument/prepareCallHierarchy");
    let requests = backend.requests.clone();
    let map = map_over(backend).call_hierarchy(true);
    let found = map.expand(&sym("x"), wide(), &view()).await;
    assert_eq!(destinations(&found), [sym("b")]);
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert_eq!(found.cost.external_calls, 3);
    assert!(
        map.last_error()
            .unwrap()
            .starts_with("textDocument/prepareCallHierarchy: ")
    );
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

/// The version probe is bounded by the timeout the caller gives. A zero bound
/// is over before any process can have answered (tokio's timeout polls the
/// inner future first, and the spawned process cannot finish on its first
/// poll), so it fails with no wait; a
/// wide one lets a real executable that answers `--version` (cargo, which runs
/// these tests) through. The same manager and binary in both calls, so only
/// the bound differs.
#[tokio::test]
async fn the_version_probe_honours_the_timeout_it_is_given() {
    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".to_owned());
    let config = Config {
        rust_analyzer_path: cargo,
        ..Config::default()
    };
    let workspace = || Workspace {
        id: "test".into(),
        path: std::env::temp_dir(),
    };
    let manager = Arc::new(Manager::new(&config));
    let error =
        ManagerBackend::open_with_probe_timeout(workspace(), Arc::clone(&manager), Duration::ZERO)
            .await
            .err()
            .expect("a probe with no time left fails");
    assert!(
        error.contains("no language server is available") && error.contains("timed out"),
        "{error}"
    );
    // A failed probe is not remembered: the wide bound runs it again.
    ManagerBackend::open_with_probe_timeout(workspace(), manager, Duration::from_secs(60))
        .await
        .map(drop)
        .expect("the executable answers --version within a minute");
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
    let error = ManagerBackend::open(workspace, Arc::new(Manager::new(&config)))
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

/// A member of `path` named `name` at `line`, column 3, of LSP kind `kind`.
fn outline_member(path: &str, name: &str, line: u32, end_line: u32, kind: u64) -> Member {
    Member {
        name: name.into(),
        place: place(path, line, 3),
        end_line,
        kind: Some(kind),
    }
}

/// The id `OutlinePlaces` mints in a Rust file whose module path is `module`.
fn minted(module: &str, name: &str, kind: SymbolKind, disambiguator: u16) -> NodeId {
    NodeId::Symbol(SymbolKey {
        language: "rust".into(),
        module_path: vec![module.into()],
        container: Vec::new(),
        name: name.into(),
        kind,
        disambiguator,
    })
}

#[test]
fn the_outline_layer_resolves_what_the_index_lacks_and_keeps_the_indexs_ids() {
    let places = OutlinePlaces::over(table_rows());
    assert!(places.outline_wanted("src/a.rs"));
    // `a` the index knows (its name is on line 1, the live outline has it on
    // line 2: the tree moved); `S` it does not; `f` is a field.
    places.learn(
        "src/a.rs",
        &[
            outline_member("src/a.rs", "a", 2, 5, 12),
            outline_member("src/a.rs", "S", 7, 9, 23),
            outline_member("src/a.rs", "f", 8, 8, 8),
        ],
    );
    assert!(!places.outline_wanted("src/a.rs"));
    assert!(places.outline_wanted("src/b.rs"));
    // One symbol, one id across the layers: the index's, at the live place.
    assert_eq!(places.defined_at(&place("src/a.rs", 2, 3)), Some(sym("a")));
    assert_eq!(places.place_of(&sym("a")), Some(place("src/a.rs", 2, 3)));
    // What the index lacks gets a minted id with the file's language.
    let s = minted("a", "S", SymbolKind::Struct, 0);
    assert_eq!(s.to_string(), "sym:rust:a::S/struct");
    assert_eq!(places.defined_at(&place("src/a.rs", 7, 3)), Some(s.clone()));
    assert_eq!(places.place_of(&s), Some(place("src/a.rs", 7, 3)));
    // A field has no node.
    assert_eq!(places.defined_at(&place("src/a.rs", 8, 3)), None);
    // The innermost learned span holds the line; the field's line is S's.
    assert_eq!(places.enclosing("src/a.rs", 8), Some(s.clone()));
    assert_eq!(places.enclosing("src/a.rs", 3), Some(sym("a")));
    // An unlearned file is the index's.
    assert_eq!(places.place_of(&sym("b")), Some(place("src/b.rs", 10, 3)));
    assert_eq!(places.enclosing("src/b.rs", 11), Some(sym("b")));
    // Contains: the name's own line, then the range.
    assert_eq!(places.defined_in("src/a.rs", (2, 5), "a"), Some(sym("a")));
    assert_eq!(places.defined_in("src/a.rs", (7, 9), "S"), Some(s.clone()));
    assert_eq!(places.defined_in("src/a.rs", (8, 8), "f"), None);
    // A later outline replaces the file's entries.
    places.learn("src/a.rs", &[outline_member("src/a.rs", "a", 2, 5, 12)]);
    assert_eq!(places.defined_at(&place("src/a.rs", 7, 3)), None);
    assert_eq!(places.place_of(&s), None);
}

#[test]
fn the_index_table_takes_the_name_line_before_a_wider_range() {
    // An item's range now runs to its end and covers a same-named symbol
    // inside it; the member's own name line still picks its own node.
    let inner = |disambiguator| {
        NodeId::Symbol(SymbolKey {
            language: "rust".into(),
            module_path: vec!["m".into()],
            container: Vec::new(),
            name: "inner".into(),
            kind: SymbolKind::Function,
            disambiguator,
        })
    };
    let mut scip = ScipPlaces::default();
    scip.by_file
        .insert("src/m.rs".to_owned(), vec![(3, inner(0)), (6, inner(1))]);
    assert_eq!(scip.defined_in("src/m.rs", (3, 9), "inner"), Some(inner(0)));
    assert_eq!(scip.defined_in("src/m.rs", (6, 9), "inner"), Some(inner(1)));
    // No exact line: the first inside the range, as before.
    assert_eq!(scip.defined_in("src/m.rs", (4, 9), "inner"), Some(inner(1)));
}

#[tokio::test]
async fn a_symbol_pays_one_outline_for_its_file_then_asks() {
    let backend = || Fake {
        members: HashMap::from([(
            "src/x.rs".to_owned(),
            vec![outline_member("src/x.rs", "x", 0, 2, 12)],
        )]),
        ..fake()
    };
    let wide = || budget(Duration::from_secs(5), 10);

    // Over an empty index: a file expansion learns `src/x.rs` (one call), and
    // the symbol it names is then asked with no second outline.
    let server = backend();
    let requests = server.requests.clone();
    let map = LspLive::new(
        Box::new(server),
        Arc::new(OutlinePlaces::over(Table(Vec::new()))),
    );
    let found = map.expand(&file("src/x.rs"), wide(), &view()).await;
    assert_eq!(found.cost.external_calls, 1);
    let x = minted("x", "x", SymbolKind::Function, 0);
    let contained: Vec<_> = found
        .edges
        .iter()
        .map(|e| (e.kind, e.dst.clone()))
        .collect();
    assert_eq!(contained, [(EdgeKind::Contains, x.clone())]);
    let found = map.expand(&x, wide(), &view()).await;
    assert_eq!(found.cost.external_calls, 2, "definition and references");
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    assert!(map.last_error().is_none());

    // A symbol of an unlearned index file: the outline, then the requests.
    let server = backend();
    let requests = server.requests.clone();
    let map = LspLive::new(
        Box::new(server),
        Arc::new(OutlinePlaces::over(table_rows())),
    );
    let found = map.expand(&sym("x"), wide(), &view()).await;
    assert_eq!(found.cost.external_calls, 3);
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    // `x` keeps the index's id; its definition is the index's `b`.
    let reached: Vec<_> = found.edges.iter().map(|e| e.dst.clone()).collect();
    assert_eq!(reached, [sym("b")]);
    // The file is learned: asking again pays no outline.
    let found = map.expand(&sym("x"), wide(), &view()).await;
    assert_eq!(found.cost.external_calls, 2);
    assert_eq!(requests.load(Ordering::SeqCst), 5);

    // Under a budget of one call the outline is all there is.
    let server = backend();
    let requests = server.requests.clone();
    let map = LspLive::new(
        Box::new(server),
        Arc::new(OutlinePlaces::over(table_rows())),
    );
    let found = map
        .expand(&sym("x"), budget(Duration::from_secs(5), 1), &view())
        .await;
    assert_eq!(found.cost.external_calls, 1);
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(found.edges.is_empty());
}

#[tokio::test]
async fn the_scip_table_never_asks_for_the_outline_of_a_symbol() {
    let backend = fake();
    let requests = backend.requests.clone();
    let map = map_over(backend);
    let found = map
        .expand(&sym("x"), budget(Duration::from_secs(5), 10), &view())
        .await;
    assert_eq!(found.cost.external_calls, 2, "definition and references");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn an_outline_error_keeps_the_index_place() {
    let backend = Fake {
        outline_error: Some("no outline today".to_owned()),
        ..fake()
    };
    let requests = backend.requests.clone();
    let map = LspLive::new(
        Box::new(backend),
        Arc::new(OutlinePlaces::over(table_rows())),
    );
    let wide = || budget(Duration::from_secs(5), 10);
    let found = map.expand(&sym("x"), wide(), &view()).await;
    // The definition is still asked at the index's place and its edge kept.
    let reached: Vec<_> = found.edges.iter().map(|e| e.dst.clone()).collect();
    assert_eq!(reached, [sym("b")]);
    assert_eq!(found.cost.external_calls, 3);
    let error = map.last_error().unwrap();
    assert!(error.contains("documentSymbol"), "{error}");
    assert!(error.contains("no outline today"), "{error}");
    // The failed file is not asked again.
    let found = map.expand(&sym("x"), wide(), &view()).await;
    assert_eq!(found.cost.external_calls, 2);
    assert_eq!(requests.load(Ordering::SeqCst), 5);
}

#[test]
fn kinds_without_a_node_are_left_out_and_equal_names_are_disambiguated_in_document_order() {
    let places = OutlinePlaces::over(Table(Vec::new()));
    // Arrival order is not document order.
    places.learn(
        "src/n.rs",
        &[
            outline_member("src/n.rs", "new", 20, 22, 6),
            outline_member("src/n.rs", "new", 2, 4, 6),
            outline_member("src/n.rs", "count", 6, 6, 13),
            outline_member("src/n.rs", "size", 7, 7, 8),
            outline_member("src/n.rs", "new", 10, 12, 9),
            outline_member("src/n.rs", "Unit", 14, 14, 22),
        ],
    );
    let id_at = |line| places.defined_at(&place("src/n.rs", line, 3));
    let method = |disambiguator| minted("n", "new", SymbolKind::Method, disambiguator);
    assert_eq!(id_at(2), Some(method(0)));
    assert_eq!(id_at(10), Some(method(1)));
    assert_eq!(id_at(20), Some(method(2)));
    assert_eq!(method(2).to_string(), "sym:rust:n::new/method~2");
    // A variable (13), a field (8) and an enum member (22) get none.
    for line in [6, 7, 14] {
        assert_eq!(id_at(line), None, "line {line}");
    }
    // The kinds this build has a node for.
    let kinds = [
        (12, SymbolKind::Function),
        (6, SymbolKind::Method),
        (9, SymbolKind::Method),
        (5, SymbolKind::Struct),
        (23, SymbolKind::Struct),
        (11, SymbolKind::Trait),
        (10, SymbolKind::Enum),
        (2, SymbolKind::Module),
        (3, SymbolKind::Module),
        (4, SymbolKind::Module),
        (14, SymbolKind::Const),
    ];
    let members: Vec<Member> = (0u32..)
        .zip(kinds)
        .map(|(line, (kind, _))| outline_member("src/k.rs", &format!("k{line}"), line, line, kind))
        .collect();
    places.learn("src/k.rs", &members);
    for (line, (kind, want)) in (0u32..).zip(kinds) {
        assert_eq!(
            places.defined_at(&place("src/k.rs", line, 3)),
            Some(minted("k", &format!("k{line}"), want, 0)),
            "LSP kind {kind}"
        );
    }
}

#[tokio::test]
async fn a_failed_file_outline_is_not_asked_again_for_a_symbol_of_that_file() {
    let backend = Fake {
        outline_error: Some("no outline today".to_owned()),
        ..fake()
    };
    let requests = backend.requests.clone();
    let map = LspLive::new(
        Box::new(backend),
        Arc::new(OutlinePlaces::over(table_rows())),
    );
    let wide = || budget(Duration::from_secs(5), 10);
    // The file branch fails: no edges, one outline request, the error kept.
    let found = map.expand(&file("src/x.rs"), wide(), &view()).await;
    assert!(found.edges.is_empty());
    assert_eq!(requests.load(Ordering::SeqCst), 1);
    assert!(map.last_error().unwrap().contains("no outline today"));
    // A symbol of that file asks no second outline: definition and references.
    let found = map.expand(&sym("x"), wide(), &view()).await;
    assert_eq!(found.cost.external_calls, 2);
    assert_eq!(
        requests.load(Ordering::SeqCst),
        3,
        "one outline request in all"
    );
}
