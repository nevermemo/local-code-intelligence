//! The outer agent's answer to a returned checkpoint (spine brief 1.5, rung
//! 3): a [`Steer`] carried by the next call, which resumes the traversal from
//! the handle ([`Traversal::resumed`](super::Traversal::resumed) and
//! [`Traversal::steer`](super::Traversal::steer)).
//!
//! A steer is a list of the same [`Directive`]s every rung uses, applied
//! before the first pop of the resumed call, plus (only here) `budget`, which
//! adds pops and tokens to the traversal's caps up to
//! [`RAISE_MAX_POPS`](super::handler::RAISE_MAX_POPS) and
//! [`RAISE_MAX_TOKENS`](super::handler::RAISE_MAX_TOKENS) in total.
//!
//! Budget semantics across calls (spine slice S4): the caps of the
//! [`TraversalBudget`](super::TraversalBudget) a call passes (`pops`,
//! `est_tokens`, `external_calls`) are absolute for the traversal, counted
//! from its first call; `deadline` and `max_depth` belong to the call. What
//! the agent raises is added on top of the caps the caller passes, stays with
//! the traversal (a handle keeps it), and is bounded by the constants above.
//! No other rung can raise anything.
//!
//! In JSON:
//!
//! ```json
//! {"actions": [
//!   {"node": "file:src/a.rs", "action": "prune"},
//!   {"node": "file:src/b.rs", "action": {"boost": 2.0}},
//!   {"action": {"map_off": "vector"}},
//!   {"action": {"budget": {"pops": 20, "tokens": 2000}}}
//! ]}
//! ```
//!
//! Node actions (`continue`, `prune`, `expand`, `boost`) need a `node`, the
//! others must not have one; the actions are those of the rule list.

use super::{
    handler::{Action, Directive},
    rules::parse_action_json,
};
use lci_core::node::NodeId;
use serde_json::Value;
use std::str::FromStr;

/// The most directives in one steer.
pub const MAX_STEER_ACTIONS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteerError {
    NotAnObject,
    TooManyActions(usize),
    Invalid { index: usize, reason: String },
}

impl std::fmt::Display for SteerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotAnObject => write!(f, "a steer is an object with an `actions` array"),
            Self::TooManyActions(n) => {
                write!(f, "{n} actions; at most {MAX_STEER_ACTIONS} are allowed")
            }
            Self::Invalid { index, reason } => write!(f, "action {index}: {reason}"),
        }
    }
}

impl std::error::Error for SteerError {}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Steer {
    pub actions: Vec<Directive>,
}

impl Steer {
    pub fn new(actions: Vec<Directive>) -> Self {
        Self { actions }
    }

    /// Parses `{"actions": [{"node"?: id, "action": ..}, ..]}`.
    pub fn from_json(value: &Value) -> Result<Self, SteerError> {
        let list = value
            .as_object()
            .and_then(|object| object.get("actions"))
            .and_then(Value::as_array)
            .ok_or(SteerError::NotAnObject)?;
        if list.len() > MAX_STEER_ACTIONS {
            return Err(SteerError::TooManyActions(list.len()));
        }
        let mut actions = Vec::with_capacity(list.len());
        for (index, raw) in list.iter().enumerate() {
            let invalid = |reason: String| SteerError::Invalid { index, reason };
            let object = raw
                .as_object()
                .ok_or_else(|| invalid("an action is an object".into()))?;
            if let Some(extra) = object.keys().find(|k| *k != "node" && *k != "action") {
                return Err(invalid(format!("unknown key {extra:?}")));
            }
            let body = object
                .get("action")
                .ok_or_else(|| invalid("missing `action`".into()))?;
            let action = parse_steer_action(body).map_err(invalid)?;
            let node = match object.get("node") {
                None => None,
                Some(raw) => Some(
                    raw.as_str()
                        .ok_or_else(|| "`node` is not a string".to_string())
                        .and_then(|id| NodeId::from_str(id).map_err(|e| e.to_string()))
                        .map_err(invalid)?,
                ),
            };
            match (action.is_node_action(), node.is_some()) {
                (true, false) => return Err(invalid(format!("{} needs a node", action.name()))),
                (false, true) => {
                    return Err(invalid(format!("{} takes no node", action.name())));
                }
                _ => {}
            }
            actions.push(Directive { node, action });
        }
        Ok(Self { actions })
    }
}

fn parse_steer_action(body: &Value) -> Result<Action, String> {
    let budget = body.as_object().and_then(|o| o.get("budget"));
    let Some(budget) = budget else {
        return parse_action_json(body);
    };
    let fields = budget
        .as_object()
        .filter(|o| !o.is_empty() && o.keys().all(|k| k == "pops" || k == "tokens"))
        .ok_or("budget takes pops and/or tokens")?;
    let number = |key: &str| match fields.get(key) {
        None => Ok(0),
        Some(v) => v
            .as_u64()
            .ok_or_else(|| format!("budget {key} is not a whole number")),
    };
    Ok(Action::Budget {
        pops: u32::try_from(number("pops")?).map_err(|_| "budget pops does not fit u32")?,
        tokens: number("tokens")?,
    })
}
