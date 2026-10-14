//! Graph traversal for `search_code` `mode = "traverse"` and for the
//! `lci traverse` command (multimap spine slice S6; spine brief 1.7).
//!
//! One function, [`run`], serves both. It loads the SCIP index (map
//! `graph.scip`, cached by file stamp), finds the start nodes from the symbol
//! names in the question (map `seeds.scip_names`, a stand-in until the legacy
//! channels are wrapped as maps), builds the steering ladder from the caller's
//! rules, runs the traversal from `lci-maps`, writes the trace, and answers
//! with the reached nodes, the totals, a summary of what the handlers decided
//! and, only when the call ended with work left (the ladder could not decide a
//! stop, or the budget ran out), a `handle` and the `checkpoint`.
//!
//! The mode touches neither the index store nor the model services. The
//! request can only shrink or reorder the walk: its budget is clamped to the
//! `[traverse]` caps, its rules are parsed into the closed rule AST, and the
//! only way to add pops or tokens is a `budget` action in `steer.actions`
//! on a continuation, within the engine's own limits.

use crate::config::TraverseConfig;
use anyhow::{Context, Result, anyhow, bail};
use lci_core::node::{NodeId, NodeKind};
use lci_maps::{
    Budget, Contribution, EdgeKind, Evidence, FreshnessModel, Map, MapDescriptor, MapHit, MapId,
    MapRequest, MapResponse, MapStatus, Registry, SnapshotRef, SnapshotView, SourceLabel,
    scip_graph::{DirSource, SCIP_MAP_ID, ScipMap, SourceText},
    traverse::{
        Seed, Traversal, TraversalBudget, TraversalResult,
        handle::HandleStore,
        handler::{Checkpoint, StopPolicy},
        ladder::Ladder,
        rules::RuleList,
        source::OnTheFly,
        steer::Steer,
        stop::Card,
        trace,
    },
    walk::tokenize,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex, OnceLock, PoisonError},
    time::{Duration, Instant, SystemTime},
};

/// The map that finds the start nodes from the question's words.
pub const SEEDS_MAP: MapId = "seeds.scip_names";
/// The maps a traversal can use, in registration order.
pub const MAPS: [MapId; 2] = [SEEDS_MAP, SCIP_MAP_ID];
/// Start nodes taken from the question.
const SEED_COUNT: usize = 3;
/// External calls a traversal may make; no map here makes any.
const EXTERNAL_CALLS: u32 = 100;
/// Most results a call returns.
const MAX_TOP_K: usize = 40;
/// Longest accepted handle text.
const MAX_HANDLE: usize = 64;

/// `budget` of a traversal request. Each value may lower the `[traverse]`
/// cap of the same name, never raise it.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetArgs {
    /// Most node expansions (pops).
    pub pops: Option<u32>,
    /// Deepest state that is expanded.
    pub max_depth: Option<u32>,
    /// Most estimated tokens the expansions may cost.
    pub est_tokens: Option<u64>,
    /// Longest the call may run, in milliseconds.
    pub deadline_ms: Option<u64>,
}

/// `steer` of a traversal request: the rules the ladder applies at each
/// stop, how many stops there may be, and, on a continuation, the outer
/// agent's answer to a returned checkpoint.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SteerArgs {
    /// Rung 1 of the ladder: an array of `{"when": predicate, "then": action}`
    /// rules (at most 64). Predicates: `kind`, `path_glob`, `witness`, `map`,
    /// `score`, `fanout`, `depth`, `budget_left`, `seen_before`, `all`, `any`,
    /// `not`. Actions: `prune`, `expand`, `continue`, `stop`, `{"boost": f}`,
    /// `{"map_on": id}`, `{"map_off": id}`, `{"map_weight": {"id", "weight"}}`,
    /// `{"rerank": "witness"|"depth"|"fanout"}`, `{"redirect": [node ids]}`,
    /// `{"emit": note}`. They can only shrink, reorder or redirect the walk.
    pub rules: Option<Value>,
    /// Reserved for an embedded script handler; not available yet.
    pub script: Option<String>,
    /// The rungs, in order, ending with `agent`. Only `rules` and `agent` are
    /// available. A stop no rung can decide returns the checkpoint to the
    /// caller.
    pub ladder: Option<Vec<String>>,
    /// Most stops in the traversal (never above the `[traverse]` cap).
    pub max_stops: Option<u32>,
    /// Confidence a rung needs to decide a stop, 0 to 1.
    pub tau: Option<f32>,
    /// A stop every this many pops. Without it, stops happen every
    /// `pops / (max_stops + 1)` pops while rules are given.
    pub stop_every: Option<u32>,
    /// Only with `handle`: the outer agent's answer to the checkpoint,
    /// `[{"node"?: id, "action": ..}]` with the actions of the rule list plus
    /// `{"budget": {"pops": n, "tokens": n}}` (bounded).
    pub actions: Option<Value>,
}

/// One traversal call.
pub struct Call<'a> {
    /// The SCIP index to walk.
    pub scip_index: &'a Path,
    /// The tree it indexes (read for the enclosing-range repair), if known.
    pub scip_root: Option<&'a Path>,
    pub question: &'a str,
    /// Explicit start nodes (canonical node ids); empty finds them from the
    /// question.
    pub seeds: &'a [String],
    /// Results to return, 1 through 40.
    pub top_k: usize,
    /// The maps to use; `None` is all of [`MAPS`].
    pub maps: Option<&'a [String]>,
    pub budget: &'a BudgetArgs,
    pub steer: &'a SteerArgs,
    /// Continue the traversal this handle names.
    pub handle: Option<&'a str>,
}

/// Where a call keeps what it writes.
pub struct Dirs {
    /// `<name>.jsonl` traces.
    pub traces: PathBuf,
    /// `<handle>.json` snapshots.
    pub handles: PathBuf,
}

impl Dirs {
    /// The layout under the data directory: `traces/` (brief 1.6) and
    /// `traverse/handles/`.
    pub fn under(data_dir: &Path) -> Self {
        Self {
            traces: data_dir.join("traces"),
            handles: data_dir.join("traverse").join("handles"),
        }
    }
}

/// What a call produced.
pub struct Report {
    /// The JSON the caller gets.
    pub response: Value,
    /// The trace file, when it could be written.
    pub trace: Option<PathBuf>,
}

/// The SCIP index and the name index over it, shared between calls.
struct Loaded {
    stamp: (Option<SystemTime>, u64),
    scip: Arc<ScipMap>,
    names: Arc<NameSeeds>,
}

type Cache = Mutex<HashMap<(PathBuf, Option<PathBuf>), Arc<Loaded>>>;

fn cache() -> &'static Cache {
    static CACHE: OnceLock<Cache> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

async fn load(index: &Path, root: Option<&Path>) -> Result<Arc<Loaded>> {
    let meta = std::fs::metadata(index).with_context(|| {
        format!(
            "no SCIP index at {}; write one with `rust-analyzer scip` in the workspace or set traverse.scip_index",
            index.display()
        )
    })?;
    let stamp = (meta.modified().ok(), meta.len());
    let key = (index.to_path_buf(), root.map(Path::to_path_buf));
    if let Some(found) = cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
        .filter(|loaded| loaded.stamp == stamp)
    {
        return Ok(found.clone());
    }
    let (path, root_dir) = (index.to_path_buf(), root.map(Path::to_path_buf));
    let loaded = tokio::task::spawn_blocking(move || -> Result<Loaded> {
        let source = root_dir.map(DirSource);
        let graph = lci_maps::scip_graph::ScipGraph::load(
            &path,
            source.as_ref().map(|s| s as &dyn SourceText),
        )
        .map_err(|error| anyhow!(error))?;
        let scip = ScipMap::with_default_ids(graph);
        let names = NameSeeds::new(&scip);
        Ok(Loaded {
            stamp,
            scip: Arc::new(scip),
            names: Arc::new(names),
        })
    })
    .await
    .context("load the SCIP index")??;
    let loaded = Arc::new(loaded);
    cache()
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, loaded.clone());
    Ok(loaded)
}

/// Matches the question's words against symbol names. Text only: it never
/// expands a node. A stand-in for the lexical and vector channels until they
/// are wrapped as maps.
struct NameSeeds {
    descriptor: MapDescriptor,
    names: Vec<(NodeId, HashSet<String>)>,
}

impl NameSeeds {
    fn new(scip: &ScipMap) -> Self {
        let graph = scip.graph();
        Self {
            descriptor: MapDescriptor {
                id: SEEDS_MAP,
                version: 1,
                contribution: Contribution::RankList,
                accepts_text: true,
                accepts: Vec::new(),
                emits: vec![NodeKind::Symbol],
                freshness: FreshnessModel::PerFile,
                source_labels: vec![SourceLabel::Lexical],
                needs: Vec::new(),
            },
            names: (0..graph.nodes.len())
                .map(|i| {
                    (
                        scip.node_id(i).clone(),
                        tokenize(&graph.nodes[i].name).into_iter().collect(),
                    )
                })
                .collect(),
        }
    }

    fn matches(&self, text: &str, limit: usize) -> Vec<NodeId> {
        let words = tokenize(text);
        let mut scored: Vec<(usize, &NodeId)> = self
            .names
            .iter()
            .map(|(id, tokens)| (words.iter().filter(|w| tokens.contains(*w)).count(), id))
            .filter(|(n, _)| *n > 0)
            .collect();
        scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)));
        scored
            .into_iter()
            .take(limit)
            .map(|(_, id)| id.clone())
            .collect()
    }
}

#[lci_maps::async_trait]
impl Map for NameSeeds {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let mut response = MapResponse::empty(&self.descriptor, view, MapStatus::Ok);
        response.hits = self
            .matches(request.text.unwrap_or_default(), request.limit)
            .into_iter()
            .zip(1..)
            .map(|(node, rank)| MapHit {
                node,
                rank,
                score: None,
                evidence: vec![Evidence {
                    via: None,
                    edge: EdgeKind::QueryMatch,
                    label: SourceLabel::Lexical,
                    detail: None,
                }],
            })
            .collect();
        response
    }
}

/// A map shared between calls, registered by value.
struct Shared<M>(Arc<M>);

#[lci_maps::async_trait]
impl<M: Map> Map for Shared<M> {
    fn describe(&self) -> &MapDescriptor {
        self.0.describe()
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        self.0.query(request, view).await
    }

    async fn expand(
        &self,
        node: &NodeId,
        budget: Budget,
        view: &SnapshotView<'_>,
    ) -> Vec<lci_maps::edge::Edge> {
        self.0.expand(node, budget, view).await
    }

    fn card(&self) -> lci_maps::edge::MapCard {
        self.0.card()
    }
}

/// The caller's budget over the caps: the lower of the two for each value.
/// Anything the request asked for above a cap is named in the second part.
fn clamp_budget(limits: &TraverseConfig, asked: &BudgetArgs) -> (TraversalBudget, Vec<String>) {
    let mut notes = Vec::new();
    let mut take = |name: &str, asked: Option<u64>, cap: u64| -> u64 {
        match asked {
            Some(value) if value > cap => {
                notes.push(format!(
                    "budget.{name} {value} is above the cap {cap}; the cap applies"
                ));
                cap
            }
            Some(value) => value,
            None => cap,
        }
    };
    let pops = take(
        "pops",
        asked.pops.map(u64::from),
        u64::from(limits.max_pops),
    );
    let depth = take(
        "max_depth",
        asked.max_depth.map(u64::from),
        u64::from(limits.max_depth),
    );
    let tokens = take("est_tokens", asked.est_tokens, limits.max_est_tokens);
    let deadline = take("deadline_ms", asked.deadline_ms, limits.deadline_ms);
    let budget = TraversalBudget {
        pops: u32::try_from(pops).unwrap_or(u32::MAX).max(1),
        max_depth: u32::try_from(depth).unwrap_or(u32::MAX),
        deadline: Instant::now() + Duration::from_millis(deadline.max(1)),
        est_tokens: tokens.max(1),
        external_calls: EXTERNAL_CALLS,
    };
    (budget, notes)
}

/// The handler a call runs under, if it has one. The stop policy is only
/// meaningful with a handler.
fn build_ladder(
    limits: &TraverseConfig,
    steer: &SteerArgs,
    pops: u32,
    notes: &mut Vec<String>,
) -> Result<Option<(Ladder, StopPolicy)>> {
    if steer.script.is_some() {
        bail!("steer.script is not available yet; give the rules in steer.rules");
    }
    if let Some(ladder) = &steer.ladder {
        let mut seen_agent = false;
        for (at, rung) in ladder.iter().enumerate() {
            if seen_agent {
                bail!("steer.ladder: `agent` must be the last rung");
            }
            match rung.as_str() {
                "rules" if at == 0 => {}
                "rules" => bail!("steer.ladder: `rules` must be the first rung"),
                "agent" => seen_agent = true,
                "system_one" => bail!("steer.ladder: the system_one rung is not available yet"),
                other => bail!(
                    "steer.ladder: unknown rung {other:?}; the rungs are rules, system_one and agent"
                ),
            }
        }
        if !seen_agent {
            bail!(
                "steer.ladder must end with `agent`: a stop no rung can decide is returned to the caller"
            );
        }
    }
    if steer.rules.is_none() && steer.stop_every.is_none() {
        if steer.max_stops.is_some() || steer.tau.is_some() {
            notes.push(
                "steer.max_stops and steer.tau do nothing without steer.rules or steer.stop_every"
                    .to_owned(),
            );
        }
        return Ok(None);
    }
    let rules = match &steer.rules {
        Some(value) => RuleList::from_json(value).map_err(|e| anyhow!("steer.rules: {e}"))?,
        None => RuleList::new(Vec::new()).map_err(|e| anyhow!("steer.rules: {e}"))?,
    };
    let tau = steer.tau.unwrap_or(limits.tau);
    let ladder = Ladder::rules(tau, rules).map_err(|e| anyhow!("steer.tau: {e}"))?;
    let max_stops = match steer.max_stops {
        Some(asked) if asked > limits.max_stops => {
            notes.push(format!(
                "steer.max_stops {asked} is above the cap {}; the cap applies",
                limits.max_stops
            ));
            limits.max_stops
        }
        Some(asked) => asked,
        None => limits.max_stops,
    };
    let every = match steer.stop_every {
        Some(0) => bail!("steer.stop_every must be at least 1"),
        Some(every) => every,
        None => pops.div_ceil(max_stops + 1).max(1),
    };
    Ok(Some((
        ladder,
        StopPolicy::every(every).max_stops(max_stops),
    )))
}

fn parse_seeds(seeds: &[String]) -> Result<Vec<NodeId>> {
    seeds
        .iter()
        .map(|text| NodeId::from_str(text).map_err(|e| anyhow!("seed {text:?}: {e}")))
        .collect()
}

fn check_handle(handle: &str) -> Result<()> {
    if handle.is_empty() || handle.len() > MAX_HANDLE {
        bail!("handle is not a handle this server issued");
    }
    Ok(())
}

/// Runs one traversal call. Errors are the caller's mistakes (a bad rule, an
/// unknown handle, a missing index) or the data directory not being writable
/// for a handle; a failed trace write is a note, not an error.
pub async fn run(limits: &TraverseConfig, dirs: &Dirs, call: &Call<'_>) -> Result<Report> {
    if call.top_k == 0 || call.top_k > MAX_TOP_K {
        bail!("top_k must be 1 through {MAX_TOP_K}");
    }
    let wanted: Vec<MapId> = match call.maps {
        None => MAPS.to_vec(),
        Some(ids) => ids
            .iter()
            .map(|id| {
                MAPS.iter()
                    .copied()
                    .find(|known| known == id)
                    .ok_or_else(|| anyhow!("unknown map {id:?}; the maps are {}", MAPS.join(", ")))
            })
            .collect::<Result<_>>()?,
    };
    let (budget, mut notes) = clamp_budget(limits, call.budget);
    let handler = build_ladder(limits, call.steer, budget.pops, &mut notes)?;
    let actions = match (&call.steer.actions, call.handle) {
        (Some(_), None) => {
            bail!("steer.actions answers a checkpoint: give the handle it came with")
        }
        (Some(value), Some(_)) => Some(
            Steer::from_json(&json!({ "actions": value }))
                .map_err(|e| anyhow!("steer.actions: {e}"))?,
        ),
        (None, _) => None,
    };
    let store = HandleStore::in_dir(&dirs.handles);
    let snapshot = match call.handle {
        Some(handle) => {
            check_handle(handle)?;
            Some(
                store
                    .get(handle)
                    .map_err(|e| anyhow!("handle {handle}: {e}"))?,
            )
        }
        None => None,
    };

    let loaded = load(call.scip_index, call.scip_root).await?;
    let mut registry = Registry::new(60.0);
    registry
        .register(Box::new(Shared(loaded.names.clone())), 1.0, 20)
        .map_err(|e| anyhow!("{e}"))?;
    registry
        .register(Box::new(Shared(loaded.scip.clone())), 1.0, 20)
        .map_err(|e| anyhow!("{e}"))?;
    for id in MAPS {
        if !wanted.contains(&id) {
            registry.set_enabled(id, false);
        }
    }
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());

    let mut seeds_found = Vec::new();
    let traversal = match snapshot {
        Some(snapshot) => Traversal::resumed(OnTheFly::new(&registry, &view), snapshot, budget),
        None => {
            let explicit = parse_seeds(call.seeds)?;
            let nodes = if explicit.is_empty() {
                if wanted.contains(&SEEDS_MAP) {
                    loaded.names.matches(call.question, SEED_COUNT)
                } else {
                    Vec::new()
                }
            } else {
                explicit
            };
            if nodes.is_empty() {
                return Ok(Report {
                    response: json!({
                        "mode": "traverse",
                        "status": "no_seeds",
                        "question": call.question,
                        "seeds": [],
                        "results": [],
                        "notes": notes_with(
                            notes,
                            "no symbol name in the question matched the SCIP graph, and no seeds were given; name a function or method the code defines",
                        ),
                    }),
                    trace: None,
                });
            }
            seeds_found = nodes.iter().map(ToString::to_string).collect();
            let seeds = nodes
                .into_iter()
                .map(|node| Seed {
                    node,
                    map: Some(SEEDS_MAP),
                })
                .collect();
            Traversal::new(OnTheFly::new(&registry, &view), seeds, budget).question(call.question)
        }
    };
    let traversal = match handler {
        Some((ladder, stops)) => traversal.handler(ladder, stops),
        None => traversal,
    };
    let traversal = match actions {
        Some(steer) => traversal.steer(steer),
        None => traversal,
    };
    let result = traversal.run().await;

    let handle = match &result.snapshot {
        Some(snapshot) => Some(
            store
                .put(snapshot)
                .context("keep the handle for a continuation")?,
        ),
        None => None,
    };
    let trace_path = match trace::write_trace(&dirs.traces, &result) {
        Ok(path) => Some(path),
        Err(error) => {
            notes.push(format!("the trace was not written: {error}"));
            None
        }
    };
    let response = response_json(
        call,
        &result,
        &seeds_found,
        handle.as_deref(),
        trace_path.as_deref(),
        notes,
    );
    Ok(Report {
        response,
        trace: trace_path,
    })
}

fn notes_with(mut notes: Vec<String>, note: &str) -> Vec<String> {
    notes.push(note.to_owned());
    notes
}

fn response_json(
    call: &Call<'_>,
    result: &TraversalResult,
    seeds: &[String],
    handle: Option<&str>,
    trace_path: Option<&Path>,
    notes: Vec<String>,
) -> Value {
    let mut results: Vec<(usize, &lci_maps::traverse::State)> = result
        .states
        .iter()
        .enumerate()
        .filter(|(_, state)| state.parent.is_some() && !result.cleared.contains(&state.node))
        .collect();
    results.sort_by(|a, b| b.1.potential.total_cmp(&a.1.potential).then(a.0.cmp(&b.0)));
    let results: Vec<Value> = results
        .into_iter()
        .take(call.top_k)
        .map(|(_, state)| {
            trace::card_json(&Card {
                node: state.node.clone(),
                via: state.via,
                witness: state.witness,
                potential: state.potential,
                depth: state.depth,
            })
        })
        .collect();
    let steering: Vec<Value> = result
        .events
        .iter()
        .filter(|event| event["ev"] == "steer")
        .map(|event| {
            json!({
                "stop": event["stop"],
                "rung": event["rung"],
                "handler": event["handler"],
                "confidence": event["confidence"],
                "actions": event["actions"],
                "outcome": event["outcome"],
            })
        })
        .collect();
    let totals = &result.totals;
    let mut response = json!({
        "mode": "traverse",
        "status": result.status.as_str(),
        "question": call.question,
        "seeds": seeds,
        "results": results,
        "totals": {
            "pops": totals.pops,
            "hops": totals.hops,
            "cross_map_hops": totals.cross_map_hops,
            "tokens": totals.tokens,
            "stops": totals.stops,
            "steers": totals.steers,
            "per_map": totals.per_map,
            "elapsed_ms": totals.elapsed_ms,
        },
        "steering": steering,
        "trace": {
            "hash": trace::trace_hash(&result.events),
            "events": result.events.len(),
            "file": trace_path.and_then(Path::file_name).map(|n| n.to_string_lossy().into_owned()),
        },
    });
    if let Some(object) = response.as_object_mut() {
        if let Some(checkpoint) = &result.checkpoint {
            object.insert("handle".into(), json!(handle));
            object.insert("checkpoint".into(), checkpoint_response(checkpoint));
        }
        if !notes.is_empty() {
            object.insert("notes".into(), json!(notes));
        }
    }
    response
}

fn checkpoint_response(checkpoint: &Checkpoint) -> Value {
    let mut value = trace::checkpoint_json(checkpoint);
    if let Some(object) = value.as_object_mut() {
        object.insert("handle".into(), json!(checkpoint.handle));
    }
    value
}

/// `search_code` with `mode = "traverse"`: the workspace's SCIP index
/// (`traverse.scip_index`, relative to the workspace), the data directory
/// for traces and handles, and the limits of `[traverse]`. An error while the
/// mode is off.
pub async fn search(
    config: &crate::config::Config,
    workspace_path: &str,
    query: &str,
    top_k: Option<usize>,
    budget: Option<&BudgetArgs>,
    steer: Option<&SteerArgs>,
    handle: Option<&str>,
) -> Result<Value> {
    let limits = &config.traverse;
    if !limits.enabled {
        bail!(
            "mode = \"traverse\" is switched off on this server; set `enabled = true` under [traverse] in its configuration"
        );
    }
    let workspace = dunce::canonicalize(workspace_path)
        .with_context(|| format!("workspace_path {workspace_path:?} is not a directory"))?;
    if !workspace.is_dir() {
        bail!("workspace_path {workspace_path:?} is not a directory");
    }
    let scip_index = workspace.join(&limits.scip_index);
    let (no_budget, no_steer) = (BudgetArgs::default(), SteerArgs::default());
    let call = Call {
        scip_index: &scip_index,
        scip_root: Some(&workspace),
        question: query,
        seeds: &[],
        top_k: top_k.unwrap_or(config.default_top_k),
        maps: None,
        budget: budget.unwrap_or(&no_budget),
        steer: steer.unwrap_or(&no_steer),
        handle,
    };
    Ok(run(limits, &Dirs::under(&config.data_dir), &call)
        .await?
        .response)
}

/// Text for the server instructions when the mode is on.
pub const INSTRUCTIONS: &str = " search_code also takes mode=\"traverse\": instead of a ranked list it walks the code graph (SCIP index: callers, callees, references) from the symbols the query names, under a budget, and returns the reached nodes with the trace totals. steer.rules is an array of {when, then} rules (prune, expand, boost, map on/off, stop, emit) applied at each stop; a stop no rule decides, or an exhausted budget, returns a handle and a checkpoint, and the next call carries the handle and steer.actions. The mode needs an index.scip in the workspace and does not use the embedding or reranker services.";

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn limits() -> TraverseConfig {
        TraverseConfig {
            enabled: true,
            ..TraverseConfig::default()
        }
    }

    #[test]
    fn a_budget_above_a_cap_is_lowered_and_named() {
        let asked = BudgetArgs {
            pops: Some(5000),
            max_depth: Some(3),
            est_tokens: None,
            deadline_ms: Some(10),
        };
        let (budget, notes) = clamp_budget(&limits(), &asked);
        assert_eq!(budget.pops, 200);
        assert_eq!(budget.max_depth, 3);
        assert_eq!(budget.est_tokens, 20_000);
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert!(notes[0].starts_with("budget.pops 5000"), "{notes:?}");
    }

    #[test]
    fn a_ladder_must_end_with_the_agent_and_cannot_use_system_one() {
        let ladder = |rungs: &[&str]| SteerArgs {
            ladder: Some(rungs.iter().map(|s| (*s).to_owned()).collect()),
            ..SteerArgs::default()
        };
        let build = |steer: &SteerArgs| build_ladder(&limits(), steer, 100, &mut Vec::new());
        assert!(build(&ladder(&["rules", "agent"])).is_ok());
        assert!(build(&ladder(&["agent"])).is_ok());
        for bad in [
            &["rules"][..],
            &["rules", "system_one", "agent"],
            &["agent", "rules"],
            &["agent", "agent"],
            &["rules", "oracle", "agent"],
        ] {
            assert!(build(&ladder(bad)).is_err(), "{bad:?}");
        }
        let script = SteerArgs {
            script: Some("1".into()),
            ..SteerArgs::default()
        };
        assert!(build(&script).is_err());
    }

    #[test]
    fn rules_make_a_handler_and_their_absence_makes_none() {
        let mut notes = Vec::new();
        assert!(
            build_ladder(&limits(), &SteerArgs::default(), 100, &mut notes)
                .unwrap()
                .is_none()
        );
        let with_rules = SteerArgs {
            rules: Some(json!([{"when": {"path_glob": "tests/**"}, "then": "prune"}])),
            max_stops: Some(99),
            ..SteerArgs::default()
        };
        let (ladder, stops) = build_ladder(&limits(), &with_rules, 100, &mut notes)
            .unwrap()
            .unwrap();
        assert_eq!(ladder.len(), 1);
        assert_eq!(stops.max_stops, 4, "capped by the configuration");
        assert_eq!(stops.every_pops, 20, "100 pops over 5 spans");
        assert_eq!(notes.len(), 1, "{notes:?}");
        let bad = SteerArgs {
            rules: Some(json!([{"when": {"nope": 1}, "then": "prune"}])),
            ..SteerArgs::default()
        };
        let error = build_ladder(&limits(), &bad, 100, &mut notes)
            .err()
            .unwrap();
        assert!(error.to_string().starts_with("steer.rules: "), "{error}");
    }
}
