//! Named multi-step queries over the SCIP index and the config-key map. A
//! recipe runs the steps that a chain of single lookups would, server side,
//! and answers once with the nodes, the steps it ran, why it stopped and what
//! it cost. (History: spike v2, slice N3.)
//!
//! Off by default: the MCP and REST tools exist only with `[recipes] enabled`
//! (and `[compose] enabled` for `lci_compose`), and `lci recipe` runs them
//! without the service. Nothing here changes `search_code`.
//!
//! # Recipes
//!
//! * `config_readers(key)`: the declarations that read or set a key of
//!   `config.example.toml` (map `config.key`, exact witnesses only), and the
//!   tests that reference those declarations.
//! * `impact_rings(symbol, rings)`: ring 1 is the declarations that reference
//!   the symbol (the exact reverse closure, the same index as `exact.closure`);
//!   ring 2 is the declarations that reference a ring-1 declaration.
//! * `flow_path(from, to, max_depth)`: the functions on the shortest call
//!   paths from `from` to `to` in the SCIP call graph (`graph.scip`).
//! * `flow_from(entry, k)`: the walk of `lci traverse` with the `flow` policy,
//!   started at `entry`.
//! * `config_neighbours(symbol)`: the keys a declaration touches, the other
//!   declarations that touch them, and the declarations that reference it.
//! * `lci_compose(steps)`: up to six operator steps with `$sN.field`
//!   references (see [`compose`]).
//!
//! # What is shared
//!
//! A name is resolved through the SCIP index's declarations
//! ([`ImpactIndex::anchor_candidates`]), so types, constants, fields and enum
//! variants resolve, not only functions. A name that matches more than one
//! declaration stops the recipe with `ambiguous_anchor` and the candidates;
//! a `sym:` node id given back as the name resolves exactly.
//!
//! The closures read the index directly and are not capped by the 40-card
//! limit of a traversal; each recipe has its own caps, and a cut result says
//! so (`fanout_cap`, `partial`); every cut a recipe makes is listed in
//! `stop.unexplored`, and `stop.at` names the first. A declaration is a test
//! when its path has a `tests` or `test` directory or is `tests.rs`,
//! `*_test.rs` or `*_tests.rs`, or when its SCIP module path holds a `tests`
//! or `test` module (an inline `#[cfg(test)] mod tests`); no file is read.
//!
//! # Operator calls
//!
//! `counts.operator_calls` has one rule: one call per node an operator
//! expands. `exact.closure` and `scip.impact` count one per declaration whose
//! referrers they read; `config.key` one per key or declaration it expands;
//! `graph.scip` and the two search directions of `flow_path` one per
//! breadth-first pop below the depth limit, per direction (the expansions a
//! walk would make); `flow_from` the expansions the traversal reports per
//! map. `seeds.scip_names` counts one per call, and the set operators and
//! `pick` count none. It is also logged per request (`operator_calls`).

mod compose;
#[cfg(test)]
mod tests;

use crate::{
    app::{
        App,
        traverse::{self, BudgetArgs, Call, Dirs, OpenedIndex, SteerArgs},
    },
    config::{RunProfileConfig, TraverseConfig, TraverseFirstStage},
};
use anyhow::{Context, Result, anyhow, bail};
use lci_core::node::{ConfigNamespace, NodeId};
use lci_maps::{
    Budget, Map, SnapshotRef, SnapshotView,
    config_key::ConfigKeyMap,
    edge::Edge,
    impact::{Chain, ImpactIndex, code_names},
    reads_key,
    scip_graph::{GraphEdgeKind, ScipMap},
    scip_impact::ImpactMap,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    path::Path,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};

pub use compose::ComposeArgs;

/// The shape version of every recipe response.
pub const VERSION: u32 = 1;
/// The recipes, in the order the tools list them.
pub const RECIPES: [&str; 6] = [
    "config_readers",
    "impact_rings",
    "flow_path",
    "flow_from",
    "config_neighbours",
    "lci_compose",
];

/// Readers of a key a result lists.
const CAP_READERS: usize = 40;
/// Tests of the readers a result lists.
const CAP_TESTS: usize = 60;
/// Declarations of ring 1 a result lists.
const CAP_RING1: usize = 60;
/// Declarations of ring 2 a result lists.
const CAP_RING2: usize = 80;
/// Functions on the paths of `flow_path` a result lists.
const CAP_PATH: usize = 40;
/// Keys a declaration may touch before `config_neighbours` cuts.
const CAP_KEYS: usize = 20;
/// Other readers `config_neighbours` lists over all keys.
const CAP_OTHER_READERS: usize = 80;
/// The walk beam of `flow_from`: the edges of a node the traversal follows.
const RECIPE_BEAM: usize = 64;
/// Deepest `flow_path` search.
const MAX_PATH_DEPTH: u32 = 8;
/// The file whose keys `config.key` knows.
const KEY_FILE: &str = "config.example.toml";

fn yes() -> bool {
    true
}

/// `config_readers`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigReadersArgs {
    /// The workspace the SCIP index and `config.example.toml` belong to.
    pub workspace_path: String,
    /// A key of `config.example.toml`: `section.key`, a top-level `key`, a bare
    /// key name that only one section has, or `cfg:toml:<id>`.
    pub key: String,
    /// Also list the tests that reference the readers (default true).
    #[serde(default = "yes")]
    pub include_tests: bool,
}

/// `impact_rings`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ImpactRingsArgs {
    /// The workspace the SCIP index belongs to.
    pub workspace_path: String,
    /// A function, method, struct, enum, trait, type alias, constant, static,
    /// field or enum variant, by name (`Type::member`, `module::name`) or by
    /// the `node` id of an `ambiguous_anchor` candidate.
    pub symbol: String,
    /// 1 or 2 (default 2).
    pub rings: Option<u32>,
}

/// `flow_path`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowPathArgs {
    /// The workspace the SCIP index belongs to.
    pub workspace_path: String,
    /// The calling function or method, by name or `node` id.
    pub from: String,
    /// The called function or method, by name or `node` id.
    pub to: String,
    /// Longest call chain searched, 1 through 8 (default 4).
    pub max_depth: Option<u32>,
}

/// `flow_from`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FlowFromArgs {
    /// The workspace the SCIP index belongs to.
    pub workspace_path: String,
    /// The function or method the walk starts at, by name or `node` id.
    pub entry: String,
    /// Results, 1 through 40 (default 10).
    pub k: Option<u32>,
}

/// `config_neighbours`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ConfigNeighboursArgs {
    /// The workspace the SCIP index and `config.example.toml` belong to.
    pub workspace_path: String,
    /// The declaration to start at, by name or `node` id.
    pub symbol: String,
}

/// One recipe call.
#[derive(Debug, Clone)]
pub enum Request {
    ConfigReaders(ConfigReadersArgs),
    ImpactRings(ImpactRingsArgs),
    FlowPath(FlowPathArgs),
    FlowFrom(FlowFromArgs),
    ConfigNeighbours(ConfigNeighboursArgs),
    Compose(ComposeArgs),
}

impl Request {
    /// The request of recipe `name` from its JSON arguments. The arguments
    /// carry `workspace_path` (the command line fills it from `--root`).
    pub fn parse(name: &str, args: &Value) -> Result<Self> {
        let args = args.clone();
        let context = || format!("arguments of {name}");
        Ok(match name {
            "config_readers" => {
                Self::ConfigReaders(serde_json::from_value(args).with_context(context)?)
            }
            "impact_rings" => {
                Self::ImpactRings(serde_json::from_value(args).with_context(context)?)
            }
            "flow_path" => Self::FlowPath(serde_json::from_value(args).with_context(context)?),
            "flow_from" => Self::FlowFrom(serde_json::from_value(args).with_context(context)?),
            "config_neighbours" => {
                Self::ConfigNeighbours(serde_json::from_value(args).with_context(context)?)
            }
            "lci_compose" => Self::Compose(serde_json::from_value(args).with_context(context)?),
            other => bail!(
                "unknown recipe {other:?}; the recipes are {}",
                RECIPES.join(", ")
            ),
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Self::ConfigReaders(_) => RECIPES[0],
            Self::ImpactRings(_) => RECIPES[1],
            Self::FlowPath(_) => RECIPES[2],
            Self::FlowFrom(_) => RECIPES[3],
            Self::ConfigNeighbours(_) => RECIPES[4],
            Self::Compose(_) => RECIPES[5],
        }
    }

    /// The `workspace_path` the request carries.
    pub fn workspace_path(&self) -> &str {
        match self {
            Self::ConfigReaders(a) => &a.workspace_path,
            Self::ImpactRings(a) => &a.workspace_path,
            Self::FlowPath(a) => &a.workspace_path,
            Self::FlowFrom(a) => &a.workspace_path,
            Self::ConfigNeighbours(a) => &a.workspace_path,
            Self::Compose(a) => &a.workspace_path,
        }
    }

    fn needs_keys(&self) -> bool {
        match self {
            Self::ConfigReaders(_) | Self::ConfigNeighbours(_) => true,
            Self::Compose(args) => args.uses_keys(),
            _ => false,
        }
    }
}

/// What a recipe call reads.
pub struct Ctx<'a> {
    /// `[traverse]`, for the SCIP index path and the walk of `flow_from`.
    pub limits: &'a TraverseConfig,
    /// `[run_profile]`, which the walk of `flow_from` runs under.
    pub profile: &'a RunProfileConfig,
    /// Where traversal traces and handles go.
    pub dirs: &'a Dirs,
    /// The SCIP index file.
    pub scip_index: &'a Path,
    /// The tree the index describes.
    pub root: &'a Path,
    /// `[recipes] flow_judge`.
    pub flow_judge: bool,
}

/// The opened index, the impact map over it and, for the config recipes, the
/// `config.key` map with the keys of `config.example.toml`.
pub(crate) struct Sources {
    pub scip: Arc<ScipMap>,
    pub impact: Arc<ImpactMap>,
    pub keys: Option<Arc<ConfigKeyMap>>,
    /// The keys of `config.example.toml`: id (`section.key` or `key`) and line.
    pub key_ids: Vec<(String, u32)>,
}

/// What the MCP and REST tools call: the request's `workspace_path` as the
/// tree and the SCIP index under it, the server's `[traverse]` limits, and
/// the data directory for the traces of `flow_from`. The tool exists only with
/// its flag on; the flag is checked here as well.
pub async fn serve(app: &App, request: &Request) -> Result<Value> {
    let config = &app.config;
    let (on, flag) = match request {
        Request::Compose(_) => (config.compose.enabled, "[compose] enabled"),
        _ => (config.recipes.enabled, "[recipes] enabled"),
    };
    if !on {
        bail!(
            "{} is switched off on this server; set `{flag} = true` in its configuration",
            request.name()
        );
    }
    let given = request.workspace_path();
    let workspace = dunce::canonicalize(given)
        .with_context(|| format!("workspace_path {given:?} is not a directory"))?;
    if !workspace.is_dir() {
        bail!("workspace_path {given:?} is not a directory");
    }
    let scip_index = workspace.join(&config.traverse.scip_index);
    let dirs = Dirs::under(&config.data_dir);
    let ctx = Ctx {
        limits: &config.traverse,
        profile: &config.run_profile,
        dirs: &dirs,
        scip_index: &scip_index,
        root: &workspace,
        flow_judge: config.recipes.flow_judge,
    };
    run(&ctx, request).await
}

/// Runs one recipe and returns its JSON answer.
pub async fn run(ctx: &Ctx<'_>, request: &Request) -> Result<Value> {
    let started = Instant::now();
    let sources = open_sources(ctx, request.needs_keys()).await?;
    let value = match request {
        Request::ConfigReaders(args) => config_readers(&sources, args, started).await?,
        Request::ImpactRings(args) => impact_rings(&sources, args, started)?,
        Request::FlowPath(args) => flow_path(&sources, args, started)?,
        Request::FlowFrom(args) => flow_from(ctx, &sources, args, started).await?,
        Request::ConfigNeighbours(args) => config_neighbours(&sources, args, started).await?,
        Request::Compose(args) => compose::run(&sources, args, started).await?,
    };
    Ok(value)
}

async fn open_sources(ctx: &Ctx<'_>, want_keys: bool) -> Result<Sources> {
    let OpenedIndex { scip, impact } = traverse::open_index(ctx.scip_index, ctx.root).await?;
    let (keys, key_ids) = if want_keys {
        let path = ctx.root.join(KEY_FILE);
        let text = std::fs::read_to_string(&path).with_context(|| {
            format!("the config recipes read the keys of {KEY_FILE} in the workspace")
        })?;
        let key_ids = key_ids_of(&text);
        let map = traverse::open_config_key(ctx.scip_index, ctx.root)
            .await
            .context("config.key (needs `rg` on PATH)")?;
        (Some(map), key_ids)
    } else {
        (None, Vec::new())
    };
    Ok(Sources {
        scip,
        impact,
        keys,
        key_ids,
    })
}

/// The key ids of an example TOML, with their lines.
pub(crate) fn key_ids_of(text: &str) -> Vec<(String, u32)> {
    reads_key::parse_example_toml(text)
        .into_iter()
        .map(|(section, key, line)| match section {
            Some(section) => (format!("{section}.{key}"), line),
            None => (key, line),
        })
        .collect()
}

// ------------------------------------------------------------------ output

/// One listed node.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct Row {
    pub node: String,
    pub name: String,
    pub path: String,
    pub line: u32,
    pub role: &'static str,
    pub group: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ring: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub depth: Option<u32>,
    pub via: Vec<String>,
    pub witness: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conf: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Why a recipe stopped.
#[derive(Debug, Clone)]
pub(crate) struct Stop {
    pub at: String,
    pub reason: &'static str,
    pub partial: bool,
    pub unexplored: Vec<Value>,
    pub next: Option<Value>,
}

impl Stop {
    pub fn complete() -> Self {
        Self {
            at: "end".into(),
            reason: "complete",
            partial: false,
            unexplored: Vec::new(),
            next: None,
        }
    }

    pub fn at(step: &str, reason: &'static str, partial: bool) -> Self {
        Self {
            at: step.into(),
            reason,
            partial,
            unexplored: Vec::new(),
            next: None,
        }
    }

    pub fn unexplored(mut self, item: Value) -> Self {
        self.unexplored.push(item);
        self
    }

    /// A cut at `step`: every cut is listed in `unexplored`; `at` stays the
    /// step of the first.
    pub fn cap(&mut self, step: &str, item: Value) {
        if self.reason == "complete" {
            self.at = step.into();
            self.reason = "fanout_cap";
            self.partial = true;
        }
        self.unexplored.push(item);
    }

    pub fn next(mut self, recipe: &str, args: Value) -> Self {
        self.next = Some(json!({"recipe": recipe, "args": args}));
        self
    }
}

/// What a recipe has done so far.
#[derive(Default)]
pub(crate) struct Run {
    pub steps: Vec<Value>,
    pub rows: Vec<Row>,
    pub calls: u64,
    pub seen: u64,
    pub judge_calls: u64,
}

impl Run {
    pub fn step(
        &mut self,
        id: &str,
        op: &str,
        params: Value,
        n_in: usize,
        n_out: usize,
        at: Instant,
    ) {
        self.steps.push(json!({
            "id": id, "op": op, "params": params,
            "n_in": n_in, "n_out": n_out,
            "ms": at.elapsed().as_secs_f64() * 1000.0,
        }));
    }
}

/// The recipe's answer. Logs `operator_calls` once per request.
pub(crate) fn finish(
    recipe: &str,
    anchor: Value,
    run: Run,
    stop: Stop,
    started: Instant,
    extra: Option<(&str, Value)>,
) -> Value {
    let ms = started.elapsed().as_secs_f64() * 1000.0;
    tracing::info!(
        recipe,
        operator_calls = run.calls,
        cards_seen = run.seen,
        stop = stop.reason,
        partial = stop.partial,
        ms,
        "recipe"
    );
    let mut value = json!({
        "recipe": recipe,
        "version": VERSION,
        "anchor": anchor,
        "steps": run.steps,
        "results": run.rows,
        "stop": {
            "at": stop.at,
            "reason": stop.reason,
            "partial": stop.partial,
            "unexplored": stop.unexplored,
            "next": stop.next,
        },
        "counts": {
            "operator_calls": run.calls,
            "cards_seen": run.seen,
            "judge_calls": run.judge_calls,
            "ms": ms,
        },
    });
    if let (Some((key, extra)), Some(object)) = (extra, value.as_object_mut()) {
        object.insert(key.to_owned(), extra);
    }
    value
}

/// The role of the code at `path`, from the path alone.
pub(crate) fn role_of(path: &str) -> &'static str {
    let path = path.replace('\\', "/");
    let file = path.rsplit('/').next().unwrap_or(&path);
    let in_tests = path
        .split('/')
        .any(|part| part == "tests" || part == "test");
    if in_tests || file == "tests.rs" || file.ends_with("_test.rs") || file.ends_with("_tests.rs") {
        "test"
    } else {
        "prod"
    }
}

/// The role of a declaration: [`role_of`] its path, and `test` also when a
/// module of its SCIP symbol (every name before the last) is `tests` or
/// `test`, which is how an inline `#[cfg(test)] mod tests` shows. No I/O.
pub(crate) fn role_of_decl(path: &str, chain: Option<&Chain>) -> &'static str {
    let inline = chain.is_some_and(|chain| {
        let modules = &chain.names[..chain.names.len().saturating_sub(1)];
        modules.iter().any(|name| name == "tests" || name == "test")
    });
    if inline { "test" } else { role_of(path) }
}

// --------------------------------------------------------------- resolution

/// What a name resolved to.
pub(crate) enum Resolution {
    One(usize, &'static str),
    Many(Vec<usize>),
    Missing(String),
}

/// A recipe that could not start or go on: the anchor as far as it got and
/// the stop.
pub(crate) struct Halt {
    pub anchor: Value,
    pub stop: Stop,
}

impl Sources {
    pub fn ix(&self) -> &ImpactIndex {
        self.impact.index()
    }

    /// The graph node of a definition, when it is a function or method.
    pub fn graph_node(&self, def: usize) -> Option<usize> {
        self.scip.graph().node_index(&self.ix().defs[def].symbol)
    }

    /// The definition of a graph node.
    pub fn def_of_graph(&self, node: usize) -> Option<usize> {
        self.ix().def_index(&self.scip.graph().nodes[node].symbol)
    }

    pub fn node_string(&self, def: usize) -> String {
        match self.impact.node_of_def(def) {
            Some(node) => node.to_string(),
            None => self.ix().defs[def].symbol.clone(),
        }
    }

    /// The role of a definition: its path and its module path.
    pub fn role(&self, def: usize) -> &'static str {
        role_of_decl(&self.ix().defs[def].path, self.ix().chains[def].as_ref())
    }

    /// A row for a definition.
    pub fn row(&self, def: usize, group: &str, via: &[&str]) -> Row {
        let decl = self.ix().decl(def);
        Row {
            node: self.node_string(def),
            role: self.role(def),
            name: decl.name,
            path: decl.path,
            line: decl.line,
            group: group.to_owned(),
            ring: None,
            depth: None,
            via: via.iter().map(|id| (*id).to_owned()).collect(),
            witness: "exact".into(),
            conf: Some(100),
            note: None,
        }
    }

    /// The definitions a name or node id stands for.
    pub fn resolve(&self, given: &str) -> Resolution {
        let given = given.trim();
        if given.is_empty() {
            return Resolution::Missing("empty name".into());
        }
        if given.starts_with("sym:") {
            return match NodeId::from_str(given) {
                Ok(node) => match self.impact.def_of(&node) {
                    Some(def) => Resolution::One(def, "id"),
                    None => Resolution::Missing("no declaration has that node id".into()),
                },
                Err(_) => Resolution::Missing("not a name or a node id".into()),
            };
        }
        let ix = self.ix();
        // The raw SCIP symbol of a declaration that has no node id (a field,
        // an enum variant) is the `node` a candidate lists; it resolves back.
        if let Some(def) = ix.def_index(given) {
            return Resolution::One(def, "id");
        }
        let (names, _) = code_names(&format!("`{given}`"));
        if names.is_empty() {
            return Resolution::Missing("not a name: use identifiers joined by `::`".into());
        }
        let matched = ix.anchor_candidates(&names);
        let found: Vec<usize> = matched
            .iter()
            .copied()
            .filter(|&def| ix.anchorable(def))
            .collect();
        match found.len() {
            0 if !matched.is_empty() => {
                let unkinded = matched
                    .iter()
                    .filter(|&&def| ix.kinds[def].is_none())
                    .count();
                Resolution::Missing(if unkinded == matched.len() {
                    format!("found {unkinded} declarations without a kind; not anchorable")
                } else {
                    format!(
                        "found {} declarations that are not anchorable ({unkinded} without a kind)",
                        matched.len()
                    )
                })
            }
            0 => Resolution::Missing("no declaration of the index has that name".into()),
            1 => Resolution::One(found[0], "exact"),
            _ => Resolution::Many(found),
        }
    }

    /// The candidates of an ambiguous name, as the answer lists them.
    pub fn candidates(&self, defs: &[usize]) -> Vec<Value> {
        defs.iter()
            .take(12)
            .map(|&def| {
                let decl = self.ix().decl(def);
                json!({
                    "node": self.node_string(def),
                    "kind": self.ix().kind_name(def),
                    "name": self.ix().chains[def].as_ref().map_or_else(|| decl.name.clone(), |c| c.names.join("::")),
                    "path": decl.path,
                    "line": decl.line,
                    "refs": self.ix().ref_count(def),
                })
            })
            .collect()
    }

    /// Resolves one anchor or explains why the recipe stops.
    pub fn anchor(&self, given: &str, step: &str) -> Result<(usize, Value), Box<Halt>> {
        match self.resolve(given) {
            Resolution::One(def, how) => Ok((
                def,
                json!({"given": given, "resolved": self.node_string(def), "how": how,
                       "kind": self.ix().kind_name(def)}),
            )),
            Resolution::Many(defs) => {
                let candidates = self.candidates(&defs);
                let shown = candidates.len();
                let mut stop = Stop::at(step, "ambiguous_anchor", true);
                if defs.len() > shown {
                    stop = stop.unexplored(json!({"candidates_not_listed": defs.len() - shown}));
                }
                Err(Box::new(Halt {
                    anchor: json!({"given": given, "resolved": null, "how": "exact",
                                   "candidates": candidates}),
                    stop,
                }))
            }
            Resolution::Missing(why) => Err(Box::new(Halt {
                anchor: json!({"given": given, "resolved": null, "how": "exact", "why": why}),
                stop: Stop::at(step, "empty", false),
            })),
        }
    }

    /// The key ids a name can stand for.
    pub fn resolve_key(&self, given: &str) -> KeyResolution {
        let given = given.trim();
        let given = given.strip_prefix("cfg:toml:").unwrap_or(given);
        if self.key_ids.iter().any(|(id, _)| id == given) {
            return KeyResolution::One(given.to_owned(), "exact");
        }
        let bare: Vec<&str> = self
            .key_ids
            .iter()
            .map(|(id, _)| id.as_str())
            .filter(|id| id.rsplit('.').next() == Some(given))
            .collect();
        match bare.len() {
            1 => KeyResolution::One(bare[0].to_owned(), "key_name"),
            0 => KeyResolution::Missing(
                self.key_ids
                    .iter()
                    .map(|(id, _)| id.as_str())
                    .filter(|id| id.contains(given))
                    .take(8)
                    .map(str::to_owned)
                    .collect(),
            ),
            _ => KeyResolution::Many(bare.into_iter().map(str::to_owned).collect()),
        }
    }

    /// The `config.key` edges leaving `node`.
    pub async fn key_edges(&self, node: &NodeId) -> Result<Vec<Edge>> {
        let map = self
            .keys
            .as_ref()
            .ok_or_else(|| anyhow!("the config.key map is not open"))?;
        let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
        let budget = Budget {
            deadline: Instant::now() + Duration::from_secs(30),
            max_external_calls: 0,
            max_nodes_touched: u64::MAX,
        };
        Ok(map.expand(node, budget, &view).await.edges)
    }

    /// The declarations that touch a key, in the map's order.
    pub async fn key_readers(&self, key: &str) -> Result<Vec<Reader>> {
        let node = key_node(key);
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for edge in self.key_edges(&node).await? {
            if !matches!(edge.dst, NodeId::Symbol(_)) || !seen.insert(edge.dst.clone()) {
                continue;
            }
            out.push(self.reader(&edge));
        }
        Ok(out)
    }

    fn reader(&self, edge: &Edge) -> Reader {
        let def = self.impact.def_of(&edge.dst);
        let row = match def {
            Some(def) => {
                let mut row = self.row(def, "readers", &["s1"]);
                row.conf = Some(u32::from(edge.conf));
                row
            }
            None => {
                let (path, line) = edge.site.as_ref().map_or((String::new(), 0), |site| {
                    (site.path.clone(), site.line + 1)
                });
                let name = match &edge.dst {
                    NodeId::Symbol(key) => key.name.clone(),
                    other => other.to_string(),
                };
                Row {
                    node: edge.dst.to_string(),
                    role: role_of(&path),
                    name,
                    path,
                    line,
                    group: "readers".into(),
                    ring: None,
                    depth: None,
                    via: vec!["s1".into()],
                    witness: "exact".into(),
                    conf: Some(u32::from(edge.conf)),
                    note: None,
                }
            }
        };
        Reader { def, row }
    }
}

pub(crate) enum KeyResolution {
    One(String, &'static str),
    Many(Vec<String>),
    Missing(Vec<String>),
}

/// A declaration the key map named.
pub(crate) struct Reader {
    pub def: Option<usize>,
    pub row: Row,
}

pub(crate) fn key_node(key: &str) -> NodeId {
    NodeId::ConfigKey {
        namespace: ConfigNamespace::Toml,
        key: key.to_owned(),
    }
}

/// The key ids a declaration touches, in the map's order.
async fn keys_of_def(sources: &Sources, def: usize) -> Result<Vec<String>> {
    let Some(node) = sources.impact.node_of_def(def) else {
        return Ok(Vec::new());
    };
    let mut seen = HashSet::new();
    Ok(sources
        .key_edges(node)
        .await?
        .into_iter()
        .filter_map(|edge| match edge.dst {
            NodeId::ConfigKey { key, .. } => Some(key),
            _ => None,
        })
        .filter(|key| seen.insert(key.clone()))
        .collect())
}

fn halt(recipe: &str, halted: Halt, run: Run, started: Instant) -> Value {
    finish(recipe, halted.anchor, run, halted.stop, started, None)
}

/// Rows of the key node itself.
fn key_row(sources: &Sources, key: &str, group: &str, via: &[&str]) -> Row {
    let line = sources
        .key_ids
        .iter()
        .find(|(id, _)| id == key)
        .map_or(0, |(_, line)| *line);
    Row {
        node: key_node(key).to_string(),
        name: key.to_owned(),
        path: KEY_FILE.into(),
        line,
        role: "prod",
        group: group.to_owned(),
        ring: None,
        depth: None,
        via: via.iter().map(|id| (*id).to_owned()).collect(),
        witness: "exact".into(),
        conf: Some(100),
        note: None,
    }
}

// ------------------------------------------------------------ config_readers

async fn config_readers(
    sources: &Sources,
    args: &ConfigReadersArgs,
    started: Instant,
) -> Result<Value> {
    let recipe = "config_readers";
    let mut run = Run::default();
    let (key, how) = match sources.resolve_key(&args.key) {
        KeyResolution::One(key, how) => (key, how),
        KeyResolution::Many(ids) => {
            let anchor =
                json!({"given": args.key, "resolved": null, "how": "key_name", "candidates": ids});
            return Ok(finish(
                recipe,
                anchor,
                run,
                Stop::at("s1", "ambiguous_anchor", true),
                started,
                None,
            ));
        }
        KeyResolution::Missing(like) => {
            let anchor = json!({"given": args.key, "resolved": null, "how": "exact",
                                "why": format!("no key of {KEY_FILE} has that name"), "candidates": like});
            return Ok(finish(
                recipe,
                anchor,
                run,
                Stop::at("s1", "empty", false),
                started,
                None,
            ));
        }
    };
    let anchor = json!({"given": args.key, "resolved": key_node(&key).to_string(), "how": how});

    let at = Instant::now();
    let mut readers = sources.key_readers(&key).await?;
    run.calls += 1;
    run.seen += readers.len() as u64;
    run.step(
        "s1",
        "config.key",
        json!({"key": key}),
        1,
        readers.len(),
        at,
    );
    if readers.is_empty() {
        return Ok(finish(
            recipe,
            anchor,
            run,
            Stop::at("s1", "empty", false),
            started,
            None,
        ));
    }
    let mut stop = Stop::complete();
    if readers.len() > CAP_READERS {
        stop.cap(
            "s1",
            json!({"readers_not_listed": readers.len() - CAP_READERS}),
        );
        readers.truncate(CAP_READERS);
    }

    if args.include_tests {
        let at = Instant::now();
        let ix = sources.ix();
        let mut covers: BTreeMap<(String, u32, usize), Vec<String>> = BTreeMap::new();
        let mut expanded = 0;
        for reader in &readers {
            let Some(def) = reader.def else { continue };
            expanded += 1;
            for user in ix.closure(&ix.defs[def].symbol) {
                let d = &ix.defs[user];
                if user == def || sources.role(user) != "test" {
                    continue;
                }
                covers
                    .entry((d.path.clone(), d.name_range.0, user))
                    .or_default()
                    .push(reader.row.name.clone());
            }
        }
        run.calls += expanded as u64;
        run.seen += covers.len() as u64;
        let tests: Vec<(usize, Vec<String>)> = covers
            .into_iter()
            .map(|((_, _, def), names)| (def, names))
            .collect();
        run.step(
            "s2",
            "exact.closure",
            json!({"depth": 1, "role": "test"}),
            expanded,
            tests.len(),
            at,
        );
        if tests.len() > CAP_TESTS {
            stop.cap("s2", json!({"tests_not_listed": tests.len() - CAP_TESTS}));
        }
        let listed: Vec<Row> = tests
            .into_iter()
            .take(CAP_TESTS)
            .map(|(def, names)| {
                let mut row = sources.row(def, "tests", &["s2"]);
                row.note = Some(format!("references {}", names.join(", ")));
                row
            })
            .collect();
        run.rows
            .extend(readers.into_iter().map(|reader| reader.row));
        run.rows.extend(listed);
    } else {
        run.rows
            .extend(readers.into_iter().map(|reader| reader.row));
    }
    Ok(finish(recipe, anchor, run, stop, started, None))
}

// -------------------------------------------------------------- impact_rings

fn impact_rings(sources: &Sources, args: &ImpactRingsArgs, started: Instant) -> Result<Value> {
    let recipe = "impact_rings";
    let rings = args.rings.unwrap_or(2);
    if !(1..=2).contains(&rings) {
        bail!("rings must be 1 or 2");
    }
    let mut run = Run::default();
    let (def, anchor) = match sources.anchor(&args.symbol, "s1") {
        Ok(found) => found,
        Err(halted) => return Ok(halt(recipe, *halted, run, started)),
    };
    run.step(
        "s1",
        "resolve",
        json!({"symbol": args.symbol}),
        1,
        1,
        Instant::now(),
    );
    let ix = sources.ix();

    let at = Instant::now();
    let ring1: Vec<usize> = ix
        .closure(&ix.defs[def].symbol)
        .into_iter()
        .filter(|&user| user != def)
        .collect();
    run.calls += 1;
    run.seen += ring1.len() as u64;
    run.step(
        "s2",
        "exact.closure",
        json!({"symbol": ix.defs[def].symbol}),
        1,
        ring1.len(),
        at,
    );
    if ring1.is_empty() {
        let mut stop = Stop::at("s2", "empty", false);
        stop = stop.unexplored(json!({"references_of_the_symbol": ix.ref_count(def)}));
        return Ok(finish(recipe, anchor, run, stop, started, None));
    }
    let mut stop = Stop::complete();
    let mut listed1 = ring1.clone();
    if listed1.len() > CAP_RING1 {
        stop.cap(
            "s2",
            json!({"ring_1_not_listed": listed1.len() - CAP_RING1}),
        );
        stop.cap("s2", json!({"ring_2": "not computed from a cut ring 1"}));
        listed1.truncate(CAP_RING1);
    }
    for &user in &listed1 {
        let mut row = sources.row(user, "ring_1", &["s2"]);
        row.ring = Some(1);
        run.rows.push(row);
    }

    if rings == 2 && stop.reason == "complete" {
        let at = Instant::now();
        let in_ring1: HashSet<usize> = ring1.iter().copied().collect();
        let mut ring2: BTreeMap<(String, u32, usize), usize> = BTreeMap::new();
        for &user in &ring1 {
            for second in ix.closure(&ix.defs[user].symbol) {
                if second == def || in_ring1.contains(&second) {
                    continue;
                }
                let d = &ix.defs[second];
                ring2
                    .entry((d.path.clone(), d.name_range.0, second))
                    .or_insert(user);
            }
        }
        run.calls += ring1.len() as u64;
        run.seen += ring2.len() as u64;
        run.step(
            "s3",
            "exact.closure",
            json!({"depth": 1, "from": "ring_1"}),
            ring1.len(),
            ring2.len(),
            at,
        );
        let total = ring2.len();
        for ((_, _, second), via) in ring2.into_iter().take(CAP_RING2) {
            let mut row = sources.row(second, "ring_2", &["s3"]);
            row.ring = Some(2);
            row.note = Some(format!("references {}", sources.ix().display_name(via)));
            run.rows.push(row);
        }
        if total > CAP_RING2 {
            stop.cap("s3", json!({"ring_2_not_listed": total - CAP_RING2}));
        }
    }
    Ok(finish(recipe, anchor, run, stop, started, None))
}

/// The edges of `graph` a recipe over calls follows: the calls, references and
/// containment edges, never `Implements` (a trait-impl link is not a call).
fn followed(
    edges: &[lci_maps::scip_graph::Edge],
) -> impl Iterator<Item = &lci_maps::scip_graph::Edge> {
    edges
        .iter()
        .filter(|e| matches!(e.kind, GraphEdgeKind::CallsOrRefs | GraphEdgeKind::Contains))
}
// ----------------------------------------------------------------- flow_path

/// Distances from `start` along `adjacent`, at most `max` hops, and the
/// number of nodes expanded: the breadth-first pops below `max`.
fn distances(
    start: usize,
    max: u32,
    adjacent: &dyn Fn(usize) -> Vec<usize>,
) -> (HashMap<usize, u32>, u64) {
    let mut dist = HashMap::from([(start, 0u32)]);
    let mut queue = VecDeque::from([start]);
    let mut pops = 0;
    while let Some(node) = queue.pop_front() {
        let here = dist[&node];
        if here == max {
            continue;
        }
        pops += 1;
        for next in adjacent(node) {
            dist.entry(next).or_insert_with(|| {
                queue.push_back(next);
                here + 1
            });
        }
    }
    (dist, pops)
}

fn flow_path(sources: &Sources, args: &FlowPathArgs, started: Instant) -> Result<Value> {
    let recipe = "flow_path";
    let max_depth = args.max_depth.unwrap_or(4);
    if !(1..=MAX_PATH_DEPTH).contains(&max_depth) {
        bail!("max_depth must be 1 through {MAX_PATH_DEPTH}");
    }
    let mut run = Run::default();
    // Both ends are resolved before the recipe stops, so the anchor always
    // has the `{from, to}` shape and shows what each end came to.
    let mut anchors = Vec::new();
    let mut nodes = Vec::new();
    let mut halted: Option<Stop> = None;
    for (given, step) in [(&args.from, "s1"), (&args.to, "s2")] {
        match sources.anchor(given, step) {
            Err(halt) => {
                let halt = *halt;
                anchors.push(halt.anchor);
                halted.get_or_insert(halt.stop);
            }
            Ok((def, anchor)) => {
                let Some(node) = sources.graph_node(def) else {
                    anchors.push(json!({"given": given, "resolved": sources.node_string(def),
                        "kind": sources.ix().kind_name(def),
                        "why": "not a function or method: the call graph has no node for it"}));
                    halted.get_or_insert(Stop::at(step, "empty", false));
                    continue;
                };
                run.step(
                    step,
                    "resolve",
                    json!({"name": given}),
                    1,
                    1,
                    Instant::now(),
                );
                anchors.push(anchor);
                nodes.push(node);
            }
        }
    }
    let to_anchor = anchors.pop().unwrap_or(Value::Null);
    let from_anchor = anchors.pop().unwrap_or(Value::Null);
    let anchor = json!({"from": from_anchor, "to": to_anchor});
    if let Some(stop) = halted {
        return Ok(finish(recipe, anchor, run, stop, started, None));
    }
    let (from, to) = (nodes[0], nodes[1]);
    let graph = sources.scip.graph();

    let at = Instant::now();
    let (forward, pops) = distances(from, max_depth, &|n| {
        followed(&graph.out[n]).map(|e| e.to).collect()
    });
    run.calls += pops;
    run.step(
        "s3",
        "graph.scip",
        json!({"direction": "callees", "depth": max_depth}),
        1,
        forward.len(),
        at,
    );

    let at = Instant::now();
    let mut reverse_edges: Vec<Vec<usize>> = vec![Vec::new(); graph.nodes.len()];
    for (caller, edges) in graph.out.iter().enumerate() {
        for edge in followed(edges) {
            reverse_edges[edge.to].push(caller);
        }
    }
    let (backward, pops) = distances(to, max_depth, &|n| reverse_edges[n].clone());
    run.calls += pops;
    run.step(
        "s4",
        "graph.scip",
        json!({"direction": "callers", "depth": max_depth}),
        1,
        backward.len(),
        at,
    );
    run.seen += (forward.len() + backward.len()) as u64;

    let shortest = forward
        .iter()
        .filter_map(|(node, f)| backward.get(node).map(|b| f + b))
        .min();
    let Some(shortest) = shortest.filter(|&len| len <= max_depth) else {
        let mut stop = Stop::at("s4", "empty", false).unexplored(
            json!({"forward_frontier": forward.len(), "reverse_frontier": backward.len(),
                               "max_depth": max_depth}),
        );
        if max_depth < MAX_PATH_DEPTH {
            stop = stop.next(
                recipe,
                json!({"from": args.from, "to": args.to,
                       "max_depth": (max_depth + 2).min(MAX_PATH_DEPTH)}),
            );
        }
        return Ok(finish(recipe, anchor, run, stop, started, None));
    };
    let mut on_path: Vec<(u32, String, usize)> = forward
        .iter()
        .filter(|(node, f)| backward.get(node).is_some_and(|b| *f + b == shortest))
        .map(|(&node, &f)| (f, graph.nodes[node].name.clone(), node))
        .collect();
    on_path.sort();
    let total = on_path.len();
    let mut stop = Stop::complete();
    if total > CAP_PATH {
        stop.cap("s4", json!({"path_nodes_not_listed": total - CAP_PATH}));
    }
    for (depth, _, node) in on_path.into_iter().take(CAP_PATH) {
        let Some(def) = sources.def_of_graph(node) else {
            continue;
        };
        let mut row = sources.row(def, "path", &["s3", "s4"]);
        row.depth = Some(depth);
        run.rows.push(row);
    }
    Ok(finish(
        recipe,
        anchor,
        run,
        stop,
        started,
        Some(("path_length", json!(shortest))),
    ))
}

// ----------------------------------------------------------------- flow_from

async fn flow_from(
    ctx: &Ctx<'_>,
    sources: &Sources,
    args: &FlowFromArgs,
    started: Instant,
) -> Result<Value> {
    let recipe = "flow_from";
    let k = args.k.unwrap_or(10);
    if !(1..=40).contains(&k) {
        bail!("k must be 1 through 40");
    }
    let mut run = Run::default();
    let (def, anchor) = match sources.anchor(&args.entry, "s1") {
        Ok(found) => found,
        Err(halted) => return Ok(halt(recipe, *halted, run, started)),
    };
    run.step(
        "s1",
        "resolve",
        json!({"name": args.entry}),
        1,
        1,
        Instant::now(),
    );
    if sources.graph_node(def).is_none() {
        let anchor = json!({"given": args.entry, "resolved": sources.node_string(def),
            "why": "not a function or method: the call graph has no node for it"});
        return Ok(finish(
            recipe,
            anchor,
            run,
            Stop::at("s1", "empty", false),
            started,
            None,
        ));
    }
    let seed = sources.node_string(def);
    // The recipe reads the walk's own cards: no first stage ahead of them.
    let limits = TraverseConfig {
        beam: ctx.limits.beam.max(RECIPE_BEAM),
        first_stage: TraverseFirstStage::Off,
        answer_seeds: false,
        exact_seeds: false,
        ..ctx.limits.clone()
    };
    let maps = ["graph.scip".to_owned(), "structure.contains".to_owned()];
    let steer = SteerArgs {
        policy: Some("flow".into()),
        ..SteerArgs::default()
    };
    let budget = BudgetArgs::default();
    let seeds = [seed.clone()];
    let call = Call {
        scip_index: ctx.scip_index,
        scip_root: Some(ctx.root),
        question: &args.entry,
        seeds: &seeds,
        seed_search: None,
        top_k: k as usize,
        maps: Some(&maps[..]),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: None,
    };
    let at = Instant::now();
    let report = Box::pin(traverse::run(&limits, ctx.profile, ctx.dirs, &call)).await?;
    let response = report.response;
    let expands: u64 = response["totals"]["per_map"]
        .as_object()
        .map(|maps| maps.values().filter_map(|m| m["expands"].as_u64()).sum())
        .unwrap_or(0);
    run.calls += expands;
    let cards = response["results"].as_array().cloned().unwrap_or_default();
    run.seen += cards.len() as u64;
    run.step(
        "s2",
        "traverse",
        json!({"policy": "flow", "beam": limits.beam, "top_k": k, "maps": maps}),
        1,
        cards.len(),
        at,
    );
    for card in &cards {
        let Some(node_text) = card["node"].as_str() else {
            continue;
        };
        if node_text == seed {
            continue;
        }
        let row = match NodeId::from_str(node_text)
            .ok()
            .and_then(|n| sources.impact.def_of(&n))
        {
            Some(found) => sources.row(found, "walk", &["s2"]),
            None => Row {
                node: node_text.to_owned(),
                name: node_text.to_owned(),
                path: card["site"]["path"].as_str().unwrap_or_default().to_owned(),
                line: card["site"]["line"].as_u64().unwrap_or(0) as u32,
                role: role_of(card["site"]["path"].as_str().unwrap_or_default()),
                group: if node_text.starts_with("file:") {
                    "file"
                } else {
                    "walk"
                }
                .into(),
                ring: None,
                depth: None,
                via: vec!["s2".into()],
                witness: "walk".into(),
                conf: None,
                note: None,
            },
        };
        let mut row = row;
        row.depth = card["depth"].as_u64().map(|d| d as u32);
        row.witness = card["witness"].as_str().unwrap_or("walk").to_owned();
        row.conf = None;
        run.rows.push(row);
    }
    let reason = response["stop_reason"].as_str().unwrap_or("complete");
    let complete = response["status"].as_str().unwrap_or("complete") == "complete";
    let mut stop = match reason {
        "budget" | "pops" | "deadline" => Stop::at("s2", "over_budget", true),
        _ if !complete => Stop::at("s2", "over_budget", true),
        _ => Stop::complete(),
    };
    if ctx.flow_judge {
        stop = stop.unexplored(json!({
            "flow_judge": "enabled, but no judge is installed in this build: walk order is unchanged and judge_calls is 0"
        }));
    }
    let trace = response["trace"]["file"].clone();
    Ok(finish(
        recipe,
        anchor,
        run,
        stop,
        started,
        Some((
            "walk",
            json!({"stop_reason": reason, "status": response["status"],
                             "trace": trace, "seeds": response["seeds"]}),
        )),
    ))
}

// ---------------------------------------------------------- config_neighbours

async fn config_neighbours(
    sources: &Sources,
    args: &ConfigNeighboursArgs,
    started: Instant,
) -> Result<Value> {
    let recipe = "config_neighbours";
    let mut run = Run::default();
    let (def, anchor) = match sources.anchor(&args.symbol, "s1") {
        Ok(found) => found,
        Err(halted) => return Ok(halt(recipe, *halted, run, started)),
    };
    run.step(
        "s1",
        "resolve",
        json!({"symbol": args.symbol}),
        1,
        1,
        Instant::now(),
    );

    let at = Instant::now();
    let mut keys = keys_of_def(sources, def).await?;
    run.calls += 1;
    run.seen += keys.len() as u64;
    run.step(
        "s2",
        "config.key",
        json!({"direction": "keys_of_symbol"}),
        1,
        keys.len(),
        at,
    );
    if keys.is_empty() {
        let stop =
            Stop::at("s2", "empty", false).next("impact_rings", json!({"symbol": args.symbol}));
        return Ok(finish(recipe, anchor, run, stop, started, None));
    }
    let mut stop = Stop::complete();
    if keys.len() > CAP_KEYS {
        stop.cap("s2", json!({"keys_not_listed": keys.len() - CAP_KEYS}));
        keys.truncate(CAP_KEYS);
    }
    for key in &keys {
        run.rows.push(key_row(sources, key, "keys", &["s2"]));
    }

    let at = Instant::now();
    let mut others: BTreeMap<String, (Row, Vec<String>)> = BTreeMap::new();
    let mut order = Vec::new();
    for key in &keys {
        run.calls += 1;
        for reader in sources.key_readers(key).await? {
            if reader.def == Some(def) {
                continue;
            }
            let id = reader.row.node.clone();
            let entry = others.entry(id.clone()).or_insert_with(|| {
                order.push(id.clone());
                (reader.row, Vec::new())
            });
            entry.1.push(key.clone());
        }
    }
    run.seen += others.len() as u64;
    run.step(
        "s3",
        "config.key",
        json!({"direction": "readers_of_key", "keys": keys}),
        keys.len(),
        others.len(),
        at,
    );
    let total = others.len();
    if total > CAP_OTHER_READERS {
        stop.cap(
            "s3",
            json!({"other_readers_not_listed": total - CAP_OTHER_READERS}),
        );
    }
    for id in order.into_iter().take(CAP_OTHER_READERS) {
        if let Some((mut row, via_keys)) = others.remove(&id) {
            row.group = "other_readers".into();
            row.via = vec!["s3".into()];
            row.note = Some(format!("reads {}", via_keys.join(", ")));
            run.rows.push(row);
        }
    }

    let at = Instant::now();
    let ix = sources.ix();
    let callers: Vec<usize> = ix
        .closure(&ix.defs[def].symbol)
        .into_iter()
        .filter(|&user| user != def)
        .collect();
    run.calls += 1;
    run.seen += callers.len() as u64;
    run.step(
        "s4",
        "exact.closure",
        json!({"symbol": ix.defs[def].symbol}),
        1,
        callers.len(),
        at,
    );
    if callers.len() > CAP_RING1 {
        stop.cap(
            "s4",
            json!({"callers_not_listed": callers.len() - CAP_RING1}),
        );
    }
    for user in callers.into_iter().take(CAP_RING1) {
        run.rows.push(sources.row(user, "callers", &["s4"]));
    }
    Ok(finish(recipe, anchor, run, stop, started, None))
}
