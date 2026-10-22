//! Arbitration: an authority map (a language server) overrules the edges of
//! subordinate maps (a stale index) before fusion. Counted per call, per map
//! and per walk; nothing changes (no edge, no key) when none is attached.
//! Stub maps that count their calls; no sleeps, no clocks.

use super::*;
use crate::traverse::{
    merged::{MergedAdjacency, PreMerged},
    source::{
        Arbitration, ArbitrationCounts, ClaimFamily, EdgeSource, ExpandCall, Expansion, arbitrate,
    },
};
use lci_core::{chunk::Chunk, node::SymbolKind};

/// `fn:B`, `method:B`, `struct:E`, `const:K` are symbols of that kind; `file:b`
/// and a bare `a` are files.
fn n(name: &str) -> NodeId {
    if let Some((kind, rest)) = name.split_once(':') {
        if kind == "file" {
            return node(rest);
        }
        return format!("sym:rust:{rest}/{kind}").parse().unwrap();
    }
    node(name)
}

/// A map that offers fixed edges, each with its own kind, and counts how
/// often it was asked. It accepts files and symbols.
struct Scripted {
    descriptor: MapDescriptor,
    witness: WitnessClass,
    edges: Vec<(&'static str, &'static str, EdgeKind)>,
    panics: bool,
    asked: Arc<AtomicU32>,
}

impl Scripted {
    fn new(
        id: MapId,
        witness: WitnessClass,
        edges: &[(&'static str, &'static str, EdgeKind)],
    ) -> Self {
        Self {
            descriptor: MapDescriptor {
                id,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::File, NodeKind::Symbol],
                emits: vec![NodeKind::File, NodeKind::Symbol],
                freshness: FreshnessModel::PerFile,
                source_labels: vec![SourceLabel::Scip],
                needs: Vec::new(),
            },
            witness,
            edges: edges.to_vec(),
            panics: false,
            asked: Arc::default(),
        }
    }

    /// The authority: a server answer, witness `Exact`.
    fn server(edges: &[(&'static str, &'static str, EdgeKind)]) -> Self {
        Self::new("lsp.live", WitnessClass::Exact, edges)
    }

    fn index(id: MapId, edges: &[(&'static str, &'static str, EdgeKind)]) -> Self {
        Self::new(id, WitnessClass::Compiler, edges)
    }

    fn panics(mut self) -> Self {
        self.panics = true;
        self
    }

    fn counter(&self) -> Arc<AtomicU32> {
        self.asked.clone()
    }
}

#[crate::async_trait]
impl Map for Scripted {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, _request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        MapResponse::empty(&self.descriptor, view, MapStatus::Ok)
    }

    async fn expand(
        &self,
        from: &NodeId,
        _budget: Budget,
        _view: &SnapshotView<'_>,
    ) -> crate::edge::Expanded {
        self.asked.fetch_add(1, Ordering::Relaxed);
        assert!(!self.panics, "this map panics on purpose");
        let edges = self
            .edges
            .iter()
            .filter(|(src, _, _)| n(src) == *from)
            .map(|(src, dst, kind)| Edge {
                src: n(src),
                dst: n(dst),
                kind: *kind,
                map: self.descriptor.id,
                witness: self.witness,
                conf: 100,
                site: None,
            })
            .collect();
        crate::edge::Expanded::new(edges, Cost::default())
    }
}

fn registry_of(maps: Vec<Scripted>) -> Registry<'static> {
    let mut registry = Registry::new(60.0);
    for map in maps {
        registry.register(Box::new(map), 1.0, 40).unwrap();
    }
    registry
}

const REFERS: EdgeKind = EdgeKind::CallsOrRefs;
const CONTAINS: EdgeKind = EdgeKind::Contains;

/// The server wins over `scip.impact` (references) and `structure.contains`;
/// `graph.scip` is not listed. Functions and methods can be overruled.
fn arbitration() -> Arbitration {
    Arbitration {
        authority: "lsp.live",
        subordinate: vec![
            ("scip.impact", ClaimFamily::Refers),
            ("structure.contains", ClaimFamily::Structure),
        ],
        kinds: vec![SymbolKind::Function, SymbolKind::Method],
    }
}

fn budget_of() -> Budget {
    Budget {
        deadline: Instant::now() + Duration::from_secs(60),
        max_external_calls: u32::MAX,
        max_nodes_touched: u64::MAX,
    }
}

/// The destinations of an expansion, sorted by node.
fn dsts(expansion: &Expansion) -> Vec<NodeId> {
    let mut all: Vec<NodeId> = expansion.edges.iter().map(|e| e.best.dst.clone()).collect();
    all.sort();
    all
}

fn sorted(names: &[&str]) -> Vec<NodeId> {
    let mut all: Vec<NodeId> = names.iter().map(|name| n(name)).collect();
    all.sort();
    all
}

fn call<'e>(expansion: &'e Expansion, map: &str) -> &'e ExpandCall {
    expansion
        .calls
        .iter()
        .find(|c| c.map == map)
        .unwrap_or_else(|| panic!("no call of {map}"))
}

fn counts(confirmed: u32, overruled: u32) -> Option<ArbitrationCounts> {
    Some(ArbitrationCounts {
        confirmed,
        overruled,
    })
}

async fn expand_a(reg: &Registry<'static>, arbitration: Option<Arbitration>) -> Expansion {
    let view = view();
    let source = OnTheFly::new(reg, &view);
    let source = match arbitration {
        Some(a) => source.with_arbitration(a),
        None => source,
    };
    source.edges(&n("a"), budget_of()).await
}

#[tokio::test]
async fn a_subordinate_edge_the_authority_also_names_is_confirmed_and_one_it_does_not_is_overruled()
{
    let server = Scripted::server(&[("a", "fn:B", REFERS)]);
    let server_asked = server.counter();
    let reg = registry_of(vec![
        server,
        Scripted::index(
            "scip.impact",
            &[
                ("a", "fn:B", REFERS),
                ("a", "fn:C", REFERS),
                ("a", "struct:E", REFERS),
            ],
        ),
        // The server answered no Structure edge at `a`: this one stands.
        Scripted::index("structure.contains", &[("a", "fn:D", CONTAINS)]),
    ]);
    let expansion = expand_a(&reg, Some(arbitration())).await;
    // C is overruled; E is a struct (the server cannot place one), so stands.
    assert_eq!(dsts(&expansion), sorted(&["fn:B", "fn:D", "struct:E"]));
    assert_eq!(call(&expansion, "scip.impact").arbitration, counts(1, 1));
    assert_eq!(
        call(&expansion, "structure.contains").arbitration,
        counts(0, 0)
    );
    assert_eq!(call(&expansion, "lsp.live").arbitration, None);
    // The arbitration removes edges; it does not ask the authority again.
    assert_eq!(server_asked.load(Ordering::Relaxed), 1);
    // `n_edges` is what each map offered.
    assert_eq!(call(&expansion, "scip.impact").n_edges, 3);
    // The confirmed destination keeps both offers.
    let b = expansion
        .edges
        .iter()
        .find(|e| e.best.dst == n("fn:B"))
        .unwrap();
    assert_eq!(b.maps, vec!["lsp.live", "scip.impact"]);
}

#[tokio::test]
async fn a_family_the_authority_did_not_answer_leaves_the_subordinate_edges_standing() {
    // The server answered Structure at `a` only: Refers has no answer.
    let reg = registry_of(vec![
        Scripted::server(&[("a", "fn:H", CONTAINS)]),
        Scripted::index(
            "scip.impact",
            &[("a", "fn:B", REFERS), ("a", "fn:C", REFERS)],
        ),
    ]);
    let expansion = expand_a(&reg, Some(arbitration())).await;
    assert_eq!(dsts(&expansion), sorted(&["fn:B", "fn:C", "fn:H"]));
    // Attached, nothing arbitrated: `Some` with zeros, not `None`.
    assert_eq!(call(&expansion, "scip.impact").arbitration, counts(0, 0));
}

#[tokio::test]
async fn an_authority_that_did_not_answer_at_all_leaves_every_edge_standing() {
    let reg = registry_of(vec![
        Scripted::server(&[("a", "fn:B", REFERS)]).panics(),
        Scripted::index(
            "scip.impact",
            &[("a", "fn:B", REFERS), ("a", "fn:C", REFERS)],
        ),
    ]);
    let expansion = expand_a(&reg, Some(arbitration())).await;
    assert_eq!(dsts(&expansion), sorted(&["fn:B", "fn:C"]));
    assert!(call(&expansion, "lsp.live").failure.is_some());
    assert_eq!(call(&expansion, "scip.impact").arbitration, counts(0, 0));
}

#[tokio::test]
async fn only_symbol_destinations_of_the_listed_kinds_are_arbitrated() {
    let reg = registry_of(vec![
        Scripted::server(&[("a", "fn:B", REFERS)]),
        Scripted::index(
            "scip.impact",
            &[
                ("a", "fn:B", REFERS),     // listed kind, named: confirmed
                ("a", "method:M", REFERS), // listed kind, not named: overruled
                ("a", "struct:S", REFERS), // not a listed kind: stands
                ("a", "const:K", REFERS),  // not a listed kind: stands
                ("a", "file:f", REFERS),   // not a symbol: stands
            ],
        ),
    ]);
    let expansion = expand_a(&reg, Some(arbitration())).await;
    assert_eq!(
        dsts(&expansion),
        sorted(&["fn:B", "struct:S", "const:K", "file:f"])
    );
    assert_eq!(call(&expansion, "scip.impact").arbitration, counts(1, 1));
}

#[tokio::test]
async fn only_the_listed_family_of_a_subordinate_map_is_arbitrated() {
    // The server answered both families at `a`; `scip.impact` is subordinate
    // for Refers only, so its Contains edge is left alone.
    let reg = registry_of(vec![
        Scripted::server(&[("a", "fn:B", REFERS), ("a", "fn:H", CONTAINS)]),
        Scripted::index(
            "scip.impact",
            &[("a", "fn:C", REFERS), ("a", "fn:G", CONTAINS)],
        ),
    ]);
    let expansion = expand_a(&reg, Some(arbitration())).await;
    assert_eq!(dsts(&expansion), sorted(&["fn:B", "fn:G", "fn:H"]));
    assert_eq!(call(&expansion, "scip.impact").arbitration, counts(0, 1));
}

#[tokio::test]
async fn a_map_that_is_not_subordinate_is_never_overruled() {
    // `graph.scip` offers callees, a direction the server's references do not
    // answer: it is not listed, so nothing of it is removed or counted.
    let reg = registry_of(vec![
        Scripted::server(&[("a", "fn:B", REFERS)]),
        Scripted::index(
            "graph.scip",
            &[("a", "fn:C", REFERS), ("a", "fn:D", REFERS)],
        ),
    ]);
    let expansion = expand_a(&reg, Some(arbitration())).await;
    assert_eq!(dsts(&expansion), sorted(&["fn:B", "fn:C", "fn:D"]));
    assert_eq!(call(&expansion, "graph.scip").arbitration, None);
    assert_eq!(call(&expansion, "graph.scip").n_edges, 2);
}

#[tokio::test]
async fn an_arbitration_that_is_not_attached_changes_no_edge_and_prints_nothing() {
    let maps = || {
        vec![
            Scripted::server(&[("a", "fn:B", REFERS)]),
            Scripted::index(
                "scip.impact",
                &[
                    ("a", "fn:B", REFERS),
                    ("a", "fn:C", REFERS),
                    ("a", "struct:E", REFERS),
                ],
            ),
            Scripted::index("structure.contains", &[("a", "fn:D", CONTAINS)]),
        ]
    };
    let reg = registry_of(maps());
    let expansion = expand_a(&reg, None).await;
    // Five edges (B twice fused), every destination, no count.
    assert_eq!(
        dsts(&expansion),
        sorted(&["fn:B", "fn:C", "fn:D", "struct:E"])
    );
    assert_eq!(
        expansion.edges.iter().map(|e| e.maps.len()).sum::<usize>(),
        5
    );
    assert!(expansion.calls.iter().all(|c| c.arbitration.is_none()));
    // Exactly what fusing the raw answers gives.
    let view = view();
    let source = OnTheFly::new(&reg, &view);
    let (raw, _) = source.ask(&n("a"), budget_of()).await;
    assert_eq!(expansion.edges, fuse(raw));

    // A walk without the builder: no key in the expand events or the totals.
    let result = walk(&reg, vec![seed("a")], budget(5), |t| t).await;
    assert_eq!(result.arbitration, None);
    assert!(result.arbitration_by_map.is_empty());
    for expand in events_of(&result, "expand") {
        for call in expand["calls"].as_array().unwrap() {
            assert!(call.get("arbitration").is_none(), "{call}");
        }
    }
    let end = events_of(&result, "end");
    assert!(end[0]["totals"].get("arbitration").is_none());
}

#[tokio::test]
async fn the_totals_sum_the_calls() {
    // At `a` the server names B: B confirmed, C overruled. B is expanded in
    // turn: the server names X, so X is confirmed and Y overruled.
    let reg = registry_of(vec![
        Scripted::server(&[("a", "fn:B", REFERS), ("fn:B", "fn:X", REFERS)]),
        Scripted::index(
            "scip.impact",
            &[
                ("a", "fn:B", REFERS),
                ("a", "fn:C", REFERS),
                ("fn:B", "fn:X", REFERS),
                ("fn:B", "fn:Y", REFERS),
            ],
        ),
    ]);
    let view = view();
    let source = OnTheFly::new(&reg, &view).with_arbitration(arbitration());
    let result = Traversal::new(source, vec![seed("a")], budget(10))
        .arbitration(arbitration())
        .run()
        .await;
    let reached: Vec<NodeId> = result.states.iter().map(|s| s.node.clone()).collect();
    assert!(reached.contains(&n("fn:B")) && reached.contains(&n("fn:X")));
    assert!(!reached.contains(&n("fn:C")) && !reached.contains(&n("fn:Y")));

    // Each expansion's call entry carries its own counts; the server's none.
    // X is expanded too and has nothing to arbitrate: attached, so zeros.
    let expands = events_of(&result, "expand");
    assert_eq!(expands.len(), 3, "{expands:?}");
    for (name, want) in [("a", (1, 1)), ("fn:B", (1, 1)), ("fn:X", (0, 0))] {
        let expand = expands
            .iter()
            .find(|e| e["node"] == n(name).to_string())
            .unwrap_or_else(|| panic!("no expand event for {name}"));
        let calls = expand["calls"].as_array().unwrap();
        let impact = calls.iter().find(|c| c["map"] == "scip.impact").unwrap();
        assert_eq!(
            impact["arbitration"],
            serde_json::json!({"confirmed": want.0, "overruled": want.1}),
            "{name}"
        );
        let server = calls.iter().find(|c| c["map"] == "lsp.live").unwrap();
        assert!(server.get("arbitration").is_none());
    }
    // The sum over the calls, in the end event, the result and per map.
    let end = events_of(&result, "end");
    assert_eq!(
        end[0]["totals"]["arbitration"],
        serde_json::json!({"confirmed": 2, "overruled": 2})
    );
    assert_eq!(result.arbitration, counts(2, 2));
    assert_eq!(
        result.arbitration_by_map,
        [
            (
                "scip.impact",
                ArbitrationCounts {
                    confirmed: 2,
                    overruled: 2
                }
            ),
            ("structure.contains", ArbitrationCounts::default()),
        ]
        .into_iter()
        .collect()
    );
}

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

#[tokio::test]
async fn a_pre_merged_source_arbitrates_as_the_on_the_fly_source_does() {
    let reg = registry_of(vec![
        Scripted::server(&[("a", "fn:B", REFERS), ("a", "fn:H", CONTAINS)]),
        Scripted::index(
            "scip.impact",
            &[
                ("a", "fn:B", REFERS),
                ("a", "fn:C", REFERS),
                ("a", "struct:E", REFERS),
                ("a", "fn:G", CONTAINS),
            ],
        ),
        Scripted::index("structure.contains", &[("a", "fn:D", CONTAINS)]),
        Scripted::index("graph.scip", &[("a", "fn:Z", REFERS)]),
    ]);
    let chunks = [chunk("src/a.rs", "h1")];
    let view = SnapshotView::new(
        SnapshotRef {
            indexed_at: "t".into(),
            fingerprint: "f".into(),
        },
        chunks.iter().collect(),
    );
    let adjacency = MergedAdjacency::build(&reg, &view).await;
    let merged = PreMerged::new(&adjacency).with_arbitration(arbitration());
    let live = OnTheFly::new(&reg, &view).with_arbitration(arbitration());
    let a = merged.edges(&n("a"), budget_of()).await;
    let b = live.edges(&n("a"), budget_of()).await;
    assert_eq!(a.edges, b.edges);
    assert_eq!(a.calls, b.calls);
    assert_eq!(call(&a, "scip.impact").arbitration, counts(1, 1));
    // The server answered Structure at `a` too (H), so D is overruled; G is
    // `scip.impact`'s Contains edge (not a listed pair) and Z is `graph.scip`.
    assert_eq!(call(&a, "structure.contains").arbitration, counts(0, 1));
    assert_eq!(
        dsts(&a),
        sorted(&["fn:B", "fn:G", "fn:H", "fn:Z", "struct:E"])
    );
    // Without it the pre-merged source gives every raw edge, as before.
    let plain = PreMerged::new(&adjacency).edges(&n("a"), budget_of()).await;
    assert!(plain.calls.iter().all(|c| c.arbitration.is_none()));
    assert!(dsts(&plain).contains(&n("fn:C")));
    // With the authority switched off in the pre-merged source nothing answers.
    let mut off = PreMerged::new(&adjacency).with_arbitration(arbitration());
    assert!(off.set_enabled("lsp.live", false));
    let off = off.edges(&n("a"), budget_of()).await;
    assert!(dsts(&off).contains(&n("fn:C")));
    assert_eq!(call(&off, "scip.impact").arbitration, counts(0, 0));
}

#[test]
fn arbitrate_leaves_the_order_of_the_edges_it_keeps() {
    let edge = |map: MapId, dst: &str, kind: EdgeKind| Edge {
        src: n("a"),
        dst: n(dst),
        kind,
        map,
        witness: WitnessClass::Compiler,
        conf: 100,
        site: None,
    };
    let mut raw = vec![
        edge("lsp.live", "fn:B", REFERS),
        edge("scip.impact", "fn:C", REFERS),
        edge("scip.impact", "fn:B", REFERS),
        edge("scip.impact", "fn:D", REFERS),
    ];
    let mut calls = Vec::new();
    arbitrate(&mut raw, &mut calls, &arbitration());
    let dsts: Vec<NodeId> = raw.iter().map(|e| e.dst.clone()).collect();
    assert_eq!(dsts, vec![n("fn:B"), n("fn:B")]);
    assert_eq!(raw[0].map, "lsp.live");
    assert_eq!(raw[1].map, "scip.impact");
}
