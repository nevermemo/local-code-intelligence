//! The `config.key` map (multimap wave W0a): the `reads_key` translation of
//! slice M1.2 ([`crate::reads_key`]) as a traversal map. A config key node
//! (`cfg:toml:section.key`) is joined to the declarations that read or set it,
//! in both directions: from the key to each declaration, and from a
//! declaration to each key it touches. Both are [`EdgeKind::ReadsKey`] edges
//! with the declaration's name line as the [`Site`].
//!
//! # What an edge says
//!
//! The extractor gives each (key, declaration) pair two witnesses: W1, the
//! SCIP closure of the key's field symbol, and W2/L, bare-name `rg` and TOML
//! text inside string literals. A pair with W1 has witness
//! [`WitnessClass::Exact`]; a pair with only W2 or L has
//! [`WitnessClass::Text`], and those edges are **off by default**
//! ([`ConfigKeyOptions::text_edges`], also `params.text_edges` of a query):
//! M1.2 measured the text class as the source of nearly all wrong edges.
//! Confidence is the extractor's per-class placeholder (100 when both W1 and
//! W2 hold, 80 W1 only, 35 W2, 15 L only), not a measured precision.
//!
//! The scope is the one M1.2 claims: this project's own `serde` config tree
//! and `config.example.toml` (the key table), on the tree it was built from.
//! Roles (read, write, init) and the test flag are not carried by the typed
//! edge, and edges into test code are kept.
//!
//! # Building and freshness
//!
//! [`ConfigKeyMap::extract`] runs the M1.2 extractor once (it needs the SCIP
//! index, the tree's source and `rg` on `PATH`, one `rg` run per key) and
//! stores the edges; `expand` is a table lookup. The edges are the tree's at
//! build time, so the model is [`FreshnessModel::Global`]: a pre-merged
//! adjacency asks again when the snapshot changes, and this map does not see
//! an edit made after it was built.
//!
//! # Site lines
//!
//! `reads_key` keeps a declaration's line 1-based ([`crate::impact::Decl`]);
//! a [`Site`] counts lines from 0, as SCIP and `graph.scip` do, so the line
//! here is one less.

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, Evidence, FreshnessModel, Map, MapDescriptor, MapHit,
    MapId, MapRequest, MapResponse, MapStatus, SnapshotView, SourceLabel,
    edge::{Edge, Expanded, MapCard, QuestionType, WitnessClass, cap_edges},
    impact::{Decl, ImpactIndex},
    live_grep::CostLog,
    reads_key,
    scip_graph::SourceText,
};
use lci_core::node::{NodeId, NodeKind};
use std::{collections::HashMap, path::Path, time::Instant};

/// The id of [`ConfigKeyMap`].
pub const CONFIG_KEY_MAP_ID: MapId = "config.key";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ConfigKeyOptions {
    /// Also return the edges whose only witnesses are text (witness
    /// [`WitnessClass::Text`]).
    pub text_edges: bool,
}

/// What building the map found.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ConfigKeyStats {
    /// Keys in the key table.
    pub keys: usize,
    /// (key, declaration) pairs by witness class.
    pub exact: usize,
    pub text: usize,
    /// How long the extraction took; zero for a map built from edges.
    pub build_ms: f64,
}

pub struct ConfigKeyMap {
    descriptor: MapDescriptor,
    options: ConfigKeyOptions,
    /// Edges by source node, both directions, sorted.
    out: HashMap<NodeId, Vec<Edge>>,
    stats: ConfigKeyStats,
    log: CostLog,
}

impl ConfigKeyMap {
    /// A map over already extracted `edges`. `resolve` maps a declaration to
    /// the node id the other maps use for it (for example the SCIP graph's);
    /// a declaration it does not know keeps the id [`reads_key`] gives it.
    pub fn new(
        edges: &[reads_key::Edge],
        resolve: &dyn Fn(&Decl) -> Option<NodeId>,
        options: ConfigKeyOptions,
    ) -> Self {
        let mut out: HashMap<NodeId, Vec<Edge>> = HashMap::new();
        let mut stats = ConfigKeyStats::default();
        for raw in edges {
            let mut forward = Edge::from(raw);
            forward.map = CONFIG_KEY_MAP_ID;
            if let Some(site) = forward.site.as_mut() {
                site.line = site.line.saturating_sub(1);
            }
            if let Some(dst) = resolve(&raw.decl) {
                forward.dst = dst;
            }
            match forward.witness {
                WitnessClass::Exact => stats.exact += 1,
                _ => stats.text += 1,
            }
            let mut reverse = forward.clone();
            reverse.src = forward.dst.clone();
            reverse.dst = forward.src.clone();
            out.entry(forward.src.clone()).or_default().push(forward);
            out.entry(reverse.src.clone()).or_default().push(reverse);
        }
        for list in out.values_mut() {
            crate::edge::sort_edges(list);
            list.dedup();
        }
        Self {
            descriptor: MapDescriptor {
                id: CONFIG_KEY_MAP_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::ConfigKey, NodeKind::Symbol],
                emits: vec![NodeKind::ConfigKey, NodeKind::Symbol],
                freshness: FreshnessModel::Global,
                source_labels: vec![SourceLabel::Config],
                needs: Vec::new(),
            },
            options,
            out,
            stats,
            log: CostLog::default(),
        }
    }

    /// A map with no edges (a build that failed, kept so the plan lists it).
    pub fn empty(options: ConfigKeyOptions) -> Self {
        Self::new(&[], &|_| None, options)
    }

    /// Runs the M1.2 extractor over the tree at `root` with the SCIP index
    /// `ix`, then builds the map. Needs `rg` on `PATH`.
    pub fn extract(
        ix: &ImpactIndex,
        root: &Path,
        source: &dyn SourceText,
        resolve: &dyn Fn(&Decl) -> Option<NodeId>,
        options: ConfigKeyOptions,
    ) -> Result<Self, String> {
        let started = Instant::now();
        let table = reads_key::build_key_table(ix, source);
        let edges = reads_key::extract(ix, root, source, &table)?;
        let mut map = Self::new(&edges, resolve, options);
        map.stats.keys = table.len();
        map.stats.build_ms = started.elapsed().as_secs_f64() * 1000.0;
        Ok(map)
    }

    pub fn stats(&self) -> ConfigKeyStats {
        self.stats
    }

    pub fn options(&self) -> ConfigKeyOptions {
        self.options
    }

    /// Nodes that have at least one edge.
    pub fn nodes(&self) -> usize {
        self.out.len()
    }

    pub fn log(&self) -> &CostLog {
        &self.log
    }

    fn edges_of(&self, node: &NodeId, text_edges: bool) -> Vec<Edge> {
        let mut edges: Vec<Edge> = self
            .out
            .get(node)
            .into_iter()
            .flatten()
            .filter(|edge| text_edges || edge.witness == WitnessClass::Exact)
            .cloned()
            .collect();
        cap_edges(&mut edges, EXPAND_LIMIT);
        edges
    }
}

#[crate::async_trait]
impl Map for ConfigKeyMap {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let started = Instant::now();
        let text_edges = request
            .params
            .get("text_edges")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(self.options.text_edges);
        let mut edges = Vec::new();
        for (seed, _) in request.seeds {
            edges.extend(self.edges_of(seed, text_edges));
        }
        cap_edges(&mut edges, usize::MAX);
        let mut seen = std::collections::HashSet::new();
        edges.retain(|edge| seen.insert(edge.dst.clone()));
        edges.truncate(request.limit);
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        response.cost.nodes_touched = edges.len() as u64;
        response.hits = edges
            .into_iter()
            .zip(1..)
            .map(|(edge, rank)| MapHit {
                node: edge.dst,
                rank,
                score: Some(f32::from(edge.conf) / 100.0),
                evidence: vec![Evidence {
                    via: Some(edge.src),
                    edge: edge.kind,
                    label: SourceLabel::Config,
                    detail: None,
                }],
            })
            .collect();
        response.cost.elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        response
    }

    async fn expand(&self, node: &NodeId, _budget: Budget, _view: &SnapshotView<'_>) -> Expanded {
        let started = Instant::now();
        let edges = self.edges_of(node, self.options.text_edges);
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.log.record(elapsed_ms);
        let cost = Cost {
            elapsed_ms,
            nodes_touched: edges.len() as u64,
            ..Cost::default()
        };
        Expanded::new(edges, cost)
    }

    fn card(&self) -> MapCard {
        MapCard {
            expand_p50_ms: self.log.p50(),
            coverage: self.descriptor.emits.clone(),
            freshness_lag: None,
            good_for: vec![QuestionType::Flow, QuestionType::WhereTo],
        }
    }
}

#[cfg(test)]
mod tests;
