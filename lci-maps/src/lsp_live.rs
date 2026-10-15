//! The `lsp.live` map (multimap wave W0c2a): symbol facts asked of a running
//! language server, so that they are as fresh as the tree is. It complements
//! `graph.scip` (an index, stale after an edit) the way `grep.live` does for
//! text.
//!
//! # Edges
//!
//! Every edge is a server answer: witness [`WitnessClass::Exact`], confidence
//! 100, with the answer's place as the [`Site`] (no blob: an edge source fills
//! it).
//!
//! * symbol -> the symbol (or, when the index knows none there, the file) at
//!   the place `textDocument/definition` gives for the symbol's name, kind
//!   [`EdgeKind::CallsOrRefs`], direction as in `graph.scip` (the source refers
//!   to the destination). Not the symbol itself.
//! * symbol -> the symbols (or files) that hold `textDocument/references` of
//!   it, declaration left out, kind [`EdgeKind::CallsOrRefs`], **direction
//!   reversed** as in `scip.impact` (the destination refers to the source).
//!   The site is the first reference inside the destination.
//! * file (or chunk, standing for its file) -> the symbols
//!   `textDocument/documentSymbol` lists whose name the index also defines,
//!   kind [`EdgeKind::Contains`]. Members the index has no node for (fields,
//!   types) are left out: a node id needs a symbol the walk can name.
//!
//! The symbol <-> place table is the SCIP index's ([`ScipPlaces`]): the node
//! ids of `graph.scip` are the only symbol ids there are. A server column is
//! UTF-16, the index's a byte column; they differ only after non-ASCII text on
//! the line.
//!
//! # Cost and lifecycle
//!
//! Cost class External: each request to the server is one external call,
//! charged in the [`Cost`] of the call that made it, and a call stops at the
//! budget's `max_external_calls`. The deadline of the budget (and
//! [`CALL_TIMEOUT`]) bounds the call: it ends with [`DEADLINE_NOTE`] as its
//! recorded failure ([`LspLive::last_error`]) and no edges, and the server keeps
//! starting for the next call.
//!
//! A cold server start can exceed the default `[traverse] deadline_ms` (5000):
//! a cold rust-analyzer took about 8 s to answer its first request, so the
//! walk that starts a cold server may return [`DEADLINE_NOTE`] and no edges
//! from this map (the server stops with the map, when the walk ends).
//!
//! The server starts at the first request (not when the map is built), is
//! reused for the walk and stops when the map is dropped:
//! [`ManagerBackend`] owns the `lci-core` language-server manager, whose client
//! ends the server's whole process tree when it drops. A missing server binary
//! is found by [`ManagerBackend::open`], before any walk, as an error the
//! caller reports; the map is then not registered.

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, Evidence, FreshnessModel, Map,
    MapDescriptor, MapHit, MapId, MapRequest, MapResponse, MapStatus, Need, SnapshotView,
    SourceLabel,
    edge::{CONF_MAX, Edge, Expanded, MapCard, QuestionType, Site, WitnessClass, cap_edges},
    live_grep::{CostLog, ScipSpans, SymbolLocator},
    scip_graph::ScipMap,
};
use lci_core::{
    config::Config,
    language,
    lsp::{Location, Manager, adapter::LspAdapter},
    node::{NodeId, NodeKind},
    workspace::Workspace,
};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

pub const LSP_LIVE_ID: MapId = "lsp.live";

/// The note of a call that ran out of the traversal's time (the deadline of
/// its [`Budget`]).
pub const DEADLINE_NOTE: &str = "lsp.live: the traversal deadline passed";

/// The longest one expansion waits for the server, whatever the budget allows.
pub const CALL_TIMEOUT: Duration = Duration::from_secs(15);

/// How long the version probe of a server binary may take.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// A place in a file: path relative to the root with `/`, 0-based line and
/// column.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Place {
    pub path: String,
    pub line: u32,
    pub col: u32,
}

/// One entry of a file's symbol list: its name and where it starts (a server
/// gives the start of the item or of the name, depending on its protocol
/// level) and the line it ends on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    pub name: String,
    pub place: Place,
    pub end_line: u32,
}

/// The questions the map asks of a language server.
#[crate::async_trait]
pub trait LspBackend: Send + Sync {
    /// Where the symbol named at `at` is defined.
    async fn definition(&self, at: &Place) -> Result<Vec<Place>, String>;
    /// Where the symbol named at `at` is referred to (declaration left out).
    async fn references(&self, at: &Place) -> Result<Vec<Place>, String>;
    /// The symbols the server lists for `path`.
    async fn document_symbols(&self, path: &str) -> Result<Vec<Member>, String>;
}

/// Symbol nodes <-> places.
pub trait SymbolPlaces: Send + Sync {
    /// Where the node's name is.
    fn place_of(&self, node: &NodeId) -> Option<Place>;
    /// The node whose name is exactly at `place`.
    fn defined_at(&self, place: &Place) -> Option<NodeId>;
    /// The node named `name` whose name lies on a line of `from..=to`.
    fn defined_in(&self, path: &str, lines: (u32, u32), name: &str) -> Option<NodeId>;
    /// The innermost node whose definition holds the line.
    fn enclosing(&self, path: &str, line: u32) -> Option<NodeId>;
}

/// The places of the SCIP graph's nodes.
#[derive(Debug, Clone, Default)]
pub struct ScipPlaces {
    spans: ScipSpans,
    by_node: HashMap<NodeId, Place>,
    by_place: HashMap<Place, NodeId>,
    /// Per file: (line of the name, node), in graph order.
    by_file: HashMap<String, Vec<(u32, NodeId)>>,
}

impl ScipPlaces {
    pub fn from_scip(map: &ScipMap) -> Self {
        let mut places = Self {
            spans: ScipSpans::from_scip(map),
            ..Self::default()
        };
        for (i, node) in map.graph().nodes.iter().enumerate() {
            let place = Place {
                path: node.file.replace('\\', "/"),
                line: node.line,
                col: node.col,
            };
            let id = map.node_id(i).clone();
            places
                .by_file
                .entry(place.path.clone())
                .or_default()
                .push((place.line, id.clone()));
            places.by_place.insert(place.clone(), id.clone());
            places.by_node.insert(id, place);
        }
        places
    }
}

impl SymbolPlaces for ScipPlaces {
    fn place_of(&self, node: &NodeId) -> Option<Place> {
        self.by_node.get(node).cloned()
    }

    fn defined_at(&self, place: &Place) -> Option<NodeId> {
        self.by_place.get(place).cloned()
    }

    fn defined_in(&self, path: &str, lines: (u32, u32), name: &str) -> Option<NodeId> {
        self.by_file
            .get(path)?
            .iter()
            .find(|(line, id)| {
                (lines.0..=lines.1).contains(line)
                    && matches!(id, NodeId::Symbol(key) if key.name == name)
            })
            .map(|(_, id)| id.clone())
    }

    fn enclosing(&self, path: &str, line: u32) -> Option<NodeId> {
        self.spans.symbol_at(path, line)
    }
}

/// The language servers of `lci-core`, through its [`Manager`]: one session
/// per server and workspace, started by its first request.
pub struct ManagerBackend {
    manager: Manager,
    workspace: Workspace,
    /// The enabled servers whose binary answered the version probe.
    adapters: Vec<Arc<dyn LspAdapter>>,
}

impl ManagerBackend {
    /// Probes the enabled servers (`--version`, no session) and keeps the ones
    /// that answer. An error when none does, naming each failure.
    pub async fn open(workspace: Workspace, config: &Config) -> Result<Self, String> {
        let manager = Manager::new(config);
        let mut adapters = Vec::new();
        let mut failures = Vec::new();
        for adapter in manager.adapters().iter().filter(|a| a.enabled()) {
            match adapter.probe_readiness(PROBE_TIMEOUT).await {
                Ok(()) => adapters.push(Arc::clone(adapter)),
                Err(error) => failures.push(format!("{}: {error:#}", adapter.provider())),
            }
        }
        if adapters.is_empty() {
            return Err(format!(
                "no language server is available ({})",
                failures.join("; ")
            ));
        }
        Ok(Self {
            manager,
            workspace,
            adapters,
        })
    }

    /// The server for the file's language.
    fn adapter_for(&self, path: &str) -> Result<&Arc<dyn LspAdapter>, String> {
        let language = language::for_path(Path::new(path))
            .map(|adapter| adapter.identifier())
            .ok_or_else(|| format!("no language is known for {path}"))?;
        self.adapters
            .iter()
            .find(|adapter| adapter.languages().contains(&language))
            .ok_or_else(|| format!("no language server is available for {language} ({path})"))
    }
}

fn places(locations: Vec<Location>) -> Vec<Place> {
    locations
        .into_iter()
        .filter_map(|location| {
            Some(Place {
                path: location.relative_file_path?,
                line: location.start_line.checked_sub(1)?,
                col: location.start_character,
            })
        })
        .collect()
}

#[crate::async_trait]
impl LspBackend for ManagerBackend {
    async fn definition(&self, at: &Place) -> Result<Vec<Place>, String> {
        let adapter = self.adapter_for(&at.path)?;
        let file = self.workspace.path.join(&at.path);
        self.manager
            .definition(adapter, &self.workspace, &file, &at.path, at.line, at.col)
            .await
            .map(places)
            .map_err(|error| format!("{error:#}"))
    }

    async fn references(&self, at: &Place) -> Result<Vec<Place>, String> {
        let adapter = self.adapter_for(&at.path)?;
        let file = self.workspace.path.join(&at.path);
        self.manager
            .references(
                adapter,
                &self.workspace,
                &file,
                &at.path,
                at.line,
                at.col,
                false,
            )
            .await
            .map(places)
            .map_err(|error| format!("{error:#}"))
    }

    async fn document_symbols(&self, path: &str) -> Result<Vec<Member>, String> {
        let adapter = self.adapter_for(path)?;
        let file = self.workspace.path.join(path);
        let found = self
            .manager
            .document_symbols(adapter, &self.workspace, &file, path)
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok(found
            .into_iter()
            .filter_map(|location| {
                Some(Member {
                    name: location.name.clone()?,
                    end_line: location.end_line.checked_sub(1)?,
                    place: places(vec![location]).pop()?,
                })
            })
            .collect())
    }
}

/// `lsp.live`; see the module notes.
pub struct LspLive {
    descriptor: MapDescriptor,
    backend: Box<dyn LspBackend>,
    places: Arc<dyn SymbolPlaces>,
    log: CostLog,
    last_error: Mutex<Option<String>>,
}

/// What a call found, or why it stopped.
struct Found {
    edges: Vec<Edge>,
    calls: u32,
}

impl LspLive {
    pub fn new(backend: Box<dyn LspBackend>, places: Arc<dyn SymbolPlaces>) -> Self {
        Self {
            descriptor: MapDescriptor {
                id: LSP_LIVE_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::Symbol, NodeKind::File, NodeKind::Chunk],
                emits: vec![NodeKind::Symbol, NodeKind::File],
                freshness: FreshnessModel::Live,
                source_labels: vec![SourceLabel::Lsp],
                needs: vec![Need::Lsp],
            },
            backend,
            places,
            log: CostLog::default(),
            last_error: Mutex::new(None),
        }
    }

    /// The call times kept so far.
    pub fn log(&self) -> &CostLog {
        &self.log
    }

    /// The error of the last expansion that failed, if any (an expansion that
    /// fails returns no edges: the trait has no way to say more).
    pub fn last_error(&self) -> Option<String> {
        self.last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn fail(&self, error: String) {
        *self
            .last_error
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(error);
    }

    /// An edge from `src` to `dst`, seen at `place`.
    fn edge(src: &NodeId, kind: EdgeKind, dst: NodeId, place: &Place) -> Edge {
        Edge {
            src: src.clone(),
            dst,
            kind,
            map: LSP_LIVE_ID,
            witness: WitnessClass::Exact,
            conf: CONF_MAX,
            site: Some(Site {
                path: place.path.clone(),
                blob: String::new(),
                line: place.line,
                col: place.col,
            }),
        }
    }

    /// The symbol that holds `place` (the one defined there, else the one
    /// whose definition holds its line), else the file.
    fn holder(&self, place: &Place, exact_first: bool) -> Option<NodeId> {
        let defined = exact_first.then(|| self.places.defined_at(place)).flatten();
        defined
            .or_else(|| self.places.enclosing(&place.path, place.line))
            .or_else(|| NodeId::file(&place.path).ok())
    }

    /// The requests for `node`, in order, until `calls` reaches `max_calls`.
    async fn ask(&self, node: &NodeId, max_calls: u32, found: &mut Found) -> Result<(), String> {
        match node {
            NodeId::Symbol(_) => {
                let Some(at) = self.places.place_of(node) else {
                    return Ok(());
                };
                if found.calls < max_calls {
                    found.calls += 1;
                    for place in self.backend.definition(&at).await? {
                        if let Some(dst) = self.holder(&place, true).filter(|dst| dst != node) {
                            found
                                .edges
                                .push(Self::edge(node, EdgeKind::CallsOrRefs, dst, &place));
                        }
                    }
                }
                if found.calls < max_calls {
                    found.calls += 1;
                    // The first reference (in place order) inside each user.
                    let mut users: BTreeMap<NodeId, Place> = BTreeMap::new();
                    let mut references = self.backend.references(&at).await?;
                    references.sort();
                    for place in references {
                        if let Some(dst) = self.holder(&place, false).filter(|dst| dst != node) {
                            users.entry(dst).or_insert(place);
                        }
                    }
                    for (dst, place) in users {
                        found
                            .edges
                            .push(Self::edge(node, EdgeKind::CallsOrRefs, dst, &place));
                    }
                }
            }
            NodeId::File { path } | NodeId::Chunk { path, .. } if found.calls < max_calls => {
                found.calls += 1;
                for member in self.backend.document_symbols(path).await? {
                    let lines = (member.place.line, member.end_line);
                    if let Some(dst) =
                        self.places
                            .defined_in(&member.place.path, lines, &member.name)
                    {
                        found
                            .edges
                            .push(Self::edge(node, EdgeKind::Contains, dst, &member.place));
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// The edges leaving `node`, sorted and capped, with the cost of the call.
    /// A call that ran out of time or failed has no edges and says why in
    /// [`Self::last_error`].
    async fn collect(&self, node: &NodeId, budget: Budget) -> Expanded {
        let started = Instant::now();
        let deadline = budget.deadline.min(started + CALL_TIMEOUT);
        let mut found = Found {
            edges: Vec::new(),
            calls: 0,
        };
        if started >= deadline {
            self.fail(DEADLINE_NOTE.to_owned());
            return Expanded::default();
        }
        let answer = tokio::time::timeout_at(
            deadline.into(),
            self.ask(node, budget.max_external_calls, &mut found),
        )
        .await;
        match answer {
            Err(_) => {
                self.fail(DEADLINE_NOTE.to_owned());
                found.edges.clear();
            }
            Ok(Err(error)) => {
                self.fail(error);
                found.edges.clear();
            }
            Ok(Ok(())) => {}
        }
        cap_edges(&mut found.edges, EXPAND_LIMIT);
        let elapsed_ms = started.elapsed().as_secs_f64() * 1000.0;
        if found.calls > 0 {
            self.log.record(elapsed_ms);
        }
        let cost = Cost {
            elapsed_ms,
            external_calls: found.calls,
            nodes_touched: found.edges.len() as u64,
            ..Cost::default()
        };
        Expanded::new(found.edges, cost)
    }
}

#[crate::async_trait]
impl Map for LspLive {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        let mut edges = Vec::new();
        for (seed, _) in request.seeds {
            let found = self.collect(seed, request.budget).await;
            response.cost.elapsed_ms += found.cost.elapsed_ms;
            response.cost.external_calls += found.cost.external_calls;
            edges.extend(found.edges);
        }
        cap_edges(&mut edges, usize::MAX);
        let mut seen = HashSet::new();
        edges.retain(|edge| seen.insert(edge.dst.clone()));
        edges.truncate(request.limit);
        response.cost.nodes_touched = edges.len() as u64;
        response.freshness.observed = crate::Observed::LiveTree;
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
                    label: SourceLabel::Lsp,
                    detail: edge.site.map(|s| format!("{}:{}", s.path, s.line + 1)),
                }],
            })
            .collect();
        response
    }

    async fn expand(&self, node: &NodeId, budget: Budget, _view: &SnapshotView<'_>) -> Expanded {
        self.collect(node, budget).await
    }

    fn card(&self) -> MapCard {
        MapCard {
            expand_p50_ms: self.log.p50(),
            coverage: self.descriptor.emits.clone(),
            freshness_lag: Some(Duration::ZERO),
            dependent: false,
            good_for: vec![QuestionType::WhereTo, QuestionType::Impact],
        }
    }
}

#[cfg(test)]
mod tests;
