use super::*;

#[tokio::test]
async fn watcher_skips_a_queued_reindex_when_the_explicit_job_already_committed() {
    use local_code_intelligence::store::Store;

    let (temp, mut config, fake, task) = fixture().await;
    config.index.freshness = IndexFreshness::Watch;
    config.watch_poll_milliseconds = 20;
    config.watch_debounce_milliseconds = 20;
    let data_dir = config.data_dir.clone();
    let workspace = temp.path().join("watched");
    write(&workspace, "lib.rs", "fn before() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    let resolved = app.index(&workspace).await.unwrap().workspace;
    let store = Store::open(&data_dir.join("lancedb")).await.unwrap();
    let version_before = store
        .snapshot(&resolved)
        .await
        .unwrap()
        .unwrap()
        .table
        .version()
        .await
        .unwrap();

    let indexed_source = "fn after() {}\n";
    write(&workspace, "lib.rs", indexed_source);
    fake.hold_documents.store(true, Ordering::SeqCst);
    let indexing_app = app.clone();
    let indexing_workspace = workspace.clone();
    let job = tokio::spawn(async move { indexing_app.index(&indexing_workspace).await });
    tokio::time::timeout(Duration::from_secs(30), fake.document_held.notified())
        .await
        .expect("explicit index did not reach the held embedder");

    app.watch(&workspace).await.unwrap();
    // Queue a watcher pass while the explicit job holds the write lock,
    // then restore the content that job scanned before it commits.
    write(&workspace, "lib.rs", "fn transient_edit() {}\n");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!job.is_finished());
    write(&workspace, "lib.rs", indexed_source);
    fake.hold_documents.store(false, Ordering::SeqCst);
    fake.release_documents.notify_one();
    let indexed = tokio::time::timeout(Duration::from_secs(30), job)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(indexed.embedded_chunks, 1);

    // Allow the queued pass to finish and several more polls to run.
    tokio::time::sleep(Duration::from_millis(500)).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    while app.status(&workspace).await.unwrap().indexing {
        assert!(
            tokio::time::Instant::now() < deadline,
            "watcher did not settle"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    app.unwatch(&workspace).await.unwrap();
    let version_after = store
        .snapshot(&resolved)
        .await
        .unwrap()
        .unwrap()
        .table
        .version()
        .await
        .unwrap();
    assert_eq!(
        version_after,
        version_before + 1,
        "only the explicit job should commit; the queued watcher must not overwrite the table"
    );
    assert!(!app.status(&workspace).await.unwrap().stale);
    task.abort();
}

#[tokio::test]
async fn watcher_debounces_and_refreshes_changed_typescript_source() {
    let (temp, mut config, _fake, task) = fixture().await;
    config.index.freshness = IndexFreshness::Watch;
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
