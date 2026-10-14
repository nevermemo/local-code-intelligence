//! Handles (spine brief 1.5, 1.6): a stopped or budget-limited traversal
//! leaves a [`Snapshot`] of its state, a [`HandleStore`] keeps it under a
//! content hash (the handle), and [`Traversal::resumed`] continues from it.
//!
//! A snapshot holds everything the engine needs to go on as if it had not
//! paused: the states, the queue (priorities and discovery numbers), the
//! counters, the cleared set, map weights and switches, the pop count, and
//! the events so far. The handle is the hash of the snapshot's JSON, so equal
//! runs give equal handles and a stored file can be checked on load. The
//! wall-clock value of a call (`elapsed_ms`) is not kept: a resumed call
//! measures its own.
//!
//! What a resume does not carry: the edge source, the scorer, the price and
//! the handler. The caller passes them again, and may change them (a changed
//! scorer is noted in the `resume` event). The budget is absolute for the
//! whole traversal (`pops`, `est_tokens` and `external_calls` count from the
//! first call), with a fresh deadline.

use super::{
    Run, State, Totals, Traversal, TraversalBudget, frontier::Frontier, trace::value_hash,
};
use crate::{MapId, edge::WitnessClass, traverse::source::EdgeSource};
use lci_core::node::NodeId;
use serde_json::{Map as JsonMap, Value, json};
use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet},
    fmt, fs, io,
    path::{Path, PathBuf},
    sync::{Mutex, PoisonError},
};

const VERSION: u32 = 1;
const WITNESSES: [WitnessClass; 7] = [
    WitnessClass::Compiler,
    WitnessClass::Lsp,
    WitnessClass::Exact,
    WitnessClass::Text,
    WitnessClass::Vector,
    WitnessClass::Heuristic,
    WitnessClass::Agent,
];
/// The most distinct map ids a process interns while reading handles.
const MAX_INTERNED: usize = 256;
const MAX_MAP_ID: usize = 64;

#[derive(Debug)]
pub enum HandleError {
    /// Not a handle this store could have issued.
    BadHandle(String),
    Unknown(String),
    Io(io::Error),
    /// The stored snapshot does not parse, is inconsistent, or does not
    /// hash to its handle.
    Corrupt(String),
}

impl fmt::Display for HandleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadHandle(h) => write!(f, "not a handle: {h:?}"),
            Self::Unknown(h) => write!(f, "unknown handle {h}"),
            Self::Io(e) => write!(f, "handle store: {e}"),
            Self::Corrupt(why) => write!(f, "corrupt handle: {why}"),
        }
    }
}

impl std::error::Error for HandleError {}

impl From<io::Error> for HandleError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// The saved state of a paused traversal.
#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub question: String,
    pub targets: Vec<NodeId>,
    /// Label of the scorer the first call used.
    pub scorer: String,
    pub states: Vec<State>,
    /// `(state, priority, discovery number)` in discovery order.
    pub frontier: Vec<(usize, f64, u64)>,
    pub next_seq: u64,
    pub in_seen: Vec<(NodeId, u32)>,
    pub expanded: Vec<usize>,
    pub popped: Vec<usize>,
    pub cleared: Vec<NodeId>,
    pub weights: Vec<(String, f64)>,
    /// Map switches applied so far, in order.
    pub switches: Vec<(String, bool)>,
    /// Counters; `elapsed_ms` is always 0 here.
    pub totals: Totals,
    pub last_stop_at: Option<u32>,
    /// The trace so far, ending with the paused call's `end` event.
    pub events: Vec<Value>,
}

impl Snapshot {
    /// The handle: `h` and the first 16 hex digits of the hash of the
    /// snapshot's JSON.
    pub fn handle(&self) -> String {
        format!("h{}", value_hash(&self.to_json()))
    }

    /// Pops done so far: where a resume continues.
    pub fn pops(&self) -> u32 {
        self.totals.pops
    }

    pub fn to_json(&self) -> Value {
        json!({
            "version": VERSION,
            "question": self.question,
            "targets": self.targets.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "scorer": self.scorer,
            "states": self.states.iter().map(|s| json!({
                "node": s.node.to_string(),
                "parent": s.parent,
                "depth": s.depth,
                "via": s.via,
                "witness": s.witness.map(WitnessClass::as_str),
                "potential": s.potential,
                "fanout": s.fanout,
            })).collect::<Vec<_>>(),
            "frontier": self.frontier.iter()
                .map(|&(state, priority, seq)| json!([state, priority, seq]))
                .collect::<Vec<_>>(),
            "next_seq": self.next_seq,
            "in_seen": self.in_seen.iter()
                .map(|(n, c)| json!([n.to_string(), c])).collect::<Vec<_>>(),
            "expanded": self.expanded,
            "popped": self.popped,
            "cleared": self.cleared.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "weights": self.weights.iter().map(|(m, w)| json!([m, w])).collect::<Vec<_>>(),
            "switches": self.switches.iter().map(|(m, on)| json!([m, on])).collect::<Vec<_>>(),
            "totals": {
                "pops": self.totals.pops,
                "hops": self.totals.hops,
                "cross_map_hops": self.totals.cross_map_hops,
                "tokens": self.totals.tokens,
                "external_calls": self.totals.external_calls,
                "stops": self.totals.stops,
                "steers": self.totals.steers,
                "per_map": self.totals.per_map,
            },
            "last_stop_at": self.last_stop_at,
            "events": self.events,
        })
    }

    /// Reads and checks a snapshot: every index in range, every id valid.
    pub fn from_json(value: &Value) -> Result<Self, String> {
        let obj = value.as_object().ok_or("snapshot is not an object")?;
        if get(obj, "version")?.as_u64() != Some(u64::from(VERSION)) {
            return Err("unsupported snapshot version".into());
        }
        let states = array(obj, "states")?
            .iter()
            .map(|s| {
                let s = s.as_object().ok_or("state is not an object")?;
                Ok(State {
                    node: node_id(get(s, "node")?)?,
                    parent: opt_u64(get(s, "parent")?)?.map(|p| p as usize),
                    depth: u32_of(get(s, "depth")?)?,
                    via: match get(s, "via")? {
                        Value::Null => None,
                        v => Some(intern(v.as_str().ok_or("via is not a string")?)?),
                    },
                    witness: match get(s, "witness")? {
                        Value::Null => None,
                        v => Some(witness_of(v.as_str().ok_or("witness is not a string")?)?),
                    },
                    potential: f64_of(get(s, "potential")?)?,
                    fanout: u32_of(get(s, "fanout")?)?,
                })
            })
            .collect::<Result<Vec<State>, String>>()?;
        let n = states.len();
        let in_range = |i: usize, what: &str| {
            if i < n {
                Ok(i)
            } else {
                Err(format!("{what} index {i} out of range"))
            }
        };
        let mut seen_nodes = HashSet::new();
        for (i, s) in states.iter().enumerate() {
            if !seen_nodes.insert(&s.node) {
                return Err(format!("node {} has two states", s.node));
            }
            if let Some(p) = s.parent
                && p >= i
            {
                return Err(format!("state {i} has parent {p}"));
            }
        }
        let frontier = array(obj, "frontier")?
            .iter()
            .map(|item| {
                let item = item.as_array().filter(|a| a.len() == 3);
                let item = item.ok_or("frontier item is not [state, priority, seq]")?;
                Ok((
                    in_range(usize_of(&item[0])?, "frontier")?,
                    f64_of(&item[1])?,
                    u64_of(&item[2])?,
                ))
            })
            .collect::<Result<Vec<_>, String>>()?;
        let next_seq = u64_of(get(obj, "next_seq")?)?;
        if frontier.iter().any(|&(_, _, seq)| seq >= next_seq) {
            return Err("frontier discovery number past next_seq".into());
        }
        let indexes = |key: &str| {
            array(obj, key)?
                .iter()
                .map(|v| in_range(usize_of(v)?, key))
                .collect::<Result<Vec<usize>, String>>()
        };
        let pairs = |key: &str| -> Result<Vec<(Value, Value)>, String> {
            array(obj, key)?
                .iter()
                .map(|v| match v.as_array().map(Vec::as_slice) {
                    Some([a, b]) => Ok((a.clone(), b.clone())),
                    _ => Err(format!("{key} item is not a pair")),
                })
                .collect()
        };
        let totals = get(obj, "totals")?
            .as_object()
            .ok_or("totals is not an object")?;
        let per_map = get(totals, "per_map")?
            .as_object()
            .ok_or("per_map is not an object")?
            .iter()
            .map(|(k, v)| Ok((intern(k)?, u32_of(v)?)))
            .collect::<Result<BTreeMap<MapId, u32>, String>>()?;
        Ok(Self {
            question: string_of(get(obj, "question")?)?,
            targets: array(obj, "targets")?
                .iter()
                .map(node_id)
                .collect::<Result<_, _>>()?,
            scorer: string_of(get(obj, "scorer")?)?,
            states,
            frontier,
            next_seq,
            in_seen: pairs("in_seen")?
                .iter()
                .map(|(a, b)| Ok((node_id(a)?, u32_of(b)?)))
                .collect::<Result<_, String>>()?,
            expanded: indexes("expanded")?,
            popped: indexes("popped")?,
            cleared: array(obj, "cleared")?
                .iter()
                .map(node_id)
                .collect::<Result<_, _>>()?,
            weights: pairs("weights")?
                .iter()
                .map(|(a, b)| Ok((string_of(a)?, f64_of(b)?)))
                .collect::<Result<_, String>>()?,
            switches: pairs("switches")?
                .iter()
                .map(|(a, b)| Ok((string_of(a)?, b.as_bool().ok_or("switch is not a boolean")?)))
                .collect::<Result<_, String>>()?,
            totals: Totals {
                pops: u32_of(get(totals, "pops")?)?,
                hops: u32_of(get(totals, "hops")?)?,
                cross_map_hops: u32_of(get(totals, "cross_map_hops")?)?,
                tokens: u64_of(get(totals, "tokens")?)?,
                external_calls: u32_of(get(totals, "external_calls")?)?,
                stops: u32_of(get(totals, "stops")?)?,
                steers: u32_of(get(totals, "steers")?)?,
                per_map,
                elapsed_ms: 0.0,
            },
            last_stop_at: opt_u64(get(obj, "last_stop_at")?)?.map(|p| p as u32),
            events: array(obj, "events")?.clone(),
        })
    }
}

type Obj = JsonMap<String, Value>;

fn get<'a>(obj: &'a Obj, key: &str) -> Result<&'a Value, String> {
    obj.get(key).ok_or_else(|| format!("missing {key}"))
}

fn array<'a>(obj: &'a Obj, key: &str) -> Result<&'a Vec<Value>, String> {
    get(obj, key)?
        .as_array()
        .ok_or_else(|| format!("{key} is not an array"))
}

fn u64_of(v: &Value) -> Result<u64, String> {
    v.as_u64().ok_or_else(|| format!("{v} is not an integer"))
}

fn usize_of(v: &Value) -> Result<usize, String> {
    u64_of(v).map(|n| n as usize)
}

fn u32_of(v: &Value) -> Result<u32, String> {
    u32::try_from(u64_of(v)?).map_err(|_| format!("{v} does not fit u32"))
}

fn opt_u64(v: &Value) -> Result<Option<u64>, String> {
    if v.is_null() {
        Ok(None)
    } else {
        u64_of(v).map(Some)
    }
}

fn f64_of(v: &Value) -> Result<f64, String> {
    v.as_f64().ok_or_else(|| format!("{v} is not a number"))
}

fn string_of(v: &Value) -> Result<String, String> {
    v.as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("{v} is not a string"))
}

fn node_id(v: &Value) -> Result<NodeId, String> {
    v.as_str()
        .ok_or_else(|| format!("{v} is not a string"))?
        .parse()
        .map_err(|e| format!("bad node id {v}: {e}"))
}

fn witness_of(name: &str) -> Result<WitnessClass, String> {
    WITNESSES
        .into_iter()
        .find(|w| w.as_str() == name)
        .ok_or_else(|| format!("unknown witness class {name:?}"))
}

/// Map ids are `&'static str`; a handle names them by text, so the text is
/// kept once per process (bounded: a handle cannot grow the table past
/// [`MAX_INTERNED`]).
fn intern(id: &str) -> Result<MapId, String> {
    static TABLE: Mutex<BTreeSet<&'static str>> = Mutex::new(BTreeSet::new());
    if id.is_empty() || id.len() > MAX_MAP_ID {
        return Err(format!("bad map id {id:?}"));
    }
    let mut table = TABLE.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(known) = table.get(id) {
        return Ok(known);
    }
    if table.len() >= MAX_INTERNED {
        return Err("too many distinct map ids".into());
    }
    let leaked: &'static str = Box::leak(id.to_string().into_boxed_str());
    table.insert(leaked);
    Ok(leaked)
}

impl Run<'_> {
    /// The state to save. Call after the `end` event is pushed.
    pub(super) fn snapshot(&self) -> Snapshot {
        let (frontier, next_seq) = self.frontier.parts();
        let sorted = |set: &HashSet<usize>| {
            let mut v: Vec<usize> = set.iter().copied().collect();
            v.sort_unstable();
            v
        };
        let mut weights: Vec<(String, f64)> =
            self.weights.iter().map(|(m, w)| (m.clone(), *w)).collect();
        weights.sort_by(|a, b| a.0.cmp(&b.0));
        let in_seen: BTreeMap<&NodeId, u32> = self.in_seen.iter().map(|(n, c)| (n, *c)).collect();
        Snapshot {
            question: self.t.question.clone(),
            targets: self.t.targets.clone(),
            scorer: self.t.scorer.label().to_string(),
            states: self.states.clone(),
            frontier,
            next_seq,
            in_seen: in_seen.into_iter().map(|(n, c)| (n.clone(), c)).collect(),
            expanded: sorted(&self.expanded),
            popped: self.popped.clone(),
            cleared: self.cleared.iter().cloned().collect(),
            weights,
            switches: self.switches.clone(),
            totals: Totals {
                elapsed_ms: 0.0,
                ..self.totals.clone()
            },
            last_stop_at: self.last_stop_at,
            events: self.events.clone(),
        }
    }

    /// Loads a snapshot into a fresh run and notes the resume in the trace.
    pub(super) fn restore(&mut self, snapshot: Snapshot) {
        let handle = snapshot.handle();
        let scorer_changed = snapshot.scorer != self.t.scorer.label();
        let Snapshot {
            question,
            states,
            frontier,
            next_seq,
            in_seen,
            expanded,
            popped,
            cleared,
            weights,
            switches,
            totals,
            last_stop_at,
            events,
            ..
        } = snapshot;
        self.state_of = states
            .iter()
            .enumerate()
            .map(|(i, s)| (s.node.clone(), i))
            .collect();
        self.states = states;
        self.frontier = Frontier::from_parts(&frontier, next_seq);
        self.in_seen = in_seen.into_iter().collect::<HashMap<_, _>>();
        self.expanded = expanded.into_iter().collect();
        self.popped = popped;
        self.cleared = cleared.into_iter().collect();
        self.weights = weights.into_iter().collect();
        for (map, on) in &switches {
            self.t.source.set_enabled(map, *on);
        }
        self.switches = switches;
        self.totals = totals;
        self.last_stop_at = last_stop_at;
        self.events = events;
        debug_assert_eq!(question, self.t.question);
        self.events.push(json!({
            "ev": "resume",
            "handle": handle,
            "pops": self.totals.pops,
            "budget": {
                "pops": self.t.budget.pops,
                "max_depth": self.t.budget.max_depth,
                "est_tokens": self.t.budget.est_tokens,
                "external_calls": self.t.budget.external_calls,
            },
            "scorer": self.t.scorer.label(),
            "scorer_changed": scorer_changed,
            "stops": {
                "every_pops": self.t.stops.every_pops,
                "max_stops": self.t.stops.max_stops,
            },
        }));
    }
}

impl<'s> Traversal<'s> {
    /// Continues the traversal a [`Snapshot`] paused. The question and
    /// targets come from the snapshot; see the module docs for the budget.
    pub fn resumed(
        source: impl EdgeSource + 's,
        snapshot: Snapshot,
        budget: TraversalBudget,
    ) -> Self {
        let mut traversal = Self::new(source, Vec::new(), budget)
            .question(snapshot.question.clone())
            .targets(snapshot.targets.clone());
        traversal.resume = Some(snapshot);
        traversal
    }
}

/// Snapshots by handle: an in-process map, and optionally a directory with
/// one `<handle>.json` per snapshot (scratch data; never in a repository).
pub struct HandleStore {
    dir: Option<PathBuf>,
    mem: Mutex<HashMap<String, Snapshot>>,
}

impl HandleStore {
    pub fn memory() -> Self {
        Self {
            dir: None,
            mem: Mutex::new(HashMap::new()),
        }
    }

    /// Also writes every snapshot under `dir` (created on first `put`), and
    /// reads from it what the process map does not hold.
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: Some(dir.into()),
            mem: Mutex::new(HashMap::new()),
        }
    }

    fn file(&self, handle: &str) -> Option<PathBuf> {
        self.dir
            .as_ref()
            .map(|dir| dir.join(format!("{handle}.json")))
    }

    /// Keeps `snapshot` and returns its handle. The same snapshot always has
    /// the same handle, so a second `put` changes nothing.
    pub fn put(&self, snapshot: &Snapshot) -> Result<String, HandleError> {
        let handle = snapshot.handle();
        if let Some(path) = self.file(&handle)
            && !path.exists()
        {
            write_file(&path, snapshot.to_json().to_string().as_bytes())?;
        }
        self.mem
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(handle.clone(), snapshot.clone());
        Ok(handle)
    }

    /// The snapshot for `handle`: the process map first, then the file,
    /// which must hash to its name.
    pub fn get(&self, handle: &str) -> Result<Snapshot, HandleError> {
        let well_formed = handle.len() == 17
            && handle.starts_with('h')
            && handle[1..]
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
        if !well_formed {
            return Err(HandleError::BadHandle(handle.to_string()));
        }
        if let Some(found) = self
            .mem
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(handle)
        {
            return Ok(found.clone());
        }
        let path = self
            .file(handle)
            .filter(|p| p.exists())
            .ok_or_else(|| HandleError::Unknown(handle.to_string()))?;
        let text = fs::read_to_string(path)?;
        let value: Value =
            serde_json::from_str(&text).map_err(|e| HandleError::Corrupt(e.to_string()))?;
        let snapshot = Snapshot::from_json(&value).map_err(HandleError::Corrupt)?;
        if snapshot.handle() != handle {
            return Err(HandleError::Corrupt(
                "content does not match its handle".into(),
            ));
        }
        self.mem
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(handle.to_string(), snapshot.clone());
        Ok(snapshot)
    }
}

/// Writes through a temporary name, so a reader never sees half a file.
fn write_file(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}
