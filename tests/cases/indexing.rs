use super::*;

#[tokio::test]
async fn search_reuses_previous_snapshot_before_held_index_job_finishes() {
    let (temp, mut config, fake, task) = fixture().await;
    config.index.wait_for_existing_job_seconds = 1;
    config.index.stale_check_interval_seconds = 0;
    config.channels.lsp = false;
    let workspace = temp.path().join("bounded-search");
    write(&workspace, "lib.rs", "fn previous_snapshot() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    app.index(&workspace).await.unwrap();

    write(&workspace, "lib.rs", "fn replacement_snapshot() {}\n");
    fake.hold_documents.store(true, Ordering::SeqCst);
    let indexing_app = app.clone();
    let indexing_workspace = workspace.clone();
    let job = tokio::spawn(async move { indexing_app.index(&indexing_workspace).await });
    tokio::time::timeout(Duration::from_secs(30), fake.document_held.notified())
        .await
        .expect("index job did not reach the held embedding response");

    // The response stays held throughout search: machine load can widen the
    // margin, but search must never depend on the index job completing.
    let search = tokio::time::timeout(
        Duration::from_secs(15),
        app.search(&workspace, "snapshot", Some(8)),
    )
    .await;
    let returned_before_release = search.is_ok();
    let job_was_running = !job.is_finished();
    fake.hold_documents.store(false, Ordering::SeqCst);
    fake.release_documents.notify_one();
    let indexed = job.await.unwrap().unwrap();

    assert!(
        returned_before_release,
        "search did not return before the held index job was released"
    );
    assert!(
        job_was_running,
        "index job ended before its response was released"
    );
    let found = search.unwrap().unwrap();
    assert!(
        found
            .warning
            .as_deref()
            .unwrap_or("")
            .contains("reusing previous snapshot")
    );
    assert!(matches!(
        found.index.action,
        local_code_intelligence::app::IndexAction::Reused
    ));
    assert!(
        found.index.wait_ms >= 1000.0,
        "index wait was not accounted for"
    );
    assert!(found.index.wait_ms <= found.timings.total_ms);
    assert_eq!(found.results.len(), 1);
    assert!(found.results[0].chunk.code.contains("previous_snapshot"));
    assert!(!found.results[0].chunk.code.contains("replacement_snapshot"));
    assert_eq!(indexed.embedded_chunks, 1);
    let current = app.search(&workspace, "snapshot", Some(8)).await.unwrap();
    assert_eq!(current.results.len(), 1);
    assert!(
        current.results[0]
            .chunk
            .code
            .contains("replacement_snapshot")
    );
    task.abort();
}

#[tokio::test]
async fn deeply_nested_modules_index_and_reuse_without_stack_overflow() {
    const CHILD: &str = "LCI_TEST_DEEP_INDEX_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "indexing::deeply_nested_modules_index_and_reuse_without_stack_overflow",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "deep indexing failed: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        // A renamed test would make the child run nothing and pass.
        assert!(
            String::from_utf8_lossy(&output.stdout).contains("1 passed"),
            "the child ran no test: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }

    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("deep-modules");
    let source = format!(
        "{}fn leaf() {{}}\n{}fn sibling() {{}}\n",
        "mod m {\n".repeat(32768),
        "}\n".repeat(32768)
    );
    write(&workspace, "lib.rs", &source);
    let app = App::open(config.clone()).await.unwrap();
    let report = app.index(&workspace).await.unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(report.parsed_files, 1);
    assert!(report.chunks > 0);
    assert!(report.embedded_chunks > 0);
    assert!(report.embedded_chunks <= report.chunks);
    assert_eq!(
        fake.documents.load(Ordering::SeqCst),
        report.embedded_chunks
    );
    let indexed = app.indexed_files(&workspace).await.unwrap();
    assert_eq!(indexed.files.len(), 1);
    assert_eq!(indexed.files[0].relative_file_path, "lib.rs");
    assert_eq!(indexed.files[0].chunks, report.chunks);

    drop(app);
    let reopened = App::open(config).await.unwrap();
    let repeat = reopened.index(&workspace).await.unwrap();
    assert_eq!(repeat.parsed_files, 0);
    assert_eq!(repeat.unchanged_files, 1);
    assert_eq!(repeat.chunks, report.chunks);
    assert_eq!(repeat.embedded_chunks, 0);
    assert_eq!(repeat.reused_chunks, report.chunks);
    // The reopened App has observed no embedding width, so it sends one
    // probe document (N43); no chunk is embedded again.
    assert_eq!(
        fake.documents.load(Ordering::SeqCst),
        report.embedded_chunks + 1
    );
    task.abort();
}

#[cfg(unix)]
#[tokio::test]
async fn unix_backslashes_preserve_indexed_names_and_result_paths() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("backslash-paths");
    let names = [r"a\b.rs", "a/b.rs", r"\tmp\x.rs"];
    for (name, source) in names.iter().zip([
        "fn backslash_name() {}\n",
        "fn nested_name() {}\n",
        "fn leading_backslash() {}\n",
    ]) {
        write(&workspace, name, source);
    }
    // A temp directory can be a symlink (macOS); results carry the canonical path.
    let workspace = std::fs::canonicalize(&workspace).unwrap();
    let app = App::open(config.clone()).await.unwrap();
    let report = app.index(&workspace).await.unwrap();
    assert_eq!(report.files, names.len(), "distinct file names collided");
    let indexed = app.indexed_files(&workspace).await.unwrap();
    let indexed_names: std::collections::HashSet<_> = indexed
        .files
        .iter()
        .map(|file| file.relative_file_path.as_str())
        .collect();
    assert_eq!(
        indexed_names,
        names.into_iter().collect::<std::collections::HashSet<_>>()
    );

    let found = app.search(&workspace, "file names", Some(8)).await.unwrap();
    assert_eq!(found.results.len(), names.len());
    for hit in &found.results {
        let relative = &hit.chunk.relative_file_path;
        assert!(names.contains(&relative.as_str()));
        let path = Path::new(&hit.file_path);
        assert!(path.starts_with(&workspace), "result left the workspace");
        assert_eq!(path, workspace.join(relative));
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains(&hit.chunk.code)
        );
    }

    drop(app);
    let reopened = App::open(config).await.unwrap();
    let reused = reopened.index(&workspace).await.unwrap();
    assert_eq!(reused.files, names.len());
    assert_eq!(reused.parsed_files, 0);
    assert_eq!(reused.unchanged_files, names.len());
    task.abort();
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
async fn filters_without_matching_chunks_skip_retrieval_channels() {
    use local_code_intelligence::filter::FilterRequest;

    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("filtered-empty");
    write(&workspace, "src/lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();
    let queries = fake.queries.load(Ordering::SeqCst);
    let rerank_requests = fake.rerank_requests.load(Ordering::SeqCst);

    let no_paths = app
        .search_with_filters(
            &workspace,
            "translator",
            Some(1),
            FilterRequest {
                include_paths: Some(vec![]),
                ..FilterRequest::default()
            },
        )
        .await
        .unwrap();
    assert!(no_paths.results.is_empty());
    assert!(no_paths.timings.total_ms > 0.0, "the total timing is set");
    assert_eq!(
        fake.queries.load(Ordering::SeqCst),
        queries,
        "an empty include_paths list embedded the query"
    );

    let no_language = app
        .search_with_filters(
            &workspace,
            "translator",
            Some(1),
            FilterRequest {
                languages: Some(vec!["python".to_owned()]),
                ..FilterRequest::default()
            },
        )
        .await
        .unwrap();
    assert!(no_language.results.is_empty());
    assert!(
        no_language.timings.total_ms > 0.0,
        "the total timing is set"
    );
    assert_eq!(
        fake.queries.load(Ordering::SeqCst),
        queries,
        "a language nothing is indexed under embedded the query"
    );
    assert_eq!(fake.rerank_requests.load(Ordering::SeqCst), rerank_requests);

    let matching = app
        .search_with_filters(
            &workspace,
            "translator",
            Some(1),
            FilterRequest {
                include_paths: Some(vec!["src/lib.rs".to_owned()]),
                ..FilterRequest::default()
            },
        )
        .await
        .unwrap();
    assert!(
        matching
            .results
            .iter()
            .any(|hit| hit.chunk.code.contains("translator"))
    );
    assert_eq!(fake.queries.load(Ordering::SeqCst), queries + 1);
    assert_eq!(
        fake.rerank_requests.load(Ordering::SeqCst),
        rerank_requests + 1
    );
    task.abort();
}

#[tokio::test]
async fn a_path_filter_gets_semantic_hits_beyond_the_nearest_unfiltered_rows() {
    use local_code_intelligence::filter::FilterRequest;

    let (temp, mut config, _fake, task) = fixture().await;
    config.channels.lsp = false;
    let workspace = temp.path().join("filtered-far");
    // The fake embeds every chunk that mentions "translator" as the query's
    // own vector: these 170 chunks are the nearest rows, more than the 160
    // the semantic channel's first fetch asks for at the default 40
    // candidates. The one chunk the filter keeps is farther away and shares
    // no query term, so only the semantic channel can find it.
    for i in 0..170 {
        write(
            &workspace,
            &format!("noise/n{i}.rs"),
            &format!("fn translator_{i}() {{}}\n"),
        );
    }
    write(&workspace, "wanted/target.rs", "fn far_away() {}\n");
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let found = app
        .search_with_filters(
            &workspace,
            "translator",
            Some(5),
            FilterRequest {
                include_paths: Some(vec!["wanted/**".to_owned()]),
                ..FilterRequest::default()
            },
        )
        .await
        .unwrap();
    let hit = found
        .results
        .iter()
        .find(|hit| hit.chunk.relative_file_path == "wanted/target.rs")
        .unwrap_or_else(|| panic!("no hit: {:?}", found.warning));
    assert!(hit.semantic_score.is_some());
    assert_eq!(found.warning, None);
    task.abort();
}

#[tokio::test]
async fn go_java_c_and_cpp_source_is_indexed_and_searchable() {
    // (language, directory, file, source, expected chunks, query, text the
    // top hit must contain). Where there are 2 chunks, the file has a
    // mandatory top-level statement (`package main`) next to the doc-commented
    // declaration, and the container splits them; C and C++ have none, so a
    // single top-level function (with its leading comment attached) collapses
    // into one chunk.
    let cases = [
        (
            "go",
            "go-service",
            "main.go",
            "package main\n\n// Greet returns a friendly greeting for name.\nfunc Greet(name string) string {\n\treturn \"hello \" + name\n}\n",
            2,
            "Greet",
            "func Greet",
        ),
        (
            "java",
            "java-service",
            "Greeter.java",
            // The class gets its single member attached (attaches_header_to_child).
            "package main;\n\n// Greet returns a friendly greeting for name.\npublic class Greeter {\n    public static String greet(String name) {\n        return \"hello \" + name;\n    }\n}\n",
            2,
            "greet",
            "static String greet",
        ),
        (
            "c",
            "c-service",
            "greet.c",
            "// Greet returns a friendly greeting for name.\nconst char *greet(const char *name) {\n    return name;\n}\n",
            1,
            "greet",
            "const char *greet",
        ),
        (
            "cpp",
            "cpp-service",
            "greet.cpp",
            "// Greet returns a friendly greeting for name.\nstd::string greet(const std::string &name) {\n    return name;\n}\n",
            1,
            "greet",
            "std::string greet",
        ),
        (
            "c",
            "c-header-service",
            "include/greet.h",
            "// Greet_header returns a friendly greeting for name.\nconst char *greet_header(const char *name);\n",
            1,
            "greet_header",
            "const char *greet_header",
        ),
        (
            "cpp",
            "cc-service",
            "greet.cc",
            "// Greet_cc returns a friendly greeting for name.\nstd::string greet_cc(const std::string &name) {\n    return name;\n}\n",
            1,
            "greet_cc",
            "std::string greet_cc",
        ),
        (
            "cpp",
            "cxx-service",
            "greet.cxx",
            "// Greet_cxx returns a friendly greeting for name.\nstd::string greet_cxx(const std::string &name) {\n    return name;\n}\n",
            1,
            "greet_cxx",
            "std::string greet_cxx",
        ),
        (
            "cpp",
            "hh-service",
            "include/greet.hh",
            "// Greet_hh returns a friendly greeting for name.\nstd::string greet_hh(const std::string &name);\n",
            1,
            "greet_hh",
            "std::string greet_hh",
        ),
        (
            "cpp",
            "hxx-service",
            "include/greet.hxx",
            "// Greet_hxx returns a friendly greeting for name.\nstd::string greet_hxx(const std::string &name);\n",
            1,
            "greet_hxx",
            "std::string greet_hxx",
        ),
    ];
    // Each case has its own fixture, so they run side by side.
    futures::future::join_all(cases.into_iter().map(
        |(language, directory, file, source, chunks, query, code)| async move {
            let (temp, config, _fake, task) = fixture().await;
            let workspace = temp.path().join(directory);
            write(&workspace, file, source);
            let app = App::open(config).await.unwrap();

            let report = app.index(&workspace).await.unwrap();
            assert_eq!(report.files, 1, "{language}");
            assert_eq!(report.chunks, chunks, "{language}");

            let indexed = app.indexed_files(&workspace).await.unwrap();
            assert_eq!(indexed.files.len(), 1, "{language}");
            assert_eq!(indexed.files[0].language, language);
            assert_eq!(indexed.files[0].chunks, chunks, "{language}");

            let found = app.search(&workspace, query, Some(1)).await.unwrap();
            assert_eq!(found.results[0].chunk.language, language);
            assert!(found.results[0].chunk.code.contains(code), "{language}");
            task.abort();
        },
    ))
    .await;
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

/// A manifest that exists but cannot be read is not an index with no files:
/// `indexed_files` and `index_workspace` fail and say so, and `status` reports
/// the snapshot as stale with the cause instead of a clean answer. Here the
/// manifest file is replaced by a directory.
#[tokio::test]
async fn an_unreadable_manifest_is_reported_not_taken_for_an_empty_index() {
    let (temp, config, _fake, task) = fixture().await;
    let data_dir = config.data_dir.clone();
    let workspace = temp.path().join("unreadable-manifest");
    write(&workspace, "src/lib.rs", "fn one() {}\n");
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();
    assert_eq!(app.indexed_files(&workspace).await.unwrap().files.len(), 1);

    let id = app.status(&workspace).await.unwrap().workspace.id;
    let manifest = data_dir.join("manifests").join(format!("{id}.json"));
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir(&manifest).unwrap();

    let listed = app.indexed_files(&workspace).await.unwrap_err();
    assert!(
        format!("{listed:#}").contains("read workspace manifest"),
        "{listed:#}"
    );
    let reindexed = app.index(&workspace).await.unwrap_err();
    assert!(
        format!("{reindexed:#}").contains("read workspace manifest"),
        "{reindexed:#}"
    );
    let status = app.status(&workspace).await.unwrap();
    assert!(status.stale, "an unreadable manifest must not look fresh");
    assert!(
        status
            .stale_check_error
            .as_deref()
            .is_some_and(|error| error.contains("read workspace manifest")),
        "{:?}",
        status.stale_check_error
    );

    // A missing manifest is still just an index that has none.
    std::fs::remove_dir(&manifest).unwrap();
    assert!(
        app.indexed_files(&workspace)
            .await
            .unwrap()
            .files
            .is_empty()
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
async fn a_failed_initial_embedding_indexes_without_vectors_and_the_next_index_run_embeds_them() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("retry");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    fake.embed_fails.store(true, Ordering::SeqCst);

    // A fresh workspace has no vectors to lose: the chunks are committed
    // without them and the search runs on the other channels.
    let first = app.search(&workspace, "translator", None).await.unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap()["index"]["action"],
        "created"
    );
    assert!(first.results[0].chunk.code.contains("translator"));
    let status = app.status(&workspace).await.unwrap();
    assert!(status.indexed && status.vectors_missing);
    assert_eq!(status.embedding_dimension, None);

    // A vectorless previous snapshot (same identity, nothing cached) stays
    // indexable without vectors while the service is still down.
    let again = app.index(&workspace).await.unwrap();
    assert!(again.vectors_missing);
    assert_eq!(again.embedded_chunks, 0);
    assert!(app.status(&workspace).await.unwrap().vectors_missing);

    fake.embed_fails.store(false, Ordering::SeqCst);
    let repaired = app.index(&workspace).await.unwrap();
    assert!(!repaired.vectors_missing);
    assert_eq!(repaired.embedded_chunks, 1);
    let status = app.status(&workspace).await.unwrap();
    assert!(!status.vectors_missing);
    assert!(status.embedding_dimension.is_some());
    let retried = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert!(
        retried.results[0]
            .retrieval_channels
            .contains(&"semantic".to_string()),
        "{:?}",
        retried.results[0].retrieval_channels
    );
    task.abort();
}

/// K0a, part 2: a vectorless index is repaired by the next search under
/// `on-search` freshness once the embedder answers again, and not under
/// `manual`, which never refreshes from a search.
#[tokio::test]
async fn a_vectorless_index_is_embedded_on_the_next_search_once_the_service_answers() {
    let (temp, mut config, fake, task) = fixture().await;
    config.index.stale_check_interval_seconds = 0;
    config.channels.lsp = false;
    let workspace = temp.path().join("repair");
    write(&workspace, "lib.rs", "fn translator() {}\nfn other() {}\n");
    let app = App::open(config).await.unwrap();

    fake.embed_fails.store(true, Ordering::SeqCst);
    let first = app.search(&workspace, "translator", Some(2)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap()["index"]["action"],
        "created"
    );
    assert!(app.status(&workspace).await.unwrap().vectors_missing);

    fake.embed_fails.store(false, Ordering::SeqCst);
    let documents = fake.documents.load(Ordering::SeqCst);
    let started = std::time::Instant::now();
    let repaired = app.search(&workspace, "translator", Some(2)).await.unwrap();
    let repair_time = started.elapsed();
    eprintln!("repair search took {repair_time:?}");
    assert_eq!(
        serde_json::to_value(&repaired).unwrap()["index"]["action"],
        "refreshed_incrementally"
    );
    assert!(
        repaired.results[0]
            .retrieval_channels
            .contains(&"semantic".to_string()),
        "{:?}",
        repaired.results[0].retrieval_channels
    );
    assert!(repaired.warning.is_none(), "{:?}", repaired.warning);
    let status = app.status(&workspace).await.unwrap();
    assert!(!status.vectors_missing);
    assert!(status.embedding_dimension.is_some());
    assert!(fake.documents.load(Ordering::SeqCst) > documents);

    // Repaired: the next search reuses the snapshot and embeds nothing.
    let documents = fake.documents.load(Ordering::SeqCst);
    let again = app.search(&workspace, "translator", Some(2)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&again).unwrap()["index"]["action"],
        "reused"
    );
    assert_eq!(fake.documents.load(Ordering::SeqCst), documents);
    task.abort();
}

/// A repair that finds the service still failing (an HTTP 503 does not trip the
/// connect breaker) is not tried again by every search: the next one inside
/// the window sends no embedding request.
#[tokio::test]
async fn a_failed_repair_is_not_retried_by_the_next_search_inside_the_window() {
    let (temp, mut config, fake, task) = fixture().await;
    config.index.stale_check_interval_seconds = 0;
    config.channels.lsp = false;
    let workspace = temp.path().join("repair-throttle");
    write(
        &workspace,
        "lib.rs",
        "fn translator() {}
",
    );
    let app = App::open(config).await.unwrap();

    fake.embed_fails.store(true, Ordering::SeqCst);
    app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert!(app.status(&workspace).await.unwrap().vectors_missing);

    // The first search after the index tries the repair and is answered 503.
    let requests = fake.embed_requests.load(Ordering::SeqCst);
    let tried = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&tried).unwrap()["index"]["action"],
        "reused"
    );
    assert!(
        fake.embed_requests.load(Ordering::SeqCst) > requests,
        "the repair was tried"
    );

    // The second one, inside the window, sends nothing: neither a repair nor
    // a query embedding.
    let requests = fake.embed_requests.load(Ordering::SeqCst);
    let second = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&second).unwrap()["index"]["action"],
        "reused"
    );
    assert!(
        second
            .warning
            .as_deref()
            .unwrap()
            .contains("the index has no vectors")
    );
    assert_eq!(fake.embed_requests.load(Ordering::SeqCst), requests);
    assert!(app.status(&workspace).await.unwrap().vectors_missing);
    task.abort();
}

#[tokio::test]
async fn a_vectorless_index_is_not_repaired_by_a_search_under_manual_freshness() {
    let (temp, mut config, fake, task) = fixture().await;
    config.index.freshness = IndexFreshness::Manual;
    config.index.stale_check_interval_seconds = 0;
    config.channels.lsp = false;
    let workspace = temp.path().join("manual-repair");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();

    fake.embed_fails.store(true, Ordering::SeqCst);
    app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert!(app.status(&workspace).await.unwrap().vectors_missing);

    // The service answers again, but a search under `manual` never indexes.
    fake.embed_fails.store(false, Ordering::SeqCst);
    let requests = fake.embed_requests.load(Ordering::SeqCst);
    let second = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&second).unwrap()["index"]["action"],
        "reused"
    );
    assert!(
        second
            .warning
            .as_deref()
            .unwrap()
            .contains("the index has no vectors")
    );
    assert!(app.status(&workspace).await.unwrap().vectors_missing);
    assert_eq!(
        fake.embed_requests.load(Ordering::SeqCst),
        requests,
        "neither a refresh nor a query embedding"
    );

    // The explicit index run fills the vectors in.
    let repaired = app.index(&workspace).await.unwrap();
    assert!(!repaired.vectors_missing);
    assert!(!app.status(&workspace).await.unwrap().vectors_missing);
    task.abort();
}

/// The first index against a refused port pays the connection attempts and
/// their backoff; with the connect breaker open the next index run makes one
/// attempt without backoff and the next search skips the repair, both still
/// without vectors.
#[tokio::test]
async fn a_refused_embedder_connection_is_not_retried_within_the_breaker_window() {
    let (temp, mut config, _fake, task) = fixture().await;
    let closed = ClosedPort::new();
    config.embedding_url = format!("http://127.0.0.1:{}/v1", closed.port);
    config.index.stale_check_interval_seconds = 0;
    config.channels.lsp = false;
    let workspace = temp.path().join("breaker");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();

    // The index run retries a refused connection with 1 s and 2 s backoff, so
    // this one takes at least three seconds on any platform.
    let started = std::time::Instant::now();
    let first = app.search(&workspace, "translator", Some(1)).await.unwrap();
    let first_time = started.elapsed();
    assert_eq!(
        serde_json::to_value(&first).unwrap()["index"]["action"],
        "created"
    );
    assert!(first_time >= Duration::from_secs(3), "{first_time:?}");

    // Inside the window the index run makes one attempt (refused connects
    // cost about 2 s on Windows and nothing elsewhere) and sleeps no
    // backoff, so it takes well under half the first run's time.
    let started = std::time::Instant::now();
    let report = app.index(&workspace).await.unwrap();
    let index_time = started.elapsed();
    eprintln!("first {first_time:?}, breaker-open index {index_time:?}");
    assert!(report.vectors_missing && report.embedded_chunks == 0);
    assert!(
        index_time < first_time / 2,
        "{index_time:?} vs {first_time:?}"
    );

    // The repair attempt of a search is skipped without re-indexing.
    let started = std::time::Instant::now();
    let again = app.search(&workspace, "translator", Some(1)).await.unwrap();
    let search_time = started.elapsed();
    eprintln!("breaker-open search {search_time:?}");
    assert_eq!(
        serde_json::to_value(&again).unwrap()["index"]["action"],
        "reused"
    );
    assert!(
        again
            .warning
            .as_deref()
            .unwrap()
            .contains("the index has no vectors")
    );
    assert!(
        search_time < first_time / 2,
        "{search_time:?} vs {first_time:?}"
    );
    assert!(app.status(&workspace).await.unwrap().vectors_missing);
    task.abort();
}

/// A fresh workspace whose embedder port refuses connections is indexed
/// without vectors; the search answers from lexical and says why.
#[tokio::test]
async fn a_fresh_workspace_is_indexed_without_vectors_when_the_embedder_port_is_closed() {
    let started = std::time::Instant::now();
    let (temp, mut config, _fake, task) = fixture().await;
    // A port that refuses connections, held for the whole test.
    let closed = ClosedPort::new();
    config.embedding_url = format!("http://127.0.0.1:{}/v1", closed.port);
    config.channels.lsp = false;
    let workspace = temp.path().join("no-embedder");
    write(
        &workspace,
        "lib.rs",
        "fn translator() {}\nfn unrelated() {}\n",
    );
    let app = App::open(config).await.unwrap();

    let report = app.search(&workspace, "translator", Some(2)).await.unwrap();
    let wire = serde_json::to_value(&report).unwrap();
    assert_eq!(wire["index"]["action"], "created", "{wire}");
    assert!(!report.results.is_empty(), "{wire}");
    assert!(
        report.results[0].chunk.code.contains("translator"),
        "{wire}"
    );
    assert_eq!(report.results[0].retrieval_channels, ["lexical"]);
    assert!(report.results[0].semantic_score.is_none());
    let warning = report.warning.as_deref().unwrap();
    assert!(
        warning.contains("Semantic retrieval unavailable: the index has no vectors (the embedding service was unreachable when it was built)"),
        "{warning}"
    );
    assert_eq!(report.timings.query_embedding_ms, 0.0);
    assert_eq!(report.timings.lancedb_search_ms, 0.0);

    let status = app.status(&workspace).await.unwrap();
    assert!(status.indexed && status.compatible && status.vectors_missing);
    assert!(status.chunks > 0);
    assert_eq!(
        serde_json::to_value(&status).unwrap()["vectors_missing"],
        true
    );

    // A second search reuses the vectorless snapshot and neither embeds the
    // query nor serves a placeholder.
    let again = app.search(&workspace, "unrelated", Some(2)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&again).unwrap()["index"]["action"],
        "reused"
    );
    assert!(again.results.iter().all(|hit| hit.semantic_score.is_none()
        && !hit.retrieval_channels.contains(&"semantic".to_string())));
    assert!(
        again
            .warning
            .as_deref()
            .unwrap()
            .contains("the index has no vectors")
    );

    // Each attempt to embed against a refused port costs about 9 s of
    // retries on Windows, so the test makes one (the first search): the
    // `index_workspace` report is checked by the navigation test instead.
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "{:?}",
        started.elapsed()
    );
    task.abort();
}

/// The hard rule survives: an embedding URL/model change makes the old
/// vectors incompatible, and a down embedder must not turn that into a
/// vectorless index.
#[tokio::test]
async fn an_embedding_identity_change_still_fails_with_the_embedder_down() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("identity");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config.clone()).await.unwrap();
    app.index(&workspace).await.unwrap();
    drop(app);

    // Another model name is another identity; the service (HTTP 503, which
    // is not retried) is down.
    fake.embed_fails.store(true, Ordering::SeqCst);
    let mut changed = config;
    changed.embedding_model = "another-model".into();
    let app = App::open(changed).await.unwrap();
    assert!(!app.status(&workspace).await.unwrap().compatible);

    let error = app
        .search(&workspace, "translator", None)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("embedding configuration changed"),
        "{error:#}"
    );
    let error = app.index(&workspace).await.unwrap_err();
    assert!(
        format!("{error:#}").contains("embedding a batch"),
        "{error:#}"
    );
    // The old snapshot, with its real vectors, is untouched.
    let status = app.status(&workspace).await.unwrap();
    assert!(!status.compatible && !status.vectors_missing);
    assert_eq!(status.chunks, 1);
    task.abort();
}

/// The gate is the empty vector cache, not the snapshot's flag: a snapshot
/// that holds no chunks has no vectors and no flag, and its next index run
/// may still go vectorless.
#[tokio::test]
async fn a_workspace_first_indexed_empty_can_then_be_indexed_vectorless_with_the_embedder_down() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("empty-first");
    std::fs::create_dir_all(&workspace).unwrap();
    let app = App::open(config).await.unwrap();
    let empty = app.index(&workspace).await.unwrap();
    assert_eq!(empty.chunks, 0);
    assert!(!empty.vectors_missing);
    let status = app.status(&workspace).await.unwrap();
    assert!(status.indexed && !status.vectors_missing);

    write(&workspace, "lib.rs", "fn translator() {}\n");
    fake.embed_fails.store(true, Ordering::SeqCst);
    let indexed = app.index(&workspace).await.unwrap();
    assert_eq!(indexed.chunks, 1);
    assert_eq!(indexed.embedded_chunks, 0);
    assert!(indexed.vectors_missing);
    assert!(app.status(&workspace).await.unwrap().vectors_missing);
    task.abort();
}

/// Only an availability error goes vectorless: a service that answers with a
/// client error (here HTTP 401) is a configuration problem and fails the
/// index with its message, fresh workspace or not.
#[tokio::test]
async fn a_configuration_error_from_the_embedder_still_fails_a_fresh_index() {
    let (temp, mut config, _fake, task) = fixture().await;
    let router = axum::Router::new().route(
        "/v1/embeddings",
        axum::routing::post(|| async { (axum::http::StatusCode::UNAUTHORIZED, "bad key marker") }),
    );
    let listener = loopback_tokio_listener();
    config.embedding_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let rejecting = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let workspace = temp.path().join("rejected");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();

    let error = app.index(&workspace).await.unwrap_err();
    assert!(format!("{error:#}").contains("bad key marker"), "{error:#}");
    let error = app
        .search(&workspace, "translator", None)
        .await
        .unwrap_err();
    assert!(format!("{error:#}").contains("bad key marker"), "{error:#}");
    assert!(!app.status(&workspace).await.unwrap().indexed);
    rejecting.abort();
    task.abort();
}

#[tokio::test]
async fn a_search_waiting_for_a_vectorless_explicit_index_uses_its_snapshot() {
    let (temp, mut config, _fake, task) = fixture().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let router = axum::Router::new().route(
        "/v1/embeddings",
        axum::routing::post({
            let requests = requests.clone();
            let started = started.clone();
            let release = release.clone();
            move || {
                let requests = requests.clone();
                let started = started.clone();
                let release = release.clone();
                async move {
                    let cause = match requests.fetch_add(1, Ordering::SeqCst) {
                        0 => {
                            started.notify_one();
                            release.notified().await;
                            "explicit-index failure"
                        }
                        // A waiter that missed the explicit index indexes by
                        // itself; answer at once so the assertions fail fast.
                        _ => "duplicate request",
                    };
                    (axum::http::StatusCode::SERVICE_UNAVAILABLE, cause)
                }
            }
        }),
    );
    let listener = loopback_tokio_listener();
    config.embedding_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let failing_models = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let workspace = temp.path().join("vectorless-explicit-index");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();

    let indexing = app.index(&workspace);
    tokio::pin!(indexing);
    tokio::select! {
        result = &mut indexing => panic!("index finished before release: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(10), started.notified()) => {
            result.expect("explicit indexing reaches the embedding request");
        }
    }

    let waiting = app.search(&workspace, "translator", None);
    tokio::pin!(waiting);
    // Keep the explicit index blocked while polling the search into its wait.
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut waiting)
            .await
            .is_err()
    );
    release.notify_one();
    let explicit = indexing.await.unwrap();
    assert!(explicit.vectors_missing);
    let found = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        serde_json::to_value(&found).unwrap()["index"]["action"],
        "waited_for_existing_job"
    );
    assert!(found.results[0].chunk.code.contains("translator"));
    assert!(
        found
            .warning
            .as_deref()
            .unwrap()
            .contains("the index has no vectors")
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        1,
        "the vectorless search embeds nothing"
    );
    failing_models.abort();
    task.abort();
}

#[tokio::test]
async fn a_search_waiting_for_a_failed_explicit_index_reports_its_error() {
    let (temp, mut config, _fake, task) = fixture().await;
    // HTTP 401 is a configuration error, not an availability one: the index
    // fails hard even on a fresh workspace, and the failure is what a waiting
    // search reports.
    let requests = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let router = axum::Router::new().route(
        "/v1/embeddings",
        axum::routing::post({
            let requests = requests.clone();
            let started = started.clone();
            let release = release.clone();
            move || {
                let requests = requests.clone();
                let started = started.clone();
                let release = release.clone();
                async move {
                    let cause = match requests.fetch_add(1, Ordering::SeqCst) {
                        0 => "first-search failure X",
                        1 => {
                            started.notify_one();
                            release.notified().await;
                            "explicit-index failure Y"
                        }
                        // A waiter that missed the explicit index indexes by
                        // itself; answer at once so the assertions fail fast.
                        _ => "duplicate request",
                    };
                    (axum::http::StatusCode::UNAUTHORIZED, cause)
                }
            }
        }),
    );
    let listener = loopback_tokio_listener();
    config.embedding_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let failing_models = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let workspace = temp.path().join("failed-explicit-index");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();

    let first = app
        .search(&workspace, "translator", None)
        .await
        .unwrap_err();
    assert!(format!("{first:#}").contains("first-search failure X"));

    let indexing = app.index(&workspace);
    tokio::pin!(indexing);
    tokio::select! {
        result = &mut indexing => panic!("index finished before release: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(10), started.notified()) => {
            result.expect("explicit indexing reaches the embedding request");
        }
    }
    assert_eq!(requests.load(Ordering::SeqCst), 2);

    let waiting = app.search(&workspace, "translator", None);
    tokio::pin!(waiting);
    // Keep the explicit index blocked while polling the search into its wait.
    assert!(
        tokio::time::timeout(Duration::from_secs(1), &mut waiting)
            .await
            .is_err()
    );
    release.notify_one();
    let explicit = indexing.await.unwrap_err();
    assert!(format!("{explicit:#}").contains("explicit-index failure Y"));
    let error = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .unwrap()
        .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("indexing failed while waiting"), "{error}");
    assert!(error.contains("explicit-index failure Y"), "{error}");
    assert!(!error.contains("first-search failure X"), "{error}");
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    failing_models.abort();
    task.abort();
}

#[tokio::test]
async fn a_search_waiting_for_a_cancelled_explicit_index_does_not_report_a_stale_error() {
    let (temp, mut config, _fake, task) = fixture().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(tokio::sync::Notify::new());
    let router = axum::Router::new().route(
        "/v1/embeddings",
        axum::routing::post({
            let requests = requests.clone();
            let started = started.clone();
            move || {
                let requests = requests.clone();
                let started = started.clone();
                async move {
                    let cause = match requests.fetch_add(1, Ordering::SeqCst) {
                        0 => "first-search failure X",
                        1 => {
                            started.notify_one();
                            std::future::pending::<()>().await;
                            unreachable!()
                        }
                        _ => "own failure Z",
                    };
                    (axum::http::StatusCode::UNAUTHORIZED, cause)
                }
            }
        }),
    );
    let listener = loopback_tokio_listener();
    config.embedding_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let failing_models = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let workspace = temp.path().join("cancelled-explicit-index");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();

    let first = app
        .search(&workspace, "translator", None)
        .await
        .unwrap_err();
    assert!(format!("{first:#}").contains("first-search failure X"));

    let mut indexing = Box::pin(app.index(&workspace));
    tokio::select! {
        result = &mut indexing => panic!("index finished while blocked: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(10), started.notified()) => {
            result.expect("explicit indexing reaches the embedding request");
        }
    }
    let waiting = app.search(&workspace, "translator", None);
    tokio::pin!(waiting);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut waiting)
            .await
            .is_err()
    );
    // Cancel the explicit index: it records no failure, so the waiter must
    // index by itself instead of reporting the earlier flight's X.
    drop(indexing);
    let error = tokio::time::timeout(Duration::from_secs(10), waiting)
        .await
        .unwrap()
        .unwrap_err();
    let error = format!("{error:#}");
    assert!(error.contains("own failure Z"), "{error}");
    assert!(!error.contains("first-search failure X"), "{error}");
    assert_eq!(requests.load(Ordering::SeqCst), 3);
    failing_models.abort();
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

/// Semantic score, by relative file path, of the chunks a search for
/// "translator" returns. The fake embeds any text containing "translator" (the
/// query included) as the same vector, so a chunk scores 1.0 exactly when the
/// text embedded for it mentioned "translator".
async fn translator_semantic_scores(
    app: &App,
    workspace: &Path,
) -> std::collections::HashMap<String, f32> {
    let found = app.search(workspace, "translator", Some(10)).await.unwrap();
    found
        .results
        .iter()
        .filter_map(|hit| Some((hit.chunk.relative_file_path.clone(), hit.semantic_score?)))
        .collect()
}

#[tokio::test]
async fn a_path_document_template_embeds_per_path_and_reuses_unchanged_chunks() {
    let (temp, mut config, fake, task) = fixture().await;
    config.embedding_document_template = "title: {path} | text: {code}".into();
    let workspace = temp.path().join("path-template");
    // Neither file's code mentions "translator"; one file's path does.
    write(&workspace, "lib.rs", "fn alpha() {}\n");
    write(&workspace, "translator_notes.rs", "fn beta() {}\n");
    let app = App::open(config).await.unwrap();

    let first = app.index(&workspace).await.unwrap();
    assert!(first.chunks >= 2);
    assert_eq!(first.embedded_chunks, first.chunks);
    assert_eq!(first.reused_chunks, 0);

    // The documents were embedded from the template: only the path in the
    // text can make translator_notes.rs match. (A `title:` document does not
    // start with the query instruction, so the fake counts it as a document.)
    let scores = translator_semantic_scores(&app, &workspace).await;
    assert!(
        (scores["translator_notes.rs"] - 1.0).abs() < 0.001,
        "{scores:?}"
    );
    assert!(scores["lib.rs"] < 0.5, "{scores:?}");

    // Unchanged files keep their vectors.
    let documents_before = fake.documents.load(Ordering::SeqCst);
    let second = app.index(&workspace).await.unwrap();
    assert_eq!(second.chunks, first.chunks);
    assert_eq!(second.embedded_chunks, 0);
    assert_eq!(second.reused_chunks, second.chunks);
    assert_eq!(fake.documents.load(Ordering::SeqCst), documents_before);

    // The same code under a new path is a new document: it is embedded
    // again, not served the vector computed for the first path.
    write(&workspace, "translator_copy.rs", "fn alpha() {}\n");
    let third = app.index(&workspace).await.unwrap();
    let copied = third.chunks - second.chunks;
    assert!(copied > 0);
    assert_eq!(third.embedded_chunks, copied);
    assert_eq!(third.reused_chunks, second.chunks);
    assert_eq!(
        fake.documents.load(Ordering::SeqCst) - documents_before,
        copied
    );
    let scores = translator_semantic_scores(&app, &workspace).await;
    assert!(
        (scores["translator_copy.rs"] - 1.0).abs() < 0.001,
        "{scores:?}"
    );
    assert!(
        (scores["translator_notes.rs"] - 1.0).abs() < 0.001,
        "{scores:?}"
    );
    assert!(scores["lib.rs"] < 0.5, "{scores:?}");

    // Only the two searches above were classified as queries.
    assert_eq!(fake.queries.load(Ordering::SeqCst), 2);
    task.abort();
}

/// The semantic score of the chunk a one-result "translator" search ranks
/// first, and the search warning. `None` means semantic retrieval served
/// nothing, for example because the index vectors have another width.
async fn translator_semantic_score(app: &App, workspace: &Path) -> (Option<f32>, Option<String>) {
    let found = app.search(workspace, "translator", Some(1)).await.unwrap();
    let value = serde_json::to_value(&found).unwrap();
    let warning = value["warning"].as_str().map(str::to_owned);
    (
        found.results.first().and_then(|hit| hit.semantic_score),
        warning,
    )
}

#[tokio::test]
async fn a_changed_embedding_width_under_the_same_identity_is_repaired_by_the_next_index_run() {
    let (temp, mut config, fake, task) = fixture().await;
    config.index.freshness = IndexFreshness::Manual;
    // No language server, so a search warning can only come from retrieval.
    config.channels.lsp = false;
    let workspace = temp.path().join("width-change");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    write(&workspace, "other.rs", "fn unrelated() {}\n");
    let app = App::open(config.clone()).await.unwrap();
    let first = app.index(&workspace).await.unwrap();
    assert_eq!(first.embedded_chunks, first.chunks);
    assert_eq!(
        app.status(&workspace).await.unwrap().embedding_dimension,
        Some(3)
    );
    drop(app);

    // A restart against a service that now returns 4-wide vectors under the
    // same URL and model name. Nothing changed on disk, so every chunk is a
    // vector-cache hit; reusing them would commit the old width again.
    fake.embedding_width.store(4, Ordering::SeqCst);
    let app = App::open(config.clone()).await.unwrap();
    let documents = fake.documents.load(Ordering::SeqCst);
    let restarted = app.index(&workspace).await.unwrap();
    assert_eq!(restarted.embedded_chunks, restarted.chunks);
    assert_eq!(restarted.reused_chunks, 0);
    // One probe document, then every chunk once.
    assert_eq!(
        fake.documents.load(Ordering::SeqCst) - documents,
        1 + restarted.chunks
    );
    assert_eq!(
        app.status(&workspace).await.unwrap().embedding_dimension,
        Some(4)
    );
    let (score, warning) = translator_semantic_score(&app, &workspace).await;
    assert!((score.unwrap() - 1.0).abs() < 0.001, "{warning:?}");
    assert_eq!(warning, None);

    // In the running process the width changes again and one file changes:
    // the run that embeds it re-embeds the cached vectors of the old width.
    fake.embedding_width.store(5, Ordering::SeqCst);
    write(&workspace, "other.rs", "fn unrelated_changed() {}\n");
    let changed = app.index(&workspace).await.unwrap();
    assert_eq!(changed.embedded_chunks, changed.chunks);
    assert_eq!(changed.reused_chunks, 0);
    assert_eq!(
        app.status(&workspace).await.unwrap().embedding_dimension,
        Some(5)
    );
    let (score, warning) = translator_semantic_score(&app, &workspace).await;
    assert!((score.unwrap() - 1.0).abs() < 0.001, "{warning:?}");

    // A search that meets another width warns and serves no semantic hits;
    // the index run it asks for repairs the index with nothing changed.
    fake.embedding_width.store(6, Ordering::SeqCst);
    let (score, warning) = translator_semantic_score(&app, &workspace).await;
    assert_eq!(score, None);
    assert!(
        warning
            .as_deref()
            .is_some_and(|w| w.contains("dimension differs from index")),
        "{warning:?}"
    );
    let repaired = app.index(&workspace).await.unwrap();
    assert_eq!(repaired.embedded_chunks, repaired.chunks);
    assert_eq!(
        app.status(&workspace).await.unwrap().embedding_dimension,
        Some(6)
    );
    let (score, warning) = translator_semantic_score(&app, &workspace).await;
    assert!((score.unwrap() - 1.0).abs() < 0.001, "{warning:?}");

    // An unchanged index run with the width unchanged still embeds nothing.
    let documents = fake.documents.load(Ordering::SeqCst);
    let unchanged = app.index(&workspace).await.unwrap();
    assert_eq!(unchanged.embedded_chunks, 0);
    assert_eq!(unchanged.reused_chunks, unchanged.chunks);
    assert_eq!(fake.documents.load(Ordering::SeqCst), documents);
    drop(app);

    // After a restart with the service down, the failed probe only warns:
    // the unchanged run reuses every cached vector, as it did before probes.
    fake.embed_fails.store(true, Ordering::SeqCst);
    let app = App::open(config).await.unwrap();
    let offline = app.index(&workspace).await.unwrap();
    assert_eq!(offline.embedded_chunks, 0);
    assert_eq!(offline.reused_chunks, offline.chunks);
    assert_eq!(
        app.status(&workspace).await.unwrap().embedding_dimension,
        Some(6)
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
    assert_eq!(reopened.parsed_files, 1);
    assert_eq!(reopened.unchanged_files, 0);
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
async fn c_and_cpp_header_and_source_extensions_reuse_update_delete_and_keep_the_snapshot() {
    use local_code_intelligence::filter::SourceRole;

    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("c-family");
    write(
        &workspace,
        "src/cache.h",
        "// Evicts the least-recently-used entry once the cache exceeds its capacity.\nint evict_cached_entry(int *usage, int count, int capacity);\n",
    );
    write(
        &workspace,
        "src/throttle.cc",
        "// Determines whether a new request should be admitted.\nbool admit_throttled_request(int recent, int limit) {\n    return recent < limit;\n}\n",
    );
    write(
        &workspace,
        "src/throttle_unittest.cc",
        "// Search decoy: admit_throttled_request is mentioned only in a test.\nconst char *expected_throttle_description = \"admit requests under the limit\";\n",
    );
    write(
        &workspace,
        "src/window.cxx",
        "// Counts requests inside a sliding window.\nint count_window_requests(int start, int end) {\n    return end - start;\n}\n",
    );
    write(
        &workspace,
        "include/window.hh",
        "// Declares the sliding window counter.\nint count_window_requests(int start, int end);\n",
    );
    write(
        &workspace,
        "include/window.hxx",
        "// Resets the sliding window counter.\nvoid reset_window_counter();\n",
    );

    let app = App::open(config.clone()).await.unwrap();
    let first = app.index(&workspace).await.unwrap();
    assert_eq!(first.files, 6);
    assert_eq!(first.parsed_files, 6);
    assert!(first.embedded_chunks > 0);

    for (query, path, language) in [
        ("evict_cached_entry", "src/cache.h", "c"),
        ("admit_throttled_request", "src/throttle.cc", "cpp"),
        ("count_window_requests", "src/window.cxx", "cpp"),
        ("count_window_requests", "include/window.hh", "cpp"),
        ("reset_window_counter", "include/window.hxx", "cpp"),
    ] {
        let report = app.search(&workspace, query, Some(8)).await.unwrap();
        let hit = report
            .results
            .iter()
            .find(|hit| hit.chunk.relative_file_path == path)
            .unwrap_or_else(|| panic!("missing {path} for {query}: {:#?}", report.results));
        assert_eq!(hit.chunk.language, language, "{path}");
        assert_eq!(hit.source_role, SourceRole::Source, "{path}");
        assert!(hit.chunk.start_line >= 1 && hit.chunk.end_line >= hit.chunk.start_line);
    }
    // A `*_unittest.cc` file is a test. (The fake model service does not rank
    // by meaning, so only the metadata is asserted here; the live
    // multilingual acceptance ranks the production file above its decoy.)
    let throttle = app
        .search(&workspace, "admit_throttled_request", Some(8))
        .await
        .unwrap();
    assert!(throttle.results.iter().any(|hit| {
        hit.chunk.relative_file_path == "src/throttle.cc" && hit.source_role == SourceRole::Source
    }));
    assert!(throttle.results.iter().any(|hit| {
        hit.chunk.relative_file_path == "src/throttle_unittest.cc"
            && hit.chunk.language == "cpp"
            && hit.source_role == SourceRole::Test
    }));
    // Role filters see the new extensions, and a language filter groups `.h`
    // with `c` and the other extensions with `cpp`.
    let only_c = app
        .search_with_filters(
            &workspace,
            "evict_cached_entry",
            Some(8),
            local_code_intelligence::filter::FilterRequest {
                languages: Some(vec!["c".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(!only_c.results.is_empty());
    assert!(
        only_c
            .results
            .iter()
            .all(|hit| hit.chunk.language == "c" && hit.chunk.relative_file_path == "src/cache.h")
    );

    // Zero work on a repeat, in the same process and after a restart.
    let unchanged = app.index(&workspace).await.unwrap();
    assert_eq!(unchanged.parsed_files, 0);
    assert_eq!(unchanged.embedded_chunks, 0);
    drop(app);
    let app = App::open(config).await.unwrap();
    let restarted = app.index(&workspace).await.unwrap();
    assert_eq!(restarted.parsed_files, 0);
    assert_eq!(restarted.embedded_chunks, 0);
    assert_eq!(restarted.unchanged_files, 6);

    // A one-file update reparses that file and reuses every other vector.
    write(
        &workspace,
        "include/window.hxx",
        "// Resets the sliding window counter.\nvoid reset_window_counter();\n\n// Reports the window size.\nint window_size_limit();\n",
    );
    let changed = app.index(&workspace).await.unwrap();
    assert_eq!(changed.parsed_files, 1);
    assert_eq!(changed.unchanged_files, 5);
    assert!(changed.reused_chunks > 0);
    let updated = app
        .search(&workspace, "window_size_limit", Some(8))
        .await
        .unwrap();
    assert!(updated.results.iter().any(|hit| {
        hit.chunk.relative_file_path == "include/window.hxx"
            && hit.chunk.code.contains("window_size_limit")
    }));

    // Deleting the decoy removes its chunks from the next snapshot.
    std::fs::remove_file(workspace.join("src/throttle_unittest.cc")).unwrap();
    let deleted = app.index(&workspace).await.unwrap();
    assert_eq!(deleted.removed_files, 1);
    assert_eq!(deleted.files, 5);
    let after_delete = app
        .search(&workspace, "expected_throttle_description", Some(8))
        .await
        .unwrap();
    assert!(
        after_delete
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path != "src/throttle_unittest.cc")
    );

    // A failed update keeps the previous snapshot searchable.
    write(
        &workspace,
        "src/cache.h",
        "// Evicts the least-recently-used entry.\nint evict_cached_entry(int *usage, int count, int capacity);\nint UNCOMMITTED_SNAPSHOT_MARKER(void);\n",
    );
    fake.embed_fails.store(true, Ordering::SeqCst);
    assert!(app.index(&workspace).await.is_err());
    assert_eq!(app.status(&workspace).await.unwrap().chunks, deleted.chunks);
    assert!(app.status(&workspace).await.unwrap().stale);
    fake.embed_fails.store(false, Ordering::SeqCst);
    let retained = app
        .search(&workspace, "evict_cached_entry", Some(8))
        .await
        .unwrap();
    let header = retained
        .results
        .iter()
        .find(|hit| hit.chunk.relative_file_path == "src/cache.h")
        .unwrap();
    assert!(header.chunk.code.contains("evict_cached_entry"));
    assert!(!header.chunk.code.contains("UNCOMMITTED_SNAPSHOT_MARKER"));
    task.abort();
}

#[tokio::test]
async fn markdown_is_indexed_as_documentation_and_reuses_updates_deletes_and_keeps_the_snapshot() {
    use local_code_intelligence::filter::{FilterRequest, SourceRole};

    let (temp, mut config, fake, task) = fixture().await;
    config.docs_lane.top_k = 10;
    let workspace = temp.path().join("docs-repo");
    let guide = "# Guide\nHow to run the quarantine_policy checker.\n\n## Install\nInstall steps for the checker.\n\n## Usage\nRun the checker nightly.\n";
    write(
        &workspace,
        "README.md",
        "# Readme\nOverview of the repository.\n\n## Layout\nSources live in src.\n",
    );
    write(&workspace, "docs/guide.md", guide);
    // Documentation in a tests directory is still documentation.
    write(
        &workspace,
        "tests/NOTES.md",
        "# Notes\nHow the tests are organised.\n",
    );
    write(
        &workspace,
        "src/lib.rs",
        "pub fn quarantine_policy_checker() -> u32 {\n    1\n}\n",
    );
    // Other prose extensions are not indexed (a case variant of `.md` is,
    // N94: `chunk::tests`), and the shared ignore files apply to Markdown
    // like to code.
    write(&workspace, "notes.markdown", "# Long\nnot indexed\n");
    write(&workspace, "page.mdx", "# Mdx\nnot indexed\n");
    write(&workspace, ".gitignore", "built/\n");
    write(
        &workspace,
        "built/out.md",
        "# Built\nignored by gitignore\n",
    );
    write(&workspace, ".lciignore", "private/**\n");
    write(
        &workspace,
        "private/secret.md",
        "# Secret\nignored by lciignore\n",
    );

    let app = App::open(config.clone()).await.unwrap();
    let first = app.index(&workspace).await.unwrap();
    assert_eq!(first.files, 4);
    assert_eq!(first.parsed_files, 4);
    // README: 2 sections, guide: 3, notes: 1, lib.rs: 1.
    assert_eq!(first.chunks, 7);
    assert_eq!(first.embedded_chunks, 7);
    assert_eq!(fake.documents.load(Ordering::SeqCst), 7);
    let indexed = app.indexed_files(&workspace).await.unwrap();
    assert_eq!(
        indexed_paths(&indexed),
        [
            ("README.md", "markdown"),
            ("docs/guide.md", "markdown"),
            ("src/lib.rs", "rust"),
            ("tests/NOTES.md", "markdown"),
        ]
    );

    // Hits carry the language and the Documentation role, in any directory.
    let found = app
        .search(&workspace, "quarantine_policy", Some(10))
        .await
        .unwrap();
    // Markdown never enters the code list; it comes back in `docs`.
    assert!(
        found
            .results
            .iter()
            .all(|hit| hit.chunk.language != "markdown")
    );
    let docs = found.docs.as_ref().expect("the index holds Markdown");
    assert!(
        docs.results
            .iter()
            .all(|hit| hit.chunk.language == "markdown")
    );
    let hits: std::collections::BTreeMap<_, _> = found
        .results
        .iter()
        .chain(docs.results.iter())
        .map(|hit| {
            (
                (hit.chunk.relative_file_path.as_str(), hit.chunk.start_line),
                (hit.chunk.language.as_str(), hit.source_role),
            )
        })
        .collect();
    assert_eq!(
        hits[&("docs/guide.md", 1)],
        ("markdown", SourceRole::Documentation)
    );
    assert_eq!(hits[&("src/lib.rs", 1)], ("rust", SourceRole::Source));
    assert_eq!(
        hits[&("tests/NOTES.md", 1)],
        ("markdown", SourceRole::Documentation)
    );
    assert!(found.results.iter().chain(docs.results.iter()).all(|hit| {
        !hit.chunk.relative_file_path.contains("secret")
            && !hit.chunk.relative_file_path.contains("built")
    }));

    // `markdown` is a language filter value, and the role filter agrees.
    for request in [
        FilterRequest {
            languages: Some(vec!["markdown".into()]),
            ..Default::default()
        },
        FilterRequest {
            source_roles: Some(vec!["documentation".into()]),
            ..Default::default()
        },
    ] {
        let only_docs = app
            .search_with_filters(&workspace, "quarantine_policy", Some(10), request)
            .await
            .unwrap();
        assert!(!only_docs.results.is_empty());
        // The request names documentation: one list, no split.
        assert!(only_docs.docs.is_none());
        assert!(only_docs.results.iter().all(|hit| {
            hit.chunk.language == "markdown" && hit.source_role == SourceRole::Documentation
        }));
    }
    let only_code = app
        .search_with_filters(
            &workspace,
            "quarantine_policy",
            Some(10),
            FilterRequest {
                languages: Some(vec!["rust".into()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        only_code
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path == "src/lib.rs")
    );

    // Zero work on a repeat, in the same process and after a restart.
    let unchanged = app.index(&workspace).await.unwrap();
    assert_eq!(unchanged.parsed_files, 0);
    assert_eq!(unchanged.embedded_chunks, 0);
    drop(app);
    let app = App::open(config).await.unwrap();
    let restarted = app.index(&workspace).await.unwrap();
    assert_eq!(restarted.parsed_files, 0);
    assert_eq!(restarted.embedded_chunks, 0);
    assert_eq!(restarted.unchanged_files, 4);

    // A one-file update reparses that file and re-embeds only the section
    // that changed.
    write(
        &workspace,
        "docs/guide.md",
        &guide.replace("Run the checker nightly.", "Run the checker hourly."),
    );
    let changed = app.index(&workspace).await.unwrap();
    assert_eq!(changed.parsed_files, 1);
    assert_eq!(changed.unchanged_files, 3);
    assert_eq!(changed.embedded_chunks, 1);
    assert_eq!(changed.reused_chunks, 6);
    let updated = app
        .search(&workspace, "checker hourly", Some(10))
        .await
        .unwrap();
    assert!(
        updated
            .docs
            .iter()
            .flat_map(|lane| &lane.results)
            .any(|hit| {
                hit.chunk.relative_file_path == "docs/guide.md"
                    && hit.chunk.start_line == 7
                    && hit.chunk.code.contains("hourly")
            })
    );

    // Deleting a Markdown file removes its chunks from the next snapshot.
    std::fs::remove_file(workspace.join("tests/NOTES.md")).unwrap();
    let deleted = app.index(&workspace).await.unwrap();
    assert_eq!(deleted.removed_files, 1);
    assert_eq!(deleted.files, 3);
    assert_eq!(deleted.chunks, 6);

    // A failed update keeps the previous snapshot searchable.
    write(
        &workspace,
        "README.md",
        "# Readme\nOverview of the repository.\n\n## Layout\nSources live in src.\n\n## Uncommitted\nUNCOMMITTED_SNAPSHOT_MARKER\n",
    );
    fake.embed_fails.store(true, Ordering::SeqCst);
    assert!(app.index(&workspace).await.is_err());
    assert_eq!(app.status(&workspace).await.unwrap().chunks, deleted.chunks);
    assert!(app.status(&workspace).await.unwrap().stale);
    fake.embed_fails.store(false, Ordering::SeqCst);
    let retained = app
        .search(&workspace, "Overview of the repository", Some(10))
        .await
        .unwrap();
    let retained_docs = retained.docs.as_ref().expect("the snapshot holds Markdown");
    let readme = retained_docs
        .results
        .iter()
        .find(|hit| hit.chunk.relative_file_path == "README.md")
        .unwrap();
    assert!(readme.chunk.code.contains("Overview"));
    assert!(
        retained
            .results
            .iter()
            .chain(retained_docs.results.iter())
            .all(|hit| !hit.chunk.code.contains("UNCOMMITTED_SNAPSHOT_MARKER"))
    );
    task.abort();
}

/// A workspace of code files, and the same files plus Markdown that matches
/// the queries below better than any code does.
fn write_code_and_prose_workspaces(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
    let code = [
        (
            "src/lib.rs",
            "pub fn translator() -> u32 {\n    1\n}\n\npub fn helper() -> u32 {\n    2\n}\n",
        ),
        (
            "src/app.ts",
            "export function translatorFactory() {\n  return 1;\n}\n",
        ),
        ("tools/run.py", "def run_translator():\n    return 3\n"),
        ("src/other.rs", "pub fn unrelated() -> u32 {\n    4\n}\n"),
    ];
    let prose = [
        (
            "README.md",
            "# Readme\nThe translator overview.\n\n## Layout\ntranslator translator translator in src.\n",
        ),
        (
            "docs/guide.md",
            "# Guide\nHow to run the translator.\n\n## Usage\nUse the translator daily, translator again.\n",
        ),
        ("docs/more.md", "# More\nAnother translator note.\n"),
    ];
    let plain = root.join("plain");
    let mixed = root.join("mixed");
    for (name, body) in code {
        write(&plain, name, body);
        write(&mixed, name, body);
    }
    for (name, body) in prose {
        write(&mixed, name, body);
    }
    (plain, mixed)
}

/// The serialized code `results` of a report, byte for byte, except for the
/// absolute `file_path` (it names the workspace directory, which differs
/// between the two fixtures; `relative_file_path` stays).
fn code_list_json(report: &local_code_intelligence::app::SearchReport) -> String {
    let mut results = serde_json::to_value(&report.results).unwrap();
    for hit in results.as_array_mut().unwrap() {
        hit.as_object_mut().unwrap().remove("file_path");
    }
    results.to_string()
}

#[tokio::test]
async fn the_code_results_are_byte_identical_with_or_without_markdown_and_docs_come_separately() {
    use local_code_intelligence::{app::SearchOptions, filter::FilterRequest};

    let (temp, config, fake, task) = fixture().await;
    let (plain, mixed) = write_code_and_prose_workspaces(temp.path());
    let app = App::open(config).await.unwrap();
    for rerank_fails in [false, true] {
        fake.rerank_fails.store(rerank_fails, Ordering::SeqCst);
        for query in ["translator", "run translator daily", "unrelated helper"] {
            let without = app.search(&plain, query, Some(5)).await.unwrap();
            let with = app.search(&mixed, query, Some(5)).await.unwrap();
            assert_eq!(
                code_list_json(&with),
                code_list_json(&without),
                "code results differ for {query:?} (rerank_fails={rerank_fails})"
            );
            assert_eq!(with.warning, without.warning);
            assert_eq!(with.reranked, without.reranked);
            assert_eq!(with.candidate_count, without.candidate_count);
            assert_eq!(with.notes, without.notes);
            // The traced candidate pool of the code lane is the one the plain
            // index produces too (ranks, scores, order).
            let traced = |path: &Path| {
                let app = &app;
                let path = path.to_owned();
                async move {
                    let report = app
                        .search_with_options(
                            &path,
                            query,
                            Some(5),
                            FilterRequest::default(),
                            SearchOptions {
                                trace_candidates: true,
                            },
                        )
                        .await
                        .unwrap();
                    (
                        serde_json::to_value(report.candidates.as_ref().unwrap()).unwrap(),
                        serde_json::to_value(report.rerank_outcome).unwrap(),
                    )
                }
            };
            assert_eq!(
                traced(&mixed).await,
                traced(&plain).await,
                "candidate pool differs for {query:?} (rerank_fails={rerank_fails})"
            );
            assert!(without.docs.is_none(), "no Markdown, no docs list");
            assert!(
                with.results
                    .iter()
                    .all(|hit| hit.chunk.language != "markdown")
            );

            let docs = with.docs.as_ref().expect("the lane ran over Markdown");
            assert!(!docs.results.is_empty());
            assert!(docs.results.len() <= 2, "the default docs list is two");
            assert!(
                docs.results
                    .iter()
                    .all(|hit| hit.chunk.language == "markdown")
            );
            assert_eq!(docs.reranked, !rerank_fails);
            // Fail-open: the failure shows on the lane only, as the code
            // list's own warning does for the code reranker.
            assert_eq!(docs.warning.is_some(), rerank_fails);
        }
    }
    fake.rerank_fails.store(false, Ordering::SeqCst);

    // Only an explicit role filter folds Markdown back into one list.
    for roles in [vec!["documentation"], vec!["source", "documentation"]] {
        let one_list = app
            .search_with_filters(
                &mixed,
                "translator",
                Some(10),
                FilterRequest {
                    source_roles: Some(roles.iter().map(|role| role.to_string()).collect()),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        assert!(one_list.docs.is_none());
        assert!(
            one_list
                .results
                .iter()
                .any(|hit| hit.chunk.language == "markdown")
        );
    }
    let code_only_role = app
        .search_with_filters(
            &mixed,
            "translator",
            Some(10),
            FilterRequest {
                source_roles: Some(vec!["source".to_string()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(code_only_role.docs.is_none());
    assert!(
        code_only_role
            .results
            .iter()
            .all(|hit| hit.chunk.language != "markdown")
    );
    task.abort();
}

/// The semantic fetch widens until it has `semantic_candidate_count` hits
/// even above its 160-row first fetch, so the code lane of a mixed index (which
/// refetches past the nearer prose) and the plain index agree there too.
#[tokio::test]
async fn the_code_results_stay_identical_at_the_largest_semantic_candidate_count() {
    let (temp, mut config, _fake, task) = fixture().await;
    config.semantic_candidate_count = 200;
    let (plain, mixed) = write_code_and_prose_workspaces(temp.path());
    // 170 more code files, none of which the fake embedder places near the
    // query, so the Markdown chunks are the nearest rows of the mixed index.
    for index in 0..170 {
        let body = format!("pub fn generated_{index}() -> u32 {{\n    {index}\n}}\n");
        write(&plain, &format!("src/gen/f{index}.rs"), &body);
        write(&mixed, &format!("src/gen/f{index}.rs"), &body);
    }
    let app = App::open(config).await.unwrap();
    let without = app.search(&plain, "translator", Some(3)).await.unwrap();
    let with = app.search(&mixed, "translator", Some(3)).await.unwrap();
    // The fused pool passes the 160-row first fetch only if the semantic
    // channel returned more than 160 hits.
    let traced = app
        .search_with_options(
            &plain,
            "translator",
            Some(3),
            Default::default(),
            local_code_intelligence::app::SearchOptions {
                trace_candidates: true,
            },
        )
        .await
        .unwrap();
    assert!(traced.candidates.unwrap().len() > 160);
    assert_eq!(code_list_json(&with), code_list_json(&without));
    assert_eq!(with.candidate_count, without.candidate_count);
    assert_eq!(with.warning, without.warning);
    assert_eq!(with.notes, without.notes);
    assert!(with.docs.is_some());
    task.abort();
}

#[tokio::test]
async fn by_default_markdown_is_not_indexed_and_the_results_equal_a_workspace_without_it() {
    use local_code_intelligence::filter::FilterRequest;

    let (temp, mut config, _fake, task) = fixture().await;
    // The shipped defaults (the fixture opts into prose and docs reranking).
    let defaults = Config::default();
    assert!(!defaults.index.prose && !defaults.docs_lane.reranker);
    config.index.prose = defaults.index.prose;
    config.docs_lane.reranker = defaults.docs_lane.reranker;
    let (plain, mixed) = write_code_and_prose_workspaces(temp.path());
    let app = App::open(config).await.unwrap();
    for query in ["translator", "run translator daily", "unrelated helper"] {
        let without = app.search(&plain, query, Some(5)).await.unwrap();
        let with = app.search(&mixed, query, Some(5)).await.unwrap();
        assert_eq!(code_list_json(&with), code_list_json(&without), "{query}");
        assert_eq!(with.warning, without.warning);
        assert_eq!(with.reranked, without.reranked);
        assert_eq!(with.candidate_count, without.candidate_count);
        assert_eq!(with.notes, without.notes);
        assert!(with.docs.is_none() && without.docs.is_none(), "{query}");
        assert!(serde_json::to_value(&with).unwrap().get("docs").is_none());
    }
    // No Markdown chunk exists: the index holds the plain workspace's chunks,
    // and even an explicit documentation filter finds nothing.
    assert_eq!(
        app.status(&mixed).await.unwrap().chunks,
        app.status(&plain).await.unwrap().chunks
    );
    let manifest = app.indexed_files(&mixed).await.unwrap();
    assert!(!manifest.files.is_empty());
    assert!(
        manifest
            .files
            .iter()
            .all(|file| !lci_core::path::has_extension_ignore_case(&file.relative_file_path, "md"))
    );
    let role = app
        .search_with_filters(
            &mixed,
            "translator",
            Some(10),
            FilterRequest {
                source_roles: Some(vec!["documentation".to_string()]),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert!(
        role.results
            .iter()
            .all(|hit| hit.chunk.language != "markdown")
    );
    assert!(role.docs.is_none());
    task.abort();
}

#[tokio::test]
async fn the_prose_switch_adds_and_drops_exactly_the_markdown_files_on_the_next_refresh() {
    let (temp, mut config, _fake, task) = fixture().await;
    let (_plain, mixed) = write_code_and_prose_workspaces(temp.path());

    config.index.prose = false;
    let app = App::open(config.clone()).await.unwrap();
    let off = app.search(&mixed, "translator", Some(5)).await.unwrap();
    assert!(off.docs.is_none());
    let code_chunks = app.status(&mixed).await.unwrap().chunks;
    drop(app);

    // The switch is part of what staleness compares: the snapshot is stale
    // under the new setting, the search refreshes it incrementally (no full
    // rebuild: the code files are reused), and the docs lane appears.
    config.index.prose = true;
    let app = App::open(config.clone()).await.unwrap();
    assert!(app.status(&mixed).await.unwrap().stale);
    let on = app.search(&mixed, "translator", Some(5)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&on.index).unwrap()["action"],
        "refreshed_incrementally"
    );
    assert!(
        on.docs
            .as_ref()
            .is_some_and(|docs| !docs.results.is_empty())
    );
    assert_eq!(code_list_json(&on), code_list_json(&off));
    let prose_chunks = app.status(&mixed).await.unwrap().chunks;
    assert!(prose_chunks > code_chunks);
    drop(app);

    config.index.prose = false;
    let app = App::open(config).await.unwrap();
    assert!(app.status(&mixed).await.unwrap().stale);
    let off_again = app.search(&mixed, "translator", Some(5)).await.unwrap();
    assert!(off_again.docs.is_none());
    assert_eq!(app.status(&mixed).await.unwrap().chunks, code_chunks);
    assert_eq!(code_list_json(&off_again), code_list_json(&off));
    task.abort();
}

#[tokio::test]
async fn a_failing_docs_reranker_is_reported_on_the_docs_list_and_keeps_fusion_order() {
    let (temp, config, fake, task) = fixture().await;
    let (plain, mixed) = write_code_and_prose_workspaces(temp.path());
    let app = App::open(config).await.unwrap();
    // The fake answers an out-of-range index to both rerank calls.
    fake.malformed_rerank.store(true, Ordering::SeqCst);
    let without = app.search(&plain, "translator", Some(5)).await.unwrap();
    let with = app.search(&mixed, "translator", Some(5)).await.unwrap();
    assert_eq!(code_list_json(&with), code_list_json(&without));
    assert_eq!(with.warning, without.warning);
    assert!(without.warning.is_some());
    let docs = with.docs.as_ref().unwrap();
    assert!(!docs.reranked);
    assert!(!docs.results.is_empty());
    assert!(docs.warning.as_deref().unwrap().contains("Docs reranker"));
    task.abort();
}

#[tokio::test]
async fn the_docs_lane_can_be_turned_off_and_still_keeps_markdown_out_of_the_code_list() {
    let (temp, mut config, _fake, task) = fixture().await;
    let (plain, mixed) = write_code_and_prose_workspaces(temp.path());
    for (enabled, top_k) in [(false, 2), (true, 0)] {
        config.docs_lane.enabled = enabled;
        config.docs_lane.top_k = top_k;
        let app = App::open(config.clone()).await.unwrap();
        let without = app.search(&plain, "translator", Some(5)).await.unwrap();
        let with = app.search(&mixed, "translator", Some(5)).await.unwrap();
        assert_eq!(code_list_json(&with), code_list_json(&without));
        assert!(with.docs.is_none());
    }
    task.abort();
}

#[tokio::test]
async fn evaluation_scores_a_query_on_the_lane_its_tag_names_and_counts_both_lists_in_tokens() {
    use lci_core::evaluation::EvalLane;
    use local_code_intelligence::evaluate::run_evaluation;

    let (temp, config, _fake, task) = fixture().await;
    let (_plain, mixed) = write_code_and_prose_workspaces(temp.path());
    let app = App::open(config).await.unwrap();
    let query = |tags: &[&str], expected: &str| EvalQuery {
        query: "translator".to_owned(),
        workspace: "mixed".to_owned(),
        top_k: Some(5),
        filters: None,
        expected_paths: expected.split(',').map(str::to_owned).collect(),
        preferred_paths: Vec::new(),
        disfavored_paths: Vec::new(),
        expected_roles: Vec::new(),
        required_snippet: None,
        tags: tags.iter().map(|tag| (*tag).to_owned()).collect(),
    };
    let definition = EvaluationFile {
        workspaces: vec![EvalWorkspace {
            key: "mixed".to_owned(),
            required: true,
        }],
        queries: vec![
            query(&[], "src/lib.rs"),
            query(
                &[EvalLane::DOCS_TAG],
                "docs/guide.md,README.md,docs/more.md",
            ),
            query(
                &[EvalLane::UNION_TAG],
                "docs/guide.md,README.md,docs/more.md",
            ),
            // A code query that names a document finds nothing: Markdown is
            // not in the code list.
            query(&[], "docs/guide.md"),
        ],
    };
    let mappings = parse_workspace_mappings(&[format!("mixed={}", mixed.display())]).unwrap();
    let report = run_evaluation(&app, &definition, &mappings).await.unwrap();
    let [code, docs, union, code_for_doc] = &report.queries[..] else {
        panic!("four queries")
    };

    assert!(code.metrics.hit, "{:?}", code.evidence);
    assert!(code.evidence.iter().all(|row| row.language != "markdown"));
    assert!(!code.unscored_evidence.is_empty());
    assert!(
        code.unscored_evidence
            .iter()
            .all(|row| row.language == "markdown")
    );

    assert!(docs.metrics.hit, "{:?}", docs.evidence);
    assert!(docs.evidence.iter().all(|row| row.language == "markdown"));
    assert!(docs.evidence.len() <= 2);
    // The default docs list is two long: a query asking for five says so.
    assert_eq!(docs.notes.len(), 1, "{:?}", docs.notes);
    assert!(docs.notes[0].contains("docs_lane"), "{:?}", docs.notes);
    assert!(code.notes.is_empty());
    assert!(!docs.unscored_evidence.is_empty());

    // The union is the code list, then the docs list, renumbered.
    assert_eq!(
        union.evidence.len(),
        code.evidence.len() + docs.evidence.len()
    );
    assert!(union.unscored_evidence.is_empty());
    assert!(union.metrics.hit);
    assert!(
        union
            .evidence
            .iter()
            .enumerate()
            .all(|(index, row)| row.rank == index + 1)
    );

    assert!(!code_for_doc.metrics.hit);

    // The token proxy counts both lists the call returned, whatever the lane.
    assert_eq!(code.metrics.returned_tokens, docs.metrics.returned_tokens);
    assert_eq!(code.metrics.returned_tokens, union.metrics.returned_tokens);
    assert!(code.metrics.returned_tokens.unwrap() > 0);
    assert!(report.mean_returned_tokens.unwrap() > 0.0);
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
    // A port that refuses connections, held for the whole test.
    let closed = ClosedPort::new();
    config.reranker_url = format!("http://127.0.0.1:{}/rerank", closed.port);
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
async fn a_refused_reranker_url_credential_is_not_reported() {
    let (temp, mut config, _fake, task) = fixture().await;
    // A port that refuses connections, held for the whole test.
    let closed = ClosedPort::new();
    config.reranker_url = format!("http://127.0.0.1:{}/rerank?api_key=marker", closed.port);
    let workspace = temp.path().join("reranker-url-credential");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app.search(&workspace, "translator", Some(1)).await.unwrap();

    let warning = report.warning.as_deref().unwrap();
    assert!(warning.contains("reranker request failed"), "{warning}");
    assert!(!warning.contains("marker"), "{warning}");
    task.abort();
}

#[tokio::test]
async fn a_reranker_http_error_body_credential_is_not_reported() {
    let (temp, mut config, _fake, task) = fixture().await;
    let router = Router::new().route(
        "/rerank",
        post(|uri: axum::http::Uri| async move {
            assert_eq!(
                uri.path_and_query().unwrap().as_str(),
                "/rerank?api_key=n89-marker"
            );
            (
                StatusCode::UNAUTHORIZED,
                format!("rejected {}; retry later", uri.path_and_query().unwrap()),
            )
        }),
    );
    let listener = loopback_tokio_listener();
    let address = listener.local_addr().unwrap();
    let reranker_task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    config.reranker_url = format!("http://{address}/rerank?api_key=n89-marker");
    let workspace = temp.path().join("reranker-body-credential");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app.search(&workspace, "translator", Some(1)).await.unwrap();

    assert!(!report.reranked);
    assert!(
        !report.results.is_empty(),
        "reranker failure must fail open"
    );
    let warning = report.warning.as_deref().unwrap();
    assert!(
        warning.contains(
            "Reranker unavailable or invalid response; fusion-ranked results returned: reranker HTTP error: HTTP status 401: rejected /rerank?api_key=[redacted]; retry later"
        ),
        "{warning}"
    );
    assert!(!warning.contains("n89-marker"), "{warning}");
    reranker_task.abort();
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

#[tokio::test]
async fn legacy_long_table_name_is_searchable_and_migrated_on_refresh() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("legacy-name");
    write(
        &workspace,
        "lib.rs",
        "fn translator() {}\nfn helper_one() {}\n",
    );
    let app = App::open(config.clone()).await.unwrap();
    let report = app.index(&workspace).await.unwrap();
    assert_eq!(report.chunks, 2);
    drop(app);

    // An index an earlier version wrote: the same Lance table under the
    // full-id name. Renaming the table directory reproduces it.
    let resolved =
        local_code_intelligence::workspace::Workspace::resolve(&workspace, &config.data_dir)
            .unwrap();
    let lancedb = config.data_dir.join("lancedb");
    let short = lancedb.join(format!("{}.lance", resolved.table_name()));
    let legacy = lancedb.join(format!("{}.lance", resolved.legacy_table_name()));
    assert!(short.is_dir(), "{}", short.display());
    std::fs::rename(&short, &legacy).unwrap();

    let app = App::open(config).await.unwrap();
    let status = app.status(&workspace).await.unwrap();
    assert!(status.indexed);
    assert_eq!(status.chunks, 2);
    assert!(status.compatible);

    let documents = fake.documents.load(Ordering::SeqCst);
    let found = app.search(&workspace, "translator", Some(1)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&found).unwrap()["index"]["action"],
        "reused"
    );
    assert!(found.results[0].chunk.code.contains("translator"));
    assert_eq!(fake.documents.load(Ordering::SeqCst), documents);
    assert!(legacy.is_dir(), "a read does not migrate");
    assert!(!short.exists());

    // The next commit moves the index to the short name and re-embeds only
    // what changed.
    write(
        &workspace,
        "lib.rs",
        "fn translator() {}\nfn helper_two() {}\n",
    );
    let refreshed = app.index(&workspace).await.unwrap();
    assert_eq!(refreshed.embedded_chunks, 1);
    assert_eq!(refreshed.reused_chunks, 1);
    assert!(short.is_dir());
    assert!(!legacy.exists());
    let indexed = app.indexed_files(&workspace).await.unwrap();
    let paths: Vec<_> = indexed
        .files
        .iter()
        .map(|f| f.relative_file_path.as_str())
        .collect();
    assert_eq!(paths, ["lib.rs"]);
    let after = app.search(&workspace, "helper_two", Some(2)).await.unwrap();
    assert!(
        after
            .results
            .iter()
            .any(|hit| hit.chunk.code.contains("helper_two"))
    );
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

#[tokio::test]
async fn default_channels_keep_every_channel_and_the_reranker_on() {
    let (temp, mut config, fake, task) = fixture().await;
    let workspace = temp.path().join("channels-default");
    write_channel_workspace(&workspace);
    let state = temp.path().join("lsp-state-default");
    // A fake server start (a debug-build process spawn under parallel tests)
    // can take longer than the 1500 ms default budget. These tests are about
    // channels, not the budget, so the budget must not be what they measure.
    // The other tests below that set 30 s do so for the same reason.
    config.lsp_search_budget_ms = 30_000;
    config = with_fake_csharp_server(config, &state, &[], LOADED_BUDGET_SECONDS);
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

/// Roadmap E0a. A traced search keeps the rank every channel gave a candidate
/// (one-based, as fusion consumed it), so the fusion score can be recomputed
/// from the dump with the documented arithmetic, bit for bit.
#[tokio::test]
async fn a_traced_search_records_the_rank_each_channel_gave_every_candidate() {
    let (temp, mut config, _fake, task) = fixture().await;
    let workspace = temp.path().join("channels-traced");
    write_channel_workspace(&workspace);
    let state = temp.path().join("lsp-state-traced");
    config.lsp_search_budget_ms = 30_000;
    config = with_fake_csharp_server(config, &state, &[], LOADED_BUDGET_SECONDS);
    let app = App::open(config.clone()).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app
        .search_with_options(
            &workspace,
            "Calculator",
            Some(10),
            local_code_intelligence::filter::FilterRequest::default(),
            local_code_intelligence::app::SearchOptions {
                trace_candidates: true,
            },
        )
        .await
        .unwrap();
    assert!(report.warning.is_none(), "{:?}", report.warning);
    assert_eq!(
        channels_seen(&report),
        channel_set(&["semantic", "lexical", "lsp"])
    );
    let rows = report.candidates.as_ref().expect("the pool was traced");
    assert!(!rows.is_empty());
    let all_three = rows
        .iter()
        .find(|row| {
            row.semantic_rank.is_some() && row.lexical_rank.is_some() && row.lsp_rank.is_some()
        })
        .unwrap_or_else(|| panic!("no candidate has a rank from every channel: {rows:?}"));
    assert!(all_three.semantic_score.is_some());
    assert!(all_three.lexical_score.is_some() && all_three.lexical_match_count > 0);
    assert_eq!(all_three.relative_file_path, "src/Calculator.cs");

    let (rrf_k, weights) = (config.rrf_k, &config.fusion);
    for row in rows {
        let mut score = 0.0_f32;
        if let Some(rank) = row.semantic_rank {
            score += weights.semantic_weight / (rrf_k + rank as f32);
        }
        if let Some(rank) = row.lexical_rank {
            score += weights.lexical_weight / (rrf_k + rank as f32);
        }
        if let Some(rank) = row.lsp_rank {
            score += weights.lsp_weight / (rrf_k + rank as f32);
        }
        assert_eq!(
            (score + row.prior).to_bits(),
            row.fusion_score.to_bits(),
            "{row:?}"
        );
    }
    // The ranks are what each hit in the results carries.
    for hit in &report.results {
        let row = rows
            .iter()
            .find(|row| {
                row.relative_file_path == hit.chunk.relative_file_path
                    && row.start_line == hit.chunk.start_line
            })
            .expect("a returned hit is in the dump");
        assert_eq!(row.semantic_rank, hit.semantic_rank);
        assert_eq!(row.lexical_rank, hit.lexical_rank);
        assert_eq!(row.lsp_rank, hit.lsp_rank);
    }
    task.abort();
}

/// Roadmap E0a. A language server that is still starting leaves a note on the
/// search and contributes nothing, which changes what a dumped pool means. The
/// dump keeps the note so a replay can exclude or stratify such queries; an
/// evaluation without the flag stays exactly as it was and carries none.
#[tokio::test]
async fn an_evaluation_dump_keeps_the_note_of_a_cold_language_server() {
    use local_code_intelligence::evaluate::{RunOptions, run_evaluation_with_options};

    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("dump-cold");
    write_channel_workspace(&workspace);
    let definition = EvaluationFile {
        workspaces: vec![EvalWorkspace {
            key: "cold".to_owned(),
            required: true,
        }],
        queries: vec![EvalQuery {
            query: "Calculator".to_owned(),
            workspace: "cold".to_owned(),
            top_k: Some(10),
            filters: None,
            expected_paths: vec!["src/Calculator.cs".to_owned()],
            preferred_paths: Vec::new(),
            disfavored_paths: Vec::new(),
            expected_roles: Vec::new(),
            required_snippet: None,
            tags: Vec::new(),
        }],
    };
    let mappings = parse_workspace_mappings(&[format!("cold={}", workspace.display())]).unwrap();

    // Each run has its own application, data directory, and language-server
    // state, so the server is still starting at its first search: it needs
    // 1.5 s for `initialize`, the search waits 200 ms.
    let mut reports = Vec::new();
    for (label, dump_candidates) in [("plain", false), ("dump", true)] {
        let mut config = config.clone();
        config.data_dir = temp.path().join(format!("data-{label}"));
        config.lsp_search_budget_ms = 200;
        config = with_fake_csharp_server(
            config,
            &temp.path().join(format!("lsp-{label}")),
            &["--initialize-delay-ms", "1500"],
            10,
        );
        let app = App::open(config).await.unwrap();
        app.index(&workspace).await.unwrap();
        let report = run_evaluation_with_options(
            &app,
            &definition,
            &mappings,
            // Priming off: this run is about the first query meeting a
            // server that is still starting.
            RunOptions {
                dump_candidates,
                prime: false,
            },
        )
        .await
        .unwrap();
        let query = &report.queries[0];
        assert!(
            query.timings.unwrap().lsp_search_ms >= 150.0,
            "{label}: the language-server channel waited out its budget: {:?}",
            query.timings
        );
        // A note is not a fallback.
        assert!(query.warning.is_none(), "{label}: {:?}", query.warning);
        assert_eq!(report.fallback_queries, 0);
        reports.push(report);
    }

    let plain = serde_json::to_value(&reports[0]).unwrap();
    assert!(plain["queries"][0].get("notes").is_none(), "{plain}");
    assert!(plain["queries"][0].get("candidates").is_none());

    let dumped = &reports[1].queries[0];
    assert_eq!(dumped.notes.len(), 1, "{:?}", dumped.notes);
    assert!(
        dumped.notes[0].contains("LSP still starting") && dumped.notes[0].contains("csharp-ls"),
        "{:?}",
        dumped.notes
    );
    assert!(
        dumped.candidates.iter().all(|row| row.lsp_rank.is_none()),
        "the cold server contributed no candidate"
    );
    assert!(!dumped.candidates.is_empty());
    let dumped_json = serde_json::to_value(&reports[1]).unwrap();
    assert_eq!(
        dumped_json["queries"][0]["notes"],
        json!([dumped.notes[0].clone()])
    );
    // The two runs agree on everything else.
    assert_eq!(reports[0].queries[0].metrics, dumped.metrics);
    task.abort();
}

fn spawn_count(state: &Path) -> u64 {
    std::fs::read_to_string(state.join("spawn_count.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

/// The note of a language server a search or a map run found running.
#[cfg(feature = "map-equivalence")]
const KEPT_CSHARP_LS: &str = "lsp: csharp-ls kept session; tree stamp unchanged since N s ago";

/// The note of a language server whose kept session the barrier stopped
/// because an edit changed the tree.
#[cfg(feature = "map-equivalence")]
const RESTARTED_CSHARP_LS: &str = "lsp: csharp-ls restarted: the tree changed since its last call";

/// `notes` with the seconds of a kept-session note replaced by `N`, which two
/// calls a moment apart need not agree on.
#[cfg(feature = "map-equivalence")]
fn masked_seconds(notes: &[String]) -> Vec<String> {
    notes
        .iter()
        .map(|note| match note.split_once("unchanged since ") {
            Some((head, tail)) => match tail.split_once(" s ago") {
                Some((seconds, rest))
                    if !seconds.is_empty() && seconds.bytes().all(|b| b.is_ascii_digit()) =>
                {
                    format!("{head}unchanged since N s ago{rest}")
                }
                _ => note.clone(),
            },
            None => note.clone(),
        })
        .collect()
}

/// Searches until one finishes without the "language server still starting"
/// note, i.e. until the server is up and primed. Bounded at about 20 s.
async fn search_until_the_language_server_answers(
    app: &App,
    workspace: &Path,
    query: &str,
) -> local_code_intelligence::app::SearchReport {
    for _ in 0..100 {
        let report = app.search(workspace, query, Some(10)).await.unwrap();
        if without_barrier_notes(&report.notes).is_empty() {
            return report;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the language server never finished starting");
}

#[tokio::test]
async fn a_cold_language_server_costs_a_search_only_its_budget_and_keeps_warming() {
    let (temp, mut config, _fake, task) = fixture().await;
    let workspace = temp.path().join("channels-cold");
    write_channel_workspace(&workspace);
    let state = temp.path().join("lsp-state-cold");
    config.lsp_search_budget_ms = 200;
    // The server needs 4 s to answer `initialize`, far past the budget; the
    // 3 s ceiling below still tells a capped wait from a full start under load.
    config = with_fake_csharp_server(config, &state, &["--initialize-delay-ms", "4000"], 10);
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let cold = app
        .search(&workspace, "Calculator", Some(10))
        .await
        .unwrap();
    assert!(
        (150.0..3000.0).contains(&cold.timings.lsp_search_ms),
        "the LSP channel waited {} ms: capped by the 200 ms budget, not the 4000 ms start",
        cold.timings.lsp_search_ms
    );
    assert!(!cold.results.is_empty());
    assert!(
        !channels_seen(&cold).contains("lsp"),
        "{:?}",
        channels_seen(&cold)
    );
    assert!(
        channels_seen(&cold).contains("semantic") && channels_seen(&cold).contains("lexical"),
        "the other channels still answer: {:?}",
        channels_seen(&cold)
    );
    // An unready server is a note, not a failure: no warning (the evaluation
    // harness counts warnings as fallbacks), and the search succeeds.
    assert!(cold.warning.is_none(), "{:?}", cold.warning);
    assert_eq!(cold.notes.len(), 1, "{:?}", cold.notes);
    assert!(
        cold.notes[0].contains("LSP still starting") && cold.notes[0].contains("csharp-ls"),
        "{:?}",
        cold.notes
    );
    assert!(
        serde_json::to_value(&cold).unwrap()["notes"].is_array(),
        "the note reaches the JSON report"
    );
    assert!(cold.reranked, "{:?}", cold.warning);

    // Giving up on the server did not discard it: the very same process
    // finishes starting (and priming), and a later search uses it.
    let warm = search_until_the_language_server_answers(&app, &workspace, "Calculator").await;
    assert_eq!(
        channels_seen(&warm),
        channel_set(&["semantic", "lexical", "lsp"]),
        "{:?} {:?}",
        warm.warning,
        warm.notes
    );
    assert!(warm.warning.is_none(), "{:?}", warm.warning);
    // Nothing to note about a start or the budget; the server it found
    // running is the one note (the tree barrier's, N104).
    assert!(
        without_barrier_notes(&warm.notes).is_empty(),
        "{:?}",
        warm.notes
    );
    assert!(
        warm.notes
            .iter()
            .all(|note| note.starts_with("lsp: csharp-ls kept session; ")),
        "{:?}",
        warm.notes
    );
    assert_eq!(
        spawn_count(&state),
        1,
        "one server served both searches: the first search did not kill it"
    );
    task.abort();
}

/// The first `workspace/symbol` of a real server (rust-analyzer building its
/// symbol index) can take longer than any search budget. The server primes
/// itself with that request in the background; searches in the meantime get
/// the budget note instead of cancelling it, and later ones hit the primed
/// server.
#[tokio::test]
async fn searches_during_priming_get_the_budget_note_and_later_ones_hit_the_primed_server() {
    let (temp, mut config, _fake, task) = fixture().await;
    let workspace = temp.path().join("channels-priming");
    write_channel_workspace(&workspace);
    let state = temp.path().join("lsp-state-priming");
    config.lsp_search_budget_ms = 200;
    config = with_fake_csharp_server(
        config,
        &state,
        &["--first-symbol-delay-ms", "15000"],
        LOADED_BUDGET_SECONDS,
    );
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    // The server starts at once; its priming request takes 15 s, longer than
    // both early searches together at their 6 s ceiling, so neither can see a
    // primed server even under load.
    for attempt in 0..2 {
        let early = app
            .search(&workspace, "Calculator", Some(10))
            .await
            .unwrap();
        assert!(early.warning.is_none(), "{:?}", early.warning);
        // Search 1 finds the session running but misses the budget: that is
        // the one note (no `kept session` label on an answer it never gave).
        assert_eq!(early.notes.len(), 1, "search {attempt}: {:?}", early.notes);
        assert!(
            !channels_seen(&early).contains("lsp"),
            "search {attempt} ran during priming"
        );
        assert!(
            early.timings.lsp_search_ms < 6000.0,
            "search {attempt} waited {} ms for priming instead of its 200 ms budget",
            early.timings.lsp_search_ms
        );
    }

    let primed = search_until_the_language_server_answers(&app, &workspace, "Calculator").await;
    assert_eq!(
        channels_seen(&primed),
        channel_set(&["semantic", "lexical", "lsp"]),
        "{:?} {:?}",
        primed.warning,
        primed.notes
    );
    assert!(primed.warning.is_none(), "{:?}", primed.warning);
    assert!(
        primed.timings.lsp_search_ms < 2000.0,
        "a primed server answers inside the budget (the search had no note; 2 s only allows for a loaded scheduler): {} ms",
        primed.timings.lsp_search_ms
    );
    assert_eq!(spawn_count(&state), 1, "priming reuses the one server");
    task.abort();
}

/// With the language-server channel alone, a server that is still starting
/// is a clean miss with a note: it did not fail, and the search has nothing
/// else to answer with.
#[tokio::test]
async fn a_cold_language_server_alone_is_a_clean_miss_with_a_note_not_an_error() {
    let (temp, mut config, _fake, task) = fixture().await;
    config.channels.semantic = false;
    config.channels.lexical = false;
    config.channels.reranker = false;
    let workspace = temp.path().join("channels-lsp-only-cold");
    write_channel_workspace(&workspace);
    let state = temp.path().join("lsp-state-lsp-only-cold");
    config.lsp_search_budget_ms = 200;
    config = with_fake_csharp_server(config, &state, &["--initialize-delay-ms", "1500"], 10);
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let cold = app
        .search(&workspace, "Calculator", Some(10))
        .await
        .expect("a server that is only starting is not a failed channel");
    assert!(cold.results.is_empty());
    assert!(cold.warning.is_none(), "{:?}", cold.warning);
    assert_eq!(cold.notes.len(), 1, "{:?}", cold.notes);
    assert!(
        cold.notes[0].contains("LSP still starting"),
        "{:?}",
        cold.notes
    );

    // Once it is up, the same search finds the symbol.
    let warm = search_until_the_language_server_answers(&app, &workspace, "Calculator").await;
    assert_eq!(channels_seen(&warm), channel_set(&["lsp"]));
    task.abort();
}

/// A provider that answers some of a search's symbol queries before the
/// budget runs out keeps those answers; only the rest is lost.
#[tokio::test]
async fn answers_gathered_before_the_budget_ran_out_are_kept() {
    let (temp, mut config, _fake, task) = fixture().await;
    let workspace = temp.path().join("channels-partial");
    write_channel_workspace(&workspace);
    let state = temp.path().join("lsp-state-partial");
    // Four terms make four symbol queries of 4000 ms each: the first fits the
    // budget together with the server's start (up to 8 s, which a loaded
    // machine needs), all four (16 s) cannot.
    config.lsp_search_budget_ms = 12_000;
    config = with_fake_csharp_server(
        config,
        &state,
        &["--symbol-delay-ms", "4000"],
        LOADED_BUDGET_SECONDS,
    );
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app
        .search(&workspace, "calculator alpha beta gamma", Some(10))
        .await
        .unwrap();
    assert_eq!(report.notes.len(), 1, "{:?}", report.notes);
    assert!(
        channels_seen(&report).contains("lsp"),
        "the symbols answered before the budget expired are kept: {:?} {:?}",
        channels_seen(&report),
        report.warning
    );
    assert!(report.warning.is_none(), "{:?}", report.warning);
    task.abort();
}

#[tokio::test]
async fn concurrent_searches_start_one_language_server() {
    let (temp, mut config, _fake, task) = fixture().await;
    let workspace = temp.path().join("channels-concurrent");
    write_channel_workspace(&workspace);
    let state = temp.path().join("lsp-state-concurrent");
    config.lsp_search_budget_ms = 30_000;
    config = with_fake_csharp_server(config, &state, &["--initialize-delay-ms", "500"], 10);
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let (first, second, third) = tokio::join!(
        app.search(&workspace, "Calculator", Some(10)),
        app.search(&workspace, "Calculator", Some(10)),
        app.search(&workspace, "Calculator", Some(10)),
    );
    for report in [first.unwrap(), second.unwrap(), third.unwrap()] {
        assert!(
            channels_seen(&report).contains("lsp"),
            "{:?} {:?}",
            report.warning,
            report.notes
        );
        assert!(
            without_barrier_notes(&report.notes).is_empty(),
            "{:?}",
            report.notes
        );
    }
    assert_eq!(
        spawn_count(&state),
        1,
        "searches that arrived while the server was starting share that start"
    );
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
    let indexing = with_fake_csharp_server(
        config.clone(),
        &state_root.join("indexing"),
        &[],
        LOADED_BUDGET_SECONDS,
    );
    let indexer = App::open(indexing).await.unwrap();
    indexer.index(&workspace).await.unwrap();
    drop(indexer);

    for variant in variants {
        let name = variant.name;
        let mut variant_config = config.clone();
        variant_config.lsp_search_budget_ms = 30_000;
        let state = state_root.join(name);
        variant_config =
            with_fake_csharp_server(variant_config, &state, &[], LOADED_BUDGET_SECONDS);
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
    config.lsp_search_budget_ms = 30_000;
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
    lsp_broken = with_fake_csharp_server(
        lsp_broken,
        &temp.path().join("lsp-state-broken"),
        &[],
        LOADED_BUDGET_SECONDS,
    );
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
    // The canned symbol points at a file the index does not contain.
    lsp_healthy =
        with_fake_csharp_server(lsp_healthy, &state, &["--primary-file", "src/Other.cs"], 10);
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

/// Roadmap F2b. `src/many.rs` holds five chunks that match the query better
/// than the only chunk of each of two other files, and the semantic channel
/// alone ranks them, so without a cap the top 3 is that one file three times.
/// With `max_chunks_per_file` the repeats are demoted, not dropped, before the
/// list is cut for the reranker, and every path that does not rerank (the
/// reranker switched off, or failed) shows the same capped order.
#[tokio::test]
async fn the_per_file_cap_puts_distinct_files_first_in_results_and_the_reranker_shortlist() {
    use local_code_intelligence::app::SearchReport;
    use std::collections::BTreeSet;

    let (temp, mut config, fake, task) = fixture().await;
    // The fake embedder gives every chunk containing "translator" one vector
    // and every other chunk another, so the five many.rs chunks outrank the
    // rest and no other channel can reorder them.
    config.channels.lexical = false;
    config.channels.lsp = false;
    let workspace = temp.path().join("per-file-cap");
    write(
        &workspace,
        "src/many.rs",
        "fn translator_one() {}\nfn translator_two() {}\nfn translator_three() {}\nfn translator_four() {}\nfn translator_five() {}\n",
    );
    write(&workspace, "src/second.rs", "fn second_file() {}\n");
    write(&workspace, "src/third.rs", "fn third_file() {}\n");
    let indexer = App::open(config.clone()).await.unwrap();
    assert_eq!(indexer.index(&workspace).await.unwrap().chunks, 7);
    drop(indexer);

    async fn search(
        config: &Config,
        workspace: &Path,
        cap: u32,
        shortlist: usize,
        reranker: bool,
        top_k: usize,
    ) -> SearchReport {
        let mut config = config.clone();
        config.fusion.max_chunks_per_file = cap;
        config.rerank_candidate_count = shortlist;
        config.channels.reranker = reranker;
        let app = App::open(config).await.unwrap();
        app.search(workspace, "translator", Some(top_k))
            .await
            .unwrap()
    }
    fn paths(report: &SearchReport) -> Vec<&str> {
        report
            .results
            .iter()
            .map(|hit| hit.chunk.relative_file_path.as_str())
            .collect()
    }
    fn distinct<'a>(paths: &[&'a str]) -> BTreeSet<&'a str> {
        paths.iter().copied().collect()
    }
    const MANY: &str = "src/many.rs";
    let others = BTreeSet::from(["src/second.rs", "src/third.rs"]);

    // Uncapped (the default): the top 3 is one file three times.
    let uncapped = search(&config, &workspace, 0, 24, false, 3).await;
    assert!(!uncapped.reranked);
    assert!(uncapped.warning.is_none(), "{:?}", uncapped.warning);
    assert_eq!(paths(&uncapped), [MANY, MANY, MANY]);

    // Cap 1, reranker off: distinct files first, many.rs's best chunk leading.
    let capped = search(&config, &workspace, 1, 24, false, 3).await;
    assert!(!capped.reranked);
    assert!(capped.warning.is_none(), "{:?}", capped.warning);
    let capped_paths = paths(&capped);
    assert_eq!(capped_paths[0], MANY);
    assert_eq!(
        distinct(&capped_paths),
        BTreeSet::from([MANY, "src/second.rs", "src/third.rs"])
    );
    assert_eq!(capped_paths.len(), 3);

    // Demotion never removes a chunk: asking for all seven returns all seven,
    // the distinct files first and the four demoted many.rs chunks after them.
    let everything = search(&config, &workspace, 1, 24, false, 8).await;
    let everything_paths = paths(&everything);
    assert_eq!(everything_paths.len(), 7);
    assert_eq!(everything_paths[0], MANY);
    assert_eq!(distinct(&everything_paths[1..3]), others);
    assert!(everything_paths[3..].iter().all(|path| *path == MANY));

    // Cap 2: two many.rs chunks keep their place, the other files follow, and
    // the third many.rs chunk is demoted behind them.
    let two = search(&config, &workspace, 2, 24, false, 5).await;
    let two_paths = paths(&two);
    assert_eq!(two_paths[..2], [MANY, MANY]);
    assert_eq!(distinct(&two_paths[2..4]), others);
    assert_eq!(two_paths[4], MANY);
    // Scores are not rescaled: the demoted many.rs chunk outscored the other
    // files' chunks it now follows, and the list holds the same scores the
    // uncapped order does (the semantic ranks 1..=7), only arranged
    // differently.
    assert!(two.results[4].fusion_score > two.results[2].fusion_score);
    let all_scores = |report: &SearchReport| {
        let mut scores: Vec<u32> = report
            .results
            .iter()
            .map(|hit| hit.fusion_score.to_bits())
            .collect();
        scores.sort_unstable();
        scores
    };
    let uncapped_all = search(&config, &workspace, 0, 24, false, 8).await;
    let capped_all = search(&config, &workspace, 2, 24, false, 8).await;
    assert_eq!(all_scores(&capped_all), all_scores(&uncapped_all));
    assert_eq!(all_scores(&everything), all_scores(&uncapped_all));

    // The default configuration (no `max_chunks_per_file` override) is the
    // cap-2 order, not the uncapped one.
    assert_eq!(config.fusion.max_chunks_per_file, 2);
    let mut default_config = config.clone();
    default_config.channels.reranker = false;
    let by_default = App::open(default_config)
        .await
        .unwrap()
        .search(&workspace, "translator", Some(5))
        .await
        .unwrap();
    let by_default_paths = paths(&by_default);
    assert_eq!(by_default_paths[..2], [MANY, MANY]);
    assert_eq!(distinct(&by_default_paths[2..4]), others);
    assert_eq!(by_default_paths[4], MANY);

    // Reranker on with a 3-chunk shortlist: uncapped, the reranker is only
    // shown many.rs; capped, it is shown one chunk of each file.
    let requests_before = fake.rerank_requests.load(Ordering::SeqCst);
    let reranked_uncapped = search(&config, &workspace, 0, 3, true, 3).await;
    assert!(
        reranked_uncapped.reranked,
        "{:?}",
        reranked_uncapped.warning
    );
    assert_eq!(paths(&reranked_uncapped), [MANY, MANY, MANY]);
    let reranked_capped = search(&config, &workspace, 1, 3, true, 3).await;
    assert!(reranked_capped.reranked, "{:?}", reranked_capped.warning);
    assert_eq!(reranked_capped.candidate_count, 3);
    assert!(
        reranked_capped
            .results
            .iter()
            .all(|hit| hit.reranker_score.is_some())
    );
    let reranked_paths = paths(&reranked_capped);
    assert_eq!(
        reranked_paths[0], MANY,
        "the fake reranker prefers translator"
    );
    assert_eq!(
        distinct(&reranked_paths),
        BTreeSet::from([MANY, "src/second.rs", "src/third.rs"])
    );
    assert_eq!(
        fake.rerank_requests.load(Ordering::SeqCst) - requests_before,
        2
    );

    // The reranker's own order is final: with a shortlist that holds every
    // chunk, the cap only decides what it sees first, and the reranker still
    // puts all five translator chunks ahead of the other files.
    let reranked_all = search(&config, &workspace, 1, 24, true, 7).await;
    assert!(reranked_all.reranked, "{:?}", reranked_all.warning);
    let reranked_all_paths = paths(&reranked_all);
    assert!(reranked_all_paths[..5].iter().all(|path| *path == MANY));
    assert_eq!(distinct(&reranked_all_paths[5..]), others);

    // A failing reranker falls back to the capped fusion order, with a warning.
    fake.rerank_fails.store(true, Ordering::SeqCst);
    let fallback = search(&config, &workspace, 1, 3, true, 3).await;
    fake.rerank_fails.store(false, Ordering::SeqCst);
    assert!(!fallback.reranked);
    let warning = fallback.warning.as_deref().expect("the fallback warns");
    assert!(
        warning.contains("fusion-ranked results returned"),
        "{warning}"
    );
    assert!(fallback.results.iter().all(|h| h.reranker_score.is_none()));
    let fallback_paths = paths(&fallback);
    assert_eq!(fallback_paths[0], MANY);
    assert_eq!(
        distinct(&fallback_paths),
        BTreeSet::from([MANY, "src/second.rs", "src/third.rs"])
    );
    task.abort();
}

// ---------------------------------------------------------------------------
// `[search_profiles.<name>]`: named query-time overrides (channel switches and
// the rerank window) that a search chooses by name. No name is the search the
// application always ran.
// ---------------------------------------------------------------------------

/// The report as JSON without what differs between two otherwise identical
/// searches: the stage timings and the index wait.
fn json_without_clocks(report: &local_code_intelligence::app::SearchReport) -> Value {
    let mut value = serde_json::to_value(report).unwrap();
    let object = value.as_object_mut().unwrap();
    object.remove("timings");
    object["index"].as_object_mut().unwrap().remove("wait_ms");
    value
}

fn sorted_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The lines of a candidate dump file, one JSON document each.
fn dump_lines_of(path: &Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Seven chunks: five in `src/many.rs` that mention the query and one each in
/// two files that do not.
fn write_profile_workspace(root: &Path) {
    write(
        root,
        "src/many.rs",
        "fn translator_one() {}\nfn translator_two() {}\nfn translator_three() {}\nfn translator_four() {}\nfn translator_five() {}\n",
    );
    write(root, "src/second.rs", "fn second_file() {}\n");
    write(root, "src/third.rs", "fn third_file() {}\n");
}

/// `fast` (reranker off), `deep` (a window of three), and `grep` (lexical
/// only, no reranker), next to a top-level window of five and no language
/// server.
fn profiled_config(mut config: Config) -> Config {
    use local_code_intelligence::config::SearchProfile;
    config.channels.lsp = false;
    config.rerank_candidate_count = 5;
    config.search_profiles = std::collections::BTreeMap::from([
        (
            "fast".to_owned(),
            SearchProfile {
                reranker: Some(false),
                ..SearchProfile::default()
            },
        ),
        (
            "deep".to_owned(),
            SearchProfile {
                rerank_candidate_count: Some(3),
                ..SearchProfile::default()
            },
        ),
        (
            "grep".to_owned(),
            SearchProfile {
                semantic: Some(false),
                reranker: Some(false),
                ..SearchProfile::default()
            },
        ),
    ]);
    config
}

/// The proof that adding profiles changed nothing for a search that names
/// none: the same JSON, key for key, as `search_with_filters` and
/// `search_with_options`, which the evaluation runner still calls.
#[tokio::test]
async fn a_search_in_no_profile_matches_search_with_filters() {
    use local_code_intelligence::{app::SearchOptions, filter::FilterRequest};

    let (temp, config, _fake, task) = fixture().await;
    let config = profiled_config(config);
    let workspace = temp.path().join("profile-none");
    write_profile_workspace(&workspace);
    let app = App::open(config).await.unwrap().with_candidate_dump(None);
    app.index(&workspace).await.unwrap();

    for filters in [
        FilterRequest::default(),
        FilterRequest {
            languages: Some(vec!["rust".to_owned()]),
            exclude_paths: Some(vec!["src/third.rs".to_owned()]),
            ..FilterRequest::default()
        },
    ] {
        let by_filters = app
            .search_with_filters(&workspace, "translator", Some(3), filters.clone())
            .await
            .unwrap();
        let by_none = app
            .search_in_profile(&workspace, "translator", Some(3), filters.clone(), None)
            .await
            .unwrap();
        let by_options = app
            .search_with_options(
                &workspace,
                "translator",
                Some(3),
                filters,
                SearchOptions::default(),
            )
            .await
            .unwrap();
        assert!(by_filters.reranked, "{:?}", by_filters.warning);
        assert!(by_filters.warning.is_none(), "{:?}", by_filters.warning);
        assert!(by_none.search_profile.is_none() && by_options.search_profile.is_none());

        let json = json_without_clocks(&by_filters);
        assert_eq!(json_without_clocks(&by_none), json);
        assert_eq!(json_without_clocks(&by_options), json);
        // Not one key more than a search had before profiles existed.
        assert_eq!(
            sorted_keys(&json),
            [
                "candidate_count",
                "filters",
                "index",
                "query",
                "reranked",
                "results",
                "warning",
                "workspace"
            ]
        );
        assert!(
            serde_json::to_value(&by_none)
                .unwrap()
                .get("search_profile")
                .is_none()
        );
    }
    task.abort();
}

/// A `top_k` above the rerank window is cut to the window, with the reranker
/// on and off, and the answer says so in `notes` (not in `warning`: the
/// search is not a fallback). A `top_k` that fits the window, or none passed,
/// says nothing.
#[tokio::test]
async fn a_top_k_above_the_rerank_window_is_named_in_a_note() {
    use local_code_intelligence::filter::FilterRequest;

    let (temp, config, _fake, task) = fixture().await;
    let config = profiled_config(config);
    let workspace = temp.path().join("profile-window-note");
    write_profile_workspace(&workspace);
    let app = App::open(config).await.unwrap().with_candidate_dump(None);
    app.index(&workspace).await.unwrap();
    let search = |top_k: usize, profile: Option<&'static str>| {
        let (app, workspace) = (&app, &workspace);
        async move {
            app.search_in_profile(
                workspace,
                "translator",
                Some(top_k),
                FilterRequest::default(),
                profile,
            )
            .await
            .unwrap()
        }
    };

    // The top-level window is five: eight asked for, five come back, and the
    // note names both numbers.
    let base = search(8, None).await;
    assert_eq!(base.results.len(), 5);
    assert_eq!(base.notes.len(), 1, "{:?}", base.notes);
    assert!(
        base.notes[0].contains("top_k 8") && base.notes[0].contains("rerank_candidate_count = 5"),
        "{:?}",
        base.notes
    );
    assert!(base.warning.is_none(), "{:?}", base.warning);
    assert!(serde_json::to_value(&base).unwrap()["notes"].is_array());

    // With the reranker off the cut is the same, and so is the note.
    let fast = search(8, Some("fast")).await;
    assert!(!fast.reranked);
    assert_eq!(fast.results.len(), 5);
    assert_eq!(fast.notes, base.notes);

    // A profile with its own window names that one.
    let deep = search(8, Some("deep")).await;
    assert_eq!(deep.results.len(), 3);
    assert_eq!(deep.notes.len(), 1, "{:?}", deep.notes);
    assert!(
        deep.notes[0].contains("top_k 8") && deep.notes[0].contains("rerank_candidate_count = 3"),
        "{:?}",
        deep.notes
    );

    // No `top_k` passed: the configured default (eight) is above the window of
    // five too, but that is the operator's setting, not a request.
    let default = app
        .search_in_profile(
            &workspace,
            "translator",
            None,
            FilterRequest::default(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(default.results.len(), 5);
    assert!(default.notes.is_empty(), "{:?}", default.notes);

    // A `top_k` that fits the window, the window itself included, says nothing.
    for top_k in [3, 5] {
        let fits = search(top_k, None).await;
        assert_eq!(fits.results.len(), top_k);
        assert!(fits.notes.is_empty(), "{:?}", fits.notes);
        assert!(serde_json::to_value(&fits).unwrap().get("notes").is_none());
    }
    task.abort();
}

#[tokio::test]
async fn profiles_switch_the_reranker_and_window_and_report_themselves() {
    use local_code_intelligence::{app::SearchReport, filter::FilterRequest};

    let (temp, config, fake, task) = fixture().await;
    let config = profiled_config(config);
    let workspace = temp.path().join("profile-switch");
    write_profile_workspace(&workspace);
    let dump_path = temp.path().join("profile-candidates.jsonl");
    let app = App::open(config.clone())
        .await
        .unwrap()
        .with_candidate_dump(Some(dump_path.clone()));
    assert_eq!(app.index(&workspace).await.unwrap().chunks, 7);

    async fn search(
        app: &App,
        workspace: &Path,
        profile: Option<&str>,
    ) -> anyhow::Result<SearchReport> {
        app.search_in_profile(
            workspace,
            "translator",
            Some(3),
            FilterRequest::default(),
            profile,
        )
        .await
    }
    let reranks = || fake.rerank_requests.load(Ordering::SeqCst);
    let queries = || fake.queries.load(Ordering::SeqCst);

    // No profile: the top-level window of five and a reranked answer.
    let (reranks_before, queries_before) = (reranks(), queries());
    let base = search(&app, &workspace, None).await.unwrap();
    assert!(
        base.reranked && base.warning.is_none(),
        "{:?}",
        base.warning
    );
    assert_eq!(base.search_profile, None);
    assert_eq!(base.candidate_count, 5);
    assert_eq!(reranks() - reranks_before, 1);
    assert_eq!(queries() - queries_before, 1);

    // `fast`: no reranker, so no request, no `reranked`, and no warning; the
    // window and the retrieval channels are the top level's.
    let (reranks_before, queries_before) = (reranks(), queries());
    let fast = search(&app, &workspace, Some("fast")).await.unwrap();
    assert!(!fast.reranked, "{:?}", fast.warning);
    assert!(fast.warning.is_none(), "{:?}", fast.warning);
    assert_eq!(fast.search_profile.as_deref(), Some("fast"));
    assert_eq!(fast.candidate_count, 5);
    assert_eq!(fast.results.len(), 3);
    assert!(fast.results.iter().all(|hit| hit.reranker_score.is_none()));
    assert_eq!(reranks(), reranks_before, "the reranker was not called");
    assert_eq!(queries() - queries_before, 1);
    assert_eq!(channels_seen(&fast), channel_set(&["semantic", "lexical"]));
    assert_eq!(
        serde_json::to_value(&fast).unwrap()["search_profile"],
        "fast"
    );

    // `deep`: the reranker is shown three candidates, not five.
    let reranks_before = reranks();
    let deep = search(&app, &workspace, Some("deep")).await.unwrap();
    assert!(deep.reranked, "{:?}", deep.warning);
    assert_eq!(deep.search_profile.as_deref(), Some("deep"));
    assert_eq!(deep.candidate_count, 3);
    assert_eq!(reranks() - reranks_before, 1);

    // `grep`: lexical only. The query is not embedded at all.
    let (reranks_before, queries_before) = (reranks(), queries());
    let grep = search(&app, &workspace, Some("grep")).await.unwrap();
    assert_eq!(grep.search_profile.as_deref(), Some("grep"));
    assert!(
        !grep.reranked && grep.warning.is_none(),
        "{:?}",
        grep.warning
    );
    assert_eq!(channels_seen(&grep), channel_set(&["lexical"]));
    assert_eq!(queries(), queries_before, "the query was not embedded");
    assert_eq!(reranks(), reranks_before);
    assert_eq!(grep.timings.query_embedding_ms, 0.0);

    // An unknown name fails before any work, with the configured names.
    let unindexed = temp.path().join("profile-unindexed");
    write_profile_workspace(&unindexed);
    let documents_before = fake.document_requests.load(Ordering::SeqCst);
    let queries_before = queries();
    for name in ["nope", "default", "", "Fast"] {
        let error = search(&app, &unindexed, Some(name))
            .await
            .expect_err("an unconfigured profile is an error, not the default");
        assert_eq!(
            error.to_string(),
            format!("unknown search profile {name:?}; configured: deep, fast, grep")
        );
    }
    assert_eq!(
        fake.document_requests.load(Ordering::SeqCst),
        documents_before,
        "the failed search did not index the workspace"
    );
    assert_eq!(queries(), queries_before);

    // The candidate dump records what each search ran with. A search that
    // named no profile left its line exactly as it was before profiles.
    let lines = dump_lines_of(&dump_path);
    assert_eq!(lines.len(), 4, "one line per successful search");
    let plain_keys = [
        "candidates",
        "channels",
        "filters",
        "query",
        "rerank_candidate_count",
        "rerank_outcome",
        "timings",
        "ts",
        "workspace_path",
    ];
    let profiled_keys = [
        "candidates",
        "channels",
        "filters",
        "query",
        "rerank_candidate_count",
        "rerank_outcome",
        "search_profile",
        "timings",
        "ts",
        "workspace_path",
    ];
    let channels = |semantic: bool, reranker: bool| json!({"semantic": semantic, "lexical": true, "lsp": false, "reranker": reranker});
    assert_eq!(sorted_keys(&lines[0]), plain_keys);
    assert_eq!(lines[0]["channels"], channels(true, true));
    assert_eq!(lines[0]["rerank_candidate_count"], 5);
    assert_eq!(lines[0]["rerank_outcome"], "scored");

    assert_eq!(sorted_keys(&lines[1]), profiled_keys);
    assert_eq!(lines[1]["search_profile"], "fast");
    assert_eq!(lines[1]["channels"], channels(true, false));
    assert_eq!(lines[1]["rerank_candidate_count"], 5);
    assert_eq!(lines[1]["rerank_outcome"], "off");

    assert_eq!(sorted_keys(&lines[2]), profiled_keys);
    assert_eq!(lines[2]["search_profile"], "deep");
    assert_eq!(lines[2]["channels"], channels(true, true));
    assert_eq!(lines[2]["rerank_candidate_count"], 3);
    assert_eq!(lines[2]["rerank_outcome"], "scored");
    let shortlisted = lines[2]["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["shortlisted"] == true)
        .count();
    assert_eq!(shortlisted, 3, "the dump marks the profile's window");

    assert_eq!(sorted_keys(&lines[3]), profiled_keys);
    assert_eq!(lines[3]["search_profile"], "grep");
    assert_eq!(lines[3]["channels"], channels(false, false));
    assert_eq!(lines[3]["rerank_outcome"], "off");
    drop(app);

    // A profile is the same search as the top-level configuration it
    // stands for: nothing but its name differs.
    async fn equivalent(config: Config, workspace: &Path) -> Value {
        let app = App::open(config).await.unwrap().with_candidate_dump(None);
        let report = search(&app, workspace, None).await.unwrap();
        json_without_clocks(&report)
    }
    fn without_profile(report: &SearchReport) -> Value {
        let mut value = json_without_clocks(report);
        value.as_object_mut().unwrap().remove("search_profile");
        value
    }
    let mut stands_for_fast = config.clone();
    stands_for_fast.search_profiles.clear();
    stands_for_fast.channels.reranker = false;
    assert_eq!(
        equivalent(stands_for_fast, &workspace).await,
        without_profile(&fast)
    );
    let mut stands_for_deep = config.clone();
    stands_for_deep.search_profiles.clear();
    stands_for_deep.rerank_candidate_count = 3;
    assert_eq!(
        equivalent(stands_for_deep, &workspace).await,
        without_profile(&deep)
    );
    let mut stands_for_grep = config.clone();
    stands_for_grep.search_profiles.clear();
    stands_for_grep.channels.semantic = false;
    stands_for_grep.channels.reranker = false;
    assert_eq!(
        equivalent(stands_for_grep, &workspace).await,
        without_profile(&grep)
    );
    let mut stands_for_none = config.clone();
    stands_for_none.search_profiles.clear();
    assert_eq!(
        equivalent(stands_for_none, &workspace).await,
        without_profile(&base)
    );
    task.abort();
}

/// Runs the command-line binary and returns its output, whatever its status.
async fn run_search_cli(config_path: &Path, args: Vec<String>) -> std::process::Output {
    let config_path = config_path.to_owned();
    // The fake model service runs on this runtime, so the command must not
    // block it.
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
        command.arg("--config").arg(config_path).args(args);
        strip_lci_env(&mut command).output().unwrap()
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn the_command_line_search_takes_a_search_profile_and_names_the_configured_ones_when_it_is_unknown()
 {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("profile-cli");
    write_profile_workspace(&workspace);
    let config_path = temp.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "data_dir = '{}'\nembedding_url = '{}'\nreranker_url = '{}'\nrerank_candidate_count = 5\n\n[channels]\nlsp = false\n\n[search_profiles.fast]\nreranker = false\n\n[search_profiles.deep]\nrerank_candidate_count = 3\n",
            temp.path().join("cli-data").display(),
            config.embedding_url,
            config.reranker_url,
        ),
    )
    .unwrap();
    let workspace_arg = workspace.display().to_string();
    let search = |extra: &[&str]| {
        let mut args = vec![
            "search".to_owned(),
            workspace_arg.clone(),
            "translator".to_owned(),
            "--top-k".to_owned(),
            "3".to_owned(),
        ];
        args.extend(extra.iter().map(|arg| (*arg).to_owned()));
        run_search_cli(&config_path, args)
    };
    let index = run_search_cli(
        &config_path,
        vec!["index".to_owned(), workspace_arg.clone()],
    )
    .await;
    assert!(
        index.status.success(),
        "{}",
        String::from_utf8_lossy(&index.stderr)
    );

    // No flag: the response a search always had.
    let reranks_before = fake.rerank_requests.load(Ordering::SeqCst);
    let plain = search(&[]).await;
    assert!(
        plain.status.success(),
        "{}",
        String::from_utf8_lossy(&plain.stderr)
    );
    let plain: Value = serde_json::from_slice(&plain.stdout).unwrap();
    assert!(plain.get("search_profile").is_none(), "{plain}");
    assert_eq!(plain["reranked"], true);
    assert_eq!(plain["candidate_count"], 5);
    assert_eq!(
        fake.rerank_requests.load(Ordering::SeqCst) - reranks_before,
        1
    );

    // The flag: the profile applies and the response names it.
    let reranks_before = fake.rerank_requests.load(Ordering::SeqCst);
    let fast = search(&["--search-profile", "fast"]).await;
    assert!(
        fast.status.success(),
        "{}",
        String::from_utf8_lossy(&fast.stderr)
    );
    let fast: Value = serde_json::from_slice(&fast.stdout).unwrap();
    assert_eq!(fast["search_profile"], "fast");
    assert_eq!(fast["reranked"], false);
    assert_eq!(fake.rerank_requests.load(Ordering::SeqCst), reranks_before);
    let deep = search(&["--search-profile", "deep"]).await;
    assert!(
        deep.status.success(),
        "{}",
        String::from_utf8_lossy(&deep.stderr)
    );
    let deep: Value = serde_json::from_slice(&deep.stdout).unwrap();
    assert_eq!(deep["search_profile"], "deep");
    assert_eq!(deep["candidate_count"], 3);

    // An unknown name is a failure that lists the configured names.
    let unknown = search(&["--search-profile", "nope"]).await;
    assert!(!unknown.status.success());
    let stderr = String::from_utf8_lossy(&unknown.stderr);
    assert!(
        stderr.contains("unknown search profile \"nope\"; configured: deep, fast"),
        "{stderr}"
    );
    assert!(unknown.stdout.is_empty());
    task.abort();
}

/// The Qt Linguist form of a `.ts` file: XML, with a word the fake embedding
/// service gives the same vector as the TypeScript fixtures, so an indexed
/// copy would show up in results.
const QT_LINGUIST_CATALOG: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<!DOCTYPE TS>\n<TS version=\"2.1\" language=\"de\">\n<context>\n    <name>SendCoinsDialog</name>\n    <message>\n        <source>Send coins</source>\n        <translatorcomment>translator note</translatorcomment>\n        <translation>Muenzen senden</translation>\n    </message>\n</context>\n</TS>\n";

fn indexed_paths(report: &local_code_intelligence::app::IndexedFilesReport) -> Vec<(&str, &str)> {
    let mut files: Vec<_> = report
        .files
        .iter()
        .map(|file| (file.relative_file_path.as_str(), file.language.as_str()))
        .collect();
    files.sort_unstable();
    files
}

#[tokio::test]
async fn a_qt_linguist_ts_file_is_skipped_while_typescript_is_indexed() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("qt-app");
    write(
        &workspace,
        "src/translator.ts",
        "export function translator() { return 1; }\n",
    );
    write(&workspace, "locale/app_de.ts", QT_LINGUIST_CATALOG);
    let app = App::open(config).await.unwrap();

    let report = app.index(&workspace).await.unwrap();
    assert_eq!(report.files, 1);
    assert_eq!(report.skipped_non_source_files, 1);
    assert_eq!(report.parsed_files, 1);
    assert_eq!(
        serde_json::to_value(&report).unwrap()["skipped_non_source_files"],
        1
    );
    // The catalog never reached the embedding service.
    assert!(report.chunks >= 1);
    assert_eq!(report.embedded_chunks, report.chunks);
    assert_eq!(fake.documents.load(Ordering::SeqCst), report.chunks);
    let indexed = app.indexed_files(&workspace).await.unwrap();
    assert_eq!(
        indexed_paths(&indexed),
        [("src/translator.ts", "typescript")]
    );

    let found = app.search(&workspace, "translator", Some(8)).await.unwrap();
    assert!(!found.results.is_empty());
    assert!(
        found
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path == "src/translator.ts"
                && hit.chunk.language == "typescript"),
        "{:?}",
        found
            .results
            .iter()
            .map(|hit| hit.chunk.relative_file_path.as_str())
            .collect::<Vec<_>>()
    );
    let catalog_only = app
        .search(&workspace, "Muenzen senden", Some(8))
        .await
        .unwrap();
    assert!(
        catalog_only
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path != "locale/app_de.ts")
    );

    // A repeat does no work and still reports the skipped file.
    let repeat = app.index(&workspace).await.unwrap();
    assert_eq!(repeat.parsed_files, 0);
    assert_eq!(repeat.unchanged_files, 1);
    assert_eq!(repeat.embedded_chunks, 0);
    assert_eq!(repeat.skipped_non_source_files, 1);
    task.abort();
}

#[tokio::test]
async fn a_ts_file_that_becomes_a_qt_linguist_catalog_drops_its_old_chunks() {
    let (temp, mut config, fake, task) = fixture().await;
    config.index.stale_check_interval_seconds = 0;
    let workspace = temp.path().join("qt-reclassified");
    write(&workspace, "keep.rs", "fn translator_keep() {}\n");
    let typescript = "export function translator_catalog() { return 1; }\n";
    write(&workspace, "catalog.ts", typescript);
    let app = App::open(config).await.unwrap();

    let first = app.index(&workspace).await.unwrap();
    assert_eq!(first.files, 2);
    assert_eq!(first.skipped_non_source_files, 0);
    let indexed = app.indexed_files(&workspace).await.unwrap();
    assert_eq!(
        indexed_paths(&indexed),
        [("catalog.ts", "typescript"), ("keep.rs", "rust")]
    );
    let catalog_chunks = first.chunks - 1;
    assert!(catalog_chunks >= 1);

    // The same path now holds a Linguist catalog: the next index treats it
    // like a deleted file and drops its chunks.
    write(&workspace, "catalog.ts", QT_LINGUIST_CATALOG);
    assert!(app.status(&workspace).await.unwrap().stale);
    let second = app.index(&workspace).await.unwrap();
    assert_eq!(second.files, 1);
    assert_eq!(second.chunks, first.chunks - catalog_chunks);
    assert_eq!(second.removed_files, 1);
    assert_eq!(second.parsed_files, 0);
    assert_eq!(second.skipped_non_source_files, 1);
    assert_eq!(second.embedded_chunks, 0);
    let indexed = app.indexed_files(&workspace).await.unwrap();
    assert_eq!(indexed_paths(&indexed), [("keep.rs", "rust")]);
    assert!(!app.status(&workspace).await.unwrap().stale);

    // Back to TypeScript, then to a catalog again, each picked up by the
    // on-search refresh with no explicit index call.
    write(&workspace, "catalog.ts", typescript);
    let restored = app.search(&workspace, "translator", Some(8)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&restored).unwrap()["index"]["action"],
        "refreshed_incrementally"
    );
    assert!(
        restored
            .results
            .iter()
            .any(|hit| hit.chunk.relative_file_path == "catalog.ts"
                && hit.chunk.language == "typescript")
    );
    let documents_before = fake.documents.load(Ordering::SeqCst);
    write(&workspace, "catalog.ts", QT_LINGUIST_CATALOG);
    let dropped = app.search(&workspace, "translator", Some(8)).await.unwrap();
    assert_eq!(
        serde_json::to_value(&dropped).unwrap()["index"]["action"],
        "refreshed_incrementally"
    );
    assert!(
        dropped
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path == "keep.rs")
    );
    assert_eq!(fake.documents.load(Ordering::SeqCst), documents_before);
    let indexed = app.indexed_files(&workspace).await.unwrap();
    assert_eq!(indexed_paths(&indexed), [("keep.rs", "rust")]);
    task.abort();
}

#[tokio::test]
async fn qt_linguist_detection_handles_a_bom_a_comment_mentioning_ts_and_an_empty_file() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("qt-edge-cases");
    write(
        &workspace,
        "bom.ts",
        &format!("\u{feff}{QT_LINGUIST_CATALOG}"),
    );
    write(
        &workspace,
        "comment.ts",
        "// <TS version=\"2.1\"> is the Qt Linguist root; this file is TypeScript.\nexport function translator() { return 1; }\n",
    );
    write(&workspace, "empty.ts", "");
    let app = App::open(config).await.unwrap();

    let report = app.index(&workspace).await.unwrap();
    assert_eq!(report.skipped_non_source_files, 1);
    assert_eq!(report.files, 2);
    let indexed = app.indexed_files(&workspace).await.unwrap();
    // The empty file is TypeScript with no chunks: indexed, not skipped.
    assert_eq!(
        indexed_paths(&indexed),
        [("comment.ts", "typescript"), ("empty.ts", "typescript")]
    );
    let found = app.search(&workspace, "translator", Some(8)).await.unwrap();
    assert!(
        found
            .results
            .iter()
            .any(|hit| hit.chunk.relative_file_path == "comment.ts")
    );
    assert!(
        found
            .results
            .iter()
            .all(|hit| hit.chunk.relative_file_path != "bom.ts")
    );
    task.abort();
}

/// Spike slice S2 (map-trait.md, "Response unchanged"): the JSON `search_code`
/// returns for the offline fixture is compared byte for byte with a golden file
/// written from the production search before the map layer existed, so adding
/// the map layer is shown not to change a response. Normalised before the
/// comparison, and only these: the timings and the index wait (wall clock), the
/// workspace path and id (per machine; backslashes in the rest of a path become
/// `/`), and scores computed by model or library float arithmetic that may
/// differ in the last bits between platforms (`semantic_score`,
/// `lexical_score`, `reranker_score`), rounded to six decimals. `fusion_score`,
/// ranks, order, warnings and notes are compared exactly. Set `BLESS_GOLDEN=1`
/// to rewrite the file.
#[tokio::test]
async fn search_code_json_matches_the_golden_file() {
    let (temp, mut config, _fake, task) = fixture().await;
    let workspace = temp.path().join("golden");
    // The C# workspace of the channel tests, with a `golden_rank_<d>` marker
    // in every chunk so the fake embedder gives each its own distance, plus a
    // Go source and a Python test (no language server is configured for
    // either, so no warning depends on the platform's error text).
    write(
        &workspace,
        "src/Calculator.cs",
        "// golden_rank_4\nnamespace Acceptance;\npublic sealed class Calculator\n{\n    // golden_rank_1\n    public int Add(int a, int b) => a + b;\n}\n",
    );
    write(
        &workspace,
        "src/totals.go",
        "package totals\n\n// CalculatorTotal sums values. golden_rank_2\nfunc CalculatorTotal(values []int) int {\n\ttotal := 0\n\tfor _, v := range values {\n\t\ttotal += v\n\t}\n\treturn total\n}\n",
    );
    write(
        &workspace,
        "tests/calculator_test.py",
        "def test_calculator_adds():\n    # golden_rank_3\n    assert 1 + 2 == 3\n",
    );
    let state = temp.path().join("lsp-state-golden");
    config.lsp_search_budget_ms = 30_000;
    config = with_fake_csharp_server(config, &state, &[], LOADED_BUDGET_SECONDS);
    let app = App::open(config).await.unwrap().with_candidate_dump(None);
    app.index(&workspace).await.unwrap();
    let report = app
        .search_in_profile(
            &workspace,
            "Calculator",
            Some(10),
            local_code_intelligence::filter::FilterRequest::default(),
            None,
        )
        .await
        .unwrap();
    // What the MCP tool returns: `serde_json::to_value` of the report.
    let mut value = serde_json::to_value(&report).unwrap();
    let root = report.workspace.path.to_string_lossy().into_owned();
    fn normalise(value: &mut Value, root: &str, key: Option<&str>) {
        match value {
            Value::Object(map) => {
                for (name, field) in map.iter_mut() {
                    normalise(field, root, Some(name));
                }
            }
            Value::Array(items) => {
                for item in items {
                    normalise(item, root, key);
                }
            }
            Value::String(text) if text.starts_with(root) => {
                *text = format!("<workspace>{}", &text[root.len()..]).replace('\\', "/");
            }
            Value::Number(number)
                if matches!(
                    key,
                    Some("semantic_score" | "lexical_score" | "reranker_score")
                ) =>
            {
                let rounded = (number.as_f64().unwrap() * 1e6).round() / 1e6;
                *value = json!(rounded);
            }
            _ => {}
        }
    }
    normalise(&mut value, &root, None);
    for field in value["timings"].as_object_mut().unwrap().values_mut() {
        *field = json!(0.0);
    }
    value["index"]["wait_ms"] = json!(0.0);
    value["workspace"]["id"] = json!("<workspace-id>");
    let actual = serde_json::to_string_pretty(&value).unwrap() + "\n";
    let golden =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/search_code_channels.json");
    if std::env::var_os("BLESS_GOLDEN").is_some() {
        std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
        std::fs::write(&golden, &actual).unwrap();
    }
    let expected = std::fs::read_to_string(&golden)
        .unwrap_or_else(|error| panic!("{}: {error}; run with BLESS_GOLDEN=1", golden.display()))
        .replace("\r\n", "\n");
    assert_eq!(actual, expected, "search_code JSON changed");
    task.abort();
}

/// Spike test T0, end to end (map-trait.md, "Bit-exact legacy fusion"): the
/// map registry with the four wrapped legacy maps, run on a real index with
/// all three channels live (fake embedder, rg, fake C# language server),
/// fuses the same pool as the production search's traced candidates: the
/// same chunks in the same fused order with every `fusion_score` equal by
/// `to_bits()`. The production search does not use the registry; this
/// shows the wrapped maps reproduce it.
#[cfg(feature = "map-equivalence")]
#[tokio::test]
async fn the_map_registry_fuses_the_pool_of_a_traced_search_bit_for_bit() {
    let (temp, mut config, _fake, task) = fixture().await;
    let workspace = temp.path().join("t0");
    write(
        &workspace,
        "src/Calculator.cs",
        "// golden_rank_4\nnamespace Acceptance;\npublic sealed class Calculator\n{\n    // golden_rank_1\n    public int Add(int a, int b) => a + b;\n}\n",
    );
    write(
        &workspace,
        "src/totals.go",
        "package totals\n\n// CalculatorTotal sums values. golden_rank_2\nfunc CalculatorTotal(values []int) int {\n\treturn 0\n}\n",
    );
    write(
        &workspace,
        "tests/calculator_test.py",
        "def test_calculator_adds():\n    # golden_rank_3\n    assert 1 + 2 == 3\n",
    );
    let state = temp.path().join("lsp-state-t0");
    config.lsp_search_budget_ms = 30_000;
    config = with_fake_csharp_server(config, &state, &[], LOADED_BUDGET_SECONDS);
    let app = App::open(config).await.unwrap().with_candidate_dump(None);
    app.index(&workspace).await.unwrap();
    let report = app
        .search_with_options(
            &workspace,
            "Calculator",
            Some(10),
            local_code_intelligence::filter::FilterRequest::default(),
            local_code_intelligence::app::SearchOptions {
                trace_candidates: true,
            },
        )
        .await
        .unwrap();
    let traced = report.candidates.as_ref().expect("the pool was traced");
    assert!(report.warning.is_none(), "{:?}", report.warning);
    assert!(
        traced.iter().any(|row| row.semantic_rank.is_some()
            && row.lexical_rank.is_some()
            && row.lsp_rank.is_some()),
        "every channel contributed: {traced:?}"
    );

    let maps = app
        .fuse_through_maps(&workspace, "Calculator")
        .await
        .unwrap();
    assert!(maps.warnings.is_empty(), "{:?}", maps.warnings);
    // The traced search started the C# server, so the map path finds it
    // running and says so, as a search does (N110).
    assert_eq!(
        masked_seconds(&maps.notes),
        [KEPT_CSHARP_LS],
        "{:?}",
        maps.notes
    );
    assert_eq!(maps.rows.len(), traced.len());
    for (row, trace) in maps.rows.iter().zip(traced) {
        assert_eq!(
            (&row.relative_file_path, row.start_line, row.end_line),
            (&trace.relative_file_path, trace.start_line, trace.end_line)
        );
        assert_eq!(
            row.fusion_score.to_bits(),
            trace.fusion_score.to_bits(),
            "{row:?} vs {trace:?}"
        );
        assert!(row.node.starts_with("chunk:"), "{}", row.node);
    }
    task.abort();
}

/// Spike T0 on failure paths: with the embedder refusing queries, and with
/// one language server failing (rust-analyzer missing, a Rust file indexed)
/// while the fake C# server answers, the map registry fuses the traced
/// search's pool bit for bit and raises the same warnings: each map warning
/// is one of the parts the production search joins with "; ".
#[cfg(feature = "map-equivalence")]
#[tokio::test]
async fn the_map_registry_matches_a_traced_search_when_a_channel_fails() {
    let (temp, mut config, fake, task) = fixture().await;
    let workspace = temp.path().join("t0-failing");
    write(
        &workspace,
        "src/Calculator.cs",
        "// golden_rank_4\nnamespace Acceptance;\npublic sealed class Calculator\n{\n    // golden_rank_1\n    public int Add(int a, int b) => a + b;\n}\n",
    );
    write(
        &workspace,
        "src/total.rs",
        "/// Calculator totals. golden_rank_2\npub fn calculator_total() -> i32 {\n    3\n}\n",
    );
    let state = temp.path().join("lsp-state-t0-failing");
    config.lsp_search_budget_ms = 30_000;
    config = with_fake_csharp_server(config, &state, &[], LOADED_BUDGET_SECONDS);
    let app = App::open(config).await.unwrap().with_candidate_dump(None);
    app.index(&workspace).await.unwrap();
    for embedding_fails in [false, true] {
        fake.embed_fails.store(embedding_fails, Ordering::SeqCst);
        let report = app
            .search_with_options(
                &workspace,
                "Calculator",
                Some(10),
                local_code_intelligence::filter::FilterRequest::default(),
                local_code_intelligence::app::SearchOptions {
                    trace_candidates: true,
                },
            )
            .await
            .unwrap();
        let traced = report.candidates.as_ref().expect("the pool was traced");
        let warning = report.warning.clone().expect("rust-analyzer is missing");
        assert!(
            warning.contains("Optional LSP provider unavailable: rust-analyzer"),
            "{warning}"
        );
        assert_eq!(
            warning.contains("Query embedding unavailable"),
            embedding_fails,
            "{warning}"
        );
        assert!(
            traced.iter().any(|row| row.lsp_rank.is_some()),
            "the C# server answered while rust-analyzer failed: {traced:?}"
        );

        let maps = app
            .fuse_through_maps(&workspace, "Calculator")
            .await
            .unwrap();
        let joined: usize = maps.warnings.iter().map(String::len).sum::<usize>()
            + 2 * maps.warnings.len().saturating_sub(1);
        assert_eq!(joined, warning.len(), "{:?} vs {warning}", maps.warnings);
        for part in &maps.warnings {
            assert!(warning.contains(part.as_str()), "{part} not in {warning}");
        }
        // The map path carries the tree barrier's notes (N110): it found the
        // C# server running, as the next search will; a search that started
        // the server itself (the first one) notes nothing.
        assert_eq!(
            masked_seconds(&maps.notes),
            [KEPT_CSHARP_LS],
            "embedding fails: {embedding_fails}"
        );
        assert!(
            report.notes.is_empty() || masked_seconds(&report.notes) == [KEPT_CSHARP_LS],
            "{:?}",
            report.notes
        );
        assert_eq!(
            maps.rows.len(),
            traced.len(),
            "embedding fails: {embedding_fails}"
        );
        for (row, trace) in maps.rows.iter().zip(traced) {
            assert_eq!(
                (&row.relative_file_path, row.start_line),
                (&trace.relative_file_path, trace.start_line)
            );
            assert_eq!(row.fusion_score.to_bits(), trace.fusion_score.to_bits());
        }
    }
    fake.embed_fails.store(false, Ordering::SeqCst);
    task.abort();
}

/// N110: the LSP map's status carries what the tree barrier did, with the
/// search report's wording: a kept server the edit made stale is restarted and
/// noted as such on both paths, and one found running is noted as kept.
#[cfg(feature = "map-equivalence")]
#[tokio::test]
async fn the_map_registry_notes_the_tree_barrier_as_the_search_report_does() {
    let (temp, mut config, _fake, task) = fixture().await;
    let workspace = temp.path().join("t0-barrier-notes");
    let calculator = |extra: &str| {
        format!(
            "// golden_rank_4\nnamespace Acceptance;\npublic sealed class Calculator\n{{\n    // golden_rank_1\n    public int Add(int a, int b) => a + b;\n}}\n{extra}"
        )
    };
    write(&workspace, "src/Calculator.cs", &calculator(""));
    let state = temp.path().join("lsp-state-t0-barrier-notes");
    config.lsp_search_budget_ms = 30_000;
    config = with_fake_csharp_server(config, &state, &[], LOADED_BUDGET_SECONDS);
    let app = App::open(config).await.unwrap().with_candidate_dump(None);
    app.index(&workspace).await.unwrap();

    // The search starts the server: nothing to note yet.
    let started = app
        .search(&workspace, "Calculator", Some(10))
        .await
        .unwrap();
    assert!(started.notes.is_empty(), "{:?}", started.notes);
    assert!(started.warning.is_none(), "{:?}", started.warning);

    // An edit between the calls: each path restarts the server it finds, and
    // says so in the same words. (The edits differ in length, so the stamp
    // moves whatever the clock does.)
    write(&workspace, "src/Calculator.cs", &calculator("// one\n"));
    let report = app
        .search(&workspace, "Calculator", Some(10))
        .await
        .unwrap();
    assert_eq!(report.notes, [RESTARTED_CSHARP_LS], "search");
    write(&workspace, "src/Calculator.cs", &calculator("// one two\n"));
    let maps = app
        .fuse_through_maps(&workspace, "Calculator")
        .await
        .unwrap();
    assert!(maps.warnings.is_empty(), "{:?}", maps.warnings);
    assert_eq!(maps.notes, report.notes, "the map path");

    // Unchanged since: each finds the server the other started.
    let report = app
        .search(&workspace, "Calculator", Some(10))
        .await
        .unwrap();
    let maps = app
        .fuse_through_maps(&workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(masked_seconds(&report.notes), [KEPT_CSHARP_LS]);
    assert_eq!(masked_seconds(&maps.notes), masked_seconds(&report.notes));
    task.abort();
}

/// K0a follow-up: on a vectorless snapshot the map registry keeps the
/// semantic map off, as the production search does, so it sends no query
/// embedding (the embedder is answering again here) and the record's plan
/// lists the map as disabled. The warning stays the production search's.
#[cfg(feature = "map-equivalence")]
#[tokio::test]
async fn the_map_registry_leaves_the_semantic_map_off_on_a_vectorless_snapshot() {
    let (temp, mut config, fake, task) = fixture().await;
    config.channels.lsp = false;
    let workspace = temp.path().join("t0-vectorless");
    write(
        &workspace,
        "lib.rs",
        "fn translator() {}\nfn unrelated() {}\n",
    );
    let app = App::open(config).await.unwrap().with_candidate_dump(None);

    fake.embed_fails.store(true, Ordering::SeqCst);
    let indexed = app.index(&workspace).await.unwrap();
    assert!(indexed.vectors_missing);
    // The service answers again, so an embedding request would be served.
    fake.embed_fails.store(false, Ordering::SeqCst);

    let requests = fake.embed_requests.load(Ordering::SeqCst);
    let maps = app
        .fuse_through_maps(&workspace, "translator")
        .await
        .unwrap();
    assert_eq!(
        fake.embed_requests.load(Ordering::SeqCst),
        requests,
        "the semantic map embedded the query on a vectorless snapshot"
    );
    assert_eq!(
        maps.warnings,
        [
            "Semantic retrieval unavailable: the index has no vectors (the embedding service was unreachable when it was built)"
        ]
    );
    assert!(!maps.rows.is_empty(), "lexical answered");

    let dir = temp.path().join("records");
    let recorded = app
        .record_through_maps(
            &workspace,
            "translator",
            &lci_maps::run_record::RecordOptions::new(&dir),
        )
        .await
        .unwrap();
    assert_eq!(
        fake.embed_requests.load(Ordering::SeqCst),
        requests,
        "the recorded run embedded the query on a vectorless snapshot"
    );
    let path = recorded.record.expect("the record was written");
    let text = std::fs::read_to_string(path).unwrap();
    let run: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    let enabled: Vec<(&str, bool)> = run["plan"]["maps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|map| {
            (
                map["map"].as_str().unwrap(),
                map["enabled"].as_bool().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        enabled,
        [
            ("vec.chunk.code", false),
            ("lexical", true),
            ("lsp.symbols", false),
            ("prior.source_role", true)
        ]
    );
    task.abort();
}

/// N105: a debug build of `lci traverse --maps seeds.hybrid` overflowed
/// Windows' 1 MiB main-thread stack when the hybrid search created the index
/// with the embedder unreachable. Runs the real binary against a tiny tree and
/// SCIP index; the embedder URL is a port nothing listens on (the index is
/// built without vectors), so only the process's own stack decides the result.
#[test]
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn traverse_seeds_hybrid_runs_through_the_binary_without_overflowing_its_stack() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("tree");
    let source = "pub fn parse_widget_config() {}\n";
    write(&root, "src/lib.rs", source);
    let mut document = scip::types::Document::default();
    document.relative_path = "src/lib.rs".to_owned();
    let symbol = "rust-analyzer cargo demo 0.1.0 widget/parse_widget_config().".to_owned();
    let mut definition = scip::types::Occurrence::default();
    definition.range = vec![0, 7, 25];
    definition.symbol = symbol.clone();
    definition.symbol_roles = 1;
    definition.enclosing_range = vec![0, 0, 0, source.trim_end().len() as i32];
    document.occurrences.push(definition);
    let mut info = scip::types::SymbolInformation::default();
    info.symbol = symbol;
    info.kind = scip::types::symbol_information::Kind::Function.into();
    document.symbols.push(info);
    let mut message = scip::types::Index::default();
    message.documents.push(document);
    let index = temp.path().join("index.scip");
    scip::write_message_to_file(&index, message).unwrap();

    let config = temp.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "data_dir = '{}'\nembedding_url = 'http://127.0.0.1:9/v1'\n\n[channels]\nsemantic = false\nlexical = true\nlsp = false\nreranker = false\n\n[index]\nfreshness = \"manual\"\n\n[traverse]\nenabled = true\n",
            temp.path().join("data").display()
        ),
    )
    .unwrap();

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
    command
        .current_dir(temp.path())
        .arg("--config")
        .arg(&config)
        .arg("traverse")
        .arg("--scip")
        .arg(&index)
        .arg("--root")
        .arg(&root)
        .args([
            "--maps",
            "seeds.hybrid",
            "--question",
            "parse widget config",
        ]);
    strip_lci_env(&mut command);
    let output = command.output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("overflowed"), "{stderr}");
    assert!(output.status.success(), "{:?}: {stderr}", output.status);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {}\nstderr: {stderr}",
            String::from_utf8_lossy(&output.stdout)
        )
    });
    assert_eq!(report["mode"], "traverse", "{report}");
}
