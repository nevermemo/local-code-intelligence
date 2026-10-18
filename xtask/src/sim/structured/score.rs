//! Scoring one structured-config question against its family (preregistration
//! section 6, harness plan section 3). The simulator never sees the gold; this
//! reads a finished [`Trace`].
//!
//! A read window (`Step.windows`, 1-based inclusive) covers a gold span when
//! the paths are equal (`/`-normalised) and the line ranges overlap. Lines a
//! grep merely listed are not evidence. Per question type:
//!
//! - `setting_location`: complete when every assignment span is covered;
//! - `behaviour_to_setting`: complete when any assignment or documentation span
//!   is covered, with `docs_only` / `assignment_only` saying which evidence
//!   completed it (the windows read up to and including the completing call);
//! - `setting_to_reader`: complete when every reader span is covered.
//!
//! Calls and tokens to complete are those of the first call after which the
//! windows read so far suffice; on a miss they are `None`, `miss_stop` names
//! why the run ended, and the charged figures are the whole run's.

use super::gold::{
    BEHAVIOUR_TO_SETTING, Family, Question, SETTING_LOCATION, SETTING_TO_READER, Span,
};
use crate::sim::sim_loop::{Step, Trace, Window};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// A table cell over fewer items than this is "too small to read"
/// (preregistration section 6).
pub(crate) const TOO_SMALL: usize = 10;
/// Files of the first search listing that count for `hit_at_5`.
const HIT_RANK: u32 = 5;

/// Everything scored about one question's run.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub(crate) struct QuestionScore {
    /// All the required evidence was read within the budget.
    pub complete: bool,
    /// `behaviour_to_setting` only: completed by a documentation span alone.
    pub docs_only: bool,
    /// `behaviour_to_setting` only: completed by an assignment span alone.
    pub assignment_only: bool,
    /// 1-based rank, among the distinct files of the first search step's
    /// listing, of the first file with a listed line inside an assignment span.
    pub listed_rank: Option<u32>,
    /// `listed_rank` is at most 5. Headline for q1 and q2; for q3 it is not
    /// the asked fact and is only reported.
    pub hit_at_5: bool,
    /// `setting_to_reader` only. Precision over the windows read in the
    /// scope file; 0 with `no_prediction` when none was read there.
    pub reader_precision: Option<f64>,
    /// Reader spans covered by any window / reader spans.
    pub reader_recall: Option<f64>,
    pub reader_f1: Option<f64>,
    pub no_prediction: bool,
    /// Secondary: the same precision and F1 over all windows read.
    pub reader_precision_all: Option<f64>,
    pub reader_f1_all: Option<f64>,
    pub calls_to_complete: Option<u32>,
    pub tokens_to_complete: Option<u64>,
    /// `Trace.stop` of a run that did not complete.
    pub miss_stop: Option<String>,
    /// Calls to complete; the whole run's calls on a miss.
    pub charged_calls: u32,
    /// Tokens to complete; the whole run's `tokens_read` on a miss.
    pub charged_tokens: u64,
    /// Follow-greps (pops) up to the completing call; all of them on a miss.
    pub charged_pops: u32,
    /// Whole run: calls, tokens of tool output, follow-greps (pops) and reads
    /// (expansions).
    pub tool_calls: u32,
    pub tokens_read: u64,
    pub follow_greps: u32,
    pub reads: u32,
}

fn norm(path: &str) -> String {
    path.replace('\\', "/")
}

/// Whether the read window overlaps the gold span (windows are `/`-normalised
/// before they get here; gold paths are `/` paths by construction).
fn covers(window: &Window, span: &Span) -> bool {
    window.path == span.path && window.start <= span.end_line && span.line <= window.end
}

fn all_covered(spans: &[Span], windows: &[Window]) -> bool {
    !spans.is_empty() && spans.iter().all(|s| windows.iter().any(|w| covers(w, s)))
}

fn any_covered(spans: &[Span], windows: &[Window]) -> bool {
    spans.iter().any(|s| windows.iter().any(|w| covers(w, s)))
}

/// Whether `windows` hold all the evidence the question asks for.
fn is_complete(qtype: &str, family: &Family, windows: &[Window]) -> bool {
    match qtype {
        SETTING_LOCATION => all_covered(&family.assignments, windows),
        BEHAVIOUR_TO_SETTING => {
            any_covered(&family.assignments, windows) || any_covered(&family.docs, windows)
        }
        SETTING_TO_READER => all_covered(&family.readers, windows),
        _ => false,
    }
}

/// The file of a reader scope: `component:src/a.rs::f (note)` is `src/a.rs`.
/// `None` for a scope that names no component.
pub(crate) fn scope_file(scope: &str) -> Option<String> {
    let rest = scope.strip_prefix("component:")?;
    let file = rest.split("::").next()?.split_whitespace().next()?;
    Some(norm(file))
}

/// The windows without repeats, in first-read order.
fn distinct(windows: &[Window]) -> Vec<&Window> {
    let mut out: Vec<&Window> = Vec::new();
    for w in windows {
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

fn f1(precision: f64, recall: f64) -> f64 {
    if precision + recall == 0.0 {
        0.0
    } else {
        2.0 * precision * recall / (precision + recall)
    }
}

/// Share of `windows` that cover some reader span; 0 for no windows.
fn precision_of(windows: &[&Window], readers: &[Span]) -> f64 {
    if windows.is_empty() {
        return 0.0;
    }
    let hits = windows
        .iter()
        .filter(|w| readers.iter().any(|s| covers(w, s)))
        .count();
    hits as f64 / windows.len() as f64
}

/// Rank (1-based) of the first file of the first search step's listing that
/// has a listed line inside an assignment span. `None` without a search step
/// or without such a file.
fn listed_rank(family: &Family, trace: &Trace) -> Option<u32> {
    let step = trace.steps.iter().find(|s| s.kind == "search")?;
    let mut files: Vec<String> = Vec::new();
    for (path, _) in &step.listed {
        let path = norm(path);
        if !files.contains(&path) {
            files.push(path);
        }
    }
    files
        .iter()
        .position(|file| {
            step.listed.iter().any(|(path, line)| {
                norm(path) == *file
                    && family
                        .assignments
                        .iter()
                        .any(|s| s.path == *file && s.line <= *line && *line <= s.end_line)
            })
        })
        .map(|i| i as u32 + 1)
}

pub(crate) fn score_question(q: &Question, f: &Family, t: &Trace) -> QuestionScore {
    // The first call after which the windows read so far are enough, and how
    // many windows that was.
    let mut windows: Vec<Window> = Vec::new();
    let mut done: Option<(usize, usize)> = None;
    for (i, step) in t.steps.iter().enumerate() {
        if step.windows.is_empty() {
            continue;
        }
        windows.extend(step.windows.iter().map(|w| Window {
            path: norm(&w.path),
            start: w.start,
            end: w.end,
        }));
        if done.is_none() && is_complete(&q.qtype, f, &windows) {
            done = Some((i, windows.len()));
        }
    }
    let complete = done.is_some();
    let (calls_to_complete, tokens_to_complete) = match done {
        Some((i, _)) => (Some(t.steps[i].call), Some(t.steps[i].cumulative_tokens)),
        None => (None, None),
    };
    let tool_calls = t.steps.len() as u32;
    let tokens_read = t.tokens_read();
    let upto = done.map_or(t.steps.len(), |(i, _)| i + 1);
    let pops = |steps: &[Step]| steps.iter().filter(|s| s.kind == "follow-grep").count() as u32;

    let mut score = QuestionScore {
        complete,
        listed_rank: listed_rank(f, t),
        calls_to_complete,
        tokens_to_complete,
        miss_stop: (!complete).then(|| t.stop.name().to_owned()),
        charged_calls: calls_to_complete.unwrap_or(tool_calls),
        charged_tokens: tokens_to_complete.unwrap_or(tokens_read),
        charged_pops: pops(&t.steps[..upto]),
        tool_calls,
        tokens_read,
        follow_greps: pops(&t.steps),
        reads: t
            .steps
            .iter()
            .filter(|s| matches!(s.kind, "read" | "follow-read" | "site-read"))
            .count() as u32,
        ..QuestionScore::default()
    };
    score.hit_at_5 = score.listed_rank.is_some_and(|r| r <= HIT_RANK);

    if q.qtype == BEHAVIOUR_TO_SETTING
        && let Some((_, len)) = done
    {
        // What completed it: the evidence read up to the completing call.
        let evidence = &windows[..len];
        let assignment = any_covered(&f.assignments, evidence);
        let docs = any_covered(&f.docs, evidence);
        score.docs_only = docs && !assignment;
        score.assignment_only = assignment && !docs;
    }

    if q.qtype == SETTING_TO_READER {
        let all = distinct(&windows);
        let scope = scope_file(&f.readers_scope);
        let predicted: Vec<&Window> = all
            .iter()
            .copied()
            .filter(|w| scope.as_deref() == Some(w.path.as_str()))
            .collect();
        let covered = f
            .readers
            .iter()
            .filter(|s| windows.iter().any(|w| covers(w, s)))
            .count();
        let recall = if f.readers.is_empty() {
            0.0
        } else {
            covered as f64 / f.readers.len() as f64
        };
        let precision = precision_of(&predicted, &f.readers);
        let precision_all = precision_of(&all, &f.readers);
        score.no_prediction = predicted.is_empty();
        score.reader_precision = Some(precision);
        score.reader_recall = Some(recall);
        score.reader_f1 = Some(f1(precision, recall));
        score.reader_precision_all = Some(precision_all);
        score.reader_f1_all = Some(f1(precision_all, recall));
    }
    score
}

/// One scored question with the labels the tables group by.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct ScoreRow {
    pub question_id: String,
    pub family: String,
    pub repo: String,
    pub stratum: String,
    pub split: String,
    pub qtype: String,
    pub score: QuestionScore,
}

fn mean(values: Vec<f64>) -> Value {
    if values.is_empty() {
        Value::Null
    } else {
        json!(values.iter().sum::<f64>() / values.len() as f64)
    }
}

/// One cell of the aggregate: counts, rates and means over `rows`.
fn cell(stratum: &str, repo: &str, qtype: &str, split: &str, rows: &[&ScoreRow]) -> Value {
    let n = rows.len();
    let scores: Vec<&QuestionScore> = rows.iter().map(|r| &r.score).collect();
    let complete = scores.iter().filter(|s| s.complete).count();
    // hit@5 is not the asked fact for q3: a pooled cell (`*` type) leaves
    // those questions out of its hit count and its rate.
    let hit_pool: Vec<&&ScoreRow> = rows
        .iter()
        .filter(|r| qtype != "*" || r.qtype != SETTING_TO_READER)
        .collect();
    let hits = hit_pool.iter().filter(|r| r.score.hit_at_5).count();
    let rate = |count: usize, of: usize| -> Value {
        if of == 0 {
            Value::Null
        } else {
            json!(count as f64 / of as f64)
        }
    };
    let mut misses: BTreeMap<String, u64> = BTreeMap::new();
    for s in &scores {
        if let Some(stop) = &s.miss_stop {
            *misses.entry(stop.clone()).or_default() += 1;
        }
    }
    let done = || scores.iter().filter(|s| s.complete);
    let success_rate = rate(complete, n);
    let hit_rate = rate(hits, hit_pool.len());
    // Two objects, merged: one `json!` of this size nears the macro's
    // recursion limit.
    let mut out = json!({
        "stratum": stratum, "repo": repo, "qtype": qtype, "split": split,
        "n": n,
        "too_small_to_read": n < TOO_SMALL,
        "complete": complete,
        "success_rate": success_rate,
        "docs_only": scores.iter().filter(|s| s.docs_only).count(),
        "assignment_only": scores.iter().filter(|s| s.assignment_only).count(),
        "hit_at_5": hits,
        "hit_at_5_n": hit_pool.len(),
        "hit_at_5_rate": hit_rate,
        "no_prediction": scores.iter().filter(|s| s.no_prediction).count(),
        "misses_by_stop": misses,
    });
    let means = json!({
        "mean_reader_precision": mean(scores.iter().filter_map(|s| s.reader_precision).collect()),
        "mean_reader_recall": mean(scores.iter().filter_map(|s| s.reader_recall).collect()),
        "mean_reader_f1": mean(scores.iter().filter_map(|s| s.reader_f1).collect()),
        "mean_reader_precision_all": mean(
            scores.iter().filter_map(|s| s.reader_precision_all).collect()
        ),
        "mean_reader_f1_all": mean(scores.iter().filter_map(|s| s.reader_f1_all).collect()),
        "mean_calls_to_complete": mean(
            done().filter_map(|s| s.calls_to_complete).map(f64::from).collect()
        ),
        "mean_tokens_to_complete": mean(
            done().filter_map(|s| s.tokens_to_complete).map(|t| t as f64).collect()
        ),
        "mean_charged_calls": mean(scores.iter().map(|s| f64::from(s.charged_calls)).collect()),
        "mean_charged_tokens": mean(scores.iter().map(|s| s.charged_tokens as f64).collect()),
        "mean_charged_pops": mean(scores.iter().map(|s| f64::from(s.charged_pops)).collect()),
        "mean_tool_calls": mean(scores.iter().map(|s| f64::from(s.tool_calls)).collect()),
        "mean_tokens_read": mean(scores.iter().map(|s| s.tokens_read as f64).collect()),
        "mean_follow_greps": mean(scores.iter().map(|s| f64::from(s.follow_greps)).collect()),
        "mean_reads": mean(scores.iter().map(|s| f64::from(s.reads)).collect()),
    });
    if let (Value::Object(out), Value::Object(means)) = (&mut out, means) {
        out.extend(means);
    }
    out
}

/// Counts, rates and means per stratum x repo x question type x split
/// (`cells`), and per stratum x split over every repo and type (`rollups`,
/// with `*` for the pooled labels). Every cell carries `n` and
/// `too_small_to_read` (n under 10). The pooled cells count hit@5 over the
/// questions that are not `setting_to_reader` (`hit_at_5_n`).
pub(crate) fn aggregate(rows: &[ScoreRow]) -> Value {
    let mut cells: BTreeMap<(&str, &str, &str, &str), Vec<&ScoreRow>> = BTreeMap::new();
    let mut rollups: BTreeMap<(&str, &str), Vec<&ScoreRow>> = BTreeMap::new();
    for row in rows {
        cells
            .entry((
                row.stratum.as_str(),
                row.repo.as_str(),
                row.qtype.as_str(),
                row.split.as_str(),
            ))
            .or_default()
            .push(row);
        rollups
            .entry((row.stratum.as_str(), row.split.as_str()))
            .or_default()
            .push(row);
    }
    let cells: Vec<Value> = cells
        .iter()
        .map(|((stratum, repo, qtype, split), rows)| cell(stratum, repo, qtype, split, rows))
        .collect();
    let rollups: Vec<Value> = rollups
        .iter()
        .map(|((stratum, split), rows)| cell(stratum, "*", "*", split, rows))
        .collect();
    json!({ "cells": cells, "rollups": rollups })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::sim_loop::Stop;

    fn span(path: &str, line: u32, end_line: u32) -> Span {
        Span {
            path: path.to_owned(),
            line,
            end_line,
            kind: None,
            how: None,
        }
    }

    fn family() -> Family {
        Family {
            id: "r.listen".to_owned(),
            stratum: "config".to_owned(),
            split: "heldout".to_owned(),
            key: "s.listen".to_owned(),
            // Two assignments: one question can cover one of them only.
            assignments: vec![span("cfg/a.toml", 10, 10), span("cfg/b.toml", 5, 6)],
            readers: vec![span("src/a.rs", 100, 100), span("src/a.rs", 200, 205)],
            readers_scope: "component:src/a.rs::serve (handoff)".to_owned(),
            docs: vec![span("src/doc.rs", 30, 34)],
        }
    }

    fn question(qtype: &str) -> Question {
        Question {
            id: format!("r.listen.{qtype}"),
            family: "r.listen".to_owned(),
            qtype: qtype.to_owned(),
            text: "text".to_owned(),
            names_key: true,
        }
    }

    fn win(path: &str, start: u32, end: u32) -> Window {
        Window {
            path: path.to_owned(),
            start,
            end,
        }
    }

    /// A call of `tokens` tokens; `cumulative_tokens` is `call * 10`.
    fn step(call: u32, kind: &'static str, windows: Vec<Window>, listed: &[(&str, u32)]) -> Step {
        Step {
            call,
            kind,
            arg: String::new(),
            tokens: 10,
            cumulative_tokens: u64::from(call) * 10,
            depth: 0,
            rarity: None,
            windows,
            listed: listed.iter().map(|(p, l)| ((*p).to_owned(), *l)).collect(),
        }
    }

    fn trace(steps: Vec<Step>, stop: Stop) -> Trace {
        Trace {
            terms: Vec::new(),
            steps,
            stop,
        }
    }

    fn near(a: Option<f64>, b: f64) -> bool {
        a.is_some_and(|a| (a - b).abs() < 1e-12)
    }

    #[test]
    fn setting_location_needs_every_assignment_and_counts_to_the_completing_call() {
        let t = trace(
            vec![
                step(1, "probe", vec![], &[]),
                step(2, "search", vec![], &[("cfg/a.toml", 10), ("x.rs", 3)]),
                // Lines 1-40 of the first file cover line 10.
                step(3, "read", vec![win("cfg/a.toml", 1, 40)], &[]),
                step(4, "follow-grep", vec![], &[]),
                // A window beside the second span does not count.
                step(5, "follow-read", vec![win("cfg/b.toml", 7, 40)], &[]),
                step(6, "follow-grep", vec![], &[]),
                // Backslash path and overlap at the span's last line count.
                step(7, "follow-read", vec![win("cfg\\b.toml", 1, 5)], &[]),
                step(8, "follow-read", vec![win("src/a.rs", 1, 40)], &[]),
            ],
            Stop::Exhausted,
        );
        let s = score_question(&question(SETTING_LOCATION), &family(), &t);
        assert!(s.complete);
        assert_eq!(s.calls_to_complete, Some(7));
        assert_eq!(s.tokens_to_complete, Some(70));
        assert_eq!((s.charged_calls, s.charged_tokens), (7, 70));
        assert_eq!(s.miss_stop, None);
        // Follow-greps at calls 4 and 6 come before the completing call 7;
        // the whole run has the same two.
        assert_eq!((s.charged_pops, s.follow_greps), (2, 2));
        assert_eq!((s.tool_calls, s.tokens_read, s.reads), (8, 80, 4));
        assert_eq!(s.reader_precision, None);
        assert!(!s.docs_only && !s.assignment_only && !s.no_prediction);
    }

    #[test]
    fn a_miss_keeps_the_stop_kind_and_charges_the_whole_run() {
        let t = trace(
            vec![
                step(1, "search", vec![], &[("cfg/a.toml", 10)]),
                step(2, "read", vec![win("cfg/a.toml", 1, 40)], &[]),
                step(3, "follow-grep", vec![], &[]),
                step(4, "follow-read", vec![win("src/a.rs", 1, 40)], &[]),
            ],
            Stop::Tokens,
        );
        let s = score_question(&question(SETTING_LOCATION), &family(), &t);
        assert!(!s.complete);
        assert_eq!((s.calls_to_complete, s.tokens_to_complete), (None, None));
        assert_eq!(s.miss_stop.as_deref(), Some("tokens"));
        assert_eq!(
            (s.charged_calls, s.charged_tokens, s.charged_pops),
            (4, 40, 1)
        );
        // The listing is not evidence, but its first file hits at rank 1.
        assert_eq!(s.listed_rank, Some(1));
        assert!(s.hit_at_5);

        // No steps at all (no term matched): a miss of zero cost.
        let empty = trace(Vec::new(), Stop::NoMatch);
        let s = score_question(&question(SETTING_LOCATION), &family(), &empty);
        assert!(!s.complete);
        assert_eq!(s.miss_stop.as_deref(), Some("no-match"));
        assert_eq!((s.charged_calls, s.charged_tokens), (0, 0));
        assert_eq!(s.listed_rank, None);
        assert!(!s.hit_at_5);
    }

    #[test]
    fn a_q2_is_complete_on_docs_alone_or_an_assignment_alone() {
        let q = question(BEHAVIOUR_TO_SETTING);
        let docs = trace(
            vec![
                step(1, "search", vec![], &[]),
                step(2, "read", vec![win("src/doc.rs", 20, 60)], &[]),
                // Read after completion: it must not change the flags.
                step(3, "follow-read", vec![win("cfg/a.toml", 1, 40)], &[]),
            ],
            Stop::Exhausted,
        );
        let s = score_question(&q, &family(), &docs);
        assert!(s.complete);
        assert!(s.docs_only);
        assert!(!s.assignment_only);
        assert_eq!(s.calls_to_complete, Some(2));
        assert_eq!(s.reader_precision, None);

        let assignment = trace(
            vec![step(1, "read", vec![win("cfg/b.toml", 6, 9)], &[])],
            Stop::Exhausted,
        );
        let s = score_question(&q, &family(), &assignment);
        assert!(s.complete);
        assert!(s.assignment_only);
        assert!(!s.docs_only);

        // Both in one read: neither flag.
        let both = trace(
            vec![step(
                1,
                "read",
                vec![win("cfg/b.toml", 6, 9), win("src/doc.rs", 1, 31)],
                &[],
            )],
            Stop::Exhausted,
        );
        let s = score_question(&q, &family(), &both);
        assert!(s.complete && !s.docs_only && !s.assignment_only);

        // Neither: a miss.
        let neither = trace(
            vec![step(1, "read", vec![win("src/doc.rs", 35, 60)], &[])],
            Stop::Calls,
        );
        let s = score_question(&q, &family(), &neither);
        assert!(!s.complete && !s.docs_only && !s.assignment_only);
        assert_eq!(s.miss_stop.as_deref(), Some("calls"));
    }

    #[test]
    fn q3_reports_reader_precision_recall_and_f1_over_the_scope_file() {
        let t = trace(
            vec![
                step(1, "search", vec![], &[]),
                step(
                    2,
                    "read",
                    vec![win("src/a.rs", 80, 120), win("src/a.rs", 300, 340)],
                    &[],
                ),
                step(3, "follow-read", vec![win("src/other.rs", 1, 40)], &[]),
                // The same window again: counted once.
                step(4, "follow-read", vec![win("src/a.rs", 80, 120)], &[]),
            ],
            Stop::Exhausted,
        );
        let s = score_question(&question(SETTING_TO_READER), &family(), &t);
        // Reader 200-205 is not covered: a miss, with partial credit.
        assert!(!s.complete);
        assert!(!s.no_prediction);
        // Scope file windows: 80-120 (covers) and 300-340 (does not).
        assert!(near(s.reader_precision, 0.5));
        assert!(near(s.reader_recall, 0.5));
        assert!(near(s.reader_f1, 0.5));
        // All three distinct windows: one of three covers a reader span.
        assert!(near(s.reader_precision_all, 1.0 / 3.0));
        assert!(near(
            s.reader_f1_all,
            2.0 * (1.0 / 3.0) * 0.5 / (1.0 / 3.0 + 0.5)
        ));
        assert_eq!(s.miss_stop.as_deref(), Some("exhausted"));

        // Covering both readers completes it.
        let t = trace(
            vec![step(
                1,
                "read",
                vec![win("src/a.rs", 90, 130), win("src/a.rs", 190, 230)],
                &[],
            )],
            Stop::Exhausted,
        );
        let s = score_question(&question(SETTING_TO_READER), &family(), &t);
        assert!(s.complete);
        assert!(near(s.reader_precision, 1.0));
        assert!(near(s.reader_recall, 1.0));
        assert!(near(s.reader_f1, 1.0));
        assert_eq!(s.calls_to_complete, Some(1));
    }

    #[test]
    fn q3_with_no_window_in_the_scope_file_has_no_prediction() {
        let t = trace(
            vec![
                step(1, "read", vec![win("src/other.rs", 1, 40)], &[]),
                step(2, "follow-read", vec![win("src/more.rs", 1, 40)], &[]),
            ],
            Stop::Calls,
        );
        let s = score_question(&question(SETTING_TO_READER), &family(), &t);
        assert!(s.no_prediction);
        assert_eq!(s.reader_precision, Some(0.0));
        assert_eq!(s.reader_recall, Some(0.0));
        assert_eq!(s.reader_f1, Some(0.0));
        assert_eq!(s.reader_precision_all, Some(0.0));
        assert!(!s.complete);

        // A run that read nothing has no prediction either.
        let s = score_question(
            &question(SETTING_TO_READER),
            &family(),
            &trace(Vec::new(), Stop::NoTerms),
        );
        assert!(s.no_prediction);
        assert_eq!(s.miss_stop.as_deref(), Some("no-terms"));
    }

    #[test]
    fn the_scope_file_is_the_component_path_before_the_function() {
        assert_eq!(
            scope_file("component:src/app/search.rs::fuse_hits").as_deref(),
            Some("src/app/search.rs")
        );
        assert_eq!(
            scope_file("component:packages/vite/src/node/server/searchRoot.ts").as_deref(),
            Some("packages/vite/src/node/server/searchRoot.ts")
        );
        assert_eq!(
            scope_file("component:a/b.py::A.__init__ (handoff to B)").as_deref(),
            Some("a/b.py")
        );
        assert_eq!(scope_file("none:package-manager"), None);
        assert_eq!(scope_file("repo"), None);
        assert_eq!(scope_file("component:"), None);
    }

    #[test]
    fn hit_at_5_ranks_the_distinct_files_of_the_first_search_listing() {
        let listing = [
            ("a1.rs", 1),
            ("a1.rs", 2),
            ("a2.rs", 1),
            ("a3.rs", 1),
            ("a4.rs", 1),
            ("a5.rs", 1),
            // Sixth distinct file, line 10 is inside the first assignment span.
            ("cfg/a.toml", 10),
        ];
        let mut f = family();
        let t = trace(vec![step(1, "search", vec![], &listing)], Stop::Exhausted);
        let s = score_question(&question(SETTING_LOCATION), &f, &t);
        assert_eq!(s.listed_rank, Some(6));
        assert!(!s.hit_at_5);

        // The same file listed first, on lines outside the span first, then in it.
        let listing = [("a1.rs", 1), ("cfg/b.toml", 1), ("cfg/b.toml", 6)];
        let t = trace(vec![step(1, "search", vec![], &listing)], Stop::Exhausted);
        let s = score_question(&question(SETTING_LOCATION), &f, &t);
        assert_eq!(s.listed_rank, Some(2));
        assert!(s.hit_at_5);

        // A listed line outside every assignment span is no hit.
        let t = trace(
            vec![step(
                1,
                "search",
                vec![],
                &[("cfg/b.toml", 7), ("cfg/a.toml", 9)],
            )],
            Stop::Exhausted,
        );
        let s = score_question(&question(SETTING_LOCATION), &f, &t);
        assert_eq!(s.listed_rank, None);
        assert!(!s.hit_at_5);

        // Only the first search step counts, and probes are not searches.
        f.assignments = vec![span("cfg/a.toml", 10, 10)];
        let t = trace(
            vec![
                step(1, "probe", vec![], &[("cfg/a.toml", 10)]),
                step(2, "search", vec![], &[("x.rs", 1)]),
                step(3, "search", vec![], &[("cfg/a.toml", 10)]),
            ],
            Stop::Exhausted,
        );
        let s = score_question(&question(SETTING_LOCATION), &f, &t);
        assert_eq!(s.listed_rank, None);

        // q3 is scored too (reported as not the asked fact).
        let t = trace(
            vec![step(1, "search", vec![], &[("cfg/a.toml", 10)])],
            Stop::Exhausted,
        );
        let s = score_question(&question(SETTING_TO_READER), &f, &t);
        assert_eq!(s.listed_rank, Some(1));
    }

    #[test]
    fn a_window_covers_a_span_from_the_first_to_the_last_line_inclusive() {
        let s = span("cfg/a.toml", 10, 12);
        // The window ends on the span's first line: covered. One line short: not.
        assert!(covers(&win("cfg/a.toml", 1, 10), &s));
        assert!(!covers(&win("cfg/a.toml", 1, 9), &s));
        // The window starts on the span's last line: covered. One line past: not.
        assert!(covers(&win("cfg/a.toml", 12, 40), &s));
        assert!(!covers(&win("cfg/a.toml", 13, 40), &s));
        // Inside, around, and a one-line window on a one-line span.
        assert!(covers(&win("cfg/a.toml", 11, 11), &s));
        assert!(covers(&win("cfg/a.toml", 1, 40), &s));
        let one = span("cfg/a.toml", 10, 10);
        assert!(covers(&win("cfg/a.toml", 10, 10), &one));
        assert!(!covers(&win("cfg/a.toml", 11, 11), &one));
        // Another file never covers it.
        assert!(!covers(&win("cfg/b.toml", 1, 40), &s));

        // The same edges decide completion.
        let t = |window| trace(vec![step(1, "read", vec![window], &[])], Stop::Exhausted);
        let mut f = family();
        f.assignments = vec![s.clone()];
        let q = question(SETTING_LOCATION);
        assert!(score_question(&q, &f, &t(win("cfg/a.toml", 1, 10))).complete);
        assert!(!score_question(&q, &f, &t(win("cfg/a.toml", 1, 9))).complete);
        assert!(score_question(&q, &f, &t(win("cfg/a.toml", 12, 40))).complete);
        assert!(!score_question(&q, &f, &t(win("cfg/a.toml", 13, 40))).complete);
    }

    #[test]
    fn an_unknown_question_type_is_never_complete() {
        let t = trace(
            vec![step(1, "read", vec![win("cfg/a.toml", 1, 40)], &[])],
            Stop::Exhausted,
        );
        assert!(!score_question(&question("other"), &family(), &t).complete);
    }

    fn row(id: &str, repo: &str, stratum: &str, qtype: &str, complete: bool) -> ScoreRow {
        ScoreRow {
            question_id: id.to_owned(),
            family: format!("{id}.family"),
            repo: repo.to_owned(),
            stratum: stratum.to_owned(),
            split: "heldout".to_owned(),
            qtype: qtype.to_owned(),
            score: QuestionScore {
                complete,
                miss_stop: (!complete).then(|| "calls".to_owned()),
                charged_tokens: 100,
                charged_calls: 4,
                tool_calls: 4,
                tokens_read: 100,
                ..QuestionScore::default()
            },
        }
    }

    #[test]
    fn rows_round_trip_through_json() {
        let t = trace(
            vec![
                step(1, "search", vec![], &[("cfg/a.toml", 10)]),
                // One of two windows in the scope file covers a reader: every
                // reader figure is exactly 0.5, which JSON reads back exactly.
                step(
                    2,
                    "read",
                    vec![win("src/a.rs", 80, 120), win("src/a.rs", 300, 340)],
                    &[],
                ),
            ],
            Stop::Gold,
        );
        let f = family();
        let q = question(SETTING_TO_READER);
        let score = score_question(&q, &f, &t);
        let row = ScoreRow {
            question_id: q.id.clone(),
            family: f.id.clone(),
            repo: "r".to_owned(),
            stratum: f.stratum.clone(),
            split: f.split.clone(),
            qtype: q.qtype.clone(),
            score,
        };
        let back = |row: &ScoreRow| -> ScoreRow {
            serde_json::from_value(serde_json::to_value(row).unwrap()).unwrap()
        };
        assert_eq!(back(&row), row);
        assert_eq!(
            serde_json::to_value(&row).unwrap()["score"]["reader_f1"],
            0.5
        );

        // A miss with its stop kind and a docs-only q2 survive too.
        let mut miss = row.clone();
        miss.score.miss_stop = Some("tokens".to_owned());
        miss.score.docs_only = true;
        assert_eq!(back(&miss), miss);
        assert!(serde_json::from_value::<ScoreRow>(json!({"question_id": "x"})).is_err());
    }

    #[test]
    fn the_aggregate_groups_by_stratum_repo_type_and_split_with_n() {
        let mut rows = Vec::new();
        for i in 0..12 {
            rows.push(row(
                &format!("c{i}"),
                "lci",
                "config",
                SETTING_LOCATION,
                i % 4 != 0,
            ));
        }
        for i in 0..3 {
            rows.push(row(
                &format!("d{i}"),
                "lci",
                "dependency",
                SETTING_LOCATION,
                true,
            ));
        }
        let mut dev = row("dev1", "lci", "config", SETTING_LOCATION, true);
        dev.split = "dev".to_owned();
        rows.push(dev);

        let agg = aggregate(&rows);
        let cells = agg["cells"].as_array().unwrap();
        assert_eq!(cells.len(), 3);
        fn find<'a>(cells: &'a [Value], stratum: &str, split: &str) -> &'a Value {
            cells
                .iter()
                .find(|c| c["stratum"] == stratum && c["split"] == split)
                .unwrap()
        }
        let config = find(cells, "config", "heldout");
        assert_eq!(config["n"], 12);
        assert_eq!(config["too_small_to_read"], false);
        assert_eq!(config["complete"], 9);
        assert_eq!(config["success_rate"], 0.75);
        assert_eq!(config["misses_by_stop"]["calls"], 3);
        assert_eq!(config["mean_charged_tokens"], 100.0);
        let dep = find(cells, "dependency", "heldout");
        assert_eq!(dep["n"], 3);
        assert_eq!(dep["too_small_to_read"], true);
        let dev = find(cells, "config", "dev");
        assert_eq!(dev["n"], 1);
        assert_eq!(dev["too_small_to_read"], true);

        let rollups = agg["rollups"].as_array().unwrap();
        assert_eq!(rollups.len(), 3);
        assert!(
            rollups
                .iter()
                .all(|r| r["repo"] == "*" && r["qtype"] == "*")
        );
        assert!(aggregate(&[])["cells"].as_array().unwrap().is_empty());
    }

    #[test]
    fn the_rollup_hit_rate_leaves_q3_out_but_its_own_cell_keeps_it() {
        let hit = |id: &str, qtype: &str, hit: bool| {
            let mut r = row(id, "lci", "config", qtype, true);
            r.score.hit_at_5 = hit;
            r
        };
        let rows = vec![
            hit("a", SETTING_LOCATION, true),
            hit("b", SETTING_LOCATION, false),
            hit("c", BEHAVIOUR_TO_SETTING, true),
            hit("d", SETTING_TO_READER, true),
            hit("e", SETTING_TO_READER, true),
        ];
        let agg = aggregate(&rows);
        let rollup = &agg["rollups"][0];
        // Five questions, but only the three that are not q3 count for hit@5.
        assert_eq!(rollup["n"], 5);
        assert_eq!(rollup["hit_at_5"], 2);
        assert_eq!(rollup["hit_at_5_n"], 3);
        assert_eq!(rollup["hit_at_5_rate"], 2.0 / 3.0);
        // The q3 cell reports its own hits (not the asked fact, but shown).
        let q3 = agg["cells"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["qtype"] == SETTING_TO_READER)
            .unwrap();
        assert_eq!(
            (q3["hit_at_5"].as_u64(), q3["hit_at_5_n"].as_u64()),
            (Some(2), Some(2))
        );
        // A pool of q3 only has no hit rate.
        let only = aggregate(&rows[3..]);
        assert_eq!(only["rollups"][0]["hit_at_5_n"], 0);
        assert!(only["rollups"][0]["hit_at_5_rate"].is_null());
    }
}
