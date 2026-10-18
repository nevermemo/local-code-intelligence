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

/// A configuration file for the command-line binary that matches
/// [`dump_config`] and points at the fake model service of `config`; its data
/// directory is `cli-data` under `dir`.
fn write_dump_cli_config(dir: &Path, config: &Config) -> std::path::PathBuf {
    let config_path = dir.join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "data_dir = '{}'\nembedding_url = '{}'\nreranker_url = '{}'\nrerank_candidate_count = 4\nrerank_document_max_bytes = 256\n\n[channels]\nlsp = false\n\n[fusion]\nmax_chunks_per_file = 2\n",
            dir.join("cli-data").display(),
            config.embedding_url,
            config.reranker_url,
        ),
    )
    .unwrap();
    config_path
}

#[tokio::test]
async fn evaluate_dump_candidates_writes_the_dump_through_the_command_line_only_when_asked() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("dump-cli");
    write_dump_workspace(&workspace);
    let config_path = write_dump_cli_config(temp.path(), &config);
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
// `LCI_CANDIDATE_DUMP`: every search of a process appends its fused pool to a
// file and returns what it would have returned without. The in-process tests
// give the `App` the destination through `with_candidate_dump` rather than the
// environment, which a multi-threaded test binary cannot set without
// affecting every other test; the command-line test sets the real variable on
// a child process only.
// ---------------------------------------------------------------------------

/// The report as JSON without what legitimately differs between two
/// otherwise identical searches: the stage timings and the index wait. Unlike
/// [`without_trace_and_timings`] it keeps any trace key.
fn strip_timings(mut value: Value) -> Value {
    let object = value.as_object_mut().unwrap();
    object.remove("timings");
    object["index"].as_object_mut().unwrap().remove("wait_ms");
    value
}

fn without_timings(report: &SearchReport) -> Value {
    strip_timings(serde_json::to_value(report).unwrap())
}

/// The keys of one dump line, as the preregistration names them.
const DUMP_LINE_KEYS: [&str; 9] = [
    "candidates",
    "channels",
    "filters",
    "query",
    "rerank_candidate_count",
    "rerank_outcome",
    "timings",
    "ts",
    "workspace_path",
];

fn dump_lines(path: &Path) -> Vec<Value> {
    let text = std::fs::read_to_string(path).unwrap();
    assert!(
        text.is_empty() || text.ends_with('\n'),
        "the dump ends in a partial line"
    );
    text.lines()
        .map(|line| serde_json::from_str(line).expect("every dump line is one JSON document"))
        .collect()
}

fn assert_dump_line_shape(line: &Value) {
    let mut keys: Vec<&str> = line
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, DUMP_LINE_KEYS);
    assert!(line["ts"].as_f64().unwrap() > 1.5e9, "{}", line["ts"]);
    for row in line["candidates"].as_array().unwrap() {
        assert!(row["shortlisted"].is_boolean(), "{row}");
    }
}

#[tokio::test]
async fn a_candidate_dump_appends_the_whole_pool_of_one_search_and_changes_nothing_it_returns() {
    let (temp, config, _fake, task) = fixture().await;
    let config = dump_config(config);
    let workspace = temp.path().join("dump-env");
    write_dump_workspace(&workspace);
    let logs = temp.path().join("logs");
    std::fs::create_dir_all(&logs).unwrap();
    let dump_path = logs.join("candidates.jsonl");

    // The reference: the same two searches without a dump.
    let plain_app = App::open(config.clone())
        .await
        .unwrap()
        .with_candidate_dump(None);
    plain_app.index(&workspace).await.unwrap();
    let plain = plain_app
        .search(&workspace, "translator", Some(3))
        .await
        .unwrap();
    let traced = search_traced(&plain_app, &workspace, FilterRequest::default(), true).await;
    let pool = traced.candidates.clone().expect("the pool was traced");
    drop(plain_app);
    assert!(plain.reranked, "{:?}", plain.warning);
    assert!(!dump_path.exists(), "no dump was asked for");

    let app = App::open(config.clone())
        .await
        .unwrap()
        .with_candidate_dump(Some(dump_path.clone()));
    let report = app.search(&workspace, "translator", Some(3)).await.unwrap();

    // The response is the undumped one: no trace in it, and nothing else
    // different but the clocks.
    let json = serde_json::to_value(&report).unwrap();
    for key in ["candidates", "rerank_outcome"] {
        assert!(json.get(key).is_none(), "the response has a {key} key");
    }
    assert!(report.candidates.is_none() && report.rerank_outcome.is_none());
    assert_eq!(without_timings(&report), without_timings(&plain));

    // Exactly one line, holding the pool the traced search saw.
    let lines = dump_lines(&dump_path);
    assert_eq!(lines.len(), 1, "one line per search");
    let line = &lines[0];
    assert_dump_line_shape(line);
    assert_eq!(line["query"], "translator");
    assert_eq!(
        line["workspace_path"],
        serde_json::to_value(&report.workspace.path).unwrap()
    );
    assert_eq!(
        line["filters"],
        serde_json::to_value(&report.filters).unwrap()
    );
    assert_eq!(
        line["channels"],
        serde_json::to_value(config.channels).unwrap()
    );
    assert_eq!(line["rerank_candidate_count"], 4);
    assert_eq!(line["rerank_outcome"], "scored");
    // The response's timings, to the last digit serde_json's number parser
    // keeps (it may differ from the written f64 by one unit in the last place).
    let response_timings = serde_json::to_value(&report.timings).unwrap();
    let response_timings = response_timings.as_object().unwrap();
    assert_eq!(
        line["timings"].as_object().unwrap().len(),
        response_timings.len()
    );
    for (stage, expected) in response_timings {
        let dumped = line["timings"][stage].as_f64().unwrap();
        assert!(
            (dumped - expected.as_f64().unwrap()).abs() < 1e-9,
            "the dumped {stage} is {dumped}, the response's is {expected}"
        );
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    assert!((now - line["ts"].as_f64().unwrap()).abs() < 120.0);

    let dumped: Vec<CandidateTrace> = serde_json::from_value(line["candidates"].clone()).unwrap();
    assert_eq!(dumped, pool);
    assert_eq!(dumped.len(), 7, "every candidate of every channel");
    assert_eq!(dumped.iter().filter(|row| row.shortlisted).count(), 4);
    assert!(dumped.iter().all(|row| row.semantic_rank.is_some()));
    assert!(dumped.iter().any(|row| row.lexical_rank.is_some()));
    assert!(dumped.iter().any(|row| row.lexical_rank.is_none()));

    // A second search appends; it never rewrites the first line.
    let first_text = std::fs::read_to_string(&dump_path).unwrap();
    app.search(&workspace, "translator_two", None)
        .await
        .unwrap();
    let text = std::fs::read_to_string(&dump_path).unwrap();
    assert!(text.starts_with(&first_text));
    let lines = dump_lines(&dump_path);
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[1]["query"], "translator_two");
    assert_dump_line_shape(&lines[1]);

    // A caller that asks for the trace still gets it, and the dump is the same.
    let both = search_traced(&app, &workspace, FilterRequest::default(), true).await;
    assert_eq!(both.candidates.as_ref(), Some(&pool));
    assert_eq!(both.rerank_outcome, Some(RerankOutcome::Scored));
    let lines = dump_lines(&dump_path);
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[2]["candidates"], line["candidates"]);

    // A search with nothing to rank still counts as a search.
    let nothing = app
        .search_with_filters(
            &workspace,
            "translator",
            Some(3),
            FilterRequest {
                languages: Some(vec!["python".to_owned()]),
                ..FilterRequest::default()
            },
        )
        .await
        .unwrap();
    assert!(nothing.results.is_empty() && nothing.candidates.is_none());
    let lines = dump_lines(&dump_path);
    assert_eq!(lines.len(), 4);
    assert_dump_line_shape(&lines[3]);
    assert_eq!(lines[3]["candidates"], json!([]));
    assert_eq!(lines[3]["rerank_outcome"], "not_run");

    // A search that fails appends nothing.
    assert!(app.search(&workspace, "   ", Some(3)).await.is_err());
    assert_eq!(dump_lines(&dump_path).len(), 4);
    task.abort();
}

#[tokio::test]
async fn a_candidate_dump_that_cannot_be_written_never_fails_the_search() {
    let (temp, config, _fake, task) = fixture().await;
    let config = dump_config(config);
    let workspace = temp.path().join("dump-unwritable");
    write_dump_workspace(&workspace);
    let plain_app = App::open(config.clone())
        .await
        .unwrap()
        .with_candidate_dump(None);
    plain_app.index(&workspace).await.unwrap();
    let plain = plain_app
        .search(&workspace, "translator", Some(3))
        .await
        .unwrap();
    drop(plain_app);

    // A directory that does not exist (the dump never creates one), and a
    // path that is a directory.
    let missing_parent = temp.path().join("no-such-dir");
    let unwritable = [
        missing_parent.join("candidates.jsonl"),
        temp.path().to_path_buf(),
    ];
    for path in unwritable {
        let app = App::open(config.clone())
            .await
            .unwrap()
            .with_candidate_dump(Some(path.clone()));
        let report = app
            .search(&workspace, "translator", Some(3))
            .await
            .unwrap_or_else(|error| panic!("the search failed over {}: {error:#}", path.display()));
        assert!(report.reranked, "{:?}", report.warning);
        assert!(report.candidates.is_none() && report.rerank_outcome.is_none());
        assert_eq!(
            without_timings(&report),
            without_timings(&plain),
            "{}",
            path.display()
        );
    }
    assert!(!missing_parent.exists(), "the dump created a directory");
    task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_searches_append_one_whole_line_each() {
    let (temp, config, _fake, task) = fixture().await;
    let config = dump_config(config);
    let workspace = temp.path().join("dump-concurrent");
    write_dump_workspace(&workspace);
    let dump_path = temp.path().join("concurrent.jsonl");
    let app = App::open(config)
        .await
        .unwrap()
        .with_candidate_dump(Some(dump_path.clone()));
    app.index(&workspace).await.unwrap();
    assert!(!dump_path.exists(), "indexing is not a search");

    let queries: Vec<String> = (0..16).map(|n| format!("translator_{n}")).collect();
    let reports = futures::future::join_all(
        queries
            .iter()
            .map(|query| app.search(&workspace, query, Some(3))),
    )
    .await;
    for report in reports {
        report.unwrap();
    }

    let lines = dump_lines(&dump_path);
    let mut dumped: Vec<&str> = lines
        .iter()
        .map(|line| line["query"].as_str().unwrap())
        .collect();
    dumped.sort_unstable();
    let mut expected: Vec<&str> = queries.iter().map(String::as_str).collect();
    expected.sort_unstable();
    assert_eq!(dumped, expected, "one line per search");
    for line in &lines {
        assert_dump_line_shape(line);
        assert_eq!(line["candidates"].as_array().unwrap().len(), 7);
    }
    task.abort();
}

/// Runs the command-line binary in `cwd`; every `LCI_*` variable is removed
/// from the child and `LCI_CANDIDATE_DUMP` is then set to `dump` when it is
/// `Some`, so what the developer's own shell exports cannot change the
/// result.
async fn run_cli(
    config_path: &Path,
    cwd: &Path,
    args: Vec<String>,
    dump: Option<std::path::PathBuf>,
) -> std::process::Output {
    let (config_path, cwd) = (config_path.to_owned(), cwd.to_owned());
    // The fake model service runs on this runtime, so the command must not
    // block it.
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
        command
            .current_dir(cwd)
            .arg("--config")
            .arg(config_path)
            .args(args);
        strip_lci_env(&mut command);
        if let Some(path) = dump {
            command.env("LCI_CANDIDATE_DUMP", path);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn the_candidate_dump_variable_makes_the_command_line_append_a_line_and_print_the_same_response()
 {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("dump-env-cli");
    write_dump_workspace(&workspace);
    let config_path = write_dump_cli_config(temp.path(), &config);
    let cwd = temp.path().join("cwd");
    let logs = temp.path().join("logs");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&logs).unwrap();
    let dump_path = logs.join("candidates.jsonl");
    let search = |dump: Option<std::path::PathBuf>| {
        let args = vec![
            "search".to_owned(),
            workspace.display().to_string(),
            "translator".to_owned(),
            "--top-k".to_owned(),
            "3".to_owned(),
        ];
        let (config_path, cwd) = (config_path.clone(), cwd.clone());
        async move {
            let output = run_cli(&config_path, &cwd, args, dump).await;
            serde_json::from_slice::<Value>(&output.stdout).unwrap()
        }
    };
    run_cli(
        &config_path,
        &cwd,
        vec!["index".to_owned(), workspace.display().to_string()],
        None,
    )
    .await;

    // Without the variable nothing is written: not to the working directory,
    // not to the directory the dump would go to, not into the workspace.
    let workspace_files = || {
        let mut names: Vec<_> = std::fs::read_dir(workspace.join("src"))
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before = workspace_files();
    let plain = search(None).await;
    assert!(plain["reranked"].as_bool().unwrap(), "{plain}");
    assert_eq!(std::fs::read_dir(&cwd).unwrap().count(), 0);
    assert_eq!(std::fs::read_dir(&logs).unwrap().count(), 0);
    assert_eq!(workspace_files(), before);

    // With it: one line, and the same response.
    let dumped = search(Some(dump_path.clone())).await;
    for key in ["candidates", "rerank_outcome"] {
        assert!(dumped.get(key).is_none(), "the response has a {key} key");
    }
    assert_eq!(strip_timings(dumped.clone()), strip_timings(plain.clone()));
    let lines = dump_lines(&dump_path);
    assert_eq!(lines.len(), 1);
    assert_dump_line_shape(&lines[0]);
    assert_eq!(lines[0]["query"], "translator");
    assert_eq!(lines[0]["workspace_path"], plain["workspace"]["path"]);
    assert_eq!(lines[0]["rerank_outcome"], "scored");
    assert_eq!(lines[0]["rerank_candidate_count"], 4);
    assert_eq!(lines[0]["candidates"].as_array().unwrap().len(), 7);
    assert_eq!(lines[0]["timings"], dumped["timings"]);
    assert_eq!(std::fs::read_dir(&cwd).unwrap().count(), 0);

    // Another process appends to the same file.
    search(Some(dump_path.clone())).await;
    assert_eq!(dump_lines(&dump_path).len(), 2);

    // A blank variable is the same as none.
    let blank = run_cli(
        &config_path,
        &cwd,
        vec![
            "search".to_owned(),
            workspace.display().to_string(),
            "translator".to_owned(),
        ],
        Some(std::path::PathBuf::from("  ")),
    )
    .await;
    assert!(blank.status.success());
    assert_eq!(dump_lines(&dump_path).len(), 2);
    assert_eq!(std::fs::read_dir(&cwd).unwrap().count(), 0);
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

// ---------------------------------------------------------------------------
// The System One rescorer (`rescorer = "system_one"`)
// ---------------------------------------------------------------------------

/// `config` with the typed rescorer on, pointed at the fake service of
/// [`fixture`] (its `/v1/systemone` route), and no language server.
fn system_one_config(mut config: Config, timeout_ms: u64) -> Config {
    config.channels.lsp = false;
    config.rescorer = local_code_intelligence::config::Rescorer::SystemOne;
    config.system_one = Some(lci_core::provider::SystemOneConfig {
        provider: lci_core::provider::ProviderKind::SystemOneWire,
        url: config.embedding_url.clone(),
        model: "decider".into(),
        timeout_ms,
        ..Default::default()
    });
    config
}

fn write_system_one_workspace(root: &Path) {
    write(root, "src/translator.rs", "fn translator() {}\n");
    write(root, "src/other.rs", "fn unrelated_helper() {}\n");
}

async fn traced_search(app: &App, workspace: &Path) -> SearchReport {
    app.search_with_options(
        workspace,
        "translator",
        Some(2),
        FilterRequest::default(),
        SearchOptions {
            trace_candidates: true,
        },
    )
    .await
    .unwrap()
}

fn result_paths(report: &SearchReport) -> Vec<&str> {
    report
        .results
        .iter()
        .map(|hit| hit.chunk.relative_file_path.as_str())
        .collect()
}

#[tokio::test]
async fn the_system_one_rescorer_reorders_the_shortlist_and_the_reranker_is_not_asked() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("system-one");
    write_system_one_workspace(&workspace);

    // The reranker puts the translator first.
    let reranker_app = App::open(config.clone()).await.unwrap();
    let reranked = traced_search(&reranker_app, &workspace).await;
    assert_eq!(result_paths(&reranked)[0], "src/translator.rs");
    assert_eq!(fake.system_one_requests.load(Ordering::SeqCst), 0);
    let reranker_requests = fake.rerank_requests.load(Ordering::SeqCst);
    assert!(reranker_requests > 0);
    drop(reranker_app);

    // The typed service prefers the other file: one request, its order,
    // probabilities as scores, and no reranker request.
    let app = App::open(system_one_config(config, 5_000)).await.unwrap();
    let report = traced_search(&app, &workspace).await;
    assert!(report.reranked, "{:?}", report.warning);
    assert_eq!(report.warning, None);
    assert_eq!(report.rerank_outcome, Some(RerankOutcome::Scored));
    assert_eq!(
        result_paths(&report),
        ["src/other.rs", "src/translator.rs"],
        "the typed order replaces fusion order"
    );
    assert_eq!(
        report
            .results
            .iter()
            .map(|hit| hit.reranker_score)
            .collect::<Vec<_>>(),
        [Some(0.9), Some(0.2)]
    );
    let rows = report.candidates.as_ref().unwrap();
    assert!(
        rows.iter()
            .filter(|row| row.shortlisted)
            .all(|row| row.rerank_rank.is_some() && row.reranker_score.is_some())
    );
    assert_eq!(fake.system_one_requests.load(Ordering::SeqCst), 1);
    assert_eq!(
        fake.rerank_requests.load(Ordering::SeqCst),
        reranker_requests
    );

    // The wire request: model, the question, one candidate and one fixed
    // `noul` question per shortlisted chunk, snippet = path, newline, code.
    let body = fake.system_one_last.lock().unwrap().clone().unwrap();
    assert_eq!(body["model"], "decider");
    assert_eq!(body["state"]["question"], "translator");
    let candidates = body["state"]["candidates"].as_object().unwrap();
    let questions = body["questions"].as_object().unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(questions.len(), 2);
    for (id, candidate) in candidates {
        let path = candidate["path"].as_str().unwrap();
        assert!(
            candidate["snippet"]
                .as_str()
                .unwrap()
                .starts_with(&format!("{path}\n")),
            "{candidate}"
        );
        assert_eq!(candidate["line"], 1);
        assert_eq!(
            questions[&format!("q_{id}")],
            json!({
                "type": "noul",
                "instructions": format!("`candidates.{id}` is code the question is about or the place a change for it goes"),
            })
        );
    }
    task.abort();
}

#[tokio::test]
async fn a_failing_system_one_rescorer_falls_back_to_fusion_order_with_one_warning() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("system-one-failing");
    write_system_one_workspace(&workspace);

    // Fusion order is what the same search returns with no rescorer at all.
    let mut off_config = config.clone();
    off_config.channels.lsp = false;
    off_config.channels.reranker = false;
    let off = traced_search(&App::open(off_config).await.unwrap(), &workspace).await;
    assert_eq!(off.rerank_outcome, Some(RerankOutcome::Off));
    let fusion_order: Vec<String> = result_paths(&off).into_iter().map(str::to_owned).collect();

    let app = App::open(system_one_config(config, 400)).await.unwrap();
    let reranker_requests = fake.rerank_requests.load(Ordering::SeqCst);
    let mut asked = 0;
    for (name, mode) in [
        ("http error", SystemOneMode::Fail),
        ("row limit", SystemOneMode::TooLong),
        ("partial answer", SystemOneMode::Partial),
        ("timeout", SystemOneMode::Hang),
    ] {
        fake.system_one_mode.store(mode as usize, Ordering::SeqCst);
        let started = std::time::Instant::now();
        let report = traced_search(&app, &workspace).await;
        asked += 1;
        assert!(started.elapsed() < Duration::from_secs(10), "{name}");
        assert!(!report.reranked, "{name}");
        assert_eq!(report.rerank_outcome, Some(RerankOutcome::Failed), "{name}");
        assert_eq!(result_paths(&report), fusion_order, "{name}");
        assert!(
            report
                .results
                .iter()
                .all(|hit| hit.reranker_score.is_none()),
            "{name}"
        );
        let warning = report.warning.as_deref().unwrap();
        assert_eq!(
            warning
                .matches("System One rescorer unavailable or invalid response")
                .count(),
            1,
            "{name}: {warning}"
        );
        assert!(
            warning.contains("fusion-ranked results returned"),
            "{name}: {warning}"
        );
        assert_eq!(
            fake.system_one_requests.load(Ordering::SeqCst),
            asked,
            "{name}: exactly one request per search, no retry"
        );
    }

    // An error status means the service is reachable, so the next search asks
    // it again and uses its answer.
    fake.system_one_mode
        .store(SystemOneMode::Answer as usize, Ordering::SeqCst);
    let recovered = traced_search(&app, &workspace).await;
    assert_eq!(recovered.rerank_outcome, Some(RerankOutcome::Scored));
    assert_eq!(result_paths(&recovered)[0], "src/other.rs");
    // The reranker is never the fallback of the typed rescorer.
    assert_eq!(
        fake.rerank_requests.load(Ordering::SeqCst),
        reranker_requests
    );
    task.abort();
}

#[tokio::test]
async fn an_unreachable_system_one_service_is_skipped_for_the_window() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("system-one-down");
    write_system_one_workspace(&workspace);
    // Nothing listens on the closed port; the other models stay on the fake.
    let closed = ClosedPort::new();
    let mut config = system_one_config(config, 5_000);
    config.system_one.as_mut().unwrap().url = format!("http://127.0.0.1:{}", closed.port);
    let app = App::open(config).await.unwrap();
    let first = traced_search(&app, &workspace).await;
    assert_eq!(first.rerank_outcome, Some(RerankOutcome::Failed));
    assert!(
        !first
            .warning
            .as_deref()
            .unwrap()
            .contains("recently unreachable")
    );
    let second = traced_search(&app, &workspace).await;
    assert_eq!(second.rerank_outcome, Some(RerankOutcome::Failed));
    assert!(
        second
            .warning
            .as_deref()
            .unwrap()
            .contains("recently unreachable"),
        "{:?}",
        second.warning
    );
    assert_eq!(result_paths(&first), result_paths(&second));
    assert_eq!(fake.system_one_requests.load(Ordering::SeqCst), 0);
    task.abort();
}

#[tokio::test]
async fn system_one_echoed_keys_never_reach_warnings_or_error_chains() {
    const KEY_ENV: &str = "LCI_TEST_SYSTEM_ONE_ECHO_KEY";
    const SECRET: &str = "sk-system-one-echo-9d8251e6";
    // Set the environment only in a child, never in the threaded test process.
    if std::env::var(KEY_ENV).as_deref() != Ok(SECRET) {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "evaluation::system_one_echoed_keys_never_reach_warnings_or_error_chains",
                "--nocapture",
            ])
            .env(KEY_ENV, SECRET)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
        return;
    }

    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("system-one-secret");
    write_system_one_workspace(&workspace);
    let mut config = system_one_config(config, 5_000);
    config.system_one.as_mut().unwrap().api_key_env = Some(KEY_ENV.into());
    let models = local_code_intelligence::models::Models::new(&config).unwrap();
    let app = App::open(config).await.unwrap();
    let candidates = [lci_core::provider::WireCandidate::new(
        "a.rs",
        1,
        "fn a() {}",
    )];
    for (status, body, needle) in [
        (
            StatusCode::OK,
            json!({"answers": SECRET}),
            "answers must be an object",
        ),
        (
            StatusCode::OK,
            json!({"answers": {"q_c01": SECRET}}),
            "q_c01 must be an object",
        ),
        (
            StatusCode::OK,
            json!({"answers": {"q_c01": {"noul": SECRET}}}),
            "q_c01.noul must be a number",
        ),
        (
            StatusCode::UNAUTHORIZED,
            json!({"detail": SECRET}),
            "HTTP status 401",
        ),
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"detail": SECRET}),
            "HTTP status 500",
        ),
    ] {
        *fake.system_one_response.lock().unwrap() = Some((status, body));
        let error = models.system_one_rank("q", &candidates).await.unwrap_err();
        assert!(format!("{error:#}").contains(needle), "{error:#}");
        assert!(!format!("{error:?}").contains(SECRET));
        for cause in error.chain() {
            assert!(!cause.to_string().contains(SECRET));
        }
        let report = traced_search(&app, &workspace).await;
        assert_eq!(report.rerank_outcome, Some(RerankOutcome::Failed));
        let warning = report.warning.as_deref().unwrap();
        assert!(warning.contains(needle), "{warning}");
        assert!(!warning.contains(SECRET));
    }
    assert_eq!(fake.system_one_requests.load(Ordering::SeqCst), 10);

    // A failure body that echoes the key as a JSON string with one character
    // \u-escaped, which no literal replacement finds. Neither it nor any
    // other body text may reach the error chain or the warning: only the
    // status does.
    let escaped = format!("\\u0073{}", &SECRET[1..]);
    assert!(!escaped.contains(SECRET));
    *fake.system_one_response.lock().unwrap() = None;
    for (status, needle) in [
        (StatusCode::UNAUTHORIZED, "HTTP status 401"),
        (StatusCode::INTERNAL_SERVER_ERROR, "HTTP status 500"),
    ] {
        *fake.system_one_raw_response.lock().unwrap() = Some((
            status,
            format!(r#"{{"error":"{escaped}","detail":"private-body-text"}}"#),
        ));
        let error = models.system_one_rank("q", &candidates).await.unwrap_err();
        let report = traced_search(&app, &workspace).await;
        assert_eq!(report.rerank_outcome, Some(RerankOutcome::Failed));
        let warning = report.warning.clone().unwrap();
        assert!(warning.contains(needle), "{warning}");
        let mut texts = vec![format!("{error:#}"), format!("{error:?}"), warning];
        texts.extend(error.chain().map(ToString::to_string));
        for text in texts {
            for hidden in [SECRET, &SECRET[1..], escaped.as_str(), "private-body-text"] {
                assert!(!text.contains(hidden), "{hidden}: {text}");
            }
        }
        let status_error = error
            .downcast_ref::<lci_core::models::HttpStatusError>()
            .unwrap();
        assert!(status_error.body.is_empty(), "{}", status_error.body);
    }
    assert_eq!(fake.system_one_requests.load(Ordering::SeqCst), 14);
    task.abort();
}

#[test]
fn evaluation_settings_record_only_the_nondefault_rescorer() {
    let mut config = Config::default();
    // The old snapshot's field order and bytes must survive a round trip.
    let old = format!(
        r#"{{"rrf_k":60.0,"fusion":{},"rerank_candidate_count":24,"rerank_document_max_bytes":4096,"semantic_candidate_count":40,"lexical_candidate_count":40,"lsp_candidate_count":40,"channels":{},"k":8,"embedding_identity":{},"reranker_model":"qwen3-reranker-4b","rerank_mode":"replace"}}"#,
        serde_json::to_string(&config.fusion).unwrap(),
        serde_json::to_string(&config.channels).unwrap(),
        serde_json::to_string(&config.embedding_identity()).unwrap(),
    );
    let loaded: CandidateArm = serde_json::from_str(&old).unwrap();
    assert_eq!(loaded.rescorer, None);
    assert_eq!(serde_json::to_string(&loaded).unwrap(), old);
    assert_eq!(
        serde_json::to_string(&CandidateArm::new(&config, 8)).unwrap(),
        old
    );

    config.rescorer = local_code_intelligence::config::Rescorer::SystemOne;
    let arm = CandidateArm::new(&config, 8);
    assert_eq!(arm.rescorer.as_deref(), Some("system_one"));
    let json = serde_json::to_string(&arm).unwrap();
    assert!(json.contains(r#""rescorer":"system_one""#));
    assert_eq!(serde_json::from_str::<CandidateArm>(&json).unwrap(), arm);
}

#[test]
fn evaluation_settings_tell_system_one_models_and_batch_sizes_apart() {
    let system_one = |model: &str, batch_size: usize| {
        let mut config = Config::default();
        config.rescorer = local_code_intelligence::config::Rescorer::SystemOne;
        config.system_one = Some(lci_core::provider::SystemOneConfig {
            provider: lci_core::provider::ProviderKind::SystemOneWire,
            model: model.into(),
            batch_size,
            ..Default::default()
        });
        config
    };
    let base = CandidateArm::new(&system_one("decider-a", 24), 8);
    assert_eq!(base.system_one_model.as_deref(), Some("decider-a"));
    assert_eq!(base.system_one_batch_size, Some(24));
    let json = serde_json::to_string(&base).unwrap();
    assert!(json.contains(r#""system_one_model":"decider-a""#), "{json}");
    assert!(json.contains(r#""system_one_batch_size":24"#), "{json}");
    assert_eq!(serde_json::from_str::<CandidateArm>(&json).unwrap(), base);

    // Runs that differ only in the model or the batch size are different
    // arms with different snapshots.
    for (name, other) in [
        ("model", CandidateArm::new(&system_one("decider-b", 24), 8)),
        (
            "batch size",
            CandidateArm::new(&system_one("decider-a", 8), 8),
        ),
    ] {
        assert_ne!(other, base, "{name}");
        assert_ne!(serde_json::to_string(&other).unwrap(), json, "{name}");
    }

    // The reranker arm records nothing of System One, even with a leftover
    // `[system_one]` section, and its bytes are those of an older snapshot.
    let mut reranker = system_one("decider-a", 24);
    reranker.rescorer = local_code_intelligence::config::Rescorer::Reranker;
    let arm = CandidateArm::new(&reranker, 8);
    assert_eq!(
        (&arm.system_one_model, arm.system_one_batch_size),
        (&None, None)
    );
    assert_eq!(
        serde_json::to_string(&CandidateArm::new(&reranker, 8)).unwrap(),
        serde_json::to_string(&CandidateArm::new(&Config::default(), 8)).unwrap()
    );
    assert!(!serde_json::to_string(&arm).unwrap().contains("system_one"));
}

#[tokio::test]
async fn a_failed_reranker_keeps_its_original_log_message() {
    use std::io::Write;

    const CHILD: &str = "LCI_TEST_RERANKER_LOG_CHILD";
    // The log is captured by a process-wide subscriber, which must be the
    // only dispatcher the process has ever had. tracing caches, per call
    // site, whether any subscriber wants it, and while a process has a single
    // dispatcher that answer is computed from the dispatcher of whichever
    // thread first reaches the call site. A scoped subscriber therefore loses
    // the event whenever a concurrent test (the other reranker tests fail the
    // same way) reaches the `warn!` first on a thread with none, and the
    // cache is never rebuilt. A child process running only this test, with a
    // global default set before any call site is reached, has no such race.
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "evaluation::a_failed_reranker_keeps_its_original_log_message",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // A renamed test would make the child run nothing and pass.
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the child ran no test: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }

    #[derive(Clone, Default)]
    struct Log(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
    impl Write for Log {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let log = Log::default();
    let writer = log.clone();
    tracing::subscriber::set_global_default(
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish(),
    )
    .expect("no other subscriber is set in this child");

    let (temp, mut config, fake, task) = fixture().await;
    let workspace = temp.path().join("reranker-log");
    write_system_one_workspace(&workspace);
    config.channels.lsp = false;
    fake.rerank_fails.store(true, Ordering::SeqCst);
    let app = App::open(config).await.unwrap();
    let report = traced_search(&app, &workspace).await;
    assert_eq!(report.rerank_outcome, Some(RerankOutcome::Failed));
    let text = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.contains("reranker failed; returning fusion-ranked results"),
        "{text}"
    );
    assert!(!text.contains("Reranker failed;"), "{text}");
    task.abort();
}
