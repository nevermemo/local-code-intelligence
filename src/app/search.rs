//! Hybrid retrieval: index freshness, semantic/lexical/LSP fan-out,
//! Reciprocal Rank Fusion, and reranking. Each channel and the reranker can
//! be switched off for a search with the `[channels]` configuration, and a
//! named `[search_profiles.<name>]` can override those switches and the
//! rerank window per search (see [`App::search_in_profile`]).

// The retrieval channels wrapped as maps (the T0 bit-for-bit check): unit tests (T0) and the
// `map-equivalence` feature only, so the default build adds no public path.
#[cfg(any(test, feature = "map-equivalence"))]
mod channel_maps;

use super::traverse::seeds::{is_source_path, snapshot_differing};
use super::{App, IndexAction, WorkspaceCoordination, ms};
use crate::{
    config::{ChannelsConfig, FusionConfig, IndexFreshness, Rescorer, SearchSettings},
    filter::{EffectiveFilter, EffectiveFilterReport, FilterRequest, SourceRole},
    lexical,
    lsp::adapter::LspAdapter,
    store::{Hit, Snapshot, Store, bounded_snippet},
    workspace::Workspace,
};
use anyhow::{Context, Result, bail, ensure};
use lci_core::provider::WireCandidate;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Ordering,
    collections::{HashMap, HashSet, hash_map::Entry},
    ffi::OsString,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

thread_local! {
    /// How many `verify_on_read` scans of the tree this thread has started.
    static VERIFY_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// The number of `freshness` scans of the tree (`[run_profile.freshness]
/// verify_on_read`) started on this thread: for tests that a caller which
/// discards the block does not pay for it. It counts only on the thread that
/// polls the search, so a test that reads it needs a current-thread runtime
/// and a search that is not spawned onto another thread; elsewhere it counts
/// nothing and a test would pass without checking. Not part of the API.
#[doc(hidden)]
pub fn verify_scans_on_this_thread() -> usize {
    VERIFY_SCANS.with(std::cell::Cell::get)
}

/// The search warning on a snapshot committed without vectors.
const VECTORLESS_WARNING: &str = "Semantic retrieval unavailable: the index has no vectors (the embedding service was unreachable when it was built)";

fn is_zero(value: &f64) -> bool {
    *value == 0.0
}

#[derive(Debug, Default, Serialize)]
pub struct Timings {
    pub query_embedding_ms: f64,
    pub lancedb_search_ms: f64,
    pub lexical_search_ms: f64,
    pub lsp_search_ms: f64,
    /// The `freshness` block of `[run_profile.freshness] verify_on_read`;
    /// absent from the JSON when it did not run.
    #[serde(skip_serializing_if = "is_zero")]
    pub verify_ms: f64,
    pub fusion_ms: f64,
    pub reranking_ms: f64,
    pub total_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct SearchIndexLifecycle {
    pub action: IndexAction,
    pub wait_ms: f64,
}

/// Stage timings of the docs lane, in milliseconds. The lane shares the
/// query embedding and the ripgrep pass with the code lane, so those are not
/// repeated here; `total_ms` is the lane's own work (candidate fetch, fusion,
/// reranking), not a wall clock of the search.
#[derive(Debug, Default, Serialize)]
pub struct DocsTimings {
    pub lancedb_search_ms: f64,
    pub lexical_search_ms: f64,
    pub fusion_ms: f64,
    pub reranking_ms: f64,
    pub total_ms: f64,
}

/// The docs lane after fusion and before its reranker: the candidates in
/// fused order, cut to `[docs_lane].rerank_candidate_count`.
struct DocsPool {
    hits: Vec<Hit>,
    warning: Option<String>,
    timings: DocsTimings,
}

/// The documentation lane of a search: prose chunks (Markdown) ranked on
/// their own and returned beside the code `results`, so documentation can
/// never displace code from the code list. See `[docs_lane]`.
#[derive(Debug, Serialize)]
pub struct DocsLane {
    /// At most `docs_top_k` prose chunks, best first.
    pub results: Vec<Hit>,
    /// Documentation candidates kept after fusion (the docs reranker's
    /// window).
    pub candidate_count: usize,
    /// Whether the docs reranker ordered `results`.
    pub reranked: bool,
    /// A docs-lane failure (its semantic fetch or its reranker). The lane
    /// fails open: this never fails or warns the search itself.
    pub warning: Option<String>,
    pub timings: DocsTimings,
}

#[derive(Debug, Serialize)]
pub struct SearchReport {
    pub workspace: Workspace,
    pub query: String,
    /// The `[search_profiles.<name>]` this search ran with. Absent from the
    /// JSON for a search that named no profile, so that search's response is
    /// the one it was before profiles existed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub search_profile: Option<String>,
    pub candidate_count: usize,
    pub reranked: bool,
    /// A channel failed or the search fell back (see the retrieval
    /// contracts); counted as a fallback by the evaluation harness.
    pub warning: Option<String>,
    /// Informational remarks about work skipped on purpose where nothing
    /// failed, for example a language server that is still starting and was
    /// not waited for (`lsp_search_budget_ms`). Absent from the JSON when
    /// empty, and never counted as a fallback.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The language-server providers that did not answer within
    /// `lsp_search_budget_ms`: the structured form of the budget note in
    /// [`Self::notes`], which stays. Absent from the JSON when no provider
    /// missed the budget, so a search without a budget hit serializes as it
    /// did before the field existed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub lsp_over_budget: Vec<&'static str>,
    pub timings: Timings,
    pub results: Vec<Hit>,
    /// The documentation lane. Present, and serialized, exactly when the
    /// lane ran and the index holds documentation chunks that pass the
    /// filters: the request named no `source_roles`, the index also holds
    /// code that passes them, and `[docs_lane]` is on. Absent from every
    /// other search's JSON, so a search without documentation answers as it
    /// did before the lane existed. `results` never holds a prose chunk in
    /// that case.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub docs: Option<DocsLane>,
    pub index: SearchIndexLifecycle,
    pub filters: EffectiveFilterReport,
    /// Every candidate of the post-filter fused pool, best first, with the
    /// evidence needed to replay the search offline. Only present (and only
    /// serialized) when the caller asked for it with
    /// [`SearchOptions::trace_candidates`]; absent from every other search's
    /// JSON, including one that fed a candidate dump (see
    /// [`CANDIDATE_DUMP_ENV`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub candidates: Option<Vec<CandidateTrace>>,
    /// What happened to the reranker for this search. Present exactly when
    /// [`Self::candidates`] is, because only a replay needs it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rerank_outcome: Option<RerankOutcome>,
    /// How the answer compares with the tree: present exactly with
    /// `[run_profile.freshness] verify_on_read` (see [`SearchFreshness`]);
    /// absent from every other search's JSON.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub freshness: Option<SearchFreshness>,
    /// What each retrieval channel did in this search, for `freshness`.
    #[serde(skip)]
    pub(crate) channel_outcomes: ChannelOutcomes,
}

/// What a retrieval channel did in one search.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ChannelOutcome {
    /// Off, or not reached (no adapter applied, no vectors).
    #[default]
    NotRun,
    /// It ran and failed (the search carries a warning for it).
    Failed,
    /// It ran and returned its answer, possibly an empty one.
    Answered,
}

/// The outcome of each channel `freshness` reports.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct ChannelOutcomes {
    semantic: ChannelOutcome,
    lexical: ChannelOutcome,
    lsp: ChannelOutcome,
}

/// `[run_profile.freshness] verify_on_read`: how the answer of one search
/// compares with the tree by content hash. It reads and labels; the search
/// never refreshes the index because of it.
#[derive(Debug, Serialize)]
pub struct SearchFreshness {
    /// The index snapshot (its manifest) against the tree's source files.
    pub snapshot: SnapshotFreshness,
    /// The channels that ran, by the id of their map.
    pub channels: Vec<ChannelFreshness>,
    /// One entry per result, in order.
    pub hits: Vec<HitFreshness>,
}

/// The index snapshot against the tree: `lag` counts the source files that
/// differ (changed, added, removed); `null` when it was not compared.
#[derive(Debug, Serialize)]
pub struct SnapshotFreshness {
    pub stale: bool,
    pub lag: Option<u32>,
}

/// What one channel answered from. `vec.chunk.code` and `lexical` read the
/// snapshot, so they carry its lag; `lsp.symbols` answers the language
/// server's view of the tree (`observed: session`), which is not compared
/// (`stale` and `lag` are `null`).
#[derive(Debug, Serialize)]
pub struct ChannelFreshness {
    pub id: &'static str,
    pub observed: &'static str,
    pub stale: Option<bool>,
    pub lag: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

/// Whether the file of one result differs now from the snapshot's: `true`,
/// `false`, or `null` for a hit only the language server returned, or when
/// the tree was not compared.
#[derive(Debug, Serialize)]
pub struct HitFreshness {
    pub path: String,
    pub file_changed: Option<bool>,
}

/// The `channels` of a search's `freshness` block. Only the channels that ran
/// are listed (one that was off or not reached is not), and one that failed
/// says so instead of a lag: it answered nothing.
fn channel_freshness(outcomes: ChannelOutcomes, lag: Option<u32>) -> Vec<ChannelFreshness> {
    const FAILED: &str = "failed; see warning";
    let by_snapshot = |id: &'static str, outcome: ChannelOutcome| match outcome {
        ChannelOutcome::NotRun => None,
        ChannelOutcome::Failed => Some(ChannelFreshness {
            id,
            observed: "snapshot",
            stale: None,
            lag: None,
            note: Some(FAILED),
        }),
        ChannelOutcome::Answered => Some(ChannelFreshness {
            id,
            observed: "snapshot",
            stale: lag.map(|lag| lag > 0),
            lag,
            note: None,
        }),
    };
    let session = match outcomes.lsp {
        ChannelOutcome::NotRun => None,
        ChannelOutcome::Failed => Some(FAILED),
        ChannelOutcome::Answered => {
            Some("answers the server's view of the tree, which is not compared")
        }
    };
    [
        by_snapshot("vec.chunk.code", outcomes.semantic),
        by_snapshot("lexical", outcomes.lexical),
        session.map(|note| ChannelFreshness {
            id: "lsp.symbols",
            observed: "session",
            stale: None,
            lag: None,
            note: Some(note),
        }),
    ]
    .into_iter()
    .flatten()
    .collect()
}

/// The snapshot's and the tree's content hashes by path, for
/// [`App::verify_on_read`].
struct Compared {
    snapshot: HashMap<String, String>,
    tree: HashMap<String, String>,
}

impl Compared {
    /// Whether the file now differs from the snapshot's copy; `None` when
    /// neither side has a source file of that path.
    fn file_changed(&self, path: &str) -> Option<bool> {
        match (self.snapshot.get(path), self.tree.get(path)) {
            (Some(then), Some(now)) => Some(then != now),
            (None, Some(_)) => Some(true),
            (Some(_), None) => is_source_path(path).then_some(true),
            (None, None) => None,
        }
    }
}

/// Switches of one search that observe it without changing what it returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SearchOptions {
    /// Record the fused pool in [`SearchReport::candidates`] (the evaluation
    /// harness's `--dump-candidates`). The MCP, REST, and CLI `search` never
    /// set it. Ranking, results, warnings, and the reported stage timings do
    /// not depend on it.
    pub trace_candidates: bool,
}

/// Names a file that every search of an [`App`] appends to, one JSON line
/// each, with the whole fused candidate pool (see [`CandidateDumpLine`]).
/// Read once, when the `App` is opened; unset or blank means no dump. The
/// MCP, REST, and CLI responses are the same with it set or not.
pub const CANDIDATE_DUMP_ENV: &str = "LCI_CANDIDATE_DUMP";

/// The dump file named by the value of [`CANDIDATE_DUMP_ENV`]: `None` when
/// the variable is unset or blank. Kept apart from the environment lookup so
/// the interpretation is testable without touching the process environment.
fn candidate_dump_path(value: Option<OsString>) -> Option<PathBuf> {
    value
        .filter(|value| !value.to_string_lossy().trim().is_empty())
        .map(PathBuf::from)
}

/// The candidate dump the environment asks for. Called by [`App::open`].
pub(super) fn candidate_dump_from_env() -> Option<PathBuf> {
    candidate_dump_path(std::env::var_os(CANDIDATE_DUMP_ENV))
}

/// One line of the candidate dump file: a search, with the settings that
/// shaped its pool and every candidate of that pool in fused order (the
/// `evaluate --dump-candidates` rows, see [`CandidateTrace`]).
#[derive(Serialize)]
struct CandidateDumpLine<'a> {
    /// Unix time in seconds, millisecond resolution, taken when the search
    /// had finished.
    ts: f64,
    workspace_path: &'a Path,
    query: &'a str,
    /// The search profile the search ran with. Left out of a line whose
    /// search named none, which keeps its fields and bytes as they were.
    #[serde(skip_serializing_if = "Option::is_none")]
    search_profile: Option<&'a str>,
    filters: &'a EffectiveFilterReport,
    /// The channels the search ran with: the profile's resolved switches
    /// when it named one, else `[channels]`.
    channels: &'a ChannelsConfig,
    /// The rerank window the search ran with (the profile's when it named
    /// one, else `rerank_candidate_count`).
    rerank_candidate_count: usize,
    rerank_outcome: Option<RerankOutcome>,
    /// The timings the caller's report carries, unchanged.
    timings: &'a Timings,
    candidates: &'a [CandidateTrace],
}

/// Serializes the dump lines of all searches in this process, whichever `App`
/// or file they belong to: a line is written under it as a single `write_all`
/// to a file opened for appending, so concurrent searches cannot interleave
/// their lines. (Another process appending to the same file relies on the
/// operating system's append semantics.)
static CANDIDATE_DUMP_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Appends `line` (already newline-terminated) to `path`, creating the file
/// but not its directory. The file is opened afresh each time, so one that the
/// harness rotates or removes between searches is simply recreated.
fn append_line(path: &Path, line: &[u8]) -> std::io::Result<()> {
    let _serialized = CANDIDATE_DUMP_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line)
}

/// Appends the finished, traced search `report` to the candidate dump
/// `path` as one JSON line, recording the `settings` the search ran with.
/// Fail-open like every optional part of a search: a line that cannot be
/// serialized or written is a warning in the log, never an error of the
/// search.
async fn append_candidate_dump(path: &Path, settings: &SearchSettings, report: &SearchReport) {
    let started = Instant::now();
    let dumped_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |elapsed| elapsed.as_secs_f64());
    let line = CandidateDumpLine {
        ts: (dumped_at * 1000.0).round() / 1000.0,
        workspace_path: &report.workspace.path,
        query: &report.query,
        search_profile: settings.profile.as_deref(),
        filters: &report.filters,
        channels: &settings.channels,
        rerank_candidate_count: settings.rerank_candidate_count,
        rerank_outcome: report.rerank_outcome,
        timings: &report.timings,
        candidates: report.candidates.as_deref().unwrap_or_default(),
    };
    let mut bytes = match serde_json::to_vec(&line) {
        Ok(bytes) => bytes,
        Err(error) => {
            tracing::warn!(%error, "candidate dump line not serialized; the search is unaffected");
            return;
        }
    };
    // Compact JSON holds no raw newline (string contents are escaped), so
    // this makes the whole record exactly one line.
    bytes.push(b'\n');
    let line_bytes = bytes.len();
    let target = path.to_owned();
    // The line is written before the search returns, so a caller that reads
    // the file after a search sees it; the blocking write stays off the
    // runtime's threads.
    match tokio::task::spawn_blocking(move || append_line(&target, &bytes)).await {
        // The dump runs after `total_ms` is fixed, so its cost is logged here
        // rather than hidden in the caller's round trip.
        Ok(Ok(())) => tracing::info!(
            dump_ms = ms(started),
            bytes = line_bytes,
            "candidate dump appended"
        ),
        Ok(Err(error)) => tracing::warn!(
            path = %path.display(),
            %error,
            "candidate dump not written; the search is unaffected"
        ),
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "candidate dump task failed; the search is unaffected"
        ),
    }
}

/// How the reranker took part in one traced search.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RerankOutcome {
    /// The reranker answered: every shortlisted candidate has a
    /// `reranker_score` and a `rerank_rank`, and the results follow them.
    Scored,
    /// The reranker was asked (or skipped as recently unreachable) and did
    /// not produce scores: the results are in fusion order, with a warning.
    /// No candidate has a `reranker_score`.
    Failed,
    /// `[channels].reranker = false` (or a search profile that turned the
    /// reranker off): no request was made and the results are in fusion
    /// order. No candidate has a `reranker_score`.
    Off,
    /// There was nothing to rerank (no candidates), so the stage never ran.
    NotRun,
}

/// One candidate of the fused pool, as `evaluate --dump-candidates` records
/// it. Together with the arm's `rrf_k` and channel weights, the fields below
/// reproduce `fusion_score`, `fused_rank`, `capped_rank`, and `shortlisted`
/// exactly, so a replay can re-run fusion, the per-file cap, and the shortlist
/// cut with other settings.
///
/// Every rank is **one-based** and every `*_rank` is exactly the value fusion
/// consumed, so the fusion score is, with all arithmetic in `f32` and in this
/// order:
///
/// ```text
/// score = 0.0
/// if semantic_rank: score += semantic_weight / (rrf_k + semantic_rank as f32)
/// if lexical_rank:  score += lexical_weight  / (rrf_k + lexical_rank  as f32)
/// if lsp_rank:      score += lsp_weight      / (rrf_k + lsp_rank      as f32)
/// fusion_score = score + prior
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateTrace {
    /// Repository-relative file path.
    pub relative_file_path: String,
    /// Language identifier.
    pub language: String,
    /// One-based inclusive start line of the chunk. With `relative_file_path`
    /// and `end_line`, it breaks `fusion_score` ties in the fused order.
    pub start_line: u32,
    /// One-based inclusive end line of the chunk.
    pub end_line: u32,
    /// Stable content identity used to break otherwise equal fused-order keys.
    #[serde(default)]
    pub content_hash: String,
    /// Source role of the file.
    pub source_role: SourceRole,
    /// One-based position in the semantic channel's list (cosine order after
    /// the filters); `None` when that channel did not return the chunk.
    pub semantic_rank: Option<usize>,
    /// The semantic channel's `1 - cosine distance`. Never enters fusion.
    pub semantic_score: Option<f32>,
    /// One-based position in the lexical channel's list; `None` when that
    /// channel did not return the chunk.
    pub lexical_rank: Option<usize>,
    /// The lexical channel's own chunk score. Never enters fusion.
    pub lexical_score: Option<f32>,
    /// Lines of the chunk credited with a query term; `0` when the lexical
    /// channel did not return the chunk.
    pub lexical_match_count: u32,
    /// One-based position in the language-server channel's list; `None` when
    /// that channel did not return the chunk.
    pub lsp_rank: Option<usize>,
    /// The source-role prior added to the channel terms, unscaled.
    pub prior: f32,
    /// The fusion score, prior included.
    pub fusion_score: f32,
    /// One-based position in the pool ordered by `fusion_score` descending,
    /// then `relative_file_path`, `start_line`, `end_line`, and `content_hash`;
    /// before the per-file cap and before the shortlist cut. The dump lists
    /// candidates in this order.
    pub fused_rank: usize,
    /// One-based position after the per-file cap (`max_chunks_per_file` of the
    /// arm) reordered the fused order, still before the shortlist cut. Equal
    /// to `fused_rank` when the cap is `0` or demotes nothing. A shortlisted
    /// candidate's zero-based index in the reranker's request is
    /// `capped_rank - 1`.
    pub capped_rank: usize,
    /// Whether the candidate was inside the shortlist cut
    /// (`capped_rank <= rerank_candidate_count`). Only these candidates reach
    /// the reranker and the results of a search.
    pub shortlisted: bool,
    /// One-based position in the reranker's output order (`reranker_score`
    /// descending, then request index); `None` unless the search's
    /// [`RerankOutcome`] is `scored` and the candidate was shortlisted.
    pub rerank_rank: Option<usize>,
    /// The reranker's score; `None` unless `rerank_rank` is set.
    pub reranker_score: Option<f32>,
    /// Bytes of the document the selected rescorer is given for this candidate
    /// (the path, a newline, then the head of the code cut to
    /// `rerank_document_max_bytes`; for the System One rescorer that document
    /// cut again to its snippet limit); `Some` for every shortlisted candidate
    /// and `None` for the others. Recorded even when the reranker was off or
    /// failed, where nothing (or nothing usable) came back.
    pub document_bytes: Option<usize>,
    /// `document_bytes / 4` (integer division): a rough token count, one token
    /// per four bytes.
    pub est_tokens: Option<usize>,
    /// The bounded evidence snippet of the chunk (the same
    /// [`bounded_snippet`] an evidence row carries) on every candidate, so a
    /// replayed result list can check `required_snippet`.
    pub snippet: String,
}

/// The document the reranker scores for `hit`: the path, a newline, and at
/// most `max_bytes` bytes of the head of the chunk.
fn rerank_document(hit: &Hit, max_bytes: usize) -> String {
    format!(
        "{}\n{}",
        hit.chunk.relative_file_path,
        head_within(&hit.chunk.code, max_bytes)
    )
}

/// The candidate the System One rescorer is sent for `hit`: its snippet is the
/// head of [`rerank_document`] cut again to `WIRE_SNIPPET_CHARS` characters.
/// Shared by the request and the candidate trace so the trace sizes what is
/// actually transmitted.
fn wire_candidate(hit: &Hit, max_bytes: usize) -> WireCandidate {
    WireCandidate::new(
        &hit.chunk.relative_file_path,
        hit.chunk.start_line,
        &rerank_document(hit, max_bytes),
    )
}

/// Bytes of the document the selected `rescorer` is given for `hit`.
fn rescorer_document_bytes(rescorer: Rescorer, hit: &Hit, max_bytes: usize) -> usize {
    match rescorer {
        Rescorer::Reranker => rerank_document(hit, max_bytes).len(),
        Rescorer::SystemOne => wire_candidate(hit, max_bytes).snippet.len(),
    }
}

fn fusion_order(a: &Hit, b: &Hit) -> Ordering {
    b.fusion_score
        .total_cmp(&a.fusion_score)
        .then_with(|| a.chunk.relative_file_path.cmp(&b.chunk.relative_file_path))
        .then(a.chunk.start_line.cmp(&b.chunk.start_line))
        .then(a.chunk.end_line.cmp(&b.chunk.end_line))
        .then(a.chunk.content_hash.cmp(&b.chunk.content_hash))
}

/// Records the fused pool before it is capped and cut, in fused order.
/// `hits` is the pool exactly as the cap receives it; `max_per_file`,
/// `shortlist`, and `max_bytes` are the cap, `rerank_candidate_count`, and
/// `rerank_document_max_bytes` the search then applies; `rescorer` selects
/// whose document the sizes describe. Reranker fields are
/// left empty (see [`record_rerank_scores`]). Observes only: the cap runs on
/// positions here, with the same function the search applies to the hits.
fn trace_candidates_for(
    hits: &[Hit],
    max_per_file: u32,
    shortlist: usize,
    max_bytes: usize,
    rescorer: Rescorer,
) -> Vec<CandidateTrace> {
    let positions: Vec<(usize, &str)> = hits
        .iter()
        .enumerate()
        .map(|(index, hit)| (index, hit.chunk.relative_file_path.as_str()))
        .collect();
    let mut capped_rank = vec![0; hits.len()];
    for (position, (index, _)) in cap_by_file(positions, max_per_file, |&(_, path)| path)
        .into_iter()
        .enumerate()
    {
        capped_rank[index] = position + 1;
    }
    hits.iter()
        .zip(capped_rank)
        .enumerate()
        .map(|(index, (hit, capped_rank))| {
            let shortlisted = capped_rank <= shortlist;
            let document_bytes =
                shortlisted.then(|| rescorer_document_bytes(rescorer, hit, max_bytes));
            CandidateTrace {
                relative_file_path: hit.chunk.relative_file_path.clone(),
                language: hit.chunk.language.clone(),
                start_line: hit.chunk.start_line,
                end_line: hit.chunk.end_line,
                content_hash: hit.chunk.content_hash.clone(),
                source_role: hit.source_role,
                semantic_rank: hit.semantic_rank,
                semantic_score: hit.semantic_score,
                lexical_rank: hit.lexical_rank,
                lexical_score: hit.lexical_score,
                lexical_match_count: hit.lexical_match_count,
                lsp_rank: hit.lsp_rank,
                prior: hit.source_role_prior,
                fusion_score: hit.fusion_score,
                fused_rank: index + 1,
                capped_rank,
                shortlisted,
                rerank_rank: None,
                reranker_score: None,
                document_bytes,
                est_tokens: document_bytes.map(|bytes| bytes / 4),
                snippet: bounded_snippet(&hit.chunk.code),
            }
        })
        .collect()
}

/// [`trace_candidates_for`] with the reranking service's documents.
#[cfg(test)]
fn trace_candidates(
    hits: &[Hit],
    max_per_file: u32,
    shortlist: usize,
    max_bytes: usize,
) -> Vec<CandidateTrace> {
    trace_candidates_for(hits, max_per_file, shortlist, max_bytes, Rescorer::Reranker)
}

/// Copies the reranker's answer onto the traced candidates. `ranks` is the
/// output of `Models::rerank`: `(index in the request, score)`, best first,
/// where the request held the shortlist in `capped_rank` order.
fn record_rerank_scores(candidates: &mut [CandidateTrace], ranks: &[(usize, f32)]) {
    let mut by_request_index = HashMap::<usize, usize>::new();
    for (position, candidate) in candidates.iter().enumerate() {
        if candidate.shortlisted {
            by_request_index.insert(candidate.capped_rank - 1, position);
        }
    }
    for (rerank_position, &(request_index, score)) in ranks.iter().enumerate() {
        if let Some(&position) = by_request_index.get(&request_index) {
            candidates[position].rerank_rank = Some(rerank_position + 1);
            candidates[position].reranker_score = Some(score);
        }
    }
}

/// The longest prefix of `text` that is at most `max_bytes` long and ends on
/// a UTF-8 character boundary.
fn head_within(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn hit_matches(filters: &EffectiveFilter, hit: &Hit) -> bool {
    filters.matches(
        &hit.chunk.language,
        &hit.chunk.relative_file_path,
        hit.source_role,
    )
}

/// Whether `hit` is a chunk of a prose extractor (Markdown today; `.rst`,
/// `.adoc`, `.txt` would join). The docs lane is split on the extractor, not on
/// [`SourceRole::Documentation`]: a Rust file under `docs/` is documentation by
/// role but code by extractor, and stays in the code list.
pub(crate) fn is_prose_chunk(hit: &Hit) -> bool {
    crate::language::is_prose_language(&hit.chunk.language)
}

/// The chunks of a search split into the code list and the prose (docs lane)
/// list. The split happens only when the request names no `source_roles` and
/// at least one code chunk passes the filters; otherwise everything stays in
/// the first list and the second is empty, which is the search before the docs
/// lane existed.
pub(crate) fn split_prose(filters: &EffectiveFilter, chunks: Vec<Hit>) -> (Vec<Hit>, Vec<Hit>) {
    if filters.source_roles.is_some() || chunks.iter().all(is_prose_chunk) {
        return (chunks, Vec::new());
    }
    let (prose, code): (Vec<Hit>, Vec<Hit>) = chunks.into_iter().partition(is_prose_chunk);
    (code, prose)
}

/// `filters` narrowed to the languages that occur in `chunks`: the filter one
/// lane hands to the semantic search, so that the lane's candidates are
/// exactly the nearest chunks of its own kind. Everything else (paths, roles)
/// is as the request had it. A lane that took every chunk keeps `filters`.
pub(crate) fn lane_filter(filters: &EffectiveFilter, chunks: &[Hit]) -> EffectiveFilter {
    let mut languages: Vec<String> = chunks
        .iter()
        .map(|hit| hit.chunk.language.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();
    languages.sort();
    let mut narrowed = filters.clone();
    narrowed.languages = Some(languages);
    narrowed
}

/// Demotes, without removing, the chunks of a file beyond its `max`-th in a
/// fused order, so that distinct files fill the head of the list first.
///
/// Walks `hits` in order and keeps a chunk in the head while fewer than `max`
/// chunks of its file (the relative path) are already kept; every other chunk
/// goes to a tail. The result is the head followed by the tail, each in the
/// original relative order, so the operation is stable and a list with few
/// distinct files still comes back complete. `fusion_score` is not touched,
/// which means a demoted chunk can follow a chunk with a lower score. `max ==
/// 0` means unlimited and returns the list unchanged.
fn cap_chunks_per_file(hits: Vec<Hit>, max: u32) -> Vec<Hit> {
    cap_by_file(hits, max, |hit| hit.chunk.relative_file_path.as_str())
}

/// What `notes` says when the cut to `rerank_candidate_count` leaves fewer
/// results than `top_k` asked for: the pool is cut before `top_k` applies, with
/// the reranker off as well. A note, not a warning: nothing failed, and a
/// warning would count the search as a fallback. `None` when `top_k` fits in
/// the window or the pool was smaller than the window anyway.
fn pool_cut_note(top_k: usize, window: usize, pool: usize) -> Option<String> {
    (top_k > window && pool > window).then(|| {
        format!(
            "top_k {top_k} asks for more results than this search keeps: the candidate pool is \
             cut to rerank_candidate_count = {window} (of {pool} candidates) before top_k \
             applies, so at most {window} results come back. Ask for {window} or fewer, or \
             raise rerank_candidate_count (a search profile may set its own)."
        )
    })
}

/// [`cap_chunks_per_file`] over any items that have a file path, so that the
/// candidate trace can run the very same cap on positions instead of cloning
/// hits.
fn cap_by_file<T>(items: Vec<T>, max: u32, path_of: impl Fn(&T) -> &str) -> Vec<T> {
    if max == 0 {
        return items;
    }
    let max = max as usize;
    let keep: Vec<bool> = {
        let mut kept_per_file = HashMap::<&str, usize>::new();
        items
            .iter()
            .map(|item| {
                let kept = kept_per_file.entry(path_of(item)).or_default();
                let fits = *kept < max;
                *kept += usize::from(fits);
                fits
            })
            .collect()
    };
    let mut head = Vec::with_capacity(items.len());
    let mut tail = Vec::new();
    for (item, fits) in items.into_iter().zip(keep) {
        if fits {
            head.push(item);
        } else {
            tail.push(item);
        }
    }
    head.append(&mut tail);
    head
}

/// The `workspace/symbol` queries for `query`: one `<term>#` per term, and
/// for a term over four bytes that ends in `s`, its singular first; at most
/// eight.
fn lsp_symbol_queries(query: &str) -> Vec<String> {
    let mut symbol_queries = Vec::new();
    for term in lexical::terms(query) {
        if term.ends_with('s') && term.len() > 4 {
            symbol_queries.push(format!("{}#", &term[..term.len() - 1]));
        }
        symbol_queries.push(format!("{term}#"));
    }
    symbol_queries.dedup();
    symbol_queries.truncate(8);
    symbol_queries
}

/// The warning for providers that failed (`provider: cause` each).
fn lsp_failure_warning(failures: &[String]) -> String {
    format!("Optional LSP provider unavailable: {}", failures.join("; "))
}

/// What the language-server channel of one search produced.
#[derive(Default)]
struct LspChannel {
    locations: Vec<crate::lsp::Location>,
    /// One `provider: cause` entry for each provider that failed.
    failures: Vec<String>,
    /// Providers that did not answer within `lsp_search_budget_ms`.
    over_budget: Vec<&'static str>,
    /// Providers whose kept session the tree barrier stopped, each with the
    /// reason (see [`App::lsp_channel`]).
    restarted: Vec<crate::lsp::Restart>,
    /// Providers whose answers came from a session this search found running,
    /// each with how long its tree stamp had been unchanged (`None`: no stamp
    /// was recorded for it).
    reused: Vec<(&'static str, Option<Duration>)>,
    elapsed_ms: f64,
}

/// Outcome of resolving which pinned index snapshot a search should read,
/// together with its lifecycle action and any fallback warning.
struct IndexResolution {
    snapshot: Option<Snapshot>,
    action: IndexAction,
    wait_ms: f64,
    warning: Option<String>,
}

fn lookup_lexical_files(file_ids: &HashMap<String, Vec<usize>>, reported: &str) -> Vec<usize> {
    let reported = crate::path::normalize_path_separators(reported).to_lowercase();
    std::iter::once(reported.as_str())
        .chain(
            reported
                .match_indices('/')
                .map(|(boundary, _)| &reported[boundary + 1..]),
        )
        .flat_map(|path| {
            #[cfg(test)]
            tests::LEXICAL_PATH_PROBES.with(|probes| probes.set(probes.get() + 1));
            file_ids.get(path).into_iter().flatten().copied()
        })
        .collect()
}

impl App {
    pub async fn search(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
    ) -> Result<SearchReport> {
        self.search_with_filters(path, query, top_k, FilterRequest::default())
            .await
    }

    pub async fn search_with_filters(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
        filter_request: FilterRequest,
    ) -> Result<SearchReport> {
        self.search_with_options(path, query, top_k, filter_request, SearchOptions::default())
            .await
    }

    /// [`Self::search_with_filters`] with options that observe the search
    /// without changing what it returns (see [`SearchOptions`]).
    ///
    /// When this `App` has a candidate dump (see [`CANDIDATE_DUMP_ENV`]),
    /// every search that returns a report runs traced and appends one line to
    /// the dump file; the trace is removed from the returned report unless
    /// the caller asked for it, so the response is the one an undumped search
    /// returns. A search that fails appends nothing, and neither a dump that
    /// cannot be written nor one that cannot be serialized fails the search.
    pub async fn search_with_options(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
        filter_request: FilterRequest,
        options: SearchOptions,
    ) -> Result<SearchReport> {
        let settings = self.config.base_search_settings();
        let verify = self.config.run_profile.freshness.verify_on_read;
        self.search_configured(
            path,
            query,
            top_k,
            filter_request,
            options,
            &settings,
            verify,
        )
        .await
    }

    /// [`Self::search_with_options`] without the `freshness` block of
    /// `[run_profile.freshness] verify_on_read`: for callers inside the crate
    /// that discard it (the evaluation runner), so they do not pay its scan of
    /// the tree.
    pub(crate) async fn search_with_options_unverified(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
        filter_request: FilterRequest,
        options: SearchOptions,
    ) -> Result<SearchReport> {
        let settings = self.config.base_search_settings();
        self.search_configured(
            path,
            query,
            top_k,
            filter_request,
            options,
            &settings,
            false,
        )
        .await
    }

    /// [`Self::search_with_filters`] run with the named
    /// `[search_profiles.<name>]` applied over `[channels]` and
    /// `rerank_candidate_count` (see [`crate::config::SearchProfile`]):
    /// `profile = None` is exactly [`Self::search_with_filters`]. A profile
    /// changes only which channels and the reranker run and how many fused
    /// candidates the reranker orders, so every profile reads the same index
    /// and the same fusion; the report names it in `search_profile`.
    ///
    /// A name that is not configured fails before any index work, with an
    /// error that lists the configured names.
    pub async fn search_in_profile(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
        filter_request: FilterRequest,
        profile: Option<&str>,
    ) -> Result<SearchReport> {
        let settings = self.config.search_settings(profile)?;
        let verify = self.config.run_profile.freshness.verify_on_read;
        self.search_configured(
            path,
            query,
            top_k,
            filter_request,
            SearchOptions::default(),
            &settings,
            verify,
        )
        .await
    }

    /// [`Self::search_in_profile`] without the `freshness` block: the start
    /// nodes of a traversal run this search and discard the block, and the
    /// traversal reads the tree once itself, so it must not scan it again.
    pub(crate) async fn search_in_profile_unverified(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
        filter_request: FilterRequest,
        profile: Option<&str>,
    ) -> Result<SearchReport> {
        let settings = self.config.search_settings(profile)?;
        self.search_configured(
            path,
            query,
            top_k,
            filter_request,
            SearchOptions::default(),
            &settings,
            false,
        )
        .await
    }

    /// The `freshness` block of a search (`[run_profile.freshness]
    /// verify_on_read`), read after the search: the manifest of the snapshot
    /// the search ran on against the tree's source files, one read of the
    /// tree's content hashes (no chunk is made; with `[run_profile.freshness]
    /// stat_gate` only the files whose modified time or length changed are
    /// read). A
    /// failure to compare leaves the lags and `file_changed` null and says so
    /// in `notes`. A refresh of the snapshot between the search and this read
    /// (`[index].freshness = "watch"`) is not seen: the block then looks as
    /// current as the new snapshot.
    async fn verify_on_read(&self, report: &mut SearchReport) -> SearchFreshness {
        VERIFY_SCANS.with(|count| count.set(count.get() + 1));
        let data_dir = self.config.data_dir.clone();
        let workspace = report.workspace.clone();
        let stat_gate = self.config.run_profile.freshness.stat_gate;
        let compared = tokio::task::spawn_blocking(move || -> Result<Compared> {
            let manifest = crate::manifest::try_load(&data_dir, &workspace)?;
            // Only the hashes are compared: no chunk is made, and under
            // `[run_profile.freshness] stat_gate` only changed files are read.
            let tree = lci_core::chunk::source_hashes(&workspace.path, false, stat_gate)?;
            Ok(Compared {
                snapshot: manifest
                    .files
                    .into_iter()
                    .map(|(path, file)| (path, file.content_hash))
                    .collect(),
                tree: tree
                    .files
                    .into_iter()
                    .map(|file| (file.relative, file.content_hash))
                    .collect(),
            })
        })
        .await
        .map_err(anyhow::Error::from)
        .and_then(|compared| compared);
        let compared = match compared {
            Ok(compared) => Some(compared),
            Err(error) => {
                report.notes.push(format!(
                    "verify_on_read: the tree could not be compared with the index snapshot: {error:#}"
                ));
                None
            }
        };
        let lag = compared.as_ref().map(|compared| {
            let tree: HashMap<&str, &str> = compared
                .tree
                .iter()
                .map(|(path, hash)| (path.as_str(), hash.as_str()))
                .collect();
            snapshot_differing(&compared.snapshot, &tree)
        });
        let listed = channel_freshness(report.channel_outcomes, lag);
        let hits = report
            .results
            .iter()
            .map(|hit| {
                let path = hit.chunk.relative_file_path.replace('\\', "/");
                // A hit no snapshot channel returned (only the language
                // server, or no channel named) is not placed by the snapshot,
                // so there is nothing to compare it with.
                let from_snapshot = hit
                    .retrieval_channels
                    .iter()
                    .any(|channel| channel != "lsp");
                let file_changed = match (&compared, from_snapshot) {
                    (Some(compared), true) => compared.file_changed(&path),
                    _ => None,
                };
                HitFreshness { path, file_changed }
            })
            .collect();
        SearchFreshness {
            snapshot: SnapshotFreshness {
                stale: lag.is_some_and(|lag| lag > 0),
                lag,
            },
            channels: listed,
            hits,
        }
    }

    /// The search behind [`Self::search_with_options`] and
    /// [`Self::search_in_profile`], run with the resolved `settings` (the
    /// query-time switches and the rerank window). Appends the candidate dump
    /// line when this `App` has a dump, as [`Self::search_with_options`]
    /// describes.
    #[allow(clippy::too_many_arguments)]
    async fn search_configured(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
        filter_request: FilterRequest,
        options: SearchOptions,
        settings: &SearchSettings,
        verify: bool,
    ) -> Result<SearchReport> {
        let mut report = self
            .search_dumped(path, query, top_k, filter_request, options, settings)
            .await?;
        // `[run_profile.freshness] verify_on_read`: read after the search (and
        // after the candidate dump, which does not carry it).
        if verify {
            let started = Instant::now();
            report.freshness = Some(self.verify_on_read(&mut report).await);
            report.timings.verify_ms = ms(started);
        }
        Ok(report)
    }

    /// [`Self::search_configured`] without the freshness block: the search and
    /// its candidate dump.
    async fn search_dumped(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
        filter_request: FilterRequest,
        options: SearchOptions,
        settings: &SearchSettings,
    ) -> Result<SearchReport> {
        let Some(dump) = self.candidate_dump.as_deref() else {
            return self
                .search_once(path, query, top_k, filter_request, options, settings)
                .await;
        };
        let mut traced = options;
        traced.trace_candidates = true;
        let mut report = self
            .search_once(path, query, top_k, filter_request, traced, settings)
            .await?;
        append_candidate_dump(dump, settings, &report).await;
        if !options.trace_candidates {
            report.candidates = None;
            report.rerank_outcome = None;
        }
        Ok(report)
    }

    /// One search, with the trace `options` asks for and the query-time
    /// `settings` it runs with.
    async fn search_once(
        &self,
        path: &Path,
        query: &str,
        top_k: Option<usize>,
        filter_request: FilterRequest,
        options: SearchOptions,
        settings: &SearchSettings,
    ) -> Result<SearchReport> {
        let total = Instant::now();
        ensure!(!query.trim().is_empty(), "query must not be empty");
        ensure!(query.len() <= 16000, "query exceeds 16000 bytes");
        let k = top_k.unwrap_or(self.config.default_top_k);
        ensure!((1..=40).contains(&k), "top_k must be 1..=40");
        let filtering_requested = filter_request.languages.is_some()
            || filter_request.include_paths.is_some()
            || filter_request.exclude_paths.is_some()
            || filter_request.source_roles.is_some();
        let filters = EffectiveFilter::from_request(&filter_request)?;
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let coordination = self.lock(&workspace.id).await;
        let initial_snapshot = self.store.snapshot(&workspace).await?;
        let resolution = self
            .resolve_index_for_search(&workspace, &coordination, initial_snapshot)
            .await?;
        let snapshot = resolution
            .snapshot
            .context("workspace is not indexed; call index_workspace first")?;
        // Not while a build runs: it must not compete with the start. The
        // read guard is held until the task is spawned.
        if self.warm_start_due(&workspace.id) {
            let languages = Store::languages(&snapshot).await.unwrap_or_default();
            if let Ok(_building_excluded) = coordination.lock.try_read() {
                self.warm_language_servers(&workspace, languages);
            }
        }
        let mut report = SearchReport {
            workspace,
            query: query.into(),
            search_profile: settings.profile.clone(),
            candidate_count: 0,
            reranked: false,
            warning: resolution.warning,
            notes: Vec::new(),
            lsp_over_budget: Vec::new(),
            timings: Timings::default(),
            results: vec![],
            docs: None,
            index: SearchIndexLifecycle {
                action: resolution.action,
                wait_ms: resolution.wait_ms,
            },
            filters: filters.report(),
            // A traced search that ends before fusion has an empty pool and
            // never reached the reranker.
            candidates: options.trace_candidates.then(Vec::new),
            rerank_outcome: options.trace_candidates.then_some(RerankOutcome::NotRun),
            freshness: None,
            channel_outcomes: ChannelOutcomes::default(),
        };
        if snapshot.table.count_rows(None).await? == 0 {
            report.timings.total_ms = ms(total);
            return Ok(report);
        }
        let mut all_chunks = Store::chunks(&snapshot, &report.workspace).await?;
        all_chunks.retain(|hit| hit_matches(&filters, hit));
        if all_chunks.is_empty() {
            report.timings.total_ms = ms(total);
            return Ok(report);
        }
        // The docs lane: with no `source_roles` filter and some code in
        // play, the prose chunks leave the code list, which then ranks
        // exactly as if they were not indexed (the code lane's candidate
        // fetch, lexical statistics, and language-server mapping all see the
        // code chunks alone). Otherwise `all_chunks` is the single list of
        // old, and `doc_chunks` is empty.
        let (all_chunks, doc_chunks) = split_prose(&filters, all_chunks);
        let code_filters = if doc_chunks.is_empty() {
            filters.clone()
        } else {
            lane_filter(&filters, &all_chunks)
        };
        let docs_active =
            !doc_chunks.is_empty() && self.config.docs_lane.enabled && settings.docs_top_k > 0;
        // `[channels]` switches, as a search profile may override them. A
        // deliberately disabled channel is skipped outright: no work, a zero
        // timing, and no "unavailable" warning.
        let mut channels = settings.channels;
        // A vectorless snapshot (built while the embedding service was
        // unreachable) holds placeholders: the query is not embedded and the
        // vector lane does not run, as with the semantic channel off, but the
        // search says why. Lexical and LSP run as usual.
        if channels.semantic && snapshot.vectors_missing {
            channels.semantic = false;
            let message = VECTORLESS_WARNING.to_owned();
            report.warning = Some(
                report
                    .warning
                    .take()
                    .map_or(message.clone(), |w| format!("{w}; {message}")),
            );
        }
        let applicable_adapters = if channels.lsp {
            self.applicable_lsp_adapters(&all_chunks)
        } else {
            Vec::new()
        };
        let (query_result, lexical_result, lsp_result) = tokio::join!(
            async {
                if !channels.semantic {
                    return None;
                }
                let timer = Instant::now();
                Some((self.models.query(query).await, ms(timer)))
            },
            async {
                if !channels.lexical {
                    return None;
                }
                let timer = Instant::now();
                Some((
                    lexical::search(&self.config.ripgrep_path, &report.workspace.path, query).await,
                    ms(timer),
                ))
            },
            async {
                if !channels.lsp {
                    return LspChannel::default();
                }
                let timer = Instant::now();
                if applicable_adapters.is_empty() {
                    return LspChannel {
                        elapsed_ms: ms(timer),
                        ..LspChannel::default()
                    };
                }
                let symbol_queries = lsp_symbol_queries(query);
                let mut channel = self
                    .lsp_channel(&applicable_adapters, &report.workspace, &symbol_queries)
                    .await;
                channel.elapsed_ms = ms(timer);
                channel
            },
        );
        let lsp_locations = lsp_result.locations;
        let lsp_failures = lsp_result.failures;
        report.timings.lsp_search_ms = lsp_result.elapsed_ms;
        // The LSP channel "ran" when it was enabled, at least one provider
        // applied to the indexed languages, and not every provider failed. A
        // provider past its budget did not fail (its server is only still
        // starting), so a search with that channel alone is a clean miss with
        // a note, not an error.
        let mut lexical_or_lsp_ran = channels.lsp
            && !applicable_adapters.is_empty()
            && lsp_failures.len() < applicable_adapters.len();
        report.channel_outcomes.lsp = if !channels.lsp || applicable_adapters.is_empty() {
            ChannelOutcome::NotRun
        } else if lexical_or_lsp_ran {
            ChannelOutcome::Answered
        } else {
            ChannelOutcome::Failed
        };
        // Informational, like the over-budget note: nothing failed.
        report.notes.extend(super::navigation::barrier_notes(
            &lsp_result.restarted,
            &lsp_result.reused,
        ));
        if !lsp_result.over_budget.is_empty() {
            // Not a warning: nothing failed, the server is just not ready
            // yet. A warning would also count the search as a fallback.
            report.notes.push(
                self.lsp_over_budget_note(&lsp_result.over_budget, &report.workspace.id)
                    .await,
            );
            report.lsp_over_budget = lsp_result.over_budget.clone();
        }
        if !lsp_failures.is_empty() {
            let message = lsp_failure_warning(&lsp_failures);
            report.warning = Some(
                report
                    .warning
                    .take()
                    .map_or(message.clone(), |w| format!("{w}; {message}")),
            );
        }
        // The query vector, kept for the docs lane when it will run.
        let mut docs_vector: Option<Vec<f32>> = None;
        let semantic = match query_result {
            // Disabled: the query was never embedded and the index is not
            // searched; both timings stay zero.
            None => vec![],
            Some((query_vector, query_ms)) => {
                report.timings.query_embedding_ms = query_ms;
                let semantic_timer = Instant::now();
                let hits = match query_vector {
                    Ok(vector) => {
                        if docs_active {
                            docs_vector = Some(vector.clone());
                        }
                        match Store::search_code_lane(
                            &snapshot,
                            &report.workspace,
                            &vector,
                            self.config.semantic_candidate_count,
                            &code_filters,
                            all_chunks.len(),
                        )
                        .await
                        {
                            Ok(found) => {
                                if let Some(message) = found.cap_warning() {
                                    report.warning =
                                        Some(report.warning.take().map_or(message.clone(), |w| {
                                            format!("{w}; {message}")
                                        }));
                                }
                                report.channel_outcomes.semantic = ChannelOutcome::Answered;
                                found.hits
                            }
                            Err(error) => {
                                report.channel_outcomes.semantic = ChannelOutcome::Failed;
                                let message = format!("Semantic retrieval unavailable: {error:#}");
                                report.warning = Some(
                                    report
                                        .warning
                                        .take()
                                        .map_or(message.clone(), |w| format!("{w}; {message}")),
                                );
                                vec![]
                            }
                        }
                    }
                    Err(error) => {
                        report.channel_outcomes.semantic = ChannelOutcome::Failed;
                        let message = format!(
                            "Query embedding unavailable; results from the other enabled channels used: {error:#}"
                        );
                        report.warning = Some(
                            report
                                .warning
                                .take()
                                .map_or(message.clone(), |w| format!("{w}; {message}")),
                        );
                        vec![]
                    }
                };
                report.timings.lancedb_search_ms = ms(semantic_timer);
                hits
            }
        };
        let mut lexical_hits = Vec::new();
        // The ripgrep pass the docs lane maps onto its own chunks.
        let mut docs_lexical: Option<lexical::LexicalSearch> = None;
        if let Some((lexical_matches, lexical_ms)) = lexical_result {
            report.timings.lexical_search_ms = lexical_ms;
            match lexical_matches {
                Ok(mut lexical_search) => {
                    lexical_or_lsp_ran = true;
                    report.channel_outcomes.lexical = ChannelOutcome::Answered;
                    if let Some(message) = lexical_search.warning.take() {
                        report.warning = Some(
                            report
                                .warning
                                .take()
                                .map_or(message.clone(), |w| format!("{w}; {message}")),
                        );
                    }
                    if docs_active {
                        docs_lexical = Some(lexical_search.clone());
                    }
                    lexical_hits = Self::map_lexical_matches(
                        &all_chunks,
                        lexical_search,
                        self.config.lexical_candidate_count,
                    );
                }
                Err(error) => {
                    report.channel_outcomes.lexical = ChannelOutcome::Failed;
                    let message = format!("Lexical retrieval unavailable: {error:#}");
                    report.warning = Some(
                        report
                            .warning
                            .take()
                            .map_or(message.clone(), |w| format!("{w}; {message}")),
                    );
                }
            }
        }
        let lsp_hits =
            Self::map_lsp_locations(&all_chunks, lsp_locations, self.config.lsp_candidate_count);
        // The docs lane's candidates, fused on their own. Cheap, and made
        // before the code lane can end the search so that a query only the
        // documentation answers still gets its `docs`.
        let docs_pool = if docs_active {
            Some(
                self.docs_pool(
                    &snapshot,
                    &report.workspace,
                    &filters,
                    &doc_chunks,
                    docs_vector.as_deref(),
                    docs_lexical,
                )
                .await,
            )
        } else {
            None
        };
        if semantic.is_empty() && lexical_hits.is_empty() && lsp_hits.is_empty() {
            // With the semantic channel off, an enabled lexical or LSP
            // channel that ran without failing and matched nothing is an
            // ordinary answer (a literal or symbol search with no hit). If
            // every enabled channel failed (or none could run) that stays an
            // error. With the semantic channel on, an empty result means it
            // failed, because it nearly always returns candidates.
            if filtering_requested || (!channels.semantic && lexical_or_lsp_ran) {
                if let Some(pool) = docs_pool {
                    report.docs = Some(self.finish_docs(query, pool, settings).await);
                }
                report.timings.total_ms = ms(total);
                return Ok(report);
            }
            let detail: Vec<&str> = report
                .warning
                .iter()
                .chain(report.notes.iter())
                .map(String::as_str)
                .collect();
            if detail.is_empty() {
                bail!("every enabled retrieval channel failed or returned no candidates");
            }
            bail!(
                "every enabled retrieval channel failed or returned no candidates ({})",
                detail.join("; ")
            );
        }
        let fusion_timer = Instant::now();
        let mut hits = Self::fuse_hits(
            self.config.rrf_k,
            &self.config.fusion,
            semantic,
            lexical_hits,
            lsp_hits,
        );
        hits.retain(|hit| hit_matches(&filters, hit));
        hits.sort_by(fusion_order);
        // Optional per-file cap: demote (never drop) a file's chunks beyond
        // `max_chunks_per_file`, before the cut, so the reranker shortlist,
        // the reranker-off results, and the reranker-failure fallback all fill
        // with distinct files first. `0` returns the order unchanged.
        //
        // A traced search first records the whole pool (it is what the cap and
        // the cut are about to discard from); the recording is not fusion work
        // and stays out of `fusion_ms`.
        let mut trace_ms = 0.0;
        let mut traced = options.trace_candidates.then(|| {
            let timer = Instant::now();
            let rows = trace_candidates_for(
                &hits,
                self.config.fusion.max_chunks_per_file,
                settings.rerank_candidate_count,
                self.config.rerank_document_max_bytes,
                self.config.rescorer,
            );
            trace_ms = ms(timer);
            rows
        });
        hits = cap_chunks_per_file(hits, self.config.fusion.max_chunks_per_file);
        let pool = hits.len();
        hits.truncate(settings.rerank_candidate_count);
        // Only for a `top_k` the caller passed: the default is the operator's own
        // setting, and a profile with a small window is meant to return fewer.
        if let Some(note) =
            top_k.and_then(|asked| pool_cut_note(asked, settings.rerank_candidate_count, pool))
        {
            report.notes.push(note);
        }
        report.timings.fusion_ms = ms(fusion_timer) - trace_ms;
        report.candidate_count = hits.len();
        // The docs lane's reranker call runs beside the code lane's; the
        // two touch nothing in common.
        let docs_future = async {
            match docs_pool {
                Some(pool) => Some(self.finish_docs(query, pool, settings).await),
                None => None,
            }
        };
        let code_future = async {
            if !hits.is_empty() {
                if channels.reranker {
                    let timer = Instant::now();
                    let scores = self.rerank_into_report(query, hits, k, &mut report).await;
                    report.timings.reranking_ms = ms(timer);
                    if let Some(traced) = &mut traced {
                        report.rerank_outcome = Some(match scores {
                            Some(ranks) => {
                                record_rerank_scores(traced, &ranks);
                                RerankOutcome::Scored
                            }
                            None => RerankOutcome::Failed,
                        });
                    }
                } else {
                    // Deliberately disabled: fusion order, `reranked = false`, no
                    // request, no warning, and a zero reranking time.
                    report.results = hits.into_iter().take(k).collect();
                    if traced.is_some() {
                        report.rerank_outcome = Some(RerankOutcome::Off);
                    }
                }
            }
        };
        let (docs, ()) = tokio::join!(docs_future, code_future);
        report.docs = docs;
        report.candidates = traced;
        report.results.retain(|hit| hit_matches(&filters, hit));
        // Like `fusion_ms`, the total leaves out the trace recording, so the
        // reported timings do not depend on whether candidates were traced.
        report.timings.total_ms = ms(total) - trace_ms;
        tracing::info!(
            query_embedding_ms = report.timings.query_embedding_ms,
            lancedb_search_ms = report.timings.lancedb_search_ms,
            lexical_search_ms = report.timings.lexical_search_ms,
            lsp_search_ms = report.timings.lsp_search_ms,
            fusion_ms = report.timings.fusion_ms,
            reranking_ms = report.timings.reranking_ms,
            total_ms = report.timings.total_ms,
            reranked = report.reranked,
            search_profile = report.search_profile.as_deref(),
            "retrieval complete"
        );
        Ok(report)
    }

    /// Resolves the index snapshot a search should read: creates or waits
    /// for a first-search index when the workspace has never been indexed,
    /// otherwise applies the configured freshness policy to an existing
    /// snapshot. The returned snapshot is pinned, so a concurrent index commit
    /// cannot change its contents and no workspace read lock is needed.
    async fn resolve_index_for_search(
        &self,
        workspace: &Workspace,
        coordination: &WorkspaceCoordination,
        initial_snapshot: Option<Snapshot>,
    ) -> Result<IndexResolution> {
        if initial_snapshot.is_none() {
            let flight = Instant::now();
            let mut created = false;
            let mut action = IndexAction::Reused;
            match coordination.lock.try_write() {
                Ok(_write) => {
                    // Re-check under the lock: a concurrent creator may have
                    // committed between the initial check and lock acquisition.
                    if self.store.snapshot(workspace).await?.is_none() {
                        // We are the creator; clear any stale flight error.
                        *coordination.flight_error.lock().await = None;
                        match self.index_locked(workspace.clone()).await {
                            Ok(_) => created = true,
                            Err(error) => {
                                *coordination.flight_error.lock().await =
                                    Some(format!("{error:#}"));
                                return Err(error);
                            }
                        }
                    } else {
                        action = IndexAction::WaitedForExistingJob;
                    }
                }
                Err(_) => {
                    // A creator or explicit index holds the write lock; wait.
                    let _write = coordination.lock.write().await;
                    if self.store.snapshot(workspace).await?.is_some() {
                        action = IndexAction::WaitedForExistingJob;
                    } else {
                        // The holder committed nothing. If an index job
                        // failed, share its cause instead of starting a
                        // duplicate within the same failed flight.
                        let shared = coordination.flight_error.lock().await.clone();
                        if let Some(cause) = shared {
                            return Err(anyhow::anyhow!(
                                "indexing failed while waiting for the in-progress index job: {cause}"
                            ));
                        }
                        // No failure was recorded (e.g. the holder was
                        // cancelled); we now hold the write
                        // lock exclusively, so index ourselves.
                        *coordination.flight_error.lock().await = None;
                        match self.index_locked(workspace.clone()).await {
                            Ok(_) => created = true,
                            Err(error) => {
                                *coordination.flight_error.lock().await =
                                    Some(format!("{error:#}"));
                                return Err(error);
                            }
                        }
                    }
                }
            }
            if created {
                action = IndexAction::Created;
            }
            let snapshot = self.store.snapshot(workspace).await?;
            Ok(IndexResolution {
                snapshot,
                action,
                wait_ms: ms(flight),
                warning: None,
            })
        } else {
            let flight = Instant::now();
            let identity_changed = initial_snapshot
                .as_ref()
                .is_some_and(|s| s.identity != self.config.embedding_identity());
            let (action, warning) = match self.config.index.freshness {
                IndexFreshness::OnSearch => {
                    let vectors_missing =
                        initial_snapshot.as_ref().is_some_and(|s| s.vectors_missing);
                    self.refresh_stale_snapshot(
                        workspace,
                        coordination,
                        identity_changed,
                        vectors_missing,
                    )
                    .await?
                }
                IndexFreshness::Manual | IndexFreshness::Watch => {
                    ensure!(
                        !identity_changed,
                        "embedding configuration changed; reindex workspace"
                    );
                    (IndexAction::Reused, None)
                }
            };
            // A fallback (the wait timed out, the staleness check or the refresh
            // failed) keeps the snapshot taken before it, the one the warning
            // names. Every other outcome re-reads: an index commit that landed
            // while the freshness check ran must be visible to this search.
            let snapshot = if warning.is_some() {
                initial_snapshot
            } else {
                self.store.snapshot(workspace).await?
            };
            Ok(IndexResolution {
                snapshot,
                action,
                wait_ms: ms(flight),
                warning,
            })
        }
    }

    /// Maps credited ripgrep lines onto the chunks that contain them and
    /// ranks the chunks by BM25 over IDF-weighted term evidence (see
    /// `lexical::score_chunks`), not by matching-line count. Document
    /// frequencies are measured over indexed files only: a term's `df` is the
    /// number of indexed files with a credited line, relative to the number of
    /// indexed files in `all_chunks`, so files ripgrep sees but the index
    /// does not cannot skew them. Chunk length is its line count, averaged
    /// over `all_chunks`. Chunks that match only terms too common to score
    /// are not candidates.
    ///
    /// A reported path selects its candidate chunks once (every chunk whose
    /// file path equals it, or is a `/`-delimited suffix of it, compared
    /// case-insensitively); each line then takes the first candidate, in
    /// `all_chunks` order, whose line range holds it.
    pub(super) fn map_lexical_matches(
        all_chunks: &[Hit],
        search: lexical::LexicalSearch,
        candidate_count: usize,
    ) -> Vec<Hit> {
        let mut file_ids = HashMap::<&str, usize>::new();
        let mut lowercase_file_ids = HashMap::<String, Vec<usize>>::new();
        let mut files: Vec<Vec<usize>> = Vec::new();
        let mut chunk_files = Vec::with_capacity(all_chunks.len());
        for (index, hit) in all_chunks.iter().enumerate() {
            let path = hit.chunk.relative_file_path.as_str();
            let next = files.len();
            let id = *file_ids.entry(path).or_insert(next);
            if id == next {
                lowercase_file_ids
                    .entry(path.to_lowercase())
                    .or_default()
                    .push(id);
                files.push(Vec::new());
            }
            files[id].push(index);
            chunk_files.push(id);
        }
        let total_files = files.len();
        let mut candidates = HashMap::<String, Vec<usize>>::new();
        let mut credited = Vec::with_capacity(search.matches.len());
        for matched in search.matches {
            let (line, term_mask) = (matched.line, matched.term_mask);
            let chunks = match candidates.entry(matched.relative_path) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let mut chunks: Vec<usize> =
                        lookup_lexical_files(&lowercase_file_ids, entry.key())
                            .into_iter()
                            .flat_map(|id| files[id].iter().copied())
                            .collect();
                    chunks.sort_unstable();
                    entry.insert(chunks)
                }
            };
            if let Some(&index) = chunks.iter().find(|&&index| {
                let chunk = &all_chunks[index].chunk;
                chunk.start_line <= line && chunk.end_line >= line
            }) {
                credited.push(lexical::CreditedLine {
                    file: chunk_files[index],
                    chunk: index,
                    term_mask,
                });
            }
        }
        let chunk_lengths: Vec<u32> = all_chunks
            .iter()
            .map(|hit| hit.chunk.end_line.saturating_sub(hit.chunk.start_line) + 1)
            .collect();
        let mut ranked =
            lexical::score_chunks(search.terms.len(), total_files, &chunk_lengths, &credited);
        ranked.sort_by(|a, b| {
            let (a_chunk, b_chunk) = (&all_chunks[a.chunk].chunk, &all_chunks[b.chunk].chunk);
            b.score
                .total_cmp(&a.score)
                .then_with(|| a_chunk.relative_file_path.cmp(&b_chunk.relative_file_path))
                .then(a_chunk.start_line.cmp(&b_chunk.start_line))
        });
        let mut lexical_hits = Vec::new();
        for (rank, scored) in ranked.into_iter().take(candidate_count).enumerate() {
            let mut hit = all_chunks[scored.chunk].clone();
            hit.lexical_rank = Some(rank + 1);
            hit.lexical_score = Some(scored.score as f32);
            hit.lexical_match_count = scored.credited_lines;
            hit.retrieval_channels.push("lexical".into());
            lexical_hits.push(hit);
        }
        lexical_hits
    }

    /// The enabled language-server adapters that apply to the languages of
    /// `chunks` that join the search channel ([`LspAdapter::joins_search_channel`]),
    /// one per provider name, sorted by provider. The LSP channel
    /// of `search_once` and the `lsp.symbols` map both pick providers here.
    fn applicable_lsp_adapters(&self, chunks: &[Hit]) -> Vec<Arc<dyn LspAdapter>> {
        let mut applicable: HashMap<&'static str, Arc<dyn LspAdapter>> = HashMap::new();
        let languages: HashSet<&str> = chunks
            .iter()
            .map(|hit| hit.chunk.language.as_str())
            .collect();
        for language in languages {
            for adapter in self.analyzer.adapters_for_language(language) {
                if adapter.enabled() && adapter.joins_search_channel() {
                    applicable
                        .entry(adapter.provider())
                        .or_insert_with(|| Arc::clone(adapter));
                }
            }
        }
        let mut providers: Vec<_> = applicable.into_values().collect();
        providers.sort_by_key(|adapter| adapter.provider());
        providers
    }

    /// Runs `symbol_queries` against every provider of `providers` side by
    /// side, each under the same `lsp_search_budget_ms` wall clock budget,
    /// so one that is still starting cannot starve the others. Past the
    /// budget the provider's future is dropped, which only stops this
    /// search waiting: its session lives in the manager and keeps starting
    /// and warming for the next search. Answers gathered before the budget
    /// ran out are kept, and merged query by query across providers, as one
    /// sequential pass would: first-seen order decides the LSP rank. The
    /// returned channel's `elapsed_ms` is left for the caller to set.
    ///
    /// Before the requests, the providers' kept sessions pass the tree barrier
    /// (`Manager::tree_barrier`): one whose tree changed since its last check
    /// is stopped and the search starts a fresh one (inside the budget, so a
    /// slow start reads as an over-budget note). The channel lists those
    /// restarts and the sessions the search found running whose requests
    /// completed (one that missed the budget or failed is not an answer from
    /// a kept session).
    async fn lsp_channel(
        &self,
        providers: &[Arc<dyn LspAdapter>],
        workspace: &Workspace,
        symbol_queries: &[String],
    ) -> LspChannel {
        let budget = Duration::from_millis(self.config.lsp_search_budget_ms);
        // A kept session passes the tree barrier once, before the providers'
        // requests and outside their budget (a stamp, not a request); the
        // requests run in one scope that records which sessions the search
        // found running.
        let provider_refs: Vec<&Arc<dyn LspAdapter>> = providers.iter().collect();
        let restarted = self.analyzer.tree_barrier(workspace, &provider_refs).await;
        let walk = Arc::new(crate::lsp::WalkSessions::default());
        let answers = walk
            .scope(futures::future::join_all(providers.iter().map(
                |adapter| async move {
                    let gathered = std::sync::Mutex::new(Vec::new());
                    let answer = tokio::time::timeout(
                        budget,
                        self.provider_symbols(adapter, workspace, symbol_queries, &gathered),
                    )
                    .await;
                    let gathered = gathered.into_inner().unwrap_or_else(|p| p.into_inner());
                    (adapter.provider(), gathered, answer)
                },
            )))
            .await;
        let mut channel = LspChannel {
            restarted,
            ..LspChannel::default()
        };
        // The providers whose requests all completed: the session is recorded
        // when a request acquires it, so a provider that then missed the
        // budget or failed is not labelled as an answer from a kept session.
        let mut completed: Vec<&'static str> = Vec::new();
        let mut per_provider = Vec::new();
        for (provider, gathered, answer) in answers {
            per_provider.push(gathered);
            match answer {
                Ok(failure) => {
                    completed.extend(failure.is_none().then_some(provider));
                    channel.failures.extend(failure);
                }
                Err(_) => {
                    tracing::debug!(
                        provider,
                        budget_ms = self.config.lsp_search_budget_ms,
                        "language server did not answer within the search budget"
                    );
                    channel.over_budget.push(provider);
                }
            }
        }
        channel.reused = walk
            .reused()
            .into_iter()
            .filter(|(provider, _)| completed.contains(provider))
            .collect();
        for index in 0..symbol_queries.len() {
            for locations in &mut per_provider {
                if let Some(found) = locations.get_mut(index) {
                    channel.locations.append(found);
                }
            }
        }
        channel
    }

    /// The note for providers past the LSP budget: not a warning, because
    /// nothing failed (the server is only not ready yet), and a warning
    /// would also count the search as a fallback.
    ///
    /// It ends with what each of those servers reports about itself (see
    /// [`readiness_clause`]), so a slow start reads differently from a server
    /// that is busy again; a provider without a session adds nothing.
    async fn lsp_over_budget_note(&self, over_budget: &[&str], workspace_id: &str) -> String {
        let mut note = format!(
            "LSP still starting or slow; symbol results from {} are missing or incomplete \
             for this search (lsp_search_budget_ms = {}). The server keeps starting in the \
             background.",
            over_budget.join(", "),
            self.config.lsp_search_budget_ms
        );
        let mut clauses = Vec::new();
        for provider in over_budget {
            if let Some(readiness) = self.analyzer.readiness(provider, workspace_id).await {
                clauses.push(readiness_clause(provider, &readiness));
            }
        }
        if !clauses.is_empty() {
            note.push_str(&format!(" Server state: {}.", clauses.join("; ")));
        }
        note
    }

    /// One provider's `workspace/symbol` answers, one list per symbol query
    /// in query order, pushed onto `gathered` as they arrive so that the
    /// caller keeps them even when it drops this future at the search budget.
    /// A provider that failed (after its own fresh-process retry) is skipped
    /// for the remaining queries of this search, so a missing or broken
    /// server costs one failure, not one per symbol query; the answers
    /// gathered before the failure are kept. Returns that failure, if any.
    ///
    /// Cancel-safe: dropping this future must (and does) leave the
    /// provider's session untouched.
    async fn provider_symbols(
        &self,
        adapter: &Arc<dyn LspAdapter>,
        workspace: &Workspace,
        symbol_queries: &[String],
        gathered: &std::sync::Mutex<Vec<Vec<crate::lsp::Location>>>,
    ) -> Option<String> {
        for symbol_query in symbol_queries {
            match self
                .analyzer
                .symbols(adapter, workspace, symbol_query)
                .await
            {
                Ok(locations) => gathered
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(locations),
                Err(error) => return Some(format!("{}: {error:#}", adapter.provider())),
            }
        }
        None
    }

    /// Maps LSP `workspace/symbol` locations onto the same-language chunks
    /// that contain them, preserving first-seen order.
    fn map_lsp_locations(
        all_chunks: &[Hit],
        lsp_locations: Vec<crate::lsp::Location>,
        candidate_count: usize,
    ) -> Vec<Hit> {
        let mut indexes = Vec::new();
        for location in lsp_locations {
            let Some(relative) = location.relative_file_path else {
                continue;
            };
            if let Some((index, _)) = all_chunks.iter().enumerate().find(|(_, hit)| {
                location.language.as_deref() == Some(hit.chunk.language.as_str())
                    && hit.chunk.relative_file_path.eq_ignore_ascii_case(&relative)
                    && hit.chunk.start_line <= location.start_line
                    && hit.chunk.end_line >= location.start_line
            }) && !indexes.contains(&index)
            {
                indexes.push(index);
            }
        }
        let mut lsp_hits = Vec::new();
        for (rank, index) in indexes.into_iter().take(candidate_count).enumerate() {
            let mut hit = all_chunks[index].clone();
            hit.lsp_rank = Some(rank + 1);
            hit.retrieval_channels.push("lsp".into());
            lsp_hits.push(hit);
        }
        lsp_hits
    }

    /// Combines the three retrieval channels with weighted Reciprocal Rank
    /// Fusion, keyed by file path, start line, and content hash so the same
    /// chunk reached through multiple channels contributes once with a summed
    /// score. A channel contributes `weight / (rrf_k + rank)`; weights of
    /// `1.0` are plain RRF, which is the configured default (the default's
    /// per-file cap is applied later and does not enter the score). The
    /// source-role prior is added last and is never scaled by a weight. The
    /// result is in no particular order.
    fn fuse_hits(
        rrf_k: f32,
        weights: &FusionConfig,
        semantic: Vec<Hit>,
        lexical_hits: Vec<Hit>,
        lsp_hits: Vec<Hit>,
    ) -> Vec<Hit> {
        let mut fused = HashMap::<(String, u32, String), Hit>::new();
        for mut hit in semantic {
            let rank = hit.semantic_rank.unwrap();
            hit.fusion_score += weights.semantic_weight / (rrf_k + rank as f32);
            fused.insert(
                (
                    hit.chunk.relative_file_path.clone(),
                    hit.chunk.start_line,
                    hit.chunk.content_hash.clone(),
                ),
                hit,
            );
        }
        for hit in lexical_hits {
            let key = (
                hit.chunk.relative_file_path.clone(),
                hit.chunk.start_line,
                hit.chunk.content_hash.clone(),
            );
            let lexical_rank = hit.lexical_rank.unwrap();
            let contribution = weights.lexical_weight / (rrf_k + lexical_rank as f32);
            if let Some(existing) = fused.get_mut(&key) {
                existing.lexical_rank = hit.lexical_rank;
                existing.lexical_match_count = hit.lexical_match_count;
                existing.lexical_score = hit.lexical_score;
                existing.fusion_score += contribution;
                existing.retrieval_channels.push("lexical".into());
            } else {
                let mut hit = hit;
                hit.fusion_score = contribution;
                fused.insert(key, hit);
            }
        }
        for hit in lsp_hits {
            let key = (
                hit.chunk.relative_file_path.clone(),
                hit.chunk.start_line,
                hit.chunk.content_hash.clone(),
            );
            let lsp_rank = hit.lsp_rank.unwrap();
            let contribution = weights.lsp_weight / (rrf_k + lsp_rank as f32);
            if let Some(existing) = fused.get_mut(&key) {
                existing.lsp_rank = hit.lsp_rank;
                existing.fusion_score += contribution;
                existing.retrieval_channels.push("lsp".into());
            } else {
                let mut hit = hit;
                hit.fusion_score = contribution;
                fused.insert(key, hit);
            }
        }
        for hit in fused.values_mut() {
            hit.fusion_score += hit.source_role_prior;
        }
        fused.into_values().collect()
    }

    /// The docs lane's candidates: the nearest prose chunks (the same query
    /// vector, a prose-only filter, `[docs_lane].semantic_candidate_count`
    /// deep) and the prose chunks the shared ripgrep pass hit, fused with the
    /// code lane's weights, `rrf_k`, and per-file cap, cut to
    /// `[docs_lane].rerank_candidate_count`. No language-server channel: a
    /// prose chunk has no symbols. Never fails: a failure is the returned
    /// pool's `warning`.
    async fn docs_pool(
        &self,
        snapshot: &Snapshot,
        workspace: &Workspace,
        filters: &EffectiveFilter,
        doc_chunks: &[Hit],
        query_vector: Option<&[f32]>,
        lexical_search: Option<lexical::LexicalSearch>,
    ) -> DocsPool {
        let started = Instant::now();
        let docs_config = &self.config.docs_lane;
        let mut warning: Option<String> = None;
        let mut warn = |message: String| {
            warning = Some(
                warning
                    .take()
                    .map_or(message.clone(), |w| format!("{w}; {message}")),
            );
        };
        let mut timings = DocsTimings::default();
        let doc_filters = lane_filter(filters, doc_chunks);
        let mut semantic = Vec::new();
        if let Some(vector) = query_vector {
            let timer = Instant::now();
            match Store::search(
                snapshot,
                workspace,
                vector,
                docs_config.semantic_candidate_count,
                &doc_filters,
                doc_chunks.len(),
            )
            .await
            {
                Ok(found) => {
                    if let Some(message) = found.cap_warning() {
                        warn(format!("Docs lane: {message}"));
                    }
                    semantic = found.hits;
                }
                Err(error) => warn(format!("Docs semantic retrieval unavailable: {error:#}")),
            }
            timings.lancedb_search_ms = ms(timer);
        }
        let mut lexical_hits = Vec::new();
        if let Some(search) = lexical_search {
            let timer = Instant::now();
            lexical_hits =
                Self::map_lexical_matches(doc_chunks, search, self.config.lexical_candidate_count);
            timings.lexical_search_ms = ms(timer);
        }
        let timer = Instant::now();
        let mut hits = Self::fuse_hits(
            self.config.rrf_k,
            &self.config.fusion,
            semantic,
            lexical_hits,
            Vec::new(),
        );
        hits.retain(|hit| hit_matches(filters, hit));
        hits.sort_by(fusion_order);
        let mut hits = cap_chunks_per_file(hits, self.config.fusion.max_chunks_per_file);
        hits.truncate(docs_config.rerank_candidate_count);
        timings.fusion_ms = ms(timer);
        timings.total_ms = ms(started);
        DocsPool {
            hits,
            warning,
            timings,
        }
    }

    /// Turns a docs pool into the report's [`DocsLane`]: the best
    /// `docs_top_k` of the pool, ordered by the reranker when the settings
    /// ask for it. Fail-open like every optional stage: a reranker that is
    /// unreachable or answers malformed data leaves the fusion order and a
    /// `warning` on the lane, nothing more.
    async fn finish_docs(
        &self,
        query: &str,
        pool: DocsPool,
        settings: &SearchSettings,
    ) -> DocsLane {
        let started = Instant::now();
        let DocsPool {
            hits,
            mut warning,
            mut timings,
        } = pool;
        let k = settings.docs_top_k;
        let candidate_count = hits.len();
        let mut reranked = false;
        let results = if hits.is_empty() || !settings.docs_reranker {
            hits.into_iter().take(k).collect()
        } else {
            let max_bytes = self.config.rerank_document_max_bytes;
            let documents: Vec<_> = hits
                .iter()
                .map(|hit| rerank_document(hit, max_bytes))
                .collect();
            let answer = self
                .models
                .rerank(query, &documents)
                .await
                .and_then(|ranks| {
                    // The documents are the only valid indexes; a reranker
                    // that names another one answered malformed data.
                    ensure!(
                        ranks.iter().all(|&(index, _)| index < hits.len()),
                        "reranker returned an index outside the {} documents sent",
                        hits.len()
                    );
                    Ok(ranks)
                });
            match answer {
                Ok(ranks) => {
                    reranked = true;
                    ranks
                        .iter()
                        .take(k)
                        .map(|&(index, score)| {
                            let mut hit = hits[index].clone();
                            hit.reranker_score = Some(score);
                            hit
                        })
                        .collect()
                }
                Err(error) => {
                    tracing::warn!(%error, "docs reranker failed; returning fusion-ranked docs");
                    let message = format!(
                        "Docs reranker unavailable or invalid response; fusion-ranked docs returned: {error:#}"
                    );
                    warning = Some(
                        warning
                            .take()
                            .map_or(message.clone(), |w| format!("{w}; {message}")),
                    );
                    hits.into_iter().take(k).collect()
                }
            }
        };
        timings.reranking_ms = ms(started);
        timings.total_ms += timings.reranking_ms;
        DocsLane {
            results,
            candidate_count,
            reranked,
            warning,
            timings,
        }
    }

    /// Reranks fused candidates (with the reranking service, or the typed
    /// System One service when `rescorer = "system_one"`; one attempt per
    /// batch, no fallback from one to the other) and writes the outcome
    /// (reranked results, or a fail-open fallback to fusion-ranked results with a warning) into
    /// `report`. Returns the reranker's full answer, as (index in `hits`,
    /// score) pairs best first, when it scored the candidates, and `None`
    /// after a fallback.
    async fn rerank_into_report(
        &self,
        query: &str,
        hits: Vec<Hit>,
        k: usize,
        report: &mut SearchReport,
    ) -> Option<Vec<(usize, f32)>> {
        let max_bytes = self.config.rerank_document_max_bytes;
        let (label, outcome) = match self.config.rescorer {
            Rescorer::Reranker => {
                let documents: Vec<_> = hits
                    .iter()
                    .map(|hit| rerank_document(hit, max_bytes))
                    .collect();
                ("Reranker", self.models.rerank(query, &documents).await)
            }
            Rescorer::SystemOne => {
                let candidates: Vec<_> = hits
                    .iter()
                    .map(|hit| wire_candidate(hit, max_bytes))
                    .collect();
                (
                    "System One rescorer",
                    self.models.system_one_rank(query, &candidates).await,
                )
            }
        };
        match outcome {
            Ok(ranks) => {
                report.reranked = true;
                report.results = ranks
                    .iter()
                    .take(k)
                    .map(|&(i, score)| {
                        let mut h = hits[i].clone();
                        h.reranker_score = Some(score);
                        h
                    })
                    .collect();
                Some(ranks)
            }
            Err(error) => {
                match self.config.rescorer {
                    Rescorer::Reranker => {
                        tracing::warn!(%error, "reranker failed; returning fusion-ranked results");
                    }
                    Rescorer::SystemOne => {
                        tracing::warn!(%error, "System One rescorer failed; returning fusion-ranked results");
                    }
                }
                // Append: earlier warnings (a refresh fallback, an
                // unavailable channel) must survive a reranker failure.
                let message = format!(
                    "{label} unavailable or invalid response; fusion-ranked results returned: {error:#}"
                );
                report.warning = Some(
                    report
                        .warning
                        .take()
                        .map_or(message.clone(), |w| format!("{w}; {message}")),
                );
                report.results = hits.into_iter().take(k).collect();
                None
            }
        }
    }
}

/// One server's state for a note: `rust-analyzer: busy for 3 s, 2 ready
/// changes`, `csharp-ls: starting for 12 s`, `pyright: stuck`.
fn readiness_clause(provider: &str, readiness: &crate::lsp::ServerReadiness) -> String {
    use crate::lsp::ServerStateName;
    let seconds = |ms: u64| ms / 1000;
    match readiness.state {
        ServerStateName::Starting => {
            format!("{provider}: starting for {} s", seconds(readiness.age_ms))
        }
        ServerStateName::Busy => format!(
            "{provider}: busy for {} s, {} ready change{}",
            seconds(readiness.changed_ms.unwrap_or(readiness.age_ms)),
            readiness.ready_changes,
            if readiness.ready_changes == 1 {
                ""
            } else {
                "s"
            }
        ),
        other => format!("{provider}: {}", other.as_str()),
    }
}

#[cfg(test)]
mod fusion_properties;

#[cfg(test)]
mod snippet_properties;

#[cfg(test)]
mod tests {
    use super::{
        App, CandidateTrace, RerankOutcome, candidate_dump_path, cap_chunks_per_file, fusion_order,
        head_within, pool_cut_note, record_rerank_scores, rerank_document, trace_candidates,
        trace_candidates_for, wire_candidate,
    };
    use crate::{
        chunk::Chunk,
        config::{FusionConfig, Rescorer},
        filter::SourceRole,
        store::{Hit, source_role_prior},
    };
    use std::{collections::HashSet, ffi::OsString, path::PathBuf};

    #[test]
    fn the_readiness_clause_names_the_state_the_duration_and_the_changes() {
        use crate::lsp::{ServerReadiness, ServerStateName};
        let readiness = |state, changed_ms, ready_changes| ServerReadiness {
            state,
            primed: false,
            progress: 0,
            ready_changes,
            age_ms: 12_300,
            changed_ms,
            idle_ms: 0,
        };
        assert_eq!(
            super::readiness_clause(
                "rust-analyzer",
                &readiness(ServerStateName::Busy, Some(3_400), 2)
            ),
            "rust-analyzer: busy for 3 s, 2 ready changes"
        );
        assert_eq!(
            super::readiness_clause(
                "rust-analyzer",
                &readiness(ServerStateName::Busy, Some(100), 1)
            ),
            "rust-analyzer: busy for 0 s, 1 ready change"
        );
        assert_eq!(
            super::readiness_clause("csharp-ls", &readiness(ServerStateName::Starting, None, 0)),
            "csharp-ls: starting for 12 s"
        );
        assert_eq!(
            super::readiness_clause("pyright", &readiness(ServerStateName::Stuck, None, 0)),
            "pyright: stuck"
        );
    }

    const RRF_K: f32 = 60.0;

    mod t0;

    fn hit_of(path: &str, language: &str) -> Hit {
        let role = if language == "markdown" {
            SourceRole::Documentation
        } else {
            SourceRole::Source
        };
        let mut hit = hit(path, 1, role);
        hit.chunk.language = language.to_owned();
        hit
    }

    #[test]
    fn prose_chunks_are_split_off_unless_the_request_names_source_roles() {
        use crate::filter::{EffectiveFilter, FilterRequest};

        let mixed = || {
            vec![
                hit_of("src/a.rs", "rust"),
                hit_of("README.md", "markdown"),
                hit_of("src/b.py", "python"),
            ]
        };
        let paths = |hits: &[Hit]| -> Vec<String> {
            hits.iter()
                .map(|hit| hit.chunk.relative_file_path.clone())
                .collect()
        };

        let open = EffectiveFilter::from_request(&FilterRequest::default()).unwrap();
        let (code, prose) = super::split_prose(&open, mixed());
        assert_eq!(paths(&code), ["src/a.rs", "src/b.py"]);
        assert_eq!(paths(&prose), ["README.md"]);

        // An explicit role filter keeps one list, whatever it names.
        let named = EffectiveFilter::from_request(&FilterRequest {
            source_roles: Some(vec!["documentation".to_owned()]),
            ..Default::default()
        })
        .unwrap();
        let (code, prose) = super::split_prose(&named, mixed());
        assert_eq!(code.len(), 3);
        assert!(prose.is_empty());

        // Only prose passes the filters: nothing to protect, one list.
        let (code, prose) = super::split_prose(&open, vec![hit_of("README.md", "markdown")]);
        assert_eq!(paths(&code), ["README.md"]);
        assert!(prose.is_empty());

        // A Rust file under docs/ is documentation by role, code by extractor.
        let mut rust_doc = hit_of("docs/example.rs", "rust");
        rust_doc.source_role = SourceRole::Documentation;
        let (code, prose) =
            super::split_prose(&open, vec![rust_doc, hit_of("README.md", "markdown")]);
        assert_eq!(paths(&code), ["docs/example.rs"]);
        assert_eq!(paths(&prose), ["README.md"]);
    }

    #[test]
    fn a_lane_filter_names_the_distinct_languages_of_its_chunks() {
        use crate::filter::{EffectiveFilter, FilterRequest};

        let open = EffectiveFilter::from_request(&FilterRequest::default()).unwrap();
        let chunks = vec![
            hit_of("a.rs", "rust"),
            hit_of("b.py", "python"),
            hit_of("c.rs", "rust"),
        ];
        let narrowed = super::lane_filter(&open, &chunks);
        assert_eq!(
            narrowed.languages,
            Some(vec!["python".to_owned(), "rust".to_owned()])
        );
    }

    thread_local! {
        pub(super) static LEXICAL_PATH_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    #[test]
    fn lexical_path_lookup_preserves_the_old_filter_file_set() {
        let paths = [
            "src/main.rs",
            "SRC/MAIN.RS",
            "main.rs",
            "ain.rs",
            "other/main.rs",
            "src/é.rs",
        ];
        let mut file_ids = std::collections::HashMap::<String, Vec<usize>>::new();
        for (id, path) in paths.iter().enumerate() {
            file_ids.entry(path.to_lowercase()).or_default().push(id);
        }
        let mut cases = vec![
            ("src/main.rs", vec![0, 1, 2]),
            ("SrC/MAIN.rs", vec![0, 1, 2]),
            ("prefix/src/main.rs", vec![0, 1, 2]),
            ("main.rs", vec![2]),
            ("other/main.rs", vec![2, 4]),
            ("prefix/src/É.rs", vec![5]),
            ("prefix/notmain.rs", vec![]),
            ("prefix/src/main.rs.bak", vec![]),
            ("missing.rs", vec![]),
        ];
        // A backslash separates components on Windows only (N90).
        if cfg!(windows) {
            cases.push(("prefix\\src\\MAIN.rs", vec![0, 1, 2]));
        }
        for (reported, expected) in cases {
            let normalized = crate::path::normalize_path_separators(reported).to_lowercase();
            let old: Vec<_> = paths
                .iter()
                .enumerate()
                .filter(|(_, path)| {
                    let path = path.to_lowercase();
                    normalized == path || normalized.ends_with(&format!("/{path}"))
                })
                .map(|(id, _)| id)
                .collect();
            assert_eq!(old, expected, "{reported}");
            let mut found = super::lookup_lexical_files(&file_ids, reported);
            found.sort_unstable();
            assert_eq!(found, old, "{reported}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn lexical_path_lookup_keeps_unix_backslashes_in_the_filename() {
        let file_ids = std::collections::HashMap::from([
            (r"a\b.rs".to_owned(), vec![0]),
            ("a/b.rs".to_owned(), vec![1]),
        ]);

        assert_eq!(super::lookup_lexical_files(&file_ids, r"a\b.rs"), [0]);
    }

    #[cfg(unix)]
    #[test]
    fn lexical_match_is_attributed_to_the_indexed_backslash_filename() {
        let chunks = vec![
            hit(r"a\b.rs", 1, SourceRole::Source),
            hit("a/b.rs", 1, SourceRole::Source),
        ];
        let results = App::map_lexical_matches(
            &chunks,
            crate::lexical::LexicalSearch {
                terms: vec!["needle".into()],
                matches: vec![crate::lexical::LexicalMatch {
                    relative_path: r"a\b.rs".into(),
                    line: 1,
                    term_mask: 1,
                }],
                warning: None,
            },
            8,
        );

        assert_eq!(
            results
                .iter()
                .map(|hit| hit.chunk.relative_file_path.as_str())
                .collect::<Vec<_>>(),
            [r"a\b.rs"]
        );
    }

    #[test]
    fn lexical_path_lookup_work_is_bounded_by_reported_path_depth() {
        let all_chunks: Vec<_> = (0..64)
            .map(|id| hit(&format!("src/file_{id}.rs"), 1, SourceRole::Source))
            .collect();
        let matches: Vec<_> = all_chunks[..8]
            .iter()
            .map(|hit| crate::lexical::LexicalMatch {
                relative_path: hit.chunk.relative_file_path.clone(),
                line: 1,
                term_mask: 1,
            })
            .collect();
        let probe_limit: usize = matches
            .iter()
            .map(|matched| 1 + matched.relative_path.matches('/').count())
            .sum();
        LEXICAL_PATH_PROBES.with(|probes| probes.set(0));
        let hits = App::map_lexical_matches(
            &all_chunks,
            crate::lexical::LexicalSearch {
                terms: vec!["needle".into()],
                matches,
                warning: None,
            },
            8,
        );
        assert_eq!(hits.len(), 8);
        let probes = LEXICAL_PATH_PROBES.with(|probes| probes.get());
        assert!(
            probes >= 8,
            "{probes} path probes: the lexical path lookup was not used for every reported path"
        );
        assert!(
            probes <= probe_limit,
            "{probes} path probes exceeded the path-depth bound of {probe_limit}"
        );
    }

    #[test]
    fn a_note_names_top_k_and_the_window_only_when_the_cut_leaves_fewer_results_than_asked() {
        let note = pool_cut_note(40, 24, 60).unwrap();
        assert!(
            note.contains("top_k 40")
                && note.contains("rerank_candidate_count = 24")
                && note.contains("of 60 candidates"),
            "{note}"
        );
        // top_k fits in the window, or the pool was never cut: nothing to say.
        assert_eq!(pool_cut_note(24, 24, 60), None);
        assert_eq!(pool_cut_note(8, 24, 60), None);
        assert_eq!(pool_cut_note(40, 24, 24), None);
        assert_eq!(pool_cut_note(40, 24, 10), None);
    }

    #[test]
    fn the_candidate_dump_is_off_unless_the_variable_names_a_file() {
        assert_eq!(candidate_dump_path(None), None);
        for blank in ["", "   ", "\t"] {
            assert_eq!(candidate_dump_path(Some(OsString::from(blank))), None);
        }
        assert_eq!(
            candidate_dump_path(Some(OsString::from("logs/candidates-s1.jsonl"))),
            Some(PathBuf::from("logs/candidates-s1.jsonl"))
        );
    }

    #[test]
    fn head_within_keeps_short_text_and_cuts_on_a_char_boundary() {
        assert_eq!(head_within("fn main() {}", 64), "fn main() {}");
        assert_eq!(head_within("abcdef", 3), "abc");
        // "é" is two bytes; a cut inside it backs off to the boundary.
        assert_eq!(head_within("caé", 3), "ca");
        assert_eq!(head_within("caé", 4), "caé");
    }

    /// A chunk as `Store::chunks` hands it to a channel: no channel has
    /// returned it yet, so it carries no rank, score, or channel name.
    fn hit(path: &str, start_line: u32, role: SourceRole) -> Hit {
        Hit {
            file_path: format!("/workspace/{path}"),
            chunk: Chunk {
                relative_file_path: path.into(),
                language: "rust".into(),
                start_line,
                end_line: start_line + 9,
                code: format!("fn at_{start_line}() {{}}"),
                content_hash: format!("{path}:{start_line}"),
            },
            semantic_score: None,
            semantic_rank: None,
            lexical_rank: None,
            lexical_match_count: 0,
            lexical_score: None,
            lsp_rank: None,
            fusion_score: 0.0,
            retrieval_channels: vec![],
            reranker_score: None,
            source_role: role,
            source_role_prior: source_role_prior(role),
        }
    }

    fn semantic_at(mut hit: Hit, rank: usize) -> Hit {
        hit.semantic_rank = Some(rank);
        hit.semantic_score = Some(1.0 - rank as f32 / 100.0);
        hit.retrieval_channels.push("semantic".into());
        hit
    }

    fn lexical_at(mut hit: Hit, rank: usize) -> Hit {
        hit.lexical_rank = Some(rank);
        hit.lexical_match_count = 3;
        hit.lexical_score = Some(5.0 / rank as f32);
        hit.retrieval_channels.push("lexical".into());
        hit
    }

    fn lsp_at(mut hit: Hit, rank: usize) -> Hit {
        hit.lsp_rank = Some(rank);
        hit.retrieval_channels.push("lsp".into());
        hit
    }

    /// One chunk and the rank each channel gave it (`None`: not returned).
    #[derive(Clone, Copy)]
    struct Row {
        path: &'static str,
        role: SourceRole,
        semantic: Option<usize>,
        lexical: Option<usize>,
        lsp: Option<usize>,
    }

    const fn row(
        path: &'static str,
        role: SourceRole,
        semantic: Option<usize>,
        lexical: Option<usize>,
        lsp: Option<usize>,
    ) -> Row {
        Row {
            path,
            role,
            semantic,
            lexical,
            lsp,
        }
    }

    /// The three channel inputs fusion receives, each ordered by rank.
    fn channels(rows: &[Row]) -> (Vec<Hit>, Vec<Hit>, Vec<Hit>) {
        let build = |rank_of: fn(&Row) -> Option<usize>, tag: fn(Hit, usize) -> Hit| {
            let mut ranked: Vec<(usize, Hit)> = rows
                .iter()
                .filter_map(|row| {
                    rank_of(row).map(|rank| (rank, tag(hit(row.path, 1, row.role), rank)))
                })
                .collect();
            ranked.sort_by_key(|(rank, _)| *rank);
            ranked.into_iter().map(|(_, hit)| hit).collect::<Vec<_>>()
        };
        (
            build(|row| row.semantic, semantic_at),
            build(|row| row.lexical, lexical_at),
            build(|row| row.lsp, lsp_at),
        )
    }

    /// Fuses the rows and orders the result the way `search` does.
    fn fuse(rrf_k: f32, weights: &FusionConfig, rows: &[Row]) -> Vec<Hit> {
        let (semantic, lexical, lsp) = channels(rows);
        let mut hits = App::fuse_hits(rrf_k, weights, semantic, lexical, lsp);
        hits.sort_by(fusion_order);
        hits
    }

    fn paths(hits: &[Hit]) -> Vec<&str> {
        hits.iter()
            .map(|hit| hit.chunk.relative_file_path.as_str())
            .collect()
    }

    fn by_path<'a>(hits: &'a [Hit], path: &str) -> &'a Hit {
        hits.iter()
            .find(|hit| hit.chunk.relative_file_path == path)
            .unwrap_or_else(|| panic!("{path} was not fused"))
    }

    /// Weighted RRF written out term by term, in the order fusion adds them
    /// (semantic, lexical, LSP) and with the prior added last.
    fn expected_score(rrf_k: f32, parts: &[(f32, usize)], prior: f32) -> f32 {
        let mut score = 0.0_f32;
        for (weight, rank) in parts {
            score += weight / (rrf_k + *rank as f32);
        }
        score + prior
    }

    #[test]
    fn unit_weights_match_plain_rrf() {
        use SourceRole::{Source, Test};
        let rows = [
            row("a.rs", Source, Some(1), Some(3), None),
            row("b.rs", Source, Some(2), None, None),
            row("c.rs", Source, None, Some(1), None),
            row("d.rs", Source, Some(3), Some(2), Some(1)),
            row("e.rs", Source, None, None, Some(2)),
            row("tests/f.rs", Test, Some(4), None, Some(3)),
            row("g.rs", Source, Some(5), Some(7), None),
            row("h.rs", Source, None, Some(4), Some(4)),
        ];
        for rrf_k in [RRF_K, 7.5, 1.0] {
            // Plain RRF exactly as it was before channel weights existed: a
            // literal `1.0 / (rrf_k + rank)` per channel, then the prior.
            let mut expected: Vec<(&str, f32)> = rows
                .iter()
                .map(|row| {
                    let mut score = 0.0_f32;
                    for rank in [row.semantic, row.lexical, row.lsp].into_iter().flatten() {
                        score += 1.0 / (rrf_k + rank as f32);
                    }
                    (row.path, score + source_role_prior(row.role))
                })
                .collect();
            expected.sort_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(b.0)));

            let fused = fuse(rrf_k, &FusionConfig::UNIT, &rows);
            assert_eq!(fused.len(), rows.len(), "rrf_k {rrf_k}");
            for (hit, (path, score)) in fused.iter().zip(&expected) {
                assert_eq!(hit.chunk.relative_file_path, *path, "order, rrf_k {rrf_k}");
                assert_eq!(
                    hit.fusion_score.to_bits(),
                    score.to_bits(),
                    "{path} at rrf_k {rrf_k}: {} vs {score}",
                    hit.fusion_score
                );
            }

            // The default weights are plain RRF too, so the default scores
            // every chunk bit for bit as `UNIT` does (its file cap acts after
            // fusion and does not enter the score).
            let by_default = fuse(rrf_k, &FusionConfig::default(), &rows);
            assert_eq!(paths(&by_default), paths(&fused), "rrf_k {rrf_k}");
            for (a, b) in by_default.iter().zip(&fused) {
                assert_eq!(
                    a.fusion_score.to_bits(),
                    b.fusion_score.to_bits(),
                    "{} at rrf_k {rrf_k}",
                    a.chunk.relative_file_path
                );
            }

            // Weighting the semantic channel 2x is an explicit choice: it
            // doubles only the semantic term (semantic-only b.rs) and leaves a
            // lexical-only chunk (c.rs) exactly as plain RRF scores it.
            let weighted = fuse(
                rrf_k,
                &FusionConfig {
                    semantic_weight: 2.0,
                    ..FusionConfig::UNIT
                },
                &rows,
            );
            assert_eq!(
                by_path(&weighted, "b.rs").fusion_score.to_bits(),
                expected_score(rrf_k, &[(2.0, 2)], source_role_prior(Source)).to_bits(),
                "rrf_k {rrf_k}"
            );
            assert_eq!(
                by_path(&weighted, "c.rs").fusion_score.to_bits(),
                by_path(&fused, "c.rs").fusion_score.to_bits(),
                "rrf_k {rrf_k}"
            );
        }
    }

    #[test]
    fn semantic_weight_two_promotes_semantic_only_rank_one_over_two_channel_agreement() {
        use SourceRole::Source;
        let rows = [
            // Semantic's single best hit, which no other channel found.
            row("x.rs", Source, Some(1), None, None),
            // Lexical and LSP agree on it; semantic did not return it.
            row("y.rs", Source, None, Some(1), Some(3)),
            // A semantic and lexical agreement, to show only the semantic term
            // of a mixed chunk is scaled.
            row("z.rs", Source, Some(2), Some(2), None),
        ];
        let unit = fuse(RRF_K, &FusionConfig::UNIT, &rows);
        assert_eq!(paths(&unit), ["y.rs", "z.rs", "x.rs"]);

        let doubled = fuse(
            RRF_K,
            &FusionConfig {
                semantic_weight: 2.0,
                ..FusionConfig::UNIT
            },
            &rows,
        );
        assert_eq!(paths(&doubled), ["z.rs", "x.rs", "y.rs"]);
        // Doubling is an explicit choice: the default weights are plain RRF
        // and fuse exactly as `unit` does.
        let by_default = fuse(RRF_K, &FusionConfig::default(), &rows);
        assert_eq!(paths(&by_default), paths(&unit));
        for (a, b) in by_default.iter().zip(&unit) {
            assert_eq!(a.fusion_score.to_bits(), b.fusion_score.to_bits());
        }

        let prior = source_role_prior(Source);
        // Only the semantic contributions double.
        assert_eq!(
            by_path(&doubled, "x.rs").fusion_score.to_bits(),
            expected_score(RRF_K, &[(2.0, 1)], prior).to_bits()
        );
        assert_eq!(
            by_path(&doubled, "z.rs").fusion_score.to_bits(),
            expected_score(RRF_K, &[(2.0, 2), (1.0, 2)], prior).to_bits()
        );
        // A chunk the semantic channel never returned is untouched.
        assert_eq!(
            by_path(&doubled, "y.rs").fusion_score.to_bits(),
            by_path(&unit, "y.rs").fusion_score.to_bits()
        );
        // The weights do not touch ranks or the channel list.
        let x = by_path(&doubled, "x.rs");
        assert_eq!(x.semantic_rank, Some(1));
        assert_eq!(x.retrieval_channels, ["semantic"]);
    }

    #[test]
    fn prior_is_added_after_weighting() {
        use SourceRole::{Source, Test};
        assert_eq!(source_role_prior(Source), 0.004);
        assert_eq!(source_role_prior(Test), 0.0);

        let rows = [
            row("src.rs", Source, Some(1), None, None),
            row("tests/t.rs", Test, Some(1), None, None),
            row("all.rs", Source, Some(1), Some(1), Some(1)),
        ];
        let weights = FusionConfig {
            semantic_weight: 3.0,
            lexical_weight: 0.5,
            lsp_weight: 4.0,
            max_chunks_per_file: 0,
        };
        let fused = fuse(RRF_K, &weights, &rows);

        let source = by_path(&fused, "src.rs");
        let test = by_path(&fused, "tests/t.rs");
        // The prior is the one thing added on top of the weighted terms ...
        assert_eq!(
            source.fusion_score.to_bits(),
            expected_score(RRF_K, &[(3.0, 1)], 0.004).to_bits()
        );
        assert_eq!(
            test.fusion_score.to_bits(),
            expected_score(RRF_K, &[(3.0, 1)], 0.0).to_bits()
        );
        // ... and it is not multiplied by the weight (3 * (1/61 + 0.004)).
        assert_ne!(
            source.fusion_score.to_bits(),
            (3.0 * (1.0 / (RRF_K + 1.0) + 0.004)).to_bits()
        );
        assert!((source.fusion_score - test.fusion_score - 0.004).abs() < 1e-6);
        assert_eq!(source.source_role_prior, 0.004);
        assert_eq!(test.source_role_prior, 0.0);

        // Three channels, three different weights, one prior.
        assert_eq!(
            by_path(&fused, "all.rs").fusion_score.to_bits(),
            expected_score(RRF_K, &[(3.0, 1), (0.5, 1), (4.0, 1)], 0.004).to_bits()
        );
    }

    #[test]
    fn lexical_fields_copied_on_merge() {
        use SourceRole::Source;
        let base = hit("merged.rs", 1, Source);
        let semantic = vec![semantic_at(base.clone(), 4)];
        let mut lexical_hit = lexical_at(base.clone(), 2);
        lexical_hit.lexical_match_count = 7;
        lexical_hit.lexical_score = Some(3.5);
        let lexical = vec![lexical_hit];
        let lsp = vec![lsp_at(base, 5)];

        let fused = App::fuse_hits(RRF_K, &FusionConfig::UNIT, semantic, lexical, lsp);
        assert_eq!(fused.len(), 1, "one chunk reached through three channels");
        let merged = &fused[0];
        assert_eq!(merged.semantic_rank, Some(4));
        assert_eq!(merged.semantic_score, Some(1.0 - 4.0 / 100.0));
        assert_eq!(merged.lexical_rank, Some(2));
        assert_eq!(merged.lexical_match_count, 7);
        assert_eq!(merged.lexical_score, Some(3.5));
        assert_eq!(merged.lsp_rank, Some(5));
        assert_eq!(merged.retrieval_channels, ["semantic", "lexical", "lsp"]);
        assert_eq!(
            merged.fusion_score.to_bits(),
            expected_score(RRF_K, &[(1.0, 4), (1.0, 2), (1.0, 5)], 0.004).to_bits()
        );

        // A chunk only the lexical channel returned keeps its own fields.
        let alone = lexical_at(hit("alone.rs", 1, Source), 1);
        let fused = App::fuse_hits(RRF_K, &FusionConfig::UNIT, vec![], vec![alone], vec![]);
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].lexical_rank, Some(1));
        assert_eq!(fused[0].lexical_match_count, 3);
        assert_eq!(fused[0].lexical_score, Some(5.0));
        assert_eq!(fused[0].retrieval_channels, ["lexical"]);
    }

    #[test]
    fn equal_score_chunks_with_the_same_path_and_start_line_have_a_stable_order() {
        use SourceRole::Source;
        let mut semantic = semantic_at(hit("same.rs", 1, Source), 1);
        semantic.chunk.end_line = 3;
        semantic.chunk.content_hash = "z-semantic".into();
        let mut lexical = lexical_at(hit("same.rs", 1, Source), 1);
        lexical.chunk.end_line = 2;
        lexical.chunk.content_hash = "a-lexical".into();
        let fused = App::fuse_hits(
            RRF_K,
            &FusionConfig::UNIT,
            vec![semantic],
            vec![lexical],
            vec![],
        );
        assert_eq!(fused.len(), 2);
        assert_eq!(fused[0].fusion_score, fused[1].fusion_score);

        let ordered_hashes = |mut hits: Vec<Hit>| {
            hits.sort_by(fusion_order);
            hits.into_iter()
                .map(|hit| hit.chunk.content_hash)
                .collect::<Vec<_>>()
        };
        let forward = ordered_hashes(fused.clone());
        let reverse = ordered_hashes(fused.iter().cloned().rev().collect());
        assert_eq!(forward, reverse);
        assert_eq!(forward, ["a-lexical", "z-semantic"]);

        let same_end_line = |mut hits: Vec<Hit>| {
            for hit in &mut hits {
                hit.chunk.end_line = 3;
            }
            hits
        };
        let forward = ordered_hashes(same_end_line(fused.clone()));
        let reverse = ordered_hashes(same_end_line(fused.iter().cloned().rev().collect()));
        assert_eq!(forward, reverse);
        assert_eq!(forward, ["a-lexical", "z-semantic"]);
    }

    /// A fused order of `(file, start line)` chunks, best first, with strictly
    /// decreasing scores (1.00, 0.99, ...) so a demotion is visible in them.
    fn fused_order(chunks: &[(&str, u32)]) -> Vec<Hit> {
        chunks
            .iter()
            .enumerate()
            .map(|(position, (path, start_line))| {
                let mut chunk = hit(path, *start_line, SourceRole::Source);
                chunk.fusion_score = 1.0 - position as f32 * 0.01;
                chunk
            })
            .collect()
    }

    fn order(hits: &[Hit]) -> Vec<(&str, u32)> {
        hits.iter()
            .map(|hit| (hit.chunk.relative_file_path.as_str(), hit.chunk.start_line))
            .collect()
    }

    /// File `a.rs` holds three chunks, interleaved with `b.rs` (two) and
    /// `c.rs` (one): the shape that fills a top 3 with one file.
    const INTERLEAVED: [(&str, u32); 6] = [
        ("a.rs", 1),
        ("b.rs", 1),
        ("a.rs", 11),
        ("c.rs", 1),
        ("a.rs", 21),
        ("b.rs", 11),
    ];

    #[test]
    fn the_per_file_cap_reorders_and_never_removes() {
        // cap zero is the identity
        {
            let hits = fused_order(&INTERLEAVED);
            let capped = cap_chunks_per_file(hits.clone(), 0);
            assert_eq!(order(&capped), INTERLEAVED);
            for (before, after) in hits.iter().zip(&capped) {
                assert_eq!(before.fusion_score.to_bits(), after.fusion_score.to_bits());
            }
            assert!(cap_chunks_per_file(Vec::new(), 0).is_empty());
            assert!(cap_chunks_per_file(Vec::new(), 2).is_empty());
        }

        // cap one moves every repeat of a file behind the first chunk of each file
        {
            let capped = cap_chunks_per_file(fused_order(&INTERLEAVED), 1);
            // Head: the first chunk of each file, in fused order. Tail: the
            // demoted chunks, in fused order.
            assert_eq!(
                order(&capped),
                [
                    ("a.rs", 1),
                    ("b.rs", 1),
                    ("c.rs", 1),
                    ("a.rs", 11),
                    ("a.rs", 21),
                    ("b.rs", 11),
                ]
            );
            // A top 3 is now three distinct files instead of a.rs, b.rs, a.rs.
            let top_three: HashSet<_> = order(&capped[..3]).into_iter().map(|(p, _)| p).collect();
            assert_eq!(top_three.len(), 3);
            // Nothing is removed: the full list is still there.
            assert_eq!(capped.len(), INTERLEAVED.len());
        }

        // cap two demotes only the third chunk of a file
        {
            let capped = cap_chunks_per_file(fused_order(&INTERLEAVED), 2);
            assert_eq!(
                order(&capped),
                [
                    ("a.rs", 1),
                    ("b.rs", 1),
                    ("a.rs", 11),
                    ("c.rs", 1),
                    ("b.rs", 11),
                    ("a.rs", 21),
                ]
            );
            // A cap at or above the largest per-file count changes nothing.
            for max in [3, 4, 100] {
                let capped = cap_chunks_per_file(fused_order(&INTERLEAVED), max);
                assert_eq!(order(&capped), INTERLEAVED, "cap {max}");
            }
        }

        // the cap changes only the order, never a score or a chunk
        {
            let hits = fused_order(&INTERLEAVED);
            let capped = cap_chunks_per_file(hits.clone(), 1);
            // Same chunks, bit-identical scores and fields, by chunk identity.
            for hit in &hits {
                let moved = capped
                    .iter()
                    .find(|other| other.chunk.content_hash == hit.chunk.content_hash)
                    .expect("a demoted chunk is still in the list");
                assert_eq!(moved.fusion_score.to_bits(), hit.fusion_score.to_bits());
                assert_eq!(moved.source_role_prior, hit.source_role_prior);
                assert_eq!(moved.chunk.start_line, hit.chunk.start_line);
            }
            // Scores are not rescaled, so the order is no longer by score: a.rs
            // line 11 (0.98) now follows c.rs line 1 (0.97).
            assert!(capped[3].fusion_score > capped[2].fusion_score);
            assert_eq!(order(&capped)[2], ("c.rs", 1));
            assert_eq!(order(&capped)[3], ("a.rs", 11));
        }

        // the cap is stable for the kept chunks and for the demoted ones
        {
            // Four files, five chunks each, round-robin: x1 y1 z1 w1 x2 y2 ...
            let mut chunks = Vec::new();
            for line in 1..=5_u32 {
                for path in ["w.rs", "x.rs", "y.rs", "z.rs"] {
                    chunks.push((path, line));
                }
            }
            for max in 1..=4_u32 {
                let capped = cap_chunks_per_file(fused_order(&chunks), max);
                assert_eq!(capped.len(), chunks.len(), "cap {max}");

                // The first `max` chunks of every file are the head; the rest are
                // the tail. Both keep the original relative order.
                let keep = |(_, line): &&(&str, u32)| *line <= max;
                let head: Vec<_> = chunks.iter().filter(keep).copied().collect();
                let tail: Vec<_> = chunks.iter().filter(|c| !keep(c)).copied().collect();
                let expected: Vec<_> = head.iter().chain(&tail).copied().collect();
                assert_eq!(order(&capped), expected, "cap {max}");

                // Applying it again changes nothing.
                let again = cap_chunks_per_file(capped.clone(), max);
                assert_eq!(order(&again), order(&capped), "cap {max} idempotent");
            }
        }

        // a single file keeps its order whatever the cap
        {
            let only_a = [("a.rs", 1), ("a.rs", 11), ("a.rs", 21), ("a.rs", 31)];
            for max in [0, 1, 2, 3, 4, 100] {
                let capped = cap_chunks_per_file(fused_order(&only_a), max);
                assert_eq!(order(&capped), only_a, "cap {max}");
            }
        }

        // files are told apart by relative path, not by name
        {
            // `src/lib.rs` and `tests/lib.rs` share a file name but are two files,
            // so a cap of 1 demotes nothing.
            let chunks = [("src/lib.rs", 1), ("tests/lib.rs", 1), ("a.rs", 1)];
            let capped = cap_chunks_per_file(fused_order(&chunks), 1);
            assert_eq!(order(&capped), chunks);
        }
    }

    // -----------------------------------------------------------------------
    // Candidate trace (`evaluate --dump-candidates`)
    // -----------------------------------------------------------------------

    /// `(path, start line)` of each traced row.
    fn traced(rows: &[CandidateTrace]) -> Vec<(&str, u32)> {
        rows.iter()
            .map(|row| (row.relative_file_path.as_str(), row.start_line))
            .collect()
    }

    #[test]
    fn the_trace_records_fused_and_capped_ranks_and_shortlist_membership() {
        // The interleaved list: a.rs three times among b.rs (two) and c.rs.
        // Cap 2 demotes only the third a.rs chunk; the shortlist holds 4.
        let hits = fused_order(&INTERLEAVED);
        let rows = trace_candidates(&hits, 2, 4, 1024);
        assert_eq!(traced(&rows), INTERLEAVED, "rows are in fused order");
        let fused: Vec<usize> = rows.iter().map(|row| row.fused_rank).collect();
        assert_eq!(fused, [1, 2, 3, 4, 5, 6]);
        // Capped order: a1 b1 a11 c1 b11 a21.
        let capped: Vec<usize> = rows.iter().map(|row| row.capped_rank).collect();
        assert_eq!(capped, [1, 2, 3, 4, 6, 5]);
        let shortlisted: Vec<bool> = rows.iter().map(|row| row.shortlisted).collect();
        assert_eq!(shortlisted, [true, true, true, true, false, false]);

        // The positions agree with what the search itself does to the hits.
        let capped_hits = cap_chunks_per_file(hits.clone(), 2);
        let mut shortlist: Vec<_> = rows.iter().filter(|row| row.shortlisted).collect();
        shortlist.sort_by_key(|row| row.capped_rank);
        let shortlist: Vec<_> = shortlist
            .into_iter()
            .map(|row| (row.relative_file_path.as_str(), row.start_line))
            .collect();
        assert_eq!(shortlist, order(&capped_hits[..4]));

        // Without a cap the capped order is the fused order.
        for row in trace_candidates(&hits, 0, 4, 1024) {
            assert_eq!(row.capped_rank, row.fused_rank);
            assert_eq!(row.shortlisted, row.fused_rank <= 4);
        }
        // A shortlist longer than the pool holds everything.
        assert!(
            trace_candidates(&hits, 2, 40, 1024)
                .iter()
                .all(|row| row.shortlisted)
        );
        assert!(trace_candidates(&[], 2, 4, 1024).is_empty());
    }

    #[test]
    fn the_trace_sizes_the_document_the_reranker_is_given() {
        let hits = fused_order(&INTERLEAVED);
        // Every code is `fn at_<line>() {}`: 12 bytes for line 1, 13 for 11
        // and 21. A document is the path, a newline, then the code.
        let rows = trace_candidates(&hits, 2, 4, 1024);
        for (row, hit) in rows.iter().zip(&hits).filter(|(row, _)| row.shortlisted) {
            let document = rerank_document(hit, 1024);
            assert_eq!(row.document_bytes, Some(document.len()));
            assert_eq!(row.est_tokens, Some(document.len() / 4));
        }
        assert_eq!(rows[0].document_bytes, Some("a.rs\n".len() + 12));
        // The rows past the shortlist were never going to be sent.
        assert!(rows[4..].iter().all(|row| row.document_bytes.is_none()));
        assert!(rows[4..].iter().all(|row| row.est_tokens.is_none()));

        // The code is cut to the byte budget, as `rerank_document` cuts it.
        let cut = trace_candidates(&hits, 2, 4, 8);
        assert_eq!(cut[0].document_bytes, Some("a.rs\n".len() + 8));
        assert_eq!(cut[0].est_tokens, Some(13 / 4));
        assert_eq!(rerank_document(&hits[0], 8), "a.rs\nfn at_1(");
        // The snippet is the bounded evidence, not the reranker document.
        assert_eq!(cut[0].snippet, hits[0].chunk.code);
    }

    #[test]
    fn the_system_one_trace_sizes_the_snippet_that_is_transmitted() {
        use lci_core::provider::WIRE_SNIPPET_CHARS;

        let mut hits = fused_order(&INTERLEAVED);
        hits[0].chunk.code = "x".repeat(4096);
        // A multibyte chunk: 3-byte characters, cut by characters not bytes.
        hits[1].chunk.code = "\u{20ac}".repeat(1600);
        // A document budget above the multibyte chunk's size, so the snippet
        // limit is what cuts it (4096 would cut it first).
        let rows = trace_candidates_for(&hits, 2, 4, 8192, Rescorer::SystemOne);

        let reranker_rows = trace_candidates_for(&hits, 2, 4, 8192, Rescorer::Reranker);
        assert_eq!(
            reranker_rows[0].document_bytes,
            Some("a.rs\n".len() + 4096),
            "the reranker is sent the long document"
        );

        // The snippet is the first 1500 characters of `path\ncode`.
        assert_eq!(rows[0].document_bytes, Some(WIRE_SNIPPET_CHARS));
        assert_eq!(rows[0].est_tokens, Some(WIRE_SNIPPET_CHARS / 4));
        // The default 4096-byte budget: a full chunk is sent as 1500 bytes.
        let default_budget = trace_candidates_for(&hits, 2, 4, 4096, Rescorer::SystemOne);
        assert_eq!(default_budget[0].document_bytes, Some(WIRE_SNIPPET_CHARS));
        let multibyte = wire_candidate(&hits[1], 8192).snippet;
        assert_eq!(multibyte.chars().count(), WIRE_SNIPPET_CHARS);
        assert!(multibyte.len() > WIRE_SNIPPET_CHARS);
        assert_eq!(rows[1].document_bytes, Some(multibyte.len()));
        // A short chunk is sent whole, as before.
        assert_eq!(rows[2].document_bytes, reranker_rows[2].document_bytes);
        assert!(rows[4..].iter().all(|row| row.document_bytes.is_none()));
    }

    #[test]
    fn rerank_scores_land_on_the_shortlisted_rows_by_request_index() {
        let hits = fused_order(&INTERLEAVED);
        let mut rows = trace_candidates(&hits, 2, 4, 1024);
        // The request held the shortlist in capped order: index 0 is a1, 1 is
        // b1, 2 is a11, 3 is c1. The reranker prefers c1, then b1 and a11
        // tied (the lower request index first), then a1.
        record_rerank_scores(&mut rows, &[(3, 0.9), (1, 0.5), (2, 0.5), (0, 0.1)]);
        let outcome: Vec<_> = rows
            .iter()
            .map(|row| (row.rerank_rank, row.reranker_score))
            .collect();
        assert_eq!(
            outcome,
            [
                (Some(4), Some(0.1)),
                (Some(2), Some(0.5)),
                (Some(3), Some(0.5)),
                (Some(1), Some(0.9)),
                (None, None),
                (None, None),
            ]
        );
        // Without scores (reranker off or failed) nothing is set.
        let rows = trace_candidates(&hits, 2, 4, 1024);
        assert!(
            rows.iter()
                .all(|row| row.rerank_rank.is_none() && row.reranker_score.is_none())
        );
    }

    #[test]
    fn traced_ranks_weights_and_prior_reproduce_every_fusion_score_bit_for_bit() {
        use SourceRole::{Source, Test};
        let rows = [
            row("a.rs", Source, Some(1), Some(3), None),
            row("b.rs", Source, Some(2), None, None),
            row("c.rs", Source, None, Some(1), None),
            row("d.rs", Source, Some(3), Some(2), Some(1)),
            row("e.rs", Source, None, None, Some(2)),
            row("tests/f.rs", Test, Some(4), None, Some(3)),
            row("g.rs", Source, Some(5), Some(7), None),
            row("h.rs", Source, None, Some(4), Some(4)),
        ];
        let weightings = [
            (RRF_K, FusionConfig::default()),
            (RRF_K, FusionConfig::UNIT),
            (
                7.5,
                FusionConfig {
                    semantic_weight: 3.0,
                    lexical_weight: 0.5,
                    lsp_weight: 4.0,
                    max_chunks_per_file: 0,
                },
            ),
            (1.0, FusionConfig::default()),
        ];
        for (rrf_k, weights) in weightings {
            let fused = fuse(rrf_k, &weights, &rows);
            let traced = trace_candidates(&fused, weights.max_chunks_per_file, 4, 1024);
            // A dump is read back from JSON before a replay uses it.
            let traced: Vec<CandidateTrace> =
                serde_json::from_str(&serde_json::to_string(&traced).unwrap()).unwrap();
            assert_eq!(traced.len(), rows.len());
            for row in &traced {
                // The documented formula, term by term.
                let mut score = 0.0_f32;
                if let Some(rank) = row.semantic_rank {
                    score += weights.semantic_weight / (rrf_k + rank as f32);
                }
                if let Some(rank) = row.lexical_rank {
                    score += weights.lexical_weight / (rrf_k + rank as f32);
                }
                if let Some(rank) = row.lsp_rank {
                    score += weights.lsp_weight / (rrf_k + rank as f32);
                }
                score += row.prior;
                assert_eq!(
                    score.to_bits(),
                    row.fusion_score.to_bits(),
                    "{} at rrf_k {rrf_k}, {weights:?}",
                    row.relative_file_path
                );
            }
            // The rows are in the fused order: score descending, then the
            // stable path and chunk identity keys.
            let mut resorted = traced.clone();
            resorted.sort_by(|a, b| {
                b.fusion_score
                    .total_cmp(&a.fusion_score)
                    .then_with(|| a.relative_file_path.cmp(&b.relative_file_path))
                    .then(a.start_line.cmp(&b.start_line))
                    .then(a.end_line.cmp(&b.end_line))
                    .then(a.content_hash.cmp(&b.content_hash))
            });
            assert_eq!(traced, resorted);
            assert_eq!(
                traced.iter().map(|row| row.fused_rank).collect::<Vec<_>>(),
                (1..=rows.len()).collect::<Vec<_>>()
            );
        }
    }

    #[test]
    fn a_trace_survives_a_json_round_trip_and_the_outcome_is_snake_case() {
        let hits = fused_order(&INTERLEAVED);
        let mut rows = trace_candidates(&hits, 2, 4, 1024);
        record_rerank_scores(&mut rows, &[(0, 0.75), (1, 0.25), (2, 0.5), (3, 0.125)]);
        assert_eq!(rows[0].content_hash, hits[0].chunk.content_hash);
        let json = serde_json::to_string(&rows).unwrap();
        assert_eq!(
            serde_json::from_str::<Vec<CandidateTrace>>(&json).unwrap(),
            rows
        );
        let mut legacy_json: serde_json::Value = serde_json::from_str(&json).unwrap();
        legacy_json[0]
            .as_object_mut()
            .unwrap()
            .remove("content_hash");
        let legacy: Vec<CandidateTrace> = serde_json::from_value(legacy_json).unwrap();
        assert_eq!(legacy[0].content_hash, "");
        assert_eq!(
            serde_json::to_string(&[
                RerankOutcome::Scored,
                RerankOutcome::Failed,
                RerankOutcome::Off,
                RerankOutcome::NotRun
            ])
            .unwrap(),
            r#"["scored","failed","off","not_run"]"#
        );
    }

    fn report_without_freshness() -> super::SearchReport {
        super::SearchReport {
            workspace: crate::workspace::Workspace {
                id: "ws_0123456789abcdef".to_owned(),
                path: "/workspace".into(),
            },
            query: "q".to_owned(),
            search_profile: None,
            candidate_count: 0,
            reranked: false,
            warning: None,
            notes: Vec::new(),
            lsp_over_budget: Vec::new(),
            timings: super::Timings::default(),
            results: Vec::new(),
            docs: None,
            index: super::SearchIndexLifecycle {
                action: crate::app::IndexAction::Reused,
                wait_ms: 0.0,
            },
            filters: crate::filter::EffectiveFilter::from_request(
                &crate::filter::FilterRequest::default(),
            )
            .unwrap()
            .report(),
            candidates: None,
            rerank_outcome: None,
            freshness: None,
            channel_outcomes: super::ChannelOutcomes::default(),
        }
    }

    #[test]
    fn a_channel_that_failed_or_did_not_run_has_no_snapshot_lag() {
        use super::ChannelOutcome::{Answered, Failed, NotRun};
        let listed = super::channel_freshness(
            super::ChannelOutcomes {
                semantic: Failed,
                lexical: Answered,
                lsp: NotRun,
            },
            Some(2),
        );
        let ids: Vec<_> = listed.iter().map(|entry| entry.id).collect();
        assert_eq!(ids, ["vec.chunk.code", "lexical"], "{listed:?}");
        // Failed: attempted, answered nothing, so no lag and a note.
        assert_eq!(listed[0].lag, None);
        assert_eq!(listed[0].stale, None);
        assert!(listed[0].note.is_some_and(|note| note.contains("failed")));
        // Answered: the snapshot's lag.
        assert_eq!((listed[1].stale, listed[1].lag), (Some(true), Some(2)));
        assert_eq!(listed[1].note, None);
        // The session channel: failed says so; answered carries the old note.
        let session = |outcome| {
            let listed = super::channel_freshness(
                super::ChannelOutcomes {
                    lsp: outcome,
                    ..super::ChannelOutcomes::default()
                },
                Some(0),
            );
            listed.into_iter().next()
        };
        assert!(session(NotRun).is_none());
        let failed = session(Failed).unwrap();
        assert_eq!((failed.id, failed.observed), ("lsp.symbols", "session"));
        assert!(failed.note.is_some_and(|note| note.contains("failed")));
        assert!(
            session(Answered)
                .unwrap()
                .note
                .is_some_and(|note| note.contains("not compared"))
        );
    }

    #[test]
    fn the_report_has_no_freshness_block_with_the_key_off() {
        let report = report_without_freshness();
        let json = serde_json::to_value(&report).unwrap();
        assert!(json.get("freshness").is_none(), "{json}");
    }

    #[test]
    fn the_freshness_block_names_the_snapshot_the_channels_and_the_hits() {
        let mut report = report_without_freshness();
        report.freshness = Some(super::SearchFreshness {
            snapshot: super::SnapshotFreshness {
                stale: true,
                lag: Some(3),
            },
            channels: vec![
                super::ChannelFreshness {
                    id: "lexical",
                    observed: "snapshot",
                    stale: Some(true),
                    lag: Some(3),
                    note: None,
                },
                super::ChannelFreshness {
                    id: "lsp.symbols",
                    observed: "session",
                    stale: None,
                    lag: None,
                    note: Some("not compared"),
                },
            ],
            hits: vec![super::HitFreshness {
                path: "a.rs".to_owned(),
                file_changed: None,
            }],
        });
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(
            json["freshness"],
            serde_json::json!({
                "snapshot": {"stale": true, "lag": 3},
                "channels": [
                    {"id": "lexical", "observed": "snapshot", "stale": true, "lag": 3},
                    {"id": "lsp.symbols", "observed": "session", "stale": null, "lag": null,
                     "note": "not compared"},
                ],
                "hits": [{"path": "a.rs", "file_changed": null}],
            })
        );
    }

    #[test]
    fn a_hit_changed_when_its_file_differs_from_the_snapshots_copy() {
        let pairs = |entries: &[(&str, &str)]| {
            entries
                .iter()
                .map(|(path, hash)| ((*path).to_owned(), (*hash).to_owned()))
                .collect()
        };
        let compared = super::Compared {
            snapshot: pairs(&[
                ("a.rs", "1"),
                ("b.rs", "1"),
                ("gone.rs", "1"),
                ("README.md", "1"),
            ]),
            tree: pairs(&[("a.rs", "2"), ("b.rs", "1"), ("new.rs", "1")]),
        };
        assert_eq!(compared.file_changed("a.rs"), Some(true));
        assert_eq!(compared.file_changed("b.rs"), Some(false));
        assert_eq!(compared.file_changed("new.rs"), Some(true));
        assert_eq!(compared.file_changed("gone.rs"), Some(true));
        // A prose file the tree scan leaves out is not compared.
        assert_eq!(compared.file_changed("README.md"), None);
        assert_eq!(compared.file_changed("unknown.rs"), None);
    }
}
