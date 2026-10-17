//! Scoring a trace against the gold of one item. The loop never sees this;
//! only `stop_at_gold` lets the gold end a run early.
//!
//! A read window hits the gold when it overlaps a gold span, or, for a gold
//! file that has no span, when it is a window of that file (any line). The
//! gold files come from `gold::expected_files`, the helper the oracle arms use
//! (impact: the R1 files; flow: the intermediate files). Spans exist only for
//! where-to (see `gold_spans`).

use super::sim_loop::Trace;
use crate::gold::{self, Set};
use serde_json::{Value, json};
use std::collections::BTreeSet;

/// Calls the smaller budget allows (`hit_at_12`).
pub const SMALL_BUDGET: u32 = 12;
/// Calls the larger budget allows (`hit_at_25`).
pub const LARGE_BUDGET: u32 = 25;

/// Where the gold of one item lies.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GoldTarget {
    /// (path, first line, last line), 1-based inclusive.
    spans: Vec<(String, i64, i64)>,
    /// Gold files with no span: any line counts.
    files: BTreeSet<String>,
}

fn norm(path: &str) -> String {
    path.replace('\\', "/")
}

/// The spans of the gold entries of one set, from the same sub-objects
/// `gold::expected_files` reads. Only where-to has spans (`expected` for a
/// registration, `members` for an exemplar, `impls` and `equivalents` for a
/// convention); impact and flow are scored by file, as the oracle arms of
/// `maps` do, so the R2 and test declarations of an impact item never count.
fn gold_spans(set: Set, item: &Value) -> Vec<(String, i64, i64)> {
    let keys: &[&str] = match set {
        Set::WhereTo => &["expected", "members", "impls", "equivalents"],
        _ => &[],
    };
    let mut out = Vec::new();
    for key in keys {
        for entry in gold::arr_of(&item["gold"], key) {
            let pair = ["span", "lines"].iter().find_map(|k| {
                let pair = entry.get(*k)?.as_array()?;
                Some((pair.first()?.as_i64()?, pair.get(1)?.as_i64()?))
            });
            if let (Some(path), Some((a, b))) = (gold::str_of(entry, "path"), pair) {
                out.push((norm(path), a.min(b), a.max(b)));
            }
        }
    }
    out
}

impl GoldTarget {
    #[cfg(test)]
    pub fn new(spans: Vec<(String, i64, i64)>, files: impl IntoIterator<Item = String>) -> Self {
        Self {
            spans,
            files: files.into_iter().map(|f| norm(&f)).collect(),
        }
    }

    /// The target of a gold item: its spans, and the expected files that have
    /// none.
    pub fn from_item(set: Set, item: &Value) -> Self {
        let spans = gold_spans(set, item);
        let with_span: BTreeSet<&str> = spans.iter().map(|(p, _, _)| p.as_str()).collect();
        let files: BTreeSet<String> = gold::expected_files(set, item)
            .unwrap_or_default()
            .iter()
            .map(|f| norm(f))
            .filter(|f| !with_span.contains(f.as_str()))
            .collect();
        Self { spans, files }
    }

    /// Whether the item has any gold to hit.
    pub fn is_known(&self) -> bool {
        !self.spans.is_empty() || !self.files.is_empty()
    }

    /// Whether lines `start..=end` of `path` (1-based) are gold.
    pub fn hits(&self, path: &str, start: u32, end: u32) -> bool {
        let path = norm(path);
        let (start, end) = (i64::from(start), i64::from(end));
        self.files.contains(&path)
            || self
                .spans
                .iter()
                .any(|(p, a, b)| *p == path && *a <= end && start <= *b)
    }
}

/// The scores of one item.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemScore {
    pub id: String,
    /// Calls made before the run stopped.
    pub tool_calls: u32,
    /// Tokens of all tool output.
    pub tokens_read: u64,
    /// The call of the first read that hit the gold.
    pub calls_to_gold: Option<u32>,
    /// Tokens read up to and including that call.
    pub tokens_to_gold: Option<u64>,
    pub hit_at_12: bool,
    pub hit_at_25: bool,
    /// Position (1-based, among the files of the first search listing) of the
    /// first file with a listed line that hits the gold.
    pub listed_at: Option<u32>,
    /// False when the item has no gold to hit (hits are then not defined).
    pub gold_known: bool,
    pub trace: Trace,
}

impl ItemScore {
    pub fn from_trace(id: &str, trace: Trace, target: &GoldTarget) -> Self {
        let first_hit = trace.steps.iter().find(|step| {
            step.windows
                .iter()
                .any(|w| target.hits(&w.path, w.start, w.end))
        });
        let calls_to_gold = first_hit.map(|s| s.call);
        let listed_at = trace
            .steps
            .iter()
            .find(|s| s.kind == "search")
            .and_then(|search| {
                let mut files: Vec<&str> = Vec::new();
                for (path, _) in &search.listed {
                    if !files.contains(&path.as_str()) {
                        files.push(path);
                    }
                }
                files.iter().position(|file| {
                    search
                        .listed
                        .iter()
                        .any(|(p, line)| p == file && target.hits(p, *line, *line))
                })
            })
            .map(|i| i as u32 + 1);
        Self {
            id: id.to_owned(),
            tool_calls: trace.steps.len() as u32,
            tokens_read: trace.tokens_read(),
            calls_to_gold,
            tokens_to_gold: first_hit.map(|s| s.cumulative_tokens),
            hit_at_12: calls_to_gold.is_some_and(|c| c <= SMALL_BUDGET),
            hit_at_25: calls_to_gold.is_some_and(|c| c <= LARGE_BUDGET),
            listed_at,
            gold_known: target.is_known(),
            trace,
        }
    }

    /// The record `items.json` holds: numbers only (what `maps compare`
    /// reads), `null` where undefined. `expansions` is the call count and
    /// `model_calls` 0, as for the other non-walk arms.
    pub fn metrics_json(&self) -> Value {
        let flag = |hit: bool| -> Value {
            if self.gold_known {
                json!(u8::from(hit))
            } else {
                Value::Null
            }
        };
        json!({
            "tool_calls": self.tool_calls,
            "tokens_read": self.tokens_read,
            "calls_to_gold": self.calls_to_gold,
            "tokens_to_gold": self.tokens_to_gold,
            "hit_at_12": flag(self.hit_at_12),
            "hit_at_25": flag(self.hit_at_25),
            "listed_at": self.listed_at,
            "expansions": self.tool_calls,
            "model_calls": 0,
        })
    }

    /// The full record of `sim.json`, with the trace.
    pub fn full_json(&self) -> Value {
        let mut record = self.metrics_json();
        record["id"] = json!(self.id);
        record["gold_known"] = json!(self.gold_known);
        record["trace"] = self.trace.to_json();
        record
    }
}

/// Means and rates over the items; hit rates and the means to the gold are
/// over the items that have gold.
pub fn aggregate(scores: &[ItemScore]) -> Value {
    let n = scores.len();
    let known: Vec<&ItemScore> = scores.iter().filter(|s| s.gold_known).collect();
    let mean = |values: Vec<f64>| -> Value {
        if values.is_empty() {
            Value::Null
        } else {
            json!(values.iter().sum::<f64>() / values.len() as f64)
        }
    };
    let rate = |count: usize| -> Value {
        if known.is_empty() {
            Value::Null
        } else {
            json!(count as f64 / known.len() as f64)
        }
    };
    json!({
        "items": n,
        "items_with_gold": known.len(),
        "mean_tool_calls": mean(scores.iter().map(|s| f64::from(s.tool_calls)).collect()),
        "mean_tokens_read": mean(scores.iter().map(|s| s.tokens_read as f64).collect()),
        "hit_at_12": rate(known.iter().filter(|s| s.hit_at_12).count()),
        "hit_at_25": rate(known.iter().filter(|s| s.hit_at_25).count()),
        "listed_rate": rate(known.iter().filter(|s| s.listed_at.is_some()).count()),
        "mean_calls_to_gold_among_hits": mean(
            known.iter().filter_map(|s| s.calls_to_gold).map(f64::from).collect()
        ),
        "mean_tokens_to_gold_among_hits": mean(
            known.iter().filter_map(|s| s.tokens_to_gold).map(|t| t as f64).collect()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_window_hits_a_span_by_overlap_and_a_spanless_file_by_name() {
        let target = GoldTarget::new(vec![("a.rs".into(), 10, 20)], ["b.rs".to_owned()]);
        assert!(target.hits("a.rs", 1, 10));
        assert!(target.hits("a.rs", 20, 60));
        assert!(!target.hits("a.rs", 21, 60));
        assert!(!target.hits("a.rs", 1, 9));
        assert!(target.hits("b.rs", 500, 540));
        assert!(!target.hits("c.rs", 1, 99));
        assert!(target.hits("a.rs", 15, 15));
    }

    #[test]
    fn from_item_reads_spans_and_keeps_span_files_span_only() {
        let item = json!({
            "id": "t", "question": "q",
            "accept": {"preferred": [{"path": "pref.rs", "lines": [1, 9]}]},
            "gold": {
                "form": "registration", "expected_files": ["reg.rs", "other.rs"],
                "expected": [{"path": "reg.rs", "lines": [10, 20]}]
            }
        });
        let target = GoldTarget::from_item(Set::WhereTo, &item);
        assert!(target.is_known());
        // reg.rs has a span: only an overlapping window counts.
        assert!(target.hits("reg.rs", 15, 16));
        assert!(!target.hits("reg.rs", 30, 40));
        // other.rs has none: any window of it counts.
        assert!(target.hits("other.rs", 30, 40));
        // accept sites are not gold.
        assert!(!target.hits("pref.rs", 1, 9));
    }

    #[test]
    fn impact_is_scored_by_r1_file_and_r2_or_test_spans_do_not_hit() {
        let item = json!({
            "id": "impact-t1", "primary": true, "question": "q",
            "gold": {
                "r1": {
                    "decls": [{"path": "one.rs", "line": 10, "span": [10, 20], "name": "one", "symbol": "s:one"}],
                    "files": ["one.rs"]
                },
                "r2": {"decls": [{"path": "two.rs", "line": 1, "span": [1, 5], "name": "two", "symbol": "s:two"}]},
                "tests": [{"decl": {"path": "t.rs", "line": 3, "span": [3, 8], "name": "t", "symbol": "s:t"}}]
            }
        });
        let target = GoldTarget::from_item(Set::Impact, &item);
        // R1 is file-level: any window of the file counts, as for the oracle arms.
        assert!(target.hits("one.rs", 90, 99));
        assert!(!target.hits("two.rs", 1, 5));
        assert!(!target.hits("t.rs", 3, 8));
    }

    #[test]
    fn an_item_without_gold_is_unknown_and_never_hit() {
        let item = json!({"id": "t", "question": "q", "gold": {"form": "registration"}});
        let target = GoldTarget::from_item(Set::WhereTo, &item);
        assert!(!target.is_known());
        assert!(!target.hits("a.rs", 1, 99));
    }
}
