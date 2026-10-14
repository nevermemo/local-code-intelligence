//! Demo of the traversal engine (spine slices S2 to S4): a lexical map finds
//! the seeds, a SCIP map expands them, and the walk runs under the steering
//! ladder. Rung 1 is one rule (prune `tests/**`), rung 2 is a stand-in for
//! System One (slice S8) that is never sure, and rung 3 is the outer agent:
//! a stop the rules cannot decide ends the call with a checkpoint and a
//! handle. The demo then plays the agent: it reads the checkpoint, stores the
//! handle (under `DIR/handles` with `--out`, else in memory), and calls again
//! with a `Steer` (boost the best queued node, prune the worst, add pops).
//! The second call's trace holds the whole walk and is printed on stdout as
//! JSON lines; a summary goes to stderr.
//!
//! ```text
//! cargo run -p lci-maps --example traverse_demo -- [--question TEXT] [--scip INDEX --root TREE] [--out DIR]
//! ```
//!
//! Both traces are written under `DIR/traces` with `--out`. Without `--scip`
//! it walks a small built-in call graph. With `--scip` it loads that SCIP
//! index (and `--root`, the tree it indexes, for the enclosing-range repair)
//! and walks it with best-effort symbol ids.

use lci_core::{chunk::Chunk, node::NodeId};
use lci_maps::{
    Budget, Contribution, EdgeKind, Evidence, FreshnessModel, Map, MapDescriptor, MapHit,
    MapRequest, MapResponse, MapStatus, Registry, SnapshotRef, SnapshotView, SourceLabel,
    audit::audit_edge_sources,
    scip_graph::{DirSource, RawDocument, RawOccurrence, ScipGraph, ScipMap},
    traverse::{
        Seed, Traversal, TraversalBudget, TraversalResult,
        handle::HandleStore,
        handler::{Action, Checkpoint, Decision, Directive, Handler, StopBudget, StopPolicy},
        ladder::Ladder,
        merged::{BuildOptions, MergedAdjacency, PreMerged},
        rules::RuleList,
        source::{EdgeSource, Expansion, OnTheFly},
        steer::Steer,
        trace,
    },
    walk::tokenize,
};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeSet, HashSet},
    path::{Path, PathBuf},
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
            let source = root.clone().map(DirSource);
            let source = source
                .as_ref()
                .map(|s| s as &dyn lci_maps::scip_graph::SourceText);
            ScipGraph::load(&path, source).expect("load the SCIP index")
        }
        None => builtin_graph(),
    };
    let scip = ScipMap::with_default_ids(graph);
    let lexical = NameLexical::new(&scip);
    let roots: Vec<NodeId> = (0..scip.graph().nodes.len())
        .map(|i| scip.node_id(i).clone())
        .collect();

    // One chunk per indexed file, its content hash the SHA-256 of the file
    // under `--root` (of its path without one): enough for the file blobs
    // the pre-merged source keys on.
    let files: BTreeSet<&str> = scip
        .graph()
        .nodes
        .iter()
        .map(|node| node.file.as_str())
        .collect();
    let chunks: Vec<Chunk> = files
        .iter()
        .map(|path| file_chunk(path, root.as_deref()))
        .collect();
    let view = SnapshotView::new(
        SnapshotRef {
            indexed_at: "demo".into(),
            fingerprint: "demo-1".into(),
        },
        chunks.iter().collect(),
    );
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

    compare_policies(&registry, &view, &chunks, &seeds, &question, &roots, &out).await;

    let rules = RuleList::from_json(&serde_json::json!([
        {"when": {"path_glob": "tests/**"}, "then": "prune"}
    ]))
    .expect("the demo rules parse");
    let ladder = |rules: RuleList| {
        Ladder::with_system_one(0.8, rules, NeverSure).expect("the demo ladder is valid")
    };
    let budget = TraversalBudget {
        pops: 12,
        max_depth: 6,
        deadline: now + Duration::from_secs(5),
        est_tokens: 4000,
        external_calls: 100,
    };
    let stops = StopPolicy::every(2).max_stops(6);

    // Call 1: rung 1 prunes test code while it can; the first stop it has no
    // rule for goes through rung 2 (unsure) to rung 3: the call returns.
    let first = Traversal::new(OnTheFly::new(&registry, &view), seeds, budget)
        .question(question.clone())
        .handler(ladder(rules), stops)
        .run()
        .await;
    for event in first.events.iter().filter(|e| e["ev"] == "steer") {
        eprintln!(
            "stop {}: rung {} {} -> {}",
            event["stop"], event["rung"], event["handler"], event["outcome"]
        );
    }
    let (Some(checkpoint), Some(snapshot)) = (first.checkpoint.as_ref(), first.snapshot.as_ref())
    else {
        eprintln!(
            "the ladder never needed the agent ({})",
            first.status.as_str()
        );
        print!("{}", first.events_jsonl());
        return;
    };

    let (traces, store) = match &out {
        Some(out) => (
            Some(out.join("traces")),
            HandleStore::in_dir(out.join("handles")),
        ),
        None => (None, HandleStore::memory()),
    };
    if let Some(traces) = &traces {
        let path = trace::write_trace(traces, &first).expect("write the first trace");
        eprintln!("call 1 trace {}", path.display());
    }
    let handle = store.put(snapshot).expect("store the handle");
    eprintln!(
        "call 1: {} at pop {} ({}), handle {handle}",
        first.status.as_str(),
        first.totals.pops,
        checkpoint.reason.as_str(),
    );
    for card in checkpoint.answer.iter().take(3) {
        eprintln!("  answer so far: {}", card.text());
    }
    for item in checkpoint.frontier.iter().take(3) {
        eprintln!("  queued: {} (score {:.2})", item.node, item.score);
    }

    // The agent: boost the best queued node, prune the worst, add pops.
    let mut actions = Vec::new();
    if let Some(best) = checkpoint.frontier.first() {
        actions.push(Directive {
            node: Some(best.node.clone()),
            action: Action::Boost(2.0),
        });
    }
    if checkpoint.frontier.len() > 1 {
        actions.push(Directive {
            node: checkpoint.frontier.last().map(|item| item.node.clone()),
            action: Action::Prune,
        });
    }
    actions.push(Directive {
        node: None,
        action: Action::Budget {
            pops: 10,
            tokens: 0,
        },
    });
    // Call 2: a store that has never seen the snapshot reads it back by handle.
    let snapshot = store.get(&handle).expect("read the handle back");
    let result = Traversal::resumed(OnTheFly::new(&registry, &view), snapshot, budget)
        .handler(
            ladder(RuleList::new(Vec::new()).unwrap()),
            stops.max_stops(0),
        )
        .steer(Steer::new(actions))
        .run()
        .await;
    if let Some(traces) = &traces {
        let path = trace::write_trace(traces, &result).expect("write the second trace");
        eprintln!("call 2 trace {}", path.display());
    }

    print!("{}", result.events_jsonl());
    eprintln!(
        "{}: {} pops, {} hops ({} cross-map), {} tokens, {} cleared, {} steers, {:.1} ms",
        result.status.as_str(),
        result.totals.pops,
        result.totals.hops,
        result.totals.cross_map_hops,
        result.totals.tokens,
        result.cleared.len(),
        result.totals.steers,
        result.totals.elapsed_ms,
    );
}

/// The chunk of one file: the whole file as one chunk.
fn file_chunk(path: &str, root: Option<&Path>) -> Chunk {
    let bytes = root
        .and_then(|root| std::fs::read(root.join(path)).ok())
        .unwrap_or_else(|| path.as_bytes().to_vec());
    let hash = Sha256::digest(&bytes)
        .iter()
        .fold(String::new(), |out, byte| format!("{out}{byte:02x}"));
    Chunk {
        relative_file_path: path.to_string(),
        language: "rust".into(),
        start_line: 1,
        end_line: 1,
        code: String::new(),
        content_hash: hash,
    }
}

/// Slice S5: the same question under both edge policies, and what differs.
///
/// Builds the pre-merged adjacency (timed), audits it against the maps on
/// every node it holds, walks the question once per policy and diffs the two
/// traces, then edits one file's blob and updates the adjacency to show how
/// little it asks again. Everything goes to stderr; with `--out` the traces
/// and the diff are written under it.
async fn compare_policies(
    registry: &Registry<'static>,
    view: &SnapshotView<'_>,
    chunks: &[Chunk],
    seeds: &[Seed],
    question: &str,
    roots: &[NodeId],
    out: &Option<PathBuf>,
) {
    let options = BuildOptions {
        roots: roots.to_vec(),
        ..BuildOptions::default()
    };
    let mut adjacency = MergedAdjacency::build_with(registry, view, &options).await;
    let built = adjacency.stats().clone();
    eprintln!(
        "pre-merged build: {} rows ({} asked, {} incomplete) in {:.0} ms over {} file blobs",
        built.nodes,
        built.expanded,
        built.incomplete,
        built.elapsed_ms,
        adjacency.blobs().len()
    );

    let nodes: Vec<NodeId> = adjacency.nodes().cloned().collect();
    match audit_edge_sources(
        &OnTheFly::new(registry, view),
        &PreMerged::new(&adjacency),
        &nodes,
        Duration::from_secs(5),
    )
    .await
    {
        Ok(report) => eprintln!(
            "equivalence audit: {} nodes, {} fused edges, identical",
            report.nodes, report.edges
        ),
        Err(failure) => eprintln!("equivalence audit FAILED: {}", failure.detail),
    }

    let budget = TraversalBudget {
        pops: 200,
        max_depth: 4,
        deadline: Instant::now() + Duration::from_secs(60),
        est_tokens: 100_000,
        external_calls: 1000,
    };
    let fly = walk(
        Box::new(OnTheFly::new(registry, view)),
        seeds,
        question,
        budget,
    )
    .await;
    let merged = walk(
        Box::new(PreMerged::new(&adjacency)),
        seeds,
        question,
        budget,
    )
    .await;
    eprintln!(
        "on-the-fly: {} pops, {} hops, {:.2} ms | pre-merged: {} pops, {} hops, {:.2} ms",
        fly.totals.pops,
        fly.totals.hops,
        fly.totals.elapsed_ms,
        merged.totals.pops,
        merged.totals.hops,
        merged.totals.elapsed_ms,
    );
    let diff = trace_diff(&fly.events, &merged.events);
    eprintln!(
        "trace diff: {} events vs {} events, {} differ",
        fly.events.len(),
        merged.events.len(),
        diff.len()
    );
    for line in diff.iter().take(8) {
        eprintln!("  {line}");
    }
    if let Some(out) = out {
        let traces = out.join("traces");
        for (policy, result) in [("on-the-fly", &fly), ("pre-merged", &merged)] {
            let path = trace::write_trace(&traces, result).expect("write a policy trace");
            eprintln!("{policy} trace {}", path.display());
        }
        std::fs::create_dir_all(out).expect("create the output directory");
        std::fs::write(out.join("trace-diff.txt"), diff.join("\n")).expect("write the diff");
    }

    // One file is edited (its blob moves): the update asks only about the
    // rows that depend on it.
    if let Some(first) = chunks.first() {
        let mut edited: Vec<Chunk> = chunks.to_vec();
        edited[0].content_hash.push_str("-edited");
        let after = SnapshotView::new(
            SnapshotRef {
                indexed_at: "demo".into(),
                fingerprint: "demo-2".into(),
            },
            edited.iter().collect(),
        );
        let stats = adjacency.update(registry, &after, &options).await;
        eprintln!(
            "edit of {}: update asked {} of {} rows ({} reused, {} undated and so always asked), {} blob added, {} removed, {:.0} ms",
            first.relative_file_path,
            stats.expanded,
            stats.nodes,
            stats.reused,
            stats.undated,
            stats.blobs_added,
            stats.blobs_removed,
            stats.elapsed_ms,
        );
        let fresh = MergedAdjacency::build_with(registry, &after, &options).await;
        eprintln!(
            "update equals a build from nothing: {} (full build {:.0} ms)",
            adjacency.canonical() == fresh.canonical(),
            fresh.stats().elapsed_ms
        );
    }
}

/// Events that differ, by position, shortened.
fn trace_diff(a: &[Value], b: &[Value]) -> Vec<String> {
    let short = |event: Option<&Value>| {
        event.map_or("(none)".to_string(), |e| {
            let text = e.to_string();
            text.chars().take(140).collect()
        })
    };
    (0..a.len().max(b.len()))
        .filter(|&i| a.get(i) != b.get(i))
        .map(|i| {
            format!(
                "event {i}: on-the-fly {} | pre-merged {}",
                short(a.get(i)),
                short(b.get(i))
            )
        })
        .collect()
}

/// A boxed source as a source, so one closure can walk either policy.
struct BoxedSource<'a>(Box<dyn EdgeSource + 'a>);

#[lci_maps::async_trait]
impl EdgeSource for BoxedSource<'_> {
    async fn edges(&self, node: &NodeId, budget: Budget) -> Expansion {
        self.0.edges(node, budget).await
    }

    fn card(&self, map: lci_maps::MapId) -> lci_maps::edge::MapCard {
        self.0.card(map)
    }

    fn set_enabled(&mut self, id: &str, on: bool) -> bool {
        self.0.set_enabled(id, on)
    }
}

/// The System One slot (slice S8) filled with a test double that is never
/// sure, so the ladder goes on to the agent.
struct NeverSure;

impl Handler for NeverSure {
    fn id(&self) -> &str {
        "system_one_double"
    }

    fn rung(&self) -> u8 {
        2
    }

    fn decide(&self, _checkpoint: &Checkpoint, _budget: &StopBudget) -> Decision {
        Decision {
            confidence: 0.3,
            ..Decision::default()
        }
    }
}

/// One traversal of `question` over `source`, no handler.
async fn walk(
    source: Box<dyn EdgeSource + '_>,
    seeds: &[Seed],
    question: &str,
    budget: TraversalBudget,
) -> TraversalResult {
    Traversal::new(BoxedSource(source), seeds.to_vec(), budget)
        .question(question)
        .run()
        .await
}
