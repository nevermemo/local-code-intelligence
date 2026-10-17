//! Graph traversal for `search_code` `mode = "traverse"` and for the
//! `lci traverse` command (History: multimap spine slices S6 and S6b; spine
//! brief 1.7).
//!
//! One function, [`run`], serves both. It loads the SCIP index (map
//! `graph.scip`, cached by file stamp), finds the start nodes, builds the
//! steering ladder from the caller's rules, runs the traversal from
//! `lci-maps`, writes the trace, and answers with the reached nodes, the
//! totals, a summary of what the handlers decided and, only when the call
//! ended with work left (the ladder could not decide a stop, or the budget ran
//! out), a `handle` and the `checkpoint`.
//!
//! Start nodes (`seed_source` in the response and in the trace):
//!
//! * `given`: the caller's node ids.
//! * `hybrid`: the top `[traverse] seed_k` hits of the hybrid search that
//!   `search_code` runs in its normal mode (lexical and vector retrieval and
//!   the reranker the configured profile enables), each hit mapped to the
//!   SCIP symbols its chunk defines (or, for a chunk inside a function, the
//!   innermost function around it), else to its chunk node. Every seed
//!   records the retrievers that found the hit and its score ([`seeds::map_hits`]).
//! * `name_tokens`: the stand-in that matches the question's words against
//!   symbol names (map `seeds.scip_names`). Used only when the caller asked
//!   for that map alone, or as the visible fallback when the hybrid search is
//!   unavailable or finds nothing; the response says why (`seed_fallback`).
//!
//! The mode reads the SCIP index and, for `hybrid`, runs the search over the
//! workspace's index (which `search_code` also creates when it is missing).
//! The request can only shrink or reorder the walk: its budget is clamped to
//! the `[traverse]` caps, its rules are parsed into the closed rule AST, and
//! the only way to add pops or tokens is a `budget` action in `steer.actions`
//! on a continuation, within the engine's own limits.
//!
//! The module is split by role. `index` loads and caches the SCIP index and
//! the maps built from it; `seeds` finds the start nodes (the hybrid search
//! seam, the name-token stand-in, the mapping of hits to graph nodes);
//! `rescore` rates the reranker frontier; `response` shapes the JSON answer;
//! `first_stage` holds the answers that are not the walk, `chunks` the index
//! behind the search maps and `presets` the named steering policies. This
//! file keeps the request types, the budget and ladder setup, [`run`] and
//! [`search`].

mod chunks;
mod first_stage;
mod index;
pub mod presets;
mod rescore;
mod response;
mod seeds;
#[cfg(test)]
mod tests;

use crate::{
    app::App,
    config::{TraverseConfig, TraverseFirstStage},
};
use anyhow::{Context, Result, anyhow, bail};
use first_stage::{
    BudgetedGit, EXACT_CLOSURE_ID, EXACT_PICKAXE_ID, ExactCard, FirstStage, closure_cards,
    pickaxe_cards, seed_cards,
};
use lci_core::node::NodeId;
use lci_maps::{
    Map, MapId, Registry, SnapshotRef, SnapshotView,
    config_key::{CONFIG_KEY_MAP_ID, ConfigKeyMap, ConfigKeyOptions},
    contains::CONTAINS_MAP_ID,
    git_history::{GIT_HISTORY_ID, GitHistoryConfig, GitHistoryMap, symbol_files},
    live_grep::{GREP_LIVE_MAP_ID, LiveGrep, ScipSpans},
    lsp_live::{LSP_LIVE_ID, LspLive, ManagerBackend, ScipPlaces},
    probe_map::{PROBE_COMPILE_ID, ProbeConfig, ProbeMap},
    scip_graph::SCIP_MAP_ID,
    scip_impact::{ImpactMap, ImpactOptions, SCIP_IMPACT_ID},
    search_maps::{
        ChunkSearch, LexicalChunks, SEARCH_LEXICAL_ID, SEARCH_VECTOR_ID, ScipDecls, VectorChunks,
    },
    traverse::{
        Seed, Traversal, TraversalBudget,
        frontier::HopPrice,
        handle::HandleStore,
        handler::StopPolicy,
        ladder::Ladder,
        priors::HopPriors,
        rescore::FrontierRescorer,
        rules::RuleList,
        source::OnTheFly,
        steer::Steer,
        tiebreak::TieBreak,
        trace::{self, PlanMap, PlanPolicy, StartPlan},
    },
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime},
};

use index::{Loaded, Shared, config_key_map, impact_map, load};
pub(crate) use index::{OpenedIndex, open_config_key, open_index};
use rescore::GraphRescorer;
use response::{
    add_commits, add_first_stage_only, add_policy, add_sites, chunk_lines, lint_steer,
    no_seeds_response, response_json,
};
pub use seeds::{AppSeeds, NoSearch, SeedHit, SeedSearch};
use seeds::{SeedMap, Seeds, find_seeds};

/// The map that finds the start nodes from the question's words (the
/// stand-in, `seed_source` `name_tokens`).
pub const SEEDS_MAP: MapId = "seeds.scip_names";
/// The map of the start nodes the hybrid search found (`seed_source`
/// `hybrid`).
pub const SEEDS_HYBRID: MapId = "seeds.hybrid";
/// The maps a traversal can use, in registration order. All of them are on
/// unless the caller names the ones it wants; `structure.contains` is what
/// lets a chunk or file start node (what the hybrid search often finds) reach
/// the symbols the SCIP graph walks.
pub const MAPS: [MapId; 4] = [SEEDS_HYBRID, SEEDS_MAP, SCIP_MAP_ID, CONTAINS_MAP_ID];
/// The maps a traversal uses only when the call names them (`--maps`); all
/// need the tree and none is part of the default: `grep.live` searches its
/// current bytes, `config.key` runs the M1.2 extractor (needs `rg`),
/// `git.history` reads the git history up to its HEAD, and `probe.compile`
/// runs `cargo check` on a scratch copy (seconds per node). The three maps of
/// wave W0c: `search.lexical` (the lexical search from a node's own name),
/// `search.vector` (the chunks nearest to a node's stored embedding; one
/// counted embedding call when none is stored; needs the embedder) and
/// `scip.impact` (the declarations that reference a symbol, the reverse
/// closure of the SCIP impact index). `lsp.live` asks a language server for
/// definition, references and document symbols (starts the server at the first
/// expansion; needs its binary). The two exact operators `exact.closure` (the
/// declarations that reference the symbol the question names, from the same
/// index as `scip.impact`) and `exact.pickaxe` (the commits `git log -G/-S/--grep`
/// finds for a history question) are never walked: their cards are the first
/// stage (see [`first_stage`] and `[traverse] first_stage`).
pub const OPT_IN_MAPS: [MapId; 10] = [
    GREP_LIVE_MAP_ID,
    CONFIG_KEY_MAP_ID,
    GIT_HISTORY_ID,
    PROBE_COMPILE_ID,
    SEARCH_LEXICAL_ID,
    SEARCH_VECTOR_ID,
    SCIP_IMPACT_ID,
    LSP_LIVE_ID,
    EXACT_CLOSURE_ID,
    EXACT_PICKAXE_ID,
];
/// External calls a traversal may make; no map here makes any.
const EXTERNAL_CALLS: u32 = 100;
/// Most results a call returns.
const MAX_TOP_K: usize = 40;
/// Longest accepted handle text.
const MAX_HANDLE: usize = 64;
/// Longest one `git log` call of `exact.pickaxe` may run.
const EXACT_GIT_TIMEOUT: Duration = Duration::from_secs(20);
/// Commits `exact.pickaxe` asks for when `[traverse] first_k` is 0 (no cap).
const PICKAXE_UNCAPPED: usize = 100;

/// `budget` of a traversal request. Each value may lower the `[traverse]`
/// cap of the same name, never raise it.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BudgetArgs {
    /// Most node expansions (pops).
    pub pops: Option<u32>,
    /// Depth at which states are recorded without being expanded: a state at
    /// `max_depth` is in the results but not expanded, so the deepest state
    /// expanded is at `max_depth - 1`.
    pub max_depth: Option<u32>,
    /// Most estimated tokens the expansions may cost.
    pub est_tokens: Option<u64>,
    /// Longest the walk may run, in milliseconds. The hybrid search for the
    /// start nodes does not count against it.
    pub deadline_ms: Option<u64>,
}

/// `steer` of a traversal request: the rules the ladder applies at each
/// stop, how many stops there may be, and, on a continuation, the outer
/// agent's answer to a returned checkpoint.
#[derive(Debug, Clone, Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SteerArgs {
    /// A named policy, the question type: `explore` (what a call with no
    /// policy runs), `impact`, `flow`, `where-to` or `history`. Its settings
    /// apply: its steering, and the measured first-stage and walk defaults of
    /// the question type (`impact` and `history` answer from the first stage
    /// alone; `where-to` adds the walk from 20 answers); the fields given
    /// beside it replace the preset's. The response's `effective_policy` shows
    /// what ran.
    pub policy: Option<String>,
    /// Rung 1 of the ladder: an array of `{"when": predicate, "then": action}`
    /// rules (at most 64). Predicates: `kind`, `path_glob`, `witness`, `map`,
    /// `score`, `fanout`, `depth`, `budget_left`, `seen_before`, `all`, `any`,
    /// `not`. Actions: `prune`, `expand`, `continue`, `stop`, `{"boost": f}`,
    /// `{"map_on": id}`, `{"map_off": id}`, `{"map_weight": {"id", "weight"}}`,
    /// `{"redirect": [node ids]}`, `{"emit": note}`. They can only shrink, reorder or redirect the walk.
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
    /// Also stop when this many new disagreements have been recorded since
    /// the last stop (a node a fuzzy map offered that a structural map, which
    /// answered the same expansion, did not). At least 1.
    /// Counts toward `max_stops`; given alone it starts the ladder like
    /// `stop_every` does, with the periodic stops at their default spacing.
    pub on_disagreement: Option<u32>,
    /// Also stop when the walk is estimated to have found this percent of
    /// what it will find (1 to 100). Counts toward `max_stops`; given alone it
    /// starts the ladder like `stop_every` does.
    pub likely_complete: Option<u32>,
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
    /// The hybrid search that finds the start nodes from the question; `None`
    /// when the call has no workspace to search (the stand-in is used and
    /// says so).
    pub seed_search: Option<&'a dyn SeedSearch>,
    /// Results to return, 1 through 40.
    pub top_k: usize,
    /// The maps to use; `None` is all of [`MAPS`] (not [`OPT_IN_MAPS`]).
    pub maps: Option<&'a [String]>,
    pub budget: &'a BudgetArgs,
    pub steer: &'a SteerArgs,
    /// Continue the traversal this handle names.
    pub handle: Option<&'a str>,
    /// The settings `lsp.live` starts its language servers with (server
    /// paths, `lsp_timeout_seconds`); `None` is the defaults.
    pub lsp: Option<&'a lci_core::config::Config>,
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

/// The handler a call runs under, if it has one, with the stop policy and
/// what the trace records of the policy. The stop policy is only meaningful
/// with a handler.
fn build_ladder(
    limits: &TraverseConfig,
    steer: &SteerArgs,
    pops: u32,
    notes: &mut Vec<String>,
) -> Result<Option<(Ladder, StopPolicy, PlanPolicy)>> {
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
    if matches!(steer.on_disagreement, Some(0)) {
        bail!("steer.on_disagreement must be at least 1");
    }
    if matches!(steer.likely_complete, Some(0) | Some(101..)) {
        bail!("steer.likely_complete is a percent from 1 to 100");
    }
    let triggered = steer.stop_every.is_some()
        || steer.on_disagreement.is_some()
        || steer.likely_complete.is_some();
    if steer.rules.is_none() && !triggered {
        if steer.max_stops.is_some() || steer.tau.is_some() {
            notes.push(
                "steer.max_stops and steer.tau do nothing without steer.rules or a stop trigger                  (steer.stop_every, steer.on_disagreement, steer.likely_complete)"
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
    let plan = PlanPolicy {
        ladder: steer
            .ladder
            .clone()
            .unwrap_or_else(|| vec!["rules".to_owned(), "agent".to_owned()]),
        tau: Some(ladder.tau()),
        rules: steer
            .rules
            .as_ref()
            .and_then(Value::as_array)
            .map_or(0, |rules| rules.len() as u32),
    };
    let mut policy = StopPolicy::every(every).max_stops(max_stops);
    if let Some(n) = steer.on_disagreement {
        policy = policy.on_disagreement(n);
    }
    if let Some(percent) = steer.likely_complete {
        policy = policy.likely_complete(percent);
    }
    Ok(Some((ladder, policy, plan)))
}

fn check_handle(handle: &str) -> Result<()> {
    if handle.is_empty() || handle.len() > MAX_HANDLE {
        bail!("handle is not a handle this server issued");
    }
    Ok(())
}

/// What a handle is bound to: the workspace (the canonical tree root, if the
/// call has one) and the index (its canonical path, size and modification
/// time), each as a short hash so a handle names no path. A handle made for
/// other nodes than the index holds now would resume on the wrong graph.
fn identity_of(call: &Call<'_>, loaded: &Loaded) -> String {
    let canonical =
        |path: &Path| std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let workspace = call.scip_root.map_or_else(
        || "no tree".to_owned(),
        |root| canonical(root).to_string_lossy().into_owned(),
    );
    let (modified, len) = loaded.stamp;
    let nanos = modified
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_nanos());
    let index = format!(
        "{}|{len}|{nanos}",
        canonical(call.scip_index).to_string_lossy()
    );
    format!(
        "w{}.i{}",
        trace::hash16(workspace.as_bytes()),
        trace::hash16(index.as_bytes())
    )
}

/// Runs one traversal call. Errors are the caller's mistakes (a bad rule, an
/// unknown handle, a missing index) or the data directory not being writable
/// for a handle; a failed trace write is a note, not an error.
pub async fn run(limits: &TraverseConfig, dirs: &Dirs, call: &Call<'_>) -> Result<Report> {
    if call.top_k == 0 || call.top_k > MAX_TOP_K {
        bail!("top_k must be 1 through {MAX_TOP_K}");
    }
    let known_map = |id: &str| {
        MAPS.iter()
            .chain(&OPT_IN_MAPS)
            .copied()
            .find(|known| *known == id)
            .ok_or_else(|| {
                anyhow!(
                    "unknown map {id:?}; the maps are {}, and, only when named, {}",
                    MAPS.join(", "),
                    OPT_IN_MAPS.join(", ")
                )
            })
    };
    // The call's question type (its steering policy) sets the measured
    // first-stage and walk defaults; a call without a named policy sets none.
    let (steer, preset) = presets::resolve(call.steer)?;
    let policy = presets::policy_for(limits, preset.as_deref())?;
    // Extra maps named by the policy that this call cannot run (no tree).
    let mut skipped_maps: Vec<String> = Vec::new();
    let wanted: Vec<MapId> = match call.maps {
        None => {
            let mut ids = MAPS.to_vec();
            // The policy's extra maps read the tree.
            if call.scip_root.is_some() {
                for name in policy.extra_maps.iter().flatten() {
                    let id = known_map(name).map_err(|e| {
                        anyhow!(
                            "policy {}: extra_maps: {e}",
                            preset.as_deref().unwrap_or_default()
                        )
                    })?;
                    if !ids.contains(&id) {
                        ids.push(id);
                    }
                }
            } else {
                skipped_maps = policy.extra_maps.iter().flatten().cloned().collect();
            }
            ids
        }
        Some(ids) => ids.iter().map(|id| known_map(id)).collect::<Result<_>>()?,
    };
    // The policy's first_k and seed_k take the place of the [traverse] keys.
    let tuned;
    let limits = if policy.first_k.is_some() || policy.seed_k.is_some() {
        tuned = TraverseConfig {
            first_k: policy.first_k.unwrap_or(limits.first_k),
            seed_k: policy.seed_k.unwrap_or(limits.seed_k),
            ..limits.clone()
        };
        &tuned
    } else {
        limits
    };
    if wanted.iter().any(|id| OPT_IN_MAPS.contains(id)) && call.scip_root.is_none() {
        bail!("the maps named beyond the defaults read the tree: give its root");
    }
    let priors = match (&limits.priors, &limits.question_type) {
        (None, None) => HopPriors::default(),
        (Some(path), Some(question_type)) => HopPriors::open(path, question_type)
            .map_err(|e| anyhow!("[traverse] priors {path}: {e}"))?,
        _ => bail!("[traverse] priors and question_type go together"),
    };
    let tie_break = (limits.tie_break.as_deref())
        .map(|path| TieBreak::open(path).map_err(|e| anyhow!("[traverse] tie_break {path}: {e}")))
        .transpose()?;
    let (mut budget, mut warnings) = clamp_budget(limits, call.budget);
    let mut notes = Vec::new();
    if !skipped_maps.is_empty() {
        notes.push(format!(
            "policy {}: the maps {} read the tree and were left out: give --root (or the workspace) to run them as measured",
            preset.as_deref().unwrap_or_default(),
            skipped_maps.join(", ")
        ));
    }
    // First stage alone: the policy says so, or the call asks for fewer
    // answers (first stage plus walk) than the policy's walk is worth. Only
    // a first call with a first stage: a continuation is the walk, and
    // without a first stage nothing else would answer.
    let walk_off = call.handle.is_none()
        && {
            let asked = match limits.first_k {
                0 => usize::MAX,
                k => (k as usize).saturating_add(call.top_k),
            };
            let wanted_off = policy.walk == Some(false)
                || (asked as u64) < u64::from(policy.walk_min_answers.unwrap_or(0));
            if wanted_off && limits.first_stage == TraverseFirstStage::Off {
                notes.push(format!(
                "policy {}: the first stage alone would answer, but [traverse] first_stage is off: the walk runs",
                preset.as_deref().unwrap_or_default()
            ));
            }
            wanted_off && limits.first_stage != TraverseFirstStage::Off
        };
    if walk_off {
        budget.pops = 1;
        notes.push(format!(
            "policy {}: the first stage alone answers (measured: the walk does not pay here at this many answers); the walk took one pop and its cards are not returned, continue the handle to walk on",
            preset.as_deref().unwrap_or_default()
        ));
    }
    let handler = build_ladder(limits, &steer, budget.pops, &mut warnings)?;
    let actions = match (&steer.actions, call.handle) {
        (Some(_), None) => {
            bail!("steer.actions answers a checkpoint: give the handle it came with")
        }
        (Some(value), Some(_)) => Some(
            Steer::from_json(&json!({ "actions": value }))
                .map_err(|e| anyhow!("steer.actions: {e}"))?,
        ),
        (None, _) => None,
    };
    warnings.extend(lint_steer(
        &steer,
        handler.as_ref().map(|(_, stops, _)| stops),
        actions.as_ref(),
        &budget,
        &wanted,
    ));
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
    // A handle resumes on the workspace and index it was made for.
    let identity = identity_of(call, &loaded);
    let make_rescorer = |setting: &str| match call
        .seed_search
        .and_then(|search| search.reranker().map(|models| (search, models)))
    {
        Some((search, models)) => Ok(GraphRescorer::new(
            models,
            loaded.scip.clone(),
            search.reranker_identity(),
            limits.rerank_scores,
        )),
        None => Err(anyhow!(
            "[traverse] {setting} needs the reranker: the call needs the application (give --root with a question or seeds)"
        )),
    };
    let rescorer = if limits.rerank_frontier {
        Some(make_rescorer("rerank_frontier")?)
    } else {
        None
    };
    if let Some(snapshot) = &snapshot {
        snapshot
            .check_identity(&identity)
            .map_err(|e| anyhow!("{e}"))?;
        snapshot.check_priors(&priors).map_err(|e| anyhow!("{e}"))?;
        snapshot
            .check_tie_break(tie_break.as_ref())
            .map_err(|e| anyhow!("{e}"))?;
        let reranker = rescorer.as_ref().map(|rescorer| rescorer.identity());
        snapshot
            .check_rerank(
                reranker
                    .as_deref()
                    .map(|identity| (limits.rerank_floor, identity)),
            )
            .map_err(|e| anyhow!("{e}"))?;
        // What a resume must repeat for the walk to be the same one.
        snapshot
            .check_boost(limits.corroboration_boost)
            .map_err(|e| anyhow!("{e}"))?;
        snapshot
            .check_top_k(call.top_k)
            .map_err(|e| anyhow!("{e}"))?;
    }
    let wants = |id: MapId| wanted.contains(&id);

    // The start nodes (not on a continuation, which resumes the old ones).
    let mut found = Seeds::default();
    if snapshot.is_none() {
        found = find_seeds(limits, call, &loaded, &wanted).await?;
        notes.append(&mut found.notes);
        // The deadline is the walk's own: a slow hybrid search (a loaded
        // embedder) must not spend it before the first pop. The search is
        // bounded by the embedding and reranker timeouts instead, and the
        // first stage's git calls below see the moved deadline.
        budget.deadline += found.search_time;
    }

    let mut registry = Registry::new(60.0);
    let mut plan_maps = Vec::new();
    register(
        &mut registry,
        &mut plan_maps,
        Box::new(SeedMap::hybrid()),
        wants(SEEDS_HYBRID),
    )?;
    register(
        &mut registry,
        &mut plan_maps,
        Box::new(SeedMap::name_tokens()),
        wants(SEEDS_MAP),
    )?;
    register(
        &mut registry,
        &mut plan_maps,
        Box::new(Shared(loaded.scip.clone())),
        wants(SCIP_MAP_ID),
    )?;
    // The chunks the search maps reach have no lines in their ids: with a
    // search map on, every chunk of the index is given to structure.contains
    // so that it can expand them.
    let early = Instant::now();
    let mut lines = chunk_lines(&found);
    let mut search_index: Option<Arc<dyn ChunkSearch + '_>> = None;
    if wants(SEARCH_LEXICAL_ID) || wants(SEARCH_VECTOR_ID) {
        match call.seed_search.and_then(|search| search.chunk_search()) {
            None => notes.push(
                "search.lexical and search.vector need the workspace index: give --root and a question or seeds, with the application available".to_owned(),
            ),
            Some(index) => {
                match index.chunk_lines().await {
                    Ok(all) => {
                        let known: HashSet<NodeId> =
                            lines.iter().map(|(node, _)| node.clone()).collect();
                        lines.extend(all.into_iter().filter(|(node, _)| !known.contains(node)));
                    }
                    Err(error) => notes.push(format!(
                        "the search maps found no chunk lines (structure.contains cannot expand the chunks they reach): {error}"
                    )),
                }
                search_index = Some(index);
            }
        }
    }
    let early = early.elapsed();
    register(
        &mut registry,
        &mut plan_maps,
        Box::new(loaded.contains.with_chunk_lines(lines)),
        wants(CONTAINS_MAP_ID),
    )?;
    // Building the tree maps is setup, not walking: the deadline does not run
    // while it happens.
    let setup = Instant::now();
    if let (true, Some(root)) = (wants(GREP_LIVE_MAP_ID), call.scip_root) {
        let grep = LiveGrep::new(root, Some(Arc::new(ScipSpans::from_scip(&loaded.scip))));
        // Measure the map's cost before the walk prices a hop by it.
        grep.calibrate(3).await;
        register(&mut registry, &mut plan_maps, Box::new(grep), true)?;
    }
    if let (true, Some(root)) = (wants(CONFIG_KEY_MAP_ID), call.scip_root) {
        let map = match config_key_map(call.scip_index, root, &loaded).await {
            Ok(map) => {
                let stats = map.stats();
                notes.push(format!(
                    "config.key: {} keys, {} exact and {} text edges (text edges off), built in {:.0} ms",
                    stats.keys, stats.exact, stats.text, stats.build_ms
                ));
                map
            }
            Err(error) => {
                notes.push(format!("config.key has no edges: {error:#}"));
                Arc::new(ConfigKeyMap::empty(ConfigKeyOptions::default()))
            }
        };
        register(&mut registry, &mut plan_maps, Box::new(Shared(map)), true)?;
    }
    let mut git_history = None;
    if wants(GIT_HISTORY_ID) {
        match call.scip_root {
            None => notes.push("git.history needs the tree: give --root".to_owned()),
            Some(root) => {
                let table = symbol_files(&loaded.scip);
                match GitHistoryMap::open_with(root, GitHistoryConfig::default(), table) {
                    Ok(map) => {
                        let map = Arc::new(map);
                        register(
                            &mut registry,
                            &mut plan_maps,
                            Box::new(Shared(map.clone())),
                            true,
                        )?;
                        git_history = Some(map);
                    }
                    Err(error) => notes.push(format!("git.history is unavailable: {error}")),
                }
            }
        }
    }
    if wants(PROBE_COMPILE_ID) {
        match call.scip_root {
            None => notes.push("probe.compile needs the tree: give --root".to_owned()),
            Some(root) => {
                let map = ProbeMap::new(loaded.scip.clone(), ProbeConfig::for_tree(root));
                register(
                    &mut registry,
                    &mut plan_maps,
                    Box::new(Shared(Arc::new(map))),
                    true,
                )?;
            }
        }
    }
    let mut lsp_live = None;
    if let (true, Some(root)) = (wants(LSP_LIVE_ID), call.scip_root) {
        // The servers of the call's settings; a server starts at the first
        // expansion and stops when the map drops.
        let workspace = dunce::canonicalize(root)
            .map(|path| lci_core::workspace::Workspace {
                id: lci_core::workspace::hash(&path.to_string_lossy()),
                path,
            })
            .map_err(|error| error.to_string());
        let backend = match workspace {
            Ok(workspace) => {
                let defaults = lci_core::config::Config::default();
                ManagerBackend::open(workspace, call.lsp.unwrap_or(&defaults)).await
            }
            Err(error) => Err(error),
        };
        match backend {
            Ok(backend) => {
                let places = Arc::new(ScipPlaces::from_scip(&loaded.scip));
                let map = Arc::new(LspLive::new(Box::new(backend), places));
                register(
                    &mut registry,
                    &mut plan_maps,
                    Box::new(Shared(map.clone())),
                    true,
                )?;
                lsp_live = Some(map);
            }
            Err(error) => notes.push(format!("lsp.live is unavailable: {error}")),
        }
    }
    let mut impact = None;
    if wants(SCIP_IMPACT_ID) {
        match call.scip_root {
            None => notes.push("scip.impact needs the tree: give --root".to_owned()),
            Some(root) => {
                let map = match impact_map(call.scip_index, root, &loaded).await {
                    Ok(map) => {
                        let stats = map.stats();
                        notes.push(format!(
                            "scip.impact: {} definitions, {} with a node id ({} those of graph.scip), cap {} users per node",
                            stats.defs,
                            stats.nodes,
                            stats.resolved,
                            map.options().cap
                        ));
                        map
                    }
                    Err(error) => {
                        notes.push(format!("scip.impact has no edges: {error:#}"));
                        Arc::new(ImpactMap::empty(ImpactOptions::default()))
                    }
                };
                impact = Some(map.clone());
                register(&mut registry, &mut plan_maps, Box::new(Shared(map)), true)?;
            }
        }
    }
    // The exact operators are descriptor-only maps; their cards are the first
    // stage, made once per call and not on a continuation.
    if wants(EXACT_CLOSURE_ID) {
        register(
            &mut registry,
            &mut plan_maps,
            Box::new(SeedMap::exact_closure()),
            true,
        )?;
    }
    if wants(EXACT_PICKAXE_ID) {
        register(
            &mut registry,
            &mut plan_maps,
            Box::new(SeedMap::exact_pickaxe()),
            true,
        )?;
    }
    let exact_wanted = wants(EXACT_CLOSURE_ID) || wants(EXACT_PICKAXE_ID);
    let mut stage = FirstStage::off();
    let mut exact_report: Vec<Value> = Vec::new();
    let mut stage_time = Duration::ZERO;
    if snapshot.is_some() {
        if exact_wanted || limits.first_stage != TraverseFirstStage::Off || limits.exact_seeds {
            notes.push(
                "the first stage is not run again on a continuation: the results are the walk's alone (the first response carried the first-stage list)"
                    .to_owned(),
            );
        }
    } else if let Some(root) = call.scip_root {
        let mut exact: Vec<ExactCard> = Vec::new();
        for id in wanted.iter().copied() {
            if id == EXACT_CLOSURE_ID {
                match impact_map(call.scip_index, root, &loaded).await {
                    Ok(map) => {
                        let scip = loaded.scip.clone();
                        let in_graph = |symbol: &str| scip.graph().node_index(symbol).is_some();
                        let found = closure_cards(&map, &in_graph, call.question);
                        notes.push(found.note);
                        exact_report.push(json!({
                            "map": id,
                            "answered": !found.cards.is_empty(),
                            "cards": found.cards.len(),
                        }));
                        exact.extend(found.cards);
                    }
                    Err(error) => {
                        notes.push(format!("{id}: no index to read: {error:#}"));
                        exact_report.push(json!({"map": id, "answered": false, "cards": 0}));
                    }
                }
            } else if id == EXACT_PICKAXE_ID {
                let (tree, question) = (root.to_path_buf(), call.question.to_owned());
                let k = match limits.first_k {
                    0 => PICKAXE_UNCAPPED,
                    k => k as usize,
                };
                // The git calls share what is left of the call's deadline
                // (the walk's deadline is not extended by them, see below).
                let git = BudgetedGit {
                    git: PathBuf::from("git"),
                    tree,
                    per_call: EXACT_GIT_TIMEOUT,
                    end: budget.deadline,
                };
                let started = Instant::now();
                let found = tokio::task::spawn_blocking(move || pickaxe_cards(&git, &question, k))
                    .await
                    .context("run exact.pickaxe")?;
                stage_time += started.elapsed();
                notes.push(found.note);
                exact_report.push(json!({
                    "map": id,
                    "answered": !found.cards.is_empty(),
                    "cards": found.cards.len(),
                    "git_calls": found.git_calls,
                }));
                stage.git_calls += found.git_calls;
                exact.extend(found.cards);
            }
        }
        if limits.first_stage != TraverseFirstStage::Off {
            stage.mode = limits.first_stage;
            stage.walk_off = walk_off;
            stage.first_k = limits.first_k as usize;
            stage.cards = exact.iter().map(|card| card.card.clone()).collect();
            if limits.answer_seeds {
                stage.cards.extend(seed_cards(&found.nodes, &found.details));
            }
        }
        if limits.exact_seeds {
            // After the others, and only nodes the graph holds.
            let aligned = found.details.len() == found.nodes.len();
            for card in exact.iter().filter(|card| card.in_graph) {
                if found.nodes.iter().any(|seed| seed.node == card.node) {
                    continue;
                }
                found.nodes.push(Seed {
                    node: card.node.clone(),
                    map: Some(card.op),
                });
                if aligned {
                    found.details.push(json!({
                        "node": card.node.to_string(),
                        "kind": "symbol",
                        "how": "exact",
                        "retrievers": [card.op],
                        "score": card.held,
                        "score_kind": "references_held",
                    }));
                }
            }
            if matches!(found.source, "" | "none") && !found.nodes.is_empty() {
                found.source = "exact";
            }
        }
    }
    let mut search_maps: (
        Option<Arc<LexicalChunks<'_>>>,
        Option<Arc<VectorChunks<'_>>>,
    ) = (None, None);
    if let Some(index) = &search_index {
        let decls: Arc<dyn lci_maps::search_maps::DeclLocator> =
            Arc::new(ScipDecls::from_scip(&loaded.scip));
        if wants(SEARCH_LEXICAL_ID) {
            let map = Arc::new(LexicalChunks::new(index.clone(), Some(decls.clone())));
            register(
                &mut registry,
                &mut plan_maps,
                Box::new(Shared(map.clone())),
                true,
            )?;
            search_maps.0 = Some(map);
        }
        if wants(SEARCH_VECTOR_ID) {
            let map = Arc::new(VectorChunks::new(index.clone(), Some(decls)));
            register(
                &mut registry,
                &mut plan_maps,
                Box::new(Shared(map.clone())),
                true,
            )?;
            search_maps.1 = Some(map);
        }
    }
    // Setup does not run the walk's deadline, but the first stage's git calls
    // do: they were bounded by the deadline and count against it.
    budget.deadline += setup.elapsed().saturating_sub(stage_time) + early;
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());

    let policy = handler.as_ref().map(|(_, _, plan)| plan.clone());
    // A continuation runs the weights and switches its handle replays.
    let running_maps = match &snapshot {
        Some(snapshot) => presets::restored(&plan_maps, &snapshot.weights, &snapshot.switches),
        None => plan_maps.clone(),
    };
    let (effective, plan_effective) = presets::effective(
        preset,
        &steer,
        handler.as_ref().map(|(_, stops, plan)| (stops, plan)),
        &running_maps,
    );
    // The most a state can be corroborated by, which the score bound needs
    // (the seed maps only start the walk; maps over one graph count once).
    let expanding = registry.independent_expanders();
    let traversal = match snapshot {
        Some(snapshot) => Traversal::resumed(OnTheFly::new(&registry, &view), snapshot, budget),
        None => {
            if found.nodes.is_empty() {
                let mut response = no_seeds_response(call, &found, notes);
                add_first_stage_only(&mut response, &stage, call.top_k, &exact_report);
                add_policy(&mut response, effective, warnings);
                return Ok(Report {
                    response,
                    trace: None,
                });
            }
            let plan = StartPlan {
                maps: plan_maps,
                policy: policy.unwrap_or_default(),
                effective: Some(plan_effective),
                seed_source: Some(found.source.to_owned()),
                seed_hits: found.details.clone(),
            };
            Traversal::new(OnTheFly::new(&registry, &view), found.nodes.clone(), budget)
                .question(call.question)
                .plan(plan)
        }
    };
    let traversal = traversal
        .identity(identity)
        .corroboration_boost(limits.corroboration_boost)
        .fusion(limits.fusion)
        .pass_through(limits.pass_through)
        .score_bound(limits.score_bound)
        .priors(priors)
        .tie_break(tie_break)
        .top_k(call.top_k, expanding)
        .price(HopPrice {
            beam: limits.beam,
            ..HopPrice::default()
        })
        .expanders(registry.expanding_maps());
    let traversal = match &rescorer {
        Some(rescorer) => traversal.rerank_frontier(rescorer, limits.rerank_floor),
        None => traversal,
    };
    // The cap on model calls covers the reranker frontier's calls.
    let traversal = if rescorer.is_some() {
        traversal.model_calls(limits.max_model_calls)
    } else {
        traversal
    };
    let traversal = match handler {
        Some((ladder, stops, _)) => traversal.handler(ladder, stops),
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
    let failures = [
        search_maps
            .0
            .as_ref()
            .map(|map| (SEARCH_LEXICAL_ID, map.failures())),
        search_maps
            .1
            .as_ref()
            .map(|map| (SEARCH_VECTOR_ID, map.failures())),
    ];
    for (id, failures) in failures.into_iter().flatten() {
        if let Some(first) = failures.first() {
            notes.push(format!(
                "{id}: {} calls failed and gave no edges (fail-open); the first: {first}",
                failures.count()
            ));
        }
    }
    // The engine's warnings: reranker failures by reason, the model_calls cap,
    // a boost or switches the handle did not take.
    warnings.extend(result.warnings.iter().cloned());
    if let Some(rescorer) = &rescorer {
        let errors = rescorer.errors();
        if let Some(first) = errors.first() {
            notes.push(format!(
                "rerank_frontier: the first of the reranker calls that failed in this call: {first}"
            ));
        }
        if rescorer.clamped() > 0 {
            warnings.push(format!(
                "rerank_frontier: {} reranker scores lay outside [0, 1] and were clamped; [traverse] rerank_scores is \"{}\"",
                rescorer.clamped(),
                limits.rerank_scores.as_str()
            ));
        }
    }
    if let Some(error) = lsp_live.as_ref().and_then(|map| map.last_error()) {
        notes.push(format!("lsp.live gave no edges for a node: {error}"));
    }
    // What the new maps cost per call, measured in this run (the p50 ring the
    // map cards report).
    let costs = [
        search_maps
            .0
            .as_ref()
            .map(|map| (SEARCH_LEXICAL_ID, map.log())),
        search_maps
            .1
            .as_ref()
            .map(|map| (SEARCH_VECTOR_ID, map.log())),
        impact.as_ref().map(|map| (SCIP_IMPACT_ID, map.log())),
    ];
    for (id, log) in costs.into_iter().flatten() {
        if let Some(p50) = log.p50() {
            notes.push(format!(
                "{id}: expand p50 {p50:.1} ms over {} timed calls",
                log.len()
            ));
        }
    }
    let mut response = response_json(
        call,
        &result,
        &found,
        handle.as_deref(),
        trace_path.as_deref(),
        notes,
        &stage,
    );
    if let (false, Some(object)) = (exact_report.is_empty(), response.as_object_mut()) {
        object.insert("exact".into(), json!(exact_report));
    }
    add_policy(&mut response, effective, warnings);
    add_sites(&mut response, &loaded, impact.as_deref(), &found.details);
    if let Some(history) = &git_history {
        add_commits(&mut response, history);
    }
    Ok(Report {
        response,
        trace: trace_path,
    })
}

/// Adds a map to the registry (weight 1, as every map here) and to the
/// plan the trace records, switched off when the call did not ask for it.
fn register<'a>(
    registry: &mut Registry<'a>,
    plan: &mut Vec<PlanMap>,
    map: Box<dyn Map + 'a>,
    enabled: bool,
) -> Result<()> {
    let descriptor = map.describe();
    plan.push(PlanMap {
        id: descriptor.id.to_owned(),
        version: descriptor.version,
        weight: 1.0,
        enabled,
    });
    let id = descriptor.id;
    registry
        .register(map, 1.0, 20)
        .map_err(|e| anyhow!("{e}"))?;
    if !enabled {
        registry.set_enabled(id, false);
    }
    Ok(())
}

/// `search_code` with `mode = "traverse"`: the workspace's SCIP index
/// (`traverse.scip_index`, relative to the workspace), the data directory
/// for traces and handles, the limits of `[traverse]` and, for the start
/// nodes, the hybrid search of `app` over the workspace with the retrievers
/// of `profile` (the endpoint's `[search_profiles]` entry, if any). An error
/// while the mode is off.
#[allow(clippy::too_many_arguments)]
pub async fn search(
    app: &App,
    workspace_path: &str,
    query: &str,
    top_k: Option<usize>,
    budget: Option<&BudgetArgs>,
    steer: Option<&SteerArgs>,
    handle: Option<&str>,
    profile: Option<&str>,
) -> Result<Value> {
    let config = &app.config;
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
    let seed_search = AppSeeds {
        app,
        workspace: &workspace,
        profile,
    };
    let call = Call {
        scip_index: &scip_index,
        scip_root: Some(&workspace),
        question: query,
        seeds: &[],
        seed_search: Some(&seed_search),
        top_k: top_k.unwrap_or(config.default_top_k),
        maps: None,
        budget: budget.unwrap_or(&no_budget),
        steer: steer.unwrap_or(&no_steer),
        handle,
        lsp: Some(config),
    };
    Ok(run(limits, &Dirs::under(&config.data_dir), &call)
        .await?
        .response)
}

/// Text for the server instructions when the mode is on.
pub const INSTRUCTIONS: &str = " search_code also takes mode=\"traverse\": instead of a ranked list it walks the code graph (SCIP index: callers, callees, references) from the symbols that the hybrid search for the query finds (the search of the normal mode, then mapped to the index; a stand-in that matches symbol names takes over only if the search is unavailable or finds nothing, and seed_source says which), under a budget, and returns the start nodes (the first stage) and then the reached nodes, each card with its origin, with the trace totals. steer.rules is an array of {when, then} rules (prune, expand, boost, map on/off, stop, emit) applied at each stop; a stop no rule decides, or an exhausted budget, returns a handle and a checkpoint, and the next call carries the handle and steer.actions. The mode needs an index.scip in the workspace, and the search that finds the start nodes uses the same index and model services as the normal mode.";
