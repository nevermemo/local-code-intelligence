//! Fixtures here are tiny and synthetic: rows written by the tests, never
//! dataset data.

use super::super::{Set, parse_run, score_set, to_evaluate};
use super::*;

fn diff(path: &str, created: bool) -> String {
    let mode = if created {
        "new file mode 100644\n"
    } else {
        ""
    };
    format!(
        "diff --git a/{path} b/{path}\n{mode}index 111..222 100644\n--- a/{path}\n+++ b/{path}\n@@ -1 +1 @@\n-x\n+y\n"
    )
}

fn polybench_row(id: &str, repo: &str, question: &str, patch: &str) -> Row {
    Row {
        row: 0,
        id: format!("polybench:{id}"),
        repo: repo.to_string(),
        base_commit: "0123abc".to_string(),
        language: "python".to_string(),
        question_parts: vec![question.to_string()],
        patch: Some(patch.to_string()),
        ..Row::default()
    }
}

fn provenance() -> Provenance {
    Provenance {
        revision: Some("rev1".to_string()),
        sha256: "aa".repeat(32),
    }
}

fn polybench_csv(rows: &[[&str; 7]]) -> Vec<u8> {
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer
        .write_record([
            "instance_id",
            "repo",
            "base_commit",
            "language",
            "patch",
            "problem_statement",
            "modified_nodes",
        ])
        .unwrap();
    for row in rows {
        writer.write_record(row).unwrap();
    }
    writer.into_inner().unwrap()
}

fn convert_one(source: Source, row: Row) -> Conversion {
    convert(source, &[row], &provenance(), &[])
}

#[test]
fn a_polybench_row_converts_to_a_file_level_exemplar_item() {
    let patch = format!(
        "{}{}{}",
        diff("src/a.py", false),
        diff("tests/test_a.py", false),
        diff("src/new.py", true)
    );
    let mut row = polybench_row(
        "acme__widgets-1",
        "acme/widgets",
        "Crash on empty input",
        &patch,
    );
    row.nodes = Some(r#"["src/a.py->program->function_definition:run", "src/a.py->program->function_definition:run"]"#.to_string());
    let out = convert_one(Source::SwePolybenchVerified, row);
    assert_eq!(out.items.len(), 1);
    let item = &out.items[0];
    assert_eq!(item["id"], "polybench:acme__widgets-1");
    assert_eq!(item["set"], "where-to");
    assert_eq!(item["question"], "Crash on empty input");
    assert_eq!(item["gold"]["form"], "exemplar");
    assert_eq!(item["gold"]["granularity"], "file");
    assert_eq!(item["gold"]["members"], json!([{"path": "src/a.py"}]));
    assert_eq!(
        item["gold"]["symbols"],
        json!(["src/a.py->program->function_definition:run"])
    );
    assert_eq!(item["stratum"], "external/clean");
    let ext = &item["external"];
    assert_eq!(
        ext["excluded_files"],
        json!({"test": 1, "created": 1, "unsupported_extension": 0})
    );
    assert_eq!(ext["licence"], "MIT");
    assert_eq!(ext["dataset"], "AmazonScience/SWE-PolyBench_Verified");
    assert_eq!(ext["revision"], "rev1");
    assert_eq!(ext["sha256"], "aa".repeat(32));
    assert_eq!(ext["heldout"], true);
    assert_eq!(ext["overlap"], false);
    assert_eq!(ext["question_truncated"], false);
    assert_eq!(ext["repo"], "acme/widgets");
    assert_eq!(ext["base_commit"], "0123abc");
}

#[test]
fn an_lca_row_converts_from_changed_files_and_kotlin_is_marked_unsupported() {
    let columns = |files: Value, diff_text: &str| -> Map<String, Value> {
        let value = json!({
            "repo_owner": "acme", "repo_name": "gadgets", "base_sha": "feedbee",
            "issue_title": "Title", "issue_body": "Body text", "changed_files": files,
            "diff": diff_text, "id": 7,
        });
        value.as_object().unwrap().clone()
    };
    let mixed =
        lca_row_from_columns("java", 0, &columns(json!(["app/x.kt", "app/y.java"]), "")).unwrap();
    assert_eq!(mixed.id, "lca:java:7");
    assert_eq!(mixed.repo, "acme/gadgets");
    assert_eq!(mixed.language, "java");
    let out = convert_one(Source::LcaBugloc, mixed);
    assert_eq!(out.items.len(), 1);
    let item = &out.items[0];
    assert_eq!(item["gold"]["members"], json!([{"path": "app/y.java"}]));
    assert_eq!(item["question"], "Title\n\nBody text");
    assert_eq!(
        item["external"]["excluded_files"]["unsupported_extension"],
        1
    );
    assert_eq!(item["external"]["licence"], "Apache-2.0");

    let kt_only = lca_row_from_columns("kt", 1, &columns(json!(["app/x.kt"]), "")).unwrap();
    assert_eq!(kt_only.language, "kotlin");
    let out = convert_one(Source::LcaBugloc, kt_only);
    assert!(out.items.is_empty());
    assert_eq!(out.report["dropped"]["no_member"], 1);
    assert_eq!(out.report["excluded_files"]["unsupported_extension"], 1);

    // The diff finds created files; changed_files still names the candidates.
    let created = lca_row_from_columns(
        "py",
        2,
        &columns(
            json!(["pkg/new.py", "pkg/old.py"]),
            &diff("pkg/new.py", true),
        ),
    )
    .unwrap();
    let out = convert_one(Source::LcaBugloc, created);
    assert_eq!(
        out.items[0]["gold"]["members"],
        json!([{"path": "pkg/old.py"}])
    );
    assert_eq!(out.items[0]["external"]["excluded_files"]["created"], 1);
}

#[test]
fn the_lca_adapter_names_a_missing_column_and_reads_a_json_encoded_list() {
    let mut cols = json!({
        "repo_owner": "o", "repo_name": "n", "base_sha": "b", "issue_title": "t",
        "issue_body": "x", "changed_files": "[\"a/b.py\"]", "diff": "", "id": "3",
    })
    .as_object()
    .unwrap()
    .clone();
    let row = lca_row_from_columns("py", 0, &cols).unwrap();
    assert_eq!(row.changed_files, Some(vec!["a/b.py".to_string()]));
    cols.remove("base_sha");
    let error = lca_row_from_columns("py", 0, &cols)
        .unwrap_err()
        .to_string();
    assert!(error.contains("base_sha"), "{error}");
}

#[test]
fn a_polybench_csv_with_quoted_multiline_patches_parses() {
    let patch = format!("{}+say(\"hi, there\")\r\n", diff("src/a.py", false));
    let statement = "Line one,\nline \"two\"\r\nline three";
    let bytes = polybench_csv(&[
        ["a__b-1", "a/b", "c0ffee", "Python", &patch, statement, "[]"],
        [
            "a__b-2",
            "a/b",
            "c0ffee",
            "Java",
            "diff --git a/X.java b/X.java\n",
            "Second",
            "",
        ],
    ]);
    let rows = read_polybench_csv(&bytes).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].id, "polybench:a__b-1");
    assert_eq!(rows[0].language, "python");
    assert_eq!(rows[0].patch.as_deref(), Some(patch.as_str()));
    assert_eq!(rows[0].question_parts, vec![statement.to_string()]);
    assert_eq!(rows[1].row, 1);
    assert_eq!(rows[1].language, "java");
    // A byte order mark in front of the header is tolerated.
    let mut with_bom = b"\xEF\xBB\xBF".to_vec();
    with_bom.extend_from_slice(&bytes);
    assert_eq!(read_polybench_csv(&with_bom).unwrap().len(), 2);
    // The CRLF inside the question is normalised by the converter.
    let out = convert_one(Source::SwePolybenchVerified, rows[0].clone());
    assert_eq!(
        out.items[0]["question"],
        "Line one,\nline \"two\"\nline three"
    );
}

#[test]
fn a_polybench_csv_missing_a_column_fails_with_its_name() {
    let error = read_polybench_csv(b"instance_id,repo\nx,y\n")
        .unwrap_err()
        .to_string();
    assert!(error.contains("base_commit"), "{error}");
}

#[test]
fn the_leak_rule_marks_a_question_that_names_a_member_stem_or_directory() {
    let leak =
        |question: &str, member: &str| leak_of(question, &[member.to_string()], "acme/widgets");
    // The stem as a whole word.
    assert!(leak("Calling Parser.run fails", "src/parser.py"));
    assert!(!leak("Calling run fails", "src/parser.py"));
    // A generic stem does not, nor does a short one.
    assert!(!leak("The index is wrong", "src/index.ts"));
    assert!(!leak("main entry crashes", "src/main.py"));
    assert!(!leak("an io problem", "src/io.py"));
    // The path verbatim always does, whatever the stem.
    assert!(leak("see src/index.ts for the bug", "src/index.ts"));
    // A directory token of 4+ characters does, split on `_` and camelCase.
    assert!(leak("The scheduler stalls", "lib/scheduler/queue.py"));
    assert!(leak(
        "the request handler is slow",
        "x/request_handler/zz.py"
    ));
    assert!(leak(
        "the request handler is slow",
        "x/requestHandler/zz.py"
    ));
    // Generic and short directory tokens, and the repository name, never do.
    assert!(!leak(
        "the src folder and the core app",
        "src/core/app/zzz.py"
    ));
    assert!(!leak("the util module", "ab/zzz.py"));
    assert!(!leak("widgets are broken", "widgets/zzz.py"));
    // The leak check sees only the question: a different file stays clean.
    assert!(!leak("Crash on startup", "pkg/qqq.py"));
}

#[test]
fn a_member_named_in_the_question_gets_the_leak_stratum() {
    let row = polybench_row(
        "p-1",
        "acme/widgets",
        "The bug is in parser.py",
        &diff("src/parser.py", false),
    );
    let out = convert_one(Source::SwePolybenchVerified, row);
    assert_eq!(out.items[0]["stratum"], "external/leak");
    assert_eq!(out.report["leak"], 1);
    assert_eq!(out.report["clean"], 0);
}

#[test]
fn the_test_rule_and_the_portability_rule_follow_the_plan() {
    for path in [
        "tests/a.py",
        "pkg/test/a.py",
        "pkg/__tests__/a.js",
        "pkg/spec/a.rb",
        "pkg/Testing/a.py",
        "a/b.test.ts",
        "a/b.spec.ts",
        "a/b_test.go",
        "a/test_b.py",
        "a/FooTest.java",
        "a/FooTests.java",
    ] {
        assert!(is_test_path(path), "{path}");
    }
    for path in [
        "src/a.py",
        "a/test.py",
        "a/contest.py",
        "a/Tester.java",
        "lib/latest/a.py",
    ] {
        assert!(!is_test_path(path), "{path}");
    }
    for path in ["a.py", "src/a/b.py"] {
        assert!(portable_path(path), "{path}");
    }
    for path in [
        "",
        "/etc/a.py",
        "../a.py",
        "a/../b.py",
        "C:/a.py",
        "a\\b.py",
        "a//b.py",
        "./a.py",
        "\"a b\".py",
    ] {
        assert!(!portable_path(path), "{path}");
    }
}

#[test]
fn a_patch_parser_reads_created_renamed_and_spaced_paths() {
    let patch = format!(
        "{}{}diff --git a/old/name.py b/new/name.py\nsimilarity index 90%\nrename from old/name.py\nrename to new/name.py\n{}",
        diff("with space/a b.py", false),
        diff("fresh.py", true),
        "diff --git \"a/q\\303.py\" \"b/q\\303.py\"\n"
    );
    let files = patch_paths(&patch);
    let summary: Vec<(&str, bool)> = files.iter().map(|f| (f.path.as_str(), f.created)).collect();
    assert_eq!(summary[0], ("with space/a b.py", false));
    assert_eq!(summary[1], ("fresh.py", true));
    assert_eq!(summary[2], ("old/name.py", false));
    assert!(!portable_path(summary[3].0));
}

#[test]
fn an_absolute_or_parent_path_drops_the_row_and_counts_it() {
    let rows: Vec<Row> = [
        ("a", diff("../escape.py", false)),
        ("b", diff("/etc/passwd.py", false)),
        ("c", diff("src/fine.py", false)),
    ]
    .into_iter()
    .map(|(id, patch)| polybench_row(id, "acme/widgets", "Question", &patch))
    .collect();
    let out = convert(Source::SwePolybenchVerified, &rows, &provenance(), &[]);
    assert_eq!(out.items.len(), 1);
    assert_eq!(out.items[0]["id"], "polybench:c");
    assert_eq!(out.report["dropped"]["portability"], 2);
    // An LCA row with a drive-letter path is dropped the same way.
    let mut row = polybench_row("d", "acme/widgets", "Question", "");
    row.changed_files = Some(vec!["C:/x.py".to_string()]);
    let out = convert_one(Source::LcaBugloc, row);
    assert_eq!(out.report["dropped"]["portability"], 1);
}

#[test]
fn rows_without_a_question_or_without_a_member_are_dropped_and_counted() {
    let rows = vec![
        polybench_row("a", "acme/widgets", "  \r\n ", &diff("src/a.py", false)),
        polybench_row(
            "b",
            "acme/widgets",
            "Question",
            &diff("tests/test_b.py", false),
        ),
        polybench_row("c", "acme/widgets", "Question", &diff("docs/c.png", false)),
        polybench_row("d", "acme/widgets", "Question", &diff("src/d.py", true)),
    ];
    let out = convert(Source::SwePolybenchVerified, &rows, &provenance(), &[]);
    assert!(out.items.is_empty());
    assert_eq!(
        out.report["dropped"],
        json!({"no_member": 3, "portability": 0, "no_question": 1})
    );
    assert_eq!(
        out.report["excluded_files"],
        json!({"test": 1, "created": 1, "unsupported_extension": 1})
    );
    assert_eq!(out.report["members_per_item"]["p50"], Value::Null);
}

#[test]
fn a_question_over_the_cap_is_cut_on_a_char_boundary_and_flagged() {
    let long = "é".repeat(QUESTION_MAX_CHARS + 100);
    let row = polybench_row("a", "acme/widgets", &long, &diff("src/a.py", false));
    let out = convert_one(Source::SwePolybenchVerified, row);
    let item = &out.items[0];
    let question = item["question"].as_str().unwrap();
    assert_eq!(question.chars().count(), QUESTION_MAX_CHARS);
    assert_eq!(item["external"]["question_truncated"], true);
    assert_eq!(item["external"]["question_chars"], QUESTION_MAX_CHARS + 100);
    assert_eq!(out.report["truncated"], 1);
    // Exactly at the cap is not truncated.
    let exact = "a".repeat(QUESTION_MAX_CHARS);
    let row = polybench_row("b", "acme/widgets", &exact, &diff("src/a.py", false));
    let out = convert_one(Source::SwePolybenchVerified, row);
    assert_eq!(out.items[0]["external"]["question_truncated"], false);
}

#[test]
fn converted_items_score_through_where_to_and_convert_to_evaluate() {
    let patch = format!("{}{}", diff("src/a.py", false), diff("src/b.py", false));
    let rows = vec![
        polybench_row("p-1", "acme/widgets", "Crash on startup", &patch),
        polybench_row(
            "p-2",
            "acme/widgets",
            "Why does beta fail",
            &diff("src/beta.py", false),
        ),
    ];
    let out = convert(Source::SwePolybenchVerified, &rows, &provenance(), &[]);
    assert_eq!(out.items.len(), 2);
    let run =
        parse_run(r#"{"polybench:p-1": ["src/a.py", "src/b.py"], "polybench:p-2": ["src/z.py"]}"#)
            .unwrap();
    let report = score_set(Set::WhereTo, &out.items, &run, false, false).unwrap();
    assert_eq!(report["overall"]["n"], 2);
    assert_eq!(report["strata"]["leak"]["clean"]["n"], 1);
    assert_eq!(
        report["strata"]["leak"]["clean"]["metrics"]["set_recall"],
        1.0
    );
    assert_eq!(report["strata"]["leak"]["leak"]["n"], 1);
    assert_eq!(
        report["strata"]["leak"]["leak"]["metrics"]["set_recall"],
        0.0
    );

    let (toml_text, summary) = to_evaluate(Set::WhereTo, &out.items, "ext", 10).unwrap();
    assert!(summary.contains("converted 2 of 2"), "{summary}");
    assert!(toml_text.contains("src/a.py") && toml_text.contains("src/b.py"));
    assert!(toml_text.contains("Crash on startup"));
    assert!(toml_text.contains("stratum:external/clean"));
}

#[test]
fn the_report_counts_per_language_repo_leak_and_overlap() {
    let mut rows = vec![
        polybench_row(
            "p-1",
            "Acme/Widgets",
            "Crash on startup",
            &diff("src/a.py", false),
        ),
        polybench_row(
            "p-2",
            "acme/widgets",
            "The parser file",
            &diff("src/parser.py", false),
        ),
        polybench_row(
            "p-3",
            "other/thing",
            "Boom",
            &format!("{}{}", diff("x/q.js", false), diff("x/r.js", false)),
        ),
    ];
    rows[2].language = "javascript".to_string();
    let out = convert(
        Source::SwePolybenchVerified,
        &rows,
        &provenance(),
        &["acme/widgets".to_string()],
    );
    let report = &out.report;
    assert_eq!(report["rows"], 3);
    assert_eq!(report["items"], 3);
    assert_eq!(
        report["per_language"],
        json!({"javascript": 1, "python": 2})
    );
    assert_eq!(
        report["per_repo"],
        json!({"Acme/Widgets": 1, "acme/widgets": 1, "other/thing": 1})
    );
    assert_eq!(report["leak"], 1);
    assert_eq!(report["clean"], 2);
    assert_eq!(report["overlap"], 2);
    assert_eq!(
        report["overlap_repos"],
        json!({"Acme/Widgets": 1, "acme/widgets": 1})
    );
    assert_eq!(report["members_per_item"]["max"], 2);
    assert_eq!(report["members_per_item"]["min"], 1);
    assert_eq!(out.items[0]["external"]["overlap"], true);
    assert_eq!(out.items[2]["external"]["overlap"], false);
    assert_eq!(report["licence"], "MIT");
    assert_eq!(report["heldout"], true);
    assert!(headline(report).contains("3 items (leak 1, clean 2, overlap 2"));
}

#[test]
fn items_come_out_in_id_order() {
    let rows = vec![
        polybench_row("p-2", "a/b", "Q", &diff("src/a.py", false)),
        polybench_row("p-1", "a/b", "Q", &diff("src/a.py", false)),
    ];
    let out = convert(Source::SwePolybenchVerified, &rows, &provenance(), &[]);
    let ids: Vec<&str> = out
        .items
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["polybench:p-1", "polybench:p-2"]);
}

#[test]
fn convert_writes_the_gold_and_a_report_without_question_text() {
    let dir = std::env::temp_dir().join(format!("lci-external-{}", std::process::id()));
    std::fs::remove_dir_all(&dir).ok();
    let (input, out) = (dir.join("in"), dir.join("gold"));
    std::fs::create_dir_all(&input).unwrap();
    let secret = "Distinctive synthetic question text";
    let bytes = polybench_csv(&[[
        "p-1",
        "acme/widgets",
        "abc",
        "Python",
        &diff("src/a.py", false),
        secret,
        "",
    ]]);
    std::fs::write(input.join("test.csv"), &bytes).unwrap();
    run_convert(
        Source::SwePolybenchVerified,
        &input,
        &out,
        &["acme/widgets".to_string()],
        Some("rev9"),
        None,
    )
    .unwrap();
    let items: Value = serde_json::from_str(
        &std::fs::read_to_string(out.join("external-swe-polybench-verified.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(items.as_array().unwrap().len(), 1);
    assert_eq!(items[0]["question"], secret);
    assert_eq!(items[0]["external"]["sha256"], hex(&Sha256::digest(&bytes)));
    assert_eq!(items[0]["external"]["revision"], "rev9");
    assert_eq!(items[0]["external"]["overlap"], true);
    let report_text =
        std::fs::read_to_string(out.join("external-swe-polybench-verified.report.json")).unwrap();
    assert!(!report_text.contains(secret));
    assert!(!report_text.contains("src/a.py"));
    // The gold file loads like any other where-to gold file.
    let loaded =
        super::super::load_gold(&out.join("external-swe-polybench-verified.json")).unwrap();
    assert_eq!(loaded.len(), 1);
    // The LCA source waits for the parquet reader (K4b) and says so.
    let error = run_convert(Source::LcaBugloc, &input, &out, &[], None, None).unwrap_err();
    assert!(format!("{error:#}").contains("K4b"), "{error:#}");
    // A malformed --known-repo is refused before anything is read.
    let error = run_convert(
        Source::SwePolybenchVerified,
        &input,
        &out,
        &["nope".to_string()],
        None,
        None,
    )
    .unwrap_err();
    assert!(error.to_string().contains("owner/name"));
    std::fs::remove_dir_all(&dir).ok();
}
