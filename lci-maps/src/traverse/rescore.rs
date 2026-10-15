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
//! expansion and counts in [`Totals::rerank_failures`](super::Totals).
//!
//! `lci-maps` has no HTTP: the caller supplies the rescorer, and with it the
//! text each node is rated by.

use lci_core::node::NodeId;

/// Rates nodes against the question, for the reranker frontier.
#[crate::async_trait]
pub trait FrontierRescorer: Send + Sync {
    /// The relevance of each of `nodes` to `question`, in `[0, 1]`, one per
    /// node in order. A value outside `[0, 1]` is clamped. An error (or a
    /// list of the wrong length) is a failed call.
    async fn relevance(&self, question: &str, nodes: &[NodeId]) -> Result<Vec<f64>, String>;
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
