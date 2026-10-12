use super::*;
use crate::app::{CandidateTrace, RerankOutcome};
use crate::config::{ChannelsConfig, FusionConfig};
use crate::filter::{FilterRequest, SourceRole};
use std::collections::BTreeMap;

fn evidence_row(
    rank: usize,
    path: &str,
    role: &str,
    snippet: &str,
    score: Option<f32>,
) -> ResultEvidence {
    ResultEvidence {
        rank,
        relative_file_path: path.to_owned(),
        language: "rust".to_owned(),
        source_role: role.to_owned(),
        start_line: 1,
        end_line: 10,
        snippet: snippet.to_owned(),
        reranker_score: score,
    }
}

fn query_with(expected: &[&str]) -> EvalQuery {
    EvalQuery {
        query: "find the metric helper".to_owned(),
        workspace: "ws".to_owned(),
        top_k: None,
        filters: None,
        expected_paths: expected.iter().map(|s| (*s).to_owned()).collect(),
        preferred_paths: Vec::new(),
        disfavored_paths: Vec::new(),
        expected_roles: Vec::new(),
        required_snippet: None,
        tags: Vec::new(),
    }
}

fn timings(total_ms: f64) -> EvalTimings {
    EvalTimings {
        query_embedding_ms: 0.0,
        lancedb_search_ms: 0.0,
        lexical_search_ms: 0.0,
        lsp_search_ms: 0.0,
        fusion_ms: 0.0,
        reranking_ms: 0.0,
        total_ms,
    }
}

#[test]
fn pattern_matching_rules() {
    // Exact match.
    assert!(pattern_matches("src/metric.rs", "src/metric.rs"));
    // Glob `*` within a component.
    assert!(pattern_matches("src/metric*.rs", "src/metric.rs"));
    assert!(pattern_matches("src/metric*.rs", "src/metrics.rs"));
    assert!(!pattern_matches("src/metric*.rs", "src/metric.rs.bak"));
    // `?` matches exactly one character.
    assert!(pattern_matches("src/metric?.rs", "src/metric1.rs"));
    assert!(!pattern_matches("src/metric?.rs", "src/metric12.rs"));
    // `**` matches any run of whole components.
    assert!(pattern_matches("src/**/metric.rs", "src/metric.rs"));
    assert!(pattern_matches("src/**/metric.rs", "src/a/b/metric.rs"));
    assert!(!pattern_matches("src/**/metric.rs", "src/metric.rs.bak"));
    // Directory trees use `/**`.
    assert!(pattern_matches("src/metrics/**", "src/metrics/latency.rs"));
    assert!(!pattern_matches(
        "src/metrics/**",
        "src/metricsx/latency.rs"
    ));
    // Component count must line up.
    assert!(!pattern_matches("metric.rs", "src/metric.rs"));
    assert!(!pattern_matches("src/metric.rs", "metric.rs"));
    // Empty patterns match nothing.
    assert!(!pattern_matches("", "src/metric.rs"));
    assert!(!pattern_matches("metric.rs", ""));
    // Character class.
    assert!(pattern_matches("src/metric[0-9].rs", "src/metric1.rs"));
    assert!(!pattern_matches("src/metric[0-9].rs", "src/metrica.rs"));
}

#[test]
fn rank_one_hit_scores_perfectly() {
    let query = query_with(&["src/metric.rs"]);
    let evidence = vec![
        evidence_row(1, "src/metric.rs", "source", "fn metric() {}", Some(0.9)),
        evidence_row(2, "src/other.rs", "source", "fn other() {}", Some(0.5)),
    ];
    let metrics = query_metrics(&query, &evidence, true);
    assert!(metrics.hit);
    assert!(metrics.hit_at_1);
    assert!(metrics.hit_at_3);
    assert!(metrics.hit_at_8_or_window);
    assert_eq!(metrics.window, 2);
    assert_eq!(metrics.first_expected_rank, Some(1));
    assert_eq!(metrics.reciprocal_rank, 1.0);
    assert_eq!(metrics.reranker, RerankerState::Reranked);
}

#[test]
fn no_expected_match_is_a_miss() {
    let mut query = query_with(&["src/missing.rs"]);
    query.disfavored_paths = vec!["src/noise.rs".to_owned()];
    let evidence = vec![
        evidence_row(1, "src/noise.rs", "source", "fn noise() {}", None),
        evidence_row(2, "src/other.rs", "source", "fn other() {}", None),
    ];
    let metrics = query_metrics(&query, &evidence, false);
    assert!(!metrics.hit);
    assert!(!metrics.hit_at_1);
    assert!(!metrics.hit_at_3);
    assert!(!metrics.hit_at_8_or_window);
    assert_eq!(metrics.first_expected_rank, None);
    assert_eq!(metrics.reciprocal_rank, 0.0);
    // No expected hit: the whole evidence is counted for disfavored.
    assert_eq!(metrics.disfavored_before_first_expected, 1);
    assert_eq!(metrics.reranker, RerankerState::NotReranked);
    assert_eq!(metrics.expected_roles_satisfied, None);
    assert_eq!(metrics.required_snippet_found, None);
}

#[test]
fn duplicate_patterns_match_one_hit() {
    let mut query = query_with(&["src/metric.rs", "metric.rs", "src/metrics/"]);
    query.preferred_paths = vec!["metric.rs".to_owned(), "src/metric.rs".to_owned()];
    let evidence = vec![
        evidence_row(1, "src/other.rs", "source", "fn other() {}", None),
        evidence_row(2, "src/metric.rs", "source", "fn metric() {}", None),
        evidence_row(
            3,
            "src/metrics/latency.rs",
            "source",
            "fn latency() {}",
            None,
        ),
    ];
    let metrics = query_metrics(&query, &evidence, false);
    // Three patterns all match rank 2; it is still one hit at rank 2.
    assert_eq!(metrics.first_expected_rank, Some(2));
    assert!(metrics.hit);
    assert!(!metrics.hit_at_1);
    assert!(metrics.hit_at_3);
    assert_eq!(metrics.reciprocal_rank, 0.5);
    // Two preferred patterns match the same row: counted once.
    assert_eq!(metrics.preferred_count, 1);
}

#[test]
fn disfavored_counted_only_before_first_expected() {
    let mut query = query_with(&["src/metric.rs"]);
    query.disfavored_paths = vec!["src/noise.rs".to_owned()];
    let before = vec![
        evidence_row(1, "src/noise.rs", "source", "n", None),
        evidence_row(2, "src/metric.rs", "source", "m", None),
        evidence_row(3, "src/noise.rs", "source", "n", None),
    ];
    let metrics = query_metrics(&query, &before, false);
    assert_eq!(metrics.disfavored_before_first_expected, 1);

    let after = vec![
        evidence_row(1, "src/metric.rs", "source", "m", None),
        evidence_row(2, "src/noise.rs", "source", "n", None),
    ];
    let metrics = query_metrics(&query, &after, false);
    assert_eq!(metrics.disfavored_before_first_expected, 0);
}

#[test]
fn window_bounds_the_windowed_metrics() {
    let query = query_with(&["src/ninth.rs"]);
    let mut evidence: Vec<ResultEvidence> = (1..=10)
        .map(|rank| evidence_row(rank, &format!("src/row{rank}.rs"), "source", "", None))
        .collect();
    evidence[8] = evidence_row(9, "src/ninth.rs", "source", "", None);
    let metrics = query_metrics(&query, &evidence, false);
    // The hit exists but outside the 8-result window.
    assert!(metrics.hit);
    assert!(!metrics.hit_at_1);
    assert!(!metrics.hit_at_3);
    assert!(!metrics.hit_at_8_or_window);
    assert_eq!(metrics.window, 8);
    assert_eq!(metrics.first_expected_rank, Some(9));

    let small = vec![
        evidence_row(1, "src/a.rs", "source", "", None),
        evidence_row(2, "src/b.rs", "source", "", None),
        evidence_row(3, "src/c.rs", "source", "", None),
    ];
    let query = query_with(&["src/c.rs"]);
    let metrics = query_metrics(&query, &small, false);
    // Window smaller than 8: the last row still counts.
    assert!(metrics.hit_at_3);
    assert!(metrics.hit_at_8_or_window);
    assert_eq!(metrics.window, 3);
}

#[test]
fn role_distribution_counts_each_role() {
    let query = query_with(&[]);
    let evidence = vec![
        evidence_row(1, "src/a.rs", "source", "", None),
        evidence_row(2, "tests/a.rs", "test", "", None),
        evidence_row(3, "src/b.rs", "source", "", None),
    ];
    let metrics = query_metrics(&query, &evidence, false);
    assert_eq!(metrics.role_distribution["source"], 2);
    assert_eq!(metrics.role_distribution["test"], 1);
    assert_eq!(metrics.role_distribution.len(), 2);
}

#[test]
fn reranker_state_variants() {
    let query = query_with(&[]);
    assert_eq!(
        query_metrics(&query, &[], false).reranker,
        RerankerState::NoResults
    );
    let unscored = vec![evidence_row(1, "src/a.rs", "source", "", None)];
    assert_eq!(
        query_metrics(&query, &unscored, true).reranker,
        RerankerState::PartiallyScored
    );
    let partial = vec![
        evidence_row(1, "src/a.rs", "source", "", Some(0.9)),
        evidence_row(2, "src/b.rs", "source", "", None),
    ];
    assert_eq!(
        query_metrics(&query, &partial, true).reranker,
        RerankerState::PartiallyScored
    );
    let scored = vec![
        evidence_row(1, "src/a.rs", "source", "", Some(0.9)),
        evidence_row(2, "src/b.rs", "source", "", Some(0.4)),
    ];
    assert_eq!(
        query_metrics(&query, &scored, true).reranker,
        RerankerState::Reranked
    );
    assert_eq!(
        query_metrics(&query, &scored, false).reranker,
        RerankerState::NotReranked
    );
}

#[test]
fn expected_roles_and_required_snippet() {
    let mut query = query_with(&["src/metric.rs"]);
    query.expected_roles = vec!["source".to_owned()];
    query.required_snippet = Some("fn metric".to_owned());
    let hit = vec![evidence_row(
        1,
        "src/metric.rs",
        "source",
        "fn metric() {}",
        None,
    )];
    let metrics = query_metrics(&query, &hit, false);
    assert_eq!(metrics.expected_roles_satisfied, Some(true));
    assert_eq!(metrics.required_snippet_found, Some(true));

    let wrong_role = vec![evidence_row(
        1,
        "src/metric.rs",
        "test",
        "fn metric() {}",
        None,
    )];
    let metrics = query_metrics(&query, &wrong_role, false);
    assert_eq!(metrics.expected_roles_satisfied, Some(false));

    let missing_snippet = vec![evidence_row(
        1,
        "src/metric.rs",
        "source",
        "fn other() {}",
        None,
    )];
    let metrics = query_metrics(&query, &missing_snippet, false);
    assert_eq!(metrics.required_snippet_found, Some(false));

    // An absent role list is satisfied on a hit; no snippet request is unreported.
    let open = query_with(&["src/metric.rs"]);
    let metrics = query_metrics(&open, &hit, false);
    assert_eq!(metrics.expected_roles_satisfied, Some(true));
    assert_eq!(metrics.required_snippet_found, None);
}

#[test]
fn aggregate_zero_samples() {
    let report = aggregate(&[]);
    assert_eq!(report.query_count, 0);
    assert_eq!(report.executed_count, 0);
    assert_eq!(report.skipped_count, 0);
    assert_eq!(report.success_count, 0);
    assert_eq!(report.failure_count, 0);
    assert_eq!(report.hit_rate, None);
    assert_eq!(report.hit_at_1_rate, None);
    assert_eq!(report.hit_at_3_rate, None);
    assert_eq!(report.hit_at_8_or_window_rate, None);
    assert_eq!(report.mrr, None);
    assert_eq!(report.median_latency_ms, None);
    assert_eq!(report.p95_latency_ms, None);
    assert_eq!(report.fallback_rate, None);
    assert_eq!(report.index_created, 0);
    assert_eq!(report.index_reused, 0);
    assert_eq!(report.index_waited, 0);
}

#[test]
fn aggregate_single_sample() {
    let query = query_with(&["src/metric.rs"]);
    let evidence = vec![evidence_row(
        1,
        "src/metric.rs",
        "source",
        "fn metric() {}",
        Some(0.9),
    )];
    let report = QueryReport::new(
        query,
        evidence,
        true,
        None,
        None,
        Some(timings(12.5)),
        Some(IndexLifecycleAction::Reused),
        Some(0.0),
    );
    let aggregate = aggregate(std::slice::from_ref(&report));
    assert_eq!(aggregate.query_count, 1);
    assert_eq!(aggregate.executed_count, 1);
    assert_eq!(aggregate.success_count, 1);
    assert_eq!(aggregate.failure_count, 0);
    assert_eq!(aggregate.hit_queries, 1);
    assert_eq!(aggregate.hit_rate, Some(1.0));
    assert_eq!(aggregate.mrr, Some(1.0));
    assert_eq!(aggregate.median_latency_ms, Some(12.5));
    assert_eq!(aggregate.p95_latency_ms, Some(12.5));
    assert_eq!(aggregate.fallback_queries, 0);
    assert_eq!(aggregate.fallback_rate, Some(0.0));
    assert_eq!(aggregate.index_reused, 1);
}

#[test]
fn aggregate_median_even_and_odd() {
    let reports: Vec<QueryReport> = [10.0, 20.0, 30.0, 40.0]
        .into_iter()
        .map(|total| {
            let query = query_with(&["src/missing.rs"]);
            QueryReport::new(
                query,
                Vec::new(),
                false,
                None,
                None,
                Some(timings(total)),
                None,
                None,
            )
        })
        .collect();
    let even_aggregate = aggregate(&reports);
    // Even sample: mean of the two middle values.
    assert_eq!(even_aggregate.median_latency_ms, Some(25.0));
    // Nearest-rank p95 of 4 samples: ceil(0.95 * 4) = 4th value.
    assert_eq!(even_aggregate.p95_latency_ms, Some(40.0));

    let odd: Vec<QueryReport> = [10.0, 20.0, 30.0]
        .into_iter()
        .map(|total| {
            let query = query_with(&["src/missing.rs"]);
            QueryReport::new(
                query,
                Vec::new(),
                false,
                None,
                None,
                Some(timings(total)),
                None,
                None,
            )
        })
        .collect();
    let aggregate = aggregate(&odd);
    // Odd sample: the middle value.
    assert_eq!(aggregate.median_latency_ms, Some(20.0));
    // Nearest-rank p95 of 3 samples: ceil(2.85) = 3rd value.
    assert_eq!(aggregate.p95_latency_ms, Some(30.0));
}

#[test]
fn aggregate_p95_nearest_rank_over_twenty() {
    let reports: Vec<QueryReport> = (1..=20)
        .map(|total| {
            let query = query_with(&["src/missing.rs"]);
            QueryReport::new(
                query,
                Vec::new(),
                false,
                None,
                None,
                Some(timings(total as f64)),
                None,
                None,
            )
        })
        .collect();
    let aggregate = aggregate(&reports);
    // Nearest-rank p95 of 20 samples: ceil(19) = 19th value.
    assert_eq!(aggregate.p95_latency_ms, Some(19.0));
    // Even sample: mean of the 10th and 11th values.
    assert_eq!(aggregate.median_latency_ms, Some(10.5));
}

#[test]
fn skipped_optional_workspace_excluded_from_denominators() {
    let skipped = QueryReport::skipped(
        query_with(&["src/metric.rs"]),
        "optional workspace not provided".to_owned(),
    );
    let query = query_with(&["src/metric.rs"]);
    let evidence = vec![evidence_row(
        1,
        "src/metric.rs",
        "source",
        "fn metric() {}",
        None,
    )];
    let hit = QueryReport::new(
        query,
        evidence,
        false,
        None,
        None,
        Some(timings(5.0)),
        None,
        None,
    );
    let aggregate = aggregate(&[skipped, hit]);
    assert_eq!(aggregate.query_count, 2);
    assert_eq!(aggregate.skipped_count, 1);
    assert_eq!(aggregate.executed_count, 1);
    assert_eq!(aggregate.success_count, 1);
    assert_eq!(aggregate.failure_count, 0);
    assert_eq!(aggregate.hit_queries, 1);
    assert_eq!(aggregate.hit_rate, Some(1.0));
    assert_eq!(aggregate.mrr, Some(1.0));
    assert_eq!(aggregate.median_latency_ms, Some(5.0));
}

#[test]
fn lifecycle_counts_and_fallback_rate() {
    let make = |action: IndexLifecycleAction, warning: Option<&str>| {
        let query = query_with(&["src/missing.rs"]);
        QueryReport::new(
            query,
            Vec::new(),
            false,
            warning.map(str::to_owned),
            None,
            None,
            Some(action),
            None,
        )
    };
    let reports = vec![
        make(
            IndexLifecycleAction::Created,
            Some("query embedding unavailable"),
        ),
        make(IndexLifecycleAction::Reused, None),
        make(IndexLifecycleAction::WaitedForExistingJob, None),
        make(
            IndexLifecycleAction::Reused,
            Some("query embedding unavailable"),
        ),
    ];
    let aggregate = aggregate(&reports);
    assert_eq!(aggregate.index_created, 1);
    assert_eq!(aggregate.index_reused, 2);
    assert_eq!(aggregate.index_waited, 1);
    assert_eq!(aggregate.fallback_queries, 2);
    assert_eq!(aggregate.fallback_rate, Some(0.5));
    assert_eq!(aggregate.hit_rate, Some(0.0));
}

#[test]
fn evaluation_file_round_trips_through_serde() {
    let file = EvaluationFile {
        workspaces: vec![
            EvalWorkspace {
                key: "core".to_owned(),
                required: true,
            },
            EvalWorkspace {
                key: "web".to_owned(),
                required: false,
            },
        ],
        queries: vec![EvalQuery {
            query: "find the metric helper".to_owned(),
            workspace: "core".to_owned(),
            top_k: Some(8),
            filters: Some(FilterRequest {
                languages: Some(vec!["rust".to_owned()]),
                include_paths: None,
                exclude_paths: Some(vec!["src/generated/**".to_owned()]),
                source_roles: Some(vec!["source".to_owned()]),
            }),
            expected_paths: vec!["src/metric.rs".to_owned()],
            preferred_paths: vec![],
            disfavored_paths: vec!["src/noise.rs".to_owned()],
            expected_roles: vec!["source".to_owned()],
            required_snippet: Some("fn metric".to_owned()),
            tags: vec!["keyword".to_owned()],
        }],
    };
    let json = serde_json::to_string(&file).unwrap();
    let back: EvaluationFile = serde_json::from_str(&json).unwrap();
    // Compare via JSON since the types no longer derive PartialEq.
    assert_eq!(json, serde_json::to_string(&back).unwrap());

    let query = query_with(&["src/metric.rs"]);
    let evidence = vec![evidence_row(
        1,
        "src/metric.rs",
        "source",
        "fn metric() {}",
        Some(0.9),
    )];
    let report = QueryReport::new(
        query,
        evidence,
        true,
        None,
        None,
        Some(timings(12.5)),
        Some(IndexLifecycleAction::Created),
        Some(3.0),
    );
    let aggregate = aggregate(std::slice::from_ref(&report));
    let json = serde_json::to_string(&aggregate).unwrap();
    let back: AggregateReport = serde_json::from_str(&json).unwrap();
    assert_eq!(json, serde_json::to_string(&back).unwrap());
}

#[test]
fn bounded_snippet_truncates_long_code() {
    let short = "fn metric() {}";
    assert_eq!(bounded_snippet(short), short);

    let long = "let x = 1; ".repeat(100);
    let snippet = bounded_snippet(&long);
    assert!(snippet.ends_with('…'));
    assert!(snippet.chars().count() <= SNIPPET_BUDGET + 1);
    assert!(snippet.starts_with(&long[..100]));
}

// ---------------------------------------------------------------------------
// Rank-aware metrics: nDCG@10, recall@k, file accuracy@k
// ---------------------------------------------------------------------------

fn rows(paths: &[&str]) -> Vec<ResultEvidence> {
    paths
        .iter()
        .enumerate()
        .map(|(index, path)| evidence_row(index + 1, path, "source", "", None))
        .collect()
}

fn assert_close(actual: f64, expected: f64) {
    assert!(
        (actual - expected).abs() < 1e-9,
        "expected {expected}, got {actual}"
    );
}

#[test]
fn ndcg_mixed_expected_and_preferred_is_hand_computed() {
    let mut query = query_with(&["src/target.rs"]);
    query.preferred_paths = vec!["src/helper.rs".to_owned()];
    // Gains by rank: noise 0, helper (preferred) 1, target (expected) 2.
    let metrics = query_metrics(
        &query,
        &rows(&["src/noise.rs", "src/helper.rs", "src/target.rs"]),
        false,
    );
    // DCG  = 1/log2(3) + 2/log2(4) = 1.6309297535714575
    // IDCG = 2/log2(2) + 1/log2(3) = 2.6309297535714578
    // nDCG = 0.6199062332840657
    assert_close(metrics.ndcg_at_10, 0.619_906_233_284_065_7);

    // Ideal order: expected first, then preferred.
    let ideal = query_metrics(
        &query,
        &rows(&["src/target.rs", "src/helper.rs", "src/noise.rs"]),
        false,
    );
    assert_close(ideal.ndcg_at_10, 1.0);

    // Two expected + one preferred, ranked preferred/expected/noise/expected:
    // DCG  = 1/log2(2) + 2/log2(3) + 2/log2(5) = 3.1232126232897013
    // IDCG = 2/log2(2) + 2/log2(3) + 1/log2(4) = 3.761859507142915
    let mut query = query_with(&["src/a.rs", "src/b.rs"]);
    query.preferred_paths = vec!["src/c.rs".to_owned()];
    let metrics = query_metrics(
        &query,
        &rows(&["src/c.rs", "src/a.rs", "src/noise.rs", "src/b.rs"]),
        false,
    );
    assert_close(metrics.ndcg_at_10, 0.830_231_064_546_517_8);
}

#[test]
fn ndcg_credits_each_gold_pattern_once_and_stops_at_rank_ten() {
    // A second chunk of the same file must not add gain, or nDCG could
    // exceed 1.
    let query = query_with(&["src/target.rs"]);
    let metrics = query_metrics(
        &query,
        &rows(&["src/target.rs", "src/target.rs", "src/target.rs"]),
        false,
    );
    assert_close(metrics.ndcg_at_10, 1.0);

    // One glob pattern is one gold slot however many files match it.
    let query = query_with(&["src/metrics/**"]);
    let metrics = query_metrics(
        &query,
        &rows(&["src/metrics/a.rs", "src/metrics/b.rs"]),
        false,
    );
    assert_close(metrics.ndcg_at_10, 1.0);

    // A hit at rank 11 is outside the nDCG@10 window.
    let query = query_with(&["src/late.rs"]);
    let mut paths: Vec<String> = (1..=10).map(|n| format!("src/row{n}.rs")).collect();
    paths.push("src/late.rs".to_owned());
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    let metrics = query_metrics(&query, &rows(&paths), false);
    assert!(metrics.hit);
    assert_close(metrics.ndcg_at_10, 0.0);
}

#[test]
fn ndcg_does_not_credit_duplicate_chunks_for_overlapping_expected_patterns() {
    let query = query_with(&["src/a.rs", "src/**"]);
    let single = query_metrics(&query, &rows(&["src/a.rs"]), false);
    let duplicate = query_metrics(&query, &rows(&["src/a.rs", "src/a.rs"]), false);

    assert_close(single.ndcg_at_10, duplicate.ndcg_at_10);
}

#[test]
fn recall_counts_distinct_expected_patterns_within_k() {
    let query = query_with(&["src/a.rs", "src/b.rs"]);

    // Only one of the two expected patterns is ever found.
    let one_found = query_metrics(&query, &rows(&["src/x.rs", "src/a.rs", "src/y.rs"]), false);
    assert_close(one_found.recall_at_3, 0.5);
    assert_close(one_found.recall_at_8, 0.5);

    // The second pattern arrives at rank 5: outside @3, inside @8.
    let late = query_metrics(
        &query,
        &rows(&["src/a.rs", "src/x.rs", "src/y.rs", "src/z.rs", "src/b.rs"]),
        false,
    );
    assert_close(late.recall_at_3, 0.5);
    assert_close(late.recall_at_8, 1.0);

    // Duplicate pattern strings are one gold item, not two.
    let query = query_with(&["src/a.rs", "src/a.rs"]);
    let metrics = query_metrics(&query, &rows(&["src/a.rs"]), false);
    assert_close(metrics.recall_at_3, 1.0);
}

#[test]
fn file_accuracy_collapses_repeated_chunks_of_one_file() {
    let query = query_with(&["src/target.rs"]);

    // Chunk rank 8, but only the fifth distinct file: a.rs, b.rs, c.rs and
    // d.rs occupy one position each however many chunks they contribute.
    let collapsed = query_metrics(
        &query,
        &rows(&[
            "src/a.rs",
            "src/a.rs",
            "src/a.rs",
            "src/b.rs",
            "src/b.rs",
            "src/c.rs",
            "src/d.rs",
            "src/target.rs",
        ]),
        false,
    );
    assert_eq!(collapsed.first_expected_rank, Some(8));
    assert_close(collapsed.file_acc_at_5, 1.0);
    assert_close(collapsed.file_acc_at_10, 1.0);

    // Sixth distinct file: inside @10 but not @5.
    let sixth = query_metrics(
        &query,
        &rows(&[
            "src/a.rs",
            "src/a.rs",
            "src/b.rs",
            "src/c.rs",
            "src/d.rs",
            "src/e.rs",
            "src/target.rs",
        ]),
        false,
    );
    assert_close(sixth.file_acc_at_5, 0.0);
    assert_close(sixth.file_acc_at_10, 1.0);

    // Eleventh distinct file: outside both.
    let mut paths: Vec<String> = (1..=10).map(|n| format!("src/row{n}.rs")).collect();
    paths.push("src/target.rs".to_owned());
    let paths: Vec<&str> = paths.iter().map(String::as_str).collect();
    let far = query_metrics(&query, &rows(&paths), false);
    assert_close(far.file_acc_at_5, 0.0);
    assert_close(far.file_acc_at_10, 0.0);
}

#[test]
fn rank_aware_metrics_are_zero_on_an_all_miss() {
    let mut query = query_with(&["src/missing.rs"]);
    query.preferred_paths = vec!["src/also_missing.rs".to_owned()];
    let metrics = query_metrics(&query, &rows(&["src/a.rs", "src/b.rs", "src/c.rs"]), false);
    assert!(!metrics.hit);
    assert_close(metrics.ndcg_at_10, 0.0);
    assert_close(metrics.recall_at_3, 0.0);
    assert_close(metrics.recall_at_8, 0.0);
    assert_close(metrics.file_acc_at_5, 0.0);
    assert_close(metrics.file_acc_at_10, 0.0);

    // No results at all is also a miss.
    let empty = query_metrics(&query, &[], false);
    assert_close(empty.ndcg_at_10, 0.0);
    assert_close(empty.recall_at_8, 0.0);
    assert_close(empty.file_acc_at_10, 0.0);
}

#[test]
fn rank_aware_metrics_are_zero_without_gold() {
    let query = query_with(&[]);
    let metrics = query_metrics(&query, &rows(&["src/a.rs", "src/b.rs"]), false);
    assert_close(metrics.ndcg_at_10, 0.0);
    assert_close(metrics.recall_at_3, 0.0);
    assert_close(metrics.recall_at_8, 0.0);
    assert_close(metrics.file_acc_at_5, 0.0);
    assert_close(metrics.file_acc_at_10, 0.0);
}

#[test]
fn skipped_and_failed_reports_carry_zero_rank_aware_metrics() {
    let skipped = QueryReport::skipped(query_with(&["src/a.rs"]), "optional".to_owned());
    assert_close(skipped.metrics.ndcg_at_10, 0.0);
    let failed = QueryReport::failed(query_with(&["src/a.rs"]), "search failed".to_owned());
    assert_close(failed.metrics.recall_at_8, 0.0);
    assert_close(failed.metrics.file_acc_at_10, 0.0);
}

#[test]
fn aggregate_means_cover_executed_queries_only() {
    let perfect = QueryReport::new(
        query_with(&["src/a.rs"]),
        rows(&["src/a.rs"]),
        false,
        None,
        None,
        None,
        None,
        None,
    );
    // Expected a.rs and b.rs; only a.rs is found, at rank 4.
    let partial = QueryReport::new(
        query_with(&["src/a.rs", "src/b.rs"]),
        rows(&["src/x.rs", "src/y.rs", "src/z.rs", "src/a.rs"]),
        false,
        None,
        None,
        None,
        None,
        None,
    );
    let skipped = QueryReport::skipped(query_with(&["src/a.rs"]), "optional".to_owned());
    let report = aggregate(&[perfect, partial.clone(), skipped]);

    assert_eq!(report.executed_count, 2);
    assert_eq!(report.mean_recall_at_3, Some(0.5));
    assert_eq!(report.mean_recall_at_8, Some(0.75));
    assert_eq!(report.mean_file_acc_at_5, Some(1.0));
    assert_eq!(report.mean_file_acc_at_10, Some(1.0));
    // Partial: one gain-2 hit at rank 4 against ideal slots at ranks 1 and 2.
    // DCG = 2/log2(5), IDCG = 2/log2(2) + 2/log2(3).
    let partial_ndcg = (2.0 / 5f64.log2()) / (2.0 + 2.0 / 3f64.log2());
    assert_close(partial.metrics.ndcg_at_10, partial_ndcg);
    assert_close(
        report.mean_ndcg_at_10.expect("mean ndcg"),
        (1.0 + partial_ndcg) / 2.0,
    );

    let empty = aggregate(&[]);
    assert_eq!(empty.mean_ndcg_at_10, None);
    assert_eq!(empty.mean_recall_at_3, None);
    assert_eq!(empty.mean_recall_at_8, None);
    assert_eq!(empty.mean_file_acc_at_5, None);
    assert_eq!(empty.mean_file_acc_at_10, None);
}

#[test]
fn refreshed_incrementally_is_counted_so_lifecycle_counts_sum_to_executed() {
    let make = |action: IndexLifecycleAction| {
        QueryReport::new(
            query_with(&["src/missing.rs"]),
            Vec::new(),
            false,
            None,
            None,
            None,
            Some(action),
            None,
        )
    };
    let reports = vec![
        make(IndexLifecycleAction::Created),
        make(IndexLifecycleAction::Reused),
        make(IndexLifecycleAction::WaitedForExistingJob),
        make(IndexLifecycleAction::RefreshedIncrementally),
        make(IndexLifecycleAction::RefreshedIncrementally),
    ];
    let report = aggregate(&reports);
    assert_eq!(report.index_refreshed, 2);
    assert_eq!(
        report.index_created + report.index_reused + report.index_waited + report.index_refreshed,
        report.executed_count
    );
}

#[test]
fn stage_latency_percentiles_on_a_known_set() {
    // Four queries; each stage gets its own series so a mixed-up field would
    // show. Sorted series and the expected statistics (median, nearest-rank
    // p95 = 4th value, max = 4th value, n = 4):
    //   embedding  10 20 30 40       p50 25   p95 40   max 40
    //   lancedb     1  2  3  5       p50 2.5  p95 5    max 5
    //   lexical     7  7  7  9       p50 7    p95 9    max 9
    //   lsp         0  0  0  0       p50 0    p95 0    max 0
    //   fusion      4  6  8 12       p50 7    p95 12   max 12
    //   reranking 100 200 300 400    p50 250  p95 400  max 400
    //   total     150 250 350 500    p50 300  p95 500  max 500
    let series = [
        (30.0, 5.0, 7.0, 0.0, 8.0, 400.0, 500.0),
        (10.0, 1.0, 7.0, 0.0, 4.0, 100.0, 150.0),
        (40.0, 3.0, 9.0, 0.0, 12.0, 300.0, 350.0),
        (20.0, 2.0, 7.0, 0.0, 6.0, 200.0, 250.0),
    ];
    let reports: Vec<QueryReport> = series
        .into_iter()
        .map(|(embed, lance, lexical, lsp, fusion, rerank, total)| {
            QueryReport::new(
                query_with(&["src/missing.rs"]),
                Vec::new(),
                false,
                None,
                None,
                Some(EvalTimings {
                    query_embedding_ms: embed,
                    lancedb_search_ms: lance,
                    lexical_search_ms: lexical,
                    lsp_search_ms: lsp,
                    fusion_ms: fusion,
                    reranking_ms: rerank,
                    total_ms: total,
                }),
                None,
                None,
            )
        })
        .collect();
    let report = aggregate(&reports);
    let stages = report.stage_latency.expect("stage latency");
    let check = |stage: LatencyPercentiles, p50: f64, p95: f64, max: f64| {
        assert_close(stage.p50, p50);
        assert_close(stage.p95, p95);
        assert_close(stage.max, max);
        assert_eq!(stage.n, 4);
    };
    check(stages.query_embedding_ms, 25.0, 40.0, 40.0);
    check(stages.lancedb_search_ms, 2.5, 5.0, 5.0);
    check(stages.lexical_search_ms, 7.0, 9.0, 9.0);
    check(stages.lsp_search_ms, 0.0, 0.0, 0.0);
    check(stages.fusion_ms, 7.0, 12.0, 12.0);
    check(stages.reranking_ms, 250.0, 400.0, 400.0);
    check(stages.total_ms, 300.0, 500.0, 500.0);
    // The total stage uses the same statistics as the existing fields.
    assert_eq!(report.median_latency_ms, Some(stages.total_ms.p50));
    assert_eq!(report.p95_latency_ms, Some(stages.total_ms.p95));

    // Reports without timings and an empty run have no stage latency.
    let no_timings = QueryReport::new(
        query_with(&["src/missing.rs"]),
        Vec::new(),
        false,
        None,
        None,
        None,
        None,
        None,
    );
    assert_eq!(aggregate(&[no_timings]).stage_latency, None);
    assert_eq!(aggregate(&[]).stage_latency, None);
}

fn timed_report(stage_timings: EvalTimings) -> QueryReport {
    QueryReport::new(
        query_with(&["src/missing.rs"]),
        Vec::new(),
        false,
        None,
        None,
        Some(stage_timings),
        None,
        None,
    )
}

#[test]
fn max_latency_shows_the_outlier_that_nearest_rank_p95_hides() {
    // 25 queries: 24 at 100 ms and one 30 s outlier. Nearest-rank p95 is rank
    // ceil(0.95 * 25) = ceil(23.75) = 24, the last 100, so only `max` sees it.
    let mut reports: Vec<QueryReport> = (0..24).map(|_| timed_report(timings(100.0))).collect();
    reports.push(timed_report(timings(30_000.0)));
    let report = aggregate(&reports);

    let total = report.stage_latency.expect("stage latency").total_ms;
    assert_close(total.p50, 100.0);
    assert_close(total.p95, 100.0);
    assert_close(total.max, 30_000.0);
    assert_eq!(total.n, 25);
    assert_eq!(report.p95_latency_ms, Some(100.0));

    // A stage that never varies still reports its own max and n.
    let fusion = report.stage_latency.expect("stage latency").fusion_ms;
    assert_close(fusion.max, 0.0);
    assert_eq!(fusion.n, 25);
}

#[test]
fn stage_latency_written_before_max_and_n_still_loads_with_zeros() {
    let report = aggregate(&[
        timed_report(timings(10.0)),
        timed_report(timings(30.0)),
        timed_report(timings(20.0)),
    ]);
    let mut value = serde_json::to_value(&report).unwrap();
    let stages = value["stage_latency"].as_object_mut().unwrap();
    assert_eq!(stages.len(), 7);
    for stage in stages.values_mut() {
        let stage = stage.as_object_mut().unwrap();
        assert!(stage.remove("max").is_some(), "max is serialized");
        assert!(stage.remove("n").is_some(), "n is serialized");
    }

    let back: AggregateReport = serde_json::from_value(value).unwrap();
    let total = back.stage_latency.expect("stage latency").total_ms;
    assert_close(total.p50, 20.0);
    assert_close(total.p95, 30.0);
    assert_eq!((total.max, total.n), (0.0, 0));
    assert_eq!(back.median_latency_ms, Some(20.0));
}

#[test]
fn reports_without_the_new_fields_still_deserialize() {
    let report = QueryReport::new(
        query_with(&["src/metric.rs"]),
        rows(&["src/metric.rs"]),
        false,
        None,
        None,
        Some(timings(5.0)),
        Some(IndexLifecycleAction::Reused),
        None,
    );
    let mut value = serde_json::to_value(aggregate(std::slice::from_ref(&report))).unwrap();
    let object = value.as_object_mut().unwrap();
    for key in [
        "mean_ndcg_at_10",
        "mean_recall_at_3",
        "mean_recall_at_8",
        "mean_file_acc_at_5",
        "mean_file_acc_at_10",
        "stage_latency",
        "index_refreshed",
    ] {
        assert!(object.remove(key).is_some(), "missing serialized {key}");
    }
    for query in object["queries"].as_array_mut().unwrap() {
        let metrics = query["metrics"].as_object_mut().unwrap();
        for key in [
            "ndcg_at_10",
            "recall_at_3",
            "recall_at_8",
            "file_acc_at_5",
            "file_acc_at_10",
        ] {
            assert!(metrics.remove(key).is_some(), "missing serialized {key}");
        }
    }
    let back: AggregateReport = serde_json::from_value(value).unwrap();
    assert_eq!(back.index_refreshed, 0);
    assert_eq!(back.mean_ndcg_at_10, None);
    assert_eq!(back.stage_latency, None);
    assert_eq!(back.queries[0].metrics.ndcg_at_10, 0.0);
}

#[test]
fn ndcg_ignores_preferred_patterns_covered_by_an_expected_pattern() {
    // `mod.rs` is also an expected hit, so its preferred slot could never be
    // earned and must not count in the ideal ranking.
    let mut query = query_with(&["src/slang/**"]);
    query.preferred_paths = vec!["src/slang/mod.rs".to_owned()];
    let metrics = query_metrics(&query, &rows(&["src/slang/mod.rs", "src/other.rs"]), false);
    assert_close(metrics.ndcg_at_10, 1.0);

    // A preferred pattern outside the expected glob still counts: with the
    // expected hit first and the preferred file second, the ranking is ideal;
    // swapped, it is not.
    query.preferred_paths.push("src/other.rs".to_owned());
    let ideal = query_metrics(&query, &rows(&["src/slang/mod.rs", "src/other.rs"]), false);
    assert_close(ideal.ndcg_at_10, 1.0);
    let swapped = query_metrics(&query, &rows(&["src/other.rs", "src/slang/mod.rs"]), false);
    // DCG = 1/log2(2) + 2/log2(3), IDCG = 2/log2(2) + 1/log2(3).
    assert_close(
        swapped.ndcg_at_10,
        (1.0 + 2.0 / 3f64.log2()) / (2.0 + 1.0 / 3f64.log2()),
    );
    assert!(swapped.ndcg_at_10 < 1.0);

    // A preferred glob that an expected glob covers is dropped too.
    let mut query = query_with(&["src/**"]);
    query.preferred_paths = vec!["src/slang/*.rs".to_owned()];
    let metrics = query_metrics(&query, &rows(&["src/slang/mod.rs"]), false);
    assert_close(metrics.ndcg_at_10, 1.0);
}

#[test]
fn ndcg_does_not_depend_on_pattern_declaration_order() {
    // Overlapping expected patterns: the literal path, a one-level glob, and
    // a recursive glob. Each result must be credited to the most specific
    // pattern it satisfies, so every declaration order scores the same.
    let results = rows(&["src/metric.rs", "src/lib.rs", "src/deep/other.rs"]);
    let orders: [[&str; 3]; 6] = [
        ["src/**", "src/*.rs", "src/metric.rs"],
        ["src/**", "src/metric.rs", "src/*.rs"],
        ["src/*.rs", "src/**", "src/metric.rs"],
        ["src/*.rs", "src/metric.rs", "src/**"],
        ["src/metric.rs", "src/**", "src/*.rs"],
        ["src/metric.rs", "src/*.rs", "src/**"],
    ];
    for order in orders {
        let metrics = query_metrics(&query_with(&order), &results, false);
        assert_close(metrics.ndcg_at_10, 1.0);
        assert_close(metrics.recall_at_3, 1.0);
    }

    // The two-pattern case from review: broad glob declared first or last.
    let results = rows(&["src/metric.rs", "src/other.rs"]);
    for order in [["src/**", "src/metric.rs"], ["src/metric.rs", "src/**"]] {
        let metrics = query_metrics(&query_with(&order), &results, false);
        assert_close(metrics.ndcg_at_10, 1.0);
    }
}

// ---------------------------------------------------------------------------
// Query tags and per-tag aggregates
// ---------------------------------------------------------------------------

fn tagged(expected: &str, tags: &[&str]) -> EvalQuery {
    let mut query = query_with(&[expected]);
    query.tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
    query
}

fn run_tagged(query: EvalQuery, results: &[&str]) -> QueryReport {
    QueryReport::new(query, rows(results), false, None, None, None, None, None)
}

#[test]
fn tags_round_trip_through_toml_and_default_to_empty() {
    let raw = r#"
[[workspaces]]
key = "self"

[[queries]]
workspace = "self"
query = "tagged query"
expected_paths = ["src/a.rs"]
tags = ["keyword", "keyword-agnostic"]

[[queries]]
workspace = "self"
query = "untagged query"
expected_paths = ["src/a.rs"]
"#;
    let file: EvaluationFile = toml::from_str(raw).unwrap();
    validate_definition(&file).unwrap();
    assert_eq!(file.queries[0].tags, ["keyword", "keyword-agnostic"]);
    assert!(file.queries[1].tags.is_empty());

    let rendered = toml::to_string(&file).unwrap();
    let back: EvaluationFile = toml::from_str(&rendered).unwrap();
    assert_eq!(back.queries[0].tags, file.queries[0].tags);
    assert!(back.queries[1].tags.is_empty());
    // An empty tag list is not written out.
    assert!(!toml::to_string(&file.queries[1]).unwrap().contains("tags"));
    assert!(
        toml::to_string(&file.queries[0])
            .unwrap()
            .contains("keyword-agnostic")
    );
}

#[test]
fn by_tag_aggregates_executed_queries_per_tag() {
    let reports = [
        // keyword: found at rank 1.
        run_tagged(tagged("src/a.rs", &["keyword"]), &["src/a.rs", "src/x.rs"]),
        // keyword and keyword-agnostic: found at rank 2.
        run_tagged(
            tagged("src/a.rs", &["keyword", "keyword-agnostic"]),
            &["src/x.rs", "src/a.rs"],
        ),
        // keyword-agnostic, repeated on the query: a miss; counts once.
        run_tagged(
            tagged("src/a.rs", &["keyword-agnostic", "keyword-agnostic"]),
            &["src/x.rs", "src/y.rs"],
        ),
        // Untagged queries only feed the overall aggregate.
        run_tagged(query_with(&["src/a.rs"]), &["src/a.rs"]),
        // Skipped queries are excluded, and a tag seen only on a skipped
        // query has no entry at all.
        QueryReport::skipped(tagged("src/a.rs", &["keyword"]), "optional".to_owned()),
        QueryReport::skipped(tagged("src/a.rs", &["skipped-only"]), "optional".to_owned()),
    ];
    let report = aggregate(&reports);

    assert_eq!(report.executed_count, 4);
    assert_eq!(
        report.by_tag.keys().map(String::as_str).collect::<Vec<_>>(),
        ["keyword", "keyword-agnostic"]
    );

    // Rank 2 earns gain 2 at a log2(3) discount against an ideal of 2 at
    // log2(2), so the second query's nDCG is 1 / log2(3).
    let rank_two_ndcg = 1.0 / 3f64.log2();
    let keyword = &report.by_tag["keyword"];
    assert_eq!(keyword.executed_count, 2);
    assert_close(keyword.hit_at_1_rate, 0.5);
    assert_close(keyword.hit_at_3_rate, 1.0);
    assert_close(keyword.mrr, 0.75);
    assert_close(keyword.mean_ndcg_at_10, (1.0 + rank_two_ndcg) / 2.0);

    let agnostic = &report.by_tag["keyword-agnostic"];
    assert_eq!(agnostic.executed_count, 2);
    assert_close(agnostic.hit_at_1_rate, 0.0);
    assert_close(agnostic.hit_at_3_rate, 0.5);
    assert_close(agnostic.mrr, 0.25);
    assert_close(agnostic.mean_ndcg_at_10, rank_two_ndcg / 2.0);

    // The overall figures still cover every executed query, tagged or not.
    assert_close(report.mrr.unwrap(), (1.0 + 0.5 + 0.0 + 1.0) / 4.0);
}

#[test]
fn untagged_runs_omit_by_tag_and_old_reports_still_load() {
    let report = aggregate(&[run_tagged(query_with(&["src/a.rs"]), &["src/a.rs"])]);
    assert!(report.by_tag.is_empty());
    let mut value = serde_json::to_value(&report).unwrap();
    // Empty `by_tag` is not serialized, so reports written before the field
    // existed are indistinguishable from untagged runs.
    assert!(value.as_object_mut().unwrap().remove("by_tag").is_none());
    assert!(
        !serde_json::to_string(&report.queries[0].query)
            .unwrap()
            .contains("tags")
    );
    let back: AggregateReport = serde_json::from_value(value).unwrap();
    assert!(back.by_tag.is_empty());

    let tagged_report = aggregate(&[run_tagged(tagged("src/a.rs", &["keyword"]), &["src/a.rs"])]);
    let json = serde_json::to_string(&tagged_report).unwrap();
    let back: AggregateReport = serde_json::from_str(&json).unwrap();
    assert_eq!(back.by_tag, tagged_report.by_tag);
    assert_eq!(back.queries[0].query.tags, ["keyword"]);
}

#[tokio::test]
async fn run_evaluation_rejects_undeclared_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = crate::app::App::open(crate::config::Config {
        data_dir: temp.path().join("data"),
        embedding_url: format!("http://{address}/v1"),
        reranker_url: format!("http://{address}/rerank"),
        ..crate::config::Config::default()
    })
    .await
    .unwrap();
    let mut query = query_with(&["src/metric.rs"]);
    query.workspace = "missing-workspace".to_owned();
    let file = EvaluationFile {
        workspaces: Vec::new(),
        queries: vec![query],
    };

    let error = run_evaluation(&app, &file, &[])
        .await
        .expect_err("an undeclared workspace must return an error");
    assert!(error.to_string().contains("missing-workspace"), "{error}");

    // The definition rules are the command line's for every caller: mapping the
    // undeclared key does not make the file valid.
    let mapped = [WorkspaceMapping {
        key: "missing-workspace".to_owned(),
        path: temp.path().to_path_buf(),
    }];
    let error = run_evaluation(&app, &file, &mapped)
        .await
        .expect_err("a mapped but undeclared workspace must return an error");
    assert!(error.to_string().contains("missing-workspace"), "{error}");
}

#[test]
fn checked_in_definitions_load_and_validate() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("evaluations");
    let core = load_definition(&root.join("core.toml")).expect("core.toml loads");
    assert!(core.queries.iter().all(|query| query.tags.is_empty()));

    // The git-mined set is generated by `cargo xtask mine-queries`: every
    // query is stratified, asks for a ten-result window, and is expected to
    // name at least one file.
    let mined = load_definition(&root.join("mined").join("self.toml")).expect("mined set loads");
    assert_eq!(mined.workspaces.len(), 1);
    assert_eq!(mined.workspaces[0].key, "self");
    assert!(mined.workspaces[0].required);
    assert!(!mined.queries.is_empty());
    for query in &mined.queries {
        assert_eq!(query.workspace, "self");
        assert_eq!(query.top_k, Some(10), "{}", query.query);
        assert!(!query.expected_paths.is_empty(), "{}", query.query);
        // The mined gold is the files that exist at the commit the set was
        // generated from. A refactor that moves or deletes one of them leaves
        // a label no search can meet, so it must regenerate the set.
        for path in query
            .expected_paths
            .iter()
            .filter(|path| !path.contains(['*', '?', '[']))
        {
            assert!(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join(path)
                    .is_file(),
                "mined expected path {path} no longer exists; regenerate evaluations/mined/self.toml \
                 with `cargo xtask mine-queries --repo . --key self --out evaluations/mined/self.toml`"
            );
        }
        // The keyword tag comes first; `gold-drift` and one length tag may
        // follow, each at most once.
        assert!(!query.tags.is_empty(), "{}", query.query);
        assert!(
            ["keyword", "keyword-agnostic"].contains(&query.tags[0].as_str()),
            "{}",
            query.query
        );
        let extra = &query.tags[1..];
        for tag in extra {
            assert!(
                ["gold-drift", "short-query", "long-query"].contains(&tag.as_str()),
                "unknown tag {tag}: {}",
                query.query
            );
        }
        let distinct: std::collections::BTreeSet<&String> = extra.iter().collect();
        assert_eq!(distinct.len(), extra.len(), "{}", query.query);
        let has = |name: &str| extra.iter().any(|tag| tag.as_str() == name);
        assert!(
            !(has("short-query") && has("long-query")),
            "short and long are exclusive: {}",
            query.query
        );
    }
}

#[test]
fn definition_rejects_misspelled_expected_path() {
    let raw = r#"
[[workspaces]]
key = "self"

[[queries]]
workspace = "self"
query = "find a file"
expected_path = "src/a.rs"
"#;

    let error = toml::from_str::<EvaluationFile>(raw).expect_err("unknown fields must be rejected");
    assert!(error.to_string().contains("expected_path"), "{error}");
}

#[test]
fn definition_rejects_singular_query_table() {
    let raw = r#"
[[workspaces]]
key = "self"

[[query]]
workspace = "self"
query = "find a file"
expected_paths = ["src/a.rs"]
"#;

    assert!(
        toml::from_str::<EvaluationFile>(raw).is_err(),
        "the singular query table must be rejected"
    );
}

#[test]
fn definition_rejects_query_without_expected_paths() {
    let raw = r#"
[[workspaces]]
key = "self"

[[queries]]
workspace = "self"
query = "find a file"
expected_paths = []
"#;
    let file: EvaluationFile = toml::from_str(raw).unwrap();

    let error = validate_definition(&file).expect_err("expected_paths must not be empty");
    assert!(error.to_string().contains("query at index 0"), "{error}");
    assert!(error.to_string().contains("expected_paths"), "{error}");
}

// ---------------------------------------------------------------------------
// Paired comparison
// ---------------------------------------------------------------------------

fn compare_query(workspace: &str, text: &str, tags: &[&str]) -> EvalQuery {
    EvalQuery {
        query: text.to_owned(),
        workspace: workspace.to_owned(),
        tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
        ..query_with(&[])
    }
}

/// An executed query report with hand-set metrics, so each test states its
/// own numbers instead of deriving them from evidence.
fn scored(
    workspace: &str,
    text: &str,
    tags: &[&str],
    reciprocal_rank: f64,
    ndcg_at_10: f64,
    hit_at_1: bool,
) -> QueryReport {
    let mut report = QueryReport::failed(compare_query(workspace, text, tags), String::new());
    report.metrics.reciprocal_rank = reciprocal_rank;
    report.metrics.ndcg_at_10 = ndcg_at_10;
    report.metrics.hit_at_1 = hit_at_1;
    report
}

/// A query in workspace `ws` whose three metrics all follow one value.
fn rr_only(text: &str, reciprocal_rank: f64) -> QueryReport {
    scored(
        "ws",
        text,
        &[],
        reciprocal_rank,
        reciprocal_rank,
        reciprocal_rank == 1.0,
    )
}

fn skipped_query(workspace: &str, text: &str) -> QueryReport {
    QueryReport::skipped(
        compare_query(workspace, text, &[]),
        "optional workspace not provided".to_owned(),
    )
}

fn default_compare(baseline: &[QueryReport], candidate: &[QueryReport]) -> ComparisonReport {
    compare(
        &aggregate(baseline),
        &aggregate(candidate),
        &CompareOptions::default(),
    )
}

fn key(workspace: &str, query: &str) -> QueryKey {
    QueryKey {
        workspace: workspace.to_owned(),
        query: query.to_owned(),
    }
}

#[test]
fn comparing_identical_reports_gives_all_ties_and_a_zero_interval() {
    let reports = vec![
        rr_only("q1", 1.0),
        rr_only("q2", 0.5),
        rr_only("q3", 1.0 / 3.0),
        rr_only("q4", 0.0),
    ];
    let comparison = default_compare(&reports, &reports);

    assert_eq!(comparison.paired, 4);
    assert_eq!(comparison.pairs.len(), 4);
    assert_eq!(comparison.unpaired_baseline, 0);
    assert_eq!(comparison.unpaired_candidate, 0);
    assert_eq!(comparison.skipped_pairs, 0);
    let overall = comparison.overall.expect("four pairs");
    assert_eq!(
        (overall.wins, overall.losses, overall.ties),
        (0, 0, 4),
        "identical reports tie on every query"
    );
    for metric in [overall.mrr, overall.ndcg_at_10, overall.hit_at_1] {
        assert_eq!(metric.mean_delta, 0.0);
        assert_eq!((metric.ci_low, metric.ci_high), (0.0, 0.0));
        assert_eq!(metric.baseline_mean, metric.candidate_mean);
        assert!(!metric.significant, "a zero interval contains zero");
    }
}

#[test]
fn a_known_shift_gives_the_hand_computed_deltas_and_win_loss_tie_counts() {
    // Reciprocal-rank deltas: +0.5, +0.5, -0.5, 0.
    let baseline = vec![
        scored("ws", "q1", &[], 0.5, 0.4, false),
        scored("ws", "q2", &[], 0.5, 0.4, false),
        scored("ws", "q3", &[], 1.0, 0.8, true),
        scored("ws", "q4", &[], 0.0, 0.0, false),
    ];
    let candidate = vec![
        scored("ws", "q1", &[], 1.0, 0.8, true),
        scored("ws", "q2", &[], 1.0, 0.8, true),
        scored("ws", "q3", &[], 0.5, 0.4, false),
        scored("ws", "q4", &[], 0.0, 0.0, false),
    ];
    let comparison = default_compare(&baseline, &candidate);
    let overall = comparison.overall.expect("four pairs");

    assert_eq!((overall.wins, overall.losses, overall.ties), (2, 1, 1));
    assert_eq!(overall.pairs, 4);
    assert_close(overall.mrr.baseline_mean, 0.5);
    assert_close(overall.mrr.candidate_mean, 0.625);
    assert_close(overall.mrr.mean_delta, 0.125);
    assert_close(overall.ndcg_at_10.baseline_mean, 0.4);
    assert_close(overall.ndcg_at_10.candidate_mean, 0.5);
    assert_close(overall.ndcg_at_10.mean_delta, 0.1);
    assert_close(overall.hit_at_1.baseline_mean, 0.25);
    assert_close(overall.hit_at_1.candidate_mean, 0.5);
    assert_close(overall.hit_at_1.mean_delta, 0.25);
    for metric in [overall.mrr, overall.ndcg_at_10, overall.hit_at_1] {
        assert!(metric.ci_low <= metric.mean_delta && metric.mean_delta <= metric.ci_high);
    }
    // Two wins, one loss and one tie are not enough to rule out zero.
    assert!(overall.mrr.ci_low < 0.0 && overall.mrr.ci_high > 0.0);
    assert!(!overall.mrr.significant);

    let first = &comparison.pairs[0];
    assert_eq!(
        (first.workspace.as_str(), first.query.as_str()),
        ("ws", "q1")
    );
    assert_close(first.baseline.reciprocal_rank, 0.5);
    assert_close(first.candidate.reciprocal_rank, 1.0);
    assert_close(first.delta.reciprocal_rank, 0.5);
    assert_close(first.delta.ndcg_at_10, 0.4);
    assert_close(first.delta.hit_at_1, 1.0);
    assert_close(comparison.pairs[2].delta.hit_at_1, -1.0);
}

#[test]
fn a_consistent_improvement_is_significant_and_a_consistent_loss_is_too() {
    // Eight different positive gains (sum 0.68, mean 0.085) over baseline
    // reciprocal ranks 0.0, 0.1, ... 0.7, so the highest candidate is 0.85. The
    // bootstrap interval is therefore not a point: it is a percentile interval
    // of resampled means, each of which lies in [0.03, 0.15] (the smallest and
    // largest gain), so `ci_low > 0` is guaranteed while the interval still has
    // width and brackets the mean.
    let gains = [0.03, 0.05, 0.08, 0.10, 0.12, 0.06, 0.09, 0.15];
    let baseline: Vec<QueryReport> = (0..8)
        .map(|index| rr_only(&format!("q{index}"), 0.1 * f64::from(index)))
        .collect();
    let better: Vec<QueryReport> = gains
        .iter()
        .enumerate()
        .map(|(index, gain)| rr_only(&format!("q{index}"), 0.1 * index as f64 + gain))
        .collect();

    let up = default_compare(&baseline, &better).overall.unwrap();
    assert_eq!((up.wins, up.losses, up.ties), (8, 0, 0));
    assert_close(up.mrr.mean_delta, 0.085);
    assert!(
        0.03 <= up.mrr.ci_low
            && up.mrr.ci_low < up.mrr.mean_delta
            && up.mrr.mean_delta < up.mrr.ci_high
            && up.mrr.ci_high <= 0.15,
        "{:?}",
        up.mrr
    );
    assert!(up.mrr.significant);

    let down = default_compare(&better, &baseline).overall.unwrap();
    assert_eq!((down.wins, down.losses, down.ties), (0, 8, 0));
    assert_close(down.mrr.mean_delta, -0.085);
    assert!(
        -0.15 <= down.mrr.ci_low
            && down.mrr.ci_low < down.mrr.mean_delta
            && down.mrr.mean_delta < down.mrr.ci_high
            && down.mrr.ci_high <= -0.03,
        "{:?}",
        down.mrr
    );
    assert!(down.mrr.significant);
}

#[test]
fn the_interval_of_a_constant_delta_is_that_delta() {
    let baseline: Vec<QueryReport> = (0..7)
        .map(|index| rr_only(&format!("q{index}"), 0.125 * f64::from(index)))
        .collect();
    let candidate: Vec<QueryReport> = (0..7)
        .map(|index| rr_only(&format!("q{index}"), 0.125 * f64::from(index) + 0.25))
        .collect();
    let mrr = default_compare(&baseline, &candidate).overall.unwrap().mrr;

    assert_close(mrr.mean_delta, 0.25);
    assert_close(mrr.ci_low, 0.25);
    assert_close(mrr.ci_high, 0.25);
    assert!(mrr.significant);

    assert_eq!(
        paired_bootstrap_ci(&[0.25; 7], 10_000, 99),
        Some((0.25, 0.25))
    );
    assert_eq!(paired_bootstrap_ci(&[], 10_000, 99), None);
}

#[test]
fn the_interval_is_reproducible_for_a_seed_and_moves_with_it() {
    // Twelve distinct reciprocal ranks on the baseline side, rotated on the
    // candidate side, so the twelve deltas are all different.
    let ranks: Vec<f64> = (1..=12).map(|rank| 1.0 / f64::from(rank)).collect();
    let baseline: Vec<QueryReport> = ranks
        .iter()
        .enumerate()
        .map(|(index, rr)| rr_only(&format!("q{index}"), *rr))
        .collect();
    let candidate: Vec<QueryReport> = ranks
        .iter()
        .cycle()
        .skip(5)
        .take(12)
        .enumerate()
        .map(|(index, rr)| rr_only(&format!("q{index}"), *rr))
        .collect();
    let run = |seed| {
        compare(
            &aggregate(&baseline),
            &aggregate(&candidate),
            &CompareOptions {
                seed,
                resamples: DEFAULT_RESAMPLES,
            },
        )
    };

    let first = run(7);
    assert_eq!(first, run(7), "the same seed must reproduce the report");
    assert_eq!(first.seed, 7);
    assert_eq!(first.resamples, 10_000);

    let other = run(8);
    let (a, b) = (first.overall.unwrap().mrr, other.overall.unwrap().mrr);
    assert_eq!(a.mean_delta, b.mean_delta, "the point estimate has no seed");
    assert_ne!(
        (a.ci_low, a.ci_high),
        (b.ci_low, b.ci_high),
        "a different seed resamples differently"
    );

    let by_default = default_compare(&baseline, &candidate);
    assert_eq!(by_default.seed, DEFAULT_SEED);
    assert_eq!(by_default.resamples, DEFAULT_RESAMPLES);
    assert_eq!(by_default, default_compare(&baseline, &candidate));
}

#[test]
fn unpaired_and_skipped_queries_are_counted_and_listed_never_dropped() {
    let baseline = vec![
        rr_only("a", 1.0),
        rr_only("b", 0.5),
        skipped_query("ws", "c"),
        rr_only("d", 1.0),
        skipped_query("ws", "g"),
        scored("other", "a", &[], 1.0, 1.0, true),
        rr_only("dup", 1.0),
        rr_only("dup", 0.5),
    ];
    let candidate = vec![
        rr_only("a", 1.0),
        rr_only("b", 1.0),
        rr_only("c", 1.0),
        rr_only("e", 1.0),
        skipped_query("ws", "g"),
        rr_only("dup", 0.5),
    ];
    let comparison = default_compare(&baseline, &candidate);

    // a, b and the first `dup` pair; the second `dup` has no counterpart.
    assert_eq!(comparison.paired, 3);
    let paired: Vec<&str> = comparison
        .pairs
        .iter()
        .map(|pair| pair.query.as_str())
        .collect();
    assert_eq!(paired, ["a", "b", "dup"]);
    let dup = &comparison.pairs[2];
    assert_close(dup.baseline.reciprocal_rank, 1.0);
    assert_close(dup.candidate.reciprocal_rank, 0.5);

    // c is skipped in the baseline, g on both sides: neither is paired.
    assert_eq!(comparison.skipped_pairs, 2);
    assert_eq!(comparison.skipped_baseline, 2);
    assert_eq!(comparison.skipped_candidate, 1);

    // The same text in another workspace is a different query.
    assert_eq!(comparison.unpaired_baseline, 3);
    assert_eq!(
        comparison.unpaired_baseline_queries,
        [key("ws", "d"), key("other", "a"), key("ws", "dup")]
    );
    assert_eq!(comparison.unpaired_candidate, 1);
    assert_eq!(comparison.unpaired_candidate_queries, [key("ws", "e")]);

    // Only the three pairs feed the statistics.
    let overall = comparison.overall.unwrap();
    assert_eq!(overall.pairs, 3);
    assert_eq!((overall.wins, overall.losses, overall.ties), (1, 1, 1));

    let summary = comparison.render_summary();
    assert!(
        summary.contains("unpaired: baseline 3, candidate 1"),
        "{summary}"
    );
    assert!(summary.contains("skipped on either side: 2"), "{summary}");
}

#[test]
fn pairs_are_grouped_by_tag_with_their_own_counts_and_intervals() {
    let baseline = vec![
        scored("ws", "q1", &["keyword"], 0.5, 0.5, false),
        scored("ws", "q2", &["keyword", "hard"], 1.0, 1.0, true),
        scored("ws", "q3", &[], 0.0, 0.0, false),
        scored("ws", "q4", &[], 0.0, 0.0, false),
        scored("ws", "q5", &["ghost"], 1.0, 1.0, true),
    ];
    let candidate = vec![
        scored("ws", "q1", &["keyword"], 1.0, 1.0, true),
        scored("ws", "q2", &["keyword", "hard"], 0.5, 0.5, false),
        // The candidate report is the only one tagging q3.
        scored("ws", "q3", &["hard"], 0.0, 0.0, false),
        scored("ws", "q4", &[], 1.0, 1.0, true),
        // q5 is skipped in the candidate, so the tag has no pair left.
        skipped_query("ws", "q5"),
    ];
    let comparison = default_compare(&baseline, &candidate);

    let overall = comparison.overall.unwrap();
    assert_eq!(overall.pairs, 4);
    assert_eq!((overall.wins, overall.losses, overall.ties), (2, 1, 1));

    let tags: Vec<&str> = comparison.by_tag.keys().map(String::as_str).collect();
    assert_eq!(tags, ["hard", "keyword"], "no group for untagged or ghost");

    let keyword = comparison.by_tag["keyword"];
    assert_eq!(keyword.pairs, 2);
    assert_eq!((keyword.wins, keyword.losses, keyword.ties), (1, 1, 0));
    assert_close(keyword.mrr.mean_delta, 0.0);
    // Deltas {+0.5, -0.5}: the resampled means are -0.5, 0 or +0.5, with the
    // two tails at a quarter each.
    assert_close(keyword.mrr.ci_low, -0.5);
    assert_close(keyword.mrr.ci_high, 0.5);
    assert!(!keyword.mrr.significant);

    let hard = comparison.by_tag["hard"];
    assert_eq!(hard.pairs, 2);
    assert_eq!((hard.wins, hard.losses, hard.ties), (0, 1, 1));
    assert_close(hard.mrr.baseline_mean, 0.5);
    assert_close(hard.mrr.candidate_mean, 0.25);
    assert_close(hard.mrr.mean_delta, -0.25);
    assert_close(hard.mrr.ci_low, -0.5);
    assert_close(hard.mrr.ci_high, 0.0);

    let pair = |query: &str| {
        comparison
            .pairs
            .iter()
            .find(|pair| pair.query == query)
            .unwrap()
    };
    assert_eq!(pair("q3").tags, ["hard"]);
    assert_eq!(pair("q2").tags, ["hard", "keyword"], "tags are sorted");

    let summary = comparison.render_summary();
    assert!(summary.contains("tag keyword"), "{summary}");
    assert!(summary.contains("tag hard"), "{summary}");
}

#[test]
fn untagged_comparisons_omit_by_tag_and_empty_ones_have_no_overall() {
    let reports = vec![rr_only("q1", 1.0)];
    let comparison = default_compare(&reports, &reports);
    assert!(comparison.by_tag.is_empty());
    let json = serde_json::to_value(&comparison).unwrap();
    assert!(json.get("by_tag").is_none());
    assert_eq!(json["overall"]["mrr"]["significant"], false);
    assert_eq!(json["paired"], 1);

    let empty = default_compare(&[], &[]);
    assert_eq!(empty.paired, 0);
    assert!(empty.overall.is_none());
    assert!(empty.render_summary().contains("no comparable pairs"));
    let no_overlap = default_compare(&[rr_only("a", 1.0)], &[rr_only("b", 1.0)]);
    assert!(no_overlap.overall.is_none());
    assert_eq!(no_overlap.unpaired_baseline, 1);
    assert_eq!(no_overlap.unpaired_candidate, 1);
}

#[test]
fn the_summary_names_each_metric_with_its_interval_and_win_loss_tie() {
    let baseline = vec![rr_only("q1", 0.5), rr_only("q2", 0.25)];
    let candidate = vec![rr_only("q1", 1.0), rr_only("q2", 0.25)];
    let summary = default_compare(&baseline, &candidate).render_summary();

    for needle in [
        "MRR",
        "nDCG@10",
        "hit@1",
        "baseline",
        "candidate",
        "95% CI",
        "reciprocal rank W/L/T: 1/0/1 over 2 pairs",
        "10000 resamples",
        "+0.2500",
    ] {
        assert!(summary.contains(needle), "missing {needle:?} in\n{summary}");
    }
}

#[test]
fn reports_load_from_disk_including_ones_that_embed_a_comparison() {
    let temp = tempfile::tempdir().unwrap();
    let baseline_report = aggregate(&[rr_only("q1", 0.5), rr_only("q2", 1.0)]);
    let baseline_path = temp.path().join("baseline.json");
    // A PowerShell redirect writes a byte-order mark.
    std::fs::write(
        &baseline_path,
        format!(
            "\u{feff}{}",
            serde_json::to_string_pretty(&baseline_report).unwrap()
        ),
    )
    .unwrap();

    let candidate_report = aggregate(&[rr_only("q1", 1.0), rr_only("q2", 1.0)]);
    let embedded = report_with_comparison(
        &candidate_report,
        Some(&compare(
            &baseline_report,
            &candidate_report,
            &CompareOptions::default(),
        )),
    )
    .unwrap();
    assert_eq!(embedded["comparison"]["paired"], 2);
    assert!(report_with_comparison(&candidate_report, None).unwrap()["comparison"].is_null());
    let candidate_path = temp.path().join("candidate.json");
    std::fs::write(&candidate_path, embedded.to_string()).unwrap();

    let loaded = load_report(&baseline_path).unwrap();
    assert_eq!(loaded.queries.len(), 2);

    let comparison =
        compare_files(&baseline_path, &candidate_path, &CompareOptions::default()).unwrap();
    assert_eq!(comparison.paired, 2);
    assert_eq!(
        comparison.baseline_source.as_deref(),
        Some(baseline_path.display().to_string().as_str())
    );
    assert_eq!(
        comparison.candidate_source.as_deref(),
        Some(candidate_path.display().to_string().as_str())
    );
    let overall = comparison.overall.unwrap();
    assert_eq!((overall.wins, overall.losses, overall.ties), (1, 0, 1));

    let missing = temp.path().join("missing.json");
    let error = load_report(&missing).unwrap_err();
    assert!(
        format!("{error:#}").contains("failed to read evaluation report"),
        "{error:#}"
    );
    let garbage = temp.path().join("garbage.json");
    std::fs::write(&garbage, "not json").unwrap();
    let error = load_report(&garbage).unwrap_err();
    assert!(
        format!("{error:#}").contains("failed to parse evaluation report"),
        "{error:#}"
    );
    assert!(compare_files(&baseline_path, &garbage, &CompareOptions::default()).is_err());
}

// ---------------------------------------------------------------------------
// Offline replay of a candidate dump
// ---------------------------------------------------------------------------

/// One candidate of a hand-built dump: its channel ranks and, if the dumped
/// reranker was asked about it, the score it gave.
#[derive(Clone, Copy)]
struct Spec {
    path: &'static str,
    start: u32,
    semantic: Option<usize>,
    lexical: Option<usize>,
    lsp: Option<usize>,
    role: SourceRole,
    rerank: f32,
    tokens: usize,
}

const fn spec(
    path: &'static str,
    start: u32,
    semantic: Option<usize>,
    lexical: Option<usize>,
    role: SourceRole,
    rerank: f32,
    tokens: usize,
) -> Spec {
    Spec {
        path,
        start,
        semantic,
        lexical,
        lsp: None,
        role,
        rerank,
        tokens,
    }
}

// With the default weights (2, 1, 1), rrf_k 60 and the 0.004 source prior the
// fused order of these six is A1, A20, B1, A40, C1, D1. The reranker (scores
// below) prefers A40, B1, A20, C1, A1, D1. a.rs holds three chunks, so a cap
// of 1 demotes A20 and A40 behind the other files, and a cap of 2 demotes A40.
const A1: Spec = spec("a.rs", 1, Some(1), Some(1), SourceRole::Source, 0.2, 100);
const A20: Spec = spec("a.rs", 20, Some(2), Some(2), SourceRole::Source, 0.4, 200);
const A40: Spec = spec("a.rs", 40, Some(3), None, SourceRole::Source, 0.9, 300);
const B1: Spec = spec("b.rs", 1, Some(4), Some(3), SourceRole::Source, 0.5, 400);
const C1: Spec = spec("c.rs", 1, Some(5), None, SourceRole::Source, 0.3, 500);
const D1: Spec = spec("d.rs", 1, None, Some(4), SourceRole::Test, 0.1, 600);
/// Deliberately not in fused order: the builder sorts.
const SIX: [Spec; 6] = [D1, A40, B1, A1, C1, A20];

/// The arm of a dump with a semantic weight of 2.0 (not the default, so a
/// replay override of it is visible) and a 1 KiB document budget.
fn dump_arm(window: usize, k: usize, cap: u32, mode: RerankMode) -> CandidateArm {
    CandidateArm {
        rrf_k: 60.0,
        fusion: FusionConfig {
            semantic_weight: 2.0,
            lexical_weight: 1.0,
            lsp_weight: 1.0,
            max_chunks_per_file: cap,
        },
        rerank_candidate_count: window,
        rerank_document_max_bytes: 1024,
        semantic_candidate_count: 40,
        lexical_candidate_count: 40,
        lsp_candidate_count: 40,
        channels: ChannelsConfig {
            reranker: mode != RerankMode::Off,
            ..ChannelsConfig::default()
        },
        k,
        embedding_identity: "test-embedding".to_owned(),
        reranker_model: "test-reranker".to_owned(),
        rerank_mode: mode,
    }
}

/// The documented fusion arithmetic, restated for the builder.
fn expected_fusion(spec: &Spec, arm: &CandidateArm) -> f32 {
    let mut score = 0.0_f32;
    if let Some(rank) = spec.semantic {
        score += arm.fusion.semantic_weight / (arm.rrf_k + rank as f32);
    }
    if let Some(rank) = spec.lexical {
        score += arm.fusion.lexical_weight / (arm.rrf_k + rank as f32);
    }
    if let Some(rank) = spec.lsp {
        score += arm.fusion.lsp_weight / (arm.rrf_k + rank as f32);
    }
    score
        + if spec.role == SourceRole::Source {
            0.004
        } else {
            0.0
        }
}

fn dump_timings(reranking_ms: f64) -> EvalTimings {
    EvalTimings {
        query_embedding_ms: 10.0,
        lancedb_search_ms: 5.0,
        lexical_search_ms: 5.0,
        lsp_search_ms: 0.0,
        fusion_ms: 1.0,
        reranking_ms,
        total_ms: 21.0 + reranking_ms,
    }
}

/// A query report exactly as `evaluate --dump-candidates` would write it for
/// a search over `specs` under `arm`: fused order, per-file cap, shortlist,
/// reranker scores (when `outcome` is `Scored`), and the results the search
/// returned, built here with a deliberately separate, plain implementation.
fn dumped_query(
    text: &str,
    expected: &[&str],
    specs: &[Spec],
    arm: &CandidateArm,
    outcome: RerankOutcome,
    reranking_ms: f64,
) -> QueryReport {
    let mut rows: Vec<CandidateTrace> = specs
        .iter()
        .map(|spec| CandidateTrace {
            relative_file_path: spec.path.to_owned(),
            language: "rust".to_owned(),
            start_line: spec.start,
            end_line: spec.start + 9,
            content_hash: format!("{}:{}", spec.path, spec.start),
            source_role: spec.role,
            semantic_rank: spec.semantic,
            semantic_score: spec.semantic.map(|_| 0.5),
            lexical_rank: spec.lexical,
            lexical_score: spec.lexical.map(|_| 1.5),
            lexical_match_count: u32::from(spec.lexical.is_some()),
            lsp_rank: spec.lsp,
            prior: if spec.role == SourceRole::Source {
                0.004
            } else {
                0.0
            },
            fusion_score: expected_fusion(spec, arm),
            fused_rank: 0,
            capped_rank: 0,
            shortlisted: false,
            rerank_rank: None,
            reranker_score: None,
            document_bytes: None,
            est_tokens: None,
            snippet: format!("snippet of {}:{}", spec.path, spec.start),
        })
        .collect();
    rows.sort_by(|a, b| {
        b.fusion_score
            .total_cmp(&a.fusion_score)
            .then_with(|| a.relative_file_path.cmp(&b.relative_file_path))
            .then(a.start_line.cmp(&b.start_line))
            .then(a.end_line.cmp(&b.end_line))
            .then(a.content_hash.cmp(&b.content_hash))
    });
    for (index, row) in rows.iter_mut().enumerate() {
        row.fused_rank = index + 1;
    }
    // The per-file cap, written the plain way.
    let cap = arm.fusion.max_chunks_per_file as usize;
    let mut per_file = std::collections::HashMap::<String, usize>::new();
    let (mut head, mut tail) = (Vec::new(), Vec::new());
    for (index, row) in rows.iter().enumerate() {
        let kept = per_file.entry(row.relative_file_path.clone()).or_insert(0);
        if cap == 0 || *kept < cap {
            *kept += 1;
            head.push(index);
        } else {
            tail.push(index);
        }
    }
    let order: Vec<usize> = head.into_iter().chain(tail).collect();
    for (position, &index) in order.iter().enumerate() {
        rows[index].capped_rank = position + 1;
        rows[index].shortlisted = position < arm.rerank_candidate_count;
    }
    let shortlist: Vec<usize> = order
        .iter()
        .copied()
        .take(arm.rerank_candidate_count)
        .collect();
    let spec_of = |row: &CandidateTrace| {
        *specs
            .iter()
            .find(|spec| spec.path == row.relative_file_path && spec.start == row.start_line)
            .unwrap()
    };
    for &index in &shortlist {
        let tokens = spec_of(&rows[index]).tokens;
        rows[index].document_bytes = Some(tokens * 4);
        rows[index].est_tokens = Some(tokens);
    }
    let results: Vec<usize> = if outcome == RerankOutcome::Scored {
        let mut ranked: Vec<(usize, f32)> = shortlist
            .iter()
            .enumerate()
            .map(|(request, &index)| (request, spec_of(&rows[index]).rerank))
            .collect();
        ranked.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        for (place, &(request, score)) in ranked.iter().enumerate() {
            rows[shortlist[request]].rerank_rank = Some(place + 1);
            rows[shortlist[request]].reranker_score = Some(score);
        }
        ranked
            .iter()
            .map(|&(request, _)| shortlist[request])
            .collect()
    } else {
        shortlist
    };
    let evidence: Vec<ResultEvidence> = results
        .iter()
        .take(arm.k)
        .enumerate()
        .map(|(position, &index)| ResultEvidence {
            rank: position + 1,
            relative_file_path: rows[index].relative_file_path.clone(),
            language: rows[index].language.clone(),
            source_role: rows[index].source_role.as_str().to_owned(),
            start_line: rows[index].start_line,
            end_line: rows[index].end_line,
            snippet: rows[index].snippet.clone(),
            reranker_score: rows[index].reranker_score,
        })
        .collect();
    let query = EvalQuery {
        query: text.to_owned(),
        top_k: Some(arm.k),
        ..query_with(expected)
    };
    let mut report = QueryReport::new(
        query,
        evidence,
        outcome == RerankOutcome::Scored,
        None,
        None,
        Some(dump_timings(reranking_ms)),
        Some(IndexLifecycleAction::Reused),
        Some(0.0),
    );
    report.candidates = rows;
    report.arm = Some(arm.clone());
    report.rerank_outcome = Some(outcome);
    report
}

/// A report holding the dumped queries.
fn dump_of(queries: &[QueryReport]) -> AggregateReport {
    aggregate(queries)
}

fn replayed(source: &AggregateReport, options: ReplayOptions) -> ReplayReport {
    replay(source, &options).unwrap()
}

/// `path:start_line` of each result of a query, best first.
fn places(report: &QueryReport) -> Vec<String> {
    report
        .evidence
        .iter()
        .map(|row| format!("{}:{}", row.relative_file_path, row.start_line))
        .collect()
}

/// The six-row dump with the whole pool shortlisted and scored: every
/// replay window and cap is exact against it.
fn full_dump() -> AggregateReport {
    let arm = dump_arm(6, 6, 0, RerankMode::Replace);
    dump_of(&[dumped_query(
        "full pool",
        &["a.rs"],
        &SIX,
        &arm,
        RerankOutcome::Scored,
        900.0,
    )])
}

#[test]
fn replay_at_the_dumped_arm_reproduces_the_dumped_evidence_and_metrics() {
    // The whole pool shortlisted: results are the reranker's order.
    let source = full_dump();
    assert_eq!(
        places(&source.queries[0]),
        ["a.rs:40", "b.rs:1", "a.rs:20", "c.rs:1", "a.rs:1", "d.rs:1"]
    );
    let out = replayed(&source, ReplayOptions::default());
    let (before, after) = (&source.queries[0], &out.report.queries[0]);
    assert_eq!(after.evidence, before.evidence);
    assert_eq!(after.metrics, before.metrics);
    assert_eq!((after.reranked, after.outcome.as_str()), (true, "success"));
    assert_eq!(out.replay.queries.replayed, 1);
    assert_eq!(out.replay.queries.refused, 0);
    assert_eq!(out.replay.past_window_queries, 0);
    assert_eq!(out.replay.arm.window, 6);

    // The production configuration shape: cap 2, window 4, k 3. a.rs:40 is
    // demoted behind c.rs and d.rs and out of the shortlist; the reranker
    // sees A1, A20, B1, C1.
    let arm = dump_arm(4, 3, 2, RerankMode::Replace);
    let source = dump_of(&[dumped_query(
        "production shape",
        &["b.rs"],
        &SIX,
        &arm,
        RerankOutcome::Scored,
        900.0,
    )]);
    assert_eq!(places(&source.queries[0]), ["b.rs:1", "a.rs:20", "c.rs:1"]);
    let rows = &source.queries[0].candidates;
    assert_eq!(
        rows.iter().map(|row| row.capped_rank).collect::<Vec<_>>(),
        [1, 2, 3, 6, 4, 5]
    );
    let out = replayed(&source, ReplayOptions::default());
    let (before, after) = (&source.queries[0], &out.report.queries[0]);
    assert_eq!(after.evidence, before.evidence);
    assert_eq!(after.metrics, before.metrics);
    assert_eq!(out.replay.queries.refused, 0);
    // Naming the dumped values explicitly changes nothing.
    let explicit = replayed(
        &source,
        ReplayOptions {
            window: Some(4),
            cap: Some(2),
            cap_stage: Some(CapStage::Shortlist),
            semantic_weight: Some(2.0),
            lexical_weight: Some(1.0),
            lsp_weight: Some(1.0),
            rrf_k: Some(60.0),
            prior: Some(0.004),
            rerank_mode: Some(RerankMode::Replace),
            vote_weight: None,
        },
    );
    assert_eq!(explicit.report.queries[0].evidence, before.evidence);
}

#[test]
fn replay_reproduces_a_pool_without_candidates_and_a_reranker_that_was_off() {
    let arm = dump_arm(4, 3, 2, RerankMode::Replace);
    let empty = dumped_query("nothing", &["a.rs"], &[], &arm, RerankOutcome::NotRun, 0.0);
    assert!(!empty.reranked && empty.evidence.is_empty());
    let source = dump_of(&[empty]);
    let out = replayed(&source, ReplayOptions::default());
    assert!(out.report.queries[0].evidence.is_empty());
    assert!(!out.report.queries[0].reranked);
    assert_eq!(out.report.queries[0].metrics, source.queries[0].metrics);
    assert_eq!(out.replay.queries.refused, 0);
    // An empty pool costs the reranker nothing, whatever the cost model says.
    assert_eq!(out.report.queries[0].timings.unwrap().reranking_ms, 0.0);

    // `[channels].reranker = false`: fused (capped) order, no scores.
    let arm = dump_arm(4, 3, 2, RerankMode::Off);
    let source = dump_of(&[dumped_query(
        "off",
        &["c.rs"],
        &SIX,
        &arm,
        RerankOutcome::Off,
        0.0,
    )]);
    assert_eq!(places(&source.queries[0]), ["a.rs:1", "a.rs:20", "b.rs:1"]);
    let out = replayed(&source, ReplayOptions::default());
    assert_eq!(out.report.queries[0].evidence, source.queries[0].evidence);
    assert_eq!(out.report.queries[0].metrics, source.queries[0].metrics);
    assert!(!out.report.queries[0].reranked);
    assert_eq!(out.replay.arm.rerank_mode, RerankMode::Off);
    assert_eq!(out.replay.past_window_queries, 0);

    // Production cuts the pool to the shortlist even without the reranker, so
    // a result window of six over a shortlist of four returned four rows;
    // replay appends the rest of the fused order and counts the query.
    let arm = dump_arm(4, 6, 2, RerankMode::Off);
    let source = dump_of(&[dumped_query(
        "off, wide",
        &["c.rs"],
        &SIX,
        &arm,
        RerankOutcome::Off,
        0.0,
    )]);
    assert_eq!(source.queries[0].evidence.len(), 4);
    let out = replayed(&source, ReplayOptions::default());
    assert_eq!(
        places(&out.report.queries[0]),
        ["a.rs:1", "a.rs:20", "b.rs:1", "c.rs:1", "d.rs:1", "a.rs:40"]
    );
    assert_eq!(
        out.report.queries[0].evidence[..4],
        source.queries[0].evidence[..]
    );
    assert_eq!(out.replay.past_window_queries, 1);
    assert!(out.render_summary().contains("past the window"));
}

#[test]
fn replay_with_a_smaller_window_reranks_only_the_window_and_appends_the_fused_tail() {
    let source = full_dump();
    let out = replayed(
        &source,
        ReplayOptions {
            window: Some(2),
            ..ReplayOptions::default()
        },
    );
    let query = &out.report.queries[0];
    // Window A1, A20 reranked to A20 (0.4), A1 (0.2); then B1, A40, C1, D1 in
    // fused order, not in reranker order.
    assert_eq!(
        places(query),
        ["a.rs:20", "a.rs:1", "b.rs:1", "a.rs:40", "c.rs:1", "d.rs:1"]
    );
    let scores: Vec<Option<f32>> = query
        .evidence
        .iter()
        .map(|row| row.reranker_score)
        .collect();
    assert_eq!(
        scores,
        [Some(0.4), Some(0.2), None, None, None, None],
        "only the window carries reranker scores"
    );
    assert!(query.reranked);
    assert_eq!(query.metrics.reranker, RerankerState::PartiallyScored);
    assert_eq!(out.replay.past_window_queries, 1);

    // A window of one cannot reorder anything.
    let one = replayed(
        &source,
        ReplayOptions {
            window: Some(1),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(
        places(&one.report.queries[0]),
        ["a.rs:1", "a.rs:20", "b.rs:1", "a.rs:40", "c.rs:1", "d.rs:1"]
    );
    // A window at least as large as the pool is the whole pool.
    let large = replayed(
        &source,
        ReplayOptions {
            window: Some(50),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(large.report.queries[0].evidence, source.queries[0].evidence);
    assert_eq!(large.replay.past_window_queries, 0);
}

#[test]
fn replay_cap_stages_differ_on_a_demoted_chunk() {
    let source = full_dump();
    let at = |stage| {
        let out = replayed(
            &source,
            ReplayOptions {
                window: Some(4),
                cap: Some(1),
                cap_stage: Some(stage),
                ..ReplayOptions::default()
            },
        );
        assert_eq!(out.replay.queries.refused, 0);
        out.report.queries[0].clone()
    };
    // Shortlist stage: the cap demotes A20 and A40 before the window is cut,
    // so the reranker sees A1, B1, C1, D1 and the demoted chunks trail it.
    assert_eq!(
        places(&at(CapStage::Shortlist)),
        ["b.rs:1", "c.rs:1", "a.rs:1", "d.rs:1", "a.rs:20", "a.rs:40"]
    );
    // Results stage: the reranker sees the uncapped head A1, A20, B1, A40 and
    // lifts A40 to the front; the cap then demotes the second and third a.rs
    // chunks behind the other files.
    assert_eq!(
        places(&at(CapStage::Results)),
        ["a.rs:40", "b.rs:1", "c.rs:1", "d.rs:1", "a.rs:20", "a.rs:1"]
    );
    // Without a cap the stage does not matter.
    let uncapped = |stage| {
        replayed(
            &source,
            ReplayOptions {
                cap: Some(0),
                cap_stage: Some(stage),
                ..ReplayOptions::default()
            },
        )
        .report
        .queries[0]
            .evidence
            .clone()
    };
    assert_eq!(uncapped(CapStage::Shortlist), uncapped(CapStage::Results));
    assert_eq!(uncapped(CapStage::Results), source.queries[0].evidence);
}

#[test]
fn replay_vote_adds_the_reciprocal_rank_of_the_reranker_to_the_fusion_score() {
    let source = full_dump();
    let dumped_arm = source.queries[0].arm.clone().unwrap();
    let vote = |weight: f32| {
        replayed(
            &source,
            ReplayOptions {
                window: Some(3),
                rerank_mode: Some(RerankMode::Vote),
                vote_weight: Some(weight),
                ..ReplayOptions::default()
            },
        )
    };
    // Window A1, A20, B1; the reranker ranks them B1 (0.5), A20 (0.4), A1
    // (0.2), so the reranker ranks are 3, 2, 1.
    let voted = |spec: &Spec, rank: f32, weight: f32| {
        expected_fusion(spec, &dumped_arm) + weight / (60.0 + rank)
    };
    for weight in [0.0, 1.0, 3.0, 5.0, 10.0] {
        let mut expected = [(&A1, 3.0), (&A20, 2.0), (&B1, 1.0)];
        expected.sort_by(|a, b| {
            voted(b.0, b.1, weight)
                .total_cmp(&voted(a.0, a.1, weight))
                .then_with(|| a.0.path.cmp(b.0.path))
                .then(a.0.start.cmp(&b.0.start))
        });
        let mut want: Vec<String> = expected
            .iter()
            .map(|(spec, _)| format!("{}:{}", spec.path, spec.start))
            .collect();
        want.extend(["a.rs:40", "c.rs:1", "d.rs:1"].map(str::to_owned));
        let out = vote(weight);
        assert_eq!(places(&out.report.queries[0]), want, "vote weight {weight}");
        assert!(out.report.queries[0].reranked);
    }
    // Spelled out: a light vote keeps fusion order, a heavy one is the
    // reranker's order.
    assert_eq!(
        &places(&vote(1.0).report.queries[0])[..3],
        ["a.rs:1", "a.rs:20", "b.rs:1"]
    );
    assert_eq!(
        &places(&vote(10.0).report.queries[0])[..3],
        ["b.rs:1", "a.rs:20", "a.rs:1"]
    );
    // Window rows carry the reranker's score, tail rows none.
    let evidence = &vote(10.0).report.queries[0].evidence;
    assert_eq!(evidence[0].reranker_score, Some(0.5));
    assert!(evidence[3..].iter().all(|row| row.reranker_score.is_none()));
    // rrf_k enters the vote: at rrf_k 1000 a rank step is worth about 1e-6
    // per unit of weight, less than the fusion gaps (3e-6 and 5e-6), so a
    // weight of 1 leaves fusion order, whereas the same vote at the dumped
    // rrf_k of 60 (2.6e-4 per step) would put B1 first.
    let flat = replayed(
        &source,
        ReplayOptions {
            window: Some(3),
            rerank_mode: Some(RerankMode::Vote),
            vote_weight: Some(1.0),
            rrf_k: Some(1000.0),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(
        &places(&flat.report.queries[0])[..3],
        ["a.rs:1", "a.rs:20", "b.rs:1"]
    );
}

#[test]
fn replay_refuses_a_window_holding_an_unscored_candidate_and_counts_it() {
    // Window 4, cap 2: only A1, A20, B1, C1 reached the reranker.
    let arm = dump_arm(4, 3, 2, RerankMode::Replace);
    let wide = dumped_query(
        "wide pool",
        &["b.rs"],
        &SIX,
        &arm,
        RerankOutcome::Scored,
        900.0,
    );
    // A pool that fits the shortlist entirely: any window is exact.
    let small = dumped_query(
        "small pool",
        &["b.rs"],
        &[A1, A20, B1],
        &arm,
        RerankOutcome::Scored,
        500.0,
    );
    let source = dump_of(&[wide, small]);
    let out = replayed(
        &source,
        ReplayOptions {
            window: Some(5),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(out.replay.queries.replayed, 1);
    assert_eq!(out.replay.queries.refused, 1);
    assert_eq!(
        out.replay.refused,
        BTreeMap::from([(ReplayRefusal::UnscoredWindow, 1)])
    );
    let refused = &out.report.queries[0];
    assert!(refused.skipped && refused.evidence.is_empty());
    assert_eq!(refused.outcome, "skipped");
    assert_eq!(refused.replay_refusal, Some(ReplayRefusal::UnscoredWindow));
    assert!(refused.reason.contains("d.rs:1"), "{}", refused.reason);
    assert_eq!(out.replay.refused_queries.len(), 1);
    assert_eq!(out.replay.refused_queries[0].query, "wide pool");
    assert_eq!(out.replay.refused_queries[0].workspace, "ws");
    assert_eq!(
        out.replay.refused_queries[0].reason,
        ReplayRefusal::UnscoredWindow
    );
    // The refused query is a skipped one for the aggregate; the other is real.
    assert_eq!(
        (out.report.executed_count, out.report.skipped_count),
        (1, 1)
    );
    assert_eq!(out.report.mrr, Some(1.0));
    assert!(!out.report.queries[1].skipped);
    assert_eq!(out.report.queries[1].replay_refusal, None);
    assert_eq!(
        places(&out.report.queries[1]),
        ["b.rs:1", "a.rs:20", "a.rs:1"]
    );

    // A cap different from the dumped one pulls other rows into the window.
    let no_cap = replayed(
        &source,
        ReplayOptions {
            cap: Some(0),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(
        no_cap.replay.refused,
        BTreeMap::from([(ReplayRefusal::UnscoredWindow, 1)])
    );
    assert!(
        no_cap.report.queries[0].reason.contains("a.rs:40"),
        "{}",
        no_cap.report.queries[0].reason
    );
    // Without the reranker no score is needed, whatever the window: the
    // fused order is exact.
    let off = replayed(
        &source,
        ReplayOptions {
            window: Some(5),
            cap: Some(0),
            rerank_mode: Some(RerankMode::Off),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(off.replay.queries.refused, 0);
    assert_eq!(
        places(&off.report.queries[0]),
        ["a.rs:1", "a.rs:20", "b.rs:1"]
    );
    // The summary names the reason and the query.
    let summary = out.render_summary();
    assert!(summary.contains("refused unscored_window: 1"), "{summary}");
    assert!(summary.contains("wide pool"), "{summary}");
}

#[test]
fn replay_refuses_the_reranker_when_the_dumped_search_had_no_scores() {
    let failed_arm = dump_arm(4, 3, 2, RerankMode::Replace);
    let failed = dumped_query(
        "reranker down",
        &["a.rs"],
        &SIX,
        &failed_arm,
        RerankOutcome::Failed,
        0.0,
    );
    // The dumped search fell back to fusion order and did not rerank.
    assert!(!failed.reranked);
    let source = dump_of(&[failed]);
    for mode in [None, Some(RerankMode::Replace)] {
        let out = replayed(
            &source,
            ReplayOptions {
                rerank_mode: mode,
                ..ReplayOptions::default()
            },
        );
        assert_eq!(
            out.replay.refused,
            BTreeMap::from([(ReplayRefusal::RerankNotScored, 1)])
        );
        assert!(
            out.report.queries[0].reason.contains("failed"),
            "{}",
            out.report.queries[0].reason
        );
    }
    let vote = replayed(
        &source,
        ReplayOptions {
            rerank_mode: Some(RerankMode::Vote),
            vote_weight: Some(1.0),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(vote.replay.refused[&ReplayRefusal::RerankNotScored], 1);
    // Fusion order needs no scores, and is exactly what the failed search
    // returned.
    let off = replayed(
        &source,
        ReplayOptions {
            rerank_mode: Some(RerankMode::Off),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(off.replay.queries.refused, 0);
    assert_eq!(off.report.queries[0].evidence, source.queries[0].evidence);

    // A dump taken with the reranker off cannot be reranked afterwards.
    let off_arm = dump_arm(4, 3, 2, RerankMode::Off);
    let source = dump_of(&[dumped_query(
        "reranker off",
        &["a.rs"],
        &SIX,
        &off_arm,
        RerankOutcome::Off,
        0.0,
    )]);
    let replace = replayed(
        &source,
        ReplayOptions {
            rerank_mode: Some(RerankMode::Replace),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(replace.replay.refused[&ReplayRefusal::RerankNotScored], 1);
}

#[test]
fn replay_refuses_a_dump_whose_fusion_or_cap_does_not_reproduce() {
    let arm = dump_arm(4, 3, 2, RerankMode::Replace);
    let make = |text: &str| dumped_query(text, &["b.rs"], &SIX, &arm, RerankOutcome::Scored, 900.0);
    let good = make("good");

    let mut drifted = make("drifted score");
    let bits = drifted.candidates[2].fusion_score.to_bits();
    drifted.candidates[2].fusion_score = f32::from_bits(bits + 1);

    let mut shuffled = make("shuffled");
    shuffled.candidates.swap(0, 1);

    let mut misranked = make("wrong rank");
    misranked.candidates[3].fused_rank = 9;

    let mut mis_capped = make("wrong cap");
    mis_capped.candidates[5].capped_rank = 2;

    let mut mis_cut = make("wrong cut");
    mis_cut.candidates[0].shortlisted = false;

    let source = dump_of(&[good, drifted, shuffled, misranked, mis_capped, mis_cut]);
    let out = replayed(&source, ReplayOptions::default());
    assert_eq!(out.replay.queries.replayed, 1);
    assert_eq!(
        out.replay.refused,
        BTreeMap::from([
            (ReplayRefusal::FusionMismatch, 3),
            (ReplayRefusal::ShortlistMismatch, 2)
        ])
    );
    let reasons: Vec<Option<ReplayRefusal>> = out
        .report
        .queries
        .iter()
        .map(|query| query.replay_refusal)
        .collect();
    assert_eq!(
        reasons,
        [
            None,
            Some(ReplayRefusal::FusionMismatch),
            Some(ReplayRefusal::FusionMismatch),
            Some(ReplayRefusal::FusionMismatch),
            Some(ReplayRefusal::ShortlistMismatch),
            Some(ReplayRefusal::ShortlistMismatch),
        ]
    );
    // A dump that fails to reproduce at its own arm is refused at every arm.
    let off = replayed(
        &source,
        ReplayOptions {
            rerank_mode: Some(RerankMode::Off),
            cap: Some(0),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(off.replay.queries.refused, 5);
}

/// Four candidates whose fused order moves with every fusion setting.
const WEIGHT_DUMP: [Spec; 4] = [
    spec("p.rs", 1, Some(1), None, SourceRole::Source, 0.1, 100),
    spec("q.rs", 1, None, Some(1), SourceRole::Source, 0.2, 100),
    spec("r.rs", 1, Some(2), Some(3), SourceRole::Test, 0.3, 100),
    Spec {
        lsp: Some(1),
        ..spec("s.rs", 1, None, None, SourceRole::Source, 0.4, 100)
    },
];

#[test]
fn replay_without_the_reranker_reorders_as_fusion_would_under_other_settings() {
    // Dumped: R 2/62 + 1/63, P 2/61 + 0.004, then Q 1/61 + 0.004 and S
    // 1/61 + 0.004 (equal scores: the path breaks the tie).
    let arm = dump_arm(4, 4, 0, RerankMode::Replace);
    let source = dump_of(&[dumped_query(
        "weights",
        &["p.rs"],
        &WEIGHT_DUMP,
        &arm,
        RerankOutcome::Scored,
        500.0,
    )]);
    let off = |options: ReplayOptions| {
        let out = replayed(
            &source,
            ReplayOptions {
                rerank_mode: Some(RerankMode::Off),
                ..options
            },
        );
        assert_eq!(out.replay.queries.refused, 0);
        let query = out.report.queries[0].clone();
        assert!(!query.reranked);
        assert!(
            query
                .evidence
                .iter()
                .all(|row| row.reranker_score.is_none())
        );
        places(&query)
    };
    assert_eq!(
        off(ReplayOptions::default()),
        ["r.rs:1", "p.rs:1", "q.rs:1", "s.rs:1"]
    );
    // Lexical weight 4: Q 4/61 + 0.004 passes P.
    assert_eq!(
        off(ReplayOptions {
            lexical_weight: Some(4.0),
            ..ReplayOptions::default()
        }),
        ["r.rs:1", "q.rs:1", "p.rs:1", "s.rs:1"]
    );
    // LSP weight 3: S 3/61 + 0.004 passes R and P.
    assert_eq!(
        off(ReplayOptions {
            lsp_weight: Some(3.0),
            ..ReplayOptions::default()
        }),
        ["s.rs:1", "r.rs:1", "p.rs:1", "q.rs:1"]
    );
    // Semantic weight 0.5: P 0.5/61 + 0.004 falls behind Q and S (1/61 +
    // 0.004); R is 0.5/62 + 1/63.
    assert_eq!(
        off(ReplayOptions {
            semantic_weight: Some(0.5),
            ..ReplayOptions::default()
        }),
        ["r.rs:1", "q.rs:1", "s.rs:1", "p.rs:1"]
    );
    // rrf_k 1 sharpens rank one: P 2/2 + 0.004 beats R 2/3 + 1/4.
    assert_eq!(
        off(ReplayOptions {
            rrf_k: Some(1.0),
            ..ReplayOptions::default()
        }),
        ["p.rs:1", "r.rs:1", "q.rs:1", "s.rs:1"]
    );
    // Source prior 0.05 lifts every `source` row but not R (a test file).
    assert_eq!(
        off(ReplayOptions {
            prior: Some(0.05),
            ..ReplayOptions::default()
        }),
        ["p.rs:1", "q.rs:1", "s.rs:1", "r.rs:1"]
    );
    // Source prior 0 removes the advantage the three source rows had over
    // nothing: the order is unchanged here, but it is recomputed, not copied.
    assert_eq!(
        off(ReplayOptions {
            prior: Some(0.0),
            ..ReplayOptions::default()
        }),
        ["r.rs:1", "p.rs:1", "q.rs:1", "s.rs:1"]
    );
    // The window plays no part in the fused order.
    assert_eq!(
        off(ReplayOptions {
            window: Some(1),
            ..ReplayOptions::default()
        }),
        off(ReplayOptions::default())
    );
    // Settings given together compose: lexical weight 4 and prior 0.05 give
    // Q 4/61 + 0.05, R 2/62 + 4/63, P 2/61 + 0.05 and S 1/61 + 0.05.
    assert_eq!(
        off(ReplayOptions {
            lexical_weight: Some(4.0),
            prior: Some(0.05),
            ..ReplayOptions::default()
        }),
        ["q.rs:1", "r.rs:1", "p.rs:1", "s.rs:1"]
    );
}

#[test]
fn replay_round_trips_through_a_saved_report_and_compare_accepts_it() {
    let arm = dump_arm(6, 6, 0, RerankMode::Replace);
    let query = |text: &str, expected: &[&str], tags: &[&str]| {
        let mut report = dumped_query(text, expected, &SIX, &arm, RerankOutcome::Scored, 900.0);
        report.query.tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
        report
    };
    let source = dump_of(&[
        query("first", &["a.rs"], &["keyword"]),
        query("second", &["d.rs"], &["keyword-agnostic"]),
        query("third", &["missing.rs"], &["keyword"]),
        skipped_query("ws", "fourth"),
    ]);
    let out = replayed(&source, ReplayOptions::default());

    // Same evidence and metrics per query, and the same aggregate.
    for (before, after) in source.queries.iter().zip(&out.report.queries) {
        assert_eq!(after.evidence, before.evidence, "{}", before.query.query);
        assert_eq!(after.metrics, before.metrics, "{}", before.query.query);
        assert_eq!(after.skipped, before.skipped);
        assert_eq!(after.outcome, before.outcome);
        assert_eq!(after.query.query, before.query.query);
    }
    let headline = |report: &AggregateReport| {
        let mut value = serde_json::to_value(report).unwrap();
        let object = value.as_object_mut().unwrap();
        for key in [
            "queries",
            "median_latency_ms",
            "p95_latency_ms",
            "stage_latency",
        ] {
            object.remove(key);
        }
        value
    };
    assert_eq!(headline(&out.report), headline(&source));
    assert_eq!(out.report.mrr, source.mrr);
    assert_eq!(out.report.mean_ndcg_at_10, source.mean_ndcg_at_10);
    assert_eq!(out.report.by_tag, source.by_tag);
    assert_eq!(out.report.executed_count, 3);
    assert_eq!(out.replay.queries.source_skipped, 1);

    // Saved and loaded, `compare` pairs it with the real report: every
    // metric ties.
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("dump.json");
    let replay_path = temp.path().join("replay.json");
    std::fs::write(&source_path, serde_json::to_string_pretty(&source).unwrap()).unwrap();
    let from_file = replay_file(&source_path, &ReplayOptions::default()).unwrap();
    assert_eq!(
        from_file.replay.source.as_deref(),
        Some(source_path.display().to_string().as_str())
    );
    std::fs::write(
        &replay_path,
        serde_json::to_string_pretty(&from_file.to_json().unwrap()).unwrap(),
    )
    .unwrap();
    let comparison = compare_files(&source_path, &replay_path, &CompareOptions::default()).unwrap();
    assert_eq!(comparison.paired, 3);
    assert_eq!(
        (comparison.skipped_pairs, comparison.unpaired_baseline),
        (1, 0)
    );
    let overall = comparison.overall.unwrap();
    assert_eq!((overall.wins, overall.losses, overall.ties), (0, 0, 3));
    assert_eq!(overall.mrr.mean_delta, 0.0);
    assert_eq!(overall.ndcg_at_10.mean_delta, 0.0);
    // The replayed report is an ordinary report to the loader.
    let loaded = load_report(&replay_path).unwrap();
    assert_eq!(loaded.mrr, source.mrr);
    assert_eq!(loaded.executed_count, 3);
    // But not a dump: it carries no candidates to replay again.
    let error = replay(&loaded, &ReplayOptions::default()).unwrap_err();
    assert!(
        format!("{error:#}").contains("no candidate dump"),
        "{error:#}"
    );

    // A refused query reads as skipped to `compare`.
    let narrow = dump_arm(4, 3, 2, RerankMode::Replace);
    let source = dump_of(&[
        dumped_query(
            "narrow",
            &["b.rs"],
            &SIX,
            &narrow,
            RerankOutcome::Scored,
            900.0,
        ),
        dumped_query(
            "small",
            &["b.rs"],
            &[A1, A20, B1],
            &narrow,
            RerankOutcome::Scored,
            500.0,
        ),
    ]);
    let out = replayed(
        &source,
        ReplayOptions {
            window: Some(6),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(out.replay.queries.refused, 1);
    let comparison = compare(&source, &out.report, &CompareOptions::default());
    assert_eq!(comparison.paired, 1);
    assert_eq!(
        (comparison.skipped_pairs, comparison.skipped_candidate),
        (1, 1)
    );
}

#[test]
fn replay_estimates_reranking_latency_from_a_cost_model_fitted_on_the_dump() {
    // Two queries whose measured rerank time is exactly 100 + 2 ms per
    // estimated token: the 6-row pool holds 2100 tokens, the 3-row one 700.
    let arm = dump_arm(6, 6, 0, RerankMode::Replace);
    let source = dump_of(&[
        dumped_query("big", &["a.rs"], &SIX, &arm, RerankOutcome::Scored, 4300.0),
        dumped_query(
            "small",
            &["a.rs"],
            &[A1, A20, B1],
            &arm,
            RerankOutcome::Scored,
            1500.0,
        ),
    ]);
    let out = replayed(&source, ReplayOptions::default());
    let model = out.replay.latency.cost_model;
    assert_eq!(model.basis, CostBasis::Fitted);
    assert!((model.intercept_ms - 100.0).abs() < 1e-9, "{model:?}");
    assert!((model.ms_per_token - 2.0).abs() < 1e-12, "{model:?}");
    let fit = model.fit.unwrap();
    assert!((fit.r_squared - 1.0).abs() < 1e-12);
    assert_eq!(fit.samples, 2);
    assert!(out.replay.latency.estimated);
    assert!(out.replay.latency.note.contains("estimates"));

    // At the dumped arm the estimate reproduces the measurement.
    let timings = out.report.queries[0].timings.unwrap();
    assert!((timings.reranking_ms - 4300.0).abs() < 1e-6);
    assert!((timings.total_ms - 4321.0).abs() < 1e-6);
    assert_eq!(timings.query_embedding_ms, 10.0);

    // A window of three (A1 + A20 + B1 = 700 tokens) estimates 1500 ms and
    // the total follows: dumped total 4321, minus 4300 measured, plus 1500.
    let narrow = replayed(
        &source,
        ReplayOptions {
            window: Some(3),
            ..ReplayOptions::default()
        },
    );
    let timings = narrow.report.queries[0].timings.unwrap();
    assert!((timings.reranking_ms - 1500.0).abs() < 1e-6, "{timings:?}");
    assert!((timings.total_ms - 1521.0).abs() < 1e-6, "{timings:?}");
    // Fusion-order arms spend nothing on the reranker.
    let off = replayed(
        &source,
        ReplayOptions {
            rerank_mode: Some(RerankMode::Off),
            ..ReplayOptions::default()
        },
    );
    let timings = off.report.queries[0].timings.unwrap();
    assert_eq!(timings.reranking_ms, 0.0);
    assert!((timings.total_ms - 21.0).abs() < 1e-6);
    assert_eq!(
        off.report.stage_latency.unwrap().reranking_ms.max,
        0.0,
        "the aggregate follows the estimates"
    );
    // The measurements stay under `replay.original`.
    let original = &narrow.replay.original;
    assert_eq!(original.query_timings.len(), 2);
    assert_eq!(original.query_timings[0].timings.reranking_ms, 4300.0);
    assert_eq!(original.query_timings[0].query, "big");
    assert_eq!(original.query_timings[1].index, 1);
    assert_eq!(original.median_latency_ms, source.median_latency_ms);
    assert_eq!(original.stage_latency, source.stage_latency);
}

#[test]
fn the_cost_model_falls_back_when_the_fit_is_weak_or_missing() {
    let fallback_slope = 1000.0 / 290.0;
    let check_fallback = |model: CostModel| {
        assert_eq!(model.basis, CostBasis::Fallback);
        assert_eq!(model.intercept_ms, 400.0);
        assert!((model.ms_per_token - fallback_slope).abs() < 1e-12);
        assert!((model.estimate_ms(2900) - (400.0 + 10_000.0)).abs() < 1e-9);
    };

    let exact = CostModel::from_samples(&[(0.0, 50.0), (100.0, 250.0), (400.0, 850.0)]);
    assert_eq!(exact.basis, CostBasis::Fitted);
    assert!((exact.intercept_ms - 50.0).abs() < 1e-9);
    assert!((exact.ms_per_token - 2.0).abs() < 1e-12);
    assert!((exact.estimate_ms(10) - 70.0).abs() < 1e-9);

    // Time unrelated to the tokens: R^2 below one half.
    let noisy = CostModel::from_samples(&[
        (100.0, 900.0),
        (200.0, 100.0),
        (300.0, 800.0),
        (400.0, 200.0),
        (500.0, 700.0),
    ]);
    let fit = noisy.fit.unwrap();
    assert!(fit.r_squared < MIN_R_SQUARED, "{fit:?}");
    check_fallback(noisy);

    // A strong fit with a negative slope is nonsense for a cost.
    let falling = CostModel::from_samples(&[(100.0, 900.0), (200.0, 700.0), (300.0, 500.0)]);
    assert!(falling.fit.unwrap().r_squared > 0.99);
    check_fallback(falling);

    // No points, one point, or no spread in the tokens: no fit at all.
    for samples in [
        &[][..],
        &[(100.0, 500.0)][..],
        &[(100.0, 500.0), (100.0, 700.0)][..],
    ] {
        let model = CostModel::from_samples(samples);
        assert!(model.fit.is_none());
        check_fallback(model);
    }
    // Equal times have no variance to explain.
    let flat = CostModel::from_samples(&[(100.0, 500.0), (200.0, 500.0), (300.0, 500.0)]);
    assert_eq!(flat.fit.unwrap().r_squared, 0.0);
    check_fallback(flat);
    // An estimate is never negative.
    let steep = CostModel::from_samples(&[(100.0, 0.0), (200.0, 1000.0)]);
    assert_eq!(steep.basis, CostBasis::Fitted);
    assert_eq!(steep.estimate_ms(0), 0.0);

    // End to end: a dump without reranker timings replays with the fallback.
    let arm = dump_arm(6, 6, 0, RerankMode::Replace);
    let mut query = dumped_query("untimed", &["a.rs"], &SIX, &arm, RerankOutcome::Scored, 0.0);
    query.timings = None;
    let out = replayed(&dump_of(&[query]), ReplayOptions::default());
    assert_eq!(out.replay.latency.cost_model.basis, CostBasis::Fallback);
    assert!(out.report.queries[0].timings.is_none());
    assert!(out.render_summary().contains("ESTIMATED"));
}

#[test]
fn replay_carries_over_skipped_and_failed_queries_and_refuses_undumped_ones() {
    let arm = dump_arm(4, 3, 2, RerankMode::Replace);
    let make = |text: &str| dumped_query(text, &["b.rs"], &SIX, &arm, RerankOutcome::Scored, 900.0);
    let skipped = skipped_query("ws", "skipped upstream");
    let failed = QueryReport::failed(
        compare_query("ws", "search failed", &[]),
        "search failed: boom".to_owned(),
    );
    // A real search that was not dumped: it has timings but no arm.
    let mut undumped = make("undumped");
    undumped.candidates.clear();
    undumped.arm = None;
    undumped.rerank_outcome = None;
    let source = dump_of(&[make("dumped"), skipped, failed, undumped]);
    let out = replayed(&source, ReplayOptions::default());
    assert_eq!(
        out.replay.queries,
        ReplayCounts {
            total: 4,
            replayed: 1,
            refused: 1,
            source_skipped: 1,
            source_failed: 1,
        }
    );
    assert_eq!(
        out.replay.refused,
        BTreeMap::from([(ReplayRefusal::NotDumped, 1)])
    );
    assert_eq!(
        out.report.queries[1].reason,
        "optional workspace not provided"
    );
    assert_eq!(out.report.queries[1].replay_refusal, None);
    assert_eq!(out.report.queries[2].outcome, "failure");
    assert_eq!(out.report.queries[2].reason, "search failed: boom");
    assert!(!out.report.queries[2].skipped);
    assert_eq!(
        out.report.queries[3].replay_refusal,
        Some(ReplayRefusal::NotDumped)
    );
    assert!(out.report.queries[3].skipped);
    assert_eq!(out.report.failure_count, 1, "only the failed search");

    // No dump anywhere is an error, not a pile of refusals.
    let plain = dump_of(&[QueryReport::failed(
        compare_query("ws", "x", &[]),
        "y".to_owned(),
    )]);
    let error = replay(&plain, &ReplayOptions::default()).unwrap_err();
    assert!(
        format!("{error:#}").contains("--dump-candidates"),
        "{error:#}"
    );

    // Queries dumped under different arms cannot share one replayed arm.
    let other = dump_arm(5, 3, 2, RerankMode::Replace);
    let mixed = dump_of(&[
        make("one"),
        dumped_query("two", &["b.rs"], &SIX, &other, RerankOutcome::Scored, 900.0),
    ]);
    let error = replay(&mixed, &ReplayOptions::default()).unwrap_err();
    assert!(format!("{error:#}").contains("different arms"), "{error:#}");
    // Their result windows may differ.
    let other_k = CandidateArm {
        k: 2,
        ..arm.clone()
    };
    let same_arm = dump_of(&[
        make("one"),
        dumped_query(
            "two",
            &["b.rs"],
            &SIX,
            &other_k,
            RerankOutcome::Scored,
            900.0,
        ),
    ]);
    let out = replayed(&same_arm, ReplayOptions::default());
    assert_eq!(out.report.queries[0].evidence.len(), 3);
    assert_eq!(out.report.queries[1].evidence.len(), 2);
    assert_eq!(out.replay.queries.refused, 0);
}

#[test]
fn replay_options_are_validated_and_default_to_the_dumped_arm() {
    let arm = dump_arm(4, 3, 2, RerankMode::Replace);
    let resolve = |options: ReplayOptions| options.resolve(&arm);
    let resolved = resolve(ReplayOptions::default()).unwrap();
    assert_eq!(resolved.window, 4);
    assert_eq!(resolved.fusion, arm.fusion);
    assert_eq!(resolved.rrf_k, 60.0);
    assert_eq!(resolved.cap_stage, CapStage::Shortlist);
    assert_eq!(resolved.rerank_mode, RerankMode::Replace);
    assert_eq!((resolved.source_prior, resolved.vote_weight), (None, None));

    let with = |change: fn(&mut ReplayOptions)| {
        let mut options = ReplayOptions::default();
        change(&mut options);
        options
    };
    let bad = [
        with(|o| o.window = Some(0)),
        with(|o| o.cap = Some(101)),
        with(|o| o.rrf_k = Some(0.0)),
        with(|o| o.rrf_k = Some(f32::NAN)),
        with(|o| o.semantic_weight = Some(0.0)),
        with(|o| o.lexical_weight = Some(-1.0)),
        with(|o| o.lsp_weight = Some(f32::INFINITY)),
        with(|o| o.prior = Some(-0.1)),
        // Vote needs its weight, and the weight needs vote.
        with(|o| o.rerank_mode = Some(RerankMode::Vote)),
        with(|o| o.vote_weight = Some(1.0)),
        with(|o| {
            o.rerank_mode = Some(RerankMode::Vote);
            o.vote_weight = Some(-1.0);
        }),
    ];
    for options in bad {
        assert!(resolve(options).is_err(), "{options:?}");
    }
    let good = [
        with(|o| o.cap = Some(0)),
        with(|o| o.cap = Some(100)),
        with(|o| o.prior = Some(0.0)),
        with(|o| {
            o.rerank_mode = Some(RerankMode::Vote);
            o.vote_weight = Some(0.0);
        }),
    ];
    for options in good {
        assert!(resolve(options).is_ok(), "{options:?}");
    }

    assert_eq!("shortlist".parse::<CapStage>(), Ok(CapStage::Shortlist));
    assert_eq!("results".parse::<CapStage>(), Ok(CapStage::Results));
    assert!("both".parse::<CapStage>().is_err());
    for (text, mode) in [
        ("replace", RerankMode::Replace),
        ("vote", RerankMode::Vote),
        ("off", RerankMode::Off),
    ] {
        assert_eq!(text.parse::<RerankMode>(), Ok(mode));
        assert_eq!(mode.as_str(), text);
    }
    assert!("margin".parse::<RerankMode>().is_err());
}

#[test]
fn the_replay_block_and_refusal_flag_are_written_into_the_report_json() {
    let arm = dump_arm(4, 3, 2, RerankMode::Replace);
    let source = dump_of(&[
        dumped_query("wide", &["b.rs"], &SIX, &arm, RerankOutcome::Scored, 900.0),
        dumped_query(
            "small",
            &["b.rs"],
            &[A1, A20, B1],
            &arm,
            RerankOutcome::Scored,
            500.0,
        ),
    ]);
    let out = replayed(
        &source,
        ReplayOptions {
            window: Some(5),
            semantic_weight: Some(3.0),
            ..ReplayOptions::default()
        },
    );
    let json = out.to_json().unwrap();
    let block = &json["replay"];
    assert_eq!(block["arm"]["window"], 5);
    assert_eq!(block["arm"]["cap_stage"], "shortlist");
    assert_eq!(block["arm"]["rerank_mode"], "replace");
    assert_eq!(block["arm"]["fusion"]["semantic_weight"], 3.0);
    assert_eq!(block["dumped_arm"]["rerank_candidate_count"], 4);
    assert_eq!(block["dumped_arm"]["fusion"]["semantic_weight"], 2.0);
    assert_eq!(block["queries"]["refused"], 1);
    assert_eq!(block["refused"]["unscored_window"], 1);
    assert_eq!(block["refused_queries"][0]["query"], "wide");
    assert_eq!(block["refused_queries"][0]["reason"], "unscored_window");
    assert_eq!(block["latency"]["estimated"], true);
    assert_eq!(block["latency"]["cost_model"]["basis"], "fitted");
    assert_eq!(
        block["original"]["query_timings"][0]["timings"]["reranking_ms"],
        900.0
    );
    assert_eq!(json["queries"][0]["replay_refusal"], "unscored_window");
    assert_eq!(json["queries"][0]["outcome"], "skipped");
    assert_eq!(json["queries"][0]["skipped"], true);
    assert!(json["queries"][1].get("replay_refusal").is_none());
    // Replay outputs carry no dump of their own.
    for query in json["queries"].as_array().unwrap() {
        for key in ["candidates", "arm", "rerank_outcome", "notes"] {
            assert!(query.get(key).is_none(), "{key}");
        }
    }
    // The block reads back unchanged.
    let back: ReplayBlock = serde_json::from_value(block.clone()).unwrap();
    assert_eq!(back, out.replay);
    // And the report part is an ordinary report.
    let loaded: AggregateReport = serde_json::from_value(json).unwrap();
    assert_eq!(loaded.executed_count, 1);
    assert_eq!(loaded.skipped_count, 1);
}

#[test]
fn replay_breaks_reranker_ties_by_position_in_the_replayed_window() {
    // The reranker scores both candidates 0.7. Dumped, z.rs fuses ahead of
    // a.rs (2/62 + 0.004 against 2/61), so z.rs was request index 0 and wins
    // the tie; path order would put a.rs first.
    let z = spec("z.rs", 1, Some(2), None, SourceRole::Source, 0.7, 100);
    let a = spec("a.rs", 1, Some(1), None, SourceRole::Test, 0.7, 100);
    let arm = dump_arm(2, 2, 0, RerankMode::Replace);
    let source = dump_of(&[dumped_query(
        "tie",
        &["a.rs"],
        &[z, a],
        &arm,
        RerankOutcome::Scored,
        500.0,
    )]);
    assert_eq!(places(&source.queries[0]), ["z.rs:1", "a.rs:1"]);
    let same = replayed(&source, ReplayOptions::default());
    assert_eq!(same.report.queries[0].evidence, source.queries[0].evidence);

    // Without the source prior a.rs fuses first, becomes request index 0, and
    // now wins the tie: the position is the replayed one, not the dumped one.
    let flipped = replayed(
        &source,
        ReplayOptions {
            prior: Some(0.0),
            ..ReplayOptions::default()
        },
    );
    assert_eq!(places(&flipped.report.queries[0]), ["a.rs:1", "z.rs:1"]);
    assert_eq!(
        flipped.report.queries[0].metrics.first_expected_rank,
        Some(1)
    );
}
