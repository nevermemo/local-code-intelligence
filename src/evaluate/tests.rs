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
