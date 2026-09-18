use super::*;

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
async fn go_source_is_indexed_and_searchable() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("go-service");
    write(
        &workspace,
        "main.go",
        "package main\n\n// Greet returns a friendly greeting for name.\nfunc Greet(name string) string {\n\treturn \"hello \" + name\n}\n",
    );
    let app = App::open(config).await.unwrap();

    let report = app.index(&workspace).await.unwrap();
    assert_eq!(report.files, 1);
    // "package main" and the doc-commented Greet function are separate
    // top-level chunks (source_file is a container), so 2 not 1.
    assert_eq!(report.chunks, 2);

    let indexed = app.indexed_files(&workspace).await.unwrap();
    assert_eq!(indexed.files.len(), 1);
    assert_eq!(indexed.files[0].language, "go");
    assert_eq!(indexed.files[0].chunks, 2);

    let found = app.search(&workspace, "Greet", Some(1)).await.unwrap();
    assert_eq!(found.results[0].chunk.language, "go");
    assert!(found.results[0].chunk.code.contains("func Greet"));
    task.abort();
}

#[tokio::test]
async fn java_source_is_indexed_and_searchable() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("java-service");
    write(
        &workspace,
        "Greeter.java",
        "package main;\n\n// Greet returns a friendly greeting for name.\npublic class Greeter {\n    public static String greet(String name) {\n        return \"hello \" + name;\n    }\n}\n",
    );
    let app = App::open(config).await.unwrap();

    let report = app.index(&workspace).await.unwrap();
    assert_eq!(report.files, 1);
    // "package main" and the doc-commented class (with its single member
    // attached, per attaches_header_to_child) are separate top-level
    // chunks (program is a container), so 2 not 1.
    assert_eq!(report.chunks, 2);

    let indexed = app.indexed_files(&workspace).await.unwrap();
    assert_eq!(indexed.files.len(), 1);
    assert_eq!(indexed.files[0].language, "java");
    assert_eq!(indexed.files[0].chunks, 2);

    let found = app.search(&workspace, "greet", Some(1)).await.unwrap();
    assert_eq!(found.results[0].chunk.language, "java");
    assert!(found.results[0].chunk.code.contains("static String greet"));
    task.abort();
}

#[tokio::test]
async fn indexed_files_lists_cached_files_with_language_and_chunk_counts() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("listing");
    write(&workspace, "src/lib.rs", "fn one() {}\nfn two() {}\n");
    write(&workspace, "src/app.py", "def main():\n    pass\n");
    let app = App::open(config).await.unwrap();

    let error = app.indexed_files(&workspace).await.unwrap_err();
    assert!(format!("{error:#}").contains("workspace is not indexed"));

    app.index(&workspace).await.unwrap();
    let report = app.indexed_files(&workspace).await.unwrap();
    let paths: Vec<_> = report
        .files
        .iter()
        .map(|f| f.relative_file_path.as_str())
        .collect();
    assert_eq!(paths, ["src/app.py", "src/lib.rs"]);
    let rust_file = report
        .files
        .iter()
        .find(|f| f.relative_file_path == "src/lib.rs")
        .unwrap();
    assert_eq!(rust_file.language, "rust");
    assert_eq!(rust_file.chunks, 2);
    let python_file = report
        .files
        .iter()
        .find(|f| f.relative_file_path == "src/app.py")
        .unwrap();
    assert_eq!(python_file.language, "python");
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
async fn manual_freshness_reuses_stale_snapshot_without_refresh() {
    let (temp, mut config, fake, task) = fixture().await;
    config.index.freshness = IndexFreshness::Manual;
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
async fn on_search_freshness_refreshes_a_stale_snapshot_before_searching() {
    let (temp, mut config, fake, task) = fixture().await;
    config.index.stale_check_interval_seconds = 0;
    let workspace = temp.path().join("stale");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    let first = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap()["index"]["action"],
        "created"
    );
    let document_requests = fake.document_requests.load(Ordering::SeqCst);

    write(&workspace, "lib.rs", "fn replacement() {}\n");
    assert!(app.status(&workspace).await.unwrap().stale);
    let refreshed = app
        .search(&workspace, "replacement", Some(1))
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(&refreshed).unwrap()["index"]["action"],
        "refreshed_incrementally"
    );
    assert!(refreshed.results[0].chunk.code.contains("replacement"));
    assert!(!app.status(&workspace).await.unwrap().stale);
    assert!(fake.document_requests.load(Ordering::SeqCst) > document_requests);
    task.abort();
}

#[tokio::test]
async fn on_search_freshness_reuses_snapshot_within_throttle_interval() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("stale");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    app.search(&workspace, "translator", Some(1)).await.unwrap();
    // Perform one more search while the workspace is unchanged, so the first
    // (always-due) staleness check runs and starts the throttle window.
    app.search(&workspace, "translator", Some(1)).await.unwrap();
    let document_requests = fake.document_requests.load(Ordering::SeqCst);

    // Changed within the default 10s throttle window: the staleness check is
    // skipped, so the previous snapshot is reused rather than refreshed.
    write(&workspace, "lib.rs", "fn replacement() {}\n");
    let reused = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&reused).unwrap()["index"]["action"],
        "reused"
    );
    assert_eq!(
        fake.document_requests.load(Ordering::SeqCst),
        document_requests
    );
    task.abort();
}

#[tokio::test]
async fn on_search_freshness_reuses_snapshot_when_refresh_is_unavailable() {
    let (temp, mut config, fake, task) = fixture().await;
    config.index.stale_check_interval_seconds = 0;
    let workspace = temp.path().join("stale");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    app.search(&workspace, "translator", Some(1)).await.unwrap();
    let document_requests = fake.document_requests.load(Ordering::SeqCst);

    // Keep the original chunk on disk (so lexical search still finds it in
    // the retained snapshot) while adding a new chunk that would need
    // embedding, so the incremental refresh has work to fail on.
    write(&workspace, "lib.rs", "fn translator() {}\nfn extra() {}\n");
    fake.embed_fails.store(true, Ordering::SeqCst);
    let unavailable = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&unavailable).unwrap()["index"]["action"],
        "reused"
    );
    assert!(
        unavailable
            .warning
            .as_ref()
            .unwrap()
            .contains("refresh unavailable")
    );
    assert!(unavailable.results[0].chunk.code.contains("translator"));
    assert_eq!(
        fake.document_requests.load(Ordering::SeqCst),
        document_requests
    );
    fake.embed_fails.store(false, Ordering::SeqCst);
    task.abort();
}

#[tokio::test]
async fn on_search_freshness_waits_for_a_concurrent_refresh_job() {
    let (temp, mut config, fake, task) = fixture().await;
    config.index.stale_check_interval_seconds = 0;
    fake.embed_delay_milliseconds.store(200, Ordering::SeqCst);
    let workspace = temp.path().join("stale");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    app.search(&workspace, "translator", Some(1)).await.unwrap();
    fake.embed_delay_milliseconds.store(0, Ordering::SeqCst);

    write(&workspace, "lib.rs", "fn replacement() {}\n");
    fake.embed_delay_milliseconds.store(200, Ordering::SeqCst);
    let first_app = app.clone();
    let first_workspace = workspace.clone();
    let first = tokio::spawn(async move {
        first_app
            .search(&first_workspace, "replacement", Some(1))
            .await
            .unwrap()
    });
    while fake.document_requests.load(Ordering::SeqCst) == 1 {
        tokio::task::yield_now().await;
    }
    let second_app = app.clone();
    let second_workspace = workspace.clone();
    let second = tokio::spawn(async move {
        second_app
            .search(&second_workspace, "replacement", Some(1))
            .await
            .unwrap()
    });
    let (first, second) = tokio::join!(first, second);
    let actions = [
        serde_json::to_value(first.unwrap()).unwrap()["index"]["action"].clone(),
        serde_json::to_value(second.unwrap()).unwrap()["index"]["action"].clone(),
    ];
    assert!(
        actions.contains(&json!("refreshed_incrementally")),
        "{actions:?}"
    );
    assert!(
        actions.contains(&json!("waited_for_existing_job")),
        "{actions:?}"
    );
    task.abort();
}

#[tokio::test]
async fn persistence_incremental_isolation_deletion_and_fail_open() {
    let (temp, mut config, fake, task) = fixture().await;
    // This test drives explicit index()/status() calls; automatic on-search
    // refresh is covered separately by the freshness-policy tests below.
    config.index.freshness = IndexFreshness::Manual;
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
    write(
        &workspace,
        "src/TelemetryProcessor.cs",
        "namespace Telemetry;\n/// <summary>Production telemetry normalization.</summary>\npublic sealed class TelemetryProcessor { public string NormalizeProductionEvent(string value) => value.Trim().ToLowerInvariant(); }\n",
    );
    write(
        &workspace,
        "tests/TelemetryProcessorTests.cs",
        "namespace Telemetry.Tests;\n// Search decoy for NormalizeProductionEvent.\npublic sealed class TelemetryProcessorTests { public const string Expected = \"trim lowercase\"; }\n",
    );
    write(
        &workspace,
        "src/pipeline.go",
        "package pipeline\n\nimport \"strings\"\n\n// BuildProductionEventPipeline trims and filters production events.\nfunc BuildProductionEventPipeline(events []string) []string {\n\tresult := make([]string, 0, len(events))\n\tfor _, event := range events {\n\t\tif trimmed := strings.TrimSpace(event); trimmed != \"\" {\n\t\t\tresult = append(result, trimmed)\n\t\t}\n\t}\n\treturn result\n}\n",
    );
    write(
        &workspace,
        "tests/pipeline_test.go",
        "package pipeline\n\n// Decoy documentation for a production event pipeline test.\nconst ExpectedPipelineDescription = \"trim and filter events\"\n",
    );
    write(
        &workspace,
        "src/OrderValidator.java",
        "package orders;\n\nimport java.util.List;\n\n/** Validates a production order before it is queued for fulfillment. */\npublic final class OrderValidator {\n    public static boolean isValidProductionOrder(String orderId, List<String> items) {\n        return orderId != null && !orderId.isEmpty() && !items.isEmpty();\n    }\n}\n",
    );
    write(
        &workspace,
        "tests/OrderValidatorTest.java",
        "package orders;\n\n// Decoy documentation for a production order test. isValidProductionOrder is mentioned only here.\npublic final class OrderValidatorTest {\n    public static final String EXPECTED_ORDER_VALIDATION_DESCRIPTION = \"reject orders with no id or no items\";\n}\n",
    );

    let app = App::open(config.clone()).await.unwrap();
    let first = app.index(&workspace).await.unwrap();
    assert_eq!(first.files, 13);
    assert_eq!(first.parsed_files, 13);

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
    let csharp_production = app
        .search(&workspace, "NormalizeProductionEvent", Some(8))
        .await
        .unwrap();
    let csharp_implementation = csharp_production
        .results
        .iter()
        .find(|hit| hit.chunk.relative_file_path == "src/TelemetryProcessor.cs")
        .unwrap();
    assert_eq!(csharp_implementation.chunk.language, "csharp");
    assert_eq!(
        csharp_implementation.source_role,
        local_code_intelligence::filter::SourceRole::Source
    );
    assert!(csharp_implementation.chunk.start_line >= 1);
    assert!(csharp_implementation.chunk.end_line >= csharp_implementation.chunk.start_line);
    assert!(csharp_production.results.iter().any(|hit| {
        hit.chunk.relative_file_path == "tests/TelemetryProcessorTests.cs"
            && hit.source_role == local_code_intelligence::filter::SourceRole::Test
    }));
    for (query, path, language) in [
        ("TelemetryPanel", "src/panel.tsx", "tsx"),
        ("flushAuditBeacon", "src/audit.js", "javascript"),
        ("translator", "src/lib.rs", "rust"),
        ("BuildProductionEventPipeline", "src/pipeline.go", "go"),
        ("isValidProductionOrder", "src/OrderValidator.java", "java"),
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
    assert_eq!(restarted.unchanged_files, 13);

    write(
        &workspace,
        "src/TelemetryProcessor.cs",
        "namespace Telemetry;\n/// <summary>Production telemetry normalization.</summary>\npublic sealed class TelemetryProcessor { public string NormalizeProductionEvent(string value) { var trimmed = value.Trim(); return trimmed.ToLowerInvariant(); } }\n",
    );
    let changed = app.index(&workspace).await.unwrap();
    assert_eq!(changed.parsed_files, 1);
    assert_eq!(changed.unchanged_files, 12);

    write(
        &workspace,
        "src/pipeline.go",
        "package pipeline\n\nimport \"strings\"\n\n// BuildProductionEventPipeline trims and filters production events.\nfunc BuildProductionEventPipeline(events []string) []string {\n\tresult := make([]string, 0, len(events))\n\tfor _, event := range events {\n\t\tif trimmed := strings.TrimSpace(event); trimmed != \"\" {\n\t\t\tresult = append(result, strings.ToLower(trimmed))\n\t\t}\n\t}\n\treturn result\n}\n",
    );
    let go_changed = app.index(&workspace).await.unwrap();
    assert_eq!(go_changed.parsed_files, 1);
    assert_eq!(go_changed.unchanged_files, 12);
    let go_updated = app
        .search(&workspace, "BuildProductionEventPipeline", Some(8))
        .await
        .unwrap();
    assert!(
        go_updated
            .results
            .iter()
            .any(|hit| hit.chunk.relative_file_path == "src/pipeline.go"
                && hit.chunk.code.contains("ToLower"))
    );

    write(
        &workspace,
        "src/OrderValidator.java",
        "package orders;\n\nimport java.util.List;\n\n/** Validates a production order before it is queued for fulfillment. */\npublic final class OrderValidator {\n    public static boolean isValidProductionOrder(String orderId, List<String> items) {\n        String trimmedId = orderId == null ? null : orderId.trim();\n        return trimmedId != null && !trimmedId.isEmpty() && !items.isEmpty();\n    }\n}\n",
    );
    let java_changed = app.index(&workspace).await.unwrap();
    assert_eq!(java_changed.parsed_files, 1);
    assert_eq!(java_changed.unchanged_files, 12);
    let java_updated = app
        .search(&workspace, "isValidProductionOrder", Some(8))
        .await
        .unwrap();
    assert!(
        java_updated
            .results
            .iter()
            .any(
                |hit| hit.chunk.relative_file_path == "src/OrderValidator.java"
                    && hit.chunk.code.contains("trimmedId")
            )
    );

    std::fs::remove_file(workspace.join("tests/TelemetryProcessorTests.cs")).unwrap();
    let csharp_deleted = app.index(&workspace).await.unwrap();
    assert_eq!(csharp_deleted.removed_files, 1);
    let after_csharp_delete = app
        .search(&workspace, "TelemetryProcessorTests", Some(8))
        .await
        .unwrap();
    assert!(
        after_csharp_delete
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path != "tests/TelemetryProcessorTests.cs")
    );

    std::fs::remove_file(workspace.join("tests/feature_pipeline_test.py")).unwrap();
    let deleted = app.index(&workspace).await.unwrap();
    assert_eq!(deleted.removed_files, 1);
    assert_eq!(deleted.files, 11);
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

    std::fs::remove_file(workspace.join("tests/pipeline_test.go")).unwrap();
    let deleted = app.index(&workspace).await.unwrap();
    assert_eq!(deleted.removed_files, 1);
    assert_eq!(deleted.files, 10);
    let after_go_delete = app
        .search(&workspace, "ExpectedPipelineDescription", Some(8))
        .await
        .unwrap();
    assert!(
        after_go_delete
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path != "tests/pipeline_test.go")
    );

    std::fs::remove_file(workspace.join("tests/OrderValidatorTest.java")).unwrap();
    let deleted = app.index(&workspace).await.unwrap();
    assert_eq!(deleted.removed_files, 1);
    assert_eq!(deleted.files, 9);
    let after_java_delete = app
        .search(&workspace, "EXPECTED_ORDER_VALIDATION_DESCRIPTION", Some(8))
        .await
        .unwrap();
    assert!(
        after_java_delete
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path != "tests/OrderValidatorTest.java")
    );

    write(
        &workspace,
        "src/TelemetryProcessor.cs",
        "namespace Telemetry;\npublic sealed class TelemetryProcessor { public string NormalizeProductionEvent(string value) => value.Trim(); public const string UNCOMMITTED_SNAPSHOT_MARKER = \"new source\"; }\n",
    );
    fake.embed_fails.store(true, Ordering::SeqCst);
    assert!(app.index(&workspace).await.is_err());
    assert_eq!(app.status(&workspace).await.unwrap().chunks, deleted.chunks);
    assert!(app.status(&workspace).await.unwrap().stale);
    fake.embed_fails.store(false, Ordering::SeqCst);
    let retained = app
        .search(&workspace, "NormalizeProductionEvent", Some(8))
        .await
        .unwrap();
    let retained_implementation = retained
        .results
        .iter()
        .find(|hit| hit.chunk.relative_file_path == "src/TelemetryProcessor.cs")
        .unwrap();
    assert!(
        retained_implementation
            .chunk
            .code
            .contains("NormalizeProductionEvent")
    );
    assert!(
        !retained_implementation
            .chunk
            .code
            .contains("UNCOMMITTED_SNAPSHOT_MARKER")
    );
    task.abort();
}
