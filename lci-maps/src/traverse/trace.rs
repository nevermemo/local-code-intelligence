//! The trace log (spine brief 1.6): the engine's JSON events as JSONL, one
//! event per line, plus the hashes the events cite.
//!
//! The trace record is [`TraversalResult::events`]; nothing here defines a
//! second format. The events carry no wall-clock value, so two runs with the
//! same inputs write byte-identical files. A `stop` event names the hash of
//! the [`Checkpoint`] the handler saw, and the `steer` that follows cites the
//! same hash as `inputs_hash`, so a decision can be tied to its inputs
//! without storing them. Hashes are the first 16 hex digits of SHA-256.
//!
//! Events of the ladder (slice S4): a `stop` event lists the `reason` and all
//! `triggers` that held; each rung consulted writes a `steer` event with its
//! `rung`, `handler`, `confidence`, proposed `actions` and an `outcome`
//! (`applied` or `uncertain`); a ladder with no confident rung ends with a
//! `steer` of rung 3 and outcome `returned`. A call that ends with work left
//! writes a `return` event (`escalated`, `handler_stop` or `budget`) with the
//! hash of the checkpoint it hands back; the next call's rung-3 `steer`
//! event cites that hash as `inputs_hash`.

use super::{
    TraversalResult,
    handler::Checkpoint,
    stop::{Card, Disagreement},
};
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

/// A card as the checkpoint JSON shows it: the fields and the one-line text.
pub fn card_json(card: &Card) -> Value {
    json!({
        "node": card.node.to_string(),
        "via": card.via,
        "witness": card.witness.map(|w| w.as_str()),
        "potential": card.potential,
        "depth": card.depth,
        "text": card.text(),
    })
}

/// A checkpoint as the hash and the trace see it. The handle is not in it:
/// the handle hashes the snapshot, which cites this hash.
pub fn checkpoint_json(checkpoint: &Checkpoint) -> Value {
    let regions = &checkpoint.regions;
    json!({
        "reason": checkpoint.reason.as_str(),
        "question": checkpoint.question,
        "seeds": checkpoint.seeds.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "pops": checkpoint.pops,
        "stop": checkpoint.stop,
        "budget_left_pct": checkpoint.budget_left_pct,
        "pops_left": checkpoint.pops_left,
        "tokens_left": checkpoint.tokens_left,
        "calls_left": checkpoint.calls_left,
        "visited": checkpoint.visited,
        "cleared": checkpoint.cleared,
        "answer": checkpoint.answer.iter().map(card_json).collect::<Vec<_>>(),
        "visited_cards": checkpoint.visited_cards.iter().map(card_json).collect::<Vec<_>>(),
        "frontier": checkpoint.frontier.iter().map(|item| json!({
            "node": item.node.to_string(),
            "via": item.via,
            "witness": item.witness.map(|w| w.as_str()),
            "score": item.score,
            "potential": item.potential,
            "depth": item.depth,
            "fanout": item.fanout,
            "seen_before": item.seen_before,
            "est_cost": item.est_cost,
        })).collect::<Vec<_>>(),
        "maps": checkpoint.maps.iter().map(|(id, stat)| json!({
            "map": id,
            "calls": stat.calls,
            "failures": stat.failures,
            "edges": stat.edges,
            "tokens": stat.tokens,
        })).collect::<Vec<_>>(),
        "regions": {
            "explored": regions.explored,
            "seen": regions.seen,
            "unknown": regions.unknown,
            "cleared": regions.cleared,
            "rows": regions.rows.iter().map(|row| json!({
                "path": row.path,
                "explored": row.explored,
                "seen": row.seen,
                "cleared": row.cleared,
            })).collect::<Vec<_>>(),
        },
        "disagreements": checkpoint.disagreements.iter().map(disagreement_json)
            .collect::<Vec<_>>(),
        "disagreements_total": checkpoint.disagreements_total,
        "likely_complete": checkpoint.likely_complete.as_ref().map(|lc| json!({
            "maps": lc.maps,
            "n1": lc.n1,
            "n2": lc.n2,
            "refound": lc.refound,
            "found": lc.found,
            "estimate": lc.estimate,
            "fraction": lc.fraction,
        })),
    })
}

pub fn disagreement_json(d: &Disagreement) -> Value {
    json!({
        "pop": d.pop,
        "src": d.src.to_string(),
        "node": d.node.to_string(),
        "offered_by": d.offered_by,
        "silent": d.silent,
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

/// One map of the plan: what the `start` event records of it.
#[derive(Debug, Clone, PartialEq)]
pub struct PlanMap {
    pub id: String,
    pub version: u32,
    pub weight: f64,
    pub enabled: bool,
}

impl PlanMap {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "version": self.version,
            "weight": self.weight,
            "enabled": self.enabled,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str()?.to_owned(),
            version: value.get("version").and_then(Value::as_u64).unwrap_or(0) as u32,
            weight: value.get("weight").and_then(Value::as_f64).unwrap_or(1.0),
            enabled: value
                .get("enabled")
                .and_then(Value::as_bool)
                .unwrap_or(true),
        })
    }
}

/// The steering policy of the plan, besides the stop policy the engine
/// already knows: the rungs, the confidence a rung needs and the rule count.
/// All empty (no rungs, no `tau`, no rules) for a walk without a handler.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanPolicy {
    /// The rungs in order, ending with `agent`; empty without a handler.
    pub ladder: Vec<String>,
    pub tau: Option<f32>,
    pub rules: u32,
}

/// What the caller says about a run that the engine cannot know: the maps
/// registered (ids, versions, weights), the steering policy and where the
/// start nodes came from. Set with `Traversal::plan`; the `start` event then
/// carries `maps`, `policy` (the stop policy as `stops` plus the fields of
/// [`PlanPolicy`]), and, when given, `seed_source` and `seed_hits`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StartPlan {
    pub maps: Vec<PlanMap>,
    pub policy: PlanPolicy,
    /// `hybrid`, `name_tokens` or `given`.
    pub seed_source: Option<String>,
    /// Per start node: the retrievers that found it and their scores (the
    /// caller's JSON objects).
    pub seed_hits: Vec<Value>,
}

impl StartPlan {
    /// The fields the engine adds to the `start` event, given the stop
    /// policy as the engine writes it.
    pub fn fields(&self, stops: Value) -> Vec<(&'static str, Value)> {
        let mut fields = vec![
            (
                "maps",
                Value::Array(self.maps.iter().map(PlanMap::to_json).collect()),
            ),
            (
                "policy",
                json!({
                    "stops": stops,
                    "ladder": self.policy.ladder,
                    // f32 to six places, so 0.8 is written 0.8.
                    "tau": self.policy.tau.map(|t| (f64::from(t) * 1e6).round() / 1e6),
                    "rules": self.policy.rules,
                }),
            ),
        ];
        if let Some(source) = &self.seed_source {
            fields.push(("seed_source", json!(source)));
        }
        if !self.seed_hits.is_empty() {
            fields.push(("seed_hits", Value::Array(self.seed_hits.clone())));
        }
        fields
    }
}

/// A `start` event as a reader sees it. Traces written before `maps` and
/// `policy` existed have neither: they read as an empty list and `None`.
#[derive(Debug, Clone, PartialEq)]
pub struct StartEvent {
    pub question: String,
    pub seeds: Vec<String>,
    pub budget: Value,
    pub scorer: Option<String>,
    pub stops: Value,
    pub maps: Vec<PlanMap>,
    pub policy: Option<StartPolicy>,
    pub seed_source: Option<String>,
    pub seed_hits: Vec<Value>,
}

/// The `policy` of a `start` event.
#[derive(Debug, Clone, PartialEq)]
pub struct StartPolicy {
    pub stops: Value,
    pub ladder: Vec<String>,
    pub tau: Option<f32>,
    pub rules: u32,
}

impl StartEvent {
    /// Reads a `start` event; `None` when `event` is not one.
    pub fn from_json(event: &Value) -> Option<Self> {
        if event.get("ev")?.as_str()? != "start" {
            return None;
        }
        let strings = |value: Option<&Value>| -> Vec<String> {
            value
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|item| item.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        Some(Self {
            question: event
                .get("question")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            seeds: strings(event.get("seeds")),
            budget: event.get("budget").cloned().unwrap_or(Value::Null),
            scorer: event
                .get("scorer")
                .and_then(Value::as_str)
                .map(str::to_owned),
            stops: event.get("stops").cloned().unwrap_or(Value::Null),
            maps: event
                .get("maps")
                .and_then(Value::as_array)
                .map(|maps| maps.iter().filter_map(PlanMap::from_json).collect())
                .unwrap_or_default(),
            policy: event
                .get("policy")
                .filter(|p| p.is_object())
                .map(|policy| StartPolicy {
                    stops: policy.get("stops").cloned().unwrap_or(Value::Null),
                    ladder: strings(policy.get("ladder")),
                    tau: policy.get("tau").and_then(Value::as_f64).map(|t| t as f32),
                    rules: policy.get("rules").and_then(Value::as_u64).unwrap_or(0) as u32,
                }),
            seed_source: event
                .get("seed_source")
                .and_then(Value::as_str)
                .map(str::to_owned),
            seed_hits: event
                .get("seed_hits")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default(),
        })
    }
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
