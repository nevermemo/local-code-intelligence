//! Contract C5, source labels (`docs/spike/multimap/specs/contract-kit.md`,
//! C5): the closed vocabulary of [`SourceLabel`] with its wire names,
//! versioned with the RunRecord schema that carries them; the check of an
//! answer against the labels its map's card lists; [`Labelled`], which
//! applies that check to every answer a registry gets from a map; and the
//! kit's C5 check.
//!
//! The vocabulary is closed by construction in Rust (an enum); on the wire
//! the schema's `label` enum closes it, and a test ties the two together.
//! What can still go wrong at run time is a map emitting a label its card
//! does not list, which is what [`check_labels`] refuses.

use crate::{
    Map, MapDescriptor, MapRequest, MapResponse, MapStatus, Registry, SnapshotView, SourceLabel,
    kit::{ContractFailure, Factory, Fixture},
};

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

/// Contract C5 on one answer: every hit carries evidence, and every evidence
/// item a label that the map's card (`descriptor.source_labels`) lists. A
/// hit without evidence has no source label and is refused (contract-kit.md
/// 5, "one without a source label"; open decision K-7). Only an answer whose
/// hits are used (`Ok` or `Partial`) is checked.
pub fn check_labels(descriptor: &MapDescriptor, response: &MapResponse) -> Result<(), String> {
    if !matches!(response.status, MapStatus::Ok | MapStatus::Partial { .. }) {
        return Ok(());
    }
    for hit in &response.hits {
        if hit.evidence.is_empty() {
            return Err(format!("{} has no evidence, so no source label", hit.node));
        }
        for evidence in &hit.evidence {
            if !descriptor.source_labels.contains(&evidence.label) {
                return Err(format!(
                    "label {} on {} is not on its card ({})",
                    evidence.label.as_str(),
                    hit.node,
                    descriptor
                        .source_labels
                        .iter()
                        .map(|label| label.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
    }
    Ok(())
}

/// A map whose answers go through [`check_labels`]: an answer with a label
/// its card does not list becomes `Failed`, with a warning naming the map,
/// so the registry drops its hits and the run goes on (contract C1).
/// Otherwise the answer is passed on unchanged.
pub struct Labelled<'a> {
    inner: Box<dyn Map + 'a>,
}

impl<'a> Labelled<'a> {
    pub fn new(inner: Box<dyn Map + 'a>) -> Self {
        Self { inner }
    }
}

#[async_trait::async_trait]
impl Map for Labelled<'_> {
    fn describe(&self) -> &MapDescriptor {
        self.inner.describe()
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let response = self.inner.query(request, view).await;
        let descriptor = self.describe();
        match check_labels(descriptor, &response) {
            Ok(()) => response,
            Err(reason) => MapResponse {
                hits: Vec::new(),
                status: MapStatus::Failed {
                    warning: format!("map {} returned an invalid answer: {reason}", descriptor.id),
                },
                ..response
            },
        }
    }
}

/// The kit's contract C5: runs the subject beside the control and checks
/// every answer the registry got, the subject's and the control's, against
/// its card. A failure names the map and the label.
pub async fn check_source_labels<'a>(
    subject: Factory<'_, 'a>,
    control: Factory<'_, 'a>,
    fixture: &Fixture<'_, 'a>,
) -> Result<(), ContractFailure> {
    let maps = [subject(), control()];
    let descriptors: Vec<MapDescriptor> = maps.iter().map(|m| m.describe().clone()).collect();
    let mut registry = Registry::new(fixture.rrf_k);
    for map in maps {
        registry
            .register(map, 1.0, 40)
            .map_err(|error| ContractFailure {
                contract: "kit",
                detail: error.to_string(),
            })?;
    }
    let run = registry
        .run(Some(fixture.text), fixture.budget, fixture.view)
        .await;
    for call in &run.calls {
        let descriptor = descriptors
            .iter()
            .find(|d| d.id == call.id)
            .expect("registered");
        check_labels(descriptor, &call.response).map_err(|reason| ContractFailure {
            contract: "C5",
            detail: format!("map {}: {reason}", call.id),
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        Contribution, EdgeKind, Evidence, FreshnessModel, MapHit, SnapshotRef, kit::FixedMap,
    };
    use lci_core::chunk::Chunk;
    use std::time::Duration;

    #[test]
    fn the_vocabulary_is_the_schema_label_enum_and_round_trips() {
        assert_eq!(LABELS_VERSION, crate::run_record::SCHEMA_ID);
        let schema = crate::run_record::schema().unwrap();
        let names: Vec<&str> = SourceLabel::ALL.iter().map(|l| l.as_str()).collect();
        assert_eq!(schema.enum_of("label").unwrap(), names);
        for label in SourceLabel::ALL {
            assert_eq!(SourceLabel::from_wire(label.as_str()), Some(label));
        }
        assert_eq!(SourceLabel::from_wire("Lexical"), None);
        assert_eq!(SourceLabel::from_wire("embedding"), None);
    }

    fn chunks() -> Vec<Chunk> {
        ["a.rs", "b.rs", "c.rs"]
            .iter()
            .map(|path| Chunk {
                relative_file_path: (*path).into(),
                language: "rust".into(),
                start_line: 1,
                end_line: 3,
                code: String::new(),
                content_hash: format!("{path}-hash"),
            })
            .collect()
    }

    fn map(
        view: &SnapshotView<'_>,
        id: &'static str,
        card: Vec<SourceLabel>,
        emitted: Option<SourceLabel>,
        positions: &[usize],
    ) -> Box<dyn Map + 'static> {
        Box::new(FixedMap {
            descriptor: MapDescriptor {
                id,
                version: 1,
                contribution: Contribution::RankList,
                accepts_text: true,
                accepts: vec![],
                emits: vec![],
                freshness: FreshnessModel::Live,
                source_labels: card,
                needs: vec![],
            },
            hits: positions
                .iter()
                .enumerate()
                .map(|(rank, &at)| MapHit {
                    node: view.ids()[at].clone(),
                    rank: rank as u32 + 1,
                    score: None,
                    evidence: emitted
                        .map(|label| Evidence {
                            via: None,
                            edge: EdgeKind::QueryMatch,
                            label,
                            detail: None,
                        })
                        .into_iter()
                        .collect(),
                })
                .collect(),
        })
    }

    fn fixture<'v, 'a>(view: &'v SnapshotView<'a>) -> Fixture<'v, 'a> {
        Fixture {
            view,
            text: "query",
            budget: Duration::from_millis(500),
            slack: Duration::from_millis(500),
            rrf_k: 60.0,
        }
    }

    /// Known-bad map: its card lists `lexical`, it emits `vector`.
    #[tokio::test]
    async fn the_kit_rejects_a_map_emitting_a_label_off_its_card() {
        let chunks = chunks();
        let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
        let fixture = fixture(&view);
        let control = || {
            map(
                &view,
                "control",
                vec![SourceLabel::Heuristic],
                Some(SourceLabel::Heuristic),
                &[0],
            )
        };
        let good = || {
            map(
                &view,
                "good",
                vec![SourceLabel::Lexical, SourceLabel::Vector],
                Some(SourceLabel::Vector),
                &[1, 2],
            )
        };
        let bad = || {
            map(
                &view,
                "mislabelled",
                vec![SourceLabel::Lexical],
                Some(SourceLabel::Vector),
                &[1, 2],
            )
        };
        check_source_labels(&good, &control, &fixture)
            .await
            .unwrap();
        let failure = check_source_labels(&bad, &control, &fixture)
            .await
            .unwrap_err();
        assert_eq!(failure.contract, "C5", "{failure:?}");
        assert!(
            failure.detail.contains("mislabelled") && failure.detail.contains("vector"),
            "{failure:?}"
        );
        // A map whose hits carry no evidence has no source label.
        let unlabelled = || map(&view, "unlabelled", vec![SourceLabel::Lexical], None, &[1]);
        let failure = check_source_labels(&unlabelled, &control, &fixture)
            .await
            .unwrap_err();
        assert_eq!(failure.contract, "C5", "{failure:?}");
        assert!(failure.detail.contains("no evidence"), "{failure:?}");
        // The control is checked too: a mislabelled control is caught.
        let failure = check_source_labels(&good, &bad, &fixture)
            .await
            .unwrap_err();
        assert_eq!(failure.contract, "C5", "{failure:?}");
    }

    /// Through [`Labelled`], the registry validates every answer: the
    /// mislabelled map fails with one warning naming it and the pool is
    /// the control's alone; a well-labelled map passes unchanged.
    #[tokio::test]
    async fn a_labelled_registry_drops_a_mislabelled_answer_and_keeps_a_good_one() {
        let chunks = chunks();
        let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
        let run_with = |maps: Vec<Box<dyn Map + 'static>>| {
            let view = &view;
            async move {
                let mut registry = Registry::new(60.0);
                for map in maps {
                    registry
                        .register(Box::new(Labelled::new(map)), 1.0, 10)
                        .unwrap();
                }
                registry.run(Some("q"), Duration::from_secs(5), view).await
            }
        };
        let control = || {
            map(
                &view,
                "control",
                vec![SourceLabel::Heuristic],
                Some(SourceLabel::Heuristic),
                &[0],
            )
        };
        let alone = run_with(vec![control()]).await;
        let bad = run_with(vec![
            map(
                &view,
                "mislabelled",
                vec![SourceLabel::Lexical],
                Some(SourceLabel::Vector),
                &[1, 2],
            ),
            control(),
        ])
        .await;
        assert_eq!(bad.fused, alone.fused);
        let warnings = bad.warnings();
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert_eq!(warnings[0].0, "mislabelled");
        assert!(warnings[0].1.contains("label vector"), "{warnings:?}");

        let good = run_with(vec![
            map(
                &view,
                "good",
                vec![SourceLabel::Lexical],
                Some(SourceLabel::Lexical),
                &[1, 2],
            ),
            control(),
        ])
        .await;
        assert!(good.warnings().is_empty());
        assert_eq!(good.fused.len(), 3);
        let plain = {
            let mut registry = Registry::new(60.0);
            registry
                .register(
                    map(
                        &view,
                        "good",
                        vec![SourceLabel::Lexical],
                        Some(SourceLabel::Lexical),
                        &[1, 2],
                    ),
                    1.0,
                    10,
                )
                .unwrap();
            registry.register(control(), 1.0, 10).unwrap();
            registry.run(Some("q"), Duration::from_secs(5), &view).await
        };
        assert_eq!(good.fused, plain.fused, "a valid answer passes unchanged");
    }
}
