//! The map registry of a traversal: the map ids and the lists of the maps a
//! call can use, and the helpers the map registration shares (K39a; History:
//! the registry of `run_with_trait_seeds` in `mod.rs`).

use super::{
    Call,
    first_stage::{EXACT_CLOSURE_ID, EXACT_PICKAXE_ID},
};
use crate::config::{LspSessions, RunProfileConfig};
use anyhow::{Result, anyhow, bail};
use lci_core::config::TraversePolicy;
use lci_maps::{
    MapId,
    config_key::CONFIG_KEY_MAP_ID,
    contains::CONTAINS_MAP_ID,
    docs_mentions::DOCS_MENTIONS_ID,
    git_history::GIT_HISTORY_ID,
    live_grep::GREP_LIVE_MAP_ID,
    lsp_live::LSP_LIVE_ID,
    probe_map::PROBE_COMPILE_ID,
    scip_graph::SCIP_MAP_ID,
    scip_impact::SCIP_IMPACT_ID,
    scip_implements::SCIP_IMPLEMENTS_ID,
    search_maps::{SEARCH_LEXICAL_ID, SEARCH_VECTOR_ID},
    structural::STRUCTURAL_ID,
    tested_by::TESTED_BY_ID,
};
use std::{path::Path, time::Duration};

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
