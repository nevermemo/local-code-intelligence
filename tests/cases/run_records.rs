//! Spike slice S3 (run-record.md): RunRecords written from the map
//! registry path (`App::record_through_maps`). The production search never
//! writes one.

use super::*;
use lci_maps::run_record::{RecordItem, RecordOptions, validate_line};
use local_code_intelligence::evaluate::RunRecordFile;

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
    tweak: impl FnOnce(&mut Config),
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
    config = with_fake_csharp_server(config, &state, &[], LOADED_BUDGET_SECONDS);
    tweak(&mut config);
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
    let (temp, app, workspace, _fake, task) = indexed_fixture("record", |_| ()).await;
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

    // The profile stamp is the resolved run profile of the app's config.
    let profile = &lines[0]["profile"];
    let resolved = app.config.run_profile();
    assert_eq!(profile["hash"], resolved.hash);
    let keys: Vec<&str> = profile["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap())
        .collect();
    assert_eq!(keys, resolved.keys);
    assert!(keys.windows(2).all(|pair| pair[0] < pair[1]), "sorted");
    assert_eq!(
        profile["embedding_identity_sha256"],
        lines[0]["snapshot"]["embedding_identity_sha256"]
    );
    assert!(profile["priors_hash"].is_null() && profile["tie_break_hash"].is_null());
    task.abort();
}

/// Privacy (run-record.md 3.2): no absolute path of this machine, no data
/// directory, no service URL in any string, even when a failing map's
/// warning names them; snippets only when asked for.
#[tokio::test]
async fn a_run_record_carries_no_machine_paths_urls_or_unasked_code() {
    let (temp, app, workspace, fake, task) = indexed_fixture("privacy", |_| ()).await;
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
    // A file URI as tools write it: `/` separators, and `file:///C:/...` on
    // Windows.
    let slashed = workspace.to_string_lossy().replace('\\', "/");
    let uri = if slashed.starts_with('/') {
        format!("file://{slashed}")
    } else {
        format!("file:///{slashed}")
    };
    let query = format!(
        "Calculator {} {} {} {uri}/src {}",
        workspace.display(),
        config.data_dir.display(),
        config.embedding_url,
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
    let (temp, app, workspace, _fake, task) = indexed_fixture("unwritable", |_| ()).await;
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

/// Fail-open stamp: a configured tie-break file that cannot be read makes the
/// record an error and leaves the answer as it is; no record file is left.
#[tokio::test]
async fn an_unreadable_profile_input_never_fails_the_run() {
    let (temp, app, workspace, _fake, task) = indexed_fixture("stamp", |config| {
        config.traverse.tie_break = Some("no-such-tie-break.json".into());
    })
    .await;
    let dir = temp.path().join("stamp-records");
    let recorded = app
        .record_through_maps(&workspace, "Calculator", &RecordOptions::new(&dir))
        .await
        .unwrap();
    let error = recorded.record.expect_err("the stamp cannot be made");
    assert!(error.contains("profile stamp"), "{error}");
    assert!(!recorded.value.rows.is_empty());
    let plain = app
        .fuse_through_maps(&workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(recorded.value.rows, plain.rows);
    let written = std::fs::read_dir(&dir).map(|d| d.count()).unwrap_or(0);
    assert_eq!(written, 0, "no record file is left behind");
    task.abort();
}

/// The offline harness on the fixture (run-record.md section 5, spike S4's
/// cloud part), for every query of the public fixture gold file:
/// - one-shot equivalence: the record's results are `search_code`'s results
///   with the reranker off (path, lines, order, source role, `fusion_score`
///   bits); its `fuse` rows are the traced candidates (rank, lines, score
///   bits) and its `cap_cut` rows their capped ranks and shortlist; each
///   map's recorded ranks are the traced channel ranks;
/// - replay: re-fusing the recorded map hits with the plan's weights gives
///   every recorded score bit for bit;
/// - scoring: the record scored against the gold query gives the metrics
///   `evaluate` computes from the search's own results;
/// - contract C5 offline: every used hit carries a label on its map's card.
#[tokio::test]
async fn recorded_runs_reproduce_search_code_replay_bit_for_bit_and_score_like_evaluate() {
    let (temp, app, workspace, _fake, task) = indexed_fixture("harness", |config| {
        // Exercise the per-file cap and the shortlist cut, which the
        // defaults leave idle on this small fixture.
        config.fusion.max_chunks_per_file = 1;
        config.rerank_candidate_count = 3;
    })
    .await;
    let gold = load_definition(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/run_records/gold.toml"),
    )
    .unwrap();
    let dir = temp.path().join("records");
    let role = |role: local_code_intelligence::filter::SourceRole| {
        serde_json::to_value(role)
            .unwrap()
            .as_str()
            .unwrap()
            .to_owned()
    };
    let (mut hits, mut demoted, mut cut) = (0, 0, 0);
    for query in &gold.queries {
        let mut options = RecordOptions::new(&dir);
        options.top_k = Some(8);
        options.capture_snippets = true;
        options.item = Some(RecordItem {
            set: "run-records-fixture".into(),
            id: query.query.clone(),
        });
        let recorded = app
            .record_through_maps(&workspace, &query.query, &options)
            .await
            .unwrap();
        let record = RunRecordFile::read(&recorded.record.unwrap()).unwrap();
        assert_eq!(record.run["item"]["id"], query.query.as_str());
        assert_eq!(record.run["item"]["set"], "run-records-fixture");
        let report = app
            .search_with_options(
                &workspace,
                &query.query,
                Some(8),
                local_code_intelligence::filter::FilterRequest::default(),
                local_code_intelligence::app::SearchOptions {
                    trace_candidates: true,
                },
            )
            .await
            .unwrap();
        assert!(!report.reranked);
        let label = &query.query;

        // Results: the same list as search_code.
        let end = record.end().unwrap();
        let results = end["results"].as_array().unwrap();
        assert_eq!(results.len(), report.results.len(), "{label}");
        assert!(!results.is_empty(), "{label}");
        let location = |row: &Value| {
            let node: lci_core::node::NodeId = row["node"].as_str().unwrap().parse().unwrap();
            (
                node.path().unwrap().to_owned(),
                row["start_line"].as_u64().unwrap() as u32,
                row["end_line"].as_u64().unwrap() as u32,
            )
        };
        let bits = |value: &Value| (value.as_f64().unwrap() as f32).to_bits();
        for (row, hit) in results.iter().zip(&report.results) {
            assert_eq!(
                location(row),
                (
                    hit.chunk.relative_file_path.clone(),
                    hit.chunk.start_line,
                    hit.chunk.end_line
                ),
                "{label}"
            );
            assert_eq!(bits(&row["score"]), hit.fusion_score.to_bits(), "{label}");
            assert_eq!(row["source_role"], role(hit.source_role), "{label}");
        }

        // Frontier rows: the traced candidates.
        let traced = report.candidates.as_ref().unwrap();
        let fuse = &record.steps[1];
        let cap_cut = &record.steps[2];
        assert_eq!(
            (fuse["kind"].as_str(), cap_cut["kind"].as_str()),
            (Some("fuse"), Some("cap_cut"))
        );
        let fuse_rows = fuse["frontier"]["rows"].as_array().unwrap();
        assert_eq!(fuse_rows.len(), traced.len(), "{label}");
        let mut by_location = std::collections::HashMap::new();
        for (row, trace) in fuse_rows.iter().zip(traced) {
            let at = (
                trace.relative_file_path.clone(),
                trace.start_line,
                trace.end_line,
            );
            assert_eq!(location(row), at, "{label}");
            assert_eq!(row["fused_rank"], trace.fused_rank, "{label}");
            assert_eq!(bits(&row["score"]), trace.fusion_score.to_bits(), "{label}");
            by_location.insert(at, trace);
        }
        let cap_rows = cap_cut["frontier"]["rows"].as_array().unwrap();
        assert_eq!(cap_rows.len(), traced.len(), "{label}");
        for (position, row) in cap_rows.iter().enumerate() {
            let trace = by_location[&location(row)];
            assert_eq!(row["capped_rank"], trace.capped_rank, "{label}");
            assert_eq!(row["capped_rank"], position + 1, "{label}");
            assert_eq!(row["shortlisted"], trace.shortlisted, "{label}");
            demoted += usize::from(row["capped_rank"] != row["fused_rank"]);
            cut += usize::from(row["shortlisted"] == false);
        }
        let node_location: std::collections::HashMap<&str, _> = fuse_rows
            .iter()
            .map(|row| (row["node"].as_str().unwrap(), location(row)))
            .collect();
        for call in record.steps[0]["map_calls"].as_array().unwrap() {
            for hit in call["hits"].as_array().unwrap() {
                let trace = by_location[&node_location[hit["node"].as_str().unwrap()]];
                let rank = Some(hit["rank"].as_u64().unwrap() as usize);
                let traced_rank = match call["map"].as_str().unwrap() {
                    "vec.chunk.code" => trace.semantic_rank,
                    "lexical" => trace.lexical_rank,
                    "lsp.symbols" => trace.lsp_rank,
                    other => panic!("unexpected map {other}"),
                };
                assert_eq!(rank, traced_rank, "{label}: {call}");
            }
        }

        // Replay and C5.
        let replay = record.replay_fuse().unwrap();
        assert_eq!(
            (replay.fuse_steps, replay.nodes, replay.rows_checked),
            (1, traced.len(), traced.len()),
            "{label}"
        );
        record.check_labels().unwrap();

        // Scoring with the evaluation metrics.
        let evidence: Vec<ResultEvidence> = report
            .results
            .iter()
            .enumerate()
            .map(|(index, hit)| ResultEvidence {
                rank: index + 1,
                relative_file_path: hit.chunk.relative_file_path.clone(),
                language: hit.chunk.language.clone(),
                source_role: role(hit.source_role),
                start_line: hit.chunk.start_line,
                end_line: hit.chunk.end_line,
                snippet: local_code_intelligence::evaluate::bounded_snippet(&hit.chunk.code),
                reranker_score: None,
            })
            .collect();
        let metrics = record.score(query).unwrap();
        assert_eq!(metrics, query_metrics(query, &evidence, false), "{label}");
        hits += usize::from(metrics.hit);
    }
    assert_eq!(
        hits,
        gold.queries.len(),
        "every fixture query finds its gold file"
    );
    assert!(
        demoted > 0 && cut > 0,
        "the cap reordered ({demoted}) and the cut dropped ({cut}) candidates"
    );

    // The directory reads back, one record per query, each paired with its
    // gold query by text.
    let records = RunRecordFile::read_dir(&dir).unwrap();
    assert_eq!(records.len(), gold.queries.len());
    for (_, record) in &records {
        assert!(record.end.is_some() && !record.torn_tail);
        record.gold_query(&gold).unwrap();
    }
    task.abort();
}
