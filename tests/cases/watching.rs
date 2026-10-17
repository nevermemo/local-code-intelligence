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

/// A `serve` process and the stderr log it writes; killed when dropped, so a
/// failed test leaves no server behind.
#[cfg(unix)]
struct ServeProcess(std::process::Child);

#[cfg(unix)]
impl Drop for ServeProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// SIGTERM ends `serve` promptly even while a watcher's reindex waits for
/// another process's data-directory index lock. That wait used to block a
/// thread that the runtime's shutdown waits for, so the process stayed alive
/// until the other process released the lock.
#[cfg(unix)]
#[tokio::test]
async fn sigterm_ends_serve_while_a_reindex_waits_for_another_process_lock() {
    let (temp, mut config, _models, models_task) = fixture().await;
    config.index.freshness = IndexFreshness::Watch;
    config.watch_poll_milliseconds = 50;
    config.watch_debounce_milliseconds = 50;
    config.channels.lsp = false;
    let [port] = free_ports();
    config.port = port;
    let workspace = temp.path().join("watched");
    write(&workspace, "lib.rs", "fn before_lock() {}\n");
    let config_path = temp.path().join("serve.toml");
    std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();

    let log = temp.path().join("serve.log");
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
    command
        .arg("--config")
        .arg(&config_path)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap());
    // The test waits for an `info` line: the default log filter, whatever
    // `RUST_LOG` the test runs with.
    strip_lci_env(&mut command).env_remove("RUST_LOG");
    let mut serve = ServeProcess(command.spawn().unwrap());
    let base = format!("http://127.0.0.1:{}", config.port);
    let client = reqwest::Client::new();
    let mut healthy = false;
    for _ in 0..200 {
        if serve.0.try_wait().unwrap().is_some() {
            break;
        }
        if let Ok(response) = client.get(format!("{base}/health")).send().await
            && response.status().is_success()
        {
            healthy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        healthy,
        "serve did not become healthy: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    for route in ["index_workspace", "watch_workspace"] {
        let response = client
            .post(format!("{base}/v1/{route}"))
            .json(&json!({"workspace_path": workspace}))
            .send()
            .await
            .unwrap();
        assert!(
            response.status().is_success(),
            "{route} failed: {}",
            response.text().await.unwrap()
        );
    }

    // This process holds the lock, as another LCI committing an index would.
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(config.data_dir.join("index.lock"))
        .unwrap();
    lock.lock().unwrap();
    write(&workspace, "lib.rs", "fn after_lock() {}\n");
    let started = std::time::Instant::now();
    while !std::fs::read_to_string(&log)
        .unwrap_or_default()
        .contains("waiting for it")
    {
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the watcher's reindex never waited for the lock: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let signal = tokio::process::Command::new("kill")
        .args(["-TERM", &serve.0.id().to_string()])
        .status()
        .await
        .unwrap();
    assert!(signal.success(), "could not send SIGTERM to serve");
    let status = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(status) = serve.0.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "serve did not exit within 10 s of SIGTERM while the lock stayed held: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        )
    });
    drop(models_task);
    assert_eq!(status.code(), Some(143), "serve exit status: {status}");
    drop(lock);
}
