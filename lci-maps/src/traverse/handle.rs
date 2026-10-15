//! Handles (spine brief 1.5, 1.6): a stopped or budget-limited traversal
//! leaves a [`Snapshot`] of its state, a [`HandleStore`] keeps it under a
//! content hash (the handle), and [`Traversal::resumed`] continues from it.
//!
//! A snapshot holds what the engine needs to go on as if it had not paused:
//! the states, the queue (priorities and discovery numbers), the states held
//! back at the depth limit, the counters, the cleared set, map weights and
//! switches, the pop count, what a cut pop already paid for its calls, the
//! `alt_path` pairs already traced, and the identity of the workspace and
//! index it walks. It does not hold the event log: each call writes its own
//! trace (a resumed call starts with its `resume` event, which names the
//! handle), so a handle stays the size of the walk, not of its history. The
//! handle is the hash of the snapshot's JSON, so equal runs give equal
//! handles and a stored file can be checked on load. The wall-clock value of
//! a call (`elapsed_ms`) is not kept: a resumed call measures its own.
//!
//! A handle is bound to the workspace and index it was made on
//! ([`Snapshot::identity`]); a caller that knows them refuses a handle that
//! names others ([`Snapshot::check_identity`]).
//!
//! What a resume does not carry: the edge source, the scorer, the price and
//! the handler. The caller passes them again, and may change them (a changed
//! scorer is noted in the `resume` event). The budget is absolute for the
//! whole traversal (`pops`, `est_tokens` and `external_calls` count from the
//! first call), with a fresh deadline; what the outer agent raised stays with
//! the traversal and is added to the caps a call passes (see [`super::steer`]).
//! The depth limit is the call's own: a resume with a lower limit does not
//! expand what the first call queued below it, and one with a higher limit
//! queues the states the first call held back. A resumed call may carry the
//! agent's [`Steer`](super::steer::Steer).

use super::{
    Fusion, Run, State, Totals, Traversal, TraversalBudget,
    frontier::Frontier,
    priors::HopPriors,
    rank_json,
    stop::{Disagreement, MapStat, Tally},
    stops_json,
    trace::{disagreement_json, value_hash},
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

const VERSION: u32 = 7;
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
    /// The handle was made for another workspace or index than the one the
    /// caller is walking (see [`Snapshot::check_identity`]).
    WrongIdentity {
        handle: String,
        found: String,
        expected: String,
    },
    /// The handle was made with other hop priors than the caller gives (see
    /// [`Snapshot::check_priors`]); each side is `question type @ table hash`,
    /// or `none` for the guessed table.
    WrongPriors {
        handle: String,
        found: String,
        expected: String,
    },
}

impl fmt::Display for HandleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadHandle(h) => write!(f, "not a handle: {h:?}"),
            Self::Unknown(h) => write!(f, "unknown handle {h}"),
            Self::Io(e) => write!(f, "handle store: {e}"),
            Self::Corrupt(why) => write!(f, "corrupt handle: {why}"),
            Self::WrongIdentity {
                handle,
                found,
                expected,
            } => {
                let found = if found.is_empty() {
                    "none"
                } else {
                    found.as_str()
                };
                write!(
                    f,
                    "handle {handle} belongs to another workspace or index \
                     (it was made for {found}, this call walks {expected}); \
                     start a new traversal instead of resuming it"
                )
            }
            Self::WrongPriors {
                handle,
                found,
                expected,
            } => write!(
                f,
                "handle {handle} was made with the hop priors {found}, this call has \
                 {expected}; give the same table and question type, or start a new traversal"
            ),
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
    /// What stops and checkpoints are built from, and what the agent raised.
    pub tally: Tally,
    /// The hash of the checkpoint the paused call returned, which a steer
    /// answers.
    pub returned: Option<String>,
    /// `(state reached, state that offered the edge)` pairs already traced
    /// as `alt_path`, so a resume does not trace them again.
    pub alts: Vec<(usize, usize)>,
    /// States found at or beyond the depth limit, which are not queued.
    pub deferred: Vec<usize>,
    /// Per cut pop `(state, tokens, external calls)` its calls were already
    /// charged, so the retry owes only the difference.
    pub paid: Vec<(usize, u64, u32)>,
    /// What the traversal is bound to: the workspace and the index it walks,
    /// as the caller fingerprinted them (empty when the caller gave none).
    pub identity: String,
    /// The corroboration boost the first call ranked with; a resume ranks with
    /// it too, so the order of results does not change between calls.
    pub boost: f64,
    /// The fusion the first call ranked with; a resume keeps it. Written to
    /// the handle only under `rrf` (a handle without it is `boost`).
    pub fusion: Fusion,
    /// Whether the first call passed through connective nodes (their pops
    /// were free); a resume keeps it.
    pub pass_through: bool,
    /// Whether the score bound could end the walk; a resume keeps it.
    pub score_bound: bool,
    /// The question type and content hash of the fitted hop priors the first
    /// call priced with (none: the guess). Written to the handle only when
    /// there are some.
    pub priors: Option<(String, String)>,
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
        let mut value = self.base_json();
        if let Some(object) = value.as_object_mut() {
            // Left out under `boost`, so those handles are what they were.
            if self.fusion == Fusion::Rrf {
                object.insert("fusion".into(), json!(self.fusion.as_str()));
            }
            if let Some((question_type, hash)) = &self.priors {
                object.insert(
                    "priors".into(),
                    json!({"question_type": question_type, "hash": hash}),
                );
            }
        }
        value
    }

    fn base_json(&self) -> Value {
        json!({
            "version": VERSION,
            "question": self.question,
            "targets": self.targets.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "scorer": self.scorer,
            "states": self.states.iter().map(state_json).collect::<Vec<_>>(),
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
                "pass_through_pops": self.totals.pass_through_pops,
                "stops": self.totals.stops,
                "steers": self.totals.steers,
            },
            "last_stop_at": self.last_stop_at,
            "tally": tally_json(&self.tally),
            "returned": self.returned,
            "alts": self.alts.iter().map(|&(a, b)| json!([a, b])).collect::<Vec<_>>(),
            "deferred": self.deferred,
            "paid": self.paid.iter()
                .map(|&(state, tokens, calls)| json!([state, tokens, calls]))
                .collect::<Vec<_>>(),
            "identity": self.identity,
            "boost": self.boost,
            "pass_through": self.pass_through,
            "score_bound": self.score_bound,
        })
    }

    /// What the traversal is bound to (see [`Snapshot::check_identity`]).
    pub fn identity(&self) -> &str {
        &self.identity
    }

    /// Refuses a snapshot made for another workspace or index than
    /// `expected`; a snapshot that names none is another identity than any
    /// that is given. An `expected` of none matches anything.
    pub fn check_identity(&self, expected: &str) -> Result<(), HandleError> {
        if expected.is_empty() || self.identity == expected {
            Ok(())
        } else {
            Err(HandleError::WrongIdentity {
                handle: self.handle(),
                found: self.identity.clone(),
                expected: expected.to_string(),
            })
        }
    }

    /// Refuses a snapshot made with other hop priors than `priors`: a
    /// different question type, a table whose content hash changed, or one
    /// side being the guess.
    pub fn check_priors(&self, priors: &HopPriors) -> Result<(), HandleError> {
        let current = priors
            .fingerprint()
            .map(|(question_type, hash)| (question_type.to_owned(), hash.to_owned()));
        if self.priors == current {
            return Ok(());
        }
        let show = |priors: &Option<(String, String)>| match priors {
            Some((question_type, hash)) => format!("{question_type} @ {hash}"),
            None => "none".to_owned(),
        };
        Err(HandleError::WrongPriors {
            handle: self.handle(),
            found: show(&self.priors),
            expected: show(&current),
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
                    // Handles kept before corroboration existed have neither.
                    maps: strings(s.get("maps"))?
                        .iter()
                        .map(|m| intern(m))
                        .collect::<Result<_, _>>()?,
                    dependent: u32_of(get(s, "dependent")?)? as usize,
                    witnesses: strings(s.get("witnesses"))?
                        .iter()
                        .map(|w| witness_of(w))
                        .collect::<Result<_, _>>()?,
                    // Only a walk under `rrf` fusion has them.
                    reach: match s.get("reach") {
                        None | Some(Value::Null) => BTreeMap::new(),
                        Some(v) => v
                            .as_object()
                            .ok_or("reach is not an object")?
                            .iter()
                            .map(|(m, p)| Ok((intern(m)?, f64_of(p)?)))
                            .collect::<Result<_, String>>()?,
                    },
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
        for s in &states {
            if !seen_nodes.insert(&s.node) {
                return Err(format!("node {} has two states", s.node));
            }
            if s.dependent > s.maps.len() {
                return Err(format!("node {} has more dependent maps than maps", s.node));
            }
            if let Some(p) = s.parent {
                in_range(p, "parent")?;
            }
        }
        // A relax can give a state a parent found after it, so the parents
        // are checked for a cycle rather than for order: every chain must
        // reach a root. 0 = not seen, 1 = on the chain being walked, 2 = ok.
        let mut mark = vec![0u8; n];
        for start in 0..n {
            let mut chain = Vec::new();
            let mut at = Some(start);
            while let Some(i) = at {
                match mark[i] {
                    2 => break,
                    1 => return Err(format!("state {i} is its own ancestor (parent cycle)")),
                    _ => {
                        mark[i] = 1;
                        chain.push(i);
                        at = states[i].parent;
                    }
                }
            }
            for i in chain {
                mark[i] = 2;
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
        let mut queued = HashSet::new();
        if let Some(&(state, _, _)) = frontier.iter().find(|(s, _, _)| !queued.insert(*s)) {
            return Err(format!("state {state} is queued twice"));
        }
        let indexes = |key: &str| {
            array(obj, key)?
                .iter()
                .map(|v| in_range(usize_of(v)?, key))
                .collect::<Result<Vec<usize>, String>>()
        };
        let expanded = indexes("expanded")?;
        if let Some(state) = expanded.iter().find(|s| queued.contains(*s)) {
            return Err(format!("state {state} is both queued and expanded"));
        }
        let deferred = indexes("deferred")?;
        let mut held = HashSet::new();
        for state in &deferred {
            if !held.insert(*state) {
                return Err(format!("state {state} is deferred twice"));
            }
            if queued.contains(state) || expanded.contains(state) {
                return Err(format!(
                    "state {state} is deferred and also queued or expanded"
                ));
            }
        }
        let paid = array(obj, "paid")?
            .iter()
            .map(|row| match row.as_array().map(Vec::as_slice) {
                Some([state, tokens, calls]) => Ok((
                    in_range(usize_of(state)?, "paid")?,
                    u64_of(tokens)?,
                    u32_of(calls)?,
                )),
                _ => Err("paid row is not [state, tokens, calls]".to_string()),
            })
            .collect::<Result<Vec<_>, String>>()?;
        let mut owing = HashSet::new();
        for &(state, _, _) in &paid {
            if !owing.insert(state) {
                return Err(format!("state {state} has two paid rows"));
            }
            if expanded.contains(&state) {
                return Err(format!("state {state} is expanded but owes a retry"));
            }
        }
        let alts = array(obj, "alts")?
            .iter()
            .map(|row| match row.as_array().map(Vec::as_slice) {
                Some([a, b]) => Ok((
                    in_range(usize_of(a)?, "alts")?,
                    in_range(usize_of(b)?, "alts")?,
                )),
                _ => Err("alts row is not a pair".to_string()),
            })
            .collect::<Result<Vec<_>, String>>()?;
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
            expanded,
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
                pass_through_pops: u32_of(get(totals, "pass_through_pops")?)?,
                stops: u32_of(get(totals, "stops")?)?,
                steers: u32_of(get(totals, "steers")?)?,
                elapsed_ms: 0.0,
            },
            last_stop_at: opt_u64(get(obj, "last_stop_at")?)?.map(|p| p as u32),
            tally: tally_from_json(get(obj, "tally")?)?,
            returned: match get(obj, "returned")? {
                Value::Null => None,
                v => Some(string_of(v)?),
            },
            alts,
            deferred,
            paid,
            identity: string_of(get(obj, "identity")?)?,
            boost: f64_of(get(obj, "boost")?)?,
            fusion: match obj.get("fusion") {
                None | Some(Value::Null) => Fusion::Boost,
                Some(v) => string_of(v)?.parse::<Fusion>()?,
            },
            pass_through: get(obj, "pass_through")?
                .as_bool()
                .ok_or("pass_through is not a boolean")?,
            score_bound: get(obj, "score_bound")?
                .as_bool()
                .ok_or("score_bound is not a boolean")?,
            priors: match obj.get("priors") {
                None | Some(Value::Null) => None,
                Some(v) => {
                    let v = v.as_object().ok_or("priors is not an object")?;
                    Some((
                        string_of(get(v, "question_type")?)?,
                        string_of(get(v, "hash")?)?,
                    ))
                }
            },
        })
    }
}

fn state_json(s: &State) -> Value {
    let mut value = json!({
        "node": s.node.to_string(),
        "parent": s.parent,
        "depth": s.depth,
        "via": s.via,
        "witness": s.witness.map(WitnessClass::as_str),
        "potential": s.potential,
        "fanout": s.fanout,
        "maps": s.maps,
        "dependent": s.dependent,
        "witnesses": s.witnesses.iter().map(|w| w.as_str()).collect::<Vec<_>>(),
    });
    // Only a walk under `rrf` fusion keeps the best potential per map.
    if let (false, Some(object)) = (s.reach.is_empty(), value.as_object_mut()) {
        object.insert(
            "reach".into(),
            Value::Object(
                s.reach
                    .iter()
                    .map(|(m, p)| ((*m).to_owned(), json!(p)))
                    .collect(),
            ),
        );
    }
    value
}

fn tally_json(tally: &Tally) -> Value {
    json!({
        "found": tally.found.iter().map(|(map, nodes)| json!([
            map, nodes.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ])).collect::<Vec<_>>(),
        "maps": tally.maps.iter().map(|(map, s)| json!([
            map, s.calls, s.failures, s.edges, s.tokens,
        ])).collect::<Vec<_>>(),
        "kind_cost": tally.kind_cost.iter()
            .map(|(kind, (n, tokens))| json!([kind, n, tokens])).collect::<Vec<_>>(),
        "disagreements": tally.disagreements.iter().map(disagreement_json).collect::<Vec<_>>(),
        "last_fresh_pop": tally.last_fresh_pop,
        "complete_fired": tally.complete_fired,
        "raised_pops": tally.raised_pops,
        "raised_tokens": tally.raised_tokens,
    })
}

fn tally_from_json(value: &Value) -> Result<Tally, String> {
    let obj = value.as_object().ok_or("tally is not an object")?;
    let rows = |key: &str, width: usize| -> Result<Vec<Vec<Value>>, String> {
        array(obj, key)?
            .iter()
            .map(|row| match row.as_array() {
                Some(cells) if cells.len() == width => Ok(cells.clone()),
                _ => Err(format!("{key} row is not {width} cells")),
            })
            .collect()
    };
    let map_ids = |v: &Value| -> Result<Vec<MapId>, String> {
        v.as_array()
            .ok_or("map list is not an array")?
            .iter()
            .map(|id| intern(id.as_str().ok_or("map id is not a string")?))
            .collect()
    };
    let mut tally = Tally::default();
    for row in rows("found", 2)? {
        let nodes = row[1].as_array().ok_or("found nodes is not an array")?;
        tally.found.insert(
            intern(row[0].as_str().ok_or("map id is not a string")?)?,
            nodes.iter().map(node_id).collect::<Result<_, _>>()?,
        );
    }
    for row in rows("maps", 5)? {
        tally.maps.insert(
            intern(row[0].as_str().ok_or("map id is not a string")?)?,
            MapStat {
                calls: u32_of(&row[1])?,
                failures: u32_of(&row[2])?,
                edges: u32_of(&row[3])?,
                tokens: u64_of(&row[4])?,
            },
        );
    }
    for row in rows("kind_cost", 3)? {
        tally
            .kind_cost
            .insert(string_of(&row[0])?, (u32_of(&row[1])?, u64_of(&row[2])?));
    }
    for d in array(obj, "disagreements")? {
        let d = d.as_object().ok_or("disagreement is not an object")?;
        tally.disagreements.push(Disagreement {
            pop: u32_of(get(d, "pop")?)?,
            src: node_id(get(d, "src")?)?,
            node: node_id(get(d, "node")?)?,
            offered_by: map_ids(get(d, "offered_by")?)?,
            silent: map_ids(get(d, "silent")?)?,
        });
    }
    tally.last_fresh_pop = u32_of(get(obj, "last_fresh_pop")?)?;
    tally.complete_fired = get(obj, "complete_fired")?
        .as_bool()
        .ok_or("complete_fired is not a boolean")?;
    tally.raised_pops = u32_of(get(obj, "raised_pops")?)?;
    tally.raised_tokens = u64_of(get(obj, "raised_tokens")?)?;
    Ok(tally)
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

/// An optional array of strings; absent is empty.
fn strings(value: Option<&Value>) -> Result<Vec<&str>, String> {
    match value {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(v) => v
            .as_array()
            .ok_or("expected an array of strings")?
            .iter()
            .map(|item| item.as_str().ok_or("expected a string".to_string()))
            .collect(),
    }
}

fn witness_of(name: &str) -> Result<WitnessClass, String> {
    WitnessClass::parse(name).ok_or_else(|| format!("unknown witness class {name:?}"))
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
            tally: self.tally.clone(),
            returned: self.returned.clone(),
            alts: {
                let mut alts: Vec<(usize, usize)> = self.alts.iter().copied().collect();
                alts.sort_unstable();
                alts
            },
            deferred: self.deferred.iter().copied().collect(),
            paid: self
                .paid
                .iter()
                .map(|(&state, &(tokens, calls))| (state, tokens, calls))
                .collect(),
            identity: self.t.identity.clone(),
            boost: self.t.boost,
            fusion: self.t.fusion,
            pass_through: self.t.pass_through,
            score_bound: self.t.score_bound,
            priors: self
                .t
                .priors
                .fingerprint()
                .map(|(question_type, hash)| (question_type.to_owned(), hash.to_owned())),
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
            tally,
            returned,
            alts,
            deferred,
            paid,
            identity,
            boost,
            fusion,
            pass_through,
            score_bound,
            ..
        } = snapshot;
        if self.t.identity.is_empty() {
            self.t.identity = identity;
        }
        self.t.boost = boost;
        self.t.fusion = fusion;
        self.t.pass_through = pass_through;
        self.t.score_bound = score_bound;
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
        self.tally = tally;
        self.returned = returned;
        self.alts = alts.into_iter().collect();
        self.paid = paid
            .into_iter()
            .map(|(state, tokens, calls)| (state, (tokens, calls)))
            .collect();
        // A cut pop that adopted something left children behind.
        self.partial = self
            .states
            .iter()
            .filter_map(|s| s.parent)
            .filter(|p| !self.expanded.contains(p))
            .collect();
        // What the first call held back at its depth limit is queued when
        // this call's limit lets it be expanded.
        let max_depth = self.t.budget.max_depth;
        let mut unblocked = 0;
        for state in deferred {
            let s = &self.states[state];
            if s.depth < max_depth && !self.cleared.contains(&s.node) {
                self.frontier.push(state, s.potential);
                unblocked += 1;
            } else {
                self.deferred.insert(state);
            }
        }
        debug_assert_eq!(question, self.t.question);
        self.events.clear();
        let mut resume = json!({
            "ev": "resume",
            "handle": handle,
            "pops": self.totals.pops,
            "unblocked": unblocked,
            "budget": {
                "pops": self.t.budget.pops,
                "max_depth": self.t.budget.max_depth,
                "est_tokens": self.t.budget.est_tokens,
                "external_calls": self.t.budget.external_calls,
                "raised_pops": self.tally.raised_pops,
                "raised_tokens": self.tally.raised_tokens,
            },
            "scorer": self.t.scorer.label(),
            "scorer_changed": scorer_changed,
            "rank": rank_json(self.t.boost, self.t.fusion),
            "stops": stops_json(&self.t.stops),
        });
        if let (true, Some(object)) = (pass_through, resume.as_object_mut()) {
            object.insert("pass_through".into(), json!(true));
        }
        if let (false, Some(object)) = (score_bound, resume.as_object_mut()) {
            object.insert("score_bound".into(), json!(false));
        }
        if let (Some(priors), Some(object)) = (self.t.priors.json(), resume.as_object_mut()) {
            object.insert("priors".into(), priors);
        }
        self.events.push(resume);
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
