//! The `scip.implements` map: which types implement which traits, and which
//! members implement which trait methods, as read from the SCIP index and
//! the source text of the tree (the trait-impl facts of
//! [`crate::scip_graph::derive_impls`]; rust-analyzer writes no SCIP
//! relationships).
//!
//! # What an edge says
//!
//! Every edge has kind [`EdgeKind::Implements`], witness
//! [`WitnessClass::Compiler`] and confidence 100. The edge carries no
//! direction mark of its own; the node it leaves tells which way it goes:
//!
//! * from a **type** to each **trait** it implements (`impl Trait for Type`);
//! * from a **trait** to each **type** that implements it;
//! * from a **trait method declaration** to each **member** that implements
//!   it (`impl Trait for Type { fn name() }`).
//!
//! The site is the trait's name in the `impl` header (path, 0-based line and
//! column, no blob: an edge source fills it).
//!
//! # What is left out
//!
//! * A trait or type the index does not define (`Default`, `Display`,
//!   `Serialize`, a type of another crate) has no node id, so its edges are
//!   dropped; [`ImplementsStats::dropped`] counts the impl facts for it.
//! * `#[derive(..)]` impls: the derive writes no header. Impls written inside
//!   a `macro_rules!` body: the header holds a `$`, no trait is read.
//! * Headers whose source text was not available (the build report of the
//!   graph counts the documents).
//! * Default methods a type does not write: the member does not exist.
//!
//! # Ids
//!
//! The caller's `resolve` gives the node id of a SCIP symbol, the type or
//! trait (ids of [`crate::scip_impact::ImpactMap`], so the edges meet that
//! map's nodes) or a method (ids of `graph.scip`).
//!
//! # Cap
//!
//! At most [`ImplementsOptions::cap`] edges (default 64) per call, in the
//! order of [`crate::edge::sort_edges`]; `query` says what the cap left out.
//!
//! # Freshness
//!
//! The index and the source are read once, when the graph is loaded; a tree
//! edit after that is not seen. The model is [`FreshnessModel::PerFile`], like
//! `graph.scip`.

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, Evidence, FreshnessModel, Map,
    MapDescriptor, MapHit, MapId, MapRequest, MapResponse, MapStatus, SnapshotView, SourceLabel,
    edge::{CONF_MAX, Edge, Expanded, MapCard, QuestionType, Site, WitnessClass, cap_edges},
    live_grep::CostLog,
    scip_graph::{GraphEdgeKind, ScipGraph, Witness},
};
use lci_core::node::{NodeId, NodeKind};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Instant,
};

/// The id of [`ImplementsMap`].
pub const SCIP_IMPLEMENTS_ID: MapId = "scip.implements";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImplementsOptions {
    /// Most edges one call offers.
    pub cap: usize,
}

impl Default for ImplementsOptions {
    fn default() -> Self {
        Self { cap: 64 }
    }
}

/// What building the map found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImplementsStats {
    /// Trait-impl facts in the graph.
    pub facts: usize,
    /// Distinct (type, trait) pairs with an id at both ends (each gives an
    /// edge in either direction).
    pub type_trait: usize,
    /// (trait method declaration, member) pairs with an id at both ends.
    pub methods: usize,
    /// Facts with no id at one end (an external trait or type), or no type.
    pub dropped: usize,
}

pub struct ImplementsMap {
    descriptor: MapDescriptor,
    options: ImplementsOptions,
    /// Every edge, by the node it leaves (unsorted, uncapped).
    edges: HashMap<NodeId, Vec<Edge>>,
    stats: ImplementsStats,
    log: CostLog,
}

fn edge(src: NodeId, dst: NodeId, at: &Witness) -> Edge {
    Edge {
        src,
        dst,
        kind: EdgeKind::Implements,
        map: SCIP_IMPLEMENTS_ID,
        witness: WitnessClass::Compiler,
        conf: CONF_MAX,
        site: Some(Site {
            path: at.file.clone(),
            blob: String::new(),
            line: at.line,
            col: at.col,
        }),
    }
}

impl ImplementsMap {
    /// A map over the trait-impl facts and `implements` edges of `graph`.
    /// `resolve` gives the node id of a SCIP symbol; a symbol it does not know
    /// (an external trait or type) drops its edges.
    pub fn new(
        graph: &ScipGraph,
        resolve: &dyn Fn(&str) -> Option<NodeId>,
        options: ImplementsOptions,
    ) -> Self {
        let mut stats = ImplementsStats {
            facts: graph.impls.len(),
            ..ImplementsStats::default()
        };
        let mut edges: HashMap<NodeId, Vec<Edge>> = HashMap::new();
        let mut pairs: BTreeMap<(&str, &str), &Witness> = BTreeMap::new();
        for fact in &graph.impls {
            let Some(type_sym) = fact.type_sym.as_deref() else {
                stats.dropped += 1;
                continue;
            };
            pairs
                .entry((type_sym, fact.trait_sym.as_str()))
                .or_insert(&fact.site);
        }
        let mut dropped_pairs: HashSet<(&str, &str)> = HashSet::new();
        for (&(type_sym, trait_sym), &site) in &pairs {
            match (resolve(type_sym), resolve(trait_sym)) {
                (Some(ty), Some(tr)) => {
                    stats.type_trait += 1;
                    edges
                        .entry(ty.clone())
                        .or_default()
                        .push(edge(ty.clone(), tr.clone(), site));
                    edges
                        .entry(tr.clone())
                        .or_default()
                        .push(edge(tr, ty, site));
                }
                _ => {
                    dropped_pairs.insert((type_sym, trait_sym));
                }
            }
        }
        stats.dropped += graph
            .impls
            .iter()
            .filter(|f| {
                f.type_sym
                    .as_deref()
                    .is_some_and(|t| dropped_pairs.contains(&(t, f.trait_sym.as_str())))
            })
            .count();
        for (member, out) in graph.out.iter().enumerate() {
            for e in out.iter().filter(|e| e.kind == GraphEdgeKind::Implements) {
                if let (Some(m), Some(decl)) = (
                    resolve(&graph.nodes[member].symbol),
                    resolve(&graph.nodes[e.to].symbol),
                ) {
                    stats.methods += 1;
                    edges
                        .entry(decl.clone())
                        .or_default()
                        .push(edge(decl, m, &e.witness));
                }
            }
        }
        Self {
            descriptor: MapDescriptor {
                id: SCIP_IMPLEMENTS_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::Symbol],
                emits: vec![NodeKind::Symbol],
                freshness: FreshnessModel::PerFile,
                source_labels: vec![SourceLabel::Scip],
                needs: Vec::new(),
            },
            options,
            edges,
            stats,
            log: CostLog::default(),
        }
    }

    /// A map with no edges (an index that failed to load, kept so the plan
    /// lists it).
    pub fn empty(options: ImplementsOptions) -> Self {
        Self::new(&ScipGraph::build(&[], None), &|_| None, options)
    }

    pub fn stats(&self) -> ImplementsStats {
        self.stats
    }

    pub fn options(&self) -> ImplementsOptions {
        self.options
    }

    pub fn log(&self) -> &CostLog {
        &self.log
    }

    /// The edges leaving `node`, sorted and capped, and how many there were
    /// before the cap.
    fn edges_of(&self, node: &NodeId) -> (Expanded, usize) {
        let started = Instant::now();
        let mut edges = self.edges.get(node).cloned().unwrap_or_default();
        let total = edges.len();
        cap_edges(&mut edges, self.options.cap.min(EXPAND_LIMIT));
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.log.record(elapsed_ms);
        let cost = Cost {
            elapsed_ms,
            nodes_touched: total as u64,
            ..Cost::default()
        };
        (Expanded::new(edges, cost), total)
    }
}

#[crate::async_trait]
impl Map for ImplementsMap {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let started = Instant::now();
        let mut edges = Vec::new();
        let mut dropped = 0;
        for (seed, _) in request.seeds {
            let (found, total) = self.edges_of(seed);
            dropped += total - found.edges.len();
            edges.extend(found.edges);
        }
        cap_edges(&mut edges, usize::MAX);
        let mut seen = HashSet::new();
        edges.retain(|edge| seen.insert(edge.dst.clone()));
        edges.truncate(request.limit);
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        if dropped > 0 {
            response.status = MapStatus::Partial {
                note: Some(format!(
                    "{dropped} more implementations than the cap of {} were not offered",
                    self.options.cap
                )),
                warning: None,
            };
        }
        response.cost = Cost {
            elapsed_ms: started.elapsed().as_secs_f64() * 1000.0,
            nodes_touched: edges.len() as u64,
            ..Cost::default()
        };
        response.hits = edges
            .into_iter()
            .zip(1..)
            .map(|(edge, rank)| MapHit {
                node: edge.dst,
                rank,
                score: Some(f32::from(edge.conf) / f32::from(CONF_MAX)),
                evidence: vec![Evidence {
                    via: Some(edge.src),
                    edge: edge.kind,
                    label: SourceLabel::Scip,
                    detail: edge
                        .site
                        .map(|s| format!("implements at {}:{}", s.path, s.line + 1)),
                }],
            })
            .collect();
        response
    }

    async fn expand(&self, node: &NodeId, _budget: Budget, _view: &SnapshotView<'_>) -> Expanded {
        self.edges_of(node).0
    }

    fn card(&self) -> MapCard {
        MapCard {
            expand_p50_ms: self.log.p50(),
            coverage: self.descriptor.emits.clone(),
            freshness_lag: None,
            // The same SCIP index as `graph.scip`.
            dependent: true,
            good_for: vec![QuestionType::Impact, QuestionType::Flow],
        }
    }
}

#[cfg(test)]
mod tests;
