use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use local_code_intelligence::{app::App, config::Config, models::QUERY_INSTRUCTION};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

#[derive(Default)]
struct FakeModels {
    documents: AtomicUsize,
    document_requests: AtomicUsize,
    queries: AtomicUsize,
    embed_delay_milliseconds: AtomicUsize,
    rerank_fails: AtomicBool,
    embed_fails: AtomicBool,
    malformed_rerank: AtomicBool,
}

async fn embed(
    State(state): State<Arc<FakeModels>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if state.embed_fails.load(Ordering::SeqCst) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    let inputs = body["input"].as_array().unwrap();
    if inputs
        .iter()
        .any(|text| !text.as_str().unwrap().starts_with(QUERY_INSTRUCTION))
    {
        state.document_requests.fetch_add(1, Ordering::SeqCst);
        let delay = state.embed_delay_milliseconds.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay as u64)).await;
        }
    }
    let mut data = Vec::new();
    for (index, text) in inputs.iter().enumerate() {
        let text = text.as_str().unwrap();
        if text.starts_with(QUERY_INSTRUCTION) {
            state.queries.fetch_add(1, Ordering::SeqCst);
        } else {
            state.documents.fetch_add(1, Ordering::SeqCst);
            assert!(!text.starts_with("Instruct:"));
        }
        let vector = if text.contains("translator") {
            vec![1.0, 0.0, 0.0]
        } else if text.contains("buildProductionTelemetryPipeline") {
            vec![0.0, 1.0, 0.0]
        } else {
            vec![0.1, 0.1, 1.0]
        };
        data.push(json!({"index":index, "embedding":vector}));
    }
    // OpenAI-compatible services may return embeddings out of order.
    data.reverse();
    Ok(Json(json!({"data":data})))
}

async fn model_endpoint() -> Json<Value> {
    Json(json!({"status":"ok"}))
}

async fn models() -> Json<Value> {
    Json(json!({"data":[
        {"id":"qwen3-embedding-4b"},
        {"id":"qwen3-reranker-4b"}
    ]}))
}

async fn rerank(
    State(state): State<Arc<FakeModels>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    if state.rerank_fails.load(Ordering::SeqCst) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    if state.malformed_rerank.load(Ordering::SeqCst) {
        return Ok(Json(
            json!({"results":[{"index":999,"relevance_score":1.0}]}),
        ));
    }
    let query = body["query"].as_str().unwrap_or("");
    let results: Vec<_> = body["documents"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let document = d.as_str().unwrap();
            let preferred_implementation = (query == "buildProductionTelemetryPipeline"
                && document.contains("src/telemetry.ts\n")
                && document.contains("function buildProductionTelemetryPipeline"))
                || (query == "build_production_feature_pipeline"
                    && document.contains("src/feature_pipeline.py\n")
                    && document.contains("def build_production_feature_pipeline"));
            let score = if preferred_implementation {
                0.995
            } else if document.contains("translator") {
                0.99
            } else {
                0.1
            };
            json!({"index":i,"relevance_score":score})
        })
        .collect();
    Ok(Json(json!({"results":results})))
}

async fn fixture() -> (
    tempfile::TempDir,
    Config,
    Arc<FakeModels>,
    tokio::task::JoinHandle<()>,
) {
    let temp = tempfile::tempdir().unwrap();
    let state = Arc::new(FakeModels::default());
    let router = Router::new()
        .route("/v1", get(model_endpoint))
        .route("/v1/models", get(models))
        .route("/v1/embeddings", post(embed))
        .route("/rerank", get(model_endpoint).post(rerank))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let config = Config {
        data_dir: temp.path().join("data"),
        embedding_url: format!("http://{address}/v1"),
        reranker_url: format!("http://{address}/rerank"),
        rust_analyzer_path: "definitely-missing-rust-analyzer".into(),
        ..Config::default()
    };
    (temp, config, state, task)
}

fn write(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[tokio::test]
async fn search_creates_reuses_and_reopens_an_index_without_duplicate_embeddings() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("automatic");
    write(
        &workspace,
        "lib.rs",
        "fn translator() { println!(\"ready\"); }\n",
    );
    let app = App::open(config.clone()).await.unwrap();

    let first = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap()["index"]["action"],
        "created"
    );
    assert!(first.results[0].chunk.code.contains("translator"));
    let document_requests = fake.document_requests.load(Ordering::SeqCst);
    let documents = fake.documents.load(Ordering::SeqCst);

    let second = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&second).unwrap()["index"]["action"],
        "reused"
    );
    assert_eq!(
        fake.document_requests.load(Ordering::SeqCst),
        document_requests
    );
    assert_eq!(fake.documents.load(Ordering::SeqCst), documents);

    drop(app);
    let reopened = App::open(config).await.unwrap();
    let after_restart = reopened
        .search(&workspace, "translator", Some(1))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&after_restart).unwrap()["index"]["action"],
        "reused"
    );
    assert_eq!(
        fake.document_requests.load(Ordering::SeqCst),
        document_requests
    );
    task.abort();
}

#[tokio::test]
async fn concurrent_first_searches_share_one_initial_index_job() {
    let (temp, config, fake, task) = fixture().await;
    fake.embed_delay_milliseconds.store(200, Ordering::SeqCst);
    let workspace = temp.path().join("concurrent");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());

    let first_app = app.clone();
    let first_workspace = workspace.clone();
    let first = tokio::spawn(async move {
        first_app
            .search(&first_workspace, "translator", Some(1))
            .await
            .unwrap()
    });
    while fake.document_requests.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    let second_app = app.clone();
    let second_workspace = workspace.clone();
    let second = tokio::spawn(async move {
        second_app
            .search(&second_workspace, "translator", Some(1))
            .await
            .unwrap()
    });
    let (first, second) = tokio::join!(first, second);
    let actions = [
        serde_json::to_value(first.unwrap()).unwrap()["index"]["action"].clone(),
        serde_json::to_value(second.unwrap()).unwrap()["index"]["action"].clone(),
    ];
    assert!(actions.contains(&json!("created")), "{actions:?}");
    assert!(
        actions.contains(&json!("waited_for_existing_job")),
        "{actions:?}"
    );
    assert_eq!(fake.document_requests.load(Ordering::SeqCst), 1);
    assert_eq!(fake.documents.load(Ordering::SeqCst), 1);
    task.abort();
}

#[tokio::test]
async fn failed_initial_search_preserves_no_snapshot_and_can_retry() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("retry");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    fake.embed_fails.store(true, Ordering::SeqCst);

    let error = app
        .search(&workspace, "translator", None)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("embedding service HTTP error"));
    assert!(!app.status(&workspace).await.unwrap().indexed);

    fake.embed_fails.store(false, Ordering::SeqCst);
    let retried = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert_eq!(
        serde_json::to_value(retried).unwrap()["index"]["action"],
        "created"
    );
    assert!(app.status(&workspace).await.unwrap().indexed);
    task.abort();
}

#[tokio::test]
async fn stale_snapshot_is_reused_without_automatic_refresh() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("stale");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    app.search(&workspace, "translator", Some(1)).await.unwrap();
    let document_requests = fake.document_requests.load(Ordering::SeqCst);
    write(&workspace, "lib.rs", "fn replacement() {}\n");
    assert!(app.status(&workspace).await.unwrap().stale);

    let stale = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&stale).unwrap()["index"]["action"],
        "reused"
    );
    assert!(stale.results[0].chunk.code.contains("translator"));
    assert_eq!(
        fake.document_requests.load(Ordering::SeqCst),
        document_requests
    );
    task.abort();
}

#[tokio::test]
async fn persistence_incremental_isolation_deletion_and_fail_open() {
    let (temp, config, fake, task) = fixture().await;
    let a = temp.path().join("a");
    let b = temp.path().join("b");
    write(&a, "lib.rs", "fn translator() {}\nfn unrelated() {}\n");
    write(&b, "lib.rs", "fn repository_b() {}\n");
    let app = App::open(config.clone()).await.unwrap();
    assert!(!app.status(&a).await.unwrap().indexed);
    let automatic = app.search(&a, "translator", None).await.unwrap();
    assert_eq!(
        serde_json::to_value(&automatic).unwrap()["index"]["action"],
        "created"
    );
    let first = app.index(&a).await.unwrap();
    assert_eq!(first.chunks, 2);
    assert_eq!(first.embedded_chunks, 0);
    assert_eq!(first.parsed_files, 0);
    assert_eq!(first.unchanged_files, 1);
    app.index(&b).await.unwrap();
    let unchanged = app.index(&a.join(".")).await.unwrap();
    assert_eq!(unchanged.workspace.id, first.workspace.id);
    assert_eq!(unchanged.embedded_chunks, 0);
    assert_eq!(unchanged.reused_chunks, 2);
    assert_eq!(unchanged.parsed_files, 0);
    assert_eq!(unchanged.unchanged_files, 1);
    let ranked = app.search(&a, "translator", Some(1)).await.unwrap();
    assert!(ranked.reranked);
    assert!(ranked.results[0].chunk.code.contains("translator"));
    assert_eq!(ranked.results[0].reranker_score, Some(0.99));
    assert!((ranked.results[0].semantic_score.unwrap() - 1.0).abs() < 0.001);
    assert!(
        ranked
            .results
            .iter()
            .all(|h| !h.chunk.code.contains("repository_b"))
    );
    assert_eq!(fake.queries.load(Ordering::SeqCst), 2);
    fake.embed_fails.store(true, Ordering::SeqCst);
    let direct_lexical = local_code_intelligence::lexical::search("rg", &a, "translator")
        .await
        .unwrap();
    assert!(
        !direct_lexical.is_empty(),
        "ripgrep returned no matches for {}",
        a.display()
    );
    let lexical_only = app.search(&a, "translator", Some(1)).await.unwrap();
    assert!(
        lexical_only.results[0]
            .retrieval_channels
            .contains(&"lexical".to_string())
    );
    assert!(
        lexical_only
            .warning
            .as_deref()
            .unwrap()
            .contains("Query embedding unavailable")
    );
    fake.embed_fails.store(false, Ordering::SeqCst);
    let mut no_rg_config = config.clone();
    no_rg_config.ripgrep_path = "definitely-missing-ripgrep".into();
    let no_rg_app = App::open(no_rg_config).await.unwrap();
    let semantic_only = no_rg_app.search(&a, "translator", Some(1)).await.unwrap();
    assert!(
        semantic_only.results[0]
            .retrieval_channels
            .contains(&"semantic".to_string())
    );
    assert!(
        semantic_only
            .warning
            .as_deref()
            .unwrap()
            .contains("Lexical retrieval unavailable")
    );
    fake.rerank_fails.store(true, Ordering::SeqCst);
    let fallback = app.search(&a, "translator", None).await.unwrap();
    assert!(!fallback.reranked && fallback.warning.is_some());
    assert!(fallback.results.iter().all(|h| h.reranker_score.is_none()));
    fake.rerank_fails.store(false, Ordering::SeqCst);
    fake.malformed_rerank.store(true, Ordering::SeqCst);
    assert!(!app.search(&a, "translator", None).await.unwrap().reranked);
    drop(app);
    let manifest_path = config
        .data_dir
        .join("manifests")
        .join(format!("{}.json", first.workspace.id));
    let mut legacy: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    legacy["version"] = json!(1);
    legacy["chunker_version"] = json!("rust-chunks-v1");
    let source_hash = legacy["files"]["lib.rs"]["content_hash"]
        .as_str()
        .unwrap()
        .to_owned();
    legacy["fingerprint"] = json!(local_code_intelligence::workspace::hash(&format!(
        "lib.rs\0{source_hash}\n"
    )));
    for file in legacy["files"].as_object_mut().unwrap().values_mut() {
        file.as_object_mut().unwrap().remove("language");
        file.as_object_mut().unwrap().remove("adapter_version");
    }
    std::fs::write(&manifest_path, serde_json::to_vec_pretty(&legacy).unwrap()).unwrap();
    let app = App::open(config.clone()).await.unwrap();
    assert_eq!(app.status(&a).await.unwrap().chunks, 2);
    assert!(app.status(&a).await.unwrap().stale);
    let reopened = app.index(&a).await.unwrap();
    assert_eq!(reopened.embedded_chunks, 0);
    assert_eq!(reopened.parsed_files, 0);
    assert_eq!(reopened.unchanged_files, 1);
    assert!(!app.status(&a).await.unwrap().stale);
    write(&a, "lib.rs", "\n\nfn translator() {}\nfn changed() {}\n");
    fake.embed_fails.store(true, Ordering::SeqCst);
    assert!(app.index(&a).await.is_err());
    assert_eq!(app.status(&a).await.unwrap().chunks, 2);
    assert!(app.status(&a).await.unwrap().stale);
    fake.embed_fails.store(false, Ordering::SeqCst);
    assert_eq!(
        app.search(&a, "translator", None).await.unwrap().results[0]
            .chunk
            .start_line,
        1
    );
    let changed = app.index(&a).await.unwrap();
    assert_eq!(changed.embedded_chunks, 1);
    assert_eq!(changed.reused_chunks, 1);
    assert_eq!(changed.parsed_files, 1);
    assert!(!app.status(&a).await.unwrap().stale);
    assert_eq!(
        app.search(&a, "translator", None).await.unwrap().results[0]
            .chunk
            .start_line,
        3
    );
    std::fs::remove_file(a.join("lib.rs")).unwrap();
    let deleted = app.index(&a).await.unwrap();
    assert_eq!(deleted.chunks, 0);
    assert_eq!(deleted.removed_files, 1);
    assert!(
        app.search(&a, "translator", None)
            .await
            .unwrap()
            .results
            .is_empty()
    );
    assert_eq!(app.status(&b).await.unwrap().chunks, 1);
    assert!(app.index(temp.path()).await.is_err());
    assert!(app.search(&b, " ", None).await.is_err());
    assert!(app.search(&b, "translator", Some(41)).await.is_err());
    drop(app);
    let mut changed_config = config;
    changed_config.embedding_model = "another-model".into();
    let app = App::open(changed_config).await.unwrap();
    assert!(!app.status(&b).await.unwrap().compatible);
    assert!(app.search(&b, "translator", None).await.is_err());
    assert_eq!(app.index(&b).await.unwrap().embedded_chunks, 1);
    task.abort();
}

#[tokio::test]
async fn mixed_languages_reuse_update_delete_and_preserve_the_previous_snapshot() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("mixed");
    write(
        &workspace,
        "src/lib.rs",
        "fn translator() { println!(\"rust anchor\"); }\n",
    );
    write(
        &workspace,
        "src/telemetry.ts",
        "// Production ingestion implementation.\nexport function buildProductionTelemetryPipeline(events: string[]): string[] {\n  return events.map(event => event.trim()).filter(Boolean);\n}\n",
    );
    write(
        &workspace,
        "src/panel.tsx",
        "export const TelemetryPanel = () => <section>Live telemetry</section>;\n",
    );
    write(
        &workspace,
        "src/audit.js",
        "export function flushAuditBeacon() { return 'sent'; }\n",
    );
    write(
        &workspace,
        "tests/telemetry.test.ts",
        "// Decoy documentation for a production telemetry pipeline test.\nexport const expectedPipelineDescription = 'trim and filter events';\n",
    );
    write(
        &workspace,
        "src/feature_pipeline.py",
        "# Production feature pipeline implementation.\ndef build_production_feature_pipeline(events):\n    return [event.strip() for event in events if event.strip()]\n",
    );
    write(
        &workspace,
        "tests/feature_pipeline_test.py",
        "# Decoy documentation for a production feature pipeline test.\nEXPECTED_PIPELINE_DESCRIPTION = 'strip and filter events'\n",
    );

    let app = App::open(config.clone()).await.unwrap();
    let first = app.index(&workspace).await.unwrap();
    assert_eq!(first.files, 7);
    assert_eq!(first.parsed_files, 7);

    let production = app
        .search(&workspace, "buildProductionTelemetryPipeline", Some(8))
        .await
        .unwrap();
    assert_eq!(
        production.results[0].chunk.relative_file_path,
        "src/telemetry.ts"
    );
    assert_eq!(production.results[0].chunk.language, "typescript");
    assert!(
        production.results[0]
            .chunk
            .code
            .contains("function buildProductionTelemetryPipeline")
    );
    let python_production = app
        .search(&workspace, "build_production_feature_pipeline", Some(8))
        .await
        .unwrap();
    assert_eq!(
        python_production.results[0].chunk.relative_file_path,
        "src/feature_pipeline.py"
    );
    assert_eq!(python_production.results[0].chunk.language, "python");
    assert!(
        python_production.results[0]
            .chunk
            .code
            .contains("def build_production_feature_pipeline")
    );
    assert!(python_production.results[0].chunk.start_line >= 1);
    assert!(
        python_production.results[0].chunk.end_line
            >= python_production.results[0].chunk.start_line
    );
    for (query, path, language) in [
        ("TelemetryPanel", "src/panel.tsx", "tsx"),
        ("flushAuditBeacon", "src/audit.js", "javascript"),
        ("translator", "src/lib.rs", "rust"),
    ] {
        let report = app.search(&workspace, query, Some(8)).await.unwrap();
        assert!(
            report.results.iter().any(|hit| {
                hit.chunk.relative_file_path == path && hit.chunk.language == language
            }),
            "missing {language} result for {query}: {:#?}",
            report.results
        );
    }

    let unchanged = app.index(&workspace).await.unwrap();
    assert_eq!(unchanged.parsed_files, 0);
    assert_eq!(unchanged.embedded_chunks, 0);
    drop(app);

    let app = App::open(config).await.unwrap();
    let restarted = app.index(&workspace).await.unwrap();
    assert_eq!(restarted.parsed_files, 0);
    assert_eq!(restarted.embedded_chunks, 0);
    assert_eq!(restarted.unchanged_files, 7);

    write(
        &workspace,
        "src/feature_pipeline.py",
        "# Production feature pipeline implementation.\ndef build_production_feature_pipeline(events):\n    cleaned = [event.strip() for event in events]\n    return [event for event in cleaned if event]\n",
    );
    let changed = app.index(&workspace).await.unwrap();
    assert_eq!(changed.parsed_files, 1);
    assert_eq!(changed.unchanged_files, 6);

    std::fs::remove_file(workspace.join("tests/feature_pipeline_test.py")).unwrap();
    let deleted = app.index(&workspace).await.unwrap();
    assert_eq!(deleted.removed_files, 1);
    assert_eq!(deleted.files, 6);
    let after_delete = app
        .search(&workspace, "EXPECTED_PIPELINE_DESCRIPTION", Some(8))
        .await
        .unwrap();
    assert!(
        after_delete
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path != "tests/feature_pipeline_test.py")
    );

    write(
        &workspace,
        "src/feature_pipeline.py",
        "# Production feature pipeline implementation.\ndef build_production_feature_pipeline(events):\n    cleaned = [event.strip() for event in events]\n    return [event for event in cleaned if event]\nUNCOMMITTED_SNAPSHOT_MARKER = 'new source'\n",
    );
    fake.embed_fails.store(true, Ordering::SeqCst);
    assert!(app.index(&workspace).await.is_err());
    assert_eq!(app.status(&workspace).await.unwrap().chunks, deleted.chunks);
    assert!(app.status(&workspace).await.unwrap().stale);
    fake.embed_fails.store(false, Ordering::SeqCst);
    let retained = app
        .search(&workspace, "build_production_feature_pipeline", Some(8))
        .await
        .unwrap();
    let retained_implementation = retained
        .results
        .iter()
        .find(|hit| hit.chunk.relative_file_path == "src/feature_pipeline.py")
        .unwrap();
    assert!(
        retained_implementation
            .chunk
            .code
            .contains("def build_production_feature_pipeline")
    );
    assert!(
        !retained_implementation
            .chunk
            .code
            .contains("UNCOMMITTED_SNAPSHOT_MARKER")
    );
    task.abort();
}

#[tokio::test]
async fn typescript_javascript_only_search_skips_rust_analyzer() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("web-only");
    write(
        &workspace,
        "src/telemetry.ts",
        "export function buildProductionTelemetryPipeline(events: string[]) { return events; }\n",
    );
    write(
        &workspace,
        "src/runtime.js",
        "export const runtimeReady = () => true;\n",
    );
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app
        .search(&workspace, "buildProductionTelemetryPipeline", Some(4))
        .await
        .unwrap();
    assert_eq!(report.results[0].chunk.language, "typescript");
    assert!(
        report
            .warning
            .as_deref()
            .is_none_or(|warning| !warning.contains("LSP")),
        "unexpected warning: {:?}",
        report.warning
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    fake.embed_fails.store(true, Ordering::SeqCst);
    let lexical_only = app
        .search(&workspace, "runtimeReady", Some(4))
        .await
        .unwrap();
    assert!(lexical_only.results.iter().any(|hit| {
        hit.chunk.relative_file_path == "src/runtime.js"
            && hit.chunk.language == "javascript"
            && hit.retrieval_channels.contains(&"lexical".to_string())
    }));
    let warning = lexical_only.warning.as_deref().unwrap();
    assert!(warning.contains("Query embedding unavailable"));
    assert!(!warning.contains("LSP"));
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    let error = app
        .definition(&workspace, "src/telemetry.ts", 1, 0)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("does not support typescript source files")
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);
    task.abort();
}

#[tokio::test]
async fn python_only_search_and_navigation_skip_rust_analyzer() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("python-only");
    write(
        &workspace,
        "src/revenue.py",
        "# Quarterly revenue summary implementation.\ndef compute_quarterly_revenue_summary(rows):\n    return [row for row in rows if row.get(\"amount\")]\n",
    );
    let app = App::open(config).await.unwrap();
    let indexed = app.index(&workspace).await.unwrap();
    assert_eq!(indexed.files, 1);
    assert_eq!(indexed.parsed_files, 1);
    assert_eq!(indexed.embedded_chunks, 1);

    let report = app
        .search(&workspace, "compute_quarterly_revenue_summary", Some(4))
        .await
        .unwrap();
    assert_eq!(report.results[0].chunk.language, "python");
    assert_eq!(report.results[0].chunk.relative_file_path, "src/revenue.py");
    assert!(
        report
            .warning
            .as_deref()
            .is_none_or(|warning| !warning.contains("LSP")),
        "unexpected warning: {:?}",
        report.warning
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    let definition_error = app
        .definition(&workspace, "src/revenue.py", 1, 0)
        .await
        .unwrap_err();
    assert!(
        definition_error
            .to_string()
            .contains("does not support python source files"),
        "unexpected error: {definition_error}"
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    let references_error = app
        .references(&workspace, "src/revenue.py", 1, 0, true)
        .await
        .unwrap_err();
    assert!(
        references_error
            .to_string()
            .contains("does not support python source files"),
        "unexpected error: {references_error}"
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);
    task.abort();
}

#[tokio::test]
async fn watcher_debounces_and_refreshes_changed_typescript_source() {
    let (temp, mut config, _fake, task) = fixture().await;
    config.watch_poll_milliseconds = 50;
    config.watch_debounce_milliseconds = 50;
    let workspace = temp.path().join("watched");
    write(&workspace, "lib.rs", "fn rust_anchor() {}\n");
    write(
        &workspace,
        "src/runtime.ts",
        "export function beforeTelemetry() {}\n",
    );
    let app = Arc::new(App::open(config).await.unwrap());
    app.index(&workspace).await.unwrap();
    let started = app.watch(&workspace).await.unwrap();
    assert!(started.watched && !started.already_in_requested_state);
    assert!(app.status(&workspace).await.unwrap().watched);
    write(
        &workspace,
        "src/runtime.ts",
        "export function afterTelemetry() {}\n",
    );
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let report = app
            .search(&workspace, "afterTelemetry", Some(1))
            .await
            .unwrap();
        if report.results.iter().any(|hit| {
            hit.chunk.relative_file_path == "src/runtime.ts"
                && hit.chunk.language == "typescript"
                && hit.chunk.code.contains("afterTelemetry")
        }) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "watcher did not refresh the index"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!app.status(&workspace).await.unwrap().stale);
    let stopped = app.unwatch(&workspace).await.unwrap();
    assert!(!stopped.watched && !stopped.already_in_requested_state);
    assert!(!app.status(&workspace).await.unwrap().watched);
    task.abort();
}

#[tokio::test]
async fn http_mcp_initialization_tools_and_health() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("mcp-repository");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    let token = tokio_util::sync::CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = local_code_intelligence::server::router(app, token.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{base}/health"))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["status"],
        "ok"
    );
    let response = client.post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}}}))
        .send().await.unwrap();
    assert!(response.status().is_success());
    let session = response
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let body = response.text().await.unwrap();
    assert!(body.contains("serverInfo"));
    let initialized = client
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session)
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await
        .unwrap();
    assert!(initialized.status().is_success());
    let tools = client
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    for name in [
        "index_workspace",
        "index_status",
        "search_code",
        "watch_workspace",
        "unwatch_workspace",
        "search_symbols",
        "find_definition",
        "find_references",
        "service_status",
    ] {
        assert!(tools.contains(name), "{tools}");
    }
    for (id, name, arguments, expected) in [
        (
            3,
            "index_workspace",
            json!({"workspace_path":workspace}),
            "embedded_chunks",
        ),
        (
            4,
            "index_status",
            json!({"workspace_path":workspace}),
            "embedding_dimension",
        ),
        (
            5,
            "search_code",
            json!({"workspace_path":workspace,"query":"translator"}),
            "reranker_score",
        ),
    ] {
        let response = client.post(format!("{base}/mcp"))
            .header("Accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session)
            .json(&json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}}))
            .send().await.unwrap();
        assert!(response.status().is_success());
        let body = response.text().await.unwrap();
        assert!(body.contains(expected), "{name}: {body}");
        assert!(!body.contains("\"isError\":true"), "{body}");
    }
    let rejected = client
        .post(format!("{base}/mcp"))
        .header("Origin", "https://untrusted.example")
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}))
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);
    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn health_is_liveness_and_ready_reports_required_and_optional_failures() {
    let temp = tempfile::tempdir().unwrap();
    let unavailable = Config {
        data_dir: temp.path().join("unavailable-data"),
        embedding_url: "http://127.0.0.1:1/v1".into(),
        reranker_url: "http://127.0.0.1:1/rerank".into(),
        ripgrep_path: "definitely-missing-ripgrep".into(),
        rust_analyzer_path: "definitely-missing-rust-analyzer".into(),
        readiness_timeout_seconds: 1,
        ..Config::default()
    };
    let app = Arc::new(App::open(unavailable).await.unwrap());
    let token = tokio_util::sync::CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = local_code_intelligence::server::router(app, token.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = reqwest::Client::new();
    assert_eq!(
        client
            .get(format!("{base}/health"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let failed_ready = client.get(format!("{base}/ready")).send().await.unwrap();
    assert_eq!(failed_ready.status(), StatusCode::SERVICE_UNAVAILABLE);
    let failed: Value = failed_ready.json().await.unwrap();
    assert_eq!(failed["ready"], false);
    assert!(
        failed["degraded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "embedding_endpoint_reachable")
    );
    token.cancel();
    server.abort();

    let (temp, mut config, _fake, models_task) = fixture().await;
    config.reranker_url = "http://127.0.0.1:1/rerank".into();
    config.ripgrep_path = "definitely-missing-ripgrep".into();
    config.rust_analyzer_path = "definitely-missing-rust-analyzer".into();
    config.readiness_timeout_seconds = 1;
    let app = Arc::new(App::open(config).await.unwrap());
    let direct = serde_json::to_value(app.service_status().await).unwrap();
    assert_eq!(direct["ready"], true);
    assert_eq!(direct["can_create_semantic_index"], true);
    for name in [
        "reranker_endpoint_reachable",
        "ripgrep_available",
        "rust_analyzer_available",
    ] {
        assert!(
            direct["degraded"]
                .as_array()
                .unwrap()
                .iter()
                .any(|entry| entry["name"] == name)
        );
    }

    let token = tokio_util::sync::CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = local_code_intelligence::server::router(app, token.clone());
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let ready = client.get(format!("{base}/ready")).send().await.unwrap();
    assert_eq!(ready.status(), StatusCode::OK);
    let ready_json: Value = ready.json().await.unwrap();
    assert_eq!(ready_json["ready"], direct["ready"]);
    assert_eq!(
        ready_json["can_create_semantic_index"],
        direct["can_create_semantic_index"]
    );
    assert_eq!(ready_json["components"], direct["components"]);

    let initialize = client.post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"readiness-test","version":"1"}}}))
        .send().await.unwrap();
    let session = initialize
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    client
        .post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session)
        .json(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
        .send()
        .await
        .unwrap();
    let mcp = client.post(format!("{base}/mcp"))
        .header("Accept", "application/json, text/event-stream")
        .header("mcp-session-id", &session)
        .json(&json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"service_status","arguments":{}}}))
        .send().await.unwrap().text().await.unwrap();
    assert!(mcp.contains("\"ready\":true"), "{mcp}");
    assert!(mcp.contains("\"can_create_semantic_index\":true"), "{mcp}");
    assert!(!mcp.contains("8765"), "{mcp}");
    assert!(!serde_json::to_string(&ready_json).unwrap().contains("8765"));
    token.cancel();
    server.abort();
    models_task.abort();
    drop(temp);
}
