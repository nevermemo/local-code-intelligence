//! `[run_profile]` and the run-profile hash: the part of a resolved
//! configuration that can change what a search returns, in a canonical form
//! with a stable hash.
//!
//! The profile is the resolved [`Config`] (as JSON, with the sections that are
//! left out of its serialization while they equal the default put back) minus
//! a deny-list of dotted keys: the machine-set keys of
//! [`layers::machine_keys`](super::layers::machine_keys) plus
//! [`EXCLUDED_KEYS`] (timeouts and timing, throughput, file paths, per-machine
//! security, service identity). Everything else is in, so a key added to
//! [`Config`] is part of the profile until someone excludes it by name.
//!
//! `golden/run_profile_default.txt` lists every leaf of the profile with its
//! default value. Adding, removing or re-defaulting a leaf fails the golden
//! test until the golden moves: run the profile test with `BLESS_GOLDEN=1`
//! (`BLESS_GOLDEN=1 cargo test -p lci-core --lib config::profile`) and review
//! the golden's diff.

use super::{Config, layers::machine_keys};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// `[run_profile]`: the home of the switches that name a run's configuration
/// (cycle 1: every new behaviour is a key here whose default keeps today's
/// behaviour). An unknown key is an error.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunProfileConfig {
    /// `[run_profile.traverse]`: switches of the traversal.
    pub traverse: RunProfileTraverse,
    /// `[run_profile.walk]`: how a traversal counts evidence.
    pub walk: WalkProfile,
    /// `[run_profile.scip]`: how a traversal treats the SCIP index.
    pub scip: ScipProfile,
    /// `[run_profile.lsp]`: how `lsp.live` gets its language servers.
    pub lsp: LspProfile,
    /// `[run_profile.freshness]`: whether an answer is checked against the tree.
    pub freshness: RunProfileFreshness,
    /// `[run_profile.seeds]`: where a traversal's starting nodes come from.
    pub seeds: RunProfileSeeds,
    /// `[run_profile.grep]`: switches of the grep-style maps.
    pub grep: RunProfileGrep,
}

/// `[run_profile.grep]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunProfileGrep {
    /// Let the opt-in map `grep.structural` run. `false` (the default): naming
    /// it gives a note and no edges, ripgrep is not started for it, it is not
    /// in the default map set, and every answer is as without the key. `true`:
    /// the map is in the default set of a call that has a tree root and names
    /// no maps, and it runs when named. From a symbol it finds the files that
    /// hold the name as a whole word with ripgrep, parses each with the
    /// chunker's tree-sitter grammar and keeps the occurrences in a header
    /// (`impl .. for ..`, `extends`, `implements`) or a call, by an ast-grep
    /// pattern; a name in a comment or a string gives no edge.
    pub structural: bool,
}

/// `[run_profile.seeds]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunProfileSeeds {
    /// Let the opt-in map `seeds.bm25` run. `false` (the default): naming it
    /// gives a note and no seeds, it is not in the default map set, and every
    /// answer is as without the key. `true`: the map is in the default set
    /// when a call names no maps, and it runs when named. It ranks the SCIP
    /// graph's function and method nodes by BM25 over their name, path and
    /// documentation tokens, and its nodes are appended after the hybrid
    /// search's seeds. The recipes' seeds are given, so they run the same
    /// under both values.
    pub bm25: bool,
    /// Let the opt-in map `seeds.literal` run. `false` (the default): naming
    /// it gives a note and no seeds, it is not in the default map set, ripgrep
    /// is not started for it, and every answer is as without the key. `true`:
    /// the map is in the default set when a call names no maps, and it runs
    /// when named. It searches the tree with ripgrep for the whole-word
    /// occurrences of the names the question puts in code spans, places each
    /// line in the SCIP graph (a definition, else the enclosing function, else
    /// the file of a declaration), and appends those nodes after the hybrid
    /// search's seeds. The recipes' seeds are given, so they run the same
    /// under both values.
    pub literal: bool,
}

/// `[run_profile.freshness]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunProfileFreshness {
    /// Check what a call cites against the working tree by content hash
    /// ("verify on read"). `false` (the default): every answer is as without
    /// the key. `true`: a traverse call with `[run_profile.traverse]
    /// map_freshness` also on labels the maps that read the index snapshot or
    /// place by the index and adds `file_changed` to each card; a search
    /// reports a `freshness` block and `file_changed` per hit. It reads and
    /// labels; it never refreshes the index and never changes
    /// `[index].freshness`. It costs one read of the tree's source files per
    /// call. See the README "Verify on read".
    pub verify_on_read: bool,
    /// Skip re-reading files whose modified time and length have not changed
    /// when `map_freshness` or `verify_on_read` reads the tree. `false` (the
    /// default): every such read hashes every source file, as before. `true`:
    /// the process keeps `(modified time, length)` and the content hash per
    /// source file and per root, lists the tree without reading it, and reads
    /// and hashes only the files whose stamp is new or changed (a file with no
    /// modified time is always read). The answers equal the full-hash path's
    /// except for one gap: a same-length edit that keeps a modified time older
    /// than 3 seconds before the previous read of the file (a deliberate
    /// `set_modified`, or a clock that went back) is not seen until the file's
    /// next edit. A file modified within 3 seconds of a read is not trusted
    /// and is read again on the next call, which covers a write in the same
    /// clock tick and filesystems with coarse stamps (FAT and exFAT 2 s, SMB
    /// shares, 1 s filesystems). The table is
    /// per process and per root and is rebuilt on the first call; the index
    /// scan is not gated. The key is in the profile hash (the stamp says what
    /// ran).
    pub stat_gate: bool,
}

/// What a traversal does with the SCIP index (`[run_profile.scip] mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScipMode {
    /// The index is required and read; a missing or unreadable file is an
    /// error. The strict opt-in.
    Authoritative,
    /// The default. The index is a cache: a missing or unreadable file gives
    /// an empty SCIP graph and a note; a readable one is loaded as under
    /// `authoritative`. An unreadable file is parsed once per file stamp.
    #[default]
    Cache,
    /// The index path is never touched; the SCIP graph and every map built
    /// from the index are empty.
    Off,
}

impl ScipMode {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Authoritative => "authoritative",
            Self::Cache => "cache",
            Self::Off => "off",
        }
    }
}

impl std::str::FromStr for ScipMode {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "authoritative" => Ok(Self::Authoritative),
            "cache" => Ok(Self::Cache),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "mode must be \"authoritative\", \"cache\" or \"off\" (got {other:?})"
            )),
        }
    }
}

/// `[run_profile.scip]`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScipProfile {
    /// `"cache"` (default: read the index when it is there), `"authoritative"`
    /// (the index is required) or `"off"` (never read it).
    pub mode: ScipMode,
}

/// `[run_profile.traverse]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RunProfileTraverse {
    /// Name each map's freshness in a traverse answer: `maps`, an array of
    /// `{id, observed, stale, lag}` with one entry for every map of the call
    /// that states it. `false` (the default): the answer has no `maps` key and
    /// is as without the key. `true` counts the tree's source files that are
    /// newer than the index file or changed since the process first saw its
    /// stamp (a read of the tree's source files once per call) and adds the
    /// result to every response. See the README "Map freshness" for what
    /// `observed`, `stale` and `lag` mean.
    pub map_freshness: bool,
    /// Label every traverse answer (a walk, a continuation, an answer with no
    /// start node) with two facts a reader would otherwise derive: an answer
    /// level `completeness` (`{complete, why, estimate}`: `complete` is true
    /// exactly when `status` is `complete`, that is when the walk ended
    /// `complete` or `bound`, and no map the call asked for was lost; a map is
    /// lost when it could not be set up (for example `lsp.live` with no server)
    /// or had failed calls, and then `complete` is false, `why` is `degraded`
    /// (the walk's own `stop_reason` when it was cut or capped) and `degraded`
    /// lists the maps; `why` is otherwise the answer's `stop_reason`, `no_seeds`
    /// without a walk; `estimate` is the mark-recapture fraction of the K29
    /// hint, `null` until two maps share a node; a partial answer also gets a
    /// `notes` line) and, on every card of `results`, `checkpoint.answer`,
    /// `visited_cards` and `frontier`, a `freshness` word: `changed` when
    /// `file_changed` is true, `fresh` when it is false, else from `maps`: any
    /// map of the card `stale` gives `stale`, all of them found with a lag
    /// gives `fresh`; anything else is `unverified` (the honest answer with
    /// `map_freshness` and `verify_on_read` off). A card whose own file is
    /// unchanged reads `fresh` even under a stale snapshot, because its text is
    /// current; except that a card citing `lsp.live` is `unverified` when the
    /// walk found a language server running (a kept session: nothing checks
    /// what it read), and so is every such card restored from a checkpoint by
    /// a continuation. `false` (the default): the answer, the trace, the checkpoint
    /// and the handle are as without the key. `true` changes the response only:
    /// nothing is withheld, `results` are the same with either value, and the
    /// trace, the checkpoint hash and the handle do not change.
    pub answer_labels: bool,
    /// Put the route on the answer's cards: with `true`, every card of a
    /// traverse answer's `results` and of the checkpoint's `answer` that the
    /// walk reached gains `kind` (the kind of the edge that discovered the
    /// node; `null` for a start node) and `path` (the route from its start
    /// node, one `{node, map, kind, witness}` entry per node, `depth + 1`
    /// entries, the last one being the card itself). `false` (the default):
    /// the answer, the trace, the checkpoint and the handle are as without
    /// the key. A handle made with it on also keeps each state's `kind`, so a
    /// continuation shows the whole chain; a handle made with it off does not
    /// have the kinds of its earlier hops (they show `null`, and the answer
    /// says so). A continuation with the key off drops the kinds an on-handle
    /// carried (its handle has none), so a later call with the key on shows
    /// `null` for those hops too, with the note. The answer grows by roughly one entry of about 25 tokens per
    /// hop of each card; the README "Paths on cards" has the measured sizes.
    pub card_paths: bool,
}

/// How a traversal groups the evidence that reaches a node
/// (`[run_profile.walk] groups`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WalkGroups {
    /// One group per map (the maps derived from one shared graph are one
    /// group together). Today's behaviour.
    #[default]
    Tool,
    /// One group per claim: the same `(source node, destination node, claim
    /// family)` is one claim, whichever maps assert it.
    Claim,
}

impl WalkGroups {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Claim => "claim",
        }
    }
}

impl std::str::FromStr for WalkGroups {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "tool" => Ok(Self::Tool),
            "claim" => Ok(Self::Claim),
            other => Err(format!(
                "groups must be \"tool\" or \"claim\" (got {other:?})"
            )),
        }
    }
}

/// How the edges above the walk's gate are priced by rank
/// (`[run_profile.walk] pricing`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WalkPricing {
    /// Rank `r` of the edges above the gate is worth `1 / (1 + r)` of its
    /// kind's prior, whatever the fan-out. Today's behaviour.
    #[default]
    Rank,
    /// Rank `r` of `n` candidates is worth `1 / (1 + a + r * gate / n)` of its
    /// kind's prior (`gate` is 8; `a` is a trait seed's implementors ranked in
    /// front, 0 for any other node), so a wide fan-out is flattened instead of
    /// starved. Equal to `rank` at `n = gate`; at or below the gate nothing
    /// changes.
    Relative,
}

impl WalkPricing {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Rank => "rank",
            Self::Relative => "relative",
        }
    }
}

impl std::str::FromStr for WalkPricing {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "rank" => Ok(Self::Rank),
            "relative" => Ok(Self::Relative),
            other => Err(format!(
                "pricing must be \"rank\" or \"relative\" (got {other:?})"
            )),
        }
    }
}

/// `[run_profile.walk]`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WalkProfile {
    /// What counts as one piece of evidence: `"tool"` (default, one per map)
    /// or `"claim"`. `"claim"` cannot be combined with `[traverse] fusion =
    /// "rrf"` or `"log_odds"`.
    pub groups: WalkGroups,
    /// What a group made only of `mentions` edges (text matches) adds to the
    /// evidence count, `0.0..=1.0`; 1.0 (default) counts it like any other.
    pub grep_corroboration: f64,
    /// How the edges above the gate are priced: `"rank"` (default, `1 / (1 +
    /// r)` of the prior) or `"relative"` (`1 / (1 + a + r * gate / n)` for `n`
    /// candidates and `a` implementors ahead of a trait seed's). A handle does
    /// not carry it or refuse a different one: a resumed call prices under the
    /// profile it is given, like `gate` and `beam`, so a walk can mix pricings
    /// across resumes.
    pub pricing: WalkPricing,
}

impl Default for WalkProfile {
    fn default() -> Self {
        Self {
            groups: WalkGroups::Tool,
            grep_corroboration: 1.0,
            pricing: WalkPricing::Rank,
        }
    }
}

/// Whose language-server sessions `lsp.live` asks (`[run_profile.lsp] sessions`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspSessions {
    /// Every traverse call starts its own servers and stops them when its
    /// walk ends: every answer is read from a server that started after the
    /// last edit.
    PerCall,
    /// The calls of one process share that process's language servers (the
    /// ones navigation and the search channel use): a server started by one
    /// call is reused by the next and stops with the idle timeout,
    /// `release_workspace` or service shutdown. Only a server that lists the
    /// configuration files of its tree stamp (`LspAdapter::keeps_session`) and
    /// sees an edit on its own (`LspAdapter::watches_workspace`, today
    /// rust-analyzer) or is told by `[run_profile.lsp] sync = "watch"` keeps
    /// its session across calls; every other server behaves as under
    /// `per_call`. A kept session first passes the tree barrier of each walk
    /// (`Manager::tree_barrier`: a stamp from the index's file list; a changed
    /// or failed stamp restarts the server), so a reused server is never
    /// staler than under `per_call` for a change inside the stamp's coverage,
    /// and the walk labels its answer with a `kept session` note.
    #[default]
    Shared,
}

impl LspSessions {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PerCall => "per_call",
            Self::Shared => "shared",
        }
    }
}

impl std::str::FromStr for LspSessions {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "per_call" => Ok(Self::PerCall),
            "shared" => Ok(Self::Shared),
            other => Err(format!(
                "sessions must be \"per_call\" or \"shared\" (got {other:?})"
            )),
        }
    }
}

/// Whether a language-server manager tells its servers about file changes
/// (`[run_profile.lsp] sync`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspSync {
    /// A server walks the tree when it starts and is told nothing after.
    /// Today's behaviour.
    #[default]
    Off,
    /// While a workspace has a live session, a watcher of the workspace sends
    /// every source-file change to each of its servers as
    /// `workspace/didChangeWatchedFiles` (see `lsp::sync`).
    Watch,
}

impl LspSync {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Watch => "watch",
        }
    }
}

impl std::str::FromStr for LspSync {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "off" => Ok(Self::Off),
            "watch" => Ok(Self::Watch),
            other => Err(format!("sync must be \"off\" or \"watch\" (got {other:?})")),
        }
    }
}

/// Where `lsp.live` takes the places of symbols from
/// (`[run_profile.lsp] places`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LspPlaces {
    /// The SCIP index's table alone: a symbol has a place only if the index
    /// has a node for it, so under `scip.mode = "off"` nothing has one.
    /// Today's behaviour.
    #[default]
    Scip,
    /// The index's table first, then the live outline of the language server
    /// (`textDocument/documentSymbol`) for what it lacks or misplaces: one
    /// more external call per file it places a symbol in or expands.
    Outline,
}

impl LspPlaces {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Scip => "scip",
            Self::Outline => "outline",
        }
    }
}

impl std::str::FromStr for LspPlaces {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "scip" => Ok(Self::Scip),
            "outline" => Ok(Self::Outline),
            other => Err(format!(
                "places must be \"scip\" or \"outline\" (got {other:?})"
            )),
        }
    }
}

/// `[run_profile.lsp]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LspProfile {
    /// `"shared"` (default): the calls of a process reuse the sessions of the
    /// servers that see an edit on their own, and every other server runs per
    /// call (see [`LspSessions`]). `"per_call"`: each traverse call starts and
    /// stops its own language servers. The set lives in a manager that the
    /// caller supplies: the MCP and REST server's own, or one per process for
    /// `lci traverse`.
    /// The recipes walk no `lsp.live` and run the same under both values.
    pub sessions: LspSessions,
    /// `false` (default): every outline `lsp.live` needs (a file's
    /// `textDocument/documentSymbol`) is asked of the server. `true`: for a
    /// server that is resent the file's text before every request
    /// (`LspAdapter::opens_documents`: pyright, gopls, jdtls, clangd), a
    /// manager keeps each non-empty outline under the content hash of the
    /// file it was asked for and answers the same file again from it while
    /// the bytes and the server session are unchanged (see
    /// `lsp::Manager::document_symbols`); the other servers are asked every
    /// time. An empty outline or an error is never kept. The recipes walk no
    /// `lsp.live` and run the same under both values.
    pub outline_cache: bool,
    /// `false` (default): `lsp.live` asks a symbol node for its definition and
    /// its references. `true`: it also asks `textDocument/implementation` and
    /// reports the answers as `implements` edges (one more external call per
    /// symbol node). The recipes walk no `lsp.live` and run the same under both
    /// values.
    pub implementations: bool,
    /// `false` (default): `lsp.live` asks a symbol node for its definition and
    /// its references (and, under `implementations`, its implementations).
    /// `true`: it also asks the call hierarchy (`textDocument/
    /// prepareCallHierarchy`, then `callHierarchy/incomingCalls` and
    /// `callHierarchy/outgoingCalls`: up to three more external calls per
    /// symbol node) and reports the callers and callees as `calls_or_refs`
    /// edges. A server that refuses a request costs the other edges nothing.
    /// The recipes walk no `lsp.live` and run the same under both values.
    pub call_hierarchy: bool,
    /// `"off"` (default): a language server is told nothing about edits after
    /// it starts. `"watch"`: a long-lived manager (the MCP and REST server's,
    /// the one `lci traverse` builds under `sessions = "shared"`) watches each
    /// workspace that has a live session and sends every source-file change
    /// to its servers (see [`LspSync`]). A per-call manager never watches:
    /// under `sessions = "per_call"` it changes nothing for `lsp.live`. The
    /// recipes walk no `lsp.live` and run the same under both values.
    pub sync: LspSync,
    /// `"scip"` (default): `lsp.live` places a symbol only where the SCIP
    /// index has a node for it, so it asks nothing about a symbol the index
    /// lacks and, under `scip.mode = "off"`, nothing at all. `"outline"`: the
    /// index's table first, then the server's live outline of a file
    /// (`textDocument/documentSymbol`, one external call per distinct file it
    /// places a symbol in or expands) for what the index lacks or misplaces
    /// (see [`LspPlaces`]). The recipes walk no `lsp.live` and run the same
    /// under both values.
    pub places: LspPlaces,
    /// `false` (default): every call probes and starts a language server as it
    /// is asked, so a server that is not there is probed again on every call.
    /// `true`: a provider whose version probe failed, or whose server could not
    /// be started (it could not be spawned, exited, or broke the protocol), is
    /// left alone for 30 seconds: the next probe or start of that provider is
    /// refused at once, with a note that names the window (see
    /// `lsp::LspBreakers`). The windows are per provider and shared by the
    /// process's managers (the MCP and REST server's and each call's);
    /// `lci traverse` under `sessions = "per_call"` builds no shared manager
    /// and so has no window. A
    /// server that is already running is still asked, a start that timed out
    /// opens no window (it may only be slow), and `/ready` still probes the
    /// binaries. The recipes walk no `lsp.live` and run the same under both
    /// values.
    pub breaker: bool,
}

/// Dotted keys left out of the profile besides [`machine_keys`]. A key naming
/// a section (`system_one`) leaves out everything under it.
///
/// Some of these change answers (the traverse priors and tie-break files, the
/// language-server switches and budget, `rust_analyzer_trusted`, the embedding
/// identity) but are per machine or per file, so they are stamped beside the
/// profile hash instead of inside it.
pub const EXCLUDED_KEYS: &[&str] = &[
    "embedding_api_key_env",
    "reranker_api_key_env",
    "embedding_timeout_seconds",
    "reranker_timeout_seconds",
    "readiness_timeout_seconds",
    "lsp_timeout_seconds",
    "lsp_start_timeout_seconds",
    "lsp_search_budget_ms",
    "lsp_idle_timeout_seconds",
    "lsp_stuck_seconds",
    "watch_poll_milliseconds",
    "watch_debounce_milliseconds",
    "index.stale_check_interval_seconds",
    "index.wait_for_existing_job_seconds",
    "traverse.deadline_ms",
    "embedding_batch_size",
    "traverse.scip_index",
    "traverse.priors",
    "traverse.tie_break",
    "rust_analyzer_trusted",
    "system_one",
];

/// Every dotted key the profile leaves out: [`machine_keys`] then
/// [`EXCLUDED_KEYS`].
pub fn excluded_keys() -> Vec<String> {
    let mut keys = machine_keys();
    keys.extend(EXCLUDED_KEYS.iter().map(|key| (*key).to_owned()));
    keys
}

/// The result-affecting profile of one resolved configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct RunProfile {
    /// The profile as a JSON value.
    pub value: Value,
    /// `value` as compact JSON with every object's keys sorted. Floats are
    /// the shortest round-trip `f64`; an `f32` setting is widened first, so
    /// `[traverse] tau = 0.8` reads `0.800000011920929`. Deterministic, not
    /// pretty.
    pub canonical: String,
    /// Lowercase hex SHA-256 of `canonical`.
    pub hash: String,
    /// The dotted path of every leaf of `value`, sorted. An array, a scalar,
    /// `null` and an empty object are leaves.
    pub keys: Vec<String>,
}

impl Config {
    /// The resolved result-affecting profile (see the module comment).
    pub fn run_profile(&self) -> RunProfile {
        let mut value = self.profile_projection();
        for key in excluded_keys() {
            remove_dotted(&mut value, &key);
        }
        let canonical = canonical_json(&value);
        RunProfile {
            hash: crate::workspace::hash(&canonical),
            keys: leaf_paths(&value),
            canonical,
            value,
        }
    }

    /// The whole configuration as JSON, with every section that
    /// serialization leaves out while it is the default put back.
    fn profile_projection(&self) -> Value {
        let mut value = serde_json::to_value(self)
            .expect("a Config serializes to JSON: every map key is a string");
        if let Value::Object(map) = &mut value {
            // Every section Config skips when serializing must be listed
            // here, or its keys never reach the profile.
            for (key, section) in [
                ("traverse", section_value(&self.traverse)),
                ("docs_lane", section_value(&self.docs_lane)),
                ("recipes", section_value(&self.recipes)),
                ("compose", section_value(&self.compose)),
                ("rescorer", section_value(&self.rescorer)),
                ("search_profiles", section_value(&self.search_profiles)),
                ("run_profile", section_value(&self.run_profile)),
            ] {
                map.insert(key.to_owned(), section);
            }
        }
        value
    }
}

fn section_value(section: &impl Serialize) -> Value {
    serde_json::to_value(section).expect("a config section serializes to JSON")
}

/// Remove the leaf or section at a dotted key, then every parent object that
/// the removal left empty. A key that is not there is ignored.
fn remove_dotted(value: &mut Value, key: &str) {
    // Returns whether the key was there; a parent emptied by the removal is
    // pruned, an already empty one is kept.
    fn remove(map: &mut Map<String, Value>, parts: &[&str]) -> bool {
        let Some((head, rest)) = parts.split_first() else {
            return false;
        };
        if rest.is_empty() {
            return map.remove(*head).is_some();
        }
        let mut removed = false;
        if let Some(Value::Object(child)) = map.get_mut(*head) {
            removed = remove(child, rest);
            if removed && child.is_empty() {
                map.remove(*head);
            }
        }
        removed
    }
    if let Value::Object(map) = value {
        let parts: Vec<&str> = key.split('.').collect();
        remove(map, &parts);
    }
}

/// Compact JSON with every object's keys in sorted order, whatever order the
/// `Map` keeps them in.
fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Object(map) => {
            let mut entries: Vec<(&String, &Value)> = map.iter().collect();
            entries.sort_by(|a, b| a.0.cmp(b.0));
            out.push('{');
            for (index, (key, child)) in entries.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(child, out);
            }
            out.push('}');
        }
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

/// The sorted dotted path of every leaf of a JSON object.
fn leaf_paths(value: &Value) -> Vec<String> {
    fn walk(value: &Value, prefix: &str, out: &mut Vec<String>) {
        match value {
            Value::Object(map) if !map.is_empty() => {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    walk(child, &path, out);
                }
            }
            _ => out.push(prefix.to_owned()),
        }
    }
    let mut out = Vec::new();
    walk(value, "", &mut out);
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{IndexFreshness, McpResponse, SearchProfile, TraverseFusion, TraversePolicy},
        provider::SystemOneConfig,
    };
    use serde_json::json;
    use std::path::PathBuf;

    fn base_toml() -> String {
        format!("data_dir = '{}'\n", std::env::temp_dir().display())
    }

    /// A configuration with every optional key set and no value at its
    /// default, so every leaf of the projection exists.
    #[allow(clippy::field_reassign_with_default)]
    fn full_config() -> Config {
        let mut config = Config::default();
        config.embedding_url = "http://example.test/v1".into();
        config.embedding_model = "m".into();
        config.reranker_url = "http://example.test/rerank".into();
        config.reranker_model = "r".into();
        config.embedding_api_key_env = Some("EMB_KEY".into());
        config.reranker_api_key_env = Some("RR_KEY".into());
        config.data_dir = PathBuf::from("/elsewhere");
        config.port = 9001;
        config.default_top_k = 9;
        config.embedding_batch_size = 3;
        config.embedding_timeout_seconds = 7;
        config.reranker_timeout_seconds = 8;
        config.readiness_timeout_seconds = 9;
        config.ripgrep_path = "rg2".into();
        config.rust_analyzer_path = "ra2".into();
        config.rust_analyzer_trusted = true;
        config.lsp_timeout_seconds = 11;
        config.lsp_start_timeout_seconds = 19;
        config.lsp_search_budget_ms = 12;
        config.lsp_idle_timeout_seconds = 13;
        config.lsp_stuck_seconds = 14;
        config.watch_poll_milliseconds = 15;
        config.watch_debounce_milliseconds = 16;
        config.csharp.path = Some("cs".into());
        config.typescript.path = Some("ts".into());
        config.python.path = Some("py".into());
        config.go.path = Some("go".into());
        config.java.path = Some("java".into());
        config.clangd.path = Some("clangd".into());
        config.index.freshness = IndexFreshness::Manual;
        config.index.stale_check_interval_seconds = 17;
        config.index.wait_for_existing_job_seconds = 18;
        config.index.prose = true;
        config.channels.lsp = false;
        config.fusion.lexical_weight = 2.0;
        config.docs_lane.top_k = 3;
        config.search_profiles.insert(
            "fast".into(),
            SearchProfile {
                reranker: Some(false),
                response: Some(McpResponse::Compact),
                compact_max_hit_lines: Some(5),
                ..SearchProfile::default()
            },
        );
        config.traverse.enabled = true;
        config.traverse.scip_index = "other.scip".into();
        config.traverse.deadline_ms = 19;
        config.traverse.max_depth = 3;
        config.run_profile.traverse.map_freshness = true;
        config.run_profile.traverse.card_paths = true;
        config.run_profile.traverse.answer_labels = true;
        config.traverse.fusion = TraverseFusion::Rrf;
        config.traverse.max_model_calls = Some(4);
        config.traverse.priors = Some("priors.json".into());
        config.traverse.question_type = Some("flow".into());
        config.traverse.tie_break = Some("tie.json".into());
        config.traverse.policies.insert(
            "p".into(),
            TraversePolicy {
                walk: Some(true),
                ..TraversePolicy::default()
            },
        );
        config.recipes.enabled = true;
        config.compose.enabled = true;
        config.system_one = Some(SystemOneConfig::default());
        config.rescorer = crate::config::Rescorer::SystemOne;
        config.run_profile.walk.groups = crate::config::WalkGroups::Claim;
        config.run_profile.walk.grep_corroboration = 0.5;
        config.run_profile.walk.pricing = crate::config::WalkPricing::Relative;
        config.run_profile.scip.mode = crate::config::profile::ScipMode::Authoritative;
        config.run_profile.lsp.sessions = LspSessions::PerCall;
        config.run_profile.freshness.verify_on_read = true;
        config.run_profile.freshness.stat_gate = true;
        config.run_profile.lsp.outline_cache = true;
        config.run_profile.lsp.implementations = true;
        config.run_profile.lsp.call_hierarchy = true;
        config.run_profile.seeds.bm25 = true;
        config.run_profile.seeds.literal = true;
        config.run_profile.grep.structural = true;
        config.run_profile.lsp.sync = LspSync::Watch;
        config.run_profile.lsp.places = LspPlaces::Outline;
        config
    }

    #[test]
    #[allow(clippy::field_reassign_with_default)]
    fn the_same_profile_hashes_the_same_whatever_the_machine_values() {
        let a = Config::default().run_profile();
        let mut other = Config::default();
        other.data_dir = PathBuf::from("/elsewhere");
        other.port = 9001;
        other.embedding_url = "http://example.test/v1".into();
        other.embedding_model = "another".into();
        other.ripgrep_path = "rg2".into();
        other.csharp.path = Some("cs".into());
        other.typescript.args = vec!["--x".into()];
        other.embedding_api_key_env = Some("EMB_KEY".into());
        other.embedding_timeout_seconds = 7;
        other.embedding_batch_size = 3;
        other.lsp_search_budget_ms = 12;
        other.rust_analyzer_trusted = true;
        other.index.stale_check_interval_seconds = 17;
        other.traverse.deadline_ms = 19;
        other.traverse.scip_index = "other.scip".into();
        other.traverse.priors = Some("priors.json".into());
        other.system_one = Some(SystemOneConfig::default());
        let b = other.run_profile();
        assert_eq!(a.hash, b.hash);
        assert_eq!(a.canonical, b.canonical);
        assert_eq!(a.hash.len(), 64);
        assert!(
            a.hash
                .chars()
                .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
        );
    }

    #[test]
    fn a_result_affecting_key_changes_the_hash() {
        let base = Config::default().run_profile().hash;
        let mut reranker_off = Config::default();
        reranker_off.channels.reranker = false;
        assert_ne!(reranker_off.run_profile().hash, base);
        let mut shallow = Config::default();
        shallow.traverse.max_depth = 3;
        assert_ne!(shallow.run_profile().hash, base);
        assert_ne!(shallow.run_profile().hash, reranker_off.run_profile().hash);
    }

    #[test]
    fn walk_groups_and_grep_corroboration_change_the_profile_hash() {
        let base = Config::default().run_profile();
        let mut claim = Config::default();
        claim.run_profile.walk.groups = WalkGroups::Claim;
        let mut discount = Config::default();
        discount.run_profile.walk.grep_corroboration = 0.5;
        let hashes = [
            base.hash.clone(),
            claim.run_profile().hash,
            discount.run_profile().hash,
        ];
        for (i, a) in hashes.iter().enumerate() {
            for b in &hashes[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Both are leaves of the profile, never excluded.
        for key in [
            "run_profile.walk.groups",
            "run_profile.walk.grep_corroboration",
        ] {
            assert!(base.keys.iter().any(|k| k == key), "{key}");
        }
    }

    #[test]
    fn walk_pricing_is_rank_by_default_and_changes_the_profile_hash() {
        let base = Config::default().run_profile();
        assert_eq!(WalkProfile::default().pricing, WalkPricing::Rank);
        assert!(base.canonical.contains(r#""pricing":"rank""#));
        let mut relative = Config::default();
        relative.run_profile.walk.pricing = WalkPricing::Relative;
        let relative = relative.run_profile();
        assert!(relative.canonical.contains(r#""pricing":"relative""#));
        assert_ne!(base.hash, relative.hash);
        assert!(base.keys.iter().any(|k| k == "run_profile.walk.pricing"));
        assert_eq!("relative".parse::<WalkPricing>(), Ok(WalkPricing::Relative));
        assert!("rank".parse::<WalkPricing>().is_ok());
        assert!("linear".parse::<WalkPricing>().is_err());
        // The key lives under `[run_profile.walk]`, not `[run_profile]`.
        let error = toml::from_str::<Config>(&format!(
            "{}[run_profile]\npricing = \"relative\"\n",
            base_toml()
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("pricing"), "{error}");
        let read: Config = toml::from_str(&format!(
            "{}[run_profile.walk]\npricing = \"relative\"\n",
            base_toml()
        ))
        .unwrap();
        read.validate().unwrap();
        assert_eq!(read.run_profile.walk.pricing, WalkPricing::Relative);
    }

    #[test]
    fn traverse_fusion_log_odds_changes_the_profile_hash() {
        let base = Config::default().run_profile();
        assert!(base.canonical.contains(r#""fusion":"boost""#));
        let mut rrf = Config::default();
        rrf.traverse.fusion = TraverseFusion::Rrf;
        let mut log_odds = Config::default();
        log_odds.traverse.fusion = TraverseFusion::LogOdds;
        let log_odds = log_odds.run_profile();
        assert!(log_odds.canonical.contains(r#""fusion":"log_odds""#));
        assert_ne!(base.hash, log_odds.hash);
        assert_ne!(rrf.run_profile().hash, log_odds.hash);
        assert!(base.keys.iter().any(|k| k == "traverse.fusion"));
    }

    #[test]
    fn scip_mode_changes_the_profile_hash() {
        let base = Config::default().run_profile();
        assert!(base.canonical.contains(r#""scip":{"mode":"cache"}"#));
        let hashes: Vec<String> = [ScipMode::Cache, ScipMode::Authoritative, ScipMode::Off]
            .into_iter()
            .map(|mode| {
                let mut config = Config::default();
                config.run_profile.scip.mode = mode;
                config.run_profile().hash
            })
            .collect();
        assert_eq!(hashes[0], base.hash);
        assert_ne!(hashes[0], hashes[1]);
        assert_ne!(hashes[0], hashes[2]);
        assert_ne!(hashes[1], hashes[2]);
        assert!(base.keys.iter().any(|key| key == "run_profile.scip.mode"));
        // The file reads the key.
        let read: Config = toml::from_str(&format!(
            "{}[run_profile.scip]
mode = \"off\"
",
            base_toml()
        ))
        .unwrap();
        read.validate().unwrap();
        assert_eq!(read.run_profile.scip.mode, ScipMode::Off);
        assert_eq!("cache".parse::<ScipMode>(), Ok(ScipMode::Cache));
    }

    #[test]
    fn scip_mode_refuses_an_unknown_value() {
        for bad in [
            "mode = \"Off\"",
            "mode = \"\"",
            "mode = \"on\"",
            "mode = true",
            "nope = 1",
        ] {
            let text = format!(
                "{}[run_profile.scip]
{bad}
",
                base_toml()
            );
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
        let error = toml::from_str::<Config>(&format!(
            "{}[run_profile.scip]
mode = \"sometimes\"
",
            base_toml()
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("sometimes"), "{error}");
        assert!("sometimes".parse::<ScipMode>().is_err());
    }

    #[test]
    fn verify_on_read_is_off_by_default_and_is_part_of_the_profile() {
        let off = Config::default();
        assert!(!off.run_profile.freshness.verify_on_read);
        let text = format!(
            "{}[run_profile.freshness]\nverify_on_read = true\n",
            base_toml()
        );
        let on: Config = toml::from_str(&text).unwrap();
        on.validate().unwrap();
        assert!(on.run_profile.freshness.verify_on_read);
        assert_eq!(
            serde_json::to_value(&on).unwrap()["run_profile"]["freshness"]["verify_on_read"],
            true
        );
        // It changes what a traverse answer and a search report hold, so it
        // changes the hash.
        let (off, on) = (off.run_profile(), on.run_profile());
        assert_eq!(
            off.value["run_profile"]["freshness"]["verify_on_read"],
            false
        );
        assert_eq!(on.value["run_profile"]["freshness"]["verify_on_read"], true);
        assert!(
            off.keys
                .iter()
                .any(|key| key == "run_profile.freshness.verify_on_read")
        );
        assert_ne!(off.hash, on.hash);
        for bad in [
            "[run_profile.freshness]\nverify_on_read = \"yes\"\n",
            "[run_profile.freshness]\nfoo = 1\n",
            "[run_profile]\nverify_on_read = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
        // The misplaced key is rejected by name.
        let error = toml::from_str::<Config>(&format!(
            "{}[run_profile]\nverify_on_read = true\n",
            base_toml()
        ))
        .unwrap_err()
        .to_string();
        assert!(error.contains("verify_on_read"), "{error}");
    }

    #[test]
    fn seeds_bm25_is_off_by_default_and_is_part_of_the_profile() {
        let off = Config::default();
        assert!(!off.run_profile.seeds.bm25);
        let on: Config = toml::from_str(&format!(
            "{}[run_profile.seeds]\nbm25 = true\n",
            base_toml()
        ))
        .unwrap();
        on.validate().unwrap();
        assert!(on.run_profile.seeds.bm25);
        assert_eq!(
            serde_json::to_value(&on).unwrap()["run_profile"]["seeds"]["bm25"],
            true
        );
        let (off, on) = (off.run_profile(), on.run_profile());
        assert_eq!(off.value["run_profile"]["seeds"]["bm25"], false);
        assert_eq!(on.value["run_profile"]["seeds"]["bm25"], true);
        assert!(off.keys.iter().any(|key| key == "run_profile.seeds.bm25"));
        assert_ne!(off.hash, on.hash);
        // Spelling out the default is the default.
        let spelled: Config = toml::from_str(&format!(
            "{}[run_profile.seeds]\nbm25 = false\n",
            base_toml()
        ))
        .unwrap();
        assert_eq!(spelled.run_profile().hash, off.hash);
        for bad in [
            "[run_profile.seeds]\nbm25 = \"yes\"\n",
            "[run_profile.seeds]\nbm25 = 1\n",
            "[run_profile.seeds]\nfoo = 1\n",
            "[run_profile]\nbm25 = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn grep_structural_is_off_by_default_and_is_part_of_the_profile() {
        let off = Config::default();
        assert!(!off.run_profile.grep.structural);
        let on: Config = toml::from_str(&format!(
            "{}[run_profile.grep]\nstructural = true\n",
            base_toml()
        ))
        .unwrap();
        on.validate().unwrap();
        assert!(on.run_profile.grep.structural);
        assert!(!on.run_profile.seeds.bm25 && !on.run_profile.seeds.literal);
        assert_eq!(
            serde_json::to_value(&on).unwrap()["run_profile"]["grep"]["structural"],
            true
        );
        let (off, on) = (off.run_profile(), on.run_profile());
        assert_eq!(off.value["run_profile"]["grep"]["structural"], false);
        assert_eq!(on.value["run_profile"]["grep"]["structural"], true);
        assert!(
            off.keys
                .iter()
                .any(|key| key == "run_profile.grep.structural")
        );
        assert_ne!(off.hash, on.hash);
        // Spelling out the default is the default.
        let spelled: Config = toml::from_str(&format!(
            "{}[run_profile.grep]\nstructural = false\n",
            base_toml()
        ))
        .unwrap();
        assert_eq!(spelled.run_profile().hash, off.hash);
        for bad in [
            "[run_profile.grep]\nstructural = \"yes\"\n",
            "[run_profile.grep]\nstructural = 1\n",
            "[run_profile.grep]\nfoo = 1\n",
            "[run_profile]\nstructural = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn seeds_literal_is_off_by_default_and_is_part_of_the_profile() {
        let off = Config::default();
        assert!(!off.run_profile.seeds.literal);
        let on: Config = toml::from_str(&format!(
            "{}[run_profile.seeds]\nliteral = true\n",
            base_toml()
        ))
        .unwrap();
        on.validate().unwrap();
        assert!(on.run_profile.seeds.literal);
        assert!(!on.run_profile.seeds.bm25);
        assert_eq!(
            serde_json::to_value(&on).unwrap()["run_profile"]["seeds"]["literal"],
            true
        );
        let (off, on) = (off.run_profile(), on.run_profile());
        assert_eq!(off.value["run_profile"]["seeds"]["literal"], false);
        assert_eq!(on.value["run_profile"]["seeds"]["literal"], true);
        assert!(
            off.keys
                .iter()
                .any(|key| key == "run_profile.seeds.literal")
        );
        assert_ne!(off.hash, on.hash);
        // Spelling out the default is the default.
        let spelled: Config = toml::from_str(&format!(
            "{}[run_profile.seeds]\nliteral = false\n",
            base_toml()
        ))
        .unwrap();
        assert_eq!(spelled.run_profile().hash, off.hash);
        for bad in [
            "[run_profile.seeds]\nliteral = \"yes\"\n",
            "[run_profile.seeds]\nliteral = 1\n",
            "[run_profile]\nliteral = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn stat_gate_is_off_by_default_and_is_part_of_the_profile() {
        let off = Config::default();
        assert!(!off.run_profile.freshness.stat_gate);
        let text = format!("{}[run_profile.freshness]\nstat_gate = true\n", base_toml());
        let on: Config = toml::from_str(&text).unwrap();
        on.validate().unwrap();
        assert!(on.run_profile.freshness.stat_gate);
        // The key is independent of `verify_on_read`.
        assert!(!on.run_profile.freshness.verify_on_read);
        // It changes what ran (the cost, and the same-stamp gap), so it
        // changes the hash.
        let (off, on) = (off.run_profile(), on.run_profile());
        assert_eq!(off.value["run_profile"]["freshness"]["stat_gate"], false);
        assert_eq!(on.value["run_profile"]["freshness"]["stat_gate"], true);
        assert!(
            off.keys
                .iter()
                .any(|key| key == "run_profile.freshness.stat_gate")
        );
        assert_ne!(off.hash, on.hash);
        for bad in [
            "[run_profile.freshness]\nstat_gate = \"yes\"\n",
            "[run_profile]\nstat_gate = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn canonical_json_has_sorted_keys_and_no_whitespace() {
        let mixed = json!({"b": 1, "a": [{"d": 1, "c": "x y"}, 2.5], "e": {}});
        assert_eq!(
            canonical_json(&mixed),
            r#"{"a":[{"c":"x y","d":1},2.5],"b":1,"e":{}}"#
        );
        // The hash function itself, on a fixed string (independent of the key set).
        assert_eq!(
            crate::workspace::hash(r#"{"a":[{"c":"x y","d":1},2.5],"b":1,"e":{}}"#),
            "8f0efc09ffcd537ac704f59b6ef31bb890f5945e5b1f289cd30efa0b0102cf78"
        );
        let profile = Config::default().run_profile();
        assert!(profile.canonical.starts_with(r#"{""#));
        assert!(profile.canonical.ends_with('}'));
        assert!(!profile.canonical.contains('\n'));
        assert!(profile.canonical.contains(r#""tau":0.800000011920929"#));
        assert_eq!(
            serde_json::from_str::<Value>(&profile.canonical).unwrap(),
            profile.value
        );
        let mut sorted = profile.keys.clone();
        sorted.sort();
        assert_eq!(profile.keys, sorted);
    }

    #[test]
    fn every_config_leaf_is_in_the_profile_or_excluded_by_name() {
        let config = full_config();
        let all = leaf_paths(&config.profile_projection());
        let profile = config.run_profile();
        let excluded = excluded_keys();
        // An exclusion names a leaf or a section, and exists in a full
        // configuration: a renamed or removed key must be dropped from the
        // deny-list.
        for key in &excluded {
            assert!(
                all.iter()
                    .any(|leaf| leaf == key || leaf.starts_with(&format!("{key}."))),
                "the deny-list names {key}, which no Config leaf has"
            );
        }
        let is_excluded = |leaf: &String| {
            excluded
                .iter()
                .any(|key| leaf == key || leaf.starts_with(&format!("{key}.")))
        };
        // Every leaf is exactly one of: in the profile, excluded.
        for leaf in &all {
            assert_ne!(
                profile.keys.contains(leaf),
                is_excluded(leaf),
                "{leaf} must be in the profile or excluded, not both or neither"
            );
        }
        for key in &profile.keys {
            assert!(all.contains(key), "{key} is not a Config leaf");
        }
        // A new key is classified here (profile or deny-list) and set in
        // `full_config`. The golden shows a new key only if the default
        // profile has it or `full_config` sets it: an optional key that is
        // unset in `full_config` and skipped while default (e.g.
        // `traverse.policies`) is not visible there.
        // Spot checks of the classification.
        for key in [
            "default_top_k",
            "rrf_k",
            "channels.reranker",
            "fusion.max_chunks_per_file",
            "docs_lane.top_k",
            "search_profiles.fast.reranker",
            "traverse.max_depth",
            "traverse.policies.p.walk",
            "recipes.enabled",
            "rescorer",
            "run_profile.walk.groups",
            "run_profile.walk.grep_corroboration",
            "run_profile.walk.pricing",
            "run_profile.traverse.map_freshness",
            "run_profile.traverse.answer_labels",
            "run_profile.traverse.card_paths",
            "run_profile.scip.mode",
            "run_profile.lsp.sessions",
            "run_profile.freshness.verify_on_read",
            "run_profile.freshness.stat_gate",
            "run_profile.lsp.outline_cache",
            "run_profile.lsp.implementations",
            "run_profile.lsp.call_hierarchy",
            "run_profile.seeds.bm25",
            "run_profile.seeds.literal",
            "run_profile.grep.structural",
            "run_profile.lsp.sync",
            "run_profile.lsp.places",
            "run_profile.lsp.breaker",
        ] {
            assert!(profile.keys.iter().any(|k| k == key), "{key} must be in");
        }
        for key in ["port", "data_dir", "traverse.deadline_ms", "system_one.url"] {
            assert!(!profile.keys.iter().any(|k| k == key), "{key} must be out");
        }
    }

    #[test]
    fn an_unknown_run_profile_key_is_rejected_by_name() {
        let error = toml::from_str::<Config>(&format!("{}[run_profile]\nfoo = 1\n", base_toml()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("foo"), "{error}");
        let empty: Config = toml::from_str(&format!("{}[run_profile]\n", base_toml())).unwrap();
        empty.validate().unwrap();
        assert_eq!(empty.run_profile, RunProfileConfig::default());
        assert_eq!(
            empty.run_profile().hash,
            Config::default().run_profile().hash
        );
    }

    #[test]
    fn map_freshness_is_off_by_default_and_is_part_of_the_profile() {
        let off = Config::default();
        assert!(!off.run_profile.traverse.map_freshness);
        let text = format!(
            "{}[run_profile.traverse]\nmap_freshness = true\n",
            base_toml()
        );
        let on: Config = toml::from_str(&text).unwrap();
        on.validate().unwrap();
        assert!(on.run_profile.traverse.map_freshness);
        assert_eq!(
            serde_json::to_value(&on).unwrap()["run_profile"]["traverse"]["map_freshness"],
            true
        );
        // It changes what a traverse answer holds, so it changes the hash.
        let (off, on) = (off.run_profile(), on.run_profile());
        assert_eq!(off.value["run_profile"]["traverse"]["map_freshness"], false);
        assert_eq!(on.value["run_profile"]["traverse"]["map_freshness"], true);
        assert!(
            off.keys
                .iter()
                .any(|key| key == "run_profile.traverse.map_freshness")
        );
        assert_ne!(off.hash, on.hash);
        for bad in [
            "[run_profile.traverse]\nmap_freshness = \"yes\"\n",
            "[run_profile.traverse]\nfoo = 1\n",
            "[run_profile]\nmap_freshness = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn traverse_card_paths_is_off_by_default_and_changes_the_profile_hash() {
        let off = Config::default();
        assert!(!off.run_profile.traverse.card_paths);
        let text = format!("{}[run_profile.traverse]\ncard_paths = true\n", base_toml());
        let on: Config = toml::from_str(&text).unwrap();
        on.validate().unwrap();
        assert!(on.run_profile.traverse.card_paths);
        // It changes what a traverse answer holds, so it changes the hash.
        let (off, on) = (off.run_profile(), on.run_profile());
        assert_eq!(off.value["run_profile"]["traverse"]["card_paths"], false);
        assert_eq!(on.value["run_profile"]["traverse"]["card_paths"], true);
        assert!(
            off.keys
                .iter()
                .any(|key| key == "run_profile.traverse.card_paths")
        );
        assert_ne!(off.hash, on.hash);
        for bad in [
            "[run_profile.traverse]\ncard_paths = \"yes\"\n",
            "[run_profile]\ncard_paths = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn traverse_answer_labels_is_off_by_default_and_changes_the_profile_hash() {
        let off = Config::default();
        assert!(!off.run_profile.traverse.answer_labels);
        let text = format!(
            "{}[run_profile.traverse]\nanswer_labels = true\n",
            base_toml()
        );
        let on: Config = toml::from_str(&text).unwrap();
        on.validate().unwrap();
        assert!(on.run_profile.traverse.answer_labels);
        // It changes what a traverse answer holds, so it changes the hash.
        let (off, on) = (off.run_profile(), on.run_profile());
        assert_eq!(off.value["run_profile"]["traverse"]["answer_labels"], false);
        assert_eq!(on.value["run_profile"]["traverse"]["answer_labels"], true);
        assert!(
            off.keys
                .iter()
                .any(|key| key == "run_profile.traverse.answer_labels")
        );
        assert_ne!(off.hash, on.hash);
        for bad in [
            "[run_profile.traverse]\nanswer_labels = \"yes\"\n",
            "[run_profile]\nanswer_labels = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn lsp_sessions_defaults_to_shared_and_per_call_changes_the_profile_hash() {
        let default = Config::default();
        assert_eq!(default.run_profile.lsp.sessions, LspSessions::Shared);
        assert!(!default.run_profile.lsp.outline_cache);
        let text = format!(
            "{}[run_profile.lsp]\nsessions = \"per_call\"\n",
            base_toml()
        );
        let per_call: Config = toml::from_str(&text).unwrap();
        per_call.validate().unwrap();
        assert_eq!(per_call.run_profile.lsp.sessions, LspSessions::PerCall);
        assert_eq!(
            serde_json::to_value(&per_call).unwrap()["run_profile"]["lsp"]["sessions"],
            "per_call"
        );
        let cached: Config = toml::from_str(&format!(
            "{}[run_profile.lsp]\noutline_cache = true\n",
            base_toml()
        ))
        .unwrap();
        cached.validate().unwrap();
        assert!(cached.run_profile.lsp.outline_cache);
        assert_eq!(cached.run_profile.lsp.sessions, LspSessions::Shared);
        let (default, per_call, cached) = (
            default.run_profile(),
            per_call.run_profile(),
            cached.run_profile(),
        );
        assert_eq!(default.value["run_profile"]["lsp"]["sessions"], "shared");
        assert_eq!(per_call.value["run_profile"]["lsp"]["sessions"], "per_call");
        assert_eq!(default.value["run_profile"]["lsp"]["outline_cache"], false);
        assert_eq!(cached.value["run_profile"]["lsp"]["outline_cache"], true);
        for key in ["run_profile.lsp.sessions", "run_profile.lsp.outline_cache"] {
            assert!(default.keys.iter().any(|k| k == key), "{key}");
        }
        assert_ne!(default.hash, per_call.hash);
        assert_ne!(default.hash, cached.hash);
        assert_ne!(per_call.hash, cached.hash);
        // Spelling out the defaults is the default.
        let spelled: Config = toml::from_str(&format!(
            "{}[run_profile.lsp]\nsessions = \"shared\"\noutline_cache = false\n",
            base_toml()
        ))
        .unwrap();
        assert_eq!(spelled.run_profile().hash, default.hash);
        for bad in [
            "[run_profile.lsp]\nsessions = \"always\"\n",
            "[run_profile.lsp]\nsessions = true\n",
            "[run_profile.lsp]\noutline_cache = \"yes\"\n",
            "[run_profile.lsp]\noutline_cache = 1\n",
            "[run_profile.lsp]\nfoo = 1\n",
            "[run_profile]\nsessions = \"shared\"\n",
            "[run_profile]\noutline_cache = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
        assert_eq!(LspSessions::Shared.as_str(), "shared");
        assert_eq!("per_call".parse::<LspSessions>(), Ok(LspSessions::PerCall));
        assert!("per-call".parse::<LspSessions>().is_err());
    }

    /// The golden's value for a leaf that a full configuration has and the
    /// default one does not (an optional key left unset).
    const UNSET: &str = "<unset by default>";

    /// `key = value` for every leaf of the profile in `full_config`, sorted
    /// by key: the default value where the default profile has the leaf,
    /// [`UNSET`] where only an optional key of the full configuration does.
    fn golden_text() -> String {
        fn leaf_values(value: &Value, prefix: &str, out: &mut Vec<(String, String)>) {
            match value {
                Value::Object(map) if !map.is_empty() => {
                    for (key, child) in map {
                        let path = if prefix.is_empty() {
                            key.clone()
                        } else {
                            format!("{prefix}.{key}")
                        };
                        leaf_values(child, &path, out);
                    }
                }
                _ => out.push((prefix.to_owned(), canonical_json(value))),
            }
        }
        let mut leaves = std::collections::BTreeMap::new();
        for key in full_config().run_profile().keys {
            leaves.insert(key, UNSET.to_owned());
        }
        let mut defaults = Vec::new();
        leaf_values(&Config::default().run_profile().value, "", &mut defaults);
        leaves.extend(defaults);
        let mut text = String::from(
            "# The default run profile: one line per leaf of `Config::run_profile()`, sorted\n\
             # by key path, with the leaf's default value. Regenerate with\n\
             # `BLESS_GOLDEN=1 cargo test -p lci-core --lib config::profile` and review the diff.\n",
        );
        for (key, value) in leaves {
            text.push_str(&format!("{key} = {value}\n"));
        }
        text.push_str(&format!(
            "# hash = {}\n",
            Config::default().run_profile().hash
        ));
        text
    }

    /// A leaf added, removed or re-defaulted shows as `-`/`+` lines.
    fn golden_diff(expected_text: &str, actual_text: &str) -> String {
        let expected: Vec<&str> = expected_text.lines().collect();
        let actual: Vec<&str> = actual_text.lines().collect();
        let mut diff = String::new();
        for line in expected.iter().filter(|line| !actual.contains(line)) {
            diff.push_str(&format!("- {line}\n"));
        }
        for line in actual.iter().filter(|line| !expected.contains(line)) {
            diff.push_str(&format!("+ {line}\n"));
        }
        if diff.is_empty() && expected_text != actual_text {
            // Same lines, different text (a header edit, a reorder, a
            // duplicate): show both in full.
            diff = format!("golden:\n{expected_text}\ncode:\n{actual_text}");
        }
        diff
    }

    #[test]
    fn lsp_request_switches_are_off_by_default_and_change_the_profile_hash() {
        for leaf in ["implementations", "call_hierarchy"] {
            let off = Config::default();
            assert!(!off.run_profile.lsp.implementations);
            assert!(!off.run_profile.lsp.call_hierarchy);
            let text = format!("{}[run_profile.lsp]\n{leaf} = true\n", base_toml());
            let on: Config = toml::from_str(&text).unwrap();
            on.validate().unwrap();
            // Only the named leaf moves; the others keep their defaults.
            let value = serde_json::to_value(&on).unwrap();
            for other in ["implementations", "call_hierarchy"] {
                assert_eq!(
                    value["run_profile"]["lsp"][other],
                    other == leaf,
                    "{leaf} vs {other}"
                );
            }
            assert_eq!(on.run_profile.lsp.sessions, LspSessions::Shared);
            assert!(!on.run_profile.lsp.outline_cache);
            // It changes what an lsp.live answer holds, so it changes the hash.
            let (off, on) = (off.run_profile(), on.run_profile());
            assert_eq!(off.value["run_profile"]["lsp"][leaf], false);
            assert_eq!(on.value["run_profile"]["lsp"][leaf], true);
            let key = format!("run_profile.lsp.{leaf}");
            assert!(off.keys.contains(&key), "{key}");
            assert_ne!(off.hash, on.hash, "{leaf}");
            // Spelling out the default is the default.
            let spelled: Config = toml::from_str(&format!(
                "{}[run_profile.lsp]\n{leaf} = false\n",
                base_toml()
            ))
            .unwrap();
            assert_eq!(spelled.run_profile().hash, off.hash, "{leaf}");
            for bad in [
                format!("[run_profile.lsp]\n{leaf} = \"yes\"\n"),
                format!("[run_profile.lsp]\n{leaf} = 1\n"),
                format!("[run_profile]\n{leaf} = true\n"),
            ] {
                let text = format!("{}{bad}", base_toml());
                assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
            }
        }
        // The two switches are separate leaves of the hash.
        let with = |leaves: &str| -> Config {
            toml::from_str(&format!("{}[run_profile.lsp]\n{leaves}", base_toml())).unwrap()
        };
        let hashes = [
            with("implementations = true\ncall_hierarchy = true\n")
                .run_profile()
                .hash,
            with("implementations = true\n").run_profile().hash,
            with("call_hierarchy = true\n").run_profile().hash,
            Config::default().run_profile().hash,
        ];
        for (i, a) in hashes.iter().enumerate() {
            for b in &hashes[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }

    #[test]
    fn lsp_sync_is_off_by_default_and_is_part_of_the_profile() {
        let off = Config::default();
        assert_eq!(off.run_profile.lsp.sync, LspSync::Off);
        let watch: Config = toml::from_str(&format!(
            "{}[run_profile.lsp]\nsync = \"watch\"\n",
            base_toml()
        ))
        .unwrap();
        watch.validate().unwrap();
        assert_eq!(watch.run_profile.lsp.sync, LspSync::Watch);
        assert_eq!(watch.run_profile.lsp.sessions, LspSessions::Shared);
        assert!(!watch.run_profile.lsp.outline_cache);
        assert_eq!(
            serde_json::to_value(&watch).unwrap()["run_profile"]["lsp"]["sync"],
            "watch"
        );
        let (off, watch) = (off.run_profile(), watch.run_profile());
        assert_eq!(off.value["run_profile"]["lsp"]["sync"], "off");
        assert_eq!(watch.value["run_profile"]["lsp"]["sync"], "watch");
        assert!(off.keys.iter().any(|key| key == "run_profile.lsp.sync"));
        assert_ne!(off.hash, watch.hash);
        let spelled: Config = toml::from_str(&format!(
            "{}[run_profile.lsp]\nsync = \"off\"\n",
            base_toml()
        ))
        .unwrap();
        assert_eq!(spelled.run_profile().hash, off.hash);
        for bad in [
            "[run_profile.lsp]\nsync = \"on\"\n",
            "[run_profile.lsp]\nsync = \"Watch\"\n",
            "[run_profile.lsp]\nsync = true\n",
            "[run_profile]\nsync = \"watch\"\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
        let error =
            toml::from_str::<Config>(&format!("{}[run_profile]\nsync = \"watch\"\n", base_toml()))
                .unwrap_err()
                .to_string();
        assert!(error.contains("sync"), "{error}");
        assert_eq!(LspSync::Watch.as_str(), "watch");
        assert_eq!("off".parse::<LspSync>(), Ok(LspSync::Off));
        assert!("on".parse::<LspSync>().is_err());
    }

    #[test]
    fn lsp_places_is_scip_by_default_and_changes_the_profile_hash() {
        let scip = Config::default();
        assert_eq!(scip.run_profile.lsp.places, LspPlaces::Scip);
        let outline: Config = toml::from_str(&format!(
            "{}[run_profile.lsp]\nplaces = \"outline\"\n",
            base_toml()
        ))
        .unwrap();
        outline.validate().unwrap();
        assert_eq!(outline.run_profile.lsp.places, LspPlaces::Outline);
        // Only the named leaf moves.
        assert_eq!(outline.run_profile.lsp.sessions, LspSessions::Shared);
        assert_eq!(outline.run_profile.lsp.sync, LspSync::Off);
        assert!(!outline.run_profile.lsp.implementations);
        assert_eq!(
            serde_json::to_value(&outline).unwrap()["run_profile"]["lsp"]["places"],
            "outline"
        );
        let (scip, outline) = (scip.run_profile(), outline.run_profile());
        assert_eq!(scip.value["run_profile"]["lsp"]["places"], "scip");
        assert_eq!(outline.value["run_profile"]["lsp"]["places"], "outline");
        assert!(scip.keys.iter().any(|key| key == "run_profile.lsp.places"));
        assert_ne!(scip.hash, outline.hash);
        // Spelling out the default is the default.
        let spelled: Config = toml::from_str(&format!(
            "{}[run_profile.lsp]\nplaces = \"scip\"\n",
            base_toml()
        ))
        .unwrap();
        assert_eq!(spelled.run_profile().hash, scip.hash);
        for bad in [
            "[run_profile.lsp]\nplaces = \"index\"\n",
            "[run_profile.lsp]\nplaces = \"Outline\"\n",
            "[run_profile.lsp]\nplaces = true\n",
            "[run_profile]\nplaces = \"outline\"\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
        assert_eq!(LspPlaces::Outline.as_str(), "outline");
        assert_eq!("scip".parse::<LspPlaces>(), Ok(LspPlaces::Scip));
        assert!("index".parse::<LspPlaces>().is_err());
    }

    #[test]
    fn lsp_breaker_is_off_by_default_and_changes_the_profile_hash() {
        let off = Config::default();
        assert!(!off.run_profile.lsp.breaker);
        let on: Config = toml::from_str(&format!(
            "{}[run_profile.lsp]\nbreaker = true\n",
            base_toml()
        ))
        .unwrap();
        on.validate().unwrap();
        assert!(on.run_profile.lsp.breaker);
        // Only the named leaf moves.
        assert_eq!(on.run_profile.lsp.sessions, LspSessions::Shared);
        assert_eq!(on.run_profile.lsp.sync, LspSync::Off);
        assert_eq!(on.run_profile.lsp.places, LspPlaces::Scip);
        assert!(!on.run_profile.lsp.implementations);
        assert!(!on.run_profile.lsp.call_hierarchy);
        assert_eq!(
            serde_json::to_value(&on).unwrap()["run_profile"]["lsp"]["breaker"],
            true
        );
        let (off, on) = (off.run_profile(), on.run_profile());
        assert_eq!(off.value["run_profile"]["lsp"]["breaker"], false);
        assert_eq!(on.value["run_profile"]["lsp"]["breaker"], true);
        assert!(off.keys.iter().any(|key| key == "run_profile.lsp.breaker"));
        assert_ne!(off.hash, on.hash);
        // Spelling out the default is the default.
        let spelled: Config = toml::from_str(&format!(
            "{}[run_profile.lsp]\nbreaker = false\n",
            base_toml()
        ))
        .unwrap();
        assert_eq!(spelled.run_profile().hash, off.hash);
        for bad in [
            "[run_profile.lsp]\nbreaker = \"yes\"\n",
            "[run_profile.lsp]\nbreaker = 1\n",
            "[run_profile]\nbreaker = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn lsp_implementations_is_off_by_default_and_changes_the_profile_hash() {
        let off = Config::default();
        assert!(!off.run_profile.lsp.implementations);
        let text = format!("{}[run_profile.lsp]\nimplementations = true\n", base_toml());
        let on: Config = toml::from_str(&text).unwrap();
        on.validate().unwrap();
        assert!(on.run_profile.lsp.implementations);
        // The other leaf of the section keeps its default.
        assert_eq!(on.run_profile.lsp.sessions, LspSessions::Shared);
        assert_eq!(
            serde_json::to_value(&on).unwrap()["run_profile"]["lsp"]["implementations"],
            true
        );
        // It changes what an lsp.live answer holds, so it changes the hash.
        let (off, on) = (off.run_profile(), on.run_profile());
        assert_eq!(off.value["run_profile"]["lsp"]["implementations"], false);
        assert_eq!(on.value["run_profile"]["lsp"]["implementations"], true);
        assert!(
            off.keys
                .iter()
                .any(|key| key == "run_profile.lsp.implementations")
        );
        assert_ne!(off.hash, on.hash);
        // Spelling out the default is the default.
        let spelled: Config = toml::from_str(&format!(
            "{}[run_profile.lsp]\nimplementations = false\n",
            base_toml()
        ))
        .unwrap();
        assert_eq!(spelled.run_profile().hash, off.hash);
        for bad in [
            "[run_profile.lsp]\nimplementations = \"yes\"\n",
            "[run_profile.lsp]\nimplementations = 1\n",
            "[run_profile]\nimplementations = true\n",
        ] {
            let text = format!("{}{bad}", base_toml());
            assert!(toml::from_str::<Config>(&text).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_default_profile_matches_the_golden() {
        let actual = golden_text();
        let golden = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/config/golden/run_profile_default.txt");
        if std::env::var_os("BLESS_GOLDEN").is_some() {
            std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
            std::fs::write(&golden, &actual).unwrap();
        }
        let expected = std::fs::read_to_string(&golden)
            .unwrap_or_else(|error| {
                panic!("{}: {error}; run with BLESS_GOLDEN=1", golden.display())
            })
            .replace("\r\n", "\n");
        assert!(
            actual == expected,
            "the default run profile changed (- golden, + code); if the change is meant, \
             run with BLESS_GOLDEN=1 and review the golden's diff:\n{}",
            golden_diff(&expected, &actual)
        );
    }
}
