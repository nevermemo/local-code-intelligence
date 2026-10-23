use super::*;

#[tokio::test]
async fn java_readiness_checks_the_launcher_jar_in_the_install_directory() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    let install = temp.path().join("jdtls");
    let plugins = install.join("plugins");
    std::fs::create_dir_all(&plugins).unwrap();
    std::fs::write(plugins.join("org.eclipse.equinox.launcher.jar"), b"").unwrap();
    config.java.path = Some(install.to_string_lossy().into_owned());
    let app = App::open(config).await.unwrap();
    let status = app.service_status().await;
    let component = status
        .components
        .iter()
        .find(|component| component.name == "java_language_server_available")
        .unwrap();
    assert!(component.ready, "{:?}", component.detail);
    assert!(!component.required);
    assert!(
        !status
            .degraded
            .iter()
            .any(|entry| entry.name == "java_language_server_available")
    );
    models_task.abort();
}

#[tokio::test]
async fn java_readiness_without_a_launcher_jar_is_degraded_but_optional() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    let install = temp.path().join("jdtls");
    std::fs::create_dir_all(
        install
            .join("plugins")
            .join("org.eclipse.equinox.launcher_1.0.0.jar"),
    )
    .unwrap();
    config.java.path = Some(install.to_string_lossy().into_owned());
    // The overall `ready` below needs the fake embedder's probes to answer.
    config.readiness_timeout_seconds = LOADED_BUDGET_SECONDS;
    let app = App::open(config).await.unwrap();
    let status = app.service_status().await;
    let component = status
        .components
        .iter()
        .find(|component| component.name == "java_language_server_available")
        .unwrap();
    assert!(!component.ready);
    assert!(!component.required);
    assert!(
        component
            .detail
            .as_deref()
            .unwrap()
            .contains("launcher jar not found")
    );
    assert!(status.ready);
    assert!(
        status
            .degraded
            .iter()
            .any(|entry| entry.name == "java_language_server_available")
    );
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
    let listener = loopback_tokio_listener();
    let bound_port = listener.local_addr().unwrap().port();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = local_code_intelligence::server::router(app, token.clone(), bound_port);
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
    // The fake embedder must answer its probes inside the timeout: 1 s timed
    // out under load. The failing components fail by refusal, not by it.
    config.readiness_timeout_seconds = LOADED_BUDGET_SECONDS;
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
    let listener = loopback_tokio_listener();
    let bound_port = listener.local_addr().unwrap().port();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = local_code_intelligence::server::router(app, token.clone(), bound_port);
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

#[tokio::test]
async fn readiness_lists_the_data_dir_length_component_as_optional() {
    // A short data_dir never trips the Windows path-length warning, so the
    // component is present, optional, ready, and not among the degraded ones.
    let (_temp, mut config, _fake, models_task) = fixture().await;
    // The overall `ready` below needs the fake embedder's probes to answer.
    config.readiness_timeout_seconds = LOADED_BUDGET_SECONDS;
    let app = App::open(config).await.unwrap();
    let status = serde_json::to_value(app.service_status().await).unwrap();
    let component = status["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|component| component["name"] == "data_dir_path_length")
        .expect("the data_dir length component is reported");
    assert_eq!(component["required"], false);
    assert_eq!(component["ready"], true);
    assert!(
        component["detail"]
            .as_str()
            .unwrap()
            .starts_with("data_dir is ")
    );
    assert_eq!(status["ready"], true);
    assert!(
        !status["degraded"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["name"] == "data_dir_path_length")
    );
    models_task.abort();
}

#[tokio::test]
async fn readiness_still_probes_the_embedding_service_when_semantic_search_is_off() {
    // `[channels].semantic = false` only skips query embedding. Indexing
    // still embeds every chunk, so the embedding service stays a required
    // dependency and an unreachable one still makes the service not ready.
    let temp = tempfile::tempdir().unwrap();
    let mut config = Config {
        data_dir: temp.path().join("data"),
        embedding_url: "http://127.0.0.1:1/v1".into(),
        reranker_url: "http://127.0.0.1:1/rerank".into(),
        readiness_timeout_seconds: 1,
        ..Config::default()
    };
    config.channels.semantic = false;
    config.channels.reranker = false;
    let app = App::open(config).await.unwrap();
    let status = serde_json::to_value(app.service_status().await).unwrap();
    assert_eq!(status["ready"], false);
    assert_eq!(status["can_create_semantic_index"], false);
    for name in ["embedding_endpoint_reachable", "embedding_model_listed"] {
        let component = status["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|component| component["name"] == name)
            .unwrap_or_else(|| panic!("{name} is still reported"));
        assert_eq!(component["required"], true, "{name}");
        assert_eq!(component["ready"], false, "{name}");
    }

    // A healthy embedding service keeps the service ready with semantic off.
    // Its probes get the loaded-machine budget: the default 5 s timed out
    // under load.
    let (_temp, mut config, _fake, models_task) = fixture().await;
    config.channels.semantic = false;
    config.readiness_timeout_seconds = LOADED_BUDGET_SECONDS;
    let app = App::open(config).await.unwrap();
    let status = serde_json::to_value(app.service_status().await).unwrap();
    assert_eq!(status["ready"], true);
    assert_eq!(status["can_create_semantic_index"], true);
    models_task.abort();
}

/// pyright-langserver exits 1 on `--version` and gopls exits 2: `/ready` runs
/// the probe of each adapter (a start whose status is ignored; `gopls
/// version`), so a stock install reports both ready.
#[cfg(windows)]
#[tokio::test]
async fn python_and_go_readiness_use_their_own_probes() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    let pyright = temp.path().join("pyright-langserver.cmd");
    std::fs::write(&pyright, "@exit /b 1\r\n").unwrap();
    let gopls = temp.path().join("gopls.cmd");
    std::fs::write(
        &gopls,
        "@if \"%1\"==\"version\" (exit /b 0) else (exit /b 2)\r\n",
    )
    .unwrap();
    config.python.path = Some(pyright.to_string_lossy().into_owned());
    config.go.path = Some(gopls.to_string_lossy().into_owned());
    config.readiness_timeout_seconds = LOADED_BUDGET_SECONDS;
    let app = App::open(config).await.unwrap();
    let status = app.service_status().await;
    for (name, provider) in [
        ("python_language_server_available", "pyright"),
        ("go_language_server_available", "gopls"),
    ] {
        let component = status
            .components
            .iter()
            .find(|component| component.name == name)
            .unwrap();
        assert!(component.ready, "{name}: {:?}", component.detail);
        assert!(
            component
                .detail
                .as_deref()
                .unwrap()
                .starts_with(&format!("configured {provider} command")),
            "{:?}",
            component.detail
        );
    }
    models_task.abort();
}
