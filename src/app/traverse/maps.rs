//! The map registry of a traversal: the map ids and the lists of the maps a
//! call can use, and the helpers the map registration shares (K39a; History:
//! the registry of `run_with_trait_seeds` in `mod.rs`).

use super::{
    Call,
    first_stage::{EXACT_CLOSURE_ID, EXACT_PICKAXE_ID},
    freshness::Observer,
    index::{Loaded, Shared, config_key_map, impact_map, implements_map},
    per_call_manager,
    response::chunk_lines,
    seeds::{SeedMap, Seeds},
};
use crate::config::{LspPlaces, LspSessions, RunProfileConfig};
use anyhow::{Result, anyhow, bail};
use lci_core::{
    config::{TraversePolicy, profile::ScipMode},
    lexical,
    node::{NodeId, SymbolKind},
};
use lci_maps::{
    Map, MapId, Registry,
    config_key::{CONFIG_KEY_MAP_ID, ConfigKeyMap, ConfigKeyOptions},
    contains::CONTAINS_MAP_ID,
    docs_mentions::{DOCS_MENTIONS_ID, DocsMentions},
    git_history::{GIT_HISTORY_ID, GitHistoryConfig, GitHistoryMap, symbol_files},
    live_grep::{GREP_LIVE_MAP_ID, LiveGrep, ScipSpans},
    lsp_live::{LSP_LIVE_ID, LspLive, ManagerBackend, OutlinePlaces, ScipPlaces, SymbolPlaces},
    probe_map::{PROBE_COMPILE_ID, ProbeConfig, ProbeMap},
    scip_graph::SCIP_MAP_ID,
    scip_impact::{ImpactMap, ImpactOptions, SCIP_IMPACT_ID},
    scip_implements::{ImplementsMap, ImplementsOptions, SCIP_IMPLEMENTS_ID},
    search_maps::{
        ChunkSearch, LexicalChunks, SEARCH_LEXICAL_ID, SEARCH_VECTOR_ID, ScipDecls, VectorChunks,
    },
    structural::{Ripgrep, STRUCTURAL_ID, Structural, StructuralOptions},
    tested_by::{TESTED_BY_ID, TestedByMap},
    traverse::{
        source::{Arbitration, ClaimFamily},
        trace::PlanMap,
    },
};
use std::{
    collections::HashSet,
    path::Path,
    sync::Arc,
    time::{Duration, Instant},
};

/// The map that finds the start nodes from the question's words (the
/// stand-in, `seed_source` `name_tokens`).
pub const SEEDS_MAP: MapId = "seeds.scip_names";
/// The map of the start nodes the hybrid search found (`seed_source`
/// `hybrid`).
pub const SEEDS_HYBRID: MapId = "seeds.hybrid";
/// The map of the start nodes BM25 over the graph's symbol names, paths and
/// documentation finds (`seed_source` `hybrid+bm25` or `bm25`). Opt-in: it
/// runs only when `[run_profile.seeds] bm25` is on, and then it is part of
/// the default set of a call that names no maps.
pub const SEEDS_BM25: MapId = "seeds.bm25";
/// The map of the start nodes ripgrep's whole-word search finds for the names
/// the question puts in code spans (`seed_source` `hybrid+literal`,
/// `hybrid+bm25+literal`, `bm25+literal` or `literal`). Opt-in: it runs only
/// when `[run_profile.seeds] literal` is on, and then it is part of the
/// default set of a call that names no maps.
pub const SEEDS_LITERAL: MapId = "seeds.literal";
/// The map of the start nodes a personalized PageRank over the SCIP graph
/// finds, the random jump on the entities `seeds.bm25`'s index finds for the
/// question (`seed_source` `hybrid+repo_map`, `hybrid+bm25+repo_map`,
/// `bm25+repo_map` or `repo_map`, and those with `+literal`). Opt-in: it runs
/// only when `[run_profile.seeds] repo_map` is on, and then it is part of the
/// default set of a call that names no maps.
pub const SEEDS_REPO_MAP: MapId = "seeds.repo_map";
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
/// closure of the SCIP impact index), and `scip.implements` (the traits a type
/// implements, the types that implement a trait and the members that implement
/// a trait method, read from the `impl .. for ..` headers of the tree and the
/// SCIP index), and `tests.tested_by` (the test units that reference a symbol,
/// read from the same impact index; a test by its path or by an inline `tests`
/// module in its symbol chain), and `docs.mentions` (the documentation files,
/// by source role, whose current bytes mention a node's name; a wrapper of
/// `grep.live` that reads documentation files only), and `grep.structural`
/// (the `impl`, `extends` and `implements` headers and the calls of a symbol's
/// name: ripgrep names the candidate files, an ast-grep pattern over each
/// file's syntax tree confirms the role; needs `rg`; in the default set of a
/// call with a tree root only with `[run_profile.grep] structural`). `lsp.live` asks a
/// language server for definition, references and document symbols (starts the
/// server at the first expansion; needs its binary; a service call with a tree
/// and the service's manager also runs it by default, `[run_profile.lsp] live`).
/// The two exact operators `exact.closure` (the
/// declarations that reference the symbol the question names, from the same
/// index as `scip.impact`) and `exact.pickaxe` (the commits `git log -G/-S/--grep`
/// finds for a history question) are never walked: their cards are the first
/// stage (see [`first_stage`](super::first_stage) and `[traverse] first_stage`).
pub const OPT_IN_MAPS: [MapId; 17] = [
    GREP_LIVE_MAP_ID,
    DOCS_MENTIONS_ID,
    STRUCTURAL_ID,
    CONFIG_KEY_MAP_ID,
    GIT_HISTORY_ID,
    PROBE_COMPILE_ID,
    SEARCH_LEXICAL_ID,
    SEARCH_VECTOR_ID,
    SCIP_IMPACT_ID,
    SCIP_IMPLEMENTS_ID,
    TESTED_BY_ID,
    LSP_LIVE_ID,
    EXACT_CLOSURE_ID,
    EXACT_PICKAXE_ID,
    SEEDS_BM25,
    SEEDS_LITERAL,
    SEEDS_REPO_MAP,
];
/// The longest the `rg --version` probe of `grep.structural` waits.
const RIPGREP_PROBE_TIMEOUT: Duration = Duration::from_secs(5);

/// Whether `rg --version` runs for the resolved ripgrep path: `grep.structural`
/// asks ripgrep for its candidate files and has no other source. The probe is
/// bounded and memoised per binary for the process (d470), as the readiness
/// check's is.
pub(super) async fn ripgrep_runs(resolved: &Path) -> bool {
    lci_core::lsp::adapter::probe_subprocess_version(resolved, RIPGREP_PROBE_TIMEOUT)
        .await
        .is_ok()
}

/// The note of a call that names `probe.compile` without the trust grant.
pub(super) const PROBE_COMPILE_SKIPPED_NOTE: &str = "probe.compile skipped: cargo check would run the tree's build scripts and procedural macros; set rust_analyzer_trusted = true in the main configuration for a trusted tree";

/// Logs, once per process, that `probe.compile` is running build scripts.
pub(super) fn warn_probe_compile_once() {
    static WARNED: std::sync::Once = std::sync::Once::new();
    WARNED.call_once(|| {
        tracing::warn!(
            "probe.compile runs cargo check on a scratch copy of the tree: build scripts and procedural macros execute (rust_analyzer_trusted = true)"
        );
    });
}

/// What a call wants of the registry: the maps (`wanted`, in the order the
/// registry registers them) and the notes the choice gives (the `[run_profile]`
/// off notes in the order bm25, literal, repo_map, structural, then the
/// policy's maps left out for want of a tree).
pub(super) struct Selection {
    pub wanted: Vec<MapId>,
    pub notes: Vec<String>,
}

impl Selection {
    /// Whether the call wants the map.
    pub(super) fn wants(&self, id: MapId) -> bool {
        self.wanted.contains(&id)
    }
}

/// Chooses the maps of a call: the named ones, or the defaults plus the
/// opt-in maps the run profile and the call's tree and manager switch on. The
/// errors are the caller's mistakes (an unknown map, a map that needs the tree
/// without one). `policy` and `preset` are what `presets::policy_for` and
/// `presets::resolve` made of the call's steering.
pub(super) fn select(
    call: &Call<'_>,
    profile: &RunProfileConfig,
    policy: &TraversePolicy,
    preset: Option<&str>,
) -> Result<Selection> {
    let mut bm25_off_note: Option<String> = None;
    let mut literal_off_note: Option<String> = None;
    let mut repo_map_off_note: Option<String> = None;
    let mut structural_off_note: Option<String> = None;
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
    // Extra maps named by the policy that this call cannot run (no tree).
    let mut skipped_maps: Vec<String> = Vec::new();
    let wanted: Vec<MapId> = match call.maps {
        None => {
            let mut ids = MAPS.to_vec();
            // The policy's extra maps read the tree.
            if call.scip_root.is_some() {
                for name in policy.extra_maps.iter().flatten() {
                    let id = known_map(name).map_err(|e| {
                        anyhow!("policy {}: extra_maps: {e}", preset.unwrap_or_default())
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
    let mut wanted = wanted;
    // `[run_profile.lsp] live`: `lsp.live` joins the default set of a call
    // that has a tree and a manager that outlives it. The CLI one-shot and
    // `sessions = "per_call"` are left out (a server that dies with the call
    // cannot amortise its start, d438); a missing or dead server is a note and
    // an unavailable map below, never an error.
    if profile.lsp.live
        && call.maps.is_none()
        && call.scip_root.is_some()
        && profile.lsp.sessions == LspSessions::Shared
        && call.lsp_sessions.is_some()
        && !wanted.contains(&LSP_LIVE_ID)
    {
        wanted.push(LSP_LIVE_ID);
    }
    // `[run_profile.seeds] bm25`: the map is in the default set only when the
    // key is on; with it off a call that names the map gets a note and the
    // map is not part of the call (nothing registers, nothing seeds).
    if profile.seeds.bm25 {
        if call.maps.is_none() && !wanted.contains(&SEEDS_BM25) {
            wanted.push(SEEDS_BM25);
        }
    } else if wanted.contains(&SEEDS_BM25) {
        wanted.retain(|id| *id != SEEDS_BM25);
        if call.maps.is_some() {
            bm25_off_note = Some(
                "seeds.bm25 is off ([run_profile.seeds] bm25 = false): the map gives no start nodes"
                    .to_owned(),
            );
        }
    }
    // `[run_profile.seeds] literal`: the same for `seeds.literal`.
    if profile.seeds.literal {
        if call.maps.is_none() && !wanted.contains(&SEEDS_LITERAL) {
            wanted.push(SEEDS_LITERAL);
        }
    } else if wanted.contains(&SEEDS_LITERAL) {
        wanted.retain(|id| *id != SEEDS_LITERAL);
        if call.maps.is_some() {
            literal_off_note = Some(
                "seeds.literal is off ([run_profile.seeds] literal = false): the map gives no start nodes"
                    .to_owned(),
            );
        }
    }
    // `[run_profile.seeds] repo_map`: the same for `seeds.repo_map`.
    if profile.seeds.repo_map {
        if call.maps.is_none() && !wanted.contains(&SEEDS_REPO_MAP) {
            wanted.push(SEEDS_REPO_MAP);
        }
    } else if wanted.contains(&SEEDS_REPO_MAP) {
        wanted.retain(|id| *id != SEEDS_REPO_MAP);
        if call.maps.is_some() {
            repo_map_off_note = Some(
                "seeds.repo_map is off ([run_profile.seeds] repo_map = false): the map gives no start nodes"
                    .to_owned(),
            );
        }
    }
    // `[run_profile.grep] structural`: the same for `grep.structural`, which
    // reads the tree, so a call joins it to the default set only with a root.
    if profile.grep.structural {
        if call.maps.is_none() && call.scip_root.is_some() && !wanted.contains(&STRUCTURAL_ID) {
            wanted.push(STRUCTURAL_ID);
        }
    } else if wanted.contains(&STRUCTURAL_ID) {
        wanted.retain(|id| *id != STRUCTURAL_ID);
        if call.maps.is_some() {
            structural_off_note = Some(
                "grep.structural is off ([run_profile.grep] structural = false): the map gives no edges"
                    .to_owned(),
            );
        }
    }
    // `seeds.bm25` and `seeds.repo_map` read the SCIP graph only, not the
    // tree; `seeds.literal` searches the tree through the application's search
    // and says so when the call has none.
    if wanted.iter().any(|id| {
        ![SEEDS_BM25, SEEDS_REPO_MAP, SEEDS_LITERAL].contains(id) && OPT_IN_MAPS.contains(id)
    }) && call.scip_root.is_none()
    {
        bail!("the maps named beyond the defaults read the tree: give its root");
    }
    let mut notes = Vec::new();
    notes.extend(bm25_off_note);
    notes.extend(literal_off_note);
    notes.extend(repo_map_off_note);
    notes.extend(structural_off_note);
    if !skipped_maps.is_empty() {
        notes.push(format!(
            "policy {}: the maps {} read the tree and were left out: give --root (or the workspace) to run them as measured",
            preset.unwrap_or_default(),
            skipped_maps.join(", ")
        ));
    }
    Ok(Selection { wanted, notes })
}

/// What the registration reads. `'f` is the borrow of the start nodes alone:
/// the caller adds the trait seeds to them once `build` has returned, so what
/// `build` returns must not hold it.
pub(super) struct Context<'a, 'f> {
    pub call: &'a Call<'a>,
    pub profile: &'a RunProfileConfig,
    pub loaded: &'a Loaded,
    pub observer: &'a Observer,
    pub selection: &'a Selection,
    pub found: &'f Seeds,
    pub walk_restarts: &'a [lci_core::lsp::Restart],
}

/// The registry of a call and what the rest of the call reads of the maps
/// that built it.
pub(super) struct Built<'a> {
    pub registry: Registry<'a>,
    pub plan_maps: Vec<PlanMap>,
    pub unavailable: Vec<MapId>,
    pub search_maps: (
        Option<Arc<LexicalChunks<'a>>>,
        Option<Arc<VectorChunks<'a>>>,
    ),
    pub git_history: Option<Arc<GitHistoryMap>>,
    pub impact: Option<Arc<ImpactMap>>,
    pub site_impact: Option<Arc<ImpactMap>>,
    pub implements: Option<Arc<ImplementsMap>>,
    pub lsp_live: Option<Arc<LspLive>>,
    pub lsp_live_sessions: Vec<(&'static str, Arc<lci_core::lsp::Manager>, String)>,
    pub lsp_live_walk: Option<Arc<lci_core::lsp::WalkSessions>>,
    /// The places arm `lsp.live` resolved to for this call (`true`: the
    /// outline arm); `false` when it is not registered.
    pub outline: bool,
    pub setup: Instant,
    pub early: Duration,
}

/// Registers the maps the call wants, in the order the registry lists them,
/// and sets up the ones that read the tree. A map that cannot be set up is a
/// note and an entry of `unavailable`, never an error.
pub(super) async fn build<'a>(
    cx: &Context<'a, '_>,
    notes: &mut Vec<String>,
    warnings: &mut Vec<String>,
) -> Result<Built<'a>> {
    let Context {
        call,
        profile,
        loaded,
        observer,
        selection,
        found,
        walk_restarts,
    } = *cx;
    let wants = |id: MapId| selection.wants(id);
    // The maps the call asked for that could not be set up (K13: an answer that
    // lacks one is not labelled complete). Read only under `answer_labels`.
    let mut unavailable: Vec<MapId> = Vec::new();
    let mut registry = Registry::new(60.0);
    let mut plan_maps = Vec::new();
    register(
        &mut registry,
        &mut plan_maps,
        observer,
        Box::new(SeedMap::hybrid()),
        wants(SEEDS_HYBRID),
    )?;
    register(
        &mut registry,
        &mut plan_maps,
        observer,
        Box::new(SeedMap::name_tokens()),
        wants(SEEDS_MAP),
    )?;
    // Registered only when the call runs it, so a call without it (the
    // default) lists and traces the maps it always did.
    if wants(SEEDS_BM25) {
        register(
            &mut registry,
            &mut plan_maps,
            observer,
            Box::new(SeedMap::bm25()),
            true,
        )?;
    }
    if wants(SEEDS_REPO_MAP) {
        register(
            &mut registry,
            &mut plan_maps,
            observer,
            Box::new(SeedMap::repo_map()),
            true,
        )?;
    }
    if wants(SEEDS_LITERAL) {
        register(
            &mut registry,
            &mut plan_maps,
            observer,
            Box::new(SeedMap::literal()),
            true,
        )?;
    }
    register(
        &mut registry,
        &mut plan_maps,
        observer,
        Box::new(Shared(loaded.scip.clone())),
        wants(SCIP_MAP_ID),
    )?;
    // The chunks the search maps reach have no lines in their ids: with a
    // search map on, every chunk of the index is given to structure.contains
    // so that it can expand them.
    let early = Instant::now();
    let mut lines = chunk_lines(found);
    let mut search_index: Option<Arc<dyn ChunkSearch + '_>> = None;
    if wants(SEARCH_LEXICAL_ID) || wants(SEARCH_VECTOR_ID) {
        match call.seed_search.and_then(|search| search.chunk_search()) {
            None => {
                notes.push(
                    "search.lexical and search.vector need the workspace index: give --root and a question or seeds, with the application available".to_owned(),
                );
                unavailable.extend(
                    [SEARCH_LEXICAL_ID, SEARCH_VECTOR_ID]
                        .into_iter()
                        .filter(|id| wants(id)),
                );
            }
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
        observer,
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
        register(
            &mut registry,
            &mut plan_maps,
            observer,
            Box::new(grep),
            true,
        )?;
    }
    if let (true, Some(root)) = (wants(DOCS_MENTIONS_ID), call.scip_root) {
        let docs = DocsMentions::new(root);
        // One walk over the documentation files only: enough for the card's median.
        docs.calibrate(1).await;
        register(
            &mut registry,
            &mut plan_maps,
            observer,
            Box::new(docs),
            true,
        )?;
    }
    if let (true, Some(root)) = (wants(STRUCTURAL_ID), call.scip_root) {
        let configured = call.lsp.map_or("rg", |config| config.ripgrep_path.as_str());
        let resolved = lexical::resolve_ripgrep_path(configured);
        if ripgrep_runs(&resolved).await {
            let structural = Structural::new(
                root,
                Box::new(Ripgrep::new(resolved.to_string_lossy().into_owned())),
                loaded.places().await,
                StructuralOptions::default(),
            );
            notes.extend(
                structural
                    .notes()
                    .iter()
                    .map(|dropped| format!("grep.structural: a pattern was dropped: {dropped}")),
            );
            // One expansion of a name that occurs nowhere: ripgrep over the
            // tree, enough for the card's median.
            structural.calibrate(1).await;
            register(
                &mut registry,
                &mut plan_maps,
                observer,
                Box::new(structural),
                true,
            )?;
        } else {
            notes.push(
                "grep.structural needs ripgrep (`rg`; [index] ripgrep_path): it was not found, the map gives no edges"
                    .to_owned(),
            );
            unavailable.push(STRUCTURAL_ID);
        }
    }
    if let (true, Some(root)) = (wants(CONFIG_KEY_MAP_ID), call.scip_root) {
        let map = match config_key_map(call.scip_index, root, loaded).await {
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
                unavailable.push(CONFIG_KEY_MAP_ID);
                Arc::new(ConfigKeyMap::empty(ConfigKeyOptions::default()))
            }
        };
        register(
            &mut registry,
            &mut plan_maps,
            observer,
            Box::new(Shared(map)),
            true,
        )?;
    }
    let mut git_history = None;
    if wants(GIT_HISTORY_ID) {
        match call.scip_root {
            None => {
                notes.push("git.history needs the tree: give --root".to_owned());
                unavailable.push(GIT_HISTORY_ID);
            }
            Some(root) => {
                let table = symbol_files(&loaded.scip);
                match GitHistoryMap::open_with(root, GitHistoryConfig::default(), table) {
                    Ok(map) => {
                        let map = Arc::new(map);
                        register(
                            &mut registry,
                            &mut plan_maps,
                            observer,
                            Box::new(Shared(map.clone())),
                            true,
                        )?;
                        git_history = Some(map);
                    }
                    Err(error) => {
                        notes.push(format!("git.history is unavailable: {error}"));
                        unavailable.push(GIT_HISTORY_ID);
                    }
                }
            }
        }
    }
    if wants(PROBE_COMPILE_ID) {
        match call.scip_root {
            None => {
                notes.push("probe.compile needs the tree: give --root".to_owned());
                unavailable.push(PROBE_COMPILE_ID);
            }
            Some(_) if !call.lsp.is_some_and(|config| config.rust_analyzer_trusted) => {
                notes.push(PROBE_COMPILE_SKIPPED_NOTE.to_owned());
                unavailable.push(PROBE_COMPILE_ID);
                warnings.push(PROBE_COMPILE_SKIPPED_NOTE.to_owned());
            }
            Some(root) => {
                notes.push(format!(
                    "probe.compile: cargo check runs on a scratch copy of {} with --offline, build scripts and procedural macros of the tree and its dependencies execute (rust_analyzer_trusted = true)",
                    root.display()
                ));
                warn_probe_compile_once();
                let map = ProbeMap::new(loaded.scip.clone(), ProbeConfig::for_tree(root));
                register(
                    &mut registry,
                    &mut plan_maps,
                    observer,
                    Box::new(Shared(Arc::new(map))),
                    true,
                )?;
            }
        }
    }
    let mut lsp_live = None;
    // Each server `lsp.live` runs, with the manager that runs it and the workspace id,
    // to report after the walk how its servers stood.
    let mut lsp_live_sessions: Vec<(&'static str, Arc<lci_core::lsp::Manager>, String)> =
        Vec::new();
    let mut lsp_live_workspace = String::new();
    let mut lsp_live_walk: Option<Arc<lci_core::lsp::WalkSessions>> = None;
    let mut lsp_live_outline = false;
    if let (true, Some(root)) = (wants(LSP_LIVE_ID), call.scip_root) {
        // The servers of the call's settings; a server starts at the first
        // expansion. `per_call`: the manager is the map's alone, so the server
        // stops when the map drops. `shared`: a server that sees an edit on its
        // own (or every server under `sync = "watch"`, the default) runs on the caller's
        // manager, whose owner stops it (idle sweeper, release, shutdown); the
        // other servers run on the call's own manager, as under `per_call`.
        let workspace = dunce::canonicalize(root)
            .map(|path| lci_core::workspace::Workspace {
                id: lci_core::workspace::hash(&path.to_string_lossy()),
                path,
            })
            .map_err(|error| error.to_string());
        let backend = match workspace {
            Ok(workspace) => {
                let defaults = lci_core::config::Config::default();
                let settings = call.lsp.unwrap_or(&defaults);
                let own = per_call_manager(call);
                let shared = match (profile.lsp.sessions, &call.lsp_sessions) {
                    (LspSessions::Shared, Some(shared)) => Some(Arc::clone(shared)),
                    _ => None,
                };
                lsp_live_workspace = workspace.id.clone();
                // The version probe is bounded like the readiness probes. It
                // runs inside the setup window that `budget.deadline +=
                // setup.elapsed()` below credits back to the walk deadline, so
                // a slow probe costs the walk no budget.
                let probe =
                    std::time::Duration::from_secs(settings.readiness_timeout_seconds.clamp(1, 30));
                let probe_started = Instant::now();
                let opened = match shared {
                    Some(shared) => ManagerBackend::open_split(workspace, shared, own, probe).await,
                    None => ManagerBackend::open_with_probe_timeout(workspace, own, probe).await,
                };
                // `cargo xtask maps lsp-cost` reads this line: the version probe is
                // memoised for the process (d470), per-call managers included, so
                // only the first probe of a binary logs real ms and later calls
                // log about 0.
                tracing::debug!(
                    probe_ms = probe_started.elapsed().as_secs_f64() * 1000.0,
                    probe_limit_ms = probe.as_millis() as u64,
                    ok = opened.is_ok(),
                    "lsp.live version probe"
                );
                opened
            }
            Err(error) => Err(error),
        };
        for restart in walk_restarts {
            notes.push(restart.note("lsp.live"));
        }
        match backend {
            Ok(backend) => {
                for (provider, manager) in backend.routes() {
                    lsp_live_sessions.push((provider, manager, lsp_live_workspace.clone()));
                }
                for restart in backend.restarted() {
                    notes.push(restart.note("lsp.live"));
                }
                // Which servers this walk found running is read when it ends
                // (the barrier cannot say, see `WalkSessions`).
                lsp_live_walk = Some(backend.walk());
                let scip_places = ScipPlaces::from_scip(&loaded.scip);
                // `outline`: the index's table first, the server's own outline
                // for what it lacks (everything under `scip.mode = "off"`).
                // `auto` (the default): the index's table when the graph read
                // for this workspace has a node (then the walk is the `scip`
                // walk), else the outline (no index is read under `scip.mode =
                // off`, or under `cache` with no file: nothing else would
                // place a symbol).
                let outline = match profile.lsp.places {
                    LspPlaces::Scip => false,
                    LspPlaces::Outline => true,
                    LspPlaces::Auto => loaded.scip.graph().nodes.is_empty(),
                };
                lsp_live_outline = outline;
                let places: Arc<dyn SymbolPlaces> = if outline {
                    Arc::new(OutlinePlaces::over(scip_places).with_tree(root))
                } else {
                    Arc::new(scip_places)
                };
                let map = Arc::new(
                    LspLive::new(Box::new(backend), places)
                        .implementations(profile.lsp.implementations)
                        .call_hierarchy(profile.lsp.call_hierarchy),
                );
                register(
                    &mut registry,
                    &mut plan_maps,
                    observer,
                    Box::new(Shared(map.clone())),
                    true,
                )?;
                lsp_live = Some(map);
            }
            Err(error) => {
                notes.push(format!("lsp.live is unavailable: {error}"));
                unavailable.push(LSP_LIVE_ID);
            }
        }
    }
    // The maps above that read the tree and were skipped without a word for lack of one.
    if call.scip_root.is_none() {
        unavailable.extend(
            [
                GREP_LIVE_MAP_ID,
                DOCS_MENTIONS_ID,
                STRUCTURAL_ID,
                CONFIG_KEY_MAP_ID,
                LSP_LIVE_ID,
            ]
            .into_iter()
            .filter(|id| wants(id)),
        );
    }
    let mut impact = None;
    if wants(SCIP_IMPACT_ID) {
        match call.scip_root {
            None => {
                notes.push("scip.impact needs the tree: give --root".to_owned());
                unavailable.push(SCIP_IMPACT_ID);
            }
            Some(root) => {
                let map = match impact_map(call.scip_index, root, loaded).await {
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
                        unavailable.push(SCIP_IMPACT_ID);
                        Arc::new(ImpactMap::empty(ImpactOptions::default()))
                    }
                };
                impact = Some(map.clone());
                register(
                    &mut registry,
                    &mut plan_maps,
                    observer,
                    Box::new(Shared(map)),
                    true,
                )?;
            }
        }
    }
    if wants(TESTED_BY_ID) {
        match call.scip_root {
            None => {
                notes.push("tests.tested_by needs the tree: give --root".to_owned());
                unavailable.push(TESTED_BY_ID);
            }
            Some(root) => {
                // The index `scip.impact` reads: shared when that map was
                // built, else built here (cached by the stamp).
                let shared = match &impact {
                    Some(map) => map.clone(),
                    None => match impact_map(call.scip_index, root, loaded).await {
                        Ok(map) => map,
                        Err(error) => {
                            notes.push(format!("tests.tested_by has no edges: {error:#}"));
                            unavailable.push(TESTED_BY_ID);
                            Arc::new(ImpactMap::empty(ImpactOptions::default()))
                        }
                    },
                };
                let map = TestedByMap::new(shared);
                notes.push(format!(
                    "tests.tested_by: {} test units in the impact index, cap {} edges per node",
                    map.test_units(),
                    lci_maps::EXPAND_LIMIT
                ));
                register(&mut registry, &mut plan_maps, observer, Box::new(map), true)?;
            }
        }
    }
    let mut implements = None;
    let mut site_impact = None;
    if wants(SCIP_IMPLEMENTS_ID) {
        match call.scip_root {
            None => {
                notes.push("scip.implements needs the tree: give --root".to_owned());
                unavailable.push(SCIP_IMPLEMENTS_ID);
            }
            Some(root) => {
                let map = match implements_map(call.scip_index, root, loaded).await {
                    Ok(map) => {
                        let stats = map.stats();
                        let report = &loaded.scip.graph().report;
                        notes.push(format!(
                            "scip.implements: {} impl headers read ({} with a trait, {} external; {} macro and {} other unresolved, {} documents without source), {} type-trait and {} method pairs, {} facts dropped for lack of a node id, cap {} edges per node",
                            report.impl_headers,
                            report.impl_resolved,
                            report.impl_external_trait,
                            report.impl_unresolved_macro,
                            report.impl_unresolved_other,
                            report.impl_no_source,
                            stats.type_trait,
                            stats.methods,
                            stats.dropped,
                            map.options().cap
                        ));
                        // The types and traits it reaches are `scip.impact`
                        // nodes, and only that map locates them; it is built
                        // already (cached by the stamp), so sites come
                        // without walking its edges.
                        if impact.is_none() {
                            match impact_map(call.scip_index, root, loaded).await {
                                Ok(sites) => site_impact = Some(sites),
                                Err(error) => notes.push(format!(
                                    "scip.implements: its types and traits have no site: {error:#}"
                                )),
                            }
                        }
                        map
                    }
                    Err(error) => {
                        notes.push(format!("scip.implements has no edges: {error:#}"));
                        unavailable.push(SCIP_IMPLEMENTS_ID);
                        Arc::new(ImplementsMap::empty(ImplementsOptions::default()))
                    }
                };
                implements = Some(map.clone());
                register(
                    &mut registry,
                    &mut plan_maps,
                    observer,
                    Box::new(Shared(map)),
                    true,
                )?;
            }
        }
    }
    // The exact operators are descriptor-only maps; their cards are the first
    // stage, made once per call and not on a continuation.
    if wants(EXACT_CLOSURE_ID) {
        register(
            &mut registry,
            &mut plan_maps,
            observer,
            Box::new(SeedMap::exact_closure()),
            true,
        )?;
    }
    if wants(EXACT_PICKAXE_ID) {
        register(
            &mut registry,
            &mut plan_maps,
            observer,
            Box::new(SeedMap::exact_pickaxe()),
            true,
        )?;
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
                observer,
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
                observer,
                Box::new(Shared(map.clone())),
                true,
            )?;
            search_maps.1 = Some(map);
        }
    }
    Ok(Built {
        registry,
        plan_maps,
        unavailable,
        search_maps,
        git_history,
        impact,
        site_impact,
        implements,
        lsp_live,
        lsp_live_sessions,
        lsp_live_walk,
        outline: lsp_live_outline,
        setup,
        early,
    })
}

/// The arbitration of a call (`[run_profile.scip] arbitration`): `None` unless
/// the key is on, `scip.mode` is not `authoritative` and `lsp.live` was built.
/// The server is the authority over the SCIP maps the call registered and
/// built: `scip.impact` (`Refers`), `scip.implements` (`Implements`, with
/// `[run_profile.lsp] implementations`) and `structure.contains`
/// (`Structure`). `graph.scip` is never listed: its edges are callees and a
/// claim family carries no direction. Only function and method destinations
/// are judged under either places arm (the index's place table holds nothing
/// else). `None` when no listed map is part of the call.
pub(super) fn arbitration(
    built: &Built<'_>,
    selection: &Selection,
    profile: &RunProfileConfig,
) -> Option<Arbitration> {
    if !profile.scip.arbitration
        || profile.scip.mode == ScipMode::Authoritative
        || built.lsp_live.is_none()
    {
        return None;
    }
    let built_ok = |id: MapId, map_built: bool| {
        selection.wants(id) && map_built && !built.unavailable.contains(&id)
    };
    let mut subordinate: Vec<(MapId, ClaimFamily)> = Vec::new();
    if built_ok(SCIP_IMPACT_ID, built.impact.is_some()) {
        subordinate.push((SCIP_IMPACT_ID, ClaimFamily::Refers));
    }
    if profile.lsp.implementations && built_ok(SCIP_IMPLEMENTS_ID, built.implements.is_some()) {
        subordinate.push((SCIP_IMPLEMENTS_ID, ClaimFamily::Implements));
    }
    if selection.wants(CONTAINS_MAP_ID) {
        subordinate.push((CONTAINS_MAP_ID, ClaimFamily::Structure));
    }
    if subordinate.is_empty() {
        return None;
    }
    Some(Arbitration {
        authority: LSP_LIVE_ID,
        subordinate,
        kinds: vec![SymbolKind::Function, SymbolKind::Method],
    })
}

/// Adds a map to the registry (weight 1, as every map here) and to the
/// plan the trace records, switched off when the call did not ask for it.
fn register<'a>(
    registry: &mut Registry<'a>,
    plan: &mut Vec<PlanMap>,
    observer: &Observer,
    map: Box<dyn Map + 'a>,
    enabled: bool,
) -> Result<()> {
    // With `[run_profile.traverse] map_freshness` on, a map that states its freshness is
    // wrapped so that its expansions are recorded; off, it is registered as is.
    let map = observer.wrap(map);
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
