//! Spike slice S3 (run-record.md): RunRecords written from the map
//! registry path (`App::record_through_maps`). The production search never
//! writes one.

use super::*;
use lci_maps::run_record::{RecordOptions, validate_line};

/// The offline fixture of the S2 golden and T0 tests: a C# file the fake
/// language server answers for, a Go source and a Python test, each chunk
/// with its own `golden_rank_<d>` marker so the fake embedder ranks them.
fn write_fixture(workspace: &Path) {
    write(
        workspace,
        "src/Calculator.cs",
        "// golden_rank_4\nnamespace Acceptance;\npublic sealed class Calculator\n{\n    // golden_rank_1\n    public int Add(int a, int b) => a + b;\n}\n",
    );
    write(
        workspace,
        "src/totals.go",
        "package totals\n\n// CalculatorTotal sums values. golden_rank_2\nfunc CalculatorTotal(values []int) int {\n\ttotal := 0\n\tfor _, v := range values {\n\t\ttotal += v\n\t}\n\treturn total\n}\n",
    );
    write(
        workspace,
        "tests/calculator_test.py",
        "def test_calculator_adds():\n    # golden_rank_3\n    assert 1 + 2 == 3\n",
    );
}

/// The fixture, indexed, with the fake C# server; the reranker is off, as
/// on the registry path, which has no reranker.
async fn indexed_fixture(
    name: &str,
) -> (
    tempfile::TempDir,
    App,
    std::path::PathBuf,
    Arc<FakeModels>,
    tokio::task::JoinHandle<()>,
) {
    let (temp, mut config, fake, task) = fixture().await;
    let workspace = temp.path().join(name);
    write_fixture(&workspace);
    let state = temp.path().join(format!("lsp-state-{name}"));
    config.lsp_search_budget_ms = 30_000;
    config.channels.reranker = false;
    config = with_fake_csharp_server(config, &state, &[], 10);
    let app = App::open(config).await.unwrap().with_candidate_dump(None);
    app.index(&workspace).await.unwrap();
    (temp, app, workspace, fake, task)
}

fn read_lines(path: &Path) -> (String, Vec<Value>) {
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.ends_with('\n'));
    let lines = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (text, lines)
}

/// Schema validity (every line written), shape, and the registry's answer
/// returned unchanged: the record is written beside `fuse_through_maps`'s
/// result, not instead of it.
#[tokio::test]
async fn a_registry_run_writes_a_schema_valid_run_record() {
    let (temp, app, workspace, _fake, task) = indexed_fixture("record").await;
    let dir = temp.path().join("records");
    let mut options = RecordOptions::new(&dir);
    options.top_k = Some(4);
    let recorded = app
        .record_through_maps(&workspace, "Calculator", &options)
        .await
        .unwrap();
    let path = recorded.record.clone().expect("the record was written");
    assert_eq!(path.parent().unwrap(), dir);
    let plain = app
        .fuse_through_maps(&workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(recorded.value.rows, plain.rows);
    assert_eq!(recorded.value.warnings, plain.warnings);
    assert_eq!(recorded.value.notes, plain.notes);

    let (_, lines) = read_lines(&path);
    for line in &lines {
        validate_line(line).unwrap_or_else(|error| panic!("{error}: {line}"));
    }
    let kinds: Vec<(&str, &str)> = lines
        .iter()
        .map(|line| {
            (
                line["type"].as_str().unwrap(),
                line["kind"].as_str().unwrap_or(""),
            )
        })
        .collect();
    assert_eq!(
        kinds,
        [
            ("run", ""),
            ("step", "fan_out"),
            ("step", "fuse"),
            ("step", "cap_cut"),
            ("end", "")
        ]
    );
    let run_id = lines[0]["run_id"].as_str().unwrap();
    assert!(lines.iter().all(|line| line["run_id"] == run_id));
    assert_eq!(path.file_name().unwrap(), &*format!("{run_id}.jsonl"));
    let planned: Vec<&str> = lines[0]["plan"]["maps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|map| map["map"].as_str().unwrap())
        .collect();
    let called: Vec<&str> = lines[1]["map_calls"]
        .as_array()
        .unwrap()
        .iter()
        .chain(lines[2]["map_calls"].as_array().unwrap())
        .map(|call| call["map"].as_str().unwrap())
        .collect();
    assert_eq!(planned, called, "the plan lists the maps in fusion order");
    assert_eq!(
        planned,
        [
            "vec.chunk.code",
            "lexical",
            "lsp.symbols",
            "prior.source_role"
        ]
    );
    let pool = plain.rows.len();
    assert_eq!(lines[2]["frontier"]["size"], pool);
    assert_eq!(lines[3]["frontier"]["size"], pool);
    let results = lines[4]["results"].as_array().unwrap();
    assert_eq!(results.len(), pool.min(4));
    assert_eq!(lines[4]["outcome"], "completed");
    assert_eq!(lines[4]["totals"]["steps"], 3);
    task.abort();
}

/// Privacy (run-record.md 3.2): no absolute path of this machine, no data
/// directory, no service URL in any string, even when a failing map's
/// warning names them; snippets only when asked for.
#[tokio::test]
async fn a_run_record_carries_no_machine_paths_urls_or_unasked_code() {
    let (temp, app, workspace, fake, task) = indexed_fixture("privacy").await;
    let config = app.config.clone();
    let mut forbidden = vec![
        temp.path().to_string_lossy().into_owned(),
        workspace.to_string_lossy().into_owned(),
        config.data_dir.to_string_lossy().into_owned(),
        config.embedding_url.clone(),
        config.reranker_url.clone(),
        config.embedding_url.trim_end_matches("/v1").to_owned(),
        config
            .embedding_url
            .trim_start_matches("http://")
            .trim_end_matches("/v1")
            .to_owned(),
    ];
    if let Ok(resolved) = std::fs::canonicalize(temp.path()) {
        forbidden.push(resolved.to_string_lossy().into_owned());
    }
    forbidden.extend(config.csharp.path.clone());
    // A query that names the machine's paths and a service URL: the record
    // stores the query text, so only redaction keeps them out.
    let host = config
        .embedding_url
        .trim_start_matches("http://")
        .trim_end_matches("/v1")
        .to_owned();
    let query = format!(
        "Calculator {} {} {} file://{}/src {}",
        workspace.display(),
        config.data_dir.display(),
        config.embedding_url,
        workspace.display(),
        host
    );
    for embedding_fails in [false, true] {
        fake.embed_fails.store(embedding_fails, Ordering::SeqCst);
        for snippets in [false, true] {
            let mut options = RecordOptions::new(temp.path().join("records"));
            options.capture_snippets = snippets;
            // Given with a trailing separator, as a caller may pass it.
            let recorded = app
                .record_through_maps(&workspace.join(""), &query, &options)
                .await
                .unwrap();
            if embedding_fails {
                assert!(
                    recorded
                        .value
                        .warnings
                        .iter()
                        .any(|w| w.contains("Query embedding unavailable")),
                    "{:?}",
                    recorded.value.warnings
                );
            }
            let (text, lines) = read_lines(&recorded.record.unwrap());
            assert!(
                text.contains(
                    "Calculator <workspace> <data_dir> <embedding_url> <workspace>/src <embedding_host>"
                ),
                "{text}"
            );
            for secret in &forbidden {
                assert!(!text.contains(secret.as_str()), "{secret} in {text}");
                let escaped = serde_json::to_string(secret).unwrap();
                let escaped = escaped.trim_matches('"');
                assert!(!text.contains(escaped), "{escaped} in {text}");
            }
            let results = lines.last().unwrap()["results"].as_array().unwrap().clone();
            assert!(!results.is_empty());
            for row in &results {
                assert_eq!(row.get("snippet").is_some(), snippets, "{row}");
            }
            let code_marker = "public int Add";
            assert_eq!(text.contains(code_marker), snippets);
        }
    }
    fake.embed_fails.store(false, Ordering::SeqCst);
    task.abort();
}

/// Fail-open writer: a record directory that cannot be created makes no
/// difference to the run; the error is reported beside the result.
#[tokio::test]
async fn an_unwritable_record_directory_never_fails_the_run() {
    let (temp, app, workspace, _fake, task) = indexed_fixture("unwritable").await;
    let blocker = temp.path().join("not-a-directory");
    std::fs::write(&blocker, "a file where the record directory should be").unwrap();
    let recorded = app
        .record_through_maps(&workspace, "Calculator", &RecordOptions::new(&blocker))
        .await
        .unwrap();
    let error = recorded.record.expect_err("nothing could be written");
    assert!(!error.is_empty());
    let plain = app
        .fuse_through_maps(&workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(recorded.value.rows, plain.rows);
    assert_eq!(recorded.value.warnings, plain.warnings);
    assert_eq!(recorded.value.notes, plain.notes);
    task.abort();
}
