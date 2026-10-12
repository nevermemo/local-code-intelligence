use super::*;
use crate::kit::{
    ContractFailure, FixedMap, Fixture, check_determinism, check_fail_open, check_real_failure,
};
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};

fn chunk(path: &str, start_line: u32) -> Chunk {
    Chunk {
        relative_file_path: path.into(),
        language: "rust".into(),
        start_line,
        end_line: start_line + 4,
        code: format!("fn at_{start_line}() {{}}"),
        content_hash: format!("{path}:{start_line}"),
    }
}

fn universe() -> Vec<Chunk> {
    ["a.rs", "b.rs", "c.rs", "d.rs", "tests/e.rs"]
        .iter()
        .map(|path| chunk(path, 1))
        .collect()
}

fn descriptor(id: MapId, contribution: Contribution) -> MapDescriptor {
    MapDescriptor {
        id,
        version: 1,
        contribution,
        accepts_text: contribution != Contribution::Prior,
        accepts: vec![],
        emits: vec![NodeKind::Chunk],
        freshness: FreshnessModel::Live,
        source_labels: vec![SourceLabel::Heuristic],
        needs: vec![],
    }
}

fn hits(view: &SnapshotView<'_>, positions: &[usize], score: Option<f32>) -> Vec<MapHit> {
    positions
        .iter()
        .enumerate()
        .map(|(rank, &position)| MapHit {
            node: view.ids()[position].clone(),
            rank: rank as u32 + 1,
            score,
            evidence: vec![],
        })
        .collect()
}

fn fixed(
    view: &SnapshotView<'_>,
    id: MapId,
    contribution: Contribution,
    positions: &[usize],
    score: Option<f32>,
) -> Box<dyn Map + 'static> {
    Box::new(FixedMap {
        descriptor: descriptor(id, contribution),
        hits: hits(view, positions, score),
    })
}

fn fixture<'v, 'a>(view: &'v SnapshotView<'a>) -> Fixture<'v, 'a> {
    Fixture {
        view,
        text: "query",
        budget: Duration::from_millis(200),
        slack: Duration::from_millis(400),
        rrf_k: 60.0,
    }
}

#[tokio::test]
async fn fusion_adds_weighted_rrf_terms_in_legacy_order_then_the_prior() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let mut registry = Registry::new(60.0);
    // Registered out of order on purpose.
    registry
        .register(
            fixed(
                &view,
                "prior.test",
                Contribution::Prior,
                &[0, 1, 2],
                Some(0.004),
            ),
            9.0,
            40,
        )
        .unwrap()
        .register(
            fixed(&view, "lsp.symbols", Contribution::RankList, &[2], None),
            4.0,
            40,
        )
        .unwrap()
        .register(
            fixed(&view, "aaa.extra", Contribution::RankList, &[3], None),
            1.0,
            40,
        )
        .unwrap()
        .register(
            fixed(&view, "lexical", Contribution::RankList, &[1, 0], None),
            0.5,
            40,
        )
        .unwrap()
        .register(
            fixed(
                &view,
                "vec.chunk.code",
                Contribution::RankList,
                &[0, 2],
                None,
            ),
            3.0,
            40,
        )
        .unwrap();
    assert_eq!(
        registry.ids(),
        [
            "vec.chunk.code",
            "lexical",
            "lsp.symbols",
            "aaa.extra",
            "prior.test"
        ]
    );
    let run = registry.run(Some("q"), Duration::from_secs(5), &view).await;
    let score = |path: &str| {
        run.fused
            .iter()
            .find(|f| view.chunk(&f.node).unwrap().relative_file_path == path)
            .unwrap()
            .score
    };
    let mut a = 0.0_f32;
    a += 3.0 / (60.0 + 1.0);
    a += 0.5 / (60.0 + 2.0);
    a += 0.004;
    assert_eq!(score("a.rs").to_bits(), a.to_bits());
    let mut c = 0.0_f32;
    c += 3.0 / (60.0 + 2.0);
    c += 4.0 / (60.0 + 1.0);
    // c.rs has no prior value in this fixture's prior map? It does (position 2).
    c += 0.004;
    assert_eq!(score("c.rs").to_bits(), c.to_bits());
    // d.rs: only the extra map; the prior map did not answer for it.
    assert_eq!(score("d.rs").to_bits(), (1.0_f32 / 61.0).to_bits());
    assert!(run.warnings().is_empty() && run.notes().is_empty());
    let order: Vec<f32> = run.fused.iter().map(|f| f.score).collect();
    assert!(order.windows(2).all(|w| w[0] >= w[1]), "{order:?}");
}

#[tokio::test]
async fn equal_scores_keep_the_legacy_chunk_order() {
    let mut chunks = [chunk("same.rs", 9), chunk("same.rs", 1), chunk("b.rs", 1)];
    // Hashes ordered against the lines, so the NodeId string order
    // (`chunk:same.rs@a..` before `@z..`) differs from the legacy one.
    chunks[0].content_hash = "a-line-9".into();
    chunks[1].content_hash = "z-line-1".into();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let mut registry = Registry::new(60.0);
    registry
        .register(
            fixed(&view, "one", Contribution::RankList, &[0], None),
            1.0,
            40,
        )
        .unwrap()
        .register(
            fixed(&view, "two", Contribution::RankList, &[1], None),
            1.0,
            40,
        )
        .unwrap()
        .register(
            fixed(&view, "three", Contribution::RankList, &[2], None),
            1.0,
            40,
        )
        .unwrap();
    let run = registry.run(Some("q"), Duration::from_secs(5), &view).await;
    let order: Vec<(&str, u32)> = run
        .fused
        .iter()
        .map(|f| {
            let chunk = view.chunk(&f.node).unwrap();
            (chunk.relative_file_path.as_str(), chunk.start_line)
        })
        .collect();
    assert_eq!(order, [("b.rs", 1), ("same.rs", 1), ("same.rs", 9)]);
}

#[test]
fn a_duplicate_or_a_filter_map_is_refused() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let mut registry = Registry::new(60.0);
    registry
        .register(
            fixed(&view, "x", Contribution::RankList, &[0], None),
            1.0,
            40,
        )
        .unwrap();
    assert!(matches!(
        registry.register(
            fixed(&view, "x", Contribution::RankList, &[0], None),
            1.0,
            40
        ),
        Err(RegisterError::Duplicate("x"))
    ));
    assert!(matches!(
        registry.register(fixed(&view, "f", Contribution::Filter, &[0], None), 1.0, 40),
        Err(RegisterError::FilterUnsupported("f"))
    ));
}

#[tokio::test]
async fn invalid_answers_become_failures_that_name_the_map() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let id = view.ids()[0].clone();
    let bad = |hits: Vec<MapHit>| -> Box<dyn Map> {
        Box::new(FixedMap {
            descriptor: descriptor("bad", Contribution::RankList),
            hits,
        })
    };
    let hit = |node: NodeId, rank, score| MapHit {
        node,
        rank,
        score,
        evidence: vec![],
    };
    for (label, hits) in [
        ("rank zero", vec![hit(id.clone(), 0, None)]),
        (
            "twice",
            vec![hit(id.clone(), 1, None), hit(id.clone(), 2, None)],
        ),
        (
            "falling rank",
            vec![
                hit(id.clone(), 2, None),
                hit(view.ids()[1].clone(), 1, None),
            ],
        ),
        ("infinite", vec![hit(id.clone(), 1, Some(f32::INFINITY))]),
        ("over the limit", hits(&view, &[0, 1, 2], None)),
    ] {
        let mut registry = Registry::new(60.0);
        registry.register(bad(hits), 1.0, 2).unwrap();
        let run = registry.run(Some("q"), Duration::from_secs(5), &view).await;
        assert!(run.fused.is_empty(), "{label}");
        let warnings = run.warnings();
        assert_eq!(warnings.len(), 1, "{label}");
        assert_eq!(warnings[0].0, "bad");
        assert!(
            warnings[0]
                .1
                .starts_with("map bad returned an invalid answer"),
            "{label}: {}",
            warnings[0].1
        );
    }
}

#[tokio::test]
async fn the_kit_passes_a_well_behaved_rank_list_and_prior() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let fixture = fixture(&view);
    let control = || {
        fixed(
            &view,
            "control",
            Contribution::RankList,
            &[0, 1, 2, 3],
            None,
        )
    };
    let subject = || fixed(&view, "subject", Contribution::RankList, &[3, 4], None);
    check_fail_open(&subject, &control, &fixture).await.unwrap();
    check_determinism(&subject, &control, &fixture)
        .await
        .unwrap();
    let prior = || {
        fixed(
            &view,
            "prior.subject",
            Contribution::Prior,
            &[0, 2],
            Some(0.004),
        )
    };
    check_fail_open(&prior, &control, &fixture).await.unwrap();
    check_determinism(&prior, &control, &fixture).await.unwrap();
}

/// Known-bad for C1: blocks its thread past the deadline, so the
/// registry's timeout cannot stop it and the search stalls.
struct Blocking(MapDescriptor);

#[async_trait::async_trait]
impl Map for Blocking {
    fn describe(&self) -> &MapDescriptor {
        &self.0
    }

    async fn query(&self, request: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let wait = request
            .budget
            .deadline
            .saturating_duration_since(Instant::now());
        std::thread::sleep(wait * 4);
        MapResponse::empty(&self.0, view, MapStatus::Ok)
    }
}

/// Known-bad for C6: its answer depends on how often it was asked.
struct Drifting(MapDescriptor, AtomicU32, Vec<NodeId>);

#[async_trait::async_trait]
impl Map for Drifting {
    fn describe(&self) -> &MapDescriptor {
        &self.0
    }

    async fn query(&self, _: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        let call = self.1.fetch_add(1, AtomicOrdering::SeqCst) as usize;
        let mut response = MapResponse::empty(&self.0, view, MapStatus::Ok);
        response.hits.push(MapHit {
            node: self.2[call % self.2.len()].clone(),
            rank: 1,
            score: None,
            evidence: vec![],
        });
        response
    }
}

#[tokio::test]
async fn the_kit_rejects_a_blocking_map_under_c1() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let fixture = fixture(&view);
    let control = || fixed(&view, "control", Contribution::RankList, &[0], None);
    let subject =
        || -> Box<dyn Map> { Box::new(Blocking(descriptor("blocking", Contribution::RankList))) };
    let failure: ContractFailure = check_fail_open(&subject, &control, &fixture)
        .await
        .unwrap_err();
    assert_eq!(failure.contract, "C1", "{failure:?}");
    assert!(failure.detail.contains("blocking"), "{failure:?}");
}

#[tokio::test]
async fn the_kit_rejects_a_drifting_map_under_c6() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let fixture = fixture(&view);
    let control = || fixed(&view, "control", Contribution::RankList, &[0], None);
    let counter = std::sync::Arc::new(AtomicU32::new(0));
    let subject = || -> Box<dyn Map> {
        // Every instance shares the counter, as a map with hidden global
        // state would.
        let start = counter.fetch_add(1, AtomicOrdering::SeqCst);
        Box::new(Drifting(
            descriptor("drifting", Contribution::RankList),
            AtomicU32::new(start),
            view.ids().to_vec(),
        ))
    };
    // The same map passes C1: fail-open does not depend on its answers.
    check_fail_open(&subject, &control, &fixture).await.unwrap();
    let failure = check_determinism(&subject, &control, &fixture)
        .await
        .unwrap_err();
    assert_eq!(failure.contract, "C6", "{failure:?}");
}

/// Known-bad for the real-failure half of C1: hides its own error behind an
/// empty `Ok` answer, so nothing tells the caller a channel was lost.
struct Swallowing(MapDescriptor);

#[async_trait::async_trait]
impl Map for Swallowing {
    fn describe(&self) -> &MapDescriptor {
        &self.0
    }

    async fn query(&self, _: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        MapResponse::empty(&self.0, view, MapStatus::Ok)
    }
}

/// A map that reports its real failure as it should.
struct Honest(MapDescriptor);

#[async_trait::async_trait]
impl Map for Honest {
    fn describe(&self) -> &MapDescriptor {
        &self.0
    }

    async fn query(&self, _: &MapRequest<'_>, view: &SnapshotView<'_>) -> MapResponse {
        MapResponse::empty(
            &self.0,
            view,
            MapStatus::Failed {
                warning: "honest: service refused".into(),
            },
        )
    }
}

#[tokio::test]
async fn the_kit_rejects_a_map_that_swallows_its_failure() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let fixture = fixture(&view);
    let control = || fixed(&view, "control", Contribution::RankList, &[0, 1], None);
    let honest =
        || -> Box<dyn Map> { Box::new(Honest(descriptor("honest", Contribution::RankList))) };
    check_real_failure(&honest, &control, &fixture)
        .await
        .unwrap();
    let swallowing = || -> Box<dyn Map> {
        Box::new(Swallowing(descriptor("swallowing", Contribution::RankList)))
    };
    // The injected faults never call the map, so it passes that half ...
    check_fail_open(&swallowing, &control, &fixture)
        .await
        .unwrap();
    // ... and fails the real-failure half.
    let failure = check_real_failure(&swallowing, &control, &fixture)
        .await
        .unwrap_err();
    assert_eq!(failure.contract, "C1", "{failure:?}");
}

#[test]
fn set_enabled_reports_an_unknown_id() {
    let chunks = universe();
    let view = SnapshotView::new(SnapshotRef::default(), chunks.iter().collect());
    let mut registry = Registry::new(60.0);
    registry
        .register(
            fixed(&view, "x", Contribution::RankList, &[0], None),
            1.0,
            40,
        )
        .unwrap();
    assert!(registry.set_enabled("x", false));
    assert!(!registry.set_enabled("typo", false));
}
