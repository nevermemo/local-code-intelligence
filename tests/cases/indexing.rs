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
