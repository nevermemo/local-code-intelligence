//! The `docs.mentions` map: from a node, the documentation files whose
//! current bytes mention its name. A wrapper of [`LiveGrep`] whose walk reads
//! documentation files only.
//!
//! # What an edge says
//!
//! An edge from `S` to a file `D` of kind [`EdgeKind::Mentions`] says that `D`
//! holds the name of `S` in its text, as `grep.live` finds it: a word in
//! prose, no proof of a reference. The edge carries the label
//! [`SourceLabel::Doc`], a witness of [`WitnessClass::Exact`] (the name stands
//! alone) or [`WitnessClass::Text`] (the name is inside a longer identifier),
//! the confidence `grep.live` computes from the count of occurrences, and one
//! site: the first occurrence of the best class. The destination is always a
//! [`NodeKind::File`]. There is no locator, so no edge goes to a symbol, and no
//! section node is made: a document has no SCIP span, and a heading splitter
//! is a separate step.
//!
//! # Which files are read
//!
//! Only files whose [`classify_source_role`] is
//! [`SourceRole::Documentation`]: any `.md` file wherever it sits, and files
//! under a `doc`, `docs` or `documentation` directory. Other files are never
//! yielded by the walk, so they are neither read nor counted. A configuration
//! file such as a root `config.example.toml` is not documentation (it is
//! `Source`); the `config.key` map covers keys in such files.
//!
//! # Needle and caps
//!
//! The needle is the one `grep.live` takes: a symbol's bare name, or the stem
//! of a file's or chunk's name; case-sensitive, whole-word matches first. Names
//! under `min_name_len` (4 bytes) and the stoplist of ubiquitous names make no
//! edge, by design. The caps are `grep.live`'s: files above 1 MiB, binary and
//! non-UTF-8 files are skipped, the scan stops at 256 MiB or at the earlier of
//! the call's deadline and 3 s, and at most 256 edges leave one call. Only the
//! occurrence cap differs: 200 instead of 50, because the cap is taken in walk
//! order and one early document can hold dozens of occurrences of a config key.
//! The binding cap is that 200-occurrence cap in walk order (at most 200
//! distinct files), not the 256-edge cap.
//!
//! # Freshness
//!
//! [`FreshnessModel::Live`], as `grep.live`: every call reads the current
//! bytes of the tree, so an edit is seen by the next call.
//!
//! The map has no key and no default; it runs only when a call names it, and
//! it needs the tree root like `grep.live`.

use crate::{
    Budget, Contribution, FreshnessModel, Map, MapDescriptor, MapId, MapRequest, MapResponse,
    SnapshotView, SourceLabel,
    edge::{Expanded, MapCard, QuestionType},
    live_grep::{CostLog, GrepOptions, LiveGrep},
};
use lci_core::{
    filter::SourceRole,
    node::{NodeId, NodeKind},
};
use std::{path::PathBuf, time::Duration};

/// The id of [`DocsMentions`].
pub const DOCS_MENTIONS_ID: MapId = "docs.mentions";

/// The options of the wrapped scan: documentation files only, and 200
/// occurrences instead of 50 (the cap is taken in walk order).
pub fn docs_options() -> GrepOptions {
    GrepOptions {
        roles: Some(vec![SourceRole::Documentation]),
        max_matches: 200,
        ..GrepOptions::default()
    }
}

/// `docs.mentions`; see the module notes.
pub struct DocsMentions {
    descriptor: MapDescriptor,
    grep: LiveGrep,
}

impl DocsMentions {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self::with_options(root, docs_options())
    }

    /// [`Self::new`] with a scan timeout no loaded machine reaches, for the
    /// tests of what the map answers (the default stops a scan after 3 s).
    #[cfg(test)]
    fn patient(root: impl Into<PathBuf>) -> Self {
        Self::with_options(
            root,
            GrepOptions {
                timeout: Duration::from_secs(3600),
                ..docs_options()
            },
        )
    }

    fn with_options(root: impl Into<PathBuf>, options: GrepOptions) -> Self {
        Self {
            descriptor: MapDescriptor {
                id: DOCS_MENTIONS_ID,
                version: 1,
                contribution: Contribution::Expansion,
                accepts_text: false,
                accepts: vec![NodeKind::Symbol, NodeKind::File, NodeKind::Chunk],
                emits: vec![NodeKind::File],
                freshness: FreshnessModel::Live,
                source_labels: vec![SourceLabel::Doc],
                needs: Vec::new(),
            },
            grep: LiveGrep::with_options(root, None, options),
        }
    }

    /// The call times kept so far.
    pub fn log(&self) -> &CostLog {
        self.grep.log()
    }

    /// Records `runs` scans for a name that occurs nowhere (the whole
    /// documentation walk is read), so the card has a measured median before
    /// the first real call. Returns that median.
    pub async fn calibrate(&self, runs: usize) -> Option<f64> {
        self.grep.calibrate(runs).await
    }
}

#[crate::async_trait]
impl Map for DocsMentions {
    fn describe(&self) -> &MapDescriptor {
        &self.descriptor
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let MapResponse {
            mut hits,
            status,
            cost,
            freshness,
        } = self.grep.query(request, view).await;
        for hit in &mut hits {
            for evidence in &mut hit.evidence {
                evidence.label = SourceLabel::Doc;
            }
        }
        MapResponse {
            hits,
            status,
            cost,
            freshness,
        }
    }

    async fn expand(&self, node: &NodeId, budget: Budget, view: &SnapshotView<'_>) -> Expanded {
        let mut expanded = self.grep.expand(node, budget, view).await;
        expanded
            .edges
            .retain(|edge| edge.dst.kind() == NodeKind::File);
        for edge in &mut expanded.edges {
            edge.map = DOCS_MENTIONS_ID;
        }
        expanded
    }

    fn card(&self) -> MapCard {
        MapCard {
            expand_p50_ms: self.grep.log().p50(),
            coverage: self.descriptor.emits.clone(),
            freshness_lag: Some(Duration::ZERO),
            dependent: false,
            good_for: vec![QuestionType::WhereTo, QuestionType::Impact],
        }
    }
}

#[cfg(test)]
mod tests;
