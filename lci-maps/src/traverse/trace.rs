//! The trace log (spine brief 1.6): the engine's JSON events as JSONL, one
//! event per line, plus the hashes the events cite.
//!
//! The trace record is [`TraversalResult::events`]; nothing here defines a
//! second format. The events carry no wall-clock value, so two runs with the
//! same inputs write byte-identical files. A `stop` event names the hash of
//! the [`Checkpoint`] the handler saw, and the `steer` that follows cites the
//! same hash as `inputs_hash`, so a decision can be tied to its inputs
//! without storing them. Hashes are the first 16 hex digits of SHA-256.

use super::{TraversalResult, handler::Checkpoint};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

/// The first 16 hex digits of the SHA-256 of `bytes`.
pub fn hash16(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .take(8)
        .fold(String::with_capacity(16), |mut out, byte| {
            out.push_str(&format!("{byte:02x}"));
            out
        })
}

/// The hash of a value's compact JSON text (object keys in the order the
/// value holds them, which is fixed for every value the engine builds).
pub fn value_hash(value: &Value) -> String {
    hash16(value.to_string().as_bytes())
}

/// A checkpoint as the hash and the trace see it.
pub fn checkpoint_json(checkpoint: &Checkpoint) -> Value {
    json!({
        "pops": checkpoint.pops,
        "stop": checkpoint.stop,
        "budget_left_pct": checkpoint.budget_left_pct,
        "tokens_left": checkpoint.tokens_left,
        "visited": checkpoint.visited,
        "cleared": checkpoint.cleared,
        "frontier": checkpoint.frontier.iter().map(|item| json!({
            "node": item.node.to_string(),
            "via": item.via,
            "witness": item.witness.map(|w| w.as_str()),
            "score": item.score,
            "depth": item.depth,
            "fanout": item.fanout,
            "seen_before": item.seen_before,
        })).collect::<Vec<_>>(),
    })
}

/// The hash of a checkpoint: equal checkpoints give equal hashes.
pub fn checkpoint_hash(checkpoint: &Checkpoint) -> String {
    value_hash(&checkpoint_json(checkpoint))
}

/// The events as JSONL text.
pub fn jsonl(events: &[Value]) -> String {
    events.iter().map(|e| format!("{e}\n")).collect()
}

/// The hash of a whole trace (its JSONL text): what a result document cites.
pub fn trace_hash(events: &[Value]) -> String {
    hash16(jsonl(events).as_bytes())
}

/// Appends events to a JSONL file.
pub struct TraceWriter {
    out: BufWriter<File>,
    path: PathBuf,
}

impl TraceWriter {
    /// Creates (or truncates) `path`, and its parent directories.
    pub fn create(path: impl Into<PathBuf>) -> io::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        Ok(Self {
            out: BufWriter::new(File::create(&path)?),
            path,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&mut self, event: &Value) -> io::Result<()> {
        writeln!(self.out, "{event}")
    }

    pub fn append_all(&mut self, events: &[Value]) -> io::Result<()> {
        events.iter().try_for_each(|event| self.append(event))
    }

    pub fn finish(mut self) -> io::Result<PathBuf> {
        self.out.flush()?;
        Ok(self.path)
    }
}

/// Writes `events` to `path` as JSONL.
pub fn write_events(path: impl Into<PathBuf>, events: &[Value]) -> io::Result<PathBuf> {
    let mut writer = TraceWriter::create(path)?;
    writer.append_all(events)?;
    writer.finish()
}

/// Writes a result's trace to `dir/<name>.jsonl`: `name` is the handle when
/// the run can be resumed, else `t` and the trace hash. Returns the path.
pub fn write_trace(dir: &Path, result: &TraversalResult) -> io::Result<PathBuf> {
    let name = match &result.snapshot {
        Some(snapshot) => snapshot.handle(),
        None => format!("t{}", trace_hash(&result.events)),
    };
    write_events(dir.join(format!("{name}.jsonl")), &result.events)
}

/// Reads a JSONL trace; a line that is not JSON is an `InvalidData` error
/// naming the line.
pub fn read_trace(path: impl AsRef<Path>) -> io::Result<Vec<Value>> {
    let mut events = Vec::new();
    for (i, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = line?;
        if line.is_empty() {
            continue;
        }
        events.push(serde_json::from_str(&line).map_err(|e| {
            io::Error::new(io::ErrorKind::InvalidData, format!("line {}: {e}", i + 1))
        })?);
    }
    Ok(events)
}
