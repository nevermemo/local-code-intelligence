use super::*;

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
    let (_temp, mut config, _fake, models_task) = fixture().await;
    config.channels.semantic = false;
    let app = App::open(config).await.unwrap();
    let status = serde_json::to_value(app.service_status().await).unwrap();
    assert_eq!(status["ready"], true);
    assert_eq!(status["can_create_semantic_index"], true);
    models_task.abort();
}
