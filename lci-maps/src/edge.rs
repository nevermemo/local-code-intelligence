//! The typed edge that the traversal engine follows: what [`crate::Map::expand`]
//! returns, with the evidence class of its witness (History: multimap spine
//! slice S1).
//!
//! Rows are sorted by confidence (high first), then destination [`NodeId`]
//! ([`sort_edges`]). A prefix of a sorted list is a cap, not a rank (design
//! rule 1): a consumer that wants a different order computes it at walk time.

use crate::{
    Cost, EdgeKind, MapDescriptor, MapId, MapResponse, MapStatus, NodeKind, Observed, SourceLabel,
};
use lci_core::node::NodeId;
use std::{cmp::Reverse, time::Duration};

/// How far a witness can be trusted, from the strongest to the weakest
/// source. Consumers read `Compiler` and `Exact` by default and report
/// `Text` separately (M1.2: text precision 0.034).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WitnessClass {
    /// The compiler or a compiler-grade index (SCIP, `cargo check`).
    Compiler,
    /// A language server answer.
    Lsp,
    /// An exact translation table (config key, git object).
    Exact,
    /// Text search or literal matching.
    Text,
    /// Embedding similarity.
    Vector,
    /// A rule of thumb or a prior.
    Heuristic,
    /// Asserted by an agent.
    Agent,
}

impl WitnessClass {
    /// Every class, strongest first.
    pub const ALL: [WitnessClass; 7] = [
        Self::Compiler,
        Self::Lsp,
        Self::Exact,
        Self::Text,
        Self::Vector,
        Self::Heuristic,
        Self::Agent,
    ];

    /// The class named `name` (the inverse of [`WitnessClass::as_str`]).
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == name)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Compiler => "compiler",
            Self::Lsp => "lsp",
            Self::Exact => "exact",
            Self::Text => "text",
            Self::Vector => "vector",
            Self::Heuristic => "heuristic",
            Self::Agent => "agent",
        }
    }

    /// The class a map's evidence label implies when the map names no
    /// finer witness.
    pub const fn of_label(label: SourceLabel) -> Self {
        match label {
            SourceLabel::Scip => Self::Compiler,
            SourceLabel::Lsp => Self::Lsp,
            SourceLabel::Git | SourceLabel::Config => Self::Exact,
            SourceLabel::Lexical | SourceLabel::Doc => Self::Text,
            SourceLabel::Vector => Self::Vector,
            SourceLabel::Tags | SourceLabel::Heuristic | SourceLabel::Prior => Self::Heuristic,
            SourceLabel::Agent => Self::Agent,
        }
    }

    /// Whether a consumer reads this class by default.
    pub const fn read_by_default(self) -> bool {
        matches!(self, Self::Compiler | Self::Exact)
    }
}

/// Where an edge was observed. A map knows the `path` and leaves `blob`
/// empty; an edge source fills `blob` from the snapshot view
/// ([`crate::traverse::merged::FileBlobs`]): a hash of the file's content, as
/// the snapshot holds it, never a path. `blob` stays empty for a file the
/// view does not hold.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Site {
    pub path: String,
    pub blob: String,
    pub line: u32,
    pub col: u32,
}

/// The largest confidence: whole percent, so `conf` orders without floats.
pub const CONF_MAX: u8 = 100;
/// The confidence of an edge whose map gave no score.
pub const CONF_UNKNOWN: u8 = 50;

/// A unit-interval score as whole-percent confidence (clamped; NaN is
/// unknown).
pub fn conf_from_unit(score: f32) -> u8 {
    if score.is_nan() {
        return CONF_UNKNOWN;
    }
    (score.clamp(0.0, 1.0) * f32::from(CONF_MAX)).round() as u8
}

#[derive(Debug, Clone, PartialEq)]
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub kind: EdgeKind,
    pub map: MapId,
    pub witness: WitnessClass,
    /// Whole percent, `0..=CONF_MAX`.
    pub conf: u8,
    pub site: Option<Site>,
}

/// How current one map's answer is, as the map or the code that built it
/// knows it. Not [`crate::Freshness`], the per-query label of a
/// [`crate::MapResponse`] (model and snapshot identity): this one is a call's
/// short statement of what the answer was read from and how far behind it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Freshness {
    /// What the answer was read from: the map's stored snapshot, the working
    /// tree as it is now, or a named point in time. (`lsp.live` is a live
    /// map that queries a language server: the server reads the tree itself
    /// when the call started it; a server kept across calls is bounded by its
    /// tree stamp's coverage, and its answer stays unverified.)
    pub observed: Observed,
    /// The source the answer was read from is known to differ from the tree
    /// now. `false` also when nothing was compared (`lag` is then `None`).
    pub stale: bool,
    /// How many files differ between what the answer was read from and the
    /// tree now (changed, added or removed), a count and never wall time;
    /// `None` when it was not measured. A live map reads the tree itself: `0`
    /// (for `lsp.live` that holds for a server the call started, not for a
    /// kept one).
    pub lag: Option<u32>,
}

impl Freshness {
    /// A map that reads the working tree (or git) as it is when it answers.
    /// (`lsp.live` states it too, for a server the call started; the walk
    /// labels a kept server's cards `unverified` and says so in a note.)
    pub const fn live() -> Self {
        Self {
            observed: Observed::LiveTree,
            stale: false,
            lag: Some(0),
        }
    }

    /// A map that answers from stored data built from the tree at some point;
    /// `lag` is the number of files that differ from that point, when known.
    pub const fn snapshot(lag: Option<u32>) -> Self {
        Self {
            observed: Observed::Snapshot,
            stale: matches!(lag, Some(1..)),
            lag,
        }
    }
}

/// What one [`crate::Map::expand`] call returns: the edges and what the call
/// cost. The cost belongs to the call, so it is reported once however many
/// edges come back, and an expansion with no edges still has one.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Expanded {
    pub edges: Vec<Edge>,
    pub cost: Cost,
    /// What the map says about the freshness of this answer; `None` when it
    /// does not say. A map that knows its source snapshot sets it, and so
    /// does the layer that built the map for it (`[run_profile.traverse] map_freshness`).
    pub freshness: Option<Freshness>,
}

impl Expanded {
    pub fn new(edges: Vec<Edge>, cost: Cost) -> Self {
        Self {
            edges,
            cost,
            freshness: None,
        }
    }

    /// Edges of a call that reports no cost.
    pub fn free(edges: Vec<Edge>) -> Self {
        Self::new(edges, Cost::default())
    }

    /// The same answer with its freshness stated.
    pub fn with_freshness(mut self, freshness: Freshness) -> Self {
        self.freshness = Some(freshness);
        self
    }
}

/// Sorts by confidence (high first), then destination, kind, map, witness,
/// site and source. A total order, so equal inputs give equal output.
pub fn sort_edges(edges: &mut [Edge]) {
    edges.sort_by(|a, b| {
        (
            Reverse(a.conf),
            &a.dst,
            a.kind,
            a.map,
            a.witness,
            &a.site,
            &a.src,
        )
            .cmp(&(
                Reverse(b.conf),
                &b.dst,
                b.kind,
                b.map,
                b.witness,
                &b.site,
                &b.src,
            ))
    });
}

/// Sorts, then keeps the first `cap` edges.
pub fn cap_edges(edges: &mut Vec<Edge>, cap: usize) {
    sort_edges(edges);
    edges.truncate(cap);
}

/// The edges a seeded query's answer stands for, as [`crate::Map::expand`]'s
/// default yields them: one per hit that is not the seed itself, witness from
/// the hit's evidence label (the descriptor's first label when the hit has
/// none), confidence from the hit's score. A failed or skipped answer has no
/// edges, as in the registry. Sorted and capped at `cap`.
pub fn edges_from_response(
    node: &NodeId,
    descriptor: &MapDescriptor,
    response: &MapResponse,
    cap: usize,
) -> Vec<Edge> {
    if matches!(
        response.status,
        MapStatus::Failed { .. } | MapStatus::Skipped { .. }
    ) {
        return Vec::new();
    }
    let fallback = descriptor.source_labels.first().copied();
    let mut edges: Vec<Edge> = response
        .hits
        .iter()
        .filter(|hit| hit.node != *node)
        .map(|hit| {
            let evidence = hit.evidence.first();
            let label = evidence.map(|e| e.label).or(fallback);
            Edge {
                src: node.clone(),
                dst: hit.node.clone(),
                kind: evidence.map_or(EdgeKind::QueryMatch, |e| e.edge),
                map: descriptor.id,
                witness: label.map_or(WitnessClass::Heuristic, WitnessClass::of_label),
                conf: hit.score.map_or(CONF_UNKNOWN, conf_from_unit),
                site: None,
            }
        })
        .collect();
    cap_edges(&mut edges, cap);
    edges
}

/// The questions a gold set asks; what a card's `good_for` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuestionType {
    Impact,
    Flow,
    WhereTo,
    History,
    Instructions,
}

/// What a map says about itself to a traversal that prices hops. A stub:
/// only `coverage` is known from the descriptor; the rest is filled from the
/// benchmark trend file later.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MapCard {
    /// Median cost of one `expand`, in milliseconds.
    pub expand_p50_ms: Option<f64>,
    /// Node kinds the map emits.
    pub coverage: Vec<NodeKind>,
    /// How far behind the snapshot the map typically is.
    pub freshness_lag: Option<Duration>,
    pub good_for: Vec<QuestionType>,
    /// The map derives its evidence from the same graph as the other maps
    /// that say so (the SCIP index and the containment map built from it), so
    /// together they corroborate once, not twice.
    pub dependent: bool,
}

impl MapCard {
    pub fn from_descriptor(descriptor: &MapDescriptor) -> Self {
        Self {
            coverage: descriptor.emits.clone(),
            ..Self::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Budget, Contribution, Evidence, FreshnessModel, Map, MapHit, SnapshotRef, SnapshotView,
        kit::FixedMap, kit_c3::check_edge_invariants,
    };
    use std::time::Instant;

    fn file(path: &str) -> NodeId {
        NodeId::file(path).unwrap()
    }

    fn edge(src: &str, dst: &str, conf: u8) -> Edge {
        Edge {
            src: file(src),
            dst: file(dst),
            kind: EdgeKind::CallsOrRefs,
            map: "m",
            witness: WitnessClass::Compiler,
            conf,
            site: None,
        }
    }

    fn descriptor(labels: Vec<SourceLabel>) -> MapDescriptor {
        MapDescriptor {
            id: "test.fixed",
            version: 1,
            contribution: Contribution::Expansion,
            accepts_text: false,
            accepts: vec![NodeKind::File],
            emits: vec![NodeKind::File],
            freshness: FreshnessModel::PerFile,
            source_labels: labels,
            needs: Vec::new(),
        }
    }

    fn budget() -> Budget {
        Budget {
            deadline: Instant::now() + Duration::from_secs(5),
            max_external_calls: u32::MAX,
            max_nodes_touched: u64::MAX,
        }
    }

    fn hit(path: &str, rank: u32, score: Option<f32>, label: SourceLabel) -> MapHit {
        MapHit {
            node: file(path),
            rank,
            score,
            evidence: vec![Evidence {
                via: None,
                edge: EdgeKind::Mentions,
                label,
                detail: None,
            }],
        }
    }

    #[test]
    fn sort_is_confidence_desc_then_node_and_a_prefix_is_a_cap() {
        let mut edges = vec![
            edge("s", "c", 50),
            edge("s", "b", 80),
            edge("s", "a", 50),
            edge("s", "d", 100),
        ];
        sort_edges(&mut edges);
        let order: Vec<_> = edges.iter().map(|e| (e.conf, e.dst.to_string())).collect();
        assert_eq!(
            order,
            [
                (100, "file:d".to_string()),
                (80, "file:b".to_string()),
                (50, "file:a".to_string()),
                (50, "file:c".to_string()),
            ]
        );
        // The cap keeps exactly the prefix of the sorted list.
        let mut capped = edges.clone();
        cap_edges(&mut capped, 2);
        assert_eq!(capped, edges[..2]);
        // Sorting is idempotent and independent of the input order.
        let mut reversed: Vec<_> = edges.iter().rev().cloned().collect();
        cap_edges(&mut reversed, usize::MAX);
        assert_eq!(reversed, edges);
    }

    #[test]
    fn invariant_check_rejects_unsorted_oversized_and_foreign_edges() {
        let desc = descriptor(vec![SourceLabel::Scip]);
        let node = file("s");
        let mut ok = vec![edge("s", "a", 90), edge("s", "b", 10)];
        ok.iter_mut().for_each(|e| e.map = "test.fixed");
        assert_eq!(check_edge_invariants(&node, &desc, &ok, 2), Ok(()));

        let broken = |edges: &[Edge], cap: usize| {
            check_edge_invariants(&node, &desc, edges, cap)
                .expect_err("must fail")
                .contract
        };
        let mut unsorted = ok.clone();
        unsorted.reverse();
        assert_eq!(broken(&unsorted, 2), "C7");
        assert_eq!(broken(&ok, 1), "C7");
        let mut foreign_src = ok.clone();
        foreign_src[0].src = file("other");
        assert_eq!(broken(&foreign_src, 2), "C7");
        let mut foreign_map = ok.clone();
        foreign_map[0].map = "someone.else";
        assert_eq!(broken(&foreign_map, 2), "C7");
        let mut over = ok.clone();
        over[0].conf = CONF_MAX + 1;
        assert_eq!(broken(&over, 2), "C7");
    }

    #[tokio::test]
    async fn default_expand_is_the_seeded_query() {
        let map = FixedMap {
            descriptor: descriptor(vec![SourceLabel::Lexical]),
            hits: vec![
                hit("b", 1, Some(0.4), SourceLabel::Vector),
                hit("a", 2, Some(0.9), SourceLabel::Lsp),
                hit("s", 3, Some(1.0), SourceLabel::Lsp), // the seed itself
                hit("c", 4, None, SourceLabel::Lexical),
            ],
        };
        let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
        let node = file("s");

        let edges = map.expand(&node, budget(), &view).await.edges;

        // The query the default asks: the node as the only seed, no text.
        let seeds = [(node.clone(), 1.0)];
        let params = serde_json::Value::Null;
        let response = map
            .query(
                &crate::MapRequest {
                    text: None,
                    seeds: &seeds,
                    limit: crate::EXPAND_LIMIT,
                    budget: budget(),
                    params: &params,
                    as_of: None,
                },
                &view,
            )
            .await;
        let mut want: Vec<_> = response
            .hits
            .iter()
            .filter(|h| h.node != node)
            .map(|h| h.node.clone())
            .collect();
        want.sort();
        let mut got: Vec<_> = edges.iter().map(|e| e.dst.clone()).collect();
        got.sort();
        assert_eq!(got, want);

        // Sorted by confidence: a 90, c unknown 50, b 40.
        let order: Vec<_> = edges.iter().map(|e| (e.dst.to_string(), e.conf)).collect();
        assert_eq!(
            order,
            [
                ("file:a".to_string(), 90),
                ("file:c".to_string(), 50),
                ("file:b".to_string(), 40)
            ]
        );
        // Witness from each hit's evidence label.
        let witness = |dst: &str| {
            edges
                .iter()
                .find(|e| e.dst == file(dst))
                .map(|e| e.witness)
                .unwrap()
        };
        assert_eq!(witness("a"), WitnessClass::Lsp);
        assert_eq!(witness("b"), WitnessClass::Vector);
        assert_eq!(witness("c"), WitnessClass::Text);
        assert!(edges.iter().all(|e| e.kind == EdgeKind::Mentions));
        assert_eq!(
            check_edge_invariants(&node, map.describe(), &edges, crate::EXPAND_LIMIT),
            Ok(())
        );
    }

    #[tokio::test]
    async fn default_expand_falls_back_to_descriptor_label_and_ignores_failures() {
        let mut bare = hit("a", 1, Some(1.0), SourceLabel::Scip);
        bare.evidence.clear();
        let map = FixedMap {
            descriptor: descriptor(vec![SourceLabel::Scip]),
            hits: vec![bare],
        };
        let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
        let edges = map.expand(&file("s"), budget(), &view).await.edges;
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].witness, WitnessClass::Compiler);
        assert_eq!(edges[0].kind, EdgeKind::QueryMatch);

        let failed = MapResponse {
            status: MapStatus::Failed {
                warning: "down".into(),
            },
            hits: vec![hit("a", 1, None, SourceLabel::Scip)],
            ..MapResponse::empty(map.describe(), &view, MapStatus::Ok)
        };
        assert!(edges_from_response(&file("s"), map.describe(), &failed, 10).is_empty());
    }

    #[test]
    fn card_stub_reports_coverage_from_the_descriptor() {
        let card = MapCard::from_descriptor(&descriptor(vec![SourceLabel::Scip]));
        assert_eq!(card.coverage, vec![NodeKind::File]);
        assert!(card.expand_p50_ms.is_none() && card.good_for.is_empty());
    }

    #[test]
    fn a_stored_answer_is_stale_only_when_a_difference_was_counted() {
        assert!(!Freshness::snapshot(Some(0)).stale);
        assert!(!Freshness::snapshot(None).stale, "nothing compared");
        let behind = Freshness::snapshot(Some(3));
        assert!(behind.stale && behind.lag == Some(3));
        assert_eq!(behind.observed, Observed::Snapshot);
        let live = Freshness::live();
        assert!(!live.stale && live.lag == Some(0));
        assert_eq!(live.observed, Observed::LiveTree);
        // An expansion states nothing unless it is told to.
        assert_eq!(Expanded::free(Vec::new()).freshness, None);
        assert_eq!(Expanded::default().freshness, None);
        let stated = Expanded::default().with_freshness(live.clone());
        assert_eq!(stated.freshness, Some(live));
    }

    #[test]
    fn default_read_classes_are_compiler_and_exact() {
        let read: Vec<_> = WitnessClass::ALL
            .into_iter()
            .filter(|w| w.read_by_default())
            .collect();
        assert_eq!(read, [WitnessClass::Compiler, WitnessClass::Exact]);
        assert_eq!(
            WitnessClass::of_label(SourceLabel::Scip),
            WitnessClass::Compiler
        );
        assert_eq!(conf_from_unit(f32::NAN), CONF_UNKNOWN);
        assert_eq!(conf_from_unit(2.0), CONF_MAX);
        assert_eq!(conf_from_unit(0.5), 50);
    }
}
