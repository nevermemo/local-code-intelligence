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
//! * `hybrid+bm25`, `bm25`: with `[run_profile.seeds] bm25 = true`, the map
//!   `seeds.bm25` ranks the SCIP graph's functions and methods by BM25 over
//!   the tokens of their name, qualified path and documentation (the
//!   scorer of the ripgrep channel, over entities). Its top `seed_k` nodes
//!   that the hybrid search did not already give go after the hybrid nodes
//!   (`hybrid+bm25`), or are the start nodes when the hybrid search gave none
//!   (`bm25`, which also keeps the stand-in from running). With the key off
//!   the map is unknown to the call: naming it gives a note and no seeds,
//!   and nothing else changes.
//! * `hybrid+literal`, `hybrid+bm25+literal`, `bm25+literal`, `literal`: with
//!   `[run_profile.seeds] literal = true`, the map `seeds.literal` runs
//!   ripgrep for the names the question puts in code spans (whole word, exact
//!   case) and places each line in the SCIP graph: the symbol defined there,
//!   else the symbol around it, else (a declaration line) the file. Its nodes
//!   go after the other sources' nodes, or are the start nodes when none
//!   answered (`literal`). `[traverse] exact_seeds` is not this source. With
//!   the key off the map is unknown to the call (a note when named) and ripgrep
//!   is never started.
//! * `hybrid+repo_map`, `hybrid+bm25+repo_map`, `bm25+repo_map`, `repo_map`:
//!   with `[run_profile.seeds] repo_map = true`, the map `seeds.repo_map` runs
//!   a personalized PageRank over the SCIP graph's call and containment edges
//!   (`implements` edges left out), the random jump on the best `seeds.bm25`
//!   matches for the question, so a function that many matched entities call
//!   becomes a start node even when its own name holds none of the question's
//!   words. A question with no entity match gives no nodes. Its top `seed_k`
//!   nodes that the earlier sources did not give go after theirs, or are the
//!   start nodes when none gave any; each of the forms also exists with
//!   `+literal` appended (`hybrid+repo_map+literal`, `repo_map+literal`, ...).
//!   With the key off the map is unknown to the call: naming it gives a note
//!   and no seeds, and nothing else changes.
//! * `traits`, `traits+hybrid`, `traits+name_tokens`: with `scip.implements`
//!   on, a trait path or `Trait::method` path in a code span of the question
//!   names trait or trait-method nodes, which the walk expands to the
//!   implementing types and members ([`TraitSeedMode`] says whether they
//!   replace the other sources' start nodes or go before them). The response
//!   then carries `trait_seeds`, how many start nodes they are (0 when none
//!   is named), and each of them a `seed_hits` record with `how` `trait` or
//!   `trait_method`.
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
//! behind the search maps, `presets` the named steering policies and
//! `freshness` what `[run_profile.traverse] map_freshness` reports. This
//! file keeps the request types, the budget and ladder setup, [`run`] and
//! [`search`].

mod chunks;
mod first_stage;
mod freshness;
mod index;
mod maps;
pub mod presets;
mod repo_map;
#[cfg(test)]
mod repo_map_tests;
mod rescore;
mod response;
pub(crate) mod seeds;
#[cfg(test)]
mod structural_tests;
#[cfg(test)]
mod tests;

use crate::{
    app::App,
    config::{LspSessions, RunProfileConfig, TraverseConfig, TraverseFirstStage},
};
use anyhow::{Context, Result, anyhow, bail};
use first_stage::{
    BudgetedGit, EXACT_CLOSURE_ID, EXACT_PICKAXE_ID, ExactCard, FirstStage, closure_cards,
    pickaxe_cards, seed_cards,
};
use lci_maps::{
    MapId, SnapshotRef, SnapshotView,
    lsp_live::LSP_LIVE_ID,
    scip_impact::SCIP_IMPACT_ID,
    scip_implements::SCIP_IMPLEMENTS_ID,
    search_maps::{SEARCH_LEXICAL_ID, SEARCH_VECTOR_ID},
    traverse::{
        Grouping, Seed, Traversal, TraversalBudget,
        frontier::HopPrice,
        handle::HandleStore,
        handler::StopPolicy,
        ladder::Ladder,
        priors::HopPriors,
        rescore::FrontierRescorer,
        rules::RuleList,
        source::{Arbitration, OnTheFly},
        steer::Steer,
        tiebreak::TieBreak,
        trace::{self, PlanPolicy, StartPlan},
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

use freshness::Observer;
use index::{Loaded, impact_map, load};
pub(crate) use index::{OpenedIndex, open_config_key, open_index};
use rescore::GraphRescorer;
use response::{
    add_answer_labels, add_commits, add_file_changed, add_first_stage_only, add_maps, add_paths,
    add_policy, add_sites, lint_steer, no_seeds_response, response_json,
};
pub use seeds::TraitSeedMode;
pub use seeds::{AppSeeds, NoSearch, SeedHit, SeedSearch};
use seeds::{Seeds, add_trait_seed_count, add_trait_seeds, find_seeds};

// The names the test modules reach through `use super::*`.
#[cfg(test)]
use {
    lci_core::node::NodeId,
    lci_maps::{
        Map,
        config_key::CONFIG_KEY_MAP_ID,
        contains::CONTAINS_MAP_ID,
        docs_mentions::DOCS_MENTIONS_ID,
        live_grep::GREP_LIVE_MAP_ID,
        scip_graph::SCIP_MAP_ID,
        scip_impact::{ImpactMap, ImpactOptions},
        structural::STRUCTURAL_ID,
        tested_by::TESTED_BY_ID,
        traverse::trace::PlanMap,
    },
    response::chunk_lines,
};

pub use maps::{
    MAPS, OPT_IN_MAPS, SEEDS_BM25, SEEDS_HYBRID, SEEDS_LITERAL, SEEDS_MAP, SEEDS_REPO_MAP,
};

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
    /// Confidence a rung needs to decide a stop, 0 to 1. Only the rules
    /// rung exists (confidence 0 or 1): any value above 0 acts alike; 0
    /// means no stop escalates.
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
    /// Also stop, once, when the capture-recapture estimate of nodes found
    /// reaches this percent (1 to 100); shown as `checkpoint.likely_complete`. Counts toward
    /// `max_stops`; given alone it starts the ladder like `stop_every` does.
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
    /// The maps to use; `None` is all of [`MAPS`] (not [`OPT_IN_MAPS`]), and
    /// [`LSP_LIVE_ID`] with `[run_profile.lsp] live` for a call with a tree, a
    /// shared manager and `sessions = "shared"`, [`SEEDS_BM25`] with
    /// `[run_profile.seeds] bm25`, [`SEEDS_LITERAL`]
    /// with `[run_profile.seeds] literal` and [`SEEDS_REPO_MAP`] with
    /// `[run_profile.seeds] repo_map`.
    pub maps: Option<&'a [String]>,
    pub budget: &'a BudgetArgs,
    pub steer: &'a SteerArgs,
    /// Continue the traversal this handle names.
    pub handle: Option<&'a str>,
    /// The settings `lsp.live` starts its language servers with (server
    /// paths, `lsp_timeout_seconds`) and `rust_analyzer_trusted`, which
    /// `probe.compile` reads; `None` is the defaults.
    pub lsp: Option<&'a lci_core::config::Config>,
    /// The language-server manager the call shares when `[run_profile.lsp]
    /// sessions = "shared"` (the default): the MCP and REST server's own, one
    /// per process for `lci traverse`. Used only under `shared`, and only for
    /// a server that sees an edit on its own (`LspAdapter::watches_workspace`)
    /// or when `sync = "watch"` (the default), and lists its tree stamp's
    /// configuration files
    /// (`LspAdapter::keeps_session`); every other server starts on the call's own
    /// manager, as under `per_call`. `None` there (a caller with no manager of
    /// its own, the recipes) starts all the call's servers as under `per_call`.
    /// Under `per_call` it is never read, except for its `[run_profile.lsp]
    /// breaker` windows, which the call's own manager reads and opens under both
    /// values, and its index file list, which `[run_profile.lsp] roots =
    /// "manifest"` reads to start a server at its project's directory (see
    /// [`per_call_manager`]).
    pub lsp_sessions: Option<Arc<lci_core::lsp::Manager>>,
}

/// The language-server manager of one `lsp.live` walk under
/// `[run_profile.lsp] sessions = "per_call"` (or with no shared manager):
/// built from the call's settings, and never syncing whatever
/// `[run_profile.lsp] sync` says, since its servers start on the tree as it is
/// and stop with the walk. Under `[run_profile.lsp] breaker = true` it reads
/// and opens the skip windows of the call's shared manager (`Call.lsp_sessions`),
/// so a provider found dead by one call is left alone by the next; a call with no
/// shared manager has no windows. It also reads the index's file list of that
/// shared manager (for `roots = "manifest"`); a call with no shared manager, or
/// one with no file list, has none, and every server starts at the workspace.
pub fn per_call_manager(call: &Call<'_>) -> Arc<lci_core::lsp::Manager> {
    let defaults = lci_core::config::Config::default();
    let manager = lci_core::lsp::Manager::new(call.lsp.unwrap_or(&defaults))
        .without_sync()
        .with_breakers(call.lsp_sessions.as_ref().and_then(|m| m.breakers()));
    // The index's file list of the call's shared manager, which `[run_profile.lsp]
    // roots = "manifest"` reads to start a server at its project's directory.
    Arc::new(
        match call.lsp_sessions.as_ref().and_then(|m| m.indexed_source()) {
            Some(source) => manager.with_indexed_files(source),
            None => manager,
        },
    )
}

/// The one tree barrier of a walk that runs `lsp.live` on a shared manager,
/// taken before the seed search. The seed search runs the search channel,
/// which keeps a watching server (rust-analyzer) through a changed tree
/// ([`lci_core::lsp::Manager::search_barrier`]); `lsp.live` then passes the
/// strict barrier when its backend opens and would restart that server inside
/// the walk's deadline. Passing the strict barrier first restarts it once, up
/// front: the seed search and `lsp.live` then find the same session (started
/// by whichever asks first, after the edit) and the recorded stamp keeps both
/// barriers from restarting it again. Covers the servers the backend would
/// route to the shared manager, probed or not: with no session there is
/// nothing to stop and the stamp is recorded for the session that starts next.
async fn walk_tree_barrier(call: &Call<'_>) -> Vec<lci_core::lsp::Restart> {
    let (Some(shared), Some(root)) = (&call.lsp_sessions, call.scip_root) else {
        return Vec::new();
    };
    let Ok(path) = dunce::canonicalize(root) else {
        return Vec::new();
    };
    let workspace = lci_core::workspace::Workspace {
        id: lci_core::workspace::hash(&path.to_string_lossy()),
        path,
    };
    let synced = shared.sync_mode() == lci_core::config::LspSync::Watch;
    let routed: Vec<&Arc<dyn lci_core::lsp::adapter::LspAdapter>> = shared
        .adapters()
        .iter()
        .filter(|adapter| adapter.enabled() && adapter.keeps_session(synced))
        .collect();
    shared.tree_barrier(&workspace, &routed).await
}

/// Where a call keeps what it writes.
pub struct Dirs {
    /// `<name>.jsonl` traces.
    pub traces: PathBuf,
    /// `<handle>.json` snapshots.
    pub handles: PathBuf,
    /// The candidates each `pop` event of the trace lists, for evaluation
    /// runs (`lci traverse --trajectory`); 0 everywhere else, which writes no
    /// `pop` event. Never set over MCP or REST.
    pub trajectory_candidates: usize,
}

impl Dirs {
    /// The layout under the data directory: `traces/` (brief 1.6) and
    /// `traverse/handles/`; no trajectory events.
    pub fn under(data_dir: &Path) -> Self {
        Self {
            traces: data_dir.join("traces"),
            handles: data_dir.join("traverse").join("handles"),
            trajectory_candidates: 0,
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
    // Under off the index is not looked at either: the path as given. Cache
    // already checked the path, so a handle resumes with any spelling of it.
    let path = if matches!(
        loaded.absent,
        Some(lci_core::config::profile::ScipMode::Off)
    ) {
        call.scip_index.to_path_buf()
    } else {
        canonical(call.scip_index)
    };
    let index = format!("{}|{len}|{nanos}", path.to_string_lossy());
    format!(
        "w{}.i{}",
        trace::hash16(workspace.as_bytes()),
        trace::hash16(index.as_bytes())
    )
}

/// Runs one traversal call. Errors are the caller's mistakes (a bad rule, an
/// unknown handle, a missing index) or the data directory not being writable
/// for a handle; a failed trace write is a note, not an error.
///
/// `profile` is `[run_profile]`: its `walk` keys set how the walk groups
/// evidence (see [`Grouping`]); its `scip.mode` says whether the SCIP index
/// is required, a cache or never read.
pub async fn run(
    limits: &TraverseConfig,
    profile: &RunProfileConfig,
    dirs: &Dirs,
    call: &Call<'_>,
) -> Result<Report> {
    run_with_trait_seeds(limits, profile, dirs, call, TraitSeedMode::Auto).await
}

/// [`run`] with the mode in which the traits and trait methods the question
/// names join the start nodes (only with `scip.implements` on; see
/// [`TraitSeedMode`]).
pub async fn run_with_trait_seeds(
    limits: &TraverseConfig,
    profile: &RunProfileConfig,
    dirs: &Dirs,
    call: &Call<'_>,
    trait_seed_mode: TraitSeedMode,
) -> Result<Report> {
    if call.top_k == 0 || call.top_k > MAX_TOP_K {
        bail!("top_k must be 1 through {MAX_TOP_K}");
    }
    // The call's question type (its steering policy) sets the measured
    // first-stage and walk defaults; a call without a named policy sets none.
    let (steer, preset) = presets::resolve(call.steer)?;
    let policy = presets::policy_for(limits, preset.as_deref())?;
    let selection = maps::select(call, profile, &policy, preset.as_deref())?;
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
    notes.extend(selection.notes.iter().cloned());
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
        &selection.wanted,
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

    let loaded = load(call.scip_index, call.scip_root, profile.scip.mode).await?;
    if let Some(note) = &loaded.note {
        notes.push(note.clone());
    }
    // `[run_profile.traverse] map_freshness` (from the run profile of the call):
    // the tree against the index file (see [`freshness`]), read once per call
    // and before the maps built from the tree are built.
    // Reading the tree is setup, not walking: the deadline does not run.
    // With `[run_profile.freshness] verify_on_read` the same read also gives the
    // file sets that the snapshot lag and `file_changed` compare with. With
    // `[run_profile.freshness] stat_gate` it re-reads only changed files.
    let verify = profile.freshness.verify_on_read;
    let observer = if profile.traverse.map_freshness {
        let started = Instant::now();
        let view = match call.scip_root {
            Some(root) => {
                match freshness::tree_view(
                    call.scip_index,
                    root,
                    loaded.stamp,
                    verify,
                    profile.freshness.stat_gate,
                )
                .await
                {
                    Ok(view) => {
                        if view.as_ref().is_none_or(|view| view.lag.is_none()) {
                            notes.push(if let Some(mode) = loaded.absent {
                                format!(
                                    "map_freshness: no SCIP index is read (scip.mode = {}); the tree was not compared with it",
                                    mode.as_str()
                                )
                            } else {
                                "map_freshness: the index file has no modified time; the tree was not compared with it"
                                    .to_owned()
                            });
                        }
                        view
                    }
                    Err(error) => {
                        notes.push(format!(
                            "map_freshness: the tree could not be compared with the index file: {error:#}"
                        ));
                        None
                    }
                }
            }
            None => {
                notes.push(
                    "map_freshness: the call has no tree (give its root); the tree was not compared with the index file"
                        .to_owned(),
                );
                None
            }
        };
        budget.deadline += started.elapsed();
        let lag = view.as_ref().and_then(|view| view.lag);
        if verify {
            Observer::on(lag).verifying(view)
        } else {
            Observer::on(lag)
        }
    } else {
        Observer::off()
    };
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
    let wants = |id: MapId| selection.wants(id);

    // `lsp.live` on a shared manager: its server passes the strict tree barrier
    // once, here, before the seed search's channel can keep a watching server
    // through the same edit (see `walk_tree_barrier`). A continuation has no
    // seed search and its backend passes the barrier itself.
    let mut walk_restarts: Vec<lci_core::lsp::Restart> = Vec::new();
    if snapshot.is_none() && wants(LSP_LIVE_ID) && profile.lsp.sessions == LspSessions::Shared {
        let barrier_started = Instant::now();
        walk_restarts = walk_tree_barrier(call).await;
        // Setup, like the version probe below: credited back to the deadline.
        budget.deadline += barrier_started.elapsed();
    }

    // The start nodes (not on a continuation, which resumes the old ones).
    let mut found = Seeds::default();
    if snapshot.is_none() {
        found = find_seeds(limits, call, &loaded, &selection.wanted).await?;
        notes.append(&mut found.notes);
        // The deadline is the walk's own: a slow hybrid search (a loaded
        // embedder) must not spend it before the first pop. The search is
        // bounded by the embedding and reranker timeouts instead, and the
        // first stage's git calls below see the moved deadline.
        budget.deadline += found.search_time;
    }

    let cx = maps::Context {
        call,
        profile,
        loaded: &loaded,
        observer: &observer,
        selection: &selection,
        found: &found,
        walk_restarts: &walk_restarts,
    };
    let built = maps::build(&cx, &mut notes, &mut warnings).await?;
    // `[run_profile.scip] arbitration`: the server's answer overrules the
    // index maps this call built. A handle made under another arbitration (or
    // none) is not the walk this call would continue.
    let arbitration = maps::arbitration(&built, &selection, profile);
    if let Some(snapshot) = &snapshot {
        snapshot
            .check_arbitration(arbitration.as_ref().map(Arbitration::identity).as_deref())
            .map_err(|e| anyhow!("{e}"))?;
    }
    // `[run_profile.walk] confirm`: the same for the confirm frontier, which
    // a handle keeps with its `top_k` and the confirmer's identity.
    let confirm_identity = built.confirmer.as_ref().map(|c| c.identity());
    if let Some(snapshot) = &snapshot {
        snapshot
            .check_confirm(confirm_identity.as_deref().map(|i| (call.top_k, i)))
            .map_err(|e| anyhow!("{e}"))?;
    }
    // The trait and trait-method paths of the question join the start nodes
    // (K39a: this ran inside the `scip.implements` registration).
    if snapshot.is_none()
        && let Some(map) = &built.implements
        && let Some(note) = add_trait_seeds(&mut found, map, call.question, trait_seed_mode)
    {
        notes.push(note);
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
        for id in selection.wanted.iter().copied() {
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
    // Setup does not run the walk's deadline, but the first stage's git calls
    // do: they were bounded by the deadline and count against it.
    budget.deadline += built.setup.elapsed().saturating_sub(stage_time) + built.early;
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());

    let policy = handler.as_ref().map(|(_, _, plan)| plan.clone());
    // A continuation runs the weights and switches its handle replays.
    let running_maps = match &snapshot {
        Some(snapshot) => {
            presets::restored(&built.plan_maps, &snapshot.weights, &snapshot.switches)
        }
        None => built.plan_maps.clone(),
    };
    // The maps this call runs, in registration order, for `maps`.
    let enabled_maps: Vec<String> = running_maps
        .iter()
        .filter(|map| map.enabled)
        .map(|map| map.id.clone())
        .collect();
    let (effective, plan_effective) = presets::effective(
        preset,
        &steer,
        handler.as_ref().map(|(_, stops, plan)| (stops, plan)),
        &running_maps,
    );
    // The most a state can be corroborated by, which the score bound needs
    // (the seed maps only start the walk; maps over one graph count once).
    let expanding = built.registry.independent_expanders();
    // A handle made without `card_paths` has no edge kind for the hops of its
    // earlier calls; their path entries say `null`.
    if profile.traverse.card_paths && snapshot.as_ref().is_some_and(|s| !s.card_paths) {
        notes.push(
            "card_paths: the handle was made without it, so path kinds before this call are not kept (the earlier hops show kind null)"
                .to_owned(),
        );
    }
    // The nodes a continuation restores from its checkpoint: their cards
    // keep evidence an earlier call gathered, possibly from a kept server
    // (`add_answer_labels`).
    let restored: HashSet<String> = snapshot
        .as_ref()
        .map(|snapshot| {
            snapshot
                .states
                .iter()
                .map(|state| state.node.to_string())
                .collect()
        })
        .unwrap_or_default();
    let source = OnTheFly::new(&built.registry, &view);
    let source = match &arbitration {
        Some(a) => source.with_arbitration(a.clone()),
        None => source,
    };
    let traversal = match snapshot {
        Some(snapshot) => Traversal::resumed(source, snapshot, budget),
        None => {
            if found.nodes.is_empty() {
                let mut response = no_seeds_response(call, &found, notes);
                add_first_stage_only(&mut response, &stage, call.top_k, &exact_report);
                add_policy(&mut response, effective, warnings);
                add_trait_seed_count(&mut response, &found);
                observer.settle(call.seed_search).await;
                add_maps(
                    &mut response,
                    // No walk ran, so no arbitration did either: no roles.
                    observer.report(enabled_maps.iter().map(String::as_str), |_| None),
                );
                add_file_changed(&mut response, &observer);
                if profile.traverse.answer_labels {
                    add_answer_labels(
                        &mut response,
                        None,
                        &built.unavailable,
                        &[],
                        &HashSet::new(),
                    );
                }
                return Ok(Report {
                    response,
                    trace: None,
                });
            }
            let plan = StartPlan {
                maps: built.plan_maps,
                policy: policy.unwrap_or_default(),
                effective: Some(plan_effective),
                seed_source: Some(found.source.to_owned()),
                seed_hits: found.details.clone(),
            };
            Traversal::new(source, found.nodes.clone(), budget)
                .question(call.question)
                .plan(plan)
        }
    };
    let traversal = traversal
        .identity(identity)
        .corroboration_boost(limits.corroboration_boost)
        .fusion(limits.fusion)
        .grouping(Grouping::new(
            profile.walk.groups,
            profile.walk.grep_corroboration,
        ))
        .pass_through(limits.pass_through)
        .card_paths(profile.traverse.card_paths)
        .trajectory(dirs.trajectory_candidates)
        .score_bound(limits.score_bound)
        .priors(priors)
        .tie_break(tie_break)
        .top_k(call.top_k, expanding)
        .price(HopPrice {
            beam: limits.beam,
            pricing: profile.walk.pricing.into(),
            ..HopPrice::default()
        })
        .trait_seeds(found.trait_nodes())
        .expanders(built.registry.expanding_maps());
    let traversal = match &rescorer {
        Some(rescorer) => traversal.rerank_frontier(rescorer, limits.rerank_floor),
        None => traversal,
    };
    let traversal = match &arbitration {
        Some(a) => traversal.arbitration(a.clone()),
        None => traversal,
    };
    let traversal = match &built.confirmer {
        Some(c) => traversal.confirm_frontier(Arc::clone(c), call.top_k),
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
        built
            .search_maps
            .0
            .as_ref()
            .map(|map| (SEARCH_LEXICAL_ID, map.failures())),
        built
            .search_maps
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
    if let Some(error) = built.lsp_live.as_ref().and_then(|map| map.last_error()) {
        notes.push(format!("lsp.live reported an error for a node: {error}"));
    }
    // What the arbitration did in this call (counts are per call).
    if let (Some(a), Some(counts)) = (&arbitration, result.arbitration) {
        let judged: Vec<MapId> = a.subordinate.iter().map(|(map, _)| *map).collect();
        notes.push(format!(
            "scip.arbitration: {} judged {} (function and method destinations, places {}) in this call: {} edges confirmed, {} overruled",
            a.authority,
            judged.join(", "),
            if built.outline { "outline" } else { "scip" },
            counts.confirmed,
            counts.overruled
        ));
    }
    // What the confirm frontier did in this call (counts are per call).
    if result.confirm_top_k.is_some() {
        let totals = &result.totals;
        notes.push(format!(
            "walk.confirm: {} grep.live hops proposed, {} confirmed, {} refuted, {} unknown, {} confirmer calls (places {})",
            totals.confirm_proposals,
            totals.confirm_confirmed,
            totals.confirm_refuted,
            totals.confirm_unknown,
            totals.confirm_calls,
            if built.outline { "outline" } else { "scip" },
        ));
    }
    // A server that is not ready when the walk ends says why a node may have
    // had no edges; a ready one adds nothing, so a walk whose servers are
    // ready answers exactly as before.
    for (provider, manager, workspace_id) in &built.lsp_live_sessions {
        if let Some(readiness) = manager.readiness(provider, workspace_id).await
            && readiness.state != lci_core::lsp::ServerStateName::Ready
        {
            notes.push(format!(
                "lsp.live: {provider} {} when the walk ended (ready changes {}, progress {})",
                readiness.state.as_str(),
                readiness.ready_changes,
                readiness.progress
            ));
        }
    }
    // An answer from a server this walk found running is not a fresh
    // server's: it is exact for what its tree stamp covers, and says so.
    if let Some(walk) = &built.lsp_live_walk {
        for (provider, unchanged) in walk.reused() {
            notes.push(match unchanged {
                Some(unchanged) => format!(
                    "lsp.live: {provider} kept session; tree stamp unchanged since {} s ago",
                    unchanged.as_secs()
                ),
                None => {
                    format!("lsp.live: {provider} kept session; no tree stamp was recorded for it")
                }
            });
        }
    }
    // What the new maps cost per call, measured in this run (the p50 ring the
    // map cards report).
    let costs = [
        built
            .search_maps
            .0
            .as_ref()
            .map(|map| (SEARCH_LEXICAL_ID, map.log())),
        built
            .search_maps
            .1
            .as_ref()
            .map(|map| (SEARCH_VECTOR_ID, map.log())),
        built.impact.as_ref().map(|map| (SCIP_IMPACT_ID, map.log())),
        built
            .implements
            .as_ref()
            .map(|map| (SCIP_IMPLEMENTS_ID, map.log())),
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
    add_trait_seed_count(&mut response, &found);
    add_sites(
        &mut response,
        &loaded,
        built.impact.as_deref().or(built.site_impact.as_deref()),
        &found.details,
    );
    if profile.traverse.card_paths {
        add_paths(&mut response, &result);
    }
    if let Some(history) = &built.git_history {
        add_commits(&mut response, history);
    }
    observer.settle(call.seed_search).await;
    // The roles of an arbitration that ran in this call.
    let role_of = |id: &str| -> Option<&'static str> {
        let arbitration = arbitration.as_ref()?;
        if id == arbitration.authority {
            Some("authority")
        } else if arbitration.is_subordinate(id) {
            Some("cache")
        } else {
            None
        }
    };
    add_maps(
        &mut response,
        observer.report(enabled_maps.iter().map(String::as_str), role_of),
    );
    add_file_changed(&mut response, &observer);
    if profile.traverse.answer_labels {
        // `lsp.live`'s evidence came from a kept server when the walk
        // found one running (see `WalkSessions`): its cards are not `fresh`.
        let kept: Vec<MapId> = match &built.lsp_live_walk {
            Some(walk) if !walk.reused().is_empty() => vec![lci_maps::lsp_live::LSP_LIVE_ID],
            _ => Vec::new(),
        };
        add_answer_labels(
            &mut response,
            Some(&result),
            &built.unavailable,
            &kept,
            &restored,
        );
    }
    Ok(Report {
        response,
        trace: trace_path,
    })
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
        lsp_sessions: Some(app.language_servers()),
    };
    Ok(run(
        limits,
        &config.run_profile,
        &Dirs::under(&config.data_dir),
        &call,
    )
    .await?
    .response)
}

/// Text for the server instructions when the mode is on.
pub const INSTRUCTIONS: &str = " search_code also takes mode=\"traverse\": instead of a ranked list it walks the code graph (SCIP index: callers, callees, references) from the symbols that the hybrid search for the query finds (the search of the normal mode, then mapped to the index; a stand-in that matches symbol names takes over only if the search is unavailable or finds nothing, and seed_source says which), under a budget, and returns the start nodes (the first stage) and then the reached nodes, each card with its origin, with the trace totals. steer.rules is an array of {when, then} rules (prune, expand, boost, map on/off, stop, emit) applied at each stop; a stop no rule decides, or an exhausted budget, returns a handle and a checkpoint, and the next call carries the handle and steer.actions. The mode reads the index.scip in the workspace as a cache: when the file is missing or unreadable it walks an empty SCIP graph and says so in a note ([run_profile.scip] mode authoritative makes that an error, off never reads the file; [run_profile.scip] arbitration = true, default false, lets a language server's answer overrule index edges it does not name and puts the counts in the trace), when a language server for the workspace's languages is installed and the service keeps it the walk also asks it (lsp.live) and says in notes when it could not, and the search that finds the start nodes uses the same index and model services as the normal mode.";
