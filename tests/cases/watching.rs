use super::*;

#[tokio::test]
async fn unwatch_deleted_workspace_stops_the_task() {
    let (temp, mut config, _fake, task) = fixture().await;
    config.watch_poll_milliseconds = 60_000;
    let workspace = temp.path().join("watched");
    write(&workspace, "lib.rs", "fn anchor() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    app.index(&workspace).await.unwrap();
    let started = app.watch(&workspace).await.unwrap();
    std::fs::remove_dir_all(&workspace).unwrap();

    let stopped = app.unwatch(&started.workspace.path).await.unwrap();
    assert_eq!(stopped.workspace.id, started.workspace.id);
    assert!(!stopped.watched && !stopped.already_in_requested_state);
    let weak = Arc::downgrade(&app);
    drop(app);
    tokio::time::timeout(Duration::from_secs(5), async {
        while weak.strong_count() != 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("unwatch did not end the watcher task");
    task.abort();
}

#[tokio::test]
async fn watcher_removes_itself_when_workspace_disappears() {
    let (temp, mut config, _fake, task) = fixture().await;
    config.watch_poll_milliseconds = 50;
    let workspace = temp.path().join("watched");
    write(&workspace, "lib.rs", "fn anchor() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    app.index(&workspace).await.unwrap();
    app.watch(&workspace).await.unwrap();
    std::fs::remove_dir_all(&workspace).unwrap();

    tokio::time::timeout(Duration::from_secs(5), async {
        while Arc::strong_count(&app) != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("watcher did not end after its workspace disappeared");
    std::fs::create_dir(&workspace).unwrap();
    assert!(!app.status(&workspace).await.unwrap().watched);
    assert!(
        app.unwatch(&workspace)
            .await
            .unwrap()
            .already_in_requested_state
    );
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
