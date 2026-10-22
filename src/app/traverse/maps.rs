//! The map registry of a traversal: the map ids and the lists of the maps a
//! call can use, and the helpers the map registration shares (K39a; History:
//! the registry of `run_with_trait_seeds` in `mod.rs`).

use super::first_stage::{EXACT_CLOSURE_ID, EXACT_PICKAXE_ID};
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
