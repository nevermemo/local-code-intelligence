//! The confirm frontier (`[run_profile.walk] confirm`, off by default): a
//! language server confirms the text proposals of the kept hops.
//!
//! `grep.live` proposes: its edges say a name occurs in a file, and a bare
//! name is right in a small share of its occurrences (a comment, a string, a
//! same-named symbol elsewhere). After the gate and the beam have decided
//! which offered edges an expansion keeps, the kept hops whose only witnesses
//! are such text proposals, in hop-value order and cut at the call's `top_k`,
//! are sent in ONE call to a [`FrontierConfirmer`], which asks whether the
//! name at each hop's [`Site`] is a reference to the expanded node. The
//! confirmer answers one [`Verdict`] per proposal:
//!
//! * [`Verdict::Confirmed`]: the hop keeps its value; it is relabelled as a
//!   server answer (witness [`WitnessClass::Lsp`], confidence at least
//!   [`CONFIRMED_CONF`]), so later corroboration and the labels of an answer
//!   read it as one ([`relabel`]).
//! * [`Verdict::Refuted`]: the server resolved the name to another node. The
//!   hop is dropped: not a hop, not a route. Its arrival still corroborates at
//!   0 when the node has a state, as an arrival outside the beam does.
//! * [`Verdict::Unknown`]: no answer, an error, the deadline or the call cap.
//!   The hop is unchanged (fail-open).
//!
//! Each proposal is at most one external call. The call is given the pop's
//! remaining `max_external_calls` ([`FrontierConfirmer::confirm`]), what it
//! used is charged to the pop, and a pop with none left makes no call (counted
//! in [`Totals::confirm_skipped`](super::Totals), no failure). The call is
//! bounded by the walk's deadline. A call that fails (an error, or a wrong
//! number of verdicts) leaves every hop as it was and counts in
//! [`Totals::confirm_failures`](super::Totals); nothing here ends the walk.
//!
//! `lci-maps` has no server of its own in the engine: the caller supplies the
//! confirmer ([`LspLive::confirmer`](crate::lsp_live::LspLive::confirmer)).

use crate::{
    MapId,
    edge::{Site, WitnessClass},
    live_grep::GREP_LIVE_MAP_ID,
    traverse::source::FusedEdge,
};
use lci_core::node::NodeId;

/// The confidence a confirmed hop has at least: a server answer, short of the
/// compiler-grade 100.
pub const CONFIRMED_CONF: u8 = 90;

/// What a confirmer decided about one proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// The name at the site is a reference to the proposal's `src`.
    Confirmed,
    /// The name at the site resolves to another node.
    Refuted,
    /// No answer: the server knew nothing there, failed, or no call was left.
    Unknown,
}

impl Verdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Confirmed => "confirmed",
            Self::Refuted => "refuted",
            Self::Unknown => "unknown",
        }
    }
}

/// One text hop to confirm: the expanded node, the node the hop reaches and
/// the place the text was found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub src: NodeId,
    pub dst: NodeId,
    pub site: Site,
}

/// What a confirmer call answered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirmation {
    /// One verdict per proposal, in order. Another count is a failed call.
    pub verdicts: Vec<Verdict>,
    /// The external calls the confirmer made (at most the `max_calls` it was
    /// given); the engine charges them to the pop.
    pub calls: u32,
}

/// Confirms text proposals, for the confirm frontier.
#[crate::async_trait]
pub trait FrontierConfirmer: Send + Sync {
    /// One verdict per proposal, in order, and the external calls made. Each
    /// proposal costs at most one call and the call stops at `max_calls`; the
    /// proposals it could not ask are [`Verdict::Unknown`]. An error is a
    /// failed call: every hop stays as it was.
    async fn confirm(&self, proposals: &[Proposal], max_calls: u32)
    -> Result<Confirmation, String>;

    /// What a handle keeps of this confirmer, so a resume can refuse another:
    /// the server and the question asked of it, as text that names no path
    /// and no secret. Equal confirmers give equal text.
    fn identity(&self) -> String;
}

/// Whether the confirm frontier proposes `edge`: a hop `grep.live` found, with
/// a site, that no compiler-grade index or server answer stands behind.
pub(super) fn proposes(edge: &FusedEdge) -> bool {
    edge.best.map == GREP_LIVE_MAP_ID
        && edge.best.site.is_some()
        && !edge
            .witnesses
            .iter()
            .any(|w| matches!(w, WitnessClass::Compiler | WitnessClass::Lsp))
}

/// A confirmed hop is a server answer: the best edge's witness and the offer
/// of `grep.live` become [`WitnessClass::Lsp`], the witness list gains it, and
/// the confidences are at least [`CONFIRMED_CONF`].
pub(super) fn relabel(edge: &mut FusedEdge) {
    edge.best.witness = WitnessClass::Lsp;
    edge.best.conf = edge.best.conf.max(CONFIRMED_CONF);
    edge.conf = edge.conf.max(CONFIRMED_CONF);
    edge.claim_conf = edge.claim_conf.max(CONFIRMED_CONF);
    edge.witnesses.push(WitnessClass::Lsp);
    edge.witnesses.sort_unstable();
    edge.witnesses.dedup();
    let grep: MapId = GREP_LIVE_MAP_ID;
    for (map, witness) in &mut edge.offers {
        if *map == grep {
            *witness = WitnessClass::Lsp;
        }
    }
}
