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
//! # Naming a trait or trait method (question seeds)
//!
//! [`ImplementsMap::find`] resolves a path written in a question to the nodes
//! the map expands, so a walk can start from "which types implement
//! `tauri::Manager`?" or "what must change if `Runtime::run` changes?". The
//! candidates ([`Target`]) are the traits and trait method declarations that
//! have at least one edge. The ids a target carries are the ids the map's
//! edges leave and reach, and they are canonical `NodeId` strings, so
//! `--seeds` takes them as printed:
//!
//! * a trait has the id `scip.impact` makes,
//!   `sym:rust:<module path>::<Name>/trait`; the module path comes from the
//!   file of the definition (`crates/x/src/a/mod.rs` gives `crates%2Fx::a`),
//!   not from the Rust path;
//! * a trait method declaration or an implementing member has the id
//!   `graph.scip` makes (`ScipMap::with_default_ids`),
//!   `sym:rust:<module path>::<m>/method[~<n>]`: no container (the graph does
//!   not keep the impl header), so the declaration and the members of one
//!   file that share a method name are told apart by the counter `~<n>`, which
//!   follows graph order and holds for one index (a re-indexed tree can
//!   renumber it);
//! * a type has `scip.impact`'s id, `sym:rust:<module path>::<Name>/struct`
//!   (`enum`, `union`, ...).
//!
//! A trait path matches when its last segment is the trait's name and the
//! segments before it are, in order, among the crate name and the modules of
//! the trait's SCIP symbol (`tauri::Manager`, `manager::Manager`); that is an
//! exact match. When no trait matches that way, the traits with that last
//! segment are ranked by how many of the written qualifiers their path holds
//! and the best `max` are named loosely ([`Found::exact`] is false): a trait
//! re-exported from another crate is written with the re-exporting crate's
//! name (`tauri::Runtime`, defined in `tauri_runtime`).
//!
//! A trait method path (`Runtime::run`) matches when its last segment is the
//! method and the segment before it is the trait that declares it; the
//! segments before that are optional and only rank the candidates
//! (`bevy::prelude::Plugin::build` names the `build` of `bevy_app`'s
//! `Plugin`). That is an exact match. A path of one segment never matches (a
//! bare word is too likely to be prose).
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
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, FreshnessModel, Map, MapDescriptor, MapId,
    MapRequest, MapResponse, SnapshotView, SourceLabel,
    edge::{
        CONF_MAX, Edge, Expanded, MapCard, QuestionType, Site, WitnessClass, cap_edges,
        scip_ranked_hits,
    },
    impact::chain_of,
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

/// What a [`Target`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    /// A trait: its edges reach the types that implement it.
    Trait,
    /// A trait method declaration: its edges reach the members that
    /// implement it.
    Method,
}

/// A trait or trait method declaration a question can name (one that has at
/// least one edge).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub node: NodeId,
    /// The SCIP symbol the node stands for.
    pub symbol: String,
    pub kind: TargetKind,
}

/// A [`Target`] a question's path names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found<'a> {
    pub target: &'a Target,
    /// The path matched the crate, modules and trait of the symbol, not only
    /// its last segment.
    pub exact: bool,
    /// The written path (segments joined with `::`).
    pub path: String,
}

pub struct ImplementsMap {
    descriptor: MapDescriptor,
    options: ImplementsOptions,
    /// Every edge, by the node it leaves (unsorted, uncapped).
    edges: HashMap<NodeId, Vec<Edge>>,
    /// The traits and trait methods with edges, in SCIP symbol order.
    targets: Vec<Target>,
    /// The name segments of each target, crate first (see [`path_of`]),
    /// parallel to `targets`.
    paths: Vec<Vec<String>>,
    stats: ImplementsStats,
    log: CostLog,
}

/// The name segments of a SCIP symbol: its package (crate) name with `-`
/// read as `_`, then its descriptors ([`chain_of`]); empty for a symbol that
/// does not parse.
fn path_of(symbol: &str) -> Vec<String> {
    let (Ok(parsed), Some(chain)) = (scip::symbol::parse_symbol(symbol), chain_of(symbol)) else {
        return Vec::new();
    };
    std::iter::once(parsed.package.name.replace('-', "_"))
        .chain(chain.names)
        .collect()
}

/// Whether `wanted` is a subsequence of `of`.
fn in_order(wanted: &[String], of: &[String]) -> bool {
    let mut rest = of.iter();
    wanted.iter().all(|w| rest.any(|o| o == w))
}

/// How many of the `wanted` segments are among `of` (anywhere).
fn score(wanted: &[String], of: &[String]) -> usize {
    wanted.iter().filter(|w| of.contains(w)).count()
}

/// The candidates `from`, best score of their `prefix` against `wanted`
/// first (ties in the order given), at most `max`.
fn ranked<'p>(
    from: &[usize],
    wanted: &[String],
    prefix: impl Fn(usize) -> &'p [String],
    max: usize,
) -> Vec<usize> {
    let mut list = from.to_vec();
    list.sort_by_key(|&i| std::cmp::Reverse(score(wanted, prefix(i))));
    list.truncate(max);
    list
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
        let mut targets: BTreeMap<String, Target> = BTreeMap::new();
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
                    targets
                        .entry(trait_sym.to_owned())
                        .or_insert_with(|| Target {
                            node: tr.clone(),
                            symbol: trait_sym.to_owned(),
                            kind: TargetKind::Trait,
                        });
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
                    targets
                        .entry(graph.nodes[e.to].symbol.clone())
                        .or_insert_with(|| Target {
                            node: decl.clone(),
                            symbol: graph.nodes[e.to].symbol.clone(),
                            kind: TargetKind::Method,
                        });
                    edges
                        .entry(decl.clone())
                        .or_default()
                        .push(edge(decl, m, &e.witness));
                }
            }
        }
        let targets: Vec<Target> = targets.into_values().collect();
        let paths = targets.iter().map(|t| path_of(&t.symbol)).collect();
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
            targets,
            paths,
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

    /// The traits and trait method declarations that have an edge, in SCIP
    /// symbol order.
    pub fn targets(&self) -> &[Target] {
        &self.targets
    }

    /// The targets the `names` (paths as [`crate::impact::code_names`] reads
    /// them from a question) name, at most `max` per name and kind; see the
    /// module header. Exact matches first, then loose ones, each group in the
    /// order of the names given and, within a name, best score first, then in
    /// SCIP symbol order.
    pub fn find(&self, names: &[Vec<String>], max: usize) -> Vec<Found<'_>> {
        let mut found: Vec<Found<'_>> = Vec::new();
        for name in names.iter().filter(|name| name.len() >= 2) {
            let n = name.len();
            let last = &name[n - 1];
            let path = name.join("::");
            // Traits: the qualifiers before the name.
            let traits: Vec<usize> = (0..self.targets.len())
                .filter(|&i| {
                    self.targets[i].kind == TargetKind::Trait && self.paths[i].last() == Some(last)
                })
                .collect();
            let prefix = |i: usize| &self.paths[i][..self.paths[i].len() - 1];
            let strict: Vec<usize> = traits
                .iter()
                .copied()
                .filter(|&i| in_order(&name[..n - 1], prefix(i)))
                .collect();
            let (chosen, exact) = if strict.is_empty() {
                (ranked(&traits, &name[..n - 1], prefix, max), false)
            } else {
                (strict.into_iter().take(max).collect(), true)
            };
            found.extend(chosen.into_iter().map(|i| Found {
                target: &self.targets[i],
                exact,
                path: path.clone(),
            }));
            // Methods: the segment before the name is the trait, the ones
            // before that are optional (`bevy::prelude::Plugin::build` names
            // the method of `bevy_app`'s `Plugin`).
            let owner = &name[n - 2];
            let methods: Vec<usize> = (0..self.targets.len())
                .filter(|&i| {
                    let p = &self.paths[i];
                    self.targets[i].kind == TargetKind::Method
                        && p.len() >= 2
                        && p.last() == Some(last)
                        && &p[p.len() - 2] == owner
                })
                .collect();
            let above = |i: usize| &self.paths[i][..self.paths[i].len() - 2];
            let best = ranked(&methods, &name[..n - 2], above, usize::MAX);
            let top = best.first().map(|&i| score(&name[..n - 2], above(i)));
            found.extend(
                best.into_iter()
                    .filter(|&i| Some(score(&name[..n - 2], above(i))) == top)
                    .take(max)
                    .map(|i| Found {
                        target: &self.targets[i],
                        exact: true,
                        path: path.clone(),
                    }),
            );
        }
        found.sort_by_key(|f| !f.exact);
        let mut seen = HashSet::new();
        found.retain(|f| seen.insert(f.target.node.clone()));
        found
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
        scip_ranked_hits(
            &self.descriptor,
            request,
            view,
            self.options.cap,
            "implementations",
            |seed| self.edges_of(seed),
            |s| format!("implements at {}:{}", s.path, s.line + 1),
        )
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
