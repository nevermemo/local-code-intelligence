//! Stops and handlers (spine brief 1.5, rung 1 only in this slice).
//!
//! At a fixed stop (every N pops) the engine builds a [`Checkpoint`] and asks
//! a [`Handler`] for a [`Decision`]: a list of [`Directive`]s over the
//! frontier. The engine applies them under its own limits, whatever the
//! handler asks: a directive can shrink the frontier (`Prune`), reorder it
//! (`Expand`, `Boost`, `Rerank`), switch a map off or scale its hops down, or
//! `Redirect` to other seeds. There is no action that raises a budget.

use crate::{MapId, edge::WitnessClass};
use lci_core::node::NodeId;
use std::time::Duration;

/// The largest factor a `Boost` may apply to a priority.
pub const BOOST_MAX: f64 = 8.0;
/// The most seeds one `Redirect` may name.
pub const REDIRECT_MAX: usize = 16;
/// The longest `Emit` note, in characters.
pub const EMIT_MAX: usize = 200;

/// One frontier node as a handler sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct FrontierItem {
    pub node: NodeId,
    /// The map whose edge discovered the node; `None` for a seed without one.
    pub via: Option<MapId>,
    /// The witness class of that edge; `None` for a seed.
    pub witness: Option<WitnessClass>,
    /// The queue priority: the path potential until a handler changes it.
    pub score: f64,
    pub depth: u32,
    /// How many edges the discovering expansion offered (seeds: the seed
    /// count).
    pub fanout: u32,
    /// Whether more than one edge into the node has been seen.
    pub seen_before: bool,
}

/// What the engine shows a handler at a stop. The full payload of the brief
/// (cards, regions, disagreements) comes with the ladder slice.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Checkpoint {
    pub pops: u32,
    /// One-based number of this stop.
    pub stop: u32,
    /// Share of the pop budget still left, whole percent.
    pub budget_left_pct: u32,
    pub tokens_left: u64,
    /// Best first, at most [`crate::traverse::CHECKPOINT_FRONTIER`] items.
    pub frontier: Vec<FrontierItem>,
    pub visited: usize,
    pub cleared: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RerankBy {
    /// Strongest witness class first.
    Witness,
    /// Shallowest first.
    Depth,
    /// Smallest fan-out first.
    Fanout,
}

impl RerankBy {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Witness => "witness",
            Self::Depth => "depth",
            Self::Fanout => "fanout",
        }
    }
}

/// What a handler can do. Node actions take a frontier node; the others act
/// on the traversal.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Node: do nothing (and stop matching further rules for it).
    Continue,
    /// Node: remove it from the frontier for good ("cleared").
    Prune,
    /// Node: expand it next.
    Expand,
    /// Node: multiply its priority by a factor in `0..=BOOST_MAX`.
    Boost(f64),
    MapOn(String),
    MapOff(String),
    /// Scale the hop values of a map by a weight in `0..=1`.
    MapWeight(String, f64),
    Rerank(RerankBy),
    /// Replace the frontier with these seeds.
    Redirect(Vec<NodeId>),
    Stop,
    /// A note for the trace.
    Emit(String),
}

impl Action {
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Continue => "continue",
            Self::Prune => "prune",
            Self::Expand => "expand",
            Self::Boost(_) => "boost",
            Self::MapOn(_) => "map_on",
            Self::MapOff(_) => "map_off",
            Self::MapWeight(..) => "map_weight",
            Self::Rerank(_) => "rerank",
            Self::Redirect(_) => "redirect",
            Self::Stop => "stop",
            Self::Emit(_) => "emit",
        }
    }

    /// Whether the action is about one frontier node.
    pub const fn is_node_action(&self) -> bool {
        matches!(
            self,
            Self::Continue | Self::Prune | Self::Expand | Self::Boost(_)
        )
    }
}

/// An action, with the node it is about (`None` for a traversal action).
#[derive(Debug, Clone, PartialEq)]
pub struct Directive {
    pub node: Option<NodeId>,
    pub action: Action,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Decision {
    pub actions: Vec<Directive>,
    /// 0 to 1. The rule list is 1 when some rule matched, else 0.
    pub confidence: f32,
    /// Wall time the handler spent.
    pub cost: Duration,
    /// The handler ran out of its evaluation time and stopped early.
    pub truncated: bool,
}

/// What a stop may spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopBudget {
    pub max_eval: Duration,
}

impl Default for StopBudget {
    fn default() -> Self {
        Self {
            max_eval: crate::traverse::rules::EVAL_CAP,
        }
    }
}

pub trait Handler: Send + Sync {
    fn id(&self) -> &str;
    fn decide(&self, checkpoint: &Checkpoint, budget: &StopBudget) -> Decision;
}

/// When a stop fires. A fixed every-N-pops stop in this slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StopPolicy {
    /// A stop after every this many pops; 0 means never.
    pub every_pops: u32,
    pub max_stops: u32,
}

impl Default for StopPolicy {
    fn default() -> Self {
        Self {
            every_pops: 0,
            max_stops: 4,
        }
    }
}
