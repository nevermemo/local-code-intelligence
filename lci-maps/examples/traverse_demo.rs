//! Demo of the traversal engine (spine slice S2): a lexical map finds the
//! seeds, a SCIP map expands them, one rule prunes `tests/**` at a stop, and
//! the run is printed as JSON lines on stdout (a summary goes to stderr).
//!
//! ```text
//! cargo run -p lci-maps --example traverse_demo -- [--question TEXT] [--scip INDEX --root TREE] [--out DIR]
//! ```
//!
//! With `--out DIR` (slice S3) the walk is two calls: call 1 stops at the first
//! stop with a depth-2 node queued and leaves a handle; call 2 reads the
//! handle back from `DIR/handles` (as a new process would) and resumes with
//! steering of its own. Both traces are written under `DIR/traces`, and the
//! second (which holds the whole walk) is printed.
//!
//! Without `--scip` it walks a small built-in call graph. With `--scip` it
//! loads that SCIP index (and `--root`, the tree it indexes, for the
//! enclosing-range repair) and walks it with best-effort symbol ids.

use lci_core::node::NodeId;
use lci_maps::{
    Budget, Contribution, EdgeKind, Evidence, FreshnessModel, Map, MapDescriptor, MapHit,
    MapRequest, MapResponse, MapStatus, Registry, SnapshotRef, SnapshotView, SourceLabel,
    scip_graph::{DirSource, RawDocument, RawOccurrence, ScipGraph, ScipMap},
    traverse::{
        Seed, Traversal, TraversalBudget, handle::HandleStore, handler::StopPolicy,
        rules::RuleList, source::OnTheFly, trace,
    },
    walk::tokenize,
};
use std::{
    collections::HashSet,
    path::PathBuf,
    time::{Duration, Instant},
};

const LEXICAL: &str = "demo.lexical";

/// Matches the question's words against symbol names. Text only: it never
/// expands a node.
struct NameLexical {
    descriptor: MapDescriptor,
    names: Vec<(NodeId, HashSet<String>)>,
}

impl NameLexical {
    fn new(scip: &ScipMap) -> Self {
        let graph = scip.graph();
        Self {
            descriptor: MapDescriptor {
                id: LEXICAL,
                version: 1,
                contribution: Contribution::RankList,
                accepts_text: true,
                accepts: Vec::new(),
                emits: vec![lci_core::node::NodeKind::Symbol],
                freshness: FreshnessModel::PerFile,
                source_labels: vec![SourceLabel::Lexical],
                needs: Vec::new(),
            },
            names: (0..graph.nodes.len())
                .map(|i| {
                    (
                        scip.node_id(i).clone(),
                        tokenize(&graph.nodes[i].name).into_iter().collect(),
                    )
                })
                .collect(),
        }
    }
}

#[lci_maps::async_trait]
impl Map for NameLexical {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let words = tokenize(request.text.unwrap_or_default());
        let mut scored: Vec<(usize, &NodeId)> = self
            .names
            .iter()
            .map(|(id, tokens)| (words.iter().filter(|w| tokens.contains(*w)).count(), id))
            .filter(|(n, _)| *n > 0)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        response.hits = scored
            .into_iter()
            .take(request.limit)
            .zip(1..)
            .map(|((_, id), rank)| MapHit {
                node: id.clone(),
                rank,
                score: None,
                evidence: vec![Evidence {
                    via: None,
                    edge: EdgeKind::QueryMatch,
                    label: SourceLabel::Lexical,
                    detail: None,
                }],
            })
            .collect();
        response
    }
}

/// `path`'s functions, each `(name, callees)`, one per line.
fn document(path: &str, functions: &[(&str, &[&str])]) -> RawDocument {
    let sym = |name: &str| format!("rust-analyzer cargo demo 0.1.0 m/{name}().");
    let mut occurrences = Vec::new();
    for (line, (name, callees)) in functions.iter().enumerate() {
        let line = line as u32;
        occurrences.push(RawOccurrence {
            range: (line, 3, line, 6),
            symbol: sym(name),
            roles: 1,
            enclosing: Some((line, 0, line, 200)),
        });
        for (i, callee) in callees.iter().enumerate() {
            let col = 10 + 5 * i as u32;
            occurrences.push(RawOccurrence {
                range: (line, col, line, col + 4),
                symbol: sym(callee),
                roles: 0,
                enclosing: None,
            });
        }
    }
    RawDocument {
        path: path.into(),
        occurrences,
    }
}

fn builtin_graph() -> ScipGraph {
    ScipGraph::build(
        &[
            document(
                "src/search.rs",
                &[
                    ("search", &["parse_query", "rank_hits", "mock_ranker"]),
                    ("parse_query", &["tokenize_query"]),
                    ("rank_hits", &["score_hit", "mock_ranker"]),
                    ("tokenize_query", &[]),
                    ("score_hit", &["boost_exact"]),
                    ("boost_exact", &[]),
                ],
            ),
            document(
                "tests/support.rs",
                &[("mock_ranker", &["fixture_hits"]), ("fixture_hits", &[])],
            ),
        ],
        None,
    )
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut question = "how does search rank hits".to_string();
    let (mut scip_path, mut root, mut out) = (None, None, None);
    let mut args = std::env::args().skip(1);
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .unwrap_or_else(|| panic!("{flag} needs a value"));
        match flag.as_str() {
            "--question" => question = value,
            "--scip" => scip_path = Some(PathBuf::from(value)),
            "--root" => root = Some(PathBuf::from(value)),
            "--out" => out = Some(PathBuf::from(value)),
            other => panic!("unknown flag {other}"),
        }
    }
    let graph = match scip_path {
        Some(path) => {
            let source = root.map(DirSource);
            let source = source
                .as_ref()
                .map(|s| s as &dyn lci_maps::scip_graph::SourceText);
            ScipGraph::load(&path, source).expect("load the SCIP index")
        }
        None => builtin_graph(),
    };
    let scip = ScipMap::with_default_ids(graph);
    let lexical = NameLexical::new(&scip);

    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
    let now = Instant::now();
    let seed_hits = lexical
        .query(
            &MapRequest {
                text: Some(&question),
                seeds: &[],
                limit: 3,
                budget: Budget {
                    deadline: now + Duration::from_secs(5),
                    max_external_calls: 0,
                    max_nodes_touched: u64::MAX,
                },
                params: &serde_json::Value::Null,
                as_of: None,
            },
            &view,
        )
        .await;
    let seeds: Vec<Seed> = seed_hits
        .hits
        .iter()
        .map(|hit| Seed {
            node: hit.node.clone(),
            map: Some(LEXICAL),
        })
        .collect();

    let mut registry = Registry::new(60.0);
    registry.register(Box::new(lexical), 1.0, 20).unwrap();
    registry.register(Box::new(scip), 1.0, 20).unwrap();

    let prune_tests = serde_json::json!({"when": {"path_glob": "tests/**"}, "then": "prune"});
    let rules = |extra: Option<serde_json::Value>| {
        let list: Vec<_> = std::iter::once(prune_tests.clone()).chain(extra).collect();
        RuleList::from_json(&serde_json::Value::Array(list)).expect("the demo rules parse")
    };
    let budget = TraversalBudget {
        pops: 40,
        max_depth: 6,
        deadline: now + Duration::from_secs(5),
        est_tokens: 4000,
        external_calls: 100,
    };
    let stops = StopPolicy {
        every_pops: 2,
        max_stops: 4,
    };
    let mut result = Traversal::new(OnTheFly::new(&registry, &view), seeds, budget)
        .question(question.clone())
        .handler(
            // Without --out the walk is one call. With it, the first stop with
            // a depth-2 node queued ends call 1.
            rules(
                out.as_ref()
                    .map(|_| serde_json::json!({"when": {"depth": {"ge": 2}}, "then": "stop"})),
            ),
            stops,
        )
        .run()
        .await;

    if let Some(out) = out {
        let traces = out.join("traces");
        let first = trace::write_trace(&traces, &result).expect("write the first trace");
        let store = HandleStore::in_dir(out.join("handles"));
        let snapshot = result
            .snapshot
            .as_ref()
            .expect("call 1 should stop with work left");
        let handle = store.put(snapshot).expect("store the handle");
        eprintln!(
            "call 1: {} at pop {}, handle {handle}, trace {}",
            result.status.as_str(),
            result.totals.pops,
            first.display(),
        );
        // Call 2: a store that has never seen the snapshot reads it from disk.
        let snapshot = HandleStore::in_dir(out.join("handles"))
            .get(&handle)
            .expect("read the handle back");
        result = Traversal::resumed(OnTheFly::new(&registry, &view), snapshot, budget)
            .handler(rules(None), stops)
            .run()
            .await;
        let second = trace::write_trace(&traces, &result).expect("write the second trace");
        eprintln!("call 2: trace {}", second.display());
    }

    print!("{}", result.events_jsonl());
    eprintln!(
        "{}: {} pops, {} hops ({} cross-map), {} tokens, {} cleared, {:.1} ms",
        result.status.as_str(),
        result.totals.pops,
        result.totals.hops,
        result.totals.cross_map_hops,
        result.totals.tokens,
        result.cleared.len(),
        result.totals.elapsed_ms,
    );
}
