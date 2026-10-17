//! The `scip.impact` map (multimap wave W0c): the one-hop reverse closure of
//! slice M1.4 ([`crate::impact::ImpactIndex::closure`]) as a traversal map.
//! From a symbol it offers the declarations that reference it, the callers
//! and users the SCIP index shows.
//!
//! # What an edge says
//!
//! An edge from `S` to `U` of kind [`EdgeKind::CallsOrRefs`] says that the unit
//! `U` holds a non-definition occurrence of `S`. **The direction is the
//! reverse of `graph.scip`'s**: there `src -> dst` means `src` refers to
//! `dst`; here `dst` refers to `src`. The edge carries no direction mark of its
//! own (the kind is reused on purpose; a new kind was not clearly needed). It
//! can be told apart by its map, `scip.impact`, and by its site, which is the
//! first occurrence of `S` inside `U` (path, 0-based line and column, no blob:
//! an edge source fills it). The witness is [`WitnessClass::Compiler`] and the
//! confidence 100 for every edge: the closure is exact for the index, and the
//! index is the only source. An occurrence inside `U`'s own definition of `S`
//! (a recursive call) makes no edge.
//!
//! Units are those of the M1.4 gold: functions and methods, structs, enums,
//! traits, type aliases, constants and statics. A reference inside an enum
//! variant or a field belongs to the enclosing unit. A reference outside any
//! unit (a `use` line, a module-level item) gives no edge.
//!
//! # Ids
//!
//! A unit the SCIP graph holds (functions and methods) takes the id the
//! caller's `resolve` gives it, so the edges meet `graph.scip`'s nodes. Other
//! units (types, constants) get a best-effort id like `config.key`'s: Rust,
//! module path of the file, no container, the last segment of the display name,
//! a disambiguator that makes the id distinct (in definition order).
//!
//! # Cap and order
//!
//! At most [`ImpactOptions::cap`] edges (default 64) per call. Every edge has
//! confidence 100, so [`crate::edge::sort_edges`] orders them by destination
//! id; the cap keeps that prefix. It is a cap, not a rank: a symbol with more
//! users than the cap loses the users whose ids sort last (`query` says so in
//! a note).
//!
//! # Freshness
//!
//! The index is read once, when the map is built; a tree edit after that is
//! not seen. The model is [`FreshnessModel::PerFile`], like `graph.scip`,
//! which reads the same file.

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, Evidence, FreshnessModel, Map,
    MapDescriptor, MapHit, MapId, MapRequest, MapResponse, MapStatus, SnapshotView, SourceLabel,
    edge::{CONF_MAX, Edge, Expanded, MapCard, QuestionType, Site, WitnessClass, cap_edges},
    impact::ImpactIndex,
    live_grep::CostLog,
};
use lci_core::node::{NodeId, NodeKind, SymbolKey, SymbolKind, rust_module_path};
use scip::types::symbol_information::Kind;
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Instant,
};

/// The id of [`ImpactMap`].
pub const SCIP_IMPACT_ID: MapId = "scip.impact";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImpactOptions {
    /// Most edges one call offers.
    pub cap: usize,
}

impl Default for ImpactOptions {
    fn default() -> Self {
        Self { cap: 64 }
    }
}

/// What building the map found.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImpactStats {
    /// Definitions in the index.
    pub defs: usize,
    /// Definitions the map has a node id for.
    pub nodes: usize,
    /// Of those, ids the caller's `resolve` gave (the rest were made here).
    pub resolved: usize,
}

pub struct ImpactMap {
    descriptor: MapDescriptor,
    ix: ImpactIndex,
    options: ImpactOptions,
    /// The node id of a definition, by index into `ix.defs`.
    ids: Vec<Option<NodeId>>,
    by_node: HashMap<NodeId, usize>,
    stats: ImpactStats,
    log: CostLog,
}

fn kind_of(kind: Option<Kind>, symbol: &str) -> SymbolKind {
    match kind {
        Some(Kind::Method | Kind::TraitMethod | Kind::StaticMethod) => SymbolKind::Method,
        Some(Kind::Struct | Kind::Class) => SymbolKind::Struct,
        Some(Kind::Enum) => SymbolKind::Enum,
        Some(Kind::Trait | Kind::Interface) => SymbolKind::Trait,
        Some(Kind::TypeAlias) => SymbolKind::TypeAlias,
        Some(Kind::Constant) => SymbolKind::Const,
        Some(Kind::StaticVariable | Kind::StaticProperty) => SymbolKind::Static,
        Some(Kind::Function) => SymbolKind::Function,
        _ if symbol.ends_with('#') => SymbolKind::Struct,
        _ => SymbolKind::Function,
    }
}

impl ImpactMap {
    /// A map over the SCIP index `ix`. `resolve` maps a SCIP symbol to the id
    /// the other maps use for it (the SCIP graph's); a unit it does not know
    /// gets an id made here.
    pub fn new(
        ix: ImpactIndex,
        resolve: &dyn Fn(&str) -> Option<NodeId>,
        options: ImpactOptions,
    ) -> Self {
        let mut taken: HashSet<NodeId> = HashSet::new();
        let mut ids: Vec<Option<NodeId>> = vec![None; ix.defs.len()];
        let mut resolved = 0;
        for (i, def) in ix.defs.iter().enumerate() {
            if let Some(id) = resolve(&def.symbol)
                && taken.insert(id.clone())
            {
                ids[i] = Some(id);
                resolved += 1;
            }
        }
        for (i, def) in ix.defs.iter().enumerate() {
            if ids[i].is_some() || !ix.unit[i] {
                continue;
            }
            let display = ix.display_name(i);
            let mut key = SymbolKey {
                language: "rust".into(),
                module_path: rust_module_path(&def.path),
                container: Vec::new(),
                name: display.rsplit("::").next().unwrap_or(&display).into(),
                kind: kind_of(ix.kinds[i], &def.symbol),
                disambiguator: 0,
            };
            let id = loop {
                let id = NodeId::Symbol(key.clone());
                if taken.insert(id.clone()) {
                    break id;
                }
                key.disambiguator = key.disambiguator.saturating_add(1);
                if key.disambiguator == u16::MAX {
                    break id;
                }
            };
            ids[i] = Some(id);
        }
        let by_node: HashMap<NodeId, usize> = ids
            .iter()
            .enumerate()
            .filter_map(|(i, id)| Some((id.clone()?, i)))
            .collect();
        let stats = ImpactStats {
            defs: ix.defs.len(),
            nodes: by_node.len(),
            resolved,
        };
        Self {
            descriptor: MapDescriptor {
                id: SCIP_IMPACT_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::Symbol],
                emits: vec![NodeKind::Symbol],
                freshness: FreshnessModel::PerFile,
                source_labels: vec![SourceLabel::Scip],
                needs: Vec::new(),
            },
            ix,
            options,
            ids,
            by_node,
            stats,
            log: CostLog::default(),
        }
    }

    /// A map with no nodes (an index that failed to load, kept so the plan
    /// lists it).
    pub fn empty(options: ImpactOptions) -> Self {
        Self::new(
            ImpactIndex::build(Vec::new(), &HashMap::new(), None),
            &|_| None,
            options,
        )
    }

    /// The SCIP index the map was built over (read only).
    pub fn index(&self) -> &ImpactIndex {
        &self.ix
    }

    pub fn stats(&self) -> ImpactStats {
        self.stats
    }

    pub fn options(&self) -> ImpactOptions {
        self.options
    }

    pub fn log(&self) -> &CostLog {
        &self.log
    }

    /// The node id of a definition of the index, if the map has one.
    pub fn node_of(&self, symbol: &str) -> Option<&NodeId> {
        self.ids.get(self.ix.def_index(symbol)?)?.as_ref()
    }

    /// The definition (an index into [`ImpactIndex::defs`]) a node id stands
    /// for, the reverse of [`ImpactMap::node_of`].
    pub fn def_of(&self, node: &NodeId) -> Option<usize> {
        self.by_node.get(node).copied()
    }

    /// The node id of the definition at `def`, if the map has one.
    pub fn node_of_def(&self, def: usize) -> Option<&NodeId> {
        self.ids.get(def)?.as_ref()
    }

    /// Where the map's node is defined: the path and the 0-based line and
    /// column of the name. For a unit the SCIP graph does not hold (a type, a
    /// constant) this is the only place that knows.
    pub fn site_of(&self, node: &NodeId) -> Option<(&str, u32, u32)> {
        let def = &self.ix.defs[*self.by_node.get(node)?];
        Some((&def.path, def.name_range.0, def.name_range.1))
    }

    /// Every edge leaving `node` (unsorted, uncapped) and the occurrences read.
    fn all_edges(&self, node: &NodeId) -> (Vec<Edge>, u64) {
        let Some(&def) = self.by_node.get(node) else {
            return (Vec::new(), 0);
        };
        let mut occurrences = self.ix.occurrences(&self.ix.defs[def].symbol);
        occurrences.sort();
        let read = occurrences.len() as u64;
        // The first occurrence of each user, in (path, line, column) order.
        let mut first: BTreeMap<usize, (String, u32, u32)> = BTreeMap::new();
        for (path, line, col) in occurrences {
            if let Some(unit) = self.ix.unit_at(&path, line, col)
                && unit != def
            {
                first.entry(unit).or_insert((path, line, col));
            }
        }
        let edges = first
            .into_iter()
            .filter_map(|(unit, (path, line, col))| {
                Some(Edge {
                    src: node.clone(),
                    dst: self.ids[unit].clone()?,
                    kind: EdgeKind::CallsOrRefs,
                    map: SCIP_IMPACT_ID,
                    witness: WitnessClass::Compiler,
                    conf: CONF_MAX,
                    site: Some(Site {
                        path,
                        blob: String::new(),
                        line,
                        col,
                    }),
                })
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
        cap_edges(&mut edges, self.options.cap.min(EXPAND_LIMIT));
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
impl Map for ImpactMap {
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
                    "{dropped} more users than the cap of {} were not offered",
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
                        .map(|s| format!("references the seed at {}:{}", s.path, s.line + 1)),
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
            // The same SCIP index as `graph.scip`, read in reverse.
            dependent: true,
            good_for: vec![QuestionType::Impact, QuestionType::Flow],
        }
    }
}

#[cfg(test)]
mod tests;
