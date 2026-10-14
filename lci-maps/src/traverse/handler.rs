//! Stops and handlers (spine brief 1.5).
//!
//! At a stop ([`StopPolicy`]) the engine builds a [`Checkpoint`] and asks a
//! [`Handler`] for a [`Decision`]: a list of [`Directive`]s over the
//! frontier. The handler is usually a [`Ladder`](super::ladder::Ladder), whose
//! decision also says which rung decided and which rungs it tried first, or
//! that it found none confident enough and the checkpoint goes back to the
//! outer agent (rung 3).
//!
//! The engine applies directives under its own limits, whatever the handler
//! asks: a directive can shrink the frontier (`Prune`), reorder it (`Expand`,
//! `Boost`, `Rerank`), switch a map off or scale its hops down, or `Redirect`
//! to other seeds. The one action that raises a budget, `Budget`, is applied
//! only when the outer agent sends it in a [`Steer`](super::steer::Steer), and
//! only up to a fixed cap.

use super::stop::{Card, Disagreement, LikelyComplete, MapStat, Regions, StopReason};
use crate::{MapId, edge::WitnessClass};
use lci_core::node::NodeId;
use std::time::Duration;

pub use super::stop::StopPolicy;

/// The largest factor a `Boost` may apply to a priority.
pub const BOOST_MAX: f64 = 8.0;
/// The most seeds one `Redirect` may name.
pub const REDIRECT_MAX: usize = 16;
/// The longest `Emit` note, in characters.
pub const EMIT_MAX: usize = 200;
/// The pops all `Budget` actions of a traversal may add together.
pub const RAISE_MAX_POPS: u32 = 200;
/// The tokens all `Budget` actions of a traversal may add together.
pub const RAISE_MAX_TOKENS: u64 = 20_000;

/// The rung of the deterministic rule list.
pub const RUNG_RULES: u8 = 1;
/// The rung of the fuzzy local models (slice S8).
pub const RUNG_SYSTEM_ONE: u8 = 2;
/// The rung of the outer agent.
pub const RUNG_AGENT: u8 = 3;

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
    /// The product of the hop values along the path.
    pub potential: f64,
    pub depth: u32,
    /// How many edges the discovering expansion offered (seeds: the seed
    /// count).
    pub fanout: u32,
    /// Whether more than one edge into the node has been seen.
    pub seen_before: bool,
    /// The tokens an expansion of a node of its kind has cost so far, on
    /// average; 0 before the first.
    pub est_cost: u64,
}

/// What the engine shows a handler at a stop, and returns to the agent
/// when the ladder escalates or a budget ends the call.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Checkpoint {
    pub reason: StopReason,
    /// The handle that resumes the traversal; set only on a checkpoint that
    /// is returned to the caller, and not part of its hash.
    pub handle: Option<String>,
    pub question: String,
    /// The roots of the search: the seeds, and the nodes a redirect added.
    pub seeds: Vec<NodeId>,
    pub pops: u32,
    /// One-based number of this stop.
    pub stop: u32,
    /// Share of the pop budget still left, whole percent.
    pub budget_left_pct: u32,
    pub pops_left: u32,
    pub tokens_left: u64,
    pub calls_left: u32,
    /// The best answer so far (a guess at what the agent wants): the targets
    /// reached, then the best visited nodes, at most [`super::stop::ANSWER_K`].
    pub answer: Vec<Card>,
    /// The best visited (expanded) nodes by potential, at most
    /// [`super::stop::CARD_K`].
    pub visited_cards: Vec<Card>,
    /// Best first, at most [`crate::traverse::CHECKPOINT_FRONTIER`] items.
    pub frontier: Vec<FrontierItem>,
    /// Per-map counts and costs, by map id.
    pub maps: Vec<(MapId, MapStat)>,
    pub regions: Regions,
    /// Expanded nodes.
    pub visited: usize,
    /// Pruned nodes.
    pub cleared: usize,
    /// The latest few, at most [`super::stop::CHECKPOINT_DISAGREEMENTS`].
    pub disagreements: Vec<Disagreement>,
    /// All recorded.
    pub disagreements_total: usize,
    pub likely_complete: Option<LikelyComplete>,
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
    /// Add to the pop and token budget, up to [`RAISE_MAX_POPS`] and
    /// [`RAISE_MAX_TOKENS`] over the traversal. Only the outer agent's steer
    /// applies it; the engine refuses it from any other rung. The rule list
    /// cannot express it.
    Budget {
        pops: u32,
        tokens: u64,
    },
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
            Self::Budget { .. } => "budget",
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

/// One rung that was consulted.
#[derive(Debug, Clone, PartialEq)]
pub struct Attempt {
    pub rung: u8,
    pub handler: String,
    pub confidence: f32,
    pub truncated: bool,
    /// What it proposed.
    pub actions: Vec<Directive>,
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
    /// The rung that decided; 0 means the handler's own [`Handler::rung`].
    pub rung: u8,
    /// The id of the handler that decided; empty means the handler itself.
    pub handler: String,
    /// No rung was confident: return the checkpoint to the outer agent.
    pub escalate: bool,
    /// The rungs consulted and found uncertain before this decision, in
    /// order. Empty for a handler that is not a ladder.
    pub trail: Vec<Attempt>,
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

    /// The rung this handler sits on (1: rules, 2: System One).
    fn rung(&self) -> u8 {
        RUNG_RULES
    }

    fn decide(&self, checkpoint: &Checkpoint, budget: &StopBudget) -> Decision;
}
