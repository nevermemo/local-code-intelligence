//! Spike test T0 (map-trait.md, "Bit-exact legacy fusion"): the existing
//! fusion unit cases run through the map registry with the wrapped legacy
//! channels and the source-role prior give every chunk the `fusion_score`
//! of `fuse_hits`, equal by `to_bits()`, in the same fused order; and the
//! contract kit's C1 and C6 pass for every wrapped map.

use super::*;
use crate::{
    app::search::legacy_maps::{
        Channel, LexicalMap, RecordedChannel, SourceRolePrior, legacy_registry, lexical_descriptor,
        source_role_descriptor,
    },
    config::{ChannelsConfig, Config},
    workspace::Workspace,
};
use lci_maps::{
    Map, SnapshotRef, SnapshotView,
    kit::{Fixture, check_determinism, check_fail_open, check_real_failure},
};
use std::time::Duration;

/// The distinct chunks of the three lists, first seen first: the universe
/// the registry's view is built from.
fn universe(lists: [&[Hit]; 3]) -> Vec<Hit> {
    let mut seen = HashSet::new();
    lists
        .into_iter()
        .flatten()
        .filter(|hit| {
            seen.insert((
                hit.chunk.relative_file_path.clone(),
                hit.chunk.start_line,
                hit.chunk.end_line,
                hit.chunk.content_hash.clone(),
            ))
        })
        .cloned()
        .collect()
}

/// Fuses the lists with `fuse_hits` and with the registry, and checks that
/// both give the same chunks in the same order with the same score bits and
/// the same per-channel ranks.
fn assert_registry_matches_fuse_hits(
    label: &str,
    rrf_k: f32,
    weights: &FusionConfig,
    semantic: Vec<Hit>,
    lexical: Vec<Hit>,
    lsp: Vec<Hit>,
) {
    let universe = universe([&semantic, &lexical, &lsp]);
    let mut legacy = App::fuse_hits(
        rrf_k,
        weights,
        semantic.clone(),
        lexical.clone(),
        lsp.clone(),
    );
    legacy.sort_by(fusion_order);

    let config = Config {
        rrf_k,
        fusion: *weights,
        ..Config::default()
    };
    let view = SnapshotView::new(
        SnapshotRef::default(),
        universe.iter().map(|hit| &hit.chunk).collect(),
    );
    let registry = legacy_registry(
        &config,
        ChannelsConfig::default(),
        Box::new(RecordedChannel::new(Channel::Semantic, semantic)),
        Box::new(RecordedChannel::new(Channel::Lexical, lexical)),
        Box::new(RecordedChannel::new(Channel::Lsp, lsp)),
        Box::new(SourceRolePrior {
            universe: &universe,
            descriptor: source_role_descriptor(),
        }),
    );
    let run = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
        .block_on(registry.run(Some("query"), Duration::from_secs(30), &view));
    assert!(
        run.warnings().is_empty() && run.notes().is_empty(),
        "{label}"
    );
    assert_eq!(run.fused.len(), legacy.len(), "{label}: pool size");
    for (position, (fused, hit)) in run.fused.iter().zip(&legacy).enumerate() {
        let chunk = view.chunk(&fused.node).unwrap();
        assert_eq!(
            (
                &chunk.relative_file_path,
                chunk.start_line,
                chunk.end_line,
                &chunk.content_hash
            ),
            (
                &hit.chunk.relative_file_path,
                hit.chunk.start_line,
                hit.chunk.end_line,
                &hit.chunk.content_hash
            ),
            "{label}: chunk at position {position}"
        );
        assert_eq!(
            fused.score.to_bits(),
            hit.fusion_score.to_bits(),
            "{label}: {} at position {position}: {} vs {}",
            hit.chunk.relative_file_path,
            fused.score,
            hit.fusion_score
        );
        let rank = |map: &str| {
            fused
                .parts
                .iter()
                .find(|part| part.map == map)
                .and_then(|part| part.rank)
                .map(|rank| rank as usize)
        };
        assert_eq!(rank("vec.chunk.code"), hit.semantic_rank, "{label}");
        assert_eq!(rank("lexical"), hit.lexical_rank, "{label}");
        assert_eq!(rank("lsp.symbols"), hit.lsp_rank, "{label}");
    }
}

/// Every row set and weighting of the existing fusion unit cases
/// (`unit_weights_match_plain_rrf`, `semantic_weight_two_...`,
/// `prior_is_added_after_weighting`,
/// `traced_ranks_weights_and_prior_reproduce_every_fusion_score_bit_for_bit`).
#[test]
fn the_fusion_unit_cases_fuse_bit_for_bit_through_the_registry() {
    use SourceRole::{Source, Test};
    let eight = [
        row("a.rs", Source, Some(1), Some(3), None),
        row("b.rs", Source, Some(2), None, None),
        row("c.rs", Source, None, Some(1), None),
        row("d.rs", Source, Some(3), Some(2), Some(1)),
        row("e.rs", Source, None, None, Some(2)),
        row("tests/f.rs", Test, Some(4), None, Some(3)),
        row("g.rs", Source, Some(5), Some(7), None),
        row("h.rs", Source, None, Some(4), Some(4)),
    ];
    let three = [
        row("x.rs", Source, Some(1), None, None),
        row("y.rs", Source, None, Some(1), Some(3)),
        row("z.rs", Source, Some(2), Some(2), None),
    ];
    let prior = [
        row("src.rs", Source, Some(1), None, None),
        row("tests/t.rs", Test, Some(1), None, None),
        row("all.rs", Source, Some(1), Some(1), Some(1)),
    ];
    let mixed = FusionConfig {
        semantic_weight: 3.0,
        lexical_weight: 0.5,
        lsp_weight: 4.0,
        max_chunks_per_file: 0,
    };
    let doubled = FusionConfig {
        semantic_weight: 2.0,
        ..FusionConfig::UNIT
    };
    let mut cases = 0;
    for (name, rows) in [("eight", &eight[..]), ("three", &three), ("prior", &prior)] {
        for rrf_k in [RRF_K, 7.5, 1.0] {
            for weights in [FusionConfig::UNIT, FusionConfig::default(), doubled, mixed] {
                let (semantic, lexical, lsp) = channels(rows);
                assert_registry_matches_fuse_hits(
                    &format!("{name} rrf_k {rrf_k} {weights:?}"),
                    rrf_k,
                    &weights,
                    semantic,
                    lexical,
                    lsp,
                );
                cases += 1;
            }
        }
    }
    assert_eq!(cases, 36);
}

/// `lexical_fields_copied_on_merge` and
/// `equal_score_chunks_with_the_same_path_and_start_line_have_a_stable_order`.
#[test]
fn the_merge_and_tie_cases_fuse_bit_for_bit_through_the_registry() {
    use SourceRole::Source;
    let base = hit("merged.rs", 1, Source);
    let mut lexical_hit = lexical_at(base.clone(), 2);
    lexical_hit.lexical_match_count = 7;
    lexical_hit.lexical_score = Some(3.5);
    assert_registry_matches_fuse_hits(
        "merged",
        RRF_K,
        &FusionConfig::UNIT,
        vec![semantic_at(base.clone(), 4)],
        vec![lexical_hit],
        vec![lsp_at(base, 5)],
    );
    assert_registry_matches_fuse_hits(
        "alone",
        RRF_K,
        &FusionConfig::UNIT,
        vec![],
        vec![lexical_at(hit("alone.rs", 1, Source), 1)],
        vec![],
    );
    let mut semantic = semantic_at(hit("same.rs", 1, Source), 1);
    semantic.chunk.end_line = 3;
    semantic.chunk.content_hash = "z-semantic".into();
    let mut lexical = lexical_at(hit("same.rs", 1, Source), 1);
    lexical.chunk.end_line = 2;
    lexical.chunk.content_hash = "a-lexical".into();
    // Equal scores where the NodeId string order (hash `a..` first) is the
    // opposite of the legacy order (start line 1 first).
    let mut first = semantic_at(hit("tie.rs", 1, Source), 1);
    first.chunk.content_hash = "z-line-1".into();
    let mut ninth = lexical_at(hit("tie.rs", 9, Source), 1);
    ninth.chunk.content_hash = "a-line-9".into();
    assert_registry_matches_fuse_hits(
        "equal scores against the NodeId order",
        RRF_K,
        &FusionConfig::UNIT,
        vec![first],
        vec![ninth],
        vec![],
    );
    assert_registry_matches_fuse_hits(
        "equal scores",
        RRF_K,
        &FusionConfig::UNIT,
        vec![semantic],
        vec![lexical],
        vec![],
    );
}

#[tokio::test]
async fn every_wrapped_map_passes_contracts_c1_and_c6() {
    use SourceRole::{Source, Test};
    let rows = [
        row("a.rs", Source, Some(1), Some(3), None),
        row("b.rs", Source, Some(2), None, Some(1)),
        row("tests/c.rs", Test, None, Some(1), Some(2)),
        row("d.rs", Source, Some(3), Some(2), None),
    ];
    let (semantic, lexical, lsp) = channels(&rows);
    let universe = universe([&semantic, &lexical, &lsp]);
    let view = SnapshotView::new(
        SnapshotRef::default(),
        universe.iter().map(|hit| &hit.chunk).collect(),
    );
    let fixture = Fixture {
        view: &view,
        text: "query",
        budget: Duration::from_millis(500),
        slack: Duration::from_millis(1000),
        rrf_k: RRF_K,
    };
    let recorded = |channel: Channel, hits: &Vec<Hit>| {
        let hits = hits.clone();
        move || -> Box<dyn Map> { Box::new(RecordedChannel::new(channel, hits.clone())) }
    };
    let semantic_map = recorded(Channel::Semantic, &semantic);
    let lexical_map = recorded(Channel::Lexical, &lexical);
    let lsp_map = recorded(Channel::Lsp, &lsp);
    let prior_map = || -> Box<dyn Map + '_> {
        Box::new(SourceRolePrior {
            universe: &universe,
            descriptor: source_role_descriptor(),
        })
    };
    for (subject, control) in [
        (
            &semantic_map as &dyn Fn() -> Box<dyn Map>,
            &lexical_map as &dyn Fn() -> Box<dyn Map>,
        ),
        (&lexical_map, &semantic_map),
        (&lsp_map, &semantic_map),
    ] {
        check_fail_open(subject, control, &fixture).await.unwrap();
        check_determinism(subject, control, &fixture).await.unwrap();
    }
    check_fail_open(&prior_map, &semantic_map, &fixture)
        .await
        .unwrap();
    check_determinism(&prior_map, &semantic_map, &fixture)
        .await
        .unwrap();
}

/// The live lexical map (ripgrep over a real directory, mapped onto its
/// chunks) passes C1 and C6, and with ripgrep missing it fails open with
/// today's warning text.
#[tokio::test]
async fn the_live_lexical_map_passes_c1_and_c6_and_fails_open_without_ripgrep() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/total.rs"),
        "pub fn calculator_total(values: &[i32]) -> i32 {\n    values.iter().sum()\n}\n\npub fn other() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/calculator.rs"),
        "pub struct Calculator;\n\nimpl Calculator {\n    pub fn add(a: i32, b: i32) -> i32 { a + b }\n}\n",
    )
    .unwrap();
    let workspace = Workspace {
        id: "t0".into(),
        path: dunce::canonicalize(dir.path()).unwrap(),
    };
    let universe: Vec<Hit> = crate::chunk::scan(&workspace.path)
        .unwrap()
        .into_iter()
        .map(|chunk| {
            let mut base = hit(
                &chunk.relative_file_path,
                chunk.start_line,
                SourceRole::Source,
            );
            base.chunk = chunk;
            base
        })
        .collect();
    let view = SnapshotView::new(
        SnapshotRef::default(),
        universe.iter().map(|hit| &hit.chunk).collect(),
    );
    let fixture = Fixture {
        view: &view,
        text: "calculator total",
        budget: Duration::from_secs(5),
        slack: Duration::from_secs(5),
        rrf_k: RRF_K,
    };
    let config = Config::default();
    let lexical = || -> Box<dyn Map + '_> {
        Box::new(LexicalMap {
            config: &config,
            workspace: &workspace,
            universe: &universe,
            descriptor: lexical_descriptor(),
        })
    };
    let control_hits: Vec<Hit> = universe
        .iter()
        .take(2)
        .enumerate()
        .map(|(rank, hit)| semantic_at(hit.clone(), rank + 1))
        .collect();
    let control = || -> Box<dyn Map> {
        Box::new(RecordedChannel::new(
            Channel::Semantic,
            control_hits.clone(),
        ))
    };
    check_fail_open(&lexical, &control, &fixture).await.unwrap();
    check_determinism(&lexical, &control, &fixture)
        .await
        .unwrap();

    // It found something, so the comparisons above were not vacuous.
    let request_params = serde_json::Value::Null;
    let request = lci_maps::MapRequest {
        text: Some("calculator total"),
        seeds: &[],
        limit: 40,
        budget: lci_maps::Budget {
            deadline: std::time::Instant::now() + Duration::from_secs(20),
            max_external_calls: 1,
            max_nodes_touched: 100,
        },
        params: &request_params,
        as_of: None,
    };
    let answer = lexical().query(&request, &view).await;
    assert_eq!(answer.status, lci_maps::MapStatus::Ok);
    assert!(!answer.hits.is_empty());
    assert_eq!(answer.freshness.observed, lci_maps::Observed::LiveTree);

    let missing = Config {
        ripgrep_path: "definitely-missing-ripgrep".into(),
        ..Config::default()
    };
    let broken = || -> Box<dyn Map + '_> {
        Box::new(LexicalMap {
            config: &missing,
            workspace: &workspace,
            universe: &universe,
            descriptor: lexical_descriptor(),
        })
    };
    check_real_failure(&broken, &control, &fixture)
        .await
        .unwrap();
    let answer = broken().query(&request, &view).await;
    let lci_maps::MapStatus::Failed { warning } = answer.status else {
        panic!("{:?}", answer.status);
    };
    assert!(
        warning.starts_with("Lexical retrieval unavailable: "),
        "{warning}"
    );
    assert!(answer.hits.is_empty());
}

/// Review finding: a provider that answered some symbol queries and then
/// failed keeps its hits (as `search_once` fuses them), with the warning.
#[test]
fn an_lsp_answer_with_hits_is_partial_even_when_every_provider_failed() {
    use crate::app::search::legacy_maps::lsp_status;
    use lci_maps::MapStatus;
    let warning = || Some("Optional LSP provider unavailable: x".to_owned());
    assert_eq!(lsp_status(None, None, false, false), MapStatus::Ok);
    assert!(matches!(
        lsp_status(None, warning(), true, true),
        MapStatus::Failed { .. }
    ));
    assert_eq!(
        lsp_status(None, warning(), true, false),
        MapStatus::Partial {
            note: None,
            warning: warning()
        }
    );
    assert!(matches!(
        lsp_status(None, warning(), false, true),
        MapStatus::Partial { .. }
    ));
    assert!(matches!(
        lsp_status(Some("slow".into()), None, false, true),
        MapStatus::Partial {
            note: Some(_),
            warning: None
        }
    ));
}
