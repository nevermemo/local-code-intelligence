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
async fn c_source_is_indexed_and_searchable() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("c-service");
    write(
        &workspace,
        "greet.c",
        "// Greet returns a friendly greeting for name.\nconst char *greet(const char *name) {\n    return name;\n}\n",
    );
    let app = App::open(config).await.unwrap();

    let report = app.index(&workspace).await.unwrap();
    assert_eq!(report.files, 1);
    // A single top-level function (with its leading comment attached) is
    // the only child of translation_unit, so it collapses into one chunk --
    // unlike Go/Java, C has no mandatory top-level statement that would
    // force a second chunk.
    assert_eq!(report.chunks, 1);

    let indexed = app.indexed_files(&workspace).await.unwrap();
    assert_eq!(indexed.files.len(), 1);
    assert_eq!(indexed.files[0].language, "c");
    assert_eq!(indexed.files[0].chunks, 1);

    let found = app.search(&workspace, "greet", Some(1)).await.unwrap();
    assert_eq!(found.results[0].chunk.language, "c");
    assert!(found.results[0].chunk.code.contains("const char *greet"));
    task.abort();
}

#[tokio::test]
async fn cpp_source_is_indexed_and_searchable() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("cpp-service");
    write(
        &workspace,
        "greet.cpp",
        "// Greet returns a friendly greeting for name.\nstd::string greet(const std::string &name) {\n    return name;\n}\n",
    );
    let app = App::open(config).await.unwrap();

    let report = app.index(&workspace).await.unwrap();
    assert_eq!(report.files, 1);
    // Same shape as the C smoke test: a single top-level function (with its
    // leading comment attached) is the only child of translation_unit, so
    // it collapses into one chunk.
    assert_eq!(report.chunks, 1);

    let indexed = app.indexed_files(&workspace).await.unwrap();
    assert_eq!(indexed.files.len(), 1);
    assert_eq!(indexed.files[0].language, "cpp");
    assert_eq!(indexed.files[0].chunks, 1);

    let found = app.search(&workspace, "greet", Some(1)).await.unwrap();
    assert_eq!(found.results[0].chunk.language, "cpp");
    assert!(found.results[0].chunk.code.contains("std::string greet"));
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
        !direct_lexical.matches.is_empty(),
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
    write(
        &workspace,
        "src/cache.c",
        "// Evicts the least-recently-used entry once the cache exceeds its capacity.\nint evict_least_recently_used(int *usage, int count, int capacity) {\n    if (count <= capacity) {\n        return -1;\n    }\n    int oldest_index = 0;\n    for (int i = 1; i < count; i++) {\n        if (usage[i] < usage[oldest_index]) {\n            oldest_index = i;\n        }\n    }\n    return oldest_index;\n}\n",
    );
    write(
        &workspace,
        "tests/cache_test.c",
        "// Decoy documentation for a cache eviction test. evict_least_recently_used is mentioned only here.\nconst char *expected_cache_eviction_description = \"evict least recently used entry\";\n",
    );
    write(
        &workspace,
        "src/RateLimiter.cpp",
        "namespace throttling {\n\n// Determines whether a new request should be admitted given recent request timestamps.\nclass RateLimiter {\npublic:\n    bool should_admit_production_request(int recent_count, int max_per_window) {\n        return recent_count < max_per_window;\n    }\n};\n\n}\n",
    );
    write(
        &workspace,
        "tests/RateLimiterTest.cpp",
        "namespace throttling {\n\n// Decoy documentation for a rate limiter test. should_admit_production_request is mentioned only here.\nconst char *expected_rate_limiter_description = \"admit requests under the configured limit\";\n\n}\n",
    );

    let app = App::open(config.clone()).await.unwrap();
    let first = app.index(&workspace).await.unwrap();
    assert_eq!(first.files, 17);
    assert_eq!(first.parsed_files, 17);

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
        ("evict_least_recently_used", "src/cache.c", "c"),
        (
            "should_admit_production_request",
            "src/RateLimiter.cpp",
            "cpp",
        ),
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
    assert_eq!(restarted.unchanged_files, 17);

    write(
        &workspace,
        "src/TelemetryProcessor.cs",
        "namespace Telemetry;\n/// <summary>Production telemetry normalization.</summary>\npublic sealed class TelemetryProcessor { public string NormalizeProductionEvent(string value) { var trimmed = value.Trim(); return trimmed.ToLowerInvariant(); } }\n",
    );
    let changed = app.index(&workspace).await.unwrap();
    assert_eq!(changed.parsed_files, 1);
    assert_eq!(changed.unchanged_files, 16);

    write(
        &workspace,
        "src/pipeline.go",
        "package pipeline\n\nimport \"strings\"\n\n// BuildProductionEventPipeline trims and filters production events.\nfunc BuildProductionEventPipeline(events []string) []string {\n\tresult := make([]string, 0, len(events))\n\tfor _, event := range events {\n\t\tif trimmed := strings.TrimSpace(event); trimmed != \"\" {\n\t\t\tresult = append(result, strings.ToLower(trimmed))\n\t\t}\n\t}\n\treturn result\n}\n",
    );
    let go_changed = app.index(&workspace).await.unwrap();
    assert_eq!(go_changed.parsed_files, 1);
    assert_eq!(go_changed.unchanged_files, 16);
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
    assert_eq!(java_changed.unchanged_files, 16);
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

    write(
        &workspace,
        "src/cache.c",
        "// Evicts the least-recently-used entry once the cache exceeds its capacity.\nint evict_least_recently_used(int *usage, int count, int capacity) {\n    if (count <= capacity) {\n        return -1;\n    }\n    int oldest_index = 0;\n    for (int i = 1; i < count; i++) {\n        if (usage[i] <= usage[oldest_index]) {\n            oldest_index = i;\n        }\n    }\n    return oldest_index;\n}\n",
    );
    let c_changed = app.index(&workspace).await.unwrap();
    assert_eq!(c_changed.parsed_files, 1);
    assert_eq!(c_changed.unchanged_files, 16);
    let c_updated = app
        .search(&workspace, "evict_least_recently_used", Some(8))
        .await
        .unwrap();
    assert!(
        c_updated
            .results
            .iter()
            .any(|hit| hit.chunk.relative_file_path == "src/cache.c"
                && hit.chunk.code.contains("usage[i] <= usage[oldest_index]"))
    );

    write(
        &workspace,
        "src/RateLimiter.cpp",
        "namespace throttling {\n\n// Determines whether a new request should be admitted given recent request timestamps.\nclass RateLimiter {\npublic:\n    bool should_admit_production_request(int recent_count, int max_per_window) {\n        return recent_count < max_per_window && recent_count >= 0;\n    }\n};\n\n}\n",
    );
    let cpp_changed = app.index(&workspace).await.unwrap();
    assert_eq!(cpp_changed.parsed_files, 1);
    assert_eq!(cpp_changed.unchanged_files, 16);
    let cpp_updated = app
        .search(&workspace, "should_admit_production_request", Some(8))
        .await
        .unwrap();
    assert!(
        cpp_updated
            .results
            .iter()
            .any(|hit| hit.chunk.relative_file_path == "src/RateLimiter.cpp"
                && hit.chunk.code.contains("recent_count >= 0"))
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
    assert_eq!(deleted.files, 15);
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
    assert_eq!(deleted.files, 14);
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
    assert_eq!(deleted.files, 13);
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

    std::fs::remove_file(workspace.join("tests/cache_test.c")).unwrap();
    let deleted = app.index(&workspace).await.unwrap();
    assert_eq!(deleted.removed_files, 1);
    assert_eq!(deleted.files, 12);
    let after_c_delete = app
        .search(&workspace, "expected_cache_eviction_description", Some(8))
        .await
        .unwrap();
    assert!(
        after_c_delete
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path != "tests/cache_test.c")
    );

    std::fs::remove_file(workspace.join("tests/RateLimiterTest.cpp")).unwrap();
    let deleted = app.index(&workspace).await.unwrap();
    assert_eq!(deleted.removed_files, 1);
    assert_eq!(deleted.files, 11);
    let after_cpp_delete = app
        .search(&workspace, "expected_rate_limiter_description", Some(8))
        .await
        .unwrap();
    assert!(
        after_cpp_delete
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path != "tests/RateLimiterTest.cpp")
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

#[tokio::test]
async fn non_utf8_source_is_indexed_lossily_and_never_breaks_search_or_status() {
    let (temp, mut config, _fake, task) = fixture().await;
    config.index.stale_check_interval_seconds = 0;
    let workspace = temp.path().join("legacy-encoding");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    app.search(&workspace, "translator", Some(1)).await.unwrap();

    // A Windows-1252 source file appears after the index exists: the
    // on-search staleness check and the refresh must both handle it.
    std::fs::write(
        workspace.join("legacy.c"),
        b"/* caf\xe9 */\nint legacy_translator(void) { return 7; }\n",
    )
    .unwrap();
    let refreshed = app.search(&workspace, "translator", Some(8)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&refreshed).unwrap()["index"]["action"],
        "refreshed_incrementally"
    );
    assert!(
        refreshed
            .results
            .iter()
            .any(|hit| hit.chunk.relative_file_path == "legacy.c"),
        "the legacy file is searchable"
    );
    let status = app.status(&workspace).await.unwrap();
    assert!(!status.stale && status.stale_check_error.is_none());

    // Every pass reads (and so decodes) every file, so the report always
    // names the lossily decoded files in the current index.
    let reindexed = app.index(&workspace).await.unwrap();
    assert_eq!(reindexed.lossy_decoded_files, vec!["legacy.c".to_owned()]);
    assert_eq!(
        reindexed.parsed_files, 0,
        "the unchanged file is not reparsed"
    );
    task.abort();
}

#[tokio::test]
async fn an_oversized_chunk_is_embedded_from_a_prefix_instead_of_failing_the_index() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("oversized");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    // One indivisible ~9 KB leaf the chunker keeps whole.
    let literal = "x".repeat(9_000);
    write(
        &workspace,
        "table.rs",
        &format!("pub static TABLE: &str = \"{literal}\";\n"),
    );
    fake.max_embed_input_bytes.store(4_000, Ordering::SeqCst);
    let app = App::open(config).await.unwrap();

    let report = app.index(&workspace).await.unwrap();
    assert_eq!(
        report.truncated_for_embedding,
        vec!["table.rs:1".to_owned()]
    );
    let found = app.search(&workspace, "TABLE", Some(8)).await.unwrap();
    // The chunker splits the declaration's header from its indivisible
    // literal; the literal's chunk is stored and returned in full.
    assert!(
        found
            .results
            .iter()
            .any(|hit| hit.chunk.relative_file_path == "table.rs"
                && hit.chunk.code.contains(&literal)),
        "the oversized chunk is still indexed and returned with its full text"
    );

    // A rejection that has nothing to do with length still fails loudly.
    fake.max_embed_input_bytes.store(1, Ordering::SeqCst);
    write(&workspace, "other.rs", "fn other() {}\n");
    let error = app.index(&workspace).await.unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("embedding service HTTP error"),
        "{message}"
    );
    assert!(message.contains("maximum context length"), "{message}");
    task.abort();
}

#[tokio::test]
async fn a_hung_reranker_is_tried_once_and_keeps_earlier_warnings() {
    let (temp, mut config, fake, task) = fixture().await;
    config.reranker_timeout_seconds = 1;
    let workspace = temp.path().join("hung-reranker");
    // Rust source makes the (deliberately missing) rust-analyzer channel
    // applicable, so the search carries an LSP warning before reranking.
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();
    fake.rerank_hangs.store(true, Ordering::SeqCst);
    let before = fake.rerank_requests.load(Ordering::SeqCst);

    let started = std::time::Instant::now();
    // Three query terms -> three `term#` symbol queries for the provider.
    let report = app
        .search(&workspace, "translator helper function", Some(1))
        .await
        .unwrap();
    assert!(!report.reranked);
    assert_eq!(
        fake.rerank_requests.load(Ordering::SeqCst) - before,
        1,
        "query-time reranking does not retry a timeout"
    );
    assert!(started.elapsed() < Duration::from_secs(10));
    let warning = report.warning.unwrap();
    assert!(
        warning.contains("Optional LSP provider unavailable"),
        "{warning}"
    );
    assert!(
        warning.contains("fusion-ranked results returned"),
        "{warning}"
    );
    // Failures are listed as "unavailable: <provider>: ..." then
    // "; <provider>: ..." for each further one.
    let rust_analyzer_failures = warning.matches("unavailable: rust-analyzer: ").count()
        + warning.matches("; rust-analyzer: ").count();
    assert_eq!(
        rust_analyzer_failures, 1,
        "a failed provider is reported once, not once per symbol query: {warning}"
    );
    task.abort();
}

#[tokio::test]
async fn an_unreachable_reranker_fails_open_fast_and_is_then_skipped_briefly() {
    let (temp, mut config, _fake, task) = fixture().await;
    // A port nothing listens on: bind to learn a free one, then release it.
    let closed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let closed_address = closed.local_addr().unwrap();
    drop(closed);
    config.reranker_url = format!("http://{closed_address}/rerank");
    let workspace = temp.path().join("unreachable-reranker");
    write(
        &workspace,
        "lib.rs",
        "fn translator() {}\nfn unrelated() {}\n",
    );
    let app = App::open(config).await.unwrap();
    // Index first so the timed searches measure retrieval, not indexing.
    app.index(&workspace).await.unwrap();

    // The first search really tries the reranker: one refused connection,
    // no backoff retries (they used to add about 9 s).
    // A refused localhost connect alone takes ~2 s on Windows; the old
    // retried path took ~9 s, so 5 s separates them without flaking.
    let first = app.search(&workspace, "translator", Some(2)).await.unwrap();
    assert!(
        first.timings.reranking_ms < 5000.0,
        "an unreachable reranker must not cost a search seconds of retries: {} ms",
        first.timings.reranking_ms
    );
    assert!(!first.reranked);
    assert!(!first.results.is_empty());
    assert!(first.results.iter().all(|hit| hit.reranker_score.is_none()));
    assert!(first.results.iter().all(|hit| hit.fusion_score > 0.0));
    let warning = first.warning.as_deref().unwrap();
    assert!(
        warning.contains("fusion-ranked results returned"),
        "{warning}"
    );
    assert!(!warning.contains("recently unreachable"), "{warning}");

    // A second search inside the window does not touch the reranker at all
    // and says so in the same warning; the ranking is the same fusion order.
    let second = app.search(&workspace, "translator", Some(2)).await.unwrap();
    assert!(!second.reranked);
    assert!(
        second.timings.reranking_ms < 250.0,
        "a skipped reranker costs nothing: {} ms",
        second.timings.reranking_ms
    );
    assert!(
        second
            .results
            .iter()
            .all(|hit| hit.reranker_score.is_none())
    );
    let warning = second.warning.as_deref().unwrap();
    assert!(
        warning.contains("fusion-ranked results returned"),
        "{warning}"
    );
    assert!(warning.contains("recently unreachable"), "{warning}");
    let order = |report: &local_code_intelligence::app::SearchReport| -> Vec<(String, u32)> {
        report
            .results
            .iter()
            .map(|hit| (hit.chunk.relative_file_path.clone(), hit.chunk.start_line))
            .collect()
    };
    assert_eq!(order(&first), order(&second));
    task.abort();
}

#[tokio::test]
async fn a_reranker_error_status_is_retried_by_the_next_search() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("failing-reranker");
    write(
        &workspace,
        "lib.rs",
        "fn translator() {}\nfn unrelated() {}\n",
    );
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();
    let before = fake.rerank_requests.load(Ordering::SeqCst);

    // An HTTP 5xx means the reranker is reachable, so it must not start the
    // unreachable-skip window: the very next search asks it again.
    fake.rerank_fails.store(true, Ordering::SeqCst);
    let failed = app.search(&workspace, "translator", Some(2)).await.unwrap();
    assert!(!failed.reranked);
    let warning = failed.warning.as_deref().unwrap();
    assert!(
        warning.contains("fusion-ranked results returned"),
        "{warning}"
    );
    assert!(!warning.contains("recently unreachable"), "{warning}");
    assert_eq!(fake.rerank_requests.load(Ordering::SeqCst) - before, 1);

    fake.rerank_fails.store(false, Ordering::SeqCst);
    let recovered = app.search(&workspace, "translator", Some(2)).await.unwrap();
    assert!(recovered.reranked, "{:?}", recovered.warning);
    assert_eq!(fake.rerank_requests.load(Ordering::SeqCst) - before, 2);
    assert!(
        recovered
            .results
            .iter()
            .all(|hit| hit.reranker_score.is_some())
    );
    task.abort();
}

#[tokio::test]
async fn reranker_documents_are_capped_while_results_keep_full_chunks() {
    let (temp, mut config, fake, task) = fixture().await;
    config.rerank_document_max_bytes = 1024;
    let workspace = temp.path().join("rerank-cap");
    let literal = "y".repeat(9_000);
    write(
        &workspace,
        "table.rs",
        &format!("pub static TABLE: &str = \"{literal}\";\n"),
    );
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();

    let report = app.search(&workspace, "TABLE", Some(8)).await.unwrap();
    assert!(report.reranked, "{:?}", report.warning);
    let longest = fake.max_rerank_document_bytes.load(Ordering::SeqCst);
    assert!(
        longest <= "table.rs\n".len() + 1024,
        "a reranker document carried {longest} bytes"
    );
    assert!(
        report
            .results
            .iter()
            .any(|hit| hit.chunk.code.contains(&literal)),
        "results still carry the full chunk"
    );
    task.abort();
}

#[tokio::test]
async fn index_commits_wait_for_another_process_holding_the_data_dir_lock() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("shared-data-dir");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());

    // Another process sharing the data directory is mid-commit; a separate
    // handle conflicts with the app's own, as a second process's would.
    let other = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(app.config.data_dir.join("index.lock"))
        .unwrap();
    other.lock().unwrap();
    let indexing = tokio::spawn({
        let app = Arc::clone(&app);
        let workspace = workspace.clone();
        async move { app.index(&workspace).await }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !indexing.is_finished(),
        "the commit waits for the other writer"
    );

    other.unlock().unwrap();
    let report = tokio::time::timeout(Duration::from_secs(30), indexing)
        .await
        .expect("the commit proceeds once the other writer is done")
        .unwrap()
        .unwrap();
    assert_eq!(report.files, 1);
    // Readers never take the lock.
    other.lock().unwrap();
    assert!(app.status(&workspace).await.unwrap().indexed);
    other.unlock().unwrap();
    task.abort();
}

// ---------------------------------------------------------------------------
// `[channels]` switches: each one turns off exactly one retrieval channel (or
// the reranker) for a search, with no work, no timing, and no warning.
// ---------------------------------------------------------------------------

/// The channels that contributed to any returned hit.
fn channels_seen(
    report: &local_code_intelligence::app::SearchReport,
) -> std::collections::BTreeSet<String> {
    report
        .results
        .iter()
        .flat_map(|hit| hit.retrieval_channels.iter().cloned())
        .collect()
}

fn channel_set(names: &[&str]) -> std::collections::BTreeSet<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

/// A C# workspace in which the fake language server's canned `Calculator`
/// symbol (file `src/Calculator.cs`, zero-based line 2) lands inside the
/// indexed class chunk, so all three retrieval channels return candidates.
fn write_channel_workspace(root: &Path) {
    write(
        root,
        "src/Calculator.cs",
        "namespace Acceptance;\n\npublic sealed class Calculator\n{\n    public int Add(int a, int b) => a + b;\n}\n",
    );
}

fn fake_csharp_lsp(state: &Path) -> local_code_intelligence::config::CSharpLspConfig {
    local_code_intelligence::config::CSharpLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: vec![
            "--mode".to_owned(),
            "workspace-symbols".to_owned(),
            "--state".to_owned(),
            state.to_string_lossy().into_owned(),
        ],
        disabled: false,
    }
}

#[tokio::test]
async fn default_channels_keep_every_channel_and_the_reranker_on() {
    let (temp, mut config, fake, task) = fixture().await;
    let workspace = temp.path().join("channels-default");
    write_channel_workspace(&workspace);
    let state = temp.path().join("lsp-state-default");
    config.lsp_timeout_seconds = 10;
    config.csharp = fake_csharp_lsp(&state);
    assert_eq!(
        config.channels,
        local_code_intelligence::config::ChannelsConfig::default()
    );
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();
    let queries_before = fake.queries.load(Ordering::SeqCst);
    let reranks_before = fake.rerank_requests.load(Ordering::SeqCst);

    let report = app
        .search(&workspace, "Calculator", Some(10))
        .await
        .unwrap();
    assert_eq!(
        channels_seen(&report),
        channel_set(&["semantic", "lexical", "lsp"]),
        "{:?}",
        report.warning
    );
    assert!(report.reranked, "{:?}", report.warning);
    assert!(report.warning.is_none(), "{:?}", report.warning);
    assert_eq!(fake.queries.load(Ordering::SeqCst) - queries_before, 1);
    assert_eq!(
        fake.rerank_requests.load(Ordering::SeqCst) - reranks_before,
        1
    );
    let timings = &report.timings;
    assert!(timings.query_embedding_ms > 0.0);
    assert!(timings.lancedb_search_ms > 0.0);
    assert!(timings.lexical_search_ms > 0.0);
    assert!(timings.lsp_search_ms > 0.0);
    assert!(timings.reranking_ms > 0.0);
    task.abort();
}

#[tokio::test]
async fn each_channel_switch_removes_exactly_that_channel() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("channels-toggle");
    write_channel_workspace(&workspace);
    let state_root = temp.path().join("lsp-state");

    // Every variant disables one channel and also breaks it, so a search that
    // still reached the channel would warn or fail instead of passing.
    struct Variant {
        name: &'static str,
        expected: &'static [&'static str],
        reranked: bool,
    }
    let variants = [
        Variant {
            name: "semantic",
            expected: &["lexical", "lsp"],
            reranked: true,
        },
        Variant {
            name: "lexical",
            expected: &["semantic", "lsp"],
            reranked: true,
        },
        Variant {
            name: "lsp",
            expected: &["semantic", "lexical"],
            reranked: true,
        },
        Variant {
            name: "reranker",
            expected: &["semantic", "lexical", "lsp"],
            reranked: false,
        },
    ];

    // Index once with the defaults; every variant reuses the same index.
    let mut indexing = config.clone();
    indexing.lsp_timeout_seconds = 10;
    indexing.csharp = fake_csharp_lsp(&state_root.join("indexing"));
    let indexer = App::open(indexing).await.unwrap();
    indexer.index(&workspace).await.unwrap();
    drop(indexer);

    for variant in variants {
        let name = variant.name;
        let mut variant_config = config.clone();
        variant_config.lsp_timeout_seconds = 10;
        let state = state_root.join(name);
        variant_config.csharp = fake_csharp_lsp(&state);
        match name {
            "semantic" => variant_config.channels.semantic = false,
            "lexical" => {
                variant_config.channels.lexical = false;
                variant_config.ripgrep_path = "definitely-missing-ripgrep".into();
            }
            "lsp" => variant_config.channels.lsp = false,
            "reranker" => variant_config.channels.reranker = false,
            _ => unreachable!(),
        }
        let app = App::open(variant_config).await.unwrap();
        fake.embed_fails.store(name == "semantic", Ordering::SeqCst);
        fake.rerank_fails
            .store(name == "reranker", Ordering::SeqCst);
        let queries_before = fake.queries.load(Ordering::SeqCst);
        let reranks_before = fake.rerank_requests.load(Ordering::SeqCst);

        let report = app
            .search(&workspace, "Calculator", Some(10))
            .await
            .unwrap();
        fake.embed_fails.store(false, Ordering::SeqCst);
        fake.rerank_fails.store(false, Ordering::SeqCst);

        assert_eq!(
            channels_seen(&report),
            channel_set(variant.expected),
            "{name}: {:?}",
            report.warning
        );
        assert!(!report.results.is_empty(), "{name}");
        assert_eq!(report.reranked, variant.reranked, "{name}");
        assert!(
            report.warning.is_none(),
            "{name}: a deliberately disabled channel must not warn: {:?}",
            report.warning
        );
        assert_eq!(
            fake.queries.load(Ordering::SeqCst) - queries_before,
            usize::from(name != "semantic"),
            "{name}: query embeddings"
        );
        assert_eq!(
            fake.rerank_requests.load(Ordering::SeqCst) - reranks_before,
            usize::from(name != "reranker"),
            "{name}: rerank requests"
        );
        let timings = &report.timings;
        assert_eq!(
            timings.query_embedding_ms == 0.0 && timings.lancedb_search_ms == 0.0,
            name == "semantic",
            "{name}: semantic timings"
        );
        assert_eq!(
            timings.lexical_search_ms == 0.0,
            name == "lexical",
            "{name}: lexical timing"
        );
        assert_eq!(
            timings.lsp_search_ms == 0.0,
            name == "lsp",
            "{name}: lsp timing"
        );
        assert_eq!(
            timings.reranking_ms == 0.0,
            name == "reranker",
            "{name}: reranking timing"
        );
        for hit in &report.results {
            if name == "semantic" {
                assert!(hit.semantic_rank.is_none() && hit.semantic_score.is_none());
            }
            if name == "lexical" {
                assert!(
                    hit.lexical_rank.is_none()
                        && hit.lexical_match_count == 0
                        && hit.lexical_score.is_none()
                );
            }
            if name == "lsp" {
                assert!(hit.lsp_rank.is_none());
            }
            if name == "reranker" {
                assert!(hit.reranker_score.is_none());
            }
        }
        // The disabled language-server channel never even starts the server.
        assert_eq!(
            state.join("spawn_count.txt").exists(),
            name != "lsp",
            "{name}: language-server process"
        );
    }
    task.abort();
}

#[tokio::test]
async fn the_grep_only_arm_runs_lexical_search_alone_and_leaves_indexing_untouched() {
    let (temp, mut config, fake, task) = fixture().await;
    config.channels.semantic = false;
    config.channels.lsp = false;
    config.channels.reranker = false;
    let workspace = temp.path().join("grep-only");
    write(
        &workspace,
        "lib.rs",
        "fn translator() {}\nfn unrelated() {}\n",
    );
    let app = App::open(config).await.unwrap();

    // Toggles are query-time only: the first search still creates the index
    // and embeds every chunk, but never embeds a query or reranks.
    let first = app.search(&workspace, "translator", Some(5)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap()["index"]["action"],
        "created"
    );
    assert!(
        fake.documents.load(Ordering::SeqCst) > 0,
        "indexing still embeds chunks"
    );
    assert_eq!(fake.queries.load(Ordering::SeqCst), 0);
    assert_eq!(fake.rerank_requests.load(Ordering::SeqCst), 0);
    assert!(!first.reranked);
    assert!(first.warning.is_none(), "{:?}", first.warning);
    assert!(!first.results.is_empty());
    assert!(
        first
            .results
            .iter()
            .all(|hit| hit.retrieval_channels == vec!["lexical".to_owned()]),
        "{:?}",
        channels_seen(&first)
    );
    assert!(first.results[0].chunk.code.contains("translator"));
    assert!(first.results.iter().all(|hit| hit.fusion_score > 0.0));
    assert_eq!(first.timings.query_embedding_ms, 0.0);
    assert_eq!(first.timings.lancedb_search_ms, 0.0);
    assert_eq!(first.timings.lsp_search_ms, 0.0);
    assert_eq!(first.timings.reranking_ms, 0.0);
    assert!(first.timings.lexical_search_ms > 0.0);

    // With the embedding and reranking services down, nothing changes: the
    // arm never depends on them at query time.
    fake.embed_fails.store(true, Ordering::SeqCst);
    fake.rerank_fails.store(true, Ordering::SeqCst);
    let second = app.search(&workspace, "translator", Some(5)).await.unwrap();
    assert!(second.warning.is_none(), "{:?}", second.warning);
    assert_eq!(channels_seen(&second), channel_set(&["lexical"]));

    // A query that matches no line is an empty answer, not a failure of
    // every channel.
    let none = app
        .search(&workspace, "zzzzqqqqnotpresent", Some(5))
        .await
        .unwrap();
    assert!(none.results.is_empty());
    assert!(none.warning.is_none(), "{:?}", none.warning);

    // A query made only of stop words has no lexical terms at all: also a
    // clean miss, not a failure and not a warning.
    let stop_words_only = app
        .search(&workspace, "add fix when the", Some(5))
        .await
        .unwrap();
    assert!(stop_words_only.results.is_empty());
    assert!(
        stop_words_only.warning.is_none(),
        "{:?}",
        stop_words_only.warning
    );
    task.abort();
}

#[tokio::test]
async fn lexical_ranking_prefers_a_rare_identifier_over_a_common_verb() {
    let (temp, mut config, _fake, task) = fixture().await;
    config.channels.semantic = false;
    config.channels.lsp = false;
    config.channels.reranker = false;
    let workspace = temp.path().join("lexical-idf");
    // "process" is on ten lines of one function and on a line of every other
    // file (ten files in all), so it identifies nothing. Counting matching
    // lines would put that function first.
    let busy: String = (0..10)
        .map(|step| format!("    process(step_{step});\n"))
        .collect();
    write(
        &workspace,
        "busy.rs",
        &format!("fn busy_loop() {{\n{busy}}}\n"),
    );
    for plain in 0..7 {
        write(
            &workspace,
            &format!("plain_{plain}.rs"),
            &format!("fn plain_{plain}() {{ process(); }}\n"),
        );
    }
    // The rare identifier is in two files: on five lines of one, one of the
    // other.
    write(
        &workspace,
        "rare.rs",
        "fn quasarflux() {\n    process(quasarflux_a);\n    quasarflux_b();\n    quasarflux_c();\n    quasarflux_d();\n}\n",
    );
    write(
        &workspace,
        "second.rs",
        "fn second() { process(quasarflux); }\n",
    );
    let app = App::open(config).await.unwrap();

    let report = app
        .search(&workspace, "process quasarflux", Some(10))
        .await
        .unwrap();
    assert!(report.warning.is_none(), "{:?}", report.warning);
    let paths: Vec<_> = report
        .results
        .iter()
        .map(|hit| hit.chunk.relative_file_path.as_str())
        .collect();
    // Chunks that match only the too-common verb are not candidates.
    assert_eq!(paths, ["rare.rs", "second.rs"]);
    assert_eq!(report.results[0].lexical_match_count, 5);
    assert_eq!(report.results[1].lexical_match_count, 1);
    let scores: Vec<f32> = report
        .results
        .iter()
        .map(|hit| hit.lexical_score.expect("a lexical score"))
        .collect();
    assert!(scores[1] > 0.0 && scores[0] > scores[1], "{scores:?}");
    assert_eq!(
        report
            .results
            .iter()
            .map(|hit| hit.lexical_rank)
            .collect::<Vec<_>>(),
        [Some(1), Some(2)]
    );
    assert!(report.results.iter().all(|hit| hit.fusion_score > 0.0));
    assert!(
        report
            .results
            .iter()
            .all(|hit| hit.semantic_score.is_none() && hit.reranker_score.is_none())
    );

    // Terms common to every file still return results: with nothing rare to
    // prefer, nothing is cut, and the function with more lines wins.
    let common = app.search(&workspace, "process", Some(3)).await.unwrap();
    assert_eq!(common.results[0].chunk.relative_file_path, "busy.rs");
    assert_eq!(common.results[0].lexical_match_count, 10);
    task.abort();
}

#[tokio::test]
async fn camel_case_query_finds_snake_case_definition() {
    let (temp, mut config, _fake, task) = fixture().await;
    config.channels.semantic = false;
    config.channels.lsp = false;
    config.channels.reranker = false;
    let workspace = temp.path().join("lexical-split");
    write(
        &workspace,
        "loader.rs",
        "fn parse_config(input: &str) -> usize {\n    input.len()\n}\n",
    );
    for filler in 0..5 {
        write(
            &workspace,
            &format!("filler_{filler}.rs"),
            &format!("fn filler_{filler}() {{}}\n"),
        );
    }
    let app = App::open(config).await.unwrap();

    // `parseconfig` itself matches nothing; the `parse` and `config`
    // sub-tokens find the definition.
    let report = app
        .search(&workspace, "parseConfig", Some(5))
        .await
        .unwrap();
    assert!(report.warning.is_none(), "{:?}", report.warning);
    assert_eq!(report.results.len(), 1);
    let hit = &report.results[0];
    assert!(hit.chunk.code.contains("fn parse_config"));
    assert_eq!(hit.retrieval_channels, vec!["lexical".to_owned()]);
    assert_eq!(hit.lexical_rank, Some(1));
    assert_eq!(hit.lexical_match_count, 1);
    assert!(hit.lexical_score.is_some_and(|score| score > 0.0));
    task.abort();
}

#[tokio::test]
async fn a_search_still_fails_when_every_enabled_channel_fails() {
    // The semantic channel alone: if the embedding service is down there is
    // nothing to fall back on, so this stays an error as it always was.
    let (temp, mut config, fake, task) = fixture().await;
    config.channels.lexical = false;
    config.channels.lsp = false;
    let workspace = temp.path().join("semantic-only");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();
    fake.embed_fails.store(true, Ordering::SeqCst);
    let error = app
        .search(&workspace, "translator", Some(2))
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("every enabled retrieval channel failed"),
        "{error:#}"
    );
    assert!(
        format!("{error:#}").contains("Query embedding unavailable; results from the other"),
        "the error carries the channel warning: {error:#}"
    );
    task.abort();
}

#[tokio::test]
async fn with_semantic_off_a_clean_miss_is_empty_but_failed_channels_are_an_error() {
    let (temp, mut config, _fake, task) = fixture().await;
    config.channels.semantic = false;
    config.channels.reranker = false;
    config.lsp_timeout_seconds = 10;
    let workspace = temp.path().join("semantic-off-failures");
    write_channel_workspace(&workspace);
    let missing_rg = "definitely-missing-ripgrep";

    // Lexical search alone, ripgrep healthy, nothing matches: an ordinary
    // empty answer with no warning.
    let mut lexical_only = config.clone();
    lexical_only.channels.lsp = false;
    let app = App::open(lexical_only).await.unwrap();
    app.index(&workspace).await.unwrap();
    let miss = app
        .search(&workspace, "zzzzqqqqnotpresent", Some(5))
        .await
        .unwrap();
    assert!(miss.results.is_empty());
    assert!(miss.warning.is_none(), "{:?}", miss.warning);
    drop(app);

    // Lexical search alone with ripgrep missing: the only enabled channel
    // failed, so the search fails and says why.
    let mut lexical_broken = config.clone();
    lexical_broken.channels.lsp = false;
    lexical_broken.ripgrep_path = missing_rg.into();
    let app = App::open(lexical_broken).await.unwrap();
    let error = app
        .search(&workspace, "Calculator", Some(5))
        .await
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("every enabled retrieval channel failed"),
        "{message}"
    );
    assert!(
        message.contains("Lexical retrieval unavailable"),
        "{message}"
    );
    drop(app);

    // Language-server search alone with the only provider failing: also an
    // error.
    let mut lsp_broken = config.clone();
    lsp_broken.channels.lexical = false;
    lsp_broken.csharp = fake_csharp_lsp(&temp.path().join("lsp-state-broken"));
    lsp_broken.csharp.path = Some("definitely-missing-csharp-ls".into());
    let app = App::open(lsp_broken).await.unwrap();
    let error = app
        .search(&workspace, "Calculator", Some(5))
        .await
        .unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("every enabled retrieval channel failed"),
        "{message}"
    );
    assert!(
        message.contains("Optional LSP provider unavailable"),
        "{message}"
    );
    drop(app);

    // Lexical search failing while a healthy language server runs and finds
    // nothing in the index: a channel ran fine, so the result is empty (with
    // the lexical warning) rather than an error.
    let state = temp.path().join("lsp-state-healthy");
    let mut lsp_healthy = config.clone();
    lsp_healthy.ripgrep_path = missing_rg.into();
    lsp_healthy.csharp = fake_csharp_lsp(&state);
    // The canned symbol points at a file the index does not contain.
    lsp_healthy
        .csharp
        .args
        .extend(["--primary-file".to_owned(), "src/Other.cs".to_owned()]);
    let app = App::open(lsp_healthy).await.unwrap();
    let empty = app.search(&workspace, "Calculator", Some(5)).await.unwrap();
    assert!(state.join("spawn_count.txt").exists());
    assert!(empty.results.is_empty());
    let warning = empty.warning.as_deref().unwrap();
    assert!(
        warning.contains("Lexical retrieval unavailable"),
        "{warning}"
    );
    assert!(!warning.contains("Optional LSP provider"), "{warning}");
    task.abort();
}

#[tokio::test]
async fn a_configuration_with_no_retrieval_channel_is_rejected_when_the_app_opens() {
    let (_temp, mut config, _fake, task) = fixture().await;
    config.channels.semantic = false;
    config.channels.lexical = false;
    config.channels.lsp = false;
    // The reranker alone is not a retrieval channel.
    config.channels.reranker = true;
    let error = App::open(config).await.err().expect("must be rejected");
    assert!(
        format!("{error:#}").contains("at least one of semantic, lexical, and lsp"),
        "{error:#}"
    );
    task.abort();
}
