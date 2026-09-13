use super::*;

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
