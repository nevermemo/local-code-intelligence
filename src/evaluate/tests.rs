use super::*;
use crate::filter::FilterRequest;

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
    // p95 = 4th value):
    //   embedding  10 20 30 40       p50 25   p95 40
    //   lancedb     1  2  3  5       p50 2.5  p95 5
    //   lexical     7  7  7  9       p50 7    p95 9
    //   lsp         0  0  0  0       p50 0    p95 0
    //   fusion      4  6  8 12       p50 7    p95 12
    //   reranking 100 200 300 400    p50 250  p95 400
    //   total     150 250 350 500    p50 300  p95 500
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
    let check = |stage: LatencyPercentiles, p50: f64, p95: f64| {
        assert_close(stage.p50, p50);
        assert_close(stage.p95, p95);
    };
    check(stages.query_embedding_ms, 25.0, 40.0);
    check(stages.lancedb_search_ms, 2.5, 5.0);
    check(stages.lexical_search_ms, 7.0, 9.0);
    check(stages.lsp_search_ms, 0.0, 0.0);
    check(stages.fusion_ms, 7.0, 12.0);
    check(stages.reranking_ms, 250.0, 400.0);
    check(stages.total_ms, 300.0, 500.0);
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
        assert_eq!(query.tags.len(), 1, "{}", query.query);
        assert!(
            ["keyword", "keyword-agnostic"].contains(&query.tags[0].as_str()),
            "{}",
            query.query
        );
    }
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
    let baseline: Vec<QueryReport> = (0..8)
        .map(|index| rr_only(&format!("q{index}"), 0.125 * f64::from(index)))
        .collect();
    let better: Vec<QueryReport> = (0..8)
        .map(|index| rr_only(&format!("q{index}"), 0.125 * f64::from(index) + 0.0625))
        .collect();

    let up = default_compare(&baseline, &better).overall.unwrap();
    assert_eq!((up.wins, up.losses, up.ties), (8, 0, 0));
    assert!(up.mrr.ci_low > 0.0, "{:?}", up.mrr);
    assert!(up.mrr.significant);

    let down = default_compare(&better, &baseline).overall.unwrap();
    assert_eq!((down.wins, down.losses, down.ties), (0, 8, 0));
    assert!(down.mrr.ci_high < 0.0, "{:?}", down.mrr);
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
