//! Hybrid retrieval: index freshness, semantic/lexical/LSP fan-out,
//! Reciprocal Rank Fusion, and reranking. Each channel and the reranker can
//! be switched off for a search with the `[channels]` configuration, and a
//! named `[search_profiles.<name>]` can override those switches and the
//! rerank window per search (see [`App::search_in_profile`]).

use super::{App, IndexAction, WorkspaceCoordination, ms};
use crate::{
    config::{ChannelsConfig, FusionConfig, IndexFreshness, SearchSettings},
    filter::{EffectiveFilter, EffectiveFilterReport, FilterRequest, SourceRole},
    lexical,
    lsp::adapter::LspAdapter,
    store::{Hit, Snapshot, Store, bounded_snippet},
    workspace::Workspace,
};
use anyhow::{Context, Result, bail, ensure};
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

#[derive(Debug, Default, Serialize)]
pub struct Timings {
    pub query_embedding_ms: f64,
    pub lancedb_search_ms: f64,
    pub lexical_search_ms: f64,
    pub lsp_search_ms: f64,
    pub fusion_ms: f64,
    pub reranking_ms: f64,
    pub total_ms: f64,
}

#[derive(Debug, Serialize)]
pub struct SearchIndexLifecycle {
    pub action: IndexAction,
    pub wait_ms: f64,
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
    pub timings: Timings,
    pub results: Vec<Hit>,
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
    /// Bytes of the document the reranker is given for this candidate (the
    /// path, a newline, then the head of the code cut to
    /// `rerank_document_max_bytes`); `Some` for every shortlisted candidate
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
/// `rerank_document_max_bytes` the search then applies. Reranker fields are
/// left empty (see [`record_rerank_scores`]). Observes only: the cap runs on
/// positions here, with the same function the search applies to the hits.
fn trace_candidates(
    hits: &[Hit],
    max_per_file: u32,
    shortlist: usize,
    max_bytes: usize,
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
            let document_bytes = shortlisted.then(|| rerank_document(hit, max_bytes).len());
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

/// What the language-server channel of one search produced.
#[derive(Default)]
struct LspChannel {
    locations: Vec<crate::lsp::Location>,
    /// One `provider: cause` entry for each provider that failed.
    failures: Vec<String>,
    /// Providers that did not answer within `lsp_search_budget_ms`.
    over_budget: Vec<&'static str>,
    elapsed_ms: f64,
}

/// Outcome of resolving which index snapshot a search should read: the
/// (re-checked) snapshot, the lifecycle action to report, and the read
/// guard that must stay held for the rest of the search so the snapshot
/// cannot be replaced out from under it.
struct IndexResolution<'a> {
    _read_guard: tokio::sync::RwLockReadGuard<'a, ()>,
    snapshot: Option<Snapshot>,
    action: IndexAction,
    wait_ms: f64,
    warning: Option<String>,
}

fn lookup_lexical_files(file_ids: &HashMap<String, Vec<usize>>, reported: &str) -> Vec<usize> {
    let reported = reported.replace('\\', "/").to_lowercase();
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
        self.search_configured(path, query, top_k, filter_request, options, &settings)
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
        self.search_configured(
            path,
            query,
            top_k,
            filter_request,
            SearchOptions::default(),
            &settings,
        )
        .await
    }

    /// The search behind [`Self::search_with_options`] and
    /// [`Self::search_in_profile`], run with the resolved `settings` (the
    /// query-time switches and the rerank window). Appends the candidate dump
    /// line when this `App` has a dump, as [`Self::search_with_options`]
    /// describes.
    async fn search_configured(
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
        let mut report = SearchReport {
            workspace,
            query: query.into(),
            search_profile: settings.profile.clone(),
            candidate_count: 0,
            reranked: false,
            warning: resolution.warning,
            notes: Vec::new(),
            timings: Timings::default(),
            results: vec![],
            index: SearchIndexLifecycle {
                action: resolution.action,
                wait_ms: resolution.wait_ms,
            },
            filters: filters.report(),
            // A traced search that ends before fusion has an empty pool and
            // never reached the reranker.
            candidates: options.trace_candidates.then(Vec::new),
            rerank_outcome: options.trace_candidates.then_some(RerankOutcome::NotRun),
        };
        if snapshot.table.count_rows(None).await? == 0 {
            report.timings.total_ms = ms(total);
            return Ok(report);
        }
        let mut all_chunks = Store::chunks(&snapshot, &report.workspace).await?;
        all_chunks.retain(|hit| hit_matches(&filters, hit));
        // `[channels]` switches, as a search profile may override them. A
        // deliberately disabled channel is skipped outright: no work, a zero
        // timing, and no "unavailable" warning.
        let channels = settings.channels;
        let mut applicable_adapters: HashMap<&'static str, Arc<dyn LspAdapter>> = HashMap::new();
        if channels.lsp {
            let indexed_languages: HashSet<&str> = all_chunks
                .iter()
                .map(|hit| hit.chunk.language.as_str())
                .collect();
            for language in indexed_languages {
                for adapter in self.analyzer.adapters_for_language(language) {
                    if adapter.enabled() {
                        applicable_adapters
                            .entry(adapter.provider())
                            .or_insert_with(|| Arc::clone(adapter));
                    }
                }
            }
        }
        let lsp_budget = Duration::from_millis(self.config.lsp_search_budget_ms);
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
                let mut symbol_queries = Vec::new();
                for term in lexical::terms(query) {
                    if term.ends_with('s') && term.len() > 4 {
                        symbol_queries.push(format!("{}#", &term[..term.len() - 1]));
                    }
                    symbol_queries.push(format!("{term}#"));
                }
                symbol_queries.dedup();
                symbol_queries.truncate(8);
                // The providers run side by side, each under the same wall
                // clock budget, so one that is still starting cannot starve
                // the others. Past the budget the provider's future is
                // dropped, which only stops this search waiting: its session
                // lives in the manager and keeps starting and warming for the
                // next search. Answers gathered before the budget ran out are
                // kept.
                let mut providers: Vec<&Arc<dyn LspAdapter>> =
                    applicable_adapters.values().collect();
                providers.sort_by_key(|adapter| adapter.provider());
                let (workspace, queries) = (&report.workspace, &symbol_queries);
                let answers =
                    futures::future::join_all(providers.into_iter().map(|adapter| async move {
                        let gathered = std::sync::Mutex::new(Vec::new());
                        let answer = tokio::time::timeout(
                            lsp_budget,
                            self.provider_symbols(adapter, workspace, queries, &gathered),
                        )
                        .await;
                        let gathered = gathered.into_inner().unwrap_or_else(|p| p.into_inner());
                        (adapter.provider(), gathered, answer)
                    }))
                    .await;
                let mut channel = LspChannel::default();
                let mut per_provider = Vec::new();
                for (provider, gathered, answer) in answers {
                    per_provider.push(gathered);
                    match answer {
                        Ok(failure) => channel.failures.extend(failure),
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
                // Merge query by query across providers, as one sequential
                // pass would: first-seen order decides the LSP rank.
                for index in 0..symbol_queries.len() {
                    for locations in &mut per_provider {
                        if let Some(found) = locations.get_mut(index) {
                            channel.locations.append(found);
                        }
                    }
                }
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
        if !lsp_result.over_budget.is_empty() {
            // Not a warning: nothing failed, the server is just not ready
            // yet. A warning would also count the search as a fallback.
            report.notes.push(format!(
                "LSP still starting or slow; symbol results from {} are missing or incomplete \
                 for this search (lsp_search_budget_ms = {}). The server keeps starting in the \
                 background.",
                lsp_result.over_budget.join(", "),
                self.config.lsp_search_budget_ms
            ));
        }
        if !lsp_failures.is_empty() {
            let message = format!(
                "Optional LSP provider unavailable: {}",
                lsp_failures.join("; ")
            );
            report.warning = Some(
                report
                    .warning
                    .take()
                    .map_or(message.clone(), |w| format!("{w}; {message}")),
            );
        }
        let semantic = match query_result {
            // Disabled: the query was never embedded and the index is not
            // searched; both timings stay zero.
            None => vec![],
            Some((query_vector, query_ms)) => {
                report.timings.query_embedding_ms = query_ms;
                let semantic_timer = Instant::now();
                let hits = match query_vector {
                    Ok(vector) => match Store::search(
                        &snapshot,
                        &report.workspace,
                        &vector,
                        self.config.semantic_candidate_count,
                        &filters,
                    )
                    .await
                    {
                        Ok(hits) => hits,
                        Err(error) => {
                            let message = format!("Semantic retrieval unavailable: {error:#}");
                            report.warning = Some(
                                report
                                    .warning
                                    .take()
                                    .map_or(message.clone(), |w| format!("{w}; {message}")),
                            );
                            vec![]
                        }
                    },
                    Err(error) => {
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
        if let Some((lexical_matches, lexical_ms)) = lexical_result {
            report.timings.lexical_search_ms = lexical_ms;
            match lexical_matches {
                Ok(lexical_search) => {
                    lexical_or_lsp_ran = true;
                    lexical_hits = Self::map_lexical_matches(
                        &all_chunks,
                        lexical_search,
                        self.config.lexical_candidate_count,
                    );
                }
                Err(error) => {
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
        if semantic.is_empty() && lexical_hits.is_empty() && lsp_hits.is_empty() {
            // With the semantic channel off, an enabled lexical or LSP
            // channel that ran without failing and matched nothing is an
            // ordinary answer (a literal or symbol search with no hit). If
            // every enabled channel failed (or none could run) that stays an
            // error. With the semantic channel on, an empty result means it
            // failed, because it nearly always returns candidates.
            if filtering_requested || (!channels.semantic && lexical_or_lsp_ran) {
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
            let rows = trace_candidates(
                &hits,
                self.config.fusion.max_chunks_per_file,
                settings.rerank_candidate_count,
                self.config.rerank_document_max_bytes,
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
    /// snapshot. Returns a read guard that must be held for the remainder
    /// of the search so the snapshot cannot be replaced concurrently.
    async fn resolve_index_for_search<'a>(
        &self,
        workspace: &Workspace,
        coordination: &'a WorkspaceCoordination,
        initial_snapshot: Option<Snapshot>,
    ) -> Result<IndexResolution<'a>> {
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
                        // The holder committed nothing. If an automatic flight
                        // failed, share its cause instead of starting a
                        // duplicate within the same failed flight.
                        let shared = coordination.flight_error.lock().await.clone();
                        if let Some(cause) = shared {
                            return Err(anyhow::anyhow!(
                                "indexing failed while waiting for the in-progress index job: {cause}"
                            ));
                        }
                        // The holder was not an automatic flight (e.g. an
                        // explicit index that failed); we now hold the write
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
            let wait_ms = ms(flight);
            let _read_guard = coordination.lock.read().await;
            let snapshot = self.store.snapshot(workspace).await?;
            Ok(IndexResolution {
                _read_guard,
                snapshot,
                action,
                wait_ms,
                warning: None,
            })
        } else {
            let flight = Instant::now();
            let identity_changed = initial_snapshot
                .as_ref()
                .is_some_and(|s| s.identity != self.config.embedding_identity());
            let (action, warning) = match self.config.index.freshness {
                IndexFreshness::OnSearch => {
                    self.refresh_stale_snapshot(workspace, coordination, identity_changed)
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
            let wait_ms = ms(flight);
            let _read_guard = coordination.lock.read().await;
            let snapshot = self.store.snapshot(workspace).await?;
            Ok(IndexResolution {
                _read_guard,
                snapshot,
                action,
                wait_ms,
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
    fn map_lexical_matches(
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

    /// Reranks fused candidates and writes the outcome (reranked results, or
    /// a fail-open fallback to fusion-ranked results with a warning) into
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
        let documents: Vec<_> = hits
            .iter()
            .map(|hit| rerank_document(hit, max_bytes))
            .collect();
        match self.models.rerank(query, &documents).await {
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
                tracing::warn!(%error, "reranker failed; returning fusion-ranked results");
                // Append: earlier warnings (a refresh fallback, an
                // unavailable channel) must survive a reranker failure.
                let message = format!(
                    "Reranker unavailable or invalid response; fusion-ranked results returned: {error:#}"
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

#[cfg(test)]
mod tests {
    use super::{
        App, CandidateTrace, RerankOutcome, candidate_dump_path, cap_chunks_per_file, fusion_order,
        head_within, pool_cut_note, record_rerank_scores, rerank_document, trace_candidates,
    };
    use crate::{
        chunk::Chunk,
        config::FusionConfig,
        filter::SourceRole,
        store::{Hit, source_role_prior},
    };
    use std::{collections::HashSet, ffi::OsString, path::PathBuf};

    const RRF_K: f32 = 60.0;

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
        for (reported, expected) in [
            ("src/main.rs", vec![0, 1, 2]),
            ("SrC/MAIN.rs", vec![0, 1, 2]),
            ("prefix\\src\\MAIN.rs", vec![0, 1, 2]),
            ("prefix/src/main.rs", vec![0, 1, 2]),
            ("main.rs", vec![2]),
            ("other/main.rs", vec![2, 4]),
            ("prefix/src/É.rs", vec![5]),
            ("prefix/notmain.rs", vec![]),
            ("prefix/src/main.rs.bak", vec![]),
            ("missing.rs", vec![]),
        ] {
            let normalized = reported.replace('\\', "/").to_lowercase();
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
            },
            8,
        );
        assert_eq!(hits.len(), 8);
        let probes = LEXICAL_PATH_PROBES.with(|probes| probes.get());
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
    fn cap_zero_is_the_identity() {
        let hits = fused_order(&INTERLEAVED);
        let capped = cap_chunks_per_file(hits.clone(), 0);
        assert_eq!(order(&capped), INTERLEAVED);
        for (before, after) in hits.iter().zip(&capped) {
            assert_eq!(before.fusion_score.to_bits(), after.fusion_score.to_bits());
        }
        assert!(cap_chunks_per_file(Vec::new(), 0).is_empty());
        assert!(cap_chunks_per_file(Vec::new(), 2).is_empty());
    }

    #[test]
    fn cap_one_moves_every_repeat_of_a_file_behind_the_first_chunk_of_each_file() {
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

    #[test]
    fn cap_two_demotes_only_the_third_chunk_of_a_file() {
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

    #[test]
    fn the_cap_changes_only_the_order_never_a_score_or_a_chunk() {
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

    #[test]
    fn the_cap_is_stable_for_the_kept_chunks_and_for_the_demoted_ones() {
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

    #[test]
    fn a_single_file_keeps_its_order_whatever_the_cap() {
        let only_a = [("a.rs", 1), ("a.rs", 11), ("a.rs", 21), ("a.rs", 31)];
        for max in [0, 1, 2, 3, 4, 100] {
            let capped = cap_chunks_per_file(fused_order(&only_a), max);
            assert_eq!(order(&capped), only_a, "cap {max}");
        }
    }

    #[test]
    fn files_are_told_apart_by_relative_path_not_by_name() {
        // `src/lib.rs` and `tests/lib.rs` share a file name but are two files,
        // so a cap of 1 demotes nothing.
        let chunks = [("src/lib.rs", 1), ("tests/lib.rs", 1), ("a.rs", 1)];
        let capped = cap_chunks_per_file(fused_order(&chunks), 1);
        assert_eq!(order(&capped), chunks);
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
}
