use super::*;

#[test]
fn evaluation_definition_and_workspace_mappings_are_portable() {
    let temp = tempfile::tempdir().unwrap();
    let definition_path = temp.path().join("evaluation.toml");
    std::fs::write(
        &definition_path,
        r#"
[[workspaces]]
key = "self"
required = true

[[workspaces]]
key = "gust"
required = false

[[queries]]
workspace = "self"
query = "find evaluator"
expected_paths = ["src/evaluate.rs"]
expected_roles = ["source"]
required_snippet = "query_metrics"
"#,
    )
    .unwrap();

    let definition = load_definition(&definition_path).unwrap();
    assert_eq!(definition.workspaces.len(), 2);
    assert_eq!(definition.queries[0].expected_paths, ["src/evaluate.rs"]);

    let mappings = parse_workspace_mappings(&[
        "self=C:\\work\\local-code-intelligence".to_owned(),
        "gust=C:\\work\\gpu-dialect-v0".to_owned(),
    ])
    .unwrap();
    assert_eq!(mappings.len(), 2);
    assert!(parse_workspace_mappings(&["missing-separator".to_owned()]).is_err());
    assert!(parse_workspace_mappings(&["=C:\\work".to_owned()]).is_err());
    assert!(
        parse_workspace_mappings(&["self=C:\\one".to_owned(), "self=C:\\two".to_owned()]).is_err()
    );
}

#[test]
fn evaluation_expectations_drive_reports_but_not_ranking() {
    let query = evaluation_query();
    let evidence = vec![evaluation_evidence(
        "src/evaluate.rs",
        "source",
        "pub fn query_metrics()",
    )];
    let metrics = query_metrics(&query, &evidence, true);
    assert!(metrics.success);
    assert!(metrics.hit_at_1);
    assert_eq!(metrics.first_expected_rank, Some(1));

    let wrong_snippet = vec![evaluation_evidence(
        "src/evaluate.rs",
        "source",
        "pub fn aggregate()",
    )];
    assert!(!query_metrics(&query, &wrong_snippet, true).success);

    let snippet_in_another_result = vec![
        evaluation_evidence("src/evaluate.rs", "source", "pub fn aggregate()"),
        ResultEvidence {
            rank: 2,
            relative_file_path: "src/evaluate.rs".to_owned(),
            language: "rust".to_owned(),
            source_role: "source".to_owned(),
            start_line: 11,
            end_line: 20,
            snippet: "pub fn query_metrics()".to_owned(),
            reranker_score: Some(0.8),
        },
    ];
    assert!(query_metrics(&query, &snippet_in_another_result, true).success);

    let wrong_role = vec![evaluation_evidence(
        "src/evaluate.rs",
        "test",
        "pub fn query_metrics()",
    )];
    assert!(!query_metrics(&query, &wrong_role, true).success);
}

#[test]
fn evaluation_reports_required_failures_optional_skips_and_json_metrics() {
    let query = evaluation_query();
    let required_failure = QueryReport::failed(
        query.clone(),
        "required workspace \"self\" not provided".to_owned(),
    );
    let mut optional_query = query;
    optional_query.workspace = "gust".to_owned();
    let optional_skip = QueryReport::skipped(
        optional_query,
        "optional workspace \"gust\" not provided".to_owned(),
    );
    let report = aggregate(&[required_failure, optional_skip]);

    assert_eq!(report.query_count, 2);
    assert_eq!(report.executed_count, 1);
    assert_eq!(report.skipped_count, 1);
    assert_eq!(report.failure_count, 1);
    assert!(has_failures(&report));

    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["queries"][0]["outcome"], "failure");
    assert_eq!(json["queries"][1]["outcome"], "skipped");
    assert!(json.get("hit_at_1_rate").is_some());
    assert!(json.get("mrr").is_some());
    assert!(json.get("role_distribution").is_some());

    let definition = EvaluationFile {
        workspaces: vec![EvalWorkspace {
            key: "self".to_owned(),
            required: true,
        }],
        queries: Vec::new(),
    };
    assert!(definition.workspaces[0].required);
}

#[test]
fn compare_subcommand_runs_offline_and_a_regression_is_not_an_error() {
    let temp = tempfile::tempdir().unwrap();
    let write_report = |name: &str, ranks: &[(&str, f64)]| {
        let reports: Vec<QueryReport> = ranks
            .iter()
            .map(|(text, reciprocal_rank)| {
                let mut query = evaluation_query();
                query.query = (*text).to_owned();
                let mut report = QueryReport::failed(query, String::new());
                report.metrics.reciprocal_rank = *reciprocal_rank;
                report.metrics.hit_at_1 = *reciprocal_rank == 1.0;
                report
            })
            .collect();
        std::fs::write(
            temp.path().join(name),
            serde_json::to_string(&aggregate(&reports)).unwrap(),
        )
        .unwrap();
    };
    write_report("base.json", &[("alpha", 1.0), ("beta", 0.5)]);
    write_report("regressed.json", &[("alpha", 0.5), ("beta", 0.25)]);
    let binary = env!("CARGO_BIN_EXE_local-code-intelligence");

    // The configuration named here does not exist: `compare` must not load
    // it, open an index, or contact a service.
    let output = std::process::Command::new(binary)
        .current_dir(temp.path())
        .args([
            "--config",
            "no-such-config.toml",
            "compare",
            "base.json",
            "regressed.json",
            "--output",
            "comparison.json",
            "--seed",
            "3",
            "--resamples",
            "500",
        ])
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "a regression is not an error: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("MRR"), "{stdout}");
    assert!(stdout.contains("W/L/T: 0/2/0"), "{stdout}");

    let json: Value = serde_json::from_str(
        &std::fs::read_to_string(temp.path().join("comparison.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(json["paired"], 2);
    assert_eq!(json["seed"], 3);
    assert_eq!(json["resamples"], 500);
    assert!(json["overall"]["mrr"]["mean_delta"].as_f64().unwrap() < 0.0);

    let unreadable = std::process::Command::new(binary)
        .current_dir(temp.path())
        .args(["compare", "base.json", "absent.json"])
        .output()
        .unwrap();
    assert!(!unreadable.status.success());
    assert!(
        String::from_utf8_lossy(&unreadable.stderr).contains("absent.json"),
        "{}",
        String::from_utf8_lossy(&unreadable.stderr)
    );
}

// ---------------------------------------------------------------------------
// `evaluate --dump-candidates`: the candidate dump observes a search, it never
// changes one.
// ---------------------------------------------------------------------------

use local_code_intelligence::{
    app::{CandidateTrace, RerankOutcome, SearchOptions, SearchReport},
    evaluate::{CandidateArm, RerankMode, RunOptions, load_report, run_evaluation_with_options},
    filter::FilterRequest,
};

/// `src/many.rs` holds five chunks that match "translator" better than the
/// only chunk of each of two other files, as in the per-file cap test.
/// `src/second.rs` is longer than the reranker's document budget.
fn write_dump_workspace(root: &Path) {
    write(
        root,
        "src/many.rs",
        "fn translator_one() {}\nfn translator_two() {}\nfn translator_three() {}\nfn translator_four() {}\nfn translator_five() {}\n",
    );
    write(
        root,
        "src/second.rs",
        &format!("pub static SECOND: &str = \"{}\";\n", "y".repeat(300)),
    );
    write(root, "src/third.rs", "fn third_file() {}\n");
}

/// Cap 2 and a shortlist of 4 over seven chunks: the third to fifth `many.rs`
/// chunks fall behind the other files and out of the shortlist. The reranker
/// document is cut to 256 bytes (the least allowed) of code.
fn dump_config(mut config: Config) -> Config {
    config.channels.lsp = false;
    config.fusion.max_chunks_per_file = 2;
    config.rerank_candidate_count = 4;
    config.rerank_document_max_bytes = 256;
    config
}

async fn search_traced(
    app: &App,
    workspace: &Path,
    filters: FilterRequest,
    trace_candidates: bool,
) -> SearchReport {
    app.search_with_options(
        workspace,
        "translator",
        Some(3),
        filters,
        SearchOptions { trace_candidates },
    )
    .await
    .unwrap()
}

/// The report as JSON without what legitimately differs between two searches
/// (timings, wait) and without the trace.
fn without_trace_and_timings(report: &SearchReport) -> Value {
    let mut value = serde_json::to_value(report).unwrap();
    let object = value.as_object_mut().unwrap();
    for key in ["timings", "candidates", "rerank_outcome"] {
        object.remove(key);
    }
    object["index"].as_object_mut().unwrap().remove("wait_ms");
    value
}

/// A candidate's fusion score from its ranks, as the dump documents it.
fn recomputed_fusion_score(row: &CandidateTrace, config: &Config) -> f32 {
    let (rrf_k, weights) = (config.rrf_k, &config.fusion);
    let mut score = 0.0_f32;
    if let Some(rank) = row.semantic_rank {
        score += weights.semantic_weight / (rrf_k + rank as f32);
    }
    if let Some(rank) = row.lexical_rank {
        score += weights.lexical_weight / (rrf_k + rank as f32);
    }
    if let Some(rank) = row.lsp_rank {
        score += weights.lsp_weight / (rrf_k + rank as f32);
    }
    score + row.prior
}

fn locate<'a>(rows: &'a [CandidateTrace], path: &str, start_line: u32) -> &'a CandidateTrace {
    rows.iter()
        .find(|row| row.relative_file_path == path && row.start_line == start_line)
        .unwrap_or_else(|| panic!("{path}:{start_line} is not in the dump"))
}

#[tokio::test]
async fn a_traced_search_records_the_pool_and_returns_exactly_what_an_untraced_one_does() {
    let (temp, config, fake, task) = fixture().await;
    let config = dump_config(config);
    let workspace = temp.path().join("dump");
    write_dump_workspace(&workspace);
    let app = App::open(config.clone()).await.unwrap();
    app.index(&workspace).await.unwrap();

    // Every untraced entry point (the CLI, MCP and REST all use these two)
    // leaves the trace out of the JSON altogether.
    let plain = search_traced(&app, &workspace, FilterRequest::default(), false).await;
    let plain_json = serde_json::to_value(&plain).unwrap();
    assert!(plain.candidates.is_none() && plain.rerank_outcome.is_none());
    let via_search =
        serde_json::to_value(app.search(&workspace, "translator", Some(3)).await.unwrap()).unwrap();
    for json in [&plain_json, &via_search] {
        for key in ["candidates", "rerank_outcome", "arm"] {
            assert!(
                json.get(key).is_none(),
                "an untraced search has a {key} key"
            );
        }
    }
    assert!(plain.reranked, "{:?}", plain.warning);

    // The traced search returns the same thing.
    let traced = search_traced(&app, &workspace, FilterRequest::default(), true).await;
    assert_eq!(
        without_trace_and_timings(&traced),
        without_trace_and_timings(&plain)
    );
    assert_eq!(traced.rerank_outcome, Some(RerankOutcome::Scored));
    let rows = traced.candidates.as_ref().expect("the pool was traced");
    assert_eq!(
        rows.len(),
        7,
        "five chunks of many.rs and one of each other file"
    );

    // Fused order, the exact fusion arithmetic, and the one-based ranks.
    for (index, row) in rows.iter().enumerate() {
        assert_eq!(row.fused_rank, index + 1);
        assert!(row.semantic_rank.is_some(), "{row:?}");
        assert_eq!(
            row.fusion_score.to_bits(),
            recomputed_fusion_score(row, &config).to_bits(),
            "{}:{}",
            row.relative_file_path,
            row.start_line
        );
    }
    assert!(rows.windows(2).all(|pair| {
        pair[0]
            .fusion_score
            .total_cmp(&pair[1].fusion_score)
            .is_ge()
    }));
    let mut semantic: Vec<usize> = rows.iter().filter_map(|row| row.semantic_rank).collect();
    semantic.sort_unstable();
    assert_eq!(semantic, (1..=7).collect::<Vec<_>>());

    // The cap keeps two chunks of many.rs in front and demotes the other
    // three behind both other files; the shortlist is the first four.
    let shortlisted: Vec<_> = rows.iter().filter(|row| row.shortlisted).collect();
    assert_eq!(shortlisted.len(), 4);
    assert!(shortlisted.iter().all(|row| row.capped_rank <= 4));
    let is_many = |row: &CandidateTrace| row.relative_file_path == "src/many.rs";
    assert_eq!(shortlisted.iter().filter(|row| is_many(row)).count(), 2);
    let cut: Vec<_> = rows.iter().filter(|row| !row.shortlisted).collect();
    assert_eq!(cut.len(), 3);
    assert!(cut.iter().all(|row| is_many(row)));
    assert!(cut.iter().all(|row| row.capped_rank > row.fused_rank));
    let mut capped: Vec<usize> = rows.iter().map(|row| row.capped_rank).collect();
    capped.sort_unstable();
    assert_eq!(capped, (1..=7).collect::<Vec<_>>());

    // Reranker scores for the shortlist only, ranked 1..=4 as the reranker
    // ordered them (score descending, request index on ties).
    assert!(shortlisted.iter().all(|row| row.reranker_score.is_some()));
    assert!(cut.iter().all(|row| {
        row.reranker_score.is_none()
            && row.rerank_rank.is_none()
            && row.document_bytes.is_none()
            && row.est_tokens.is_none()
    }));
    let mut reranked = shortlisted.clone();
    reranked.sort_by_key(|row| row.rerank_rank);
    assert_eq!(
        reranked
            .iter()
            .map(|row| row.rerank_rank)
            .collect::<Vec<_>>(),
        [Some(1), Some(2), Some(3), Some(4)]
    );
    assert!(reranked.windows(2).all(|pair| {
        let (a, b) = (
            pair[0].reranker_score.unwrap(),
            pair[1].reranker_score.unwrap(),
        );
        a > b || (a == b && pair[0].capped_rank < pair[1].capped_rank)
    }));
    // The results are the head of that order, with the same scores.
    for (position, hit) in plain.results.iter().enumerate() {
        let row = locate(rows, &hit.chunk.relative_file_path, hit.chunk.start_line);
        assert_eq!(row.rerank_rank, Some(position + 1));
        assert_eq!(row.reranker_score, hit.reranker_score);
        assert_eq!(
            row.snippet,
            local_code_intelligence::evaluate::bounded_snippet(&hit.chunk.code)
        );
        assert_eq!(row.fusion_score.to_bits(), hit.fusion_score.to_bits());
    }

    // The document sizes are what the reranker received: path, newline, and
    // at most 256 bytes of code (only `src/second.rs`, with a 300-byte literal, is cut: 14 + 256).
    let longest = shortlisted
        .iter()
        .map(|row| row.document_bytes.unwrap())
        .max()
        .unwrap();
    assert_eq!(longest, "src/second.rs\n".len() + 256);
    assert_eq!(
        fake.max_rerank_document_bytes.load(Ordering::SeqCst),
        longest
    );
    for row in &shortlisted {
        assert_eq!(row.est_tokens, Some(row.document_bytes.unwrap() / 4));
    }
    task.abort();
}

#[tokio::test]
async fn a_traced_search_without_reranker_scores_says_why_and_still_returns_the_same_results() {
    let (temp, config, fake, task) = fixture().await;
    let config = dump_config(config);
    let workspace = temp.path().join("dump-unscored");
    write_dump_workspace(&workspace);
    let indexer = App::open(config.clone()).await.unwrap();
    indexer.index(&workspace).await.unwrap();
    drop(indexer);

    let assert_unscored = |report: &SearchReport| {
        let rows = report.candidates.as_ref().unwrap();
        assert_eq!(rows.len(), 7);
        assert!(
            rows.iter()
                .all(|row| row.reranker_score.is_none() && row.rerank_rank.is_none())
        );
        // The shortlist still names the four documents the reranker would be
        // given, in fusion order.
        assert_eq!(rows.iter().filter(|row| row.shortlisted).count(), 4);
        assert!(
            rows.iter()
                .filter(|row| row.shortlisted)
                .all(|row| row.document_bytes.is_some())
        );
        assert!(
            report
                .results
                .iter()
                .all(|hit| hit.reranker_score.is_none())
        );
    };

    // Reranker switched off.
    let mut off_config = config.clone();
    off_config.channels.reranker = false;
    let off_app = App::open(off_config).await.unwrap();
    let off_plain = search_traced(&off_app, &workspace, FilterRequest::default(), false).await;
    let off = search_traced(&off_app, &workspace, FilterRequest::default(), true).await;
    assert_eq!(off.rerank_outcome, Some(RerankOutcome::Off));
    assert!(!off.reranked && off.warning.is_none(), "{:?}", off.warning);
    assert_unscored(&off);
    assert_eq!(
        without_trace_and_timings(&off),
        without_trace_and_timings(&off_plain)
    );
    // Off is fusion order: the results are the head of the capped order.
    let rows = off.candidates.as_ref().unwrap();
    for (position, hit) in off.results.iter().enumerate() {
        let row = locate(rows, &hit.chunk.relative_file_path, hit.chunk.start_line);
        assert_eq!(row.capped_rank, position + 1);
    }
    drop(off_app);

    // Reranker failing: fusion order, with a warning.
    let app = App::open(config.clone()).await.unwrap();
    fake.rerank_fails.store(true, Ordering::SeqCst);
    let failed_plain = search_traced(&app, &workspace, FilterRequest::default(), false).await;
    let failed = search_traced(&app, &workspace, FilterRequest::default(), true).await;
    fake.rerank_fails.store(false, Ordering::SeqCst);
    assert_eq!(failed.rerank_outcome, Some(RerankOutcome::Failed));
    assert!(!failed.reranked);
    assert!(
        failed
            .warning
            .as_deref()
            .unwrap()
            .contains("fusion-ranked results returned"),
        "{:?}",
        failed.warning
    );
    assert_unscored(&failed);
    assert_eq!(
        without_trace_and_timings(&failed),
        without_trace_and_timings(&failed_plain)
    );

    // A filter that matches nothing is an empty report with an empty dump.
    let nothing = search_traced(
        &app,
        &workspace,
        FilterRequest {
            languages: Some(vec!["python".to_owned()]),
            ..FilterRequest::default()
        },
        true,
    )
    .await;
    assert!(nothing.results.is_empty());
    assert_eq!(nothing.candidates, Some(Vec::new()));
    assert_eq!(nothing.rerank_outcome, Some(RerankOutcome::NotRun));
    task.abort();
}

fn dump_definition() -> EvaluationFile {
    let query = |text: &str, workspace: &str, top_k: Option<usize>| EvalQuery {
        query: text.to_owned(),
        workspace: workspace.to_owned(),
        top_k,
        filters: None,
        expected_paths: vec!["src/many.rs".to_owned()],
        preferred_paths: Vec::new(),
        disfavored_paths: Vec::new(),
        expected_roles: Vec::new(),
        required_snippet: Some("translator".to_owned()),
        tags: Vec::new(),
    };
    EvaluationFile {
        workspaces: vec![
            EvalWorkspace {
                key: "dump".to_owned(),
                required: true,
            },
            EvalWorkspace {
                key: "absent".to_owned(),
                required: false,
            },
        ],
        queries: vec![
            query("translator", "dump", Some(3)),
            query("translator_two", "dump", None),
            query("translator", "absent", None),
        ],
    }
}

#[tokio::test]
async fn the_evaluation_dump_adds_candidates_and_an_arm_and_changes_nothing_else() {
    let (temp, config, _fake, task) = fixture().await;
    let config = dump_config(config);
    let workspace = temp.path().join("dump-evaluation");
    write_dump_workspace(&workspace);
    let app = App::open(config.clone()).await.unwrap();
    app.index(&workspace).await.unwrap();
    let definition = dump_definition();
    let mappings = parse_workspace_mappings(&[format!("dump={}", workspace.display())]).unwrap();

    let plain = run_evaluation_with_options(&app, &definition, &mappings, RunOptions::default())
        .await
        .unwrap();
    let dumped = run_evaluation_with_options(
        &app,
        &definition,
        &mappings,
        RunOptions {
            dump_candidates: true,
        },
    )
    .await
    .unwrap();

    // Without the flag no query carries a dump key.
    let plain_json = serde_json::to_value(&plain).unwrap();
    for query in plain_json["queries"].as_array().unwrap() {
        for key in ["candidates", "arm", "rerank_outcome", "notes"] {
            assert!(query.get(key).is_none(), "a plain report has a {key} key");
        }
    }

    // With it, everything an evaluation reports is the same.
    assert_eq!(plain.queries.len(), 3);
    assert_eq!((plain.executed_count, plain.skipped_count), (2, 1));
    assert_eq!(plain.success_count, dumped.success_count);
    assert_eq!(plain.mrr, dumped.mrr);
    assert_eq!(plain.mean_ndcg_at_10, dumped.mean_ndcg_at_10);
    assert_eq!(plain.hit_at_1_rate, dumped.hit_at_1_rate);
    for (before, after) in plain.queries.iter().zip(&dumped.queries) {
        assert_eq!(
            serde_json::to_value(&before.evidence).unwrap(),
            serde_json::to_value(&after.evidence).unwrap(),
            "{}",
            before.query.query
        );
        assert_eq!(before.metrics, after.metrics);
        assert_eq!(before.reranked, after.reranked);
        assert_eq!(before.warning, after.warning);
        assert_eq!(before.outcome, after.outcome);
        assert_eq!(before.index_action, after.index_action);
    }

    // The dumped queries carry the pool, the arm, and the reranker outcome;
    // a skipped query carries none.
    let default_top_k = config.default_top_k;
    for (query, expected_k) in dumped.queries[..2].iter().zip([3, default_top_k]) {
        assert_eq!(query.candidates.len(), 7, "{}", query.query.query);
        assert_eq!(query.rerank_outcome, Some(RerankOutcome::Scored));
        let arm = query.arm.as_ref().expect("a dumped query has an arm");
        assert_eq!(arm, &CandidateArm::new(&config, expected_k));
        assert_eq!(arm.k, expected_k);
        assert_eq!(arm.rrf_k, config.rrf_k);
        assert_eq!(arm.fusion, config.fusion);
        assert_eq!(arm.rerank_candidate_count, 4);
        assert_eq!(arm.rerank_document_max_bytes, 256);
        assert_eq!(arm.channels, config.channels);
        assert_eq!(arm.rerank_mode, RerankMode::Replace);
        assert_eq!(arm.embedding_identity, config.embedding_identity());
        assert_eq!(arm.reranker_model, config.reranker_model);
        // The evidence rows are the head of the dumped rerank order, with the
        // same bounded snippets, so `required_snippet` replays exactly.
        for row in &query.evidence {
            let traced = locate(&query.candidates, &row.relative_file_path, row.start_line);
            assert_eq!(traced.rerank_rank, Some(row.rank));
            assert_eq!(traced.reranker_score, row.reranker_score);
            assert_eq!(traced.snippet, row.snippet);
        }
    }
    let skipped = &dumped.queries[2];
    assert!(skipped.skipped);
    assert!(
        skipped.candidates.is_empty()
            && skipped.arm.is_none()
            && skipped.rerank_outcome.is_none()
            && skipped.notes.is_empty()
    );
    // These searches had no notes, so a dumped query has no `notes` key
    // either (the cold language server case is in the indexing tests).
    let dumped_json = serde_json::to_value(&dumped).unwrap();
    for query in dumped_json["queries"].as_array().unwrap() {
        assert!(query.get("notes").is_none(), "{query}");
    }

    // A saved dump loads back unchanged through the report loader that
    // `compare` uses, and `compare` accepts it against the plain report.
    let saved = temp.path().join("dump.json");
    std::fs::write(&saved, serde_json::to_string_pretty(&dumped).unwrap()).unwrap();
    let loaded = load_report(&saved).unwrap();
    for (before, after) in dumped.queries.iter().zip(&loaded.queries) {
        assert_eq!(before.candidates, after.candidates);
        assert_eq!(before.arm, after.arm);
        assert_eq!(before.rerank_outcome, after.rerank_outcome);
    }
    let plain_saved = temp.path().join("plain.json");
    std::fs::write(&plain_saved, serde_json::to_string_pretty(&plain).unwrap()).unwrap();
    let comparison = local_code_intelligence::evaluate::compare_files(
        &plain_saved,
        &saved,
        &local_code_intelligence::evaluate::CompareOptions {
            seed: 1,
            resamples: 100,
        },
    )
    .unwrap();
    assert_eq!(comparison.paired, 2);
    task.abort();
}

#[tokio::test]
async fn evaluate_dump_candidates_writes_the_dump_through_the_command_line_only_when_asked() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("dump-cli");
    write_dump_workspace(&workspace);
    let config_path = temp.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "data_dir = '{}'\nembedding_url = '{}'\nreranker_url = '{}'\nrerank_candidate_count = 4\nrerank_document_max_bytes = 256\n\n[channels]\nlsp = false\n\n[fusion]\nmax_chunks_per_file = 2\n",
            temp.path().join("cli-data").display(),
            config.embedding_url,
            config.reranker_url,
        ),
    )
    .unwrap();
    let definition_path = temp.path().join("definition.toml");
    std::fs::write(
        &definition_path,
        "[[workspaces]]\nkey = \"dump\"\nrequired = true\n\n[[queries]]\nworkspace = \"dump\"\nquery = \"translator\"\ntop_k = 3\nexpected_paths = [\"src/many.rs\"]\n",
    )
    .unwrap();

    let run = |output: &'static str, dump: bool| {
        let binary = env!("CARGO_BIN_EXE_local-code-intelligence");
        let (config_path, definition_path) = (config_path.clone(), definition_path.clone());
        let output_path = temp.path().join(output);
        let workspace = format!("dump={}", workspace.display());
        async move {
            // The fake model service runs on this runtime, so the command
            // must not block it.
            let (result, output_path) = tokio::task::spawn_blocking(move || {
                let mut command = std::process::Command::new(binary);
                command
                    .arg("--config")
                    .arg(&config_path)
                    .arg("evaluate")
                    .arg(&definition_path)
                    .args(["--workspace", &workspace, "--output"])
                    .arg(&output_path);
                if dump {
                    command.arg("--dump-candidates");
                }
                (command.output().unwrap(), output_path)
            })
            .await
            .unwrap();
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
            serde_json::from_str::<Value>(&std::fs::read_to_string(output_path).unwrap()).unwrap()
        }
    };
    let plain = run("plain.json", false).await;
    let dumped = run("dumped.json", true).await;

    let plain_query = &plain["queries"][0];
    for key in ["candidates", "arm", "rerank_outcome", "notes"] {
        assert!(plain_query.get(key).is_none(), "{key}");
    }
    let dumped_query = &dumped["queries"][0];
    assert_eq!(dumped_query["candidates"].as_array().unwrap().len(), 7);
    assert_eq!(dumped_query["rerank_outcome"], "scored");
    assert_eq!(dumped_query["arm"]["k"], 3);
    assert_eq!(dumped_query["arm"]["rerank_candidate_count"], 4);
    assert_eq!(dumped_query["arm"]["fusion"]["max_chunks_per_file"], 2);
    assert_eq!(dumped_query["arm"]["rerank_mode"], "replace");
    let first = &dumped_query["candidates"][0];
    for key in [
        "relative_file_path",
        "language",
        "start_line",
        "end_line",
        "source_role",
        "semantic_rank",
        "prior",
        "fusion_score",
        "fused_rank",
        "capped_rank",
        "shortlisted",
        "document_bytes",
        "est_tokens",
        "snippet",
    ] {
        assert!(first.get(key).is_some(), "missing {key}: {first}");
    }
    assert_eq!(plain_query["evidence"], dumped_query["evidence"]);
    assert_eq!(plain_query["metrics"], dumped_query["metrics"]);
    task.abort();
}

// ---------------------------------------------------------------------------
// `replay`: offline scoring of other arms from a candidate dump, checked
// against live searches of the same pool.
// ---------------------------------------------------------------------------

use local_code_intelligence::evaluate::{
    AggregateReport, CapStage, CompareOptions, ReplayOptions, ReplayRefusal, compare_files, replay,
    replay_file, report_with_comparison,
};

/// A definition over the dump workspace with three queries, one per entry of
/// `top_ks`. A `top_k` within the shortlist of four makes replay at the dumped
/// arm exact; `None` takes the default of eight, more than production returns
/// from a shortlist of four.
fn replay_definition(top_ks: [Option<usize>; 3]) -> EvaluationFile {
    let mut definition = dump_definition();
    definition.workspaces.truncate(1);
    definition.queries.truncate(1);
    for (index, text) in ["translator_two", "translator"].into_iter().enumerate() {
        let mut query = definition.queries[0].clone();
        query.query = text.to_owned();
        query.tags = vec![format!("tag{index}")];
        definition.queries.push(query);
    }
    for (query, top_k) in definition.queries.iter_mut().zip(top_ks) {
        query.top_k = top_k;
    }
    definition
}

/// The report as `evaluate --output` writes it, saved to `path`.
fn save_report(report: &AggregateReport, path: &Path) {
    let json = serde_json::to_string_pretty(&report_with_comparison(report, None).unwrap());
    std::fs::write(path, json.unwrap()).unwrap();
}

fn dump_mappings(workspace: &Path) -> Vec<local_code_intelligence::evaluate::WorkspaceMapping> {
    parse_workspace_mappings(&[format!("dump={}", workspace.display())]).unwrap()
}

/// Index a fresh dump workspace under `config` and run the definition once,
/// with or without the candidate dump. The application is dropped on return
/// so that another one can open the same data directory.
async fn live_run(
    temp: &Path,
    config: &Config,
    definition: &EvaluationFile,
    dump_candidates: bool,
) -> AggregateReport {
    let workspace = temp.join("dump-replay");
    if !workspace.exists() {
        write_dump_workspace(&workspace);
    }
    let app = App::open(config.clone()).await.unwrap();
    app.index(&workspace).await.unwrap();
    run_evaluation_with_options(
        &app,
        definition,
        &dump_mappings(&workspace),
        RunOptions { dump_candidates },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn replay_at_the_dumped_arm_reproduces_a_live_search_exactly() {
    let (temp, config, _fake, task) = fixture().await;
    let config = dump_config(config);
    let definition = replay_definition([Some(3), Some(4), Some(2)]);
    let dumped = live_run(temp.path(), &config, &definition, true).await;
    assert!(
        dumped
            .queries
            .iter()
            .all(|query| query.candidates.len() == 7)
    );

    // Through the file the command line reads (f32 scores travel as f64).
    let saved = temp.path().join("dump.json");
    save_report(&dumped, &saved);
    let replayed = replay_file(&saved, &ReplayOptions::default()).unwrap();

    // Fusion, the per-file cap, and the shortlist cut of the live search
    // reproduce bit for bit, so nothing is refused...
    assert_eq!(replayed.replay.queries.replayed, 3);
    assert_eq!(replayed.replay.queries.refused, 0, "{:?}", replayed.replay);
    assert_eq!(replayed.replay.past_window_queries, 0);
    // ...and the replayed results and metrics are the live ones.
    for (live, replay) in dumped.queries.iter().zip(&replayed.report.queries) {
        assert_eq!(replay.evidence, live.evidence, "{}", live.query.query);
        assert_eq!(replay.metrics, live.metrics, "{}", live.query.query);
        assert_eq!(replay.reranked, live.reranked);
        assert_eq!(replay.outcome, live.outcome);
        assert!(replay.reranked && replay.metrics.success);
    }
    assert_eq!(replayed.report.mrr, dumped.mrr);
    assert_eq!(replayed.report.mean_ndcg_at_10, dumped.mean_ndcg_at_10);
    assert_eq!(replayed.report.by_tag, dumped.by_tag);
    assert_eq!(replayed.report.success_count, dumped.success_count);
    assert_eq!(replayed.replay.dumped_arm.rerank_candidate_count, 4);
    assert_eq!(replayed.replay.arm.window, 4);

    // A window of two keeps the leading two reranked and appends the rest of
    // the fused order after them.
    let narrow = replay_file(
        &saved,
        &ReplayOptions {
            window: Some(2),
            ..ReplayOptions::default()
        },
    )
    .unwrap();
    assert_eq!(narrow.replay.queries.refused, 0);
    assert_eq!(narrow.report.queries[0].evidence.len(), 3);

    // Production returns only the reranked shortlist, so a result window
    // larger than it (the default of eight against a shortlist of four) is
    // where replay differs: it appends the fused tail, and says so.
    let wide = live_run(temp.path(), &config, &replay_definition([None; 3]), true).await;
    let wide_replay = replay(&wide, &ReplayOptions::default()).unwrap();
    assert_eq!(wide_replay.replay.queries.refused, 0);
    assert_eq!(wide_replay.replay.past_window_queries, 3);
    for (live, replay) in wide.queries.iter().zip(&wide_replay.report.queries) {
        assert_eq!(live.evidence.len(), 4);
        assert_eq!(replay.evidence.len(), 7);
        assert_eq!(replay.evidence[..4], live.evidence[..]);
        assert!(
            replay.evidence[4..]
                .iter()
                .all(|row| row.reranker_score.is_none())
        );
    }
    task.abort();
}

#[tokio::test]
async fn replay_without_the_reranker_ranks_as_a_live_search_under_other_fusion_settings() {
    let (temp, config, _fake, task) = fixture().await;
    let config = dump_config(config);
    // Production returns at most the shortlist of four even without the
    // reranker, so the result windows stay within it.
    let definition = replay_definition([Some(3), Some(4), Some(4)]);
    let dumped = live_run(temp.path(), &config, &definition, true).await;

    // A live run of other settings: no reranker, no per-file cap, other
    // weights and rrf_k, on the same index.
    let mut other = config.clone();
    other.channels.reranker = false;
    other.fusion.max_chunks_per_file = 0;
    other.fusion.semantic_weight = 1.5;
    other.fusion.lexical_weight = 3.0;
    other.rrf_k = 20.0;
    let live = live_run(temp.path(), &other, &definition, false).await;
    let replayed = replay(
        &dumped,
        &ReplayOptions {
            cap: Some(0),
            semantic_weight: Some(1.5),
            lexical_weight: Some(3.0),
            rrf_k: Some(20.0),
            rerank_mode: Some(RerankMode::Off),
            ..ReplayOptions::default()
        },
    )
    .unwrap();
    assert_eq!(replayed.replay.queries.refused, 0);
    assert_eq!(replayed.replay.past_window_queries, 0);
    for (live, replay) in live.queries.iter().zip(&replayed.report.queries) {
        assert!(!live.evidence.is_empty());
        assert_eq!(replay.evidence, live.evidence, "{}", live.query.query);
        assert_eq!(replay.metrics, live.metrics, "{}", live.query.query);
        assert!(!replay.reranked && !live.reranked);
    }
    assert_eq!(replayed.report.mrr, live.mrr);
    // The comparison would pass vacuously if the settings changed nothing.
    let places = |report: &AggregateReport| -> Vec<Vec<(String, u32)>> {
        report
            .queries
            .iter()
            .map(|query| {
                query
                    .evidence
                    .iter()
                    .map(|row| (row.relative_file_path.clone(), row.start_line))
                    .collect()
            })
            .collect()
    };
    assert_ne!(places(&live), places(&dumped));

    // Dropping the cap with the reranker kept would put chunks into the
    // window that the dump (cap 2, shortlist 4) never sent to the reranker,
    // so replay refuses rather than guessing their scores.
    let uncapped = replay(
        &dumped,
        &ReplayOptions {
            cap: Some(0),
            cap_stage: Some(CapStage::Results),
            ..ReplayOptions::default()
        },
    )
    .unwrap();
    assert_eq!(uncapped.replay.queries.refused, 3);
    assert_eq!(
        uncapped.replay.refused.get(&ReplayRefusal::UnscoredWindow),
        Some(&3)
    );
    assert_eq!(uncapped.report.executed_count, 0);
    task.abort();
}

#[tokio::test]
async fn the_replay_subcommand_runs_offline_and_its_report_pairs_with_compare() {
    let (temp, config, _fake, task) = fixture().await;
    let config = dump_config(config);
    let definition = replay_definition([Some(3), Some(4), Some(2)]);
    let dumped = live_run(temp.path(), &config, &definition, true).await;
    save_report(&dumped, &temp.path().join("dump.json"));
    let plain = live_run(temp.path(), &config, &definition, false).await;
    save_report(&plain, &temp.path().join("plain.json"));
    let binary = env!("CARGO_BIN_EXE_local-code-intelligence");
    let run = |args: &[&str]| {
        std::process::Command::new(binary)
            .current_dir(temp.path())
            // The configuration named here does not exist: `replay` must not
            // load it, open an index, or contact a service.
            .args(["--config", "no-such-config.toml"])
            .args(args)
            .output()
            .unwrap()
    };

    let output = run(&[
        "replay",
        "dump.json",
        "--output",
        "replayed.json",
        "--window",
        "2",
        "--cap",
        "0",
        "--cap-stage",
        "results",
        "--rerank-mode",
        "vote",
        "--vote-weight",
        "2",
        // The dumped values, spelled out: the options parse and change nothing.
        "--prior",
        "0.004",
        "--semantic-weight",
        "2",
    ]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("Replay of dump.json"), "{stdout}");
    assert!(stdout.contains("window 2, cap 0 (results)"), "{stdout}");
    assert!(stdout.contains("vote weight 2"), "{stdout}");
    assert!(stdout.contains("ESTIMATED"), "{stdout}");
    let replayed: Value =
        serde_json::from_str(&std::fs::read_to_string(temp.path().join("replayed.json")).unwrap())
            .unwrap();
    assert_eq!(replayed["replay"]["source"], "dump.json");
    assert_eq!(replayed["replay"]["arm"]["cap_stage"], "results");
    assert_eq!(replayed["replay"]["arm"]["rerank_mode"], "vote");
    assert_eq!(replayed["replay"]["arm"]["vote_weight"], 2.0);
    assert_eq!(replayed["replay"]["queries"]["replayed"], 3);
    assert_eq!(replayed["replay"]["latency"]["estimated"], true);
    assert_eq!(replayed["query_count"], 3);

    // `compare` pairs the replayed report with the real one.
    let comparison = compare_files(
        &temp.path().join("dump.json"),
        &temp.path().join("replayed.json"),
        &CompareOptions::default(),
    )
    .unwrap();
    assert_eq!((comparison.paired, comparison.unpaired_baseline), (3, 0));
    let compared = run(&["compare", "dump.json", "replayed.json"]);
    assert!(compared.status.success());
    assert!(String::from_utf8_lossy(&compared.stdout).contains("pairs: 3"));

    // Without --output only the summary is printed, and the defaults are the
    // dumped arm.
    let summary = run(&["replay", "dump.json"]);
    assert!(summary.status.success());
    let summary = String::from_utf8_lossy(&summary.stdout);
    assert!(summary.contains("replayed 3 | refused 0"), "{summary}");

    // Bad usage and unreadable input fail with a message.
    for (args, message) in [
        (vec!["replay", "dump.json", "--vote-weight", "1"], "vote"),
        (vec!["replay", "dump.json", "--rerank-mode", "vote"], "vote"),
        (vec!["replay", "dump.json", "--window", "0"], "at least 1"),
        (vec!["replay", "dump.json", "--cap", "101"], "0..=100"),
        (
            vec!["replay", "dump.json", "--semantic-weight", "0"],
            "greater than zero",
        ),
        (
            vec!["replay", "dump.json", "--cap-stage", "never"],
            "cap stage",
        ),
        (vec!["replay", "absent.json"], "absent.json"),
        // A report made without --dump-candidates has nothing to replay.
        (vec!["replay", "plain.json"], "--dump-candidates"),
    ] {
        let failed = run(&args);
        assert!(!failed.status.success(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&failed.stderr).contains(message),
            "{args:?}: {}",
            String::from_utf8_lossy(&failed.stderr)
        );
    }
    task.abort();
}
