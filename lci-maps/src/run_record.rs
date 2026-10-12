//! RunRecord writing (spike slice S3; spec
//! `docs/spike/multimap/specs/run-record.md`, schema
//! `docs/spike/multimap/specs/runrecord-0.schema.json`).
//!
//! A RunRecord is JSON Lines: one `run` line, one `step` line per engine
//! step, one `end` line. [`RunRecordWriter`] checks every line against the
//! schema before writing it and appends it with a single `write_all`, so a
//! writer killed mid-run leaves a readable prefix of whole lines (at worst
//! a torn last line, which a reader drops). Every writer method returns an
//! error instead of panicking: the caller keeps a run going when the record
//! cannot be written (fail-open).
//!
//! This module also turns the registry's types ([`MapCall`], [`Fused`],
//! [`Cost`]) into their record form. Nothing on the production search path
//! writes a RunRecord.

mod schema;

pub use schema::Schema;

use crate::{
    Contribution, Cost, EdgeKind, FreshnessModel, Fused, MapCall, MapStatus, Observed, Run,
    SnapshotView,
};
use serde_json::{Map as JsonMap, Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::OnceLock,
    time::{SystemTime, UNIX_EPOCH},
};

/// The schema id every `run` line carries. Not frozen: `/1` is the first
/// frozen version (S4).
pub const SCHEMA_ID: &str = "lci.runrecord/0";

/// The checked-in schema, compiled into the crate so a writer never runs
/// against a different file than the one in the repository.
pub const SCHEMA_JSON: &str =
    include_str!("../../docs/spike/multimap/specs/runrecord-0.schema.json");

/// Most frontier rows written per step unless the run says otherwise
/// (run-record.md, R-3); a step's `frontier.size` is always exact.
pub const DEFAULT_FRONTIER_ROWS: usize = 50;

/// The longest `detail` of an evidence item, in characters.
pub const DETAIL_MAX_CHARS: usize = 200;

/// The parsed RunRecord schema, or why the checked-in file does not load
/// (a test keeps it loading; a writer reports the error instead of
/// panicking).
pub fn schema() -> Result<&'static Schema, String> {
    static SCHEMA: OnceLock<Result<Schema, String>> = OnceLock::new();
    SCHEMA
        .get_or_init(|| Schema::parse(SCHEMA_JSON))
        .as_ref()
        .map_err(|error| format!("the RunRecord schema does not load: {error}"))
}

/// Checks one line against the schema.
pub fn validate_line(line: &Value) -> Result<(), String> {
    schema()?.validate(line)
}

/// Crockford's base32 alphabet, as ULIDs use it.
const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

fn crockford(mut value: u128, digits: usize) -> String {
    let mut out = vec![b'0'; digits];
    for slot in out.iter_mut().rev() {
        *slot = CROCKFORD[(value & 31) as usize];
        value >>= 5;
    }
    String::from_utf8(out).expect("ASCII")
}

/// A ULID for `at`: 48 bits of milliseconds since the epoch, then 80 random
/// bits (from the standard library's randomly keyed hasher; no new
/// dependency). Sortable by start time to the millisecond.
pub fn run_id_at(at: SystemTime) -> String {
    use std::hash::{BuildHasher, Hasher};
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let millis = (since.as_millis() as u64) & ((1 << 48) - 1);
    let mut random = 0u128;
    for round in 0..2u64 {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(COUNTER.fetch_add(1, Ordering::Relaxed));
        hasher.write_u128(since.as_nanos());
        hasher.write_u32(std::process::id());
        hasher.write_u64(round);
        random = (random << 64) | u128::from(hasher.finish());
    }
    random &= (1 << 80) - 1;
    crockford(u128::from(millis), 10) + &crockford(random, 16)
}

/// RFC 3339 UTC with milliseconds, from the system clock.
pub fn utc_rfc3339(at: SystemTime) -> String {
    let since = at.duration_since(UNIX_EPOCH).unwrap_or_default();
    let seconds = since.as_secs();
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600,
        rest % 3_600 / 60,
        rest % 60,
        since.subsec_millis()
    )
}

/// Replaces each secret (an absolute path, a service URL, a `host:port`) in
/// every string of `value` with its placeholder, wherever it stands on its
/// own: the character before it is not a letter, digit, `_`, `-` or `.`,
/// and the one after it is not a letter, digit, `_` or `-`. So a secret
/// `/src` redacts `/src/x.rs` and `"/src"` but leaves the repository path
/// `lib/src/x.rs` (and so the NodeIds) alone. Longer secrets are replaced
/// first, so a path inside another is never half-replaced. Empty secrets
/// are ignored.
pub fn redact(value: &mut Value, secrets: &[(String, &str)]) {
    let mut ordered: Vec<&(String, &str)> = secrets.iter().filter(|(s, _)| !s.is_empty()).collect();
    ordered.sort_by_key(|(secret, _)| std::cmp::Reverse(secret.len()));
    fn word(c: char) -> bool {
        c.is_alphanumeric() || c == '_' || c == '-'
    }
    fn replace(text: &str, secret: &str, placeholder: &str) -> String {
        let mut out = String::with_capacity(text.len());
        let mut rest = 0;
        let mut from = 0;
        while let Some(found) = text[from..].find(secret) {
            let start = from + found;
            let end = start + secret.len();
            let before = text[..start].chars().next_back();
            let after = text[end..].chars().next();
            if before.is_some_and(|c| word(c) || c == '.') || after.is_some_and(word) {
                from = start + text[start..].chars().next().map_or(1, char::len_utf8);
                continue;
            }
            out.push_str(&text[rest..start]);
            out.push_str(placeholder);
            rest = end;
            from = end;
        }
        out.push_str(&text[rest..]);
        out
    }
    fn walk(value: &mut Value, secrets: &[&(String, &str)]) {
        match value {
            Value::String(text) => {
                for (secret, placeholder) in secrets {
                    if text.contains(secret.as_str()) {
                        *text = replace(text, secret, placeholder);
                    }
                }
            }
            Value::Array(items) => items.iter_mut().for_each(|item| walk(item, secrets)),
            Value::Object(fields) => fields.values_mut().for_each(|field| walk(field, secrets)),
            _ => {}
        }
    }
    walk(value, &ordered);
}

/// An `f32` as a JSON number, widened to `f64` (exact). Narrowing the parsed
/// number back to `f32` gives the same bits even though serde_json's default
/// float parsing is best effort in the last `f64` bits: the value is an
/// `f32`, whose spacing is 2^29 `f64` steps, so a parse that is off by a few
/// steps still rounds to it. The replay depends on this.
pub fn number(value: f32) -> Value {
    json!(f64::from(value))
}

pub fn contribution_name(contribution: Contribution) -> &'static str {
    match contribution {
        Contribution::RankList => "rank_list",
        Contribution::Prior => "prior",
        Contribution::Filter => "filter",
        Contribution::Expansion => "expansion",
    }
}

fn freshness_name(model: FreshnessModel) -> &'static str {
    match model {
        FreshnessModel::PerFile => "per-file",
        FreshnessModel::PerCommit => "per-commit",
        FreshnessModel::Live => "live",
        FreshnessModel::Global => "global",
    }
}

fn edge_name(edge: EdgeKind) -> &'static str {
    match edge {
        EdgeKind::QueryMatch => "query_match",
        EdgeKind::Prior => "prior",
    }
}

pub fn cost_json(cost: &Cost) -> Value {
    json!({
        "elapsed_ms": cost.elapsed_ms.max(0.0),
        "external_calls": cost.external_calls,
        "nodes_touched": cost.nodes_touched,
        "bytes_read": cost.bytes_read,
        "est_tokens": cost.est_tokens,
    })
}

fn bounded_detail(detail: &str) -> String {
    match detail.char_indices().nth(DETAIL_MAX_CHARS) {
        Some((cut, _)) => detail[..cut].to_owned(),
        None => detail.to_owned(),
    }
}

/// One map call as the registry recorded it. `cost.elapsed_ms` is the wall
/// time the registry measured around the call (`MapCall::engine_ms`), so a
/// failure the registry decided (a panic, a deadline) still has a time; the
/// map's other cost fields are kept as it reported them. `text_sent` and
/// `limit` describe the request: the registry sends the query text to every
/// call.
pub fn map_call_json(call: &MapCall, text_sent: bool, limit: Option<usize>) -> Value {
    let response = &call.response;
    let (status, message) = match &response.status {
        MapStatus::Ok => ("ok", None),
        MapStatus::Partial { note, warning } => (
            "partial",
            Some(
                [warning.as_deref(), note.as_deref()]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
        ),
        MapStatus::Skipped { reason } => ("skipped", Some(reason.clone())),
        MapStatus::Failed { warning } => ("failed", Some(warning.clone())),
    };
    let mut cost = cost_json(&response.cost);
    cost["elapsed_ms"] = json!(call.engine_ms.max(0.0));
    let hits: Vec<Value> = response
        .hits
        .iter()
        .map(|hit| {
            let evidence: Vec<Value> = hit
                .evidence
                .iter()
                .map(|item| {
                    let mut object = JsonMap::new();
                    if let Some(via) = &item.via {
                        object.insert("via".into(), json!(via.to_string()));
                    }
                    object.insert("edge".into(), json!(edge_name(item.edge)));
                    object.insert("label".into(), json!(item.label.as_str()));
                    if let Some(detail) = &item.detail {
                        object.insert("detail".into(), json!(bounded_detail(detail)));
                    }
                    Value::Object(object)
                })
                .collect();
            json!({
                "node": hit.node.to_string(),
                "rank": hit.rank,
                "score": hit.score.map(number),
                "evidence": evidence,
            })
        })
        .collect();
    let mut request = JsonMap::new();
    request.insert("text".into(), json!(text_sent));
    if let Some(limit) = limit {
        request.insert("limit".into(), json!(limit));
    }
    let mut object = JsonMap::new();
    object.insert("map".into(), json!(call.id));
    object.insert("version".into(), json!(call.version));
    object.insert("request".into(), Value::Object(request));
    object.insert("status".into(), json!(status));
    if let Some(message) = message {
        object.insert("message".into(), json!(message));
    }
    object.insert("cost".into(), cost);
    object.insert(
        "freshness".into(),
        json!({
            "model": freshness_name(response.freshness.model),
            "observed": match response.freshness.observed {
                Observed::Snapshot => "snapshot",
                Observed::LiveTree => "live-tree",
                Observed::AsOf(_) => "as-of",
            },
            "stale": response.freshness.stale,
        }),
    );
    object.insert("hits".into(), Value::Array(hits));
    Value::Object(object)
}

/// Each map's part of a fused score, by map id. A map adds at most one
/// part per node (the registry refuses an answer with a node twice).
pub fn contributions_json(fused: &Fused) -> Value {
    let mut object = JsonMap::new();
    for part in &fused.parts {
        object.insert(part.map.into(), number(part.value));
    }
    Value::Object(object)
}

/// The frontier row of a fused node: node, lines (for a chunk of the view),
/// score, contributions and its one-based `fused_rank`. The caller adds
/// what only it knows (source role, snippet, cap and cut).
pub fn fused_row(
    fused: &Fused,
    fused_rank: usize,
    view: &SnapshotView<'_>,
) -> JsonMap<String, Value> {
    let mut row = JsonMap::new();
    row.insert("node".into(), json!(fused.node.to_string()));
    if let Some(chunk) = view.chunk(&fused.node) {
        row.insert("start_line".into(), json!(chunk.start_line));
        row.insert("end_line".into(), json!(chunk.end_line));
    }
    row.insert("score".into(), number(fused.score));
    row.insert("contributions".into(), contributions_json(fused));
    row.insert("fused_rank".into(), json!(fused_rank));
    row
}

/// The map calls of a registry run split into its two engine steps:
/// rank lists and expansions (fanned out together, step `fan_out`, yield
/// Y1) and priors (asked about the fused nodes, step `fuse`).
pub fn split_calls(run: &Run) -> (Vec<&MapCall>, Vec<&MapCall>) {
    run.calls
        .iter()
        .partition(|call| call.contribution != Contribution::Prior)
}

/// The evaluation item a recorded run answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RecordItem {
    /// A public set name, or an opaque id for a private set.
    pub set: String,
    pub id: String,
}

/// Where and what a recorded run writes.
#[derive(Debug, Clone)]
pub struct RecordOptions {
    /// The record is `<dir>/<run_id>.jsonl`.
    pub dir: PathBuf,
    /// The result window, as `search_code`'s `top_k`; `None` uses the
    /// service default.
    pub top_k: Option<usize>,
    /// Write bounded snippets (run-record.md 3.2); off by default.
    pub capture_snippets: bool,
    /// Most frontier rows per step (R-3).
    pub frontier_rows: usize,
    pub item: Option<RecordItem>,
    pub arm: Option<String>,
}

impl RecordOptions {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            top_k: None,
            capture_snippets: false,
            frontier_rows: DEFAULT_FRONTIER_ROWS,
            item: None,
            arm: None,
        }
    }
}

/// A run's own outcome and its record.
#[derive(Debug)]
pub struct Recorded<T> {
    /// What the run returns, exactly as without a record.
    pub value: T,
    /// The record's path, or why it was not written whole (fail-open). A
    /// record cut short keeps the lines written before the failure.
    pub record: Result<PathBuf, String>,
}

/// An append-only JSON Lines file for one run.
#[derive(Debug)]
pub struct RunRecordWriter {
    file: File,
    path: PathBuf,
    lines: usize,
}

impl RunRecordWriter {
    /// Creates `<dir>/<run_id>.jsonl` (and `dir`); never overwrites a file.
    pub fn create(dir: &Path, run_id: &str) -> io::Result<Self> {
        if run_id.len() != 26 || !run_id.bytes().all(|b| CROCKFORD.contains(&b)) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("run id {run_id:?} is not a ULID"),
            ));
        }
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{run_id}.jsonl"));
        let file = OpenOptions::new()
            .append(true)
            .create_new(true)
            .open(&path)?;
        Ok(Self {
            file,
            path,
            lines: 0,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Lines written so far.
    pub fn lines(&self) -> usize {
        self.lines
    }

    /// Checks `line` against the schema and appends it, newline included,
    /// with one `write_all`. A line the schema refuses is not written.
    pub fn append(&mut self, line: &Value) -> io::Result<()> {
        validate_line(line).map_err(|error| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("RunRecord line refused by {SCHEMA_ID}: {error}"),
            )
        })?;
        let mut bytes = serde_json::to_vec(line)?;
        bytes.push(b'\n');
        self.file.write_all(&bytes)?;
        self.lines += 1;
        Ok(())
    }
}

#[cfg(test)]
mod tests;
