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
//! * with `[run_profile.lsp] implementations = true` (default off), a third
//!   request for a symbol: the places `textDocument/implementation` gives for
//!   the symbol's name, kind [`EdgeKind::Implements`], direction as in
//!   `scip.implements` (from the interface or trait member to the members that
//!   implement it), one edge per destination, the symbol itself left out. It is
//!   asked only at a node with a place, which with the index alone
//!   means a function or method: an interface member, not the interface (the
//!   outline of `places = "outline"` gives types and traits places too). An
//!   error reply from the implementation request costs the other edges nothing
//!   (the error is kept in [`LspLive::last_error`]); a hang still trips the
//!   call's deadline and clears them all.
//! * with `[run_profile.lsp] call_hierarchy = true` (default off), up to three
//!   more requests for a symbol: `textDocument/prepareCallHierarchy` at its
//!   name, then `callHierarchy/incomingCalls` and `callHierarchy/outgoingCalls`
//!   of the item that answers (none: no further request). Both directions are
//!   [`EdgeKind::CallsOrRefs`] (the kind has no direction, d397), one edge per
//!   destination: a caller is a destination that refers to the symbol
//!   (direction reversed, as `references`), a callee one the symbol refers to
//!   (as `graph.scip`). The site is the smallest place of the call itself
//!   (`fromRanges`), else the item's. A destination that `definition` or
//!   `references` already named keeps that edge. Each request fails open as
//!   the implementation request does: an error reply (a server without `callHierarchyProvider`
//!   answers -32601) is recorded in [`LspLive::last_error`] and keeps the
//!   other edges, a failed prepare ends the block, a failed incoming request
//!   still asks outgoing. Servers miss calls (rust-analyzer's `callHierarchy`
//!   missed 16% of the call pairs SCIP has, in macro arguments): this map
//!   complements `graph.scip`, it does not replace it.
//! * file (or chunk, standing for its file) -> the symbols
//!   `textDocument/documentSymbol` lists whose name the table also defines,
//!   kind [`EdgeKind::Contains`]. Under `places = "outline"` that includes
//!   fields, properties and variables; members the table has no node for (enum
//!   members, impl blocks, type parameters) are left out.
//!
//! The symbol <-> place table is the SCIP index's ([`ScipPlaces`]): the node
//! ids of `graph.scip` are the ids there are. Under `[run_profile.lsp] places =
//! "outline"` it is the index's plus the server's own outline
//! ([`OutlinePlaces`]): the first time the walk needs a file's places (it
//! expands a symbol the index places there, or expands the file) it asks the
//! server for the file's `textDocument/documentSymbol`, one more external call
//! per file and map instance, and from then on the outline's places win in that
//! file (the index may be older than the tree). An outline member takes the
//! index's id for the same symbol when there is one, else (a Rust file of the
//! tree) the id of the declaration `sym.defs` would store, else a minted id with
//! the file's real language, so under `scip.mode = "off"` (an empty index) every
//! function, method, type, enum, module and constant of an outlined file has
//! an id. Under `places = "scip"`, the default, no outline is asked for a
//! symbol and a symbol the index lacks has no place.
//! A server column is UTF-16, the index's a byte column; they differ only after
//! non-ASCII text on the line.
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
//! from this map (under `per_call`, and for a server that does not watch its
//! workspace, the server stops with the map, when the walk ends; a shared
//! server keeps starting for the next call).
//!
//! The server starts at the first request (not when the map is built) and is
//! reused for the walk. Who stops it is the manager [`ManagerBackend::open`]
//! (one manager for every server) or [`ManagerBackend::open_split`] (a
//! manager per server) routes it to:
//!
//! * a manager that only the map holds (`[run_profile.lsp] sessions =
//!   "per_call"`, or a server that does not see an edit on its own under
//!   `sessions = "shared"` with `sync = "off"`: the caller builds one per
//!   call): the server stops when the map is dropped, because the manager's
//!   clients end the server's whole process tree when they drop;
//! * the process's own manager (`sessions = "shared"`, the default: the MCP
//!   and REST server's, the one navigation and the search channel use; only
//!   for a server that watches its workspace, today rust-analyzer, or under
//!   `sync = "watch"`): dropping the map stops nothing. The server stays for
//!   the next call and ends with that manager's idle sweeper
//!   (`lsp_idle_timeout_seconds`), `release_workspace`, a stuck-session
//!   replacement or service shutdown.
//!
//! A server kept in the process's manager passes the tree barrier when the
//! backend opens ([`Manager::tree_barrier`]: a stamp from the index's file
//! list, once per walk): a changed tree stops it and the walk starts a fresh
//! one ([`ManagerBackend::restarted`]), so a reused server is never staler
//! than under `per_call` for a change the stamp covers; the providers it kept
//! are listed when the walk ends by [`ManagerBackend::walk`] (the sessions its
//! requests found running, not the barrier's view: two walks can both pass
//! the barrier before either starts a server), and the walk labels their
//! answers.
//!
//! A missing server binary is found by [`ManagerBackend::open`], before any
//! walk, as an error the caller reports; the map is then not registered. The
//! version probe of a server runs once per manager ([`Manager::probe_once`]),
//! so a shared manager probes a binary once, not on every call. Under
//! `[run_profile.lsp] outline_cache = true` the manager also keeps each file's
//! non-empty outline by content hash and server session, for the servers that
//! are resent a file's text before every request (pyright, gopls, jdtls,
//! clangd; [`Manager::document_symbols`]): a per-call manager reuses it within
//! the walk, a shared one across calls too.

mod places;

pub use places::OutlinePlaces;

use crate::{
    Budget, Contribution, Cost, EXPAND_LIMIT, EdgeKind, Evidence, FreshnessModel, Map,
    MapDescriptor, MapHit, MapId, MapRequest, MapResponse, MapStatus, Need, SnapshotView,
    SourceLabel,
    edge::{CONF_MAX, Edge, Expanded, MapCard, QuestionType, Site, WitnessClass, cap_edges},
    live_grep::{CostLog, ScipSpans, SymbolLocator},
    scip_graph::ScipMap,
};
use lci_core::{
    config::LspSync,
    language,
    lsp::{HierarchyCall, HierarchyItem, Location, Manager, WalkSessions, adapter::LspAdapter},
    node::{NodeId, NodeKind},
    workspace::Workspace,
};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
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

/// How long the version probe of a server binary may take when the caller does
/// not say (`readiness_timeout_seconds` default). 15 s: a binary starting
/// under load took up to 4.3 s and one probe timed out at 5 s (d438, d450).
const PROBE_TIMEOUT: Duration = Duration::from_secs(15);

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
    /// The last line of the item (the line of the name for a server that
    /// gives no wider range).
    pub end_line: u32,
    /// The LSP `SymbolKind` number the server gave, if any.
    pub kind: Option<u64>,
}

/// A call-hierarchy item: where the callable's name is, and the server's own
/// item, which a follow-up request sends back verbatim (its `data` is opaque).
#[derive(Debug, Clone, PartialEq)]
pub struct CallItem {
    pub place: Place,
    pub raw: serde_json::Value,
}

/// A caller or callee of an item, with the places of the calls (in the
/// caller's file for a caller, in the asked item's file for a callee).
#[derive(Debug, Clone, PartialEq)]
pub struct CallLink {
    pub item: CallItem,
    pub sites: Vec<Place>,
}

/// The questions the map asks of a language server.
#[crate::async_trait]
pub trait LspBackend: Send + Sync {
    /// Where the symbol named at `at` is defined.
    async fn definition(&self, at: &Place) -> Result<Vec<Place>, String>;
    /// Where the symbol named at `at` is referred to (declaration left out).
    async fn references(&self, at: &Place) -> Result<Vec<Place>, String>;
    /// Where the member named at `at` is implemented (`textDocument/
    /// implementation`).
    async fn implementation(&self, at: &Place) -> Result<Vec<Place>, String>;
    /// The call-hierarchy items of the callable named at `at`
    /// (`textDocument/prepareCallHierarchy`).
    async fn prepare_call_hierarchy(&self, at: &Place) -> Result<Vec<CallItem>, String>;
    /// The callers of `item` (`callHierarchy/incomingCalls`).
    async fn incoming_calls(&self, item: &CallItem) -> Result<Vec<CallLink>, String>;
    /// The callees of `item` (`callHierarchy/outgoingCalls`).
    async fn outgoing_calls(&self, item: &CallItem) -> Result<Vec<CallLink>, String>;
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
    /// The nodes of the index's table in `path`: (line of the name, node). A
    /// table that has no index behind it gives none.
    fn nodes_in(&self, _path: &str) -> Vec<(u32, NodeId)> {
        Vec::new()
    }
    /// Takes in the server's outline of `path` (a table with no live layer
    /// ignores it). A second call for the same file replaces what the first
    /// one learned.
    fn learn(&self, _path: &str, _members: &[Member]) {}
    /// Whether the server's outline of `path` would add to the table: true
    /// until it is learned, never for a table with no live layer.
    fn outline_wanted(&self, _path: &str) -> bool {
        false
    }
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
        let nodes = self.by_file.get(path)?;
        let named = |id: &NodeId| matches!(id, NodeId::Symbol(key) if key.name == name);
        // The name's own line first: a range that now runs to the end of the
        // item must not re-target the edge to a same-named symbol inside it.
        nodes
            .iter()
            .find(|(line, id)| *line == lines.0 && named(id))
            .or_else(|| {
                nodes
                    .iter()
                    .find(|(line, id)| (lines.0..=lines.1).contains(line) && named(id))
            })
            .map(|(_, id)| id.clone())
    }

    fn nodes_in(&self, path: &str) -> Vec<(u32, NodeId)> {
        self.by_file.get(path).cloned().unwrap_or_default()
    }

    fn enclosing(&self, path: &str, line: u32) -> Option<NodeId> {
        self.spans.symbol_at(path, line)
    }
}

/// The language servers of `lci-core`, through a [`Manager`]: one session per
/// server and workspace, started by its first request. A manager is shared
/// (an `Arc`): a caller that keeps another reference keeps the sessions alive
/// past this backend (see the module notes). Each server is routed to one
/// manager when the backend opens ([`Self::open_split`]); the one-manager
/// constructors route every server to it.
pub struct ManagerBackend {
    workspace: Workspace,
    /// The enabled servers whose binary answered the version probe, each with
    /// the manager whose session answers its requests.
    adapters: Vec<(Arc<dyn LspAdapter>, Arc<Manager>)>,
    /// The providers whose kept session the tree barrier stopped because the
    /// tree changed since its last check (see [`Self::open_split`]).
    restarted: Vec<&'static str>,
    /// The sessions this walk's requests acquired, and which of them it found
    /// running instead of starting (see [`WalkSessions`]).
    walk: Arc<WalkSessions>,
}

impl ManagerBackend {
    /// Probes the enabled servers of `manager` (`--version`, no session; once
    /// per provider and manager), all at once, and keeps the ones that answer.
    /// An error when none does, naming each failure.
    pub async fn open(workspace: Workspace, manager: Arc<Manager>) -> Result<Self, String> {
        Self::open_with_probe_timeout(workspace, manager, PROBE_TIMEOUT).await
    }

    /// [`Self::open`] with `probe_timeout` as the bound of each version probe
    /// and so of the whole call, the probes running side by side
    /// (the traversal passes `readiness_timeout_seconds`; `open` uses
    /// [`PROBE_TIMEOUT`], which is that setting's default). Every server uses
    /// `manager`.
    pub async fn open_with_probe_timeout(
        workspace: Workspace,
        manager: Arc<Manager>,
        probe_timeout: Duration,
    ) -> Result<Self, String> {
        Self::open_split(workspace, Arc::clone(&manager), manager, probe_timeout).await
    }

    /// [`Self::open_with_probe_timeout`] for a process that has a long-lived
    /// `shared` manager and a throwaway `own` one (`sessions = "shared"`):
    /// the version probes run through `shared`, so its per-provider cache
    /// holds, and each probed server is then routed to `shared` when it sees
    /// an edit on its own ([`LspAdapter::watches_workspace`]) or `shared`
    /// syncs edits to its servers (`sync = "watch"`) and it lists the
    /// configuration files of its tree stamp ([`LspAdapter::keeps_session`]),
    /// and to `own` otherwise.
    /// A server on `own` starts after the last edit, as under `per_call`, and
    /// stops when `own` drops. A server on `shared` first passes the tree
    /// barrier ([`Manager::tree_barrier`], one stamp, once here and not per
    /// expansion, before the server is started or reused): its session is
    /// stopped, and so started again from the disk, when the stamp of its
    /// languages changed since the session was last checked or cannot be
    /// taken. So a reused server is never staler than under `per_call` for a
    /// change the stamp covers, whatever the server's own watcher or the sync
    /// did. No barrier when `shared` and `own` are one manager.
    pub async fn open_split(
        workspace: Workspace,
        shared: Arc<Manager>,
        own: Arc<Manager>,
        probe_timeout: Duration,
    ) -> Result<Self, String> {
        let mut adapters = Vec::new();
        let mut failures = Vec::new();
        // Side by side, each bounded by `probe_timeout`: a hanging binary costs
        // the caller one bound in all, not one per server.
        let enabled: Vec<&Arc<dyn LspAdapter>> =
            shared.adapters().iter().filter(|a| a.enabled()).collect();
        let probes = enabled
            .iter()
            .map(|adapter| shared.probe_once(adapter, probe_timeout));
        let synced = shared.sync_mode() == LspSync::Watch;
        let split = !Arc::ptr_eq(&shared, &own);
        let mut on_shared: Vec<&Arc<dyn LspAdapter>> = Vec::new();
        for (adapter, probed) in enabled.iter().zip(futures::future::join_all(probes).await) {
            match probed {
                Ok(()) => {
                    let manager = if adapter.keeps_session(synced) {
                        on_shared.push(*adapter);
                        &shared
                    } else {
                        &own
                    };
                    adapters.push((Arc::clone(adapter), Arc::clone(manager)));
                }
                Err(error) => failures.push(format!("{}: {error:#}", adapter.provider())),
            }
        }
        if adapters.is_empty() {
            return Err(format!(
                "no language server is available ({})",
                failures.join("; ")
            ));
        }
        let restarted = if split {
            shared.tree_barrier(&workspace, &on_shared).await
        } else {
            Vec::new()
        };
        Ok(Self {
            workspace,
            adapters,
            restarted,
            walk: Arc::default(),
        })
    }

    /// The providers whose kept session the barrier stopped when this backend
    /// opened: the tree changed since its last check, so the walk's server
    /// starts from the disk.
    pub fn restarted(&self) -> &[&'static str] {
        &self.restarted
    }

    /// The record of which sessions this backend's requests found running
    /// rather than started (read it when the walk ends: [`WalkSessions::reused`]
    /// names the providers whose answers came from a reused server, so the
    /// caller labels them). Take it before the backend is boxed.
    pub fn walk(&self) -> Arc<WalkSessions> {
        Arc::clone(&self.walk)
    }

    /// Each probed server's provider and the manager that runs it, for the
    /// walk-end readiness notes.
    pub fn routes(&self) -> Vec<(&'static str, Arc<Manager>)> {
        self.adapters
            .iter()
            .map(|(adapter, manager)| (adapter.provider(), Arc::clone(manager)))
            .collect()
    }

    /// The server for the file's language and the manager that runs it.
    fn adapter_for(&self, path: &str) -> Result<(&Arc<dyn LspAdapter>, &Arc<Manager>), String> {
        let language = language::for_path(Path::new(path))
            .map(|adapter| adapter.identifier())
            .ok_or_else(|| format!("no language is known for {path}"))?;
        self.adapters
            .iter()
            .find(|(adapter, _)| adapter.languages().contains(&language))
            .map(|(adapter, manager)| (adapter, manager))
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

fn call_item(item: HierarchyItem) -> Option<CallItem> {
    Some(CallItem {
        place: places(vec![item.location]).pop()?,
        raw: item.raw,
    })
}

fn call_links(calls: Vec<HierarchyCall>) -> Vec<CallLink> {
    calls
        .into_iter()
        .filter_map(|call| {
            Some(CallLink {
                item: call_item(call.item)?,
                sites: places(call.from_ranges),
            })
        })
        .collect()
}

#[crate::async_trait]
impl LspBackend for ManagerBackend {
    async fn definition(&self, at: &Place) -> Result<Vec<Place>, String> {
        let (adapter, manager) = self.adapter_for(&at.path)?;
        let file = self.workspace.path.join(&at.path);
        self.walk
            .scope(manager.definition(adapter, &self.workspace, &file, &at.path, at.line, at.col))
            .await
            .map(places)
            .map_err(|error| format!("{error:#}"))
    }

    async fn references(&self, at: &Place) -> Result<Vec<Place>, String> {
        let (adapter, manager) = self.adapter_for(&at.path)?;
        let file = self.workspace.path.join(&at.path);
        self.walk
            .scope(manager.references(
                adapter,
                &self.workspace,
                &file,
                &at.path,
                at.line,
                at.col,
                false,
            ))
            .await
            .map(places)
            .map_err(|error| format!("{error:#}"))
    }

    async fn implementation(&self, at: &Place) -> Result<Vec<Place>, String> {
        let (adapter, manager) = self.adapter_for(&at.path)?;
        let file = self.workspace.path.join(&at.path);
        self.walk
            .scope(manager.implementation(
                adapter,
                &self.workspace,
                &file,
                &at.path,
                at.line,
                at.col,
            ))
            .await
            .map(places)
            .map_err(|error| format!("{error:#}"))
    }

    async fn prepare_call_hierarchy(&self, at: &Place) -> Result<Vec<CallItem>, String> {
        let (adapter, manager) = self.adapter_for(&at.path)?;
        let file = self.workspace.path.join(&at.path);
        self.walk
            .scope(manager.prepare_call_hierarchy(
                adapter,
                &self.workspace,
                &file,
                &at.path,
                at.line,
                at.col,
            ))
            .await
            .map(|items| items.into_iter().filter_map(call_item).collect())
            .map_err(|error| format!("{error:#}"))
    }

    async fn incoming_calls(&self, item: &CallItem) -> Result<Vec<CallLink>, String> {
        let (adapter, manager) = self.adapter_for(&item.place.path)?;
        self.walk
            .scope(manager.incoming_calls(adapter, &self.workspace, &item.raw))
            .await
            .map(call_links)
            .map_err(|error| format!("{error:#}"))
    }

    async fn outgoing_calls(&self, item: &CallItem) -> Result<Vec<CallLink>, String> {
        let (adapter, manager) = self.adapter_for(&item.place.path)?;
        self.walk
            .scope(manager.outgoing_calls(adapter, &self.workspace, &item.raw))
            .await
            .map(call_links)
            .map_err(|error| format!("{error:#}"))
    }

    async fn document_symbols(&self, path: &str) -> Result<Vec<Member>, String> {
        let (adapter, manager) = self.adapter_for(path)?;
        let file = self.workspace.path.join(path);
        let found = self
            .walk
            .scope(manager.document_symbols(adapter, &self.workspace, &file, path))
            .await
            .map_err(|error| format!("{error:#}"))?;
        Ok(found
            .into_iter()
            .filter_map(|location| {
                Some(Member {
                    name: location.name.clone()?,
                    end_line: location.end_line.checked_sub(1)?,
                    kind: location.kind,
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
    /// Whether a symbol node also asks `textDocument/implementation`
    /// (`[run_profile.lsp] implementations`).
    implementations: bool,
    /// Whether a symbol node also asks the call hierarchy
    /// (`[run_profile.lsp] call_hierarchy`).
    call_hierarchy: bool,
}

/// What a call found, or why it stopped.
struct Found {
    edges: Vec<Edge>,
    calls: u32,
    /// The destinations that already have a [`EdgeKind::CallsOrRefs`] edge from
    /// the `definition` or `references` request or the call hierarchy: the
    /// kind has no direction and the engine fuses an expansion's edges by
    /// destination, so a call-hierarchy edge to one of them would only inflate
    /// the count. (The two older requests keep every edge they made.)
    refers: BTreeSet<NodeId>,
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
            implementations: false,
            call_hierarchy: false,
        }
    }

    /// Whether a symbol node also asks the server where it is implemented
    /// (default off): one more external call per symbol node, and
    /// [`EdgeKind::Implements`] edges.
    #[must_use]
    pub fn implementations(mut self, on: bool) -> Self {
        self.implementations = on;
        self
    }

    /// Whether a symbol node also asks the server for its call hierarchy
    /// (default off): up to three more external calls per symbol node (prepare,
    /// incoming, outgoing) and [`EdgeKind::CallsOrRefs`] edges to the callers
    /// and callees.
    #[must_use]
    pub fn call_hierarchy(mut self, on: bool) -> Self {
        self.call_hierarchy = on;
        self
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

    /// The call-hierarchy requests for the symbol `node` named at `at`: prepare,
    /// then the callers, then the callees of the item that answers, each behind
    /// the budget's gate. Every error reply is recorded and keeps the edges
    /// already found; a failed or empty prepare ends the block, a failed
    /// incoming request still asks outgoing.
    async fn ask_call_hierarchy(
        &self,
        node: &NodeId,
        at: &Place,
        max_calls: u32,
        found: &mut Found,
    ) {
        found.calls += 1;
        let items = match self.backend.prepare_call_hierarchy(at).await {
            Ok(items) => items,
            Err(error) => {
                self.fail(format!("textDocument/prepareCallHierarchy: {error}"));
                return;
            }
        };
        let Some(item) = items
            .iter()
            .find(|item| item.place == *at)
            .or_else(|| items.first())
        else {
            return;
        };
        if found.calls < max_calls {
            found.calls += 1;
            match self.backend.incoming_calls(item).await {
                Ok(links) => self.call_edges(node, links, found),
                Err(error) => self.fail(format!("callHierarchy/incomingCalls: {error}")),
            }
        }
        if found.calls < max_calls {
            found.calls += 1;
            match self.backend.outgoing_calls(item).await {
                Ok(links) => self.call_edges(node, links, found),
                Err(error) => self.fail(format!("callHierarchy/outgoingCalls: {error}")),
            }
        }
    }

    /// The edges of one call-hierarchy answer: one per destination (the symbol
    /// holding the caller's or callee's name, else its file; `node` itself
    /// left out) that no earlier request named, seen at the smallest place of
    /// the calls, else at the item's place. Which way the calls run is not in
    /// the edge (the kind has no direction); the request that made it is in the
    /// trace.
    fn call_edges(&self, node: &NodeId, links: Vec<CallLink>, found: &mut Found) {
        let mut linked: BTreeMap<NodeId, Place> = BTreeMap::new();
        for link in links {
            let Some(dst) = self
                .holder(&link.item.place, true)
                .filter(|dst| dst != node)
            else {
                continue;
            };
            let site = link.sites.into_iter().min().unwrap_or(link.item.place);
            let seen = linked.entry(dst).or_insert_with(|| site.clone());
            if site < *seen {
                *seen = site;
            }
        }
        for (dst, place) in linked {
            if found.refers.insert(dst.clone()) {
                found
                    .edges
                    .push(Self::edge(node, EdgeKind::CallsOrRefs, dst, &place));
            }
        }
    }

    /// The requests for `node`, in order, until `calls` reaches `max_calls`.
    async fn ask(&self, node: &NodeId, max_calls: u32, found: &mut Found) -> Result<(), String> {
        match node {
            NodeId::Symbol(_) => {
                let Some(mut at) = self.places.place_of(node) else {
                    return Ok(());
                };
                // Under `places = outline` the symbol's file is outlined once,
                // before the first request at it, and the live place replaces
                // the index's. A failed outline keeps the index's place and
                // counts as asked: the file is not asked again.
                if self.places.outline_wanted(&at.path) && found.calls < max_calls {
                    found.calls += 1;
                    match self.backend.document_symbols(&at.path).await {
                        Ok(members) => self.places.learn(&at.path, &members),
                        Err(error) => {
                            self.fail(format!("textDocument/documentSymbol: {error}"));
                            self.places.learn(&at.path, &[]);
                        }
                    }
                    if let Some(live) = self.places.place_of(node) {
                        at = live;
                    }
                }
                if found.calls < max_calls {
                    found.calls += 1;
                    for place in self.backend.definition(&at).await? {
                        if let Some(dst) = self.holder(&place, true).filter(|dst| dst != node) {
                            found.refers.insert(dst.clone());
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
                        found.refers.insert(dst.clone());
                        found
                            .edges
                            .push(Self::edge(node, EdgeKind::CallsOrRefs, dst, &place));
                    }
                }
                if self.implementations && found.calls < max_calls {
                    found.calls += 1;
                    // An answer this request cannot give (a server without
                    // `textDocument/implementation`, say) is recorded and
                    // costs the edges of the requests before it nothing.
                    match self.backend.implementation(&at).await {
                        Ok(mut implementors) => {
                            implementors.sort();
                            // The first place (in place order) in each destination.
                            let mut members: BTreeMap<NodeId, Place> = BTreeMap::new();
                            for place in implementors {
                                if let Some(dst) =
                                    self.holder(&place, true).filter(|dst| dst != node)
                                {
                                    members.entry(dst).or_insert(place);
                                }
                            }
                            for (dst, place) in members {
                                found.edges.push(Self::edge(
                                    node,
                                    EdgeKind::Implements,
                                    dst,
                                    &place,
                                ));
                            }
                        }
                        Err(error) => self.fail(format!("textDocument/implementation: {error}")),
                    }
                }
                if self.call_hierarchy && found.calls < max_calls {
                    self.ask_call_hierarchy(node, &at, max_calls, found).await;
                }
            }
            NodeId::File { path } | NodeId::Chunk { path, .. } if found.calls < max_calls => {
                found.calls += 1;
                let members = match self.backend.document_symbols(path).await {
                    Ok(members) => members,
                    Err(error) => {
                        // Asked, whatever the answer: no symbol of this file
                        // asks for its outline again.
                        self.places.learn(path, &[]);
                        return Err(error);
                    }
                };
                self.places.learn(path, &members);
                for member in members {
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
    /// [`Self::last_error`]; an error reply from the implementation request
    /// alone is recorded there too but keeps the node's other edges.
    async fn collect(&self, node: &NodeId, budget: Budget) -> Expanded {
        let started = Instant::now();
        let deadline = budget.deadline.min(started + CALL_TIMEOUT);
        let mut found = Found {
            edges: Vec::new(),
            calls: 0,
            refers: BTreeSet::new(),
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
