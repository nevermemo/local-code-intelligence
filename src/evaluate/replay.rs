//! Offline replay of retrieval arms from a candidate dump.
//!
//! [`replay`] reads a report written by `evaluate --dump-candidates`, re-runs
//! the pure part of the retrieval pipeline on the dumped candidate pool with
//! other settings, and scores the result with the unchanged evaluation
//! metrics. It reads no files besides the report ([`replay_file`]), needs no
//! services, configuration, or index, and gives the same output for the same
//! inputs. Replay observes a finished run; it never changes ranking.
//!
//! Per dumped query, in this order:
//!
//! 1. **Reproduce fusion** from the dumped ranks, weights, and `rrf_k`
//!    exactly as the dump documents it, and check at the *dumped* arm that
//!    every row's `fusion_score` matches bit for bit, that the dump is in
//!    `fused_rank` order, and that the dumped per-file cap reproduces
//!    `capped_rank` and `shortlisted`. A query that fails is refused.
//! 2. **Re-sort** the pool under the replayed arm: score descending, then
//!    path, start line, end line, and content hash.
//! 3. **Cap** (demote a file's chunks beyond `cap`, stably) when
//!    `--cap-stage` is `shortlist`, the production behaviour.
//! 4. **Cut the window**: the first `window` rows.
//! 5. **Rerank** the window with the dumped reranker scores: `replace` orders
//!    it by score descending then window position, the order
//!    `Models::rerank` returns; `vote` re-sorts it by
//!    `fusion_score + vote_weight / (rrf_k + rerank_rank)`; `off` keeps the
//!    fused order. A window row without a reranker score refuses the query.
//! 6. **Tail**: the rest of the fused order follows the window.
//! 7. **Cap** the whole list when `--cap-stage` is `results`.
//! 8. **Take `k`** (the query's own result window, dumped in its arm).
//! 9. **Score** the evidence rows with `query_metrics` and `aggregate`.
//!
//! What is exact: any window up to the dumped shortlist, the cap at either
//! stage, any weights, `rrf_k`, or source-role prior under `--rerank-mode
//! off`, and the reranker's scores as dumped (the reranker scores each
//! (query, document) pair independently, so a candidate's score does not
//! depend on its neighbours). What is refused, per query and counted: a
//! reranked window holding a candidate the dump never sent to the reranker.
//! What cannot be replayed: a window or channel depth larger than the dump
//! took, another document budget, reranker, instruction, chunking, or
//! embedding.
//!
//! One deliberate difference from production: production returns only the
//! window (the shortlist, `rerank_candidate_count` rows), so with a window
//! smaller than `k` it returns fewer than `k` results, with or without the
//! reranker; replay always appends the rest of the fused order, the semantics
//! a window decoupled from `top_k` will have. They agree whenever the window
//! is at least `k` or covers the whole pool, which holds for every
//! checked-in configuration; [`ReplayBlock::past_window_queries`] counts the
//! replayed queries where they do not.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::app::{CandidateTrace, RerankOutcome};
use crate::config::FusionConfig;
use crate::filter::SourceRole;

use super::compare::load_report;
use super::metrics::aggregate;
use super::types::{
    AggregateReport, CandidateArm, EvalTimings, QueryReport, ReplayRefusal, RerankMode,
    ResultEvidence, StageLatency,
};

/// Fixed part of the fallback reranker cost model, in milliseconds.
pub const FALLBACK_INTERCEPT_MS: f64 = 400.0;
/// Reranker throughput of the fallback cost model, in estimated tokens per
/// second.
pub const FALLBACK_TOKENS_PER_SECOND: f64 = 290.0;
/// A fitted cost model with a smaller R-squared is not used.
pub const MIN_R_SQUARED: f64 = 0.5;

// ---------------------------------------------------------------------------
// Arm
// ---------------------------------------------------------------------------

/// Where the per-file cap runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapStage {
    /// After fusion and before the window cut: the production behaviour. A
    /// demoted chunk falls out of the reranker's window.
    Shortlist,
    /// After reranking, over the whole result list: the reranker sees the
    /// uncapped fused head and the cap only reorders what it returned.
    Results,
}

impl CapStage {
    /// The lowercase CLI name of the stage.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Shortlist => "shortlist",
            Self::Results => "results",
        }
    }
}

impl FromStr for CapStage {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "shortlist" => Ok(Self::Shortlist),
            "results" => Ok(Self::Results),
            other => Err(format!(
                "unknown cap stage {other:?}; expected shortlist or results"
            )),
        }
    }
}

impl FromStr for RerankMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, String> {
        match value {
            "replace" => Ok(Self::Replace),
            "vote" => Ok(Self::Vote),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "unknown rerank mode {other:?}; expected replace, vote, or off"
            )),
        }
    }
}

/// The settings a replay overrides; every `None` keeps the dumped arm's
/// value.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ReplayOptions {
    /// Rerank window: the number of leading rows (after a shortlist-stage
    /// cap) the reranker orders. Defaults to the dumped
    /// `rerank_candidate_count`.
    pub window: Option<usize>,
    /// Per-file cap (`max_chunks_per_file`; `0` is unlimited).
    pub cap: Option<u32>,
    /// Where the cap runs; defaults to [`CapStage::Shortlist`].
    pub cap_stage: Option<CapStage>,
    /// Weight of the semantic channel.
    pub semantic_weight: Option<f32>,
    /// Weight of the lexical channel.
    pub lexical_weight: Option<f32>,
    /// Weight of the language-server channel.
    pub lsp_weight: Option<f32>,
    /// Reciprocal Rank Fusion constant.
    pub rrf_k: Option<f32>,
    /// Source-role prior of every `source` row. Other roles keep the prior
    /// they were dumped with (`0` today).
    pub prior: Option<f32>,
    /// How the reranker's scores order the window.
    pub rerank_mode: Option<RerankMode>,
    /// Weight of the reranker's reciprocal-rank vote; required by, and only
    /// valid with, [`RerankMode::Vote`].
    pub vote_weight: Option<f32>,
}

/// The fully resolved arm a replay scores: the dumped arm with the overrides
/// applied.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ReplayArm {
    /// Reciprocal Rank Fusion constant.
    pub rrf_k: f32,
    /// Channel weights and the per-file cap.
    pub fusion: FusionConfig,
    /// Replacement for the prior of `source` rows; `None` keeps each row's
    /// dumped prior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_prior: Option<f32>,
    /// Where the cap runs.
    pub cap_stage: CapStage,
    /// Rerank window.
    pub window: usize,
    /// How the reranker's scores order the window.
    pub rerank_mode: RerankMode,
    /// Weight of the vote; present exactly for [`RerankMode::Vote`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vote_weight: Option<f32>,
}

impl ReplayOptions {
    /// The arm these options give on top of `dumped`.
    pub fn resolve(&self, dumped: &CandidateArm) -> Result<ReplayArm> {
        let arm = ReplayArm {
            rrf_k: self.rrf_k.unwrap_or(dumped.rrf_k),
            fusion: FusionConfig {
                semantic_weight: self
                    .semantic_weight
                    .unwrap_or(dumped.fusion.semantic_weight),
                lexical_weight: self.lexical_weight.unwrap_or(dumped.fusion.lexical_weight),
                lsp_weight: self.lsp_weight.unwrap_or(dumped.fusion.lsp_weight),
                max_chunks_per_file: self.cap.unwrap_or(dumped.fusion.max_chunks_per_file),
            },
            source_prior: self.prior,
            cap_stage: self.cap_stage.unwrap_or(CapStage::Shortlist),
            window: self.window.unwrap_or(dumped.rerank_candidate_count),
            rerank_mode: self.rerank_mode.unwrap_or(dumped.rerank_mode),
            vote_weight: self.vote_weight,
        };
        arm.validate()?;
        Ok(arm)
    }
}

impl ReplayArm {
    /// The same bounds the configuration enforces on the live settings.
    fn validate(&self) -> Result<()> {
        ensure!(
            self.rrf_k.is_finite() && self.rrf_k > 0.0,
            "rrf_k must be a finite number greater than zero (got {})",
            self.rrf_k
        );
        for (name, weight) in [
            ("semantic_weight", self.fusion.semantic_weight),
            ("lexical_weight", self.fusion.lexical_weight),
            ("lsp_weight", self.fusion.lsp_weight),
        ] {
            ensure!(
                weight.is_finite() && weight > 0.0,
                "{name} must be a finite number greater than zero (got {weight})"
            );
        }
        ensure!(
            self.fusion.max_chunks_per_file <= FusionConfig::MAX_CHUNKS_PER_FILE_LIMIT,
            "cap must be 0..={} (got {}); 0 means unlimited",
            FusionConfig::MAX_CHUNKS_PER_FILE_LIMIT,
            self.fusion.max_chunks_per_file
        );
        ensure!(self.window >= 1, "window must be at least 1");
        if let Some(prior) = self.source_prior {
            ensure!(
                prior.is_finite() && prior >= 0.0,
                "prior must be a finite number, zero or more (got {prior})"
            );
        }
        match (self.rerank_mode, self.vote_weight) {
            (RerankMode::Vote, Some(weight)) => ensure!(
                weight.is_finite() && weight >= 0.0,
                "vote weight must be a finite number, zero or more (got {weight})"
            ),
            (RerankMode::Vote, None) => {
                bail!("--rerank-mode vote needs --vote-weight, which no dump records")
            }
            (_, Some(_)) => bail!("--vote-weight only applies with --rerank-mode vote"),
            (_, None) => {}
        }
        Ok(())
    }

    /// One line describing the arm.
    fn describe(&self) -> String {
        let mut text = format!(
            "window {}, cap {} ({}), rerank {}, rrf_k {}, weights semantic {} / lexical {} / lsp {}",
            self.window,
            self.fusion.max_chunks_per_file,
            self.cap_stage.as_str(),
            self.rerank_mode.as_str(),
            self.rrf_k,
            self.fusion.semantic_weight,
            self.fusion.lexical_weight,
            self.fusion.lsp_weight,
        );
        if let Some(weight) = self.vote_weight {
            let _ = write!(text, ", vote weight {weight}");
        }
        if let Some(prior) = self.source_prior {
            let _ = write!(text, ", source prior {prior}");
        }
        text
    }
}

/// The arm of a dump, as a [`ReplayArm`] with no override: what replaying
/// with default options scores.
fn dumped_replay_arm(dumped: &CandidateArm) -> ReplayArm {
    ReplayArm {
        rrf_k: dumped.rrf_k,
        fusion: dumped.fusion,
        source_prior: None,
        cap_stage: CapStage::Shortlist,
        window: dumped.rerank_candidate_count,
        rerank_mode: dumped.rerank_mode,
        vote_weight: None,
    }
}

// ---------------------------------------------------------------------------
// Pure pipeline
// ---------------------------------------------------------------------------

/// A candidate with the score the replayed arm gives it.
#[derive(Clone, Copy)]
struct Scored<'a> {
    row: &'a CandidateTrace,
    score: f32,
}

/// The fusion score of `row` from its channel ranks: the arithmetic of the
/// dump's documentation, in `f32` and in that order, so that at the dumped
/// arm it equals `row.fusion_score` bit for bit.
fn fusion_score(row: &CandidateTrace, rrf_k: f32, weights: &FusionConfig, prior: f32) -> f32 {
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
    score + prior
}

/// The prior a row carries under `arm`.
fn row_prior(row: &CandidateTrace, arm: &ReplayArm) -> f32 {
    match arm.source_prior {
        Some(prior) if row.source_role == SourceRole::Source => prior,
        _ => row.prior,
    }
}

/// Search's fused order: score descending, then path, start, end, and content
/// hash.
fn fused_order(a: &Scored<'_>, b: &Scored<'_>) -> Ordering {
    b.score
        .total_cmp(&a.score)
        .then_with(|| a.row.relative_file_path.cmp(&b.row.relative_file_path))
        .then(a.row.start_line.cmp(&b.row.start_line))
        .then(a.row.end_line.cmp(&b.row.end_line))
        .then(a.row.content_hash.cmp(&b.row.content_hash))
}

/// Demotes, without removing, the items of a file beyond its `max`-th, stably:
/// the head is every item that is among the first `max` of its file, the tail
/// the rest, each in the original order. `max == 0` returns the list
/// unchanged. This mirrors the cap `App::search` applies (`cap_by_file` in
/// `src/app/search.rs`); a dump taken from the real cap fails verification
/// (`shortlist_mismatch`) if the two ever drift apart.
fn cap_by_file<T>(items: Vec<T>, max: u32, path_of: impl Fn(&T) -> &str) -> Vec<T> {
    if max == 0 {
        return items;
    }
    let max = max as usize;
    let keep: Vec<bool> = {
        let mut kept_per_file = std::collections::HashMap::<&str, usize>::new();
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

/// Why one query was not replayed.
struct Refusal {
    reason: ReplayRefusal,
    detail: String,
}

fn refuse(reason: ReplayRefusal, detail: impl Into<String>) -> Refusal {
    Refusal {
        reason,
        detail: detail.into(),
    }
}

fn location(row: &CandidateTrace) -> String {
    format!("{}:{}", row.relative_file_path, row.start_line)
}

fn outcome_name(outcome: Option<RerankOutcome>) -> &'static str {
    match outcome {
        Some(RerankOutcome::Scored) => "scored",
        Some(RerankOutcome::Failed) => "failed",
        Some(RerankOutcome::Off) => "off",
        Some(RerankOutcome::NotRun) => "not_run",
        None => "absent",
    }
}

/// Checks, at the dumped arm, that the dump is what the documented pipeline
/// produces: fusion scores bit for bit, the fused order and `fused_rank`, and
/// the per-file cap's `capped_rank` and `shortlisted`.
fn verify_dump(rows: &[CandidateTrace], dumped: &CandidateArm) -> Result<(), Refusal> {
    let scored: Vec<Scored<'_>> = rows
        .iter()
        .map(|row| Scored {
            row,
            score: fusion_score(row, dumped.rrf_k, &dumped.fusion, row.prior),
        })
        .collect();
    for (index, item) in scored.iter().enumerate() {
        if item.score.to_bits() != item.row.fusion_score.to_bits() {
            return Err(refuse(
                ReplayRefusal::FusionMismatch,
                format!(
                    "{}: dumped fusion_score {} but the dumped ranks give {}",
                    location(item.row),
                    item.row.fusion_score,
                    item.score
                ),
            ));
        }
        if item.row.fused_rank != index + 1 {
            return Err(refuse(
                ReplayRefusal::FusionMismatch,
                format!(
                    "{}: dumped fused_rank {} at position {}",
                    location(item.row),
                    item.row.fused_rank,
                    index + 1
                ),
            ));
        }
    }
    if let Some(pair) = scored
        .windows(2)
        .find(|pair| fused_order(&pair[0], &pair[1]) == Ordering::Greater)
    {
        return Err(refuse(
            ReplayRefusal::FusionMismatch,
            format!(
                "{} is listed before {} but fuses after it",
                location(pair[0].row),
                location(pair[1].row)
            ),
        ));
    }

    let positions: Vec<(usize, &str)> = rows
        .iter()
        .enumerate()
        .map(|(index, row)| (index, row.relative_file_path.as_str()))
        .collect();
    let capped = cap_by_file(
        positions,
        dumped.fusion.max_chunks_per_file,
        |&(_, path)| path,
    );
    for (position, (index, _)) in capped.into_iter().enumerate() {
        let row = &rows[index];
        let capped_rank = position + 1;
        let shortlisted = capped_rank <= dumped.rerank_candidate_count;
        if row.capped_rank != capped_rank || row.shortlisted != shortlisted {
            return Err(refuse(
                ReplayRefusal::ShortlistMismatch,
                format!(
                    "{}: dumped capped_rank {} shortlisted {}, recomputed {} and {}",
                    location(row),
                    row.capped_rank,
                    row.shortlisted,
                    capped_rank,
                    shortlisted
                ),
            ));
        }
    }
    Ok(())
}

/// One replayed query.
struct Replayed {
    evidence: Vec<ResultEvidence>,
    /// The reranker's scores ordered the window (`replace` or `vote` over a
    /// nonempty pool).
    reranked: bool,
    /// Sum of `est_tokens` over the reranked window, the input of the cost
    /// model.
    window_tokens: usize,
    /// Some returned result lies past the window.
    past_window: bool,
}

/// Replays one dumped query under `arm`. `dumped` is the query's own dumped
/// arm (it holds the query's result window `k`).
fn replay_query(
    report: &QueryReport,
    dumped: &CandidateArm,
    arm: &ReplayArm,
) -> Result<Replayed, Refusal> {
    let rows = &report.candidates;
    verify_dump(rows, dumped)?;
    if rows.is_empty() {
        return Ok(Replayed {
            evidence: Vec::new(),
            reranked: false,
            window_tokens: 0,
            past_window: false,
        });
    }
    let uses_reranker = matches!(arm.rerank_mode, RerankMode::Replace | RerankMode::Vote);
    if uses_reranker && report.rerank_outcome != Some(RerankOutcome::Scored) {
        return Err(refuse(
            ReplayRefusal::RerankNotScored,
            format!(
                "the dumped reranker outcome was {}, so there are no scores to replay",
                outcome_name(report.rerank_outcome)
            ),
        ));
    }

    // Re-sort under the replayed arm, then (production order) cap and cut.
    let mut pool: Vec<Scored<'_>> = rows
        .iter()
        .map(|row| Scored {
            row,
            score: fusion_score(row, arm.rrf_k, &arm.fusion, row_prior(row, arm)),
        })
        .collect();
    pool.sort_by(fused_order);
    let cap = arm.fusion.max_chunks_per_file;
    if arm.cap_stage == CapStage::Shortlist {
        pool = cap_by_file(pool, cap, |item| item.row.relative_file_path.as_str());
    }
    let tail = pool.split_off(arm.window.min(pool.len()));
    let mut window = pool;

    let mut window_tokens: usize = 0;
    if uses_reranker {
        let mut scores = Vec::with_capacity(window.len());
        for item in &window {
            let Some(score) = item.row.reranker_score else {
                return Err(refuse(
                    ReplayRefusal::UnscoredWindow,
                    format!(
                        "{} is in the replayed window of {} but the dumped search never sent it to the reranker",
                        location(item.row),
                        arm.window
                    ),
                ));
            };
            scores.push(score);
        }
        window_tokens = window
            .iter()
            .map(|item| item.row.est_tokens.unwrap_or(0))
            .sum();
        // `Models::rerank`'s order: score descending, then request index,
        // which here is the position in the replayed window.
        let mut by_score: Vec<usize> = (0..window.len()).collect();
        by_score.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
        match (arm.rerank_mode, arm.vote_weight) {
            (RerankMode::Vote, Some(weight)) => {
                let mut voted: Vec<Scored<'_>> = by_score
                    .iter()
                    .enumerate()
                    .map(|(place, &index)| Scored {
                        row: window[index].row,
                        score: window[index].score + weight / (arm.rrf_k + (place + 1) as f32),
                    })
                    .collect();
                voted.sort_by(fused_order);
                window = voted;
            }
            _ => window = by_score.iter().map(|&index| window[index]).collect(),
        }
    }

    // Window first (reranked or not), then the rest of the fused order. The
    // flag says whether the row came from the window.
    let mut ordered: Vec<(&CandidateTrace, bool)> = window
        .iter()
        .map(|item| (item.row, true))
        .chain(tail.iter().map(|item| (item.row, false)))
        .collect();
    if arm.cap_stage == CapStage::Results {
        ordered = cap_by_file(ordered, cap, |&(row, _)| row.relative_file_path.as_str());
    }
    ordered.truncate(dumped.k);
    let past_window = ordered.iter().any(|&(_, in_window)| !in_window);
    let evidence = ordered
        .into_iter()
        .enumerate()
        .map(|(index, (row, in_window))| ResultEvidence {
            rank: index + 1,
            relative_file_path: row.relative_file_path.clone(),
            language: row.language.clone(),
            source_role: row.source_role.as_str().to_owned(),
            start_line: row.start_line,
            end_line: row.end_line,
            snippet: row.snippet.clone(),
            reranker_score: if uses_reranker && in_window {
                row.reranker_score
            } else {
                None
            },
        })
        .collect();
    Ok(Replayed {
        evidence,
        reranked: uses_reranker,
        window_tokens,
        past_window,
    })
}

// ---------------------------------------------------------------------------
// Latency cost model
// ---------------------------------------------------------------------------

/// A least-squares line `milliseconds = intercept_ms + ms_per_token * tokens`.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct LinearFit {
    /// Fixed cost per rerank request.
    pub intercept_ms: f64,
    /// Marginal cost of one estimated token.
    pub ms_per_token: f64,
    /// Coefficient of determination of the fit.
    pub r_squared: f64,
    /// Number of dumped queries the fit used.
    pub samples: usize,
}

/// Which line a [`CostModel`] estimates with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CostBasis {
    /// The least-squares fit over the dump (R-squared at least
    /// [`MIN_R_SQUARED`] and a positive slope).
    Fitted,
    /// [`FALLBACK_INTERCEPT_MS`] plus tokens at
    /// [`FALLBACK_TOKENS_PER_SECOND`]; the dump gave no usable fit.
    Fallback,
}

/// The reranker latency model a replay uses for `reranking_ms`: the reranker
/// is the only stage whose cost a replayed arm changes, and its cost is
/// roughly a fixed part plus a part proportional to the document tokens it
/// is given.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CostModel {
    /// Whether the fit or the fallback line estimates.
    pub basis: CostBasis,
    /// Intercept of the line in use.
    pub intercept_ms: f64,
    /// Slope of the line in use.
    pub ms_per_token: f64,
    /// The least-squares fit over the dump, whether or not it was good
    /// enough to use; `None` when the dump has fewer than two usable points
    /// or all of them share one token count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fit: Option<LinearFit>,
}

impl CostModel {
    /// The model for `samples` of `(tokens, reranking_ms)`.
    pub fn from_samples(samples: &[(f64, f64)]) -> Self {
        let fit = fit_line(samples);
        match fit {
            Some(line) if line.r_squared >= MIN_R_SQUARED && line.ms_per_token > 0.0 => Self {
                basis: CostBasis::Fitted,
                intercept_ms: line.intercept_ms,
                ms_per_token: line.ms_per_token,
                fit,
            },
            _ => Self {
                basis: CostBasis::Fallback,
                intercept_ms: FALLBACK_INTERCEPT_MS,
                ms_per_token: 1000.0 / FALLBACK_TOKENS_PER_SECOND,
                fit,
            },
        }
    }

    /// Estimated `reranking_ms` for a window of `tokens` estimated tokens.
    pub fn estimate_ms(&self, tokens: usize) -> f64 {
        (self.intercept_ms + self.ms_per_token * tokens as f64).max(0.0)
    }

    /// The model for a dump: one point per query whose reranker answered, the
    /// tokens of the documents it scored against its measured `reranking_ms`.
    fn from_report(source: &AggregateReport) -> Self {
        let samples: Vec<(f64, f64)> = source
            .queries
            .iter()
            .filter(|query| !query.skipped && query.rerank_outcome == Some(RerankOutcome::Scored))
            .filter_map(|query| {
                let reranking_ms = query.timings?.reranking_ms;
                let tokens: usize = query
                    .candidates
                    .iter()
                    .filter(|row| row.rerank_rank.is_some())
                    .filter_map(|row| row.est_tokens)
                    .sum();
                reranking_ms
                    .is_finite()
                    .then_some((tokens as f64, reranking_ms))
            })
            .collect();
        Self::from_samples(&samples)
    }
}

/// Ordinary least squares of `y` on `x`. `None` for fewer than two points or
/// no spread in `x`. A series of equal `y` has no variance to explain and
/// gets an R-squared of `0`.
pub fn fit_line(samples: &[(f64, f64)]) -> Option<LinearFit> {
    if samples.len() < 2 {
        return None;
    }
    let n = samples.len() as f64;
    let mean_x = samples.iter().map(|&(x, _)| x).sum::<f64>() / n;
    let mean_y = samples.iter().map(|&(_, y)| y).sum::<f64>() / n;
    let (mut sxx, mut sxy, mut syy) = (0.0, 0.0, 0.0);
    for &(x, y) in samples {
        sxx += (x - mean_x) * (x - mean_x);
        sxy += (x - mean_x) * (y - mean_y);
        syy += (y - mean_y) * (y - mean_y);
    }
    if sxx <= 0.0 {
        return None;
    }
    let slope = sxy / sxx;
    Some(LinearFit {
        intercept_ms: mean_y - slope * mean_x,
        ms_per_token: slope,
        r_squared: if syy > 0.0 {
            sxy * sxy / (sxx * syy)
        } else {
            0.0
        },
        samples: samples.len(),
    })
}

// ---------------------------------------------------------------------------
// Report
// ---------------------------------------------------------------------------

/// How many of the source report's queries each outcome took.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayCounts {
    /// Queries in the source report.
    pub total: usize,
    /// Queries scored under the replayed arm.
    pub replayed: usize,
    /// Queries refused (see [`ReplayBlock::refused`]); reported as skipped.
    pub refused: usize,
    /// Queries the source report already skipped; carried over as skipped.
    pub source_skipped: usize,
    /// Queries whose search failed in the source report; carried over
    /// unchanged as failures.
    pub source_failed: usize,
}

/// One refused query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RefusedQuery {
    /// Workspace key of the query.
    pub workspace: String,
    /// Query text.
    pub query: String,
    /// Why it was refused.
    pub reason: ReplayRefusal,
    /// The first row or fact that caused the refusal.
    pub detail: String,
}

/// How the per-query latencies in a replayed report were made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayLatency {
    /// Always `true`: `reranking_ms` and `total_ms` are not measurements.
    pub estimated: bool,
    /// What is estimated and what is measured.
    pub note: String,
    /// The model that produced the estimate.
    pub cost_model: CostModel,
}

/// The source report's measured timings, which the estimates replace in the
/// replayed queries.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayOriginal {
    /// Median total latency of the source report.
    pub median_latency_ms: Option<f64>,
    /// 95th percentile total latency of the source report.
    pub p95_latency_ms: Option<f64>,
    /// Per-stage latency of the source report.
    pub stage_latency: Option<StageLatency>,
    /// The measured timings of every source query that has them.
    pub query_timings: Vec<OriginalTimings>,
}

/// The measured timings of one source query.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OriginalTimings {
    /// Position of the query in the report's `queries`.
    pub index: usize,
    /// Workspace key of the query.
    pub workspace: String,
    /// Query text.
    pub query: String,
    /// The timings the dumped search reported.
    pub timings: EvalTimings,
}

/// The `replay` block of a replayed report.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayBlock {
    /// The report that was replayed, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The arm the source report was dumped under. Its `k` is the first
    /// dumped query's; `k` is per query (the query's `top_k`).
    pub dumped_arm: CandidateArm,
    /// The arm that was replayed.
    pub arm: ReplayArm,
    /// What happened to each query.
    pub queries: ReplayCounts,
    /// Refused queries by reason; reasons that did not occur are absent.
    pub refused: BTreeMap<ReplayRefusal, usize>,
    /// Every refused query, in report order.
    pub refused_queries: Vec<RefusedQuery>,
    /// Replayed queries with a result past the window: the window is smaller
    /// than the `k` results asked for, so the rest of the fused order fills
    /// them (unreranked, when the reranker is on). Production would have
    /// returned fewer results, so this is zero when the replay reproduces a
    /// production search.
    pub past_window_queries: usize,
    /// How `reranking_ms` and `total_ms` were produced.
    pub latency: ReplayLatency,
    /// The measured timings of the source report.
    pub original: ReplayOriginal,
}

/// A replayed evaluation: an ordinary [`AggregateReport`] that `compare`
/// pairs with real reports, plus the [`ReplayBlock`] that says how it was
/// made.
#[derive(Debug, Clone)]
pub struct ReplayReport {
    /// The replayed queries and their aggregate.
    pub report: AggregateReport,
    /// How the report was made.
    pub replay: ReplayBlock,
}

impl ReplayReport {
    /// The report as JSON with the replay block under a `replay` key. The
    /// result still loads with [`load_report`], so `compare` accepts it.
    pub fn to_json(&self) -> Result<serde_json::Value> {
        let mut value = serde_json::to_value(&self.report)?;
        value["replay"] = serde_json::to_value(&self.replay)?;
        Ok(value)
    }

    /// A short human-readable summary: the arms, what was refused and why,
    /// the headline metrics, and how the latency was estimated.
    pub fn render_summary(&self) -> String {
        let (block, report) = (&self.replay, &self.report);
        let mut out = String::new();
        let _ = writeln!(
            out,
            "Replay of {}",
            block.source.as_deref().unwrap_or("the report")
        );
        let _ = writeln!(
            out,
            "  dumped arm:   {}",
            dumped_replay_arm(&block.dumped_arm).describe()
        );
        let _ = writeln!(out, "  replayed arm: {}", block.arm.describe());
        let counts = &block.queries;
        let _ = writeln!(
            out,
            "  queries: {} | replayed {} | refused {} | skipped in the source {} | failed in the source {}",
            counts.total,
            counts.replayed,
            counts.refused,
            counts.source_skipped,
            counts.source_failed
        );
        for (reason, count) in &block.refused {
            let _ = writeln!(out, "    refused {}: {count}", reason.as_str());
        }
        for refused in block.refused_queries.iter().take(5) {
            let _ = writeln!(
                out,
                "    - [{}] {}: {}",
                refused.reason.as_str(),
                refused.query,
                refused.detail
            );
        }
        if block.refused_queries.len() > 5 {
            let _ = writeln!(
                out,
                "    ... and {} more (see replay.refused_queries)",
                block.refused_queries.len() - 5
            );
        }
        if block.past_window_queries > 0 {
            let _ = writeln!(
                out,
                "  {} replayed queries return results past the window (the rest of the fused order fills them; production would return fewer)",
                block.past_window_queries
            );
        }
        let value = |metric: Option<f64>| metric.map_or("n/a".to_owned(), |v| format!("{v:.4}"));
        let _ = writeln!(
            out,
            "  over {} executed: MRR {} | nDCG@10 {} | hit@1 {} | hit@3 {}",
            report.executed_count,
            value(report.mrr),
            value(report.mean_ndcg_at_10),
            value(report.hit_at_1_rate),
            value(report.hit_at_3_rate),
        );
        let model = &block.latency.cost_model;
        let basis = match (model.basis, model.fit) {
            (CostBasis::Fitted, Some(fit)) => {
                format!(
                    "fitted on {} dumped queries, R^2 {:.3}",
                    fit.samples, fit.r_squared
                )
            }
            (_, Some(fit)) => format!(
                "fallback: the fit over {} dumped queries had R^2 {:.3}",
                fit.samples, fit.r_squared
            ),
            (_, None) => "fallback: the dump has no usable reranking timings".to_owned(),
        };
        let _ = writeln!(
            out,
            "  latency is ESTIMATED: reranking_ms = {:.1} + {:.3} * tokens ({basis}); other stages are the dump's measurements, the originals are under replay.original",
            model.intercept_ms, model.ms_per_token,
        );
        out
    }
}

/// The note stored with every replayed report's latency.
const LATENCY_NOTE: &str = "reranking_ms and total_ms are estimates from the cost model, not measurements: reranking_ms = intercept + ms_per_token * the estimated tokens of the replayed rerank window (0 without the reranker), and total_ms is the dumped total with its measured reranking_ms swapped for the estimate. The other stages are the dumped search's measurements. The measured timings are under replay.original.";

/// The dumped arm of `source`, which every dumped query must share (apart
/// from its own result window `k`).
fn dumped_arm(source: &AggregateReport) -> Result<CandidateArm> {
    let mut arms = source.queries.iter().filter_map(|query| query.arm.as_ref());
    let Some(first) = arms.next() else {
        bail!(
            "the report holds no candidate dump; take one with `evaluate --dump-candidates` and replay that report"
        );
    };
    for arm in arms {
        let mut same_k = arm.clone();
        same_k.k = first.k;
        ensure!(
            &same_k == first,
            "the report's queries were dumped under different arms; replay needs one dumped configuration"
        );
    }
    Ok(first.clone())
}

/// A query whose search failed (or whose workspace was required but absent):
/// no timings, no lifecycle, no evidence, and no dump to replay.
fn is_failed_search(query: &QueryReport) -> bool {
    query.outcome == "failure"
        && query.timings.is_none()
        && query.index_action.is_none()
        && query.evidence.is_empty()
}

/// A source query carried over without its dump fields.
fn without_dump(query: &QueryReport) -> QueryReport {
    let mut kept = query.clone();
    kept.candidates.clear();
    kept.arm = None;
    kept.rerank_outcome = None;
    kept.notes.clear();
    kept.replay_refusal = None;
    kept
}

/// What became of one source query.
enum Handled {
    /// Scored under the replayed arm.
    Replayed {
        report: Box<QueryReport>,
        past_window: bool,
    },
    /// Skipped in the source report; carried over.
    SourceSkipped(Box<QueryReport>),
    /// Its search failed in the source report; carried over.
    SourceFailed(Box<QueryReport>),
    /// Not scored, and why.
    Refused(Refusal),
}

fn handle(query: &QueryReport, arm: &ReplayArm, model: &CostModel) -> Handled {
    if query.skipped {
        return Handled::SourceSkipped(Box::new(without_dump(query)));
    }
    let Some(query_arm) = &query.arm else {
        return if is_failed_search(query) {
            Handled::SourceFailed(Box::new(without_dump(query)))
        } else {
            Handled::Refused(refuse(
                ReplayRefusal::NotDumped,
                "the query carries no candidate dump",
            ))
        };
    };
    let lane = query.query.lane();
    if lane != lci_core::evaluation::EvalLane::Code {
        return Handled::Refused(refuse(
            ReplayRefusal::NonCodeLane,
            format!(
                "the query is scored on the {lane:?} lane and a dump holds code candidates only"
            ),
        ));
    }
    match replay_query(query, query_arm, arm) {
        Ok(replayed) => {
            let timings = query.timings.map(|measured| {
                let reranking_ms = if replayed.reranked {
                    model.estimate_ms(replayed.window_tokens)
                } else {
                    0.0
                };
                EvalTimings {
                    reranking_ms,
                    total_ms: (measured.total_ms - measured.reranking_ms + reranking_ms).max(0.0),
                    ..measured
                }
            });
            Handled::Replayed {
                report: Box::new(QueryReport::new(
                    query.query.clone(),
                    replayed.evidence,
                    replayed.reranked,
                    query.warning.clone(),
                    query.filters.clone(),
                    timings,
                    query.index_action,
                    query.index_wait_ms,
                )),
                past_window: replayed.past_window,
            }
        }
        Err(refusal) => Handled::Refused(refusal),
    }
}

/// Replay the candidate dump in `source` under `options`.
///
/// Errors when the report holds no dump, when its dumped queries disagree on
/// the arm, or when an option is out of range. A query that cannot be scored
/// is not an error: it is refused, counted, and reported as skipped.
pub fn replay(source: &AggregateReport, options: &ReplayOptions) -> Result<ReplayReport> {
    let dumped = dumped_arm(source)?;
    let arm = options.resolve(&dumped)?;
    let model = CostModel::from_report(source);

    let mut counts = ReplayCounts::default();
    let mut refused: BTreeMap<ReplayRefusal, usize> = BTreeMap::new();
    let mut refused_queries = Vec::new();
    let mut past_window_queries = 0;
    let mut reports = Vec::with_capacity(source.queries.len());
    for query in &source.queries {
        counts.total += 1;
        match handle(query, &arm, &model) {
            Handled::Replayed {
                report,
                past_window,
            } => {
                counts.replayed += 1;
                past_window_queries += usize::from(past_window);
                reports.push(*report);
            }
            Handled::SourceSkipped(report) => {
                counts.source_skipped += 1;
                reports.push(*report);
            }
            Handled::SourceFailed(report) => {
                counts.source_failed += 1;
                reports.push(*report);
            }
            Handled::Refused(refusal) => {
                counts.refused += 1;
                *refused.entry(refusal.reason).or_default() += 1;
                let mut skipped = QueryReport::skipped(
                    query.query.clone(),
                    format!(
                        "replay refused ({}): {}",
                        refusal.reason.as_str(),
                        refusal.detail
                    ),
                );
                skipped.replay_refusal = Some(refusal.reason);
                reports.push(skipped);
                refused_queries.push(RefusedQuery {
                    workspace: query.query.workspace.clone(),
                    query: query.query.query.clone(),
                    reason: refusal.reason,
                    detail: refusal.detail,
                });
            }
        }
    }

    let original = ReplayOriginal {
        median_latency_ms: source.median_latency_ms,
        p95_latency_ms: source.p95_latency_ms,
        stage_latency: source.stage_latency,
        query_timings: source
            .queries
            .iter()
            .enumerate()
            .filter_map(|(index, query)| {
                query.timings.map(|timings| OriginalTimings {
                    index,
                    workspace: query.query.workspace.clone(),
                    query: query.query.query.clone(),
                    timings,
                })
            })
            .collect(),
    };
    Ok(ReplayReport {
        report: aggregate(&reports),
        replay: ReplayBlock {
            source: None,
            dumped_arm: dumped,
            arm,
            queries: counts,
            refused,
            refused_queries,
            past_window_queries,
            latency: ReplayLatency {
                estimated: true,
                note: LATENCY_NOTE.to_owned(),
                cost_model: model,
            },
            original,
        },
    })
}

/// [`replay`] of the report saved at `path`, recording the path as the
/// source.
pub fn replay_file(path: &Path, options: &ReplayOptions) -> Result<ReplayReport> {
    let source = load_report(path)?;
    let mut replayed = replay(&source, options)?;
    replayed.replay.source = Some(path.display().to_string());
    Ok(replayed)
}

#[cfg(test)]
mod fusion_properties;

#[cfg(test)]
mod tests {
    use super::{Scored, fused_order};
    use crate::{app::CandidateTrace, filter::SourceRole};

    fn candidate(
        end_line: u32,
        content_hash: &str,
        semantic_rank: Option<usize>,
        lexical_rank: Option<usize>,
    ) -> CandidateTrace {
        CandidateTrace {
            relative_file_path: "same.rs".into(),
            language: "rust".into(),
            start_line: 1,
            end_line,
            content_hash: content_hash.into(),
            source_role: SourceRole::Source,
            semantic_rank,
            semantic_score: semantic_rank.map(|_| 0.5),
            lexical_rank,
            lexical_score: lexical_rank.map(|_| 1.5),
            lexical_match_count: u32::from(lexical_rank.is_some()),
            lsp_rank: None,
            prior: 0.004,
            fusion_score: 1.0,
            fused_rank: 0,
            capped_rank: 0,
            shortlisted: false,
            rerank_rank: None,
            reranker_score: None,
            document_bytes: None,
            est_tokens: None,
            snippet: String::new(),
        }
    }

    #[test]
    fn equal_score_candidates_with_the_same_path_and_start_line_have_a_stable_order() {
        let semantic = candidate(3, "z-semantic", Some(1), None);
        let lexical = candidate(2, "a-lexical", None, Some(1));
        let ordered = |rows: [&CandidateTrace; 2]| {
            let mut rows = rows.map(|row| Scored { row, score: 1.0 });
            rows.sort_by(fused_order);
            rows.map(|item| (item.row.end_line, item.row.content_hash.clone()))
        };

        let forward = ordered([&semantic, &lexical]);
        let reverse = ordered([&lexical, &semantic]);
        assert_eq!(forward, reverse);
        assert_eq!(
            forward,
            [(2, "a-lexical".to_owned()), (3, "z-semantic".to_owned())]
        );

        let mut same_end_semantic = semantic.clone();
        let mut same_end_lexical = lexical.clone();
        same_end_semantic.end_line = 3;
        same_end_lexical.end_line = 3;
        let forward = ordered([&same_end_semantic, &same_end_lexical]);
        let reverse = ordered([&same_end_lexical, &same_end_semantic]);
        assert_eq!(forward, reverse);
        assert_eq!(
            forward,
            [(3, "a-lexical".to_owned()), (3, "z-semantic".to_owned())]
        );
    }
}
