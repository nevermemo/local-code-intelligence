//! The reranker frontier (`[traverse] rerank_frontier`, experiment E2 arm 3).
//!
//! After the gate and the beam have decided which offered edges an expansion
//! keeps, ONE batched call to a [`FrontierRescorer`] rates the kept
//! destinations against the question, and the value of each kept hop is
//! multiplied by [`factor`]: `floor + (1 - floor) * s`, `s` the relevance in
//! `[0, 1]`. The factor is in `[floor, 1]`, so a hop is still worth at most 1
//! and the score bound ([`super::stop`]) stays sound; a floor of 1 is the
//! identity. The call is skipped when nothing is kept. It never ends the walk:
//! a call that fails, times out at the walk's deadline or answers badly (the
//! wrong count, a score that is not a number) leaves the factor at 1 for that
//! expansion and counts in [`Totals::rerank_failures`](super::Totals), by
//! [`RerankFailure`] in [`Totals::rerank_failure_reasons`](super::Totals). A
//! call skipped because the `model_calls` cap is reached (see
//! [`Traversal::model_calls`](super::Traversal::model_calls)) is no failure: it
//! counts in [`Totals::rerank_skipped`](super::Totals).
//!
//! `lci-maps` has no HTTP: the caller supplies the rescorer, and with it the
//! text each node is rated by and what its scores mean (a relevance in
//! `[0, 1]` for every document, whatever the other documents of the call
//! scored).

use lci_core::node::NodeId;
use std::{fmt, str::FromStr};

/// Why a rescorer call gave no usable scores.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RerankFailure {
    /// The walk's deadline ended the call before the rescorer answered.
    Deadline,
    /// The rescorer's own timeout.
    Timeout,
    /// The service could not be reached, or is skipped as recently
    /// unreachable.
    Unreachable,
    /// The service answered with an error status.
    Status,
    /// The answer could not be read: a body that does not parse, an index out
    /// of range or repeated, a score that is not a number.
    Malformed,
    /// The answer holds another number of scores than documents.
    CountMismatch,
    /// Any other error.
    Other,
}

impl RerankFailure {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Deadline => "deadline",
            Self::Timeout => "timeout",
            Self::Unreachable => "unreachable",
            Self::Status => "status",
            Self::Malformed => "malformed",
            Self::CountMismatch => "count_mismatch",
            Self::Other => "other",
        }
    }
}

impl fmt::Display for RerankFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for RerankFailure {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        [
            Self::Deadline,
            Self::Timeout,
            Self::Unreachable,
            Self::Status,
            Self::Malformed,
            Self::CountMismatch,
            Self::Other,
        ]
        .into_iter()
        .find(|reason| reason.as_str() == text)
        .ok_or_else(|| format!("unknown rerank failure reason {text:?}"))
    }
}

/// A failed rescorer call: the reason, and the rescorer's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RescoreError {
    pub reason: RerankFailure,
    pub message: String,
}

impl RescoreError {
    pub fn new(reason: RerankFailure, message: impl Into<String>) -> Self {
        Self {
            reason,
            message: message.into(),
        }
    }
}

/// A bare message is an error of reason [`RerankFailure::Other`].
impl From<String> for RescoreError {
    fn from(message: String) -> Self {
        Self::new(RerankFailure::Other, message)
    }
}

impl From<&str> for RescoreError {
    fn from(message: &str) -> Self {
        Self::new(RerankFailure::Other, message)
    }
}

impl fmt::Display for RescoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.reason, self.message)
    }
}

impl std::error::Error for RescoreError {}

/// Rates nodes against the question, for the reranker frontier.
#[crate::async_trait]
pub trait FrontierRescorer: Send + Sync {
    /// The relevance of each of `nodes` to `question`, in `[0, 1]`, one per
    /// node in order, each decided by its own score alone. A value outside
    /// `[0, 1]` is clamped. An error (or a list of the wrong length) is a
    /// failed call.
    async fn relevance(&self, question: &str, nodes: &[NodeId]) -> Result<Vec<f64>, RescoreError>;

    /// What a handle keeps of this rescorer, so a resume can refuse another:
    /// the model, the endpoint and how its scores are read, as text that names
    /// no path and no secret. Equal rescorers give equal text.
    fn identity(&self) -> String;
}

/// What a hop is multiplied by when its destination has relevance `s`:
/// `floor + (1 - floor) * s`, with `s` clamped to `[0, 1]` and the floor to
/// `[0, 1]`. Never above 1.
pub fn factor(floor: f64, s: f64) -> f64 {
    let floor = floor.clamp(0.0, 1.0);
    floor + (1.0 - floor) * s.clamp(0.0, 1.0)
}

/// The floor a traversal uses: the given one clamped to `[0, 1]`; anything
/// that is not a number is 1 (no effect).
pub(super) fn sane_floor(floor: f64) -> f64 {
    if floor.is_finite() {
        floor.clamp(0.0, 1.0)
    } else {
        1.0
    }
}
