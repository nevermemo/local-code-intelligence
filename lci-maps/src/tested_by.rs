//! The `tests.tested_by` map: from a symbol, the test units that reference
//! it, read from the SCIP impact index that `scip.impact` already loads.
//!
//! # What an edge says
//!
//! An edge from `S` to `T` of kind [`EdgeKind::Tests`] says that the unit `T`
//! is a test unit and holds a non-definition occurrence of `S`. Like
//! `scip.impact` the direction is the reverse of `graph.scip`'s (`T` refers to
//! `S`), the site is the first occurrence of `S` inside `T` (path, 0-based
//! line and column, no blob), the witness is [`WitnessClass::Compiler`] and the
//! confidence 100. The destination stays a [`NodeKind::Symbol`] so
//! `graph.scip` and `structure.contains` can expand a test unit like any other
//! and the impact gold matches it by symbol.
//!
//! # What a test unit is
//!
//! A unit is a test unit when either rule holds:
//!
//! 1. its file is a test by path:
//!    [`lci_core::filter::classify_source_role`] says
//!    [`SourceRole::Test`] (`tests.rs`, `test_*.rs`, `*_test(s).rs`, any
//!    `tests/` or `test/` directory);
//! 2. its symbol chain has a module or owner segment named `tests`, or
//!    `test` when the file's own stem is not `test` (every segment but the
//!    last, which is the unit's own name): an inline `#[cfg(test)] mod tests
//!    { .. }` in a source file, which no path rule can see. A bare `test.rs`
//!    stays source, like `classify_source_role` keeps it (a production module
//!    such as `xtask/src/test.rs`), so its own module name makes no test.
//!
//! So an edge says that the destination is a unit in a test file or a test
//! module, not that it is a `#[test]` function: the map can offer a fixture
//! helper inside `mod tests`, and it misses a `#[test]` function that sits
//! outside a tests module in a non-test file.
//!
//! A test that calls the symbol only through a helper is not an edge: the
//! helper is the user, and the test is reached by a second hop over
//! `scip.impact` (the `tests_deep` gold). A unit whose id the impact map does
//! not hold gives no edge, as in `scip.impact`.
//!
//! # Cap, order and cost
//!
//! At most [`EXPAND_LIMIT`] edges per call, sorted by [`cap_edges`]; `query`
//! reports the dropped count in a partial note. The cost of a call is the
//! occurrences read. The map has no key and no default: it runs only when a
//! call names it.
//!
//! # Freshness
//!
//! The index is read once, when the shared [`ImpactMap`] is built; the model
//! is [`FreshnessModel::PerFile`], like `scip.impact`.

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, Evidence, FreshnessModel, Map,
    MapDescriptor, MapHit, MapId, MapRequest, MapResponse, MapStatus, SnapshotView, SourceLabel,
    edge::{CONF_MAX, Edge, Expanded, MapCard, QuestionType, WitnessClass, cap_edges},
    impact::Chain,
    live_grep::CostLog,
    scip_impact::ImpactMap,
};
use lci_core::{
    filter::{SourceRole, classify_source_role},
    node::{NodeId, NodeKind},
};
use std::{collections::HashSet, sync::Arc, time::Instant};

/// The id of [`TestedByMap`].
pub const TESTED_BY_ID: MapId = "tests.tested_by";

/// Whether a unit is a test unit (in a test file or a test module): its file
/// is a test by path, or a module or owner segment of its chain is `tests`,
/// or `test` when that is not the file's own module (a bare `test.rs` is a
/// source file).
fn is_test_unit(path: &str, chain: Option<&Chain>) -> bool {
    if classify_source_role(path) == SourceRole::Test {
        return true;
    }
    let stem = path
        .rsplit(['/', '\\'])
        .next()
        .and_then(|file| file.strip_suffix(".rs"))
        .unwrap_or_default();
    chain.is_some_and(|chain| {
        let scopes = &chain.names[..chain.names.len().saturating_sub(1)];
        scopes
            .iter()
            .any(|name| name == "tests" || (name == "test" && stem != "test"))
    })
}

pub struct TestedByMap {
    descriptor: MapDescriptor,
    impact: Arc<ImpactMap>,
    /// Units of the index that are test units.
    test_units: usize,
    log: CostLog,
}

impl TestedByMap {
    pub fn new(impact: Arc<ImpactMap>) -> Self {
        let ix = impact.index();
        let test_units = ix
            .defs
            .iter()
            .enumerate()
            .filter(|&(i, def)| ix.unit[i] && is_test_unit(&def.path, ix.chains[i].as_ref()))
            .count();
        Self {
            descriptor: MapDescriptor {
                id: TESTED_BY_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::Symbol],
                emits: vec![NodeKind::Symbol],
                freshness: FreshnessModel::PerFile,
                source_labels: vec![SourceLabel::Scip],
                needs: Vec::new(),
            },
            impact,
            test_units,
            log: CostLog::default(),
        }
    }

    /// Units of the index that count as test units (found at build).
    pub fn test_units(&self) -> usize {
        self.test_units
    }

    /// The test units that reference `node` (unsorted, uncapped) and the
    /// occurrences read.
    fn all_edges(&self, node: &NodeId) -> (Vec<Edge>, u64) {
        let (users, read) = self.impact.user_units(node);
        let ix = self.impact.index();
        let edges = users
            .into_iter()
            .filter(|(unit, _, site)| is_test_unit(&site.path, ix.chains[*unit].as_ref()))
            .map(|(_, dst, site)| Edge {
                src: node.clone(),
                dst,
                kind: EdgeKind::Tests,
                map: TESTED_BY_ID,
                witness: WitnessClass::Compiler,
                conf: CONF_MAX,
                site: Some(site),
            })
            .collect();
        (edges, read)
    }

    /// The edges leaving `node`, sorted and capped, with the cost of the
    /// call, and how many edges there were before the cap.
    fn edges_of(&self, node: &NodeId) -> (Expanded, usize) {
        let started = Instant::now();
        let (mut edges, read) = self.all_edges(node);
        let total = edges.len();
        cap_edges(&mut edges, EXPAND_LIMIT);
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.log.record(elapsed_ms);
        let cost = Cost {
            elapsed_ms,
            nodes_touched: read,
            ..Cost::default()
        };
        (Expanded::new(edges, cost), total)
    }
}

#[crate::async_trait]
impl Map for TestedByMap {
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
                    "{dropped} more test units than the cap of {EXPAND_LIMIT} were not offered"
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
                        .map(|s| format!("tests the seed at {}:{}", s.path, s.line + 1)),
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
            // The same SCIP index as `graph.scip` and `scip.impact`.
            dependent: true,
            good_for: vec![QuestionType::Impact],
        }
    }
}

#[cfg(test)]
mod tests;
