//! The closed source-label vocabulary of contract C5
//! (`docs/spike/multimap/specs/contract-kit.md`, C5): the wire names of
//! [`SourceLabel`], versioned with the RunRecord schema that carries them.

use crate::SourceLabel;

/// The label vocabulary is versioned with the RunRecord schema: a label is
/// added or renamed only with a new schema id, and a test checks that the
/// checked-in schema's `label` enum lists exactly [`SourceLabel::ALL`].
pub const LABELS_VERSION: &str = crate::run_record::SCHEMA_ID;

impl SourceLabel {
    /// Every label, in the order of the schema's `label` enum.
    pub const ALL: [SourceLabel; 11] = [
        Self::Vector,
        Self::Lexical,
        Self::Lsp,
        Self::Scip,
        Self::Tags,
        Self::Heuristic,
        Self::Git,
        Self::Config,
        Self::Doc,
        Self::Agent,
        Self::Prior,
    ];

    /// The wire name a RunRecord carries.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Vector => "vector",
            Self::Lexical => "lexical",
            Self::Lsp => "lsp",
            Self::Scip => "scip",
            Self::Tags => "tags",
            Self::Heuristic => "heuristic",
            Self::Git => "git",
            Self::Config => "config",
            Self::Doc => "doc",
            Self::Agent => "agent",
            Self::Prior => "prior",
        }
    }

    /// The label with wire name `name`; `None` for a name outside the
    /// vocabulary.
    pub fn from_wire(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|label| label.as_str() == name)
    }
}
