//! The containment map `structure.contains` (spine slice S6c).
//!
//! The SCIP graph ([`crate::scip_graph::ScipMap`]) has edges only between
//! function and method symbols, so a traversal that starts from a chunk or a
//! file (what the hybrid search often finds: an import header, a gap between
//! functions) reaches nothing. This map connects the three node kinds by the
//! structure of the files, with an exact witness ([`WitnessClass::Exact`],
//! confidence 100): the SCIP index says where each definition is, and a file,
//! a chunk and a definition either nest or they do not.
//!
//! Edges:
//!
//! * file -> every symbol defined in the file, kind `contains`, top-level
//!   functions first, then members (methods and nested functions), each group
//!   in definition order. The edge cap keeps the first [`EXPAND_LIMIT`].
//! * chunk -> the symbols whose definition span overlaps the chunk's lines,
//!   kind `contains`. A chunk that overlaps none (an import header, a gap, a
//!   type) falls through to the symbols of its file, as for a file node, and
//!   so does a chunk whose lines the map does not know. The edge API gives
//!   `expand` no question, so the map cannot rank the file's symbols by the
//!   question's words: the traversal's hop scorer does that (it sees every
//!   edge's destination and the question), keeping the best sixteen when
//!   more than eight edges are offered.
//! * symbol -> its file and, for a nested function, the function around it,
//!   kind `defined_in`, so a walk can climb as well as descend.
//!
//! The nodes are the symbols of the SCIP graph, with the ids of the
//! [`ScipMap`] the structure is built from: the graph holds functions and
//! methods only, so there is no `impl` or type node to climb to; a method's
//! parent is the function it is nested in, if any, else its file.
//!
//! A chunk id names its path and content hash, not its lines. The lines come
//! from the snapshot view when it holds the chunk, else from the table
//! [`ContainsMap::with_chunk_lines`] was given (the app fills it with the
//! chunks of the hybrid seeds).
//!
//! Edge sites are the definition of the symbol at the end of the edge (the
//! symbol itself for `defined_in` to its file), with the zero-based line and
//! column of the SCIP index, like the sites of [`ScipMap`]. The blob is
//! filled by the edge source.

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, Evidence, FreshnessModel, Map,
    MapDescriptor, MapHit, MapId, MapRequest, MapResponse, MapStatus, SnapshotView, SourceLabel,
    edge::{CONF_MAX, Edge, Site, WitnessClass, cap_edges, sort_edges},
    scip_graph::{FnKind, GraphEdgeKind, ScipMap},
};
use lci_core::node::{NodeId, NodeKind};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

/// The id of [`ContainsMap`].
pub const CONTAINS_MAP_ID: MapId = "structure.contains";

/// One symbol of the graph, as the structure needs it.
#[derive(Debug)]
struct Fact {
    /// The file, `/` separated.
    file: String,
    /// Name line and column (0-based) and the first and last line of the
    /// definition (0-based, inclusive).
    line: u32,
    col: u32,
    span: (u32, u32),
    /// The function the symbol is nested in.
    parent: Option<usize>,
}

/// What does not change between calls: built once from the graph.
#[derive(Debug)]
struct Structure {
    ids: Vec<NodeId>,
    index: HashMap<NodeId, usize>,
    facts: Vec<Fact>,
    /// The symbols of each file, top-level first, then members, each group
    /// in definition order.
    by_file: HashMap<String, Vec<usize>>,
    /// The id of each file that holds a symbol.
    files: HashMap<String, NodeId>,
}

impl Structure {
    fn new(scip: &ScipMap) -> Self {
        let graph = scip.graph();
        let mut parent: Vec<Option<usize>> = vec![None; graph.nodes.len()];
        for (from, edges) in graph.out.iter().enumerate() {
            for edge in edges.iter().filter(|e| e.kind == GraphEdgeKind::Contains) {
                parent[edge.to] = Some(from);
            }
        }
        let facts: Vec<Fact> = graph
            .nodes
            .iter()
            .zip(&parent)
            .map(|(node, &parent)| Fact {
                file: node.file.replace('\\', "/"),
                line: node.line,
                col: node.col,
                span: node.span,
                parent,
            })
            .collect();
        let ids: Vec<NodeId> = (0..graph.nodes.len())
            .map(|i| scip.node_id(i).clone())
            .collect();
        let index = ids.iter().cloned().zip(0..).collect();

        let mut by_file: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, fact) in facts.iter().enumerate() {
            by_file.entry(fact.file.clone()).or_default().push(i);
        }
        for symbols in by_file.values_mut() {
            symbols.sort_by_key(|&i| {
                let member = graph.nodes[i].kind == FnKind::Method || facts[i].parent.is_some();
                (member, facts[i].line, facts[i].col, i)
            });
        }
        let files = by_file
            .keys()
            .filter_map(|path| Some((path.clone(), NodeId::file(path.clone()).ok()?)))
            .collect();
        Self {
            ids,
            index,
            facts,
            by_file,
            files,
        }
    }
}

/// The containment of files, chunks and the symbols of a SCIP graph as a
/// [`Map`]. Cheap to clone: the structure is shared.
#[derive(Debug, Clone)]
pub struct ContainsMap {
    structure: Arc<Structure>,
    /// The lines (1-based, inclusive) of the chunks the caller knows.
    chunk_lines: Arc<HashMap<NodeId, (u32, u32)>>,
    descriptor: MapDescriptor,
}

impl ContainsMap {
    /// The structure of `scip`'s graph, with its node ids.
    pub fn new(scip: &ScipMap) -> Self {
        Self {
            structure: Arc::new(Structure::new(scip)),
            chunk_lines: Arc::default(),
            descriptor: MapDescriptor {
                id: CONTAINS_MAP_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::Symbol, NodeKind::File, NodeKind::Chunk],
                emits: vec![NodeKind::Symbol, NodeKind::File],
                freshness: FreshnessModel::PerFile,
                source_labels: vec![SourceLabel::Scip],
                needs: Vec::new(),
            },
        }
    }

    /// The same structure with the lines (1-based, inclusive) of some chunks.
    /// Chunk nodes carry no lines; the view may not hold them either.
    pub fn with_chunk_lines(&self, lines: impl IntoIterator<Item = (NodeId, (u32, u32))>) -> Self {
        Self {
            chunk_lines: Arc::new(lines.into_iter().collect()),
            ..self.clone()
        }
    }

    /// The symbols of the graph in the order the file edges list them
    /// (top-level first, then members), for a path.
    pub fn symbols_of(&self, path: &str) -> Vec<&NodeId> {
        self.structure
            .by_file
            .get(&path.replace('\\', "/"))
            .map(|symbols| symbols.iter().map(|&i| &self.structure.ids[i]).collect())
            .unwrap_or_default()
    }

    fn site(&self, symbol: usize) -> Site {
        let fact = &self.structure.facts[symbol];
        Site {
            path: fact.file.clone(),
            blob: String::new(),
            line: fact.line,
            col: fact.col,
        }
    }

    fn edge(&self, src: &NodeId, dst: &NodeId, kind: EdgeKind, symbol: usize) -> Edge {
        Edge {
            src: src.clone(),
            dst: dst.clone(),
            kind,
            map: CONTAINS_MAP_ID,
            witness: WitnessClass::Exact,
            conf: CONF_MAX,
            site: Some(self.site(symbol)),
            cost: Cost::default(),
        }
    }

    /// `contains` edges from `src` to the first [`EXPAND_LIMIT`] of
    /// `symbols`, which are already in listing order.
    fn contains(&self, src: &NodeId, symbols: impl Iterator<Item = usize>) -> Vec<Edge> {
        symbols
            .take(EXPAND_LIMIT)
            .map(|i| self.edge(src, &self.structure.ids[i], EdgeKind::Contains, i))
            .collect()
    }

    fn lines_of(&self, chunk: &NodeId, view: &SnapshotView<'_>) -> Option<(u32, u32)> {
        self.chunk_lines.get(chunk).copied().or_else(|| {
            view.chunk(chunk)
                .map(|chunk| (chunk.start_line, chunk.end_line))
        })
    }

    /// The out-edges of `node`, unsorted, with the cost of the call.
    fn edges_of(&self, node: &NodeId, view: &SnapshotView<'_>) -> Vec<Edge> {
        let structure = &*self.structure;
        let mut edges = match node {
            NodeId::Symbol(_) => {
                let Some(&at) = structure.index.get(node) else {
                    return Vec::new();
                };
                let fact = &structure.facts[at];
                let mut edges = Vec::new();
                if let Some(file) = structure.files.get(&fact.file) {
                    edges.push(self.edge(node, file, EdgeKind::DefinedIn, at));
                }
                if let Some(parent) = fact.parent {
                    edges.push(self.edge(
                        node,
                        &structure.ids[parent],
                        EdgeKind::DefinedIn,
                        parent,
                    ));
                }
                edges
            }
            NodeId::File { path } => {
                let Some(symbols) = structure.by_file.get(&path.replace('\\', "/")) else {
                    return Vec::new();
                };
                self.contains(node, symbols.iter().copied())
            }
            NodeId::Chunk { path, .. } => {
                let Some(symbols) = structure.by_file.get(&path.replace('\\', "/")) else {
                    return Vec::new();
                };
                let overlapping: Vec<usize> = match self.lines_of(node, view) {
                    Some((start, end)) => {
                        let first = start.saturating_sub(1);
                        let last = end.max(start).saturating_sub(1);
                        symbols
                            .iter()
                            .copied()
                            .filter(|&i| {
                                let (from, to) = structure.facts[i].span;
                                from <= last && first <= to
                            })
                            .collect()
                    }
                    None => Vec::new(),
                };
                if overlapping.is_empty() {
                    self.contains(node, symbols.iter().copied())
                } else {
                    self.contains(node, overlapping.into_iter())
                }
            }
            _ => return Vec::new(),
        };
        let cost = Cost {
            nodes_touched: edges.len() as u64,
            ..Cost::default()
        };
        for edge in &mut edges {
            edge.cost = cost;
        }
        edges
    }
}

#[async_trait::async_trait]
impl Map for ContainsMap {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let mut edges = Vec::new();
        for (seed, _) in request.seeds {
            edges.extend(self.edges_of(seed, view));
        }
        sort_edges(&mut edges);
        let mut seen = HashSet::new();
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
                score: Some(f32::from(edge.conf) / f32::from(CONF_MAX)),
                evidence: vec![Evidence {
                    via: Some(edge.src),
                    edge: edge.kind,
                    label: SourceLabel::Scip,
                    detail: None,
                }],
            })
            .collect();
        response
    }

    async fn expand(&self, node: &NodeId, _budget: Budget, view: &SnapshotView<'_>) -> Vec<Edge> {
        let mut edges = self.edges_of(node, view);
        cap_edges(&mut edges, EXPAND_LIMIT);
        edges
    }
}
