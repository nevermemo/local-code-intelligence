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
