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
