use super::*;

#[tokio::test]
async fn http_mcp_initialization_tools_and_health() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("mcp-repository");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    let token = tokio_util::sync::CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound_port = listener.local_addr().unwrap().port();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = local_code_intelligence::server::router(app, token.clone(), bound_port);
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

#[tokio::test]
async fn rest_surface_mirrors_the_mcp_tools() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("rest-repository");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    let token = tokio_util::sync::CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound_port = listener.local_addr().unwrap().port();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = local_code_intelligence::server::router(app, token.clone(), bound_port);
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();

    let openapi = client
        .get(format!("{base}/openapi.json"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(openapi["openapi"], "3.0.3");
    for path in [
        "/v1/index_workspace",
        "/v1/index_status",
        "/v1/list_indexed_files",
        "/v1/watch_workspace",
        "/v1/unwatch_workspace",
        "/v1/search_symbols",
        "/v1/find_definition",
        "/v1/find_references",
        "/v1/search_code",
        "/v1/service_status",
    ] {
        assert!(openapi["paths"][path].is_object(), "{path}: {openapi}");
    }

    let indexed = client
        .post(format!("{base}/v1/index_workspace"))
        .json(&json!({"workspace_path": workspace}))
        .send()
        .await
        .unwrap();
    assert_eq!(indexed.status(), StatusCode::OK);
    let indexed = indexed.json::<Value>().await.unwrap();
    assert!(indexed["embedded_chunks"].is_number(), "{indexed}");

    let status = client
        .post(format!("{base}/v1/index_status"))
        .json(&json!({"workspace_path": workspace}))
        .send()
        .await
        .unwrap();
    assert_eq!(status.status(), StatusCode::OK);
    assert!(status.json::<Value>().await.unwrap()["embedding_dimension"].is_number());

    let searched = client
        .post(format!("{base}/v1/search_code"))
        .json(&json!({"workspace_path": workspace, "query": "translator"}))
        .send()
        .await
        .unwrap();
    assert_eq!(searched.status(), StatusCode::OK);
    assert!(
        searched.json::<Value>().await.unwrap()["results"]
            .as_array()
            .is_some_and(|results| !results.is_empty())
    );

    let ready = client
        .get(format!("{base}/v1/service_status"))
        .send()
        .await
        .unwrap();
    assert_eq!(ready.status(), StatusCode::OK);
    assert!(ready.json::<Value>().await.unwrap()["ready"].is_boolean());

    let failed = client
        .post(format!("{base}/v1/index_status"))
        .json(&json!({"workspace_path": "   "}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(failed.json::<Value>().await.unwrap()["error"].is_string());

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn non_mcp_routes_reject_dns_rebinding_hosts_and_foreign_origins() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("rebinding-repository");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    let token = tokio_util::sync::CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound_port = listener.local_addr().unwrap().port();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = local_code_intelligence::server::router(app, token.clone(), bound_port);
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let client = reqwest::Client::new();
    let body = json!({"workspace_path": workspace});

    // A page whose domain re-resolved to 127.0.0.1 sends its own Host.
    let rebinding_host = format!("evil.test:{bound_port}");
    for (method, path) in [
        ("POST", "/v1/index_status"),
        ("POST", "/v1/search_code"),
        ("GET", "/openapi.json"),
        ("GET", "/health"),
        ("GET", "/ready"),
    ] {
        let request = match method {
            "POST" => client.post(format!("{base}{path}")).json(&body),
            _ => client.get(format!("{base}{path}")),
        };
        let response = request
            .header("Host", &rebinding_host)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::FORBIDDEN, "{path}");
    }

    // A loopback Host with a foreign browser Origin is rejected too.
    let foreign_origin = client
        .post(format!("{base}/v1/index_status"))
        .header("Origin", "https://untrusted.example")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(foreign_origin.status(), StatusCode::FORBIDDEN);
    assert!(foreign_origin.json::<Value>().await.unwrap()["error"].is_string());

    // This server's own loopback origin, and plain non-browser clients
    // (no Origin), keep working.
    let same_origin = client
        .post(format!("{base}/v1/index_status"))
        .header("Origin", format!("http://127.0.0.1:{bound_port}"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(same_origin.status(), StatusCode::OK);
    let plain = client.get(format!("{base}/health")).send().await.unwrap();
    assert_eq!(plain.status(), StatusCode::OK);

    token.cancel();
    server.abort();
    models_task.abort();
}

// ---------------------------------------------------------------------------
// Search profiles: `/mcp/<name>` and `POST /v1/search_code?search_profile=<name>`
// run `search_code` with a configured `[search_profiles.<name>]`. Nothing else
// differs between the endpoints, and a server with no profiles is the server
// it always was.
// ---------------------------------------------------------------------------

/// `fast` (no reranker) and `deep` (a rerank window of three) next to a
/// top-level window of five and no language server.
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
    ]);
    config
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

/// Serves `app` on an ephemeral loopback port: the base URL, the token that
/// stops the MCP sessions, and the server task.
async fn serve_on_loopback(
    app: Arc<App>,
) -> (
    String,
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let token = tokio_util::sync::CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bound_port = listener.local_addr().unwrap().port();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let router = local_code_intelligence::server::router(app, token.clone(), bound_port);
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (base, token, server)
}

async fn mcp_post(
    client: &reqwest::Client,
    url: &str,
    session: Option<&str>,
    message: &Value,
) -> reqwest::Response {
    let mut request = client
        .post(url)
        .header("Accept", "application/json, text/event-stream")
        .json(message);
    if let Some(session) = session {
        request = request.header("mcp-session-id", session);
    }
    request.send().await.unwrap()
}

fn initialize_message() -> Value {
    json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"integration-test","version":"1"}}})
}

/// The JSON-RPC response in a Streamable HTTP body, which is either plain
/// JSON or a stream of `data:` events (some of them empty priming events).
fn rpc_response(body: &str) -> Value {
    if let Ok(value) = serde_json::from_str::<Value>(body) {
        return value;
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|data| serde_json::from_str::<Value>(data.trim()).ok())
        .find(|message| message.get("result").is_some() || message.get("error").is_some())
        .unwrap_or_else(|| panic!("no JSON-RPC response in {body}"))
}

/// Opens an MCP session on `path`: its id and the `initialize` result.
async fn mcp_initialize(client: &reqwest::Client, base: &str, path: &str) -> (String, Value) {
    let url = format!("{base}{path}");
    let response = mcp_post(client, &url, None, &initialize_message()).await;
    assert!(
        response.status().is_success(),
        "{path}: {}",
        response.status()
    );
    let session = response
        .headers()
        .get("mcp-session-id")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let result = rpc_response(&response.text().await.unwrap())["result"].clone();
    let initialized = mcp_post(
        client,
        &url,
        Some(&session),
        &json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    assert!(initialized.status().is_success(), "{path}");
    (session, result)
}

/// The `result` of one request in an open session.
async fn mcp_result(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    session: &str,
    method: &str,
    params: Value,
) -> Value {
    let response = mcp_post(
        client,
        &format!("{base}{path}"),
        Some(session),
        &json!({"jsonrpc":"2.0","id":7,"method":method,"params":params}),
    )
    .await;
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{path} {method}: {status} {body}");
    let response = rpc_response(&body);
    assert!(response.get("error").is_none(), "{path} {method}: {body}");
    response["result"].clone()
}

/// The structured result of `search_code` for `translator` (three results).
async fn mcp_search_code(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    session: &str,
    workspace: &Path,
) -> Value {
    let result = mcp_result(
        client,
        base,
        path,
        session,
        "tools/call",
        json!({"name":"search_code","arguments":{"workspace_path":workspace,"query":"translator","top_k":3}}),
    )
    .await;
    assert_ne!(result["isError"], true, "{path}: {result}");
    result["structuredContent"].clone()
}

/// A search report as JSON without what differs between two otherwise
/// identical searches: the stage timings and the index wait.
fn without_clocks(mut report: Value) -> Value {
    let object = report.as_object_mut().unwrap();
    object.remove("timings");
    object["index"].as_object_mut().unwrap().remove("wait_ms");
    report
}

/// Whether `report` has exactly the keys a search report had before profiles
/// existed: a search that names no profile has not one more.
fn has_the_unprofiled_report_keys(report: &Value) -> bool {
    let mut keys: Vec<&str> = report
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys == [
        "candidate_count",
        "filters",
        "index",
        "query",
        "reranked",
        "results",
        "timings",
        "warning",
        "workspace",
    ]
}

#[tokio::test]
async fn each_search_profile_has_its_own_mcp_endpoint_with_the_same_tools() {
    let (temp, config, fake, models_task) = fixture().await;
    let workspace = temp.path().join("mcp-profiles");
    write_profile_workspace(&workspace);
    let app = Arc::new(
        App::open(profiled_config(config))
            .await
            .unwrap()
            .with_candidate_dump(None),
    );
    app.index(&workspace).await.unwrap();
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();

    // Every endpoint opens a session and answers `initialize` with the same
    // server info, capabilities, and instructions.
    let (default_session, default_info) = mcp_initialize(&client, &base, "/mcp").await;
    let (fast_session, fast_info) = mcp_initialize(&client, &base, "/mcp/fast").await;
    let (deep_session, deep_info) = mcp_initialize(&client, &base, "/mcp/deep").await;
    assert!(default_info["serverInfo"].is_object(), "{default_info}");
    assert!(
        default_info["instructions"]
            .as_str()
            .is_some_and(|text| text.contains("search_code"))
    );
    assert_eq!(fast_info, default_info);
    assert_eq!(deep_info, default_info);
    // The trailing-slash form of the default endpoint still answers.
    let slash = mcp_post(
        &client,
        &format!("{base}/mcp/"),
        None,
        &initialize_message(),
    )
    .await;
    assert!(slash.status().is_success(), "{}", slash.status());

    // The same tools with the same schemas on every endpoint, and no
    // profile argument on `search_code`.
    let default_tools = mcp_result(
        &client,
        &base,
        "/mcp",
        &default_session,
        "tools/list",
        json!({}),
    )
    .await;
    for (path, session) in [("/mcp/fast", &fast_session), ("/mcp/deep", &deep_session)] {
        let tools = mcp_result(&client, &base, path, session, "tools/list", json!({})).await;
        assert_eq!(tools, default_tools, "{path}");
    }
    let tools = default_tools["tools"].as_array().unwrap();
    let mut names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "find_definition",
            "find_references",
            "index_status",
            "index_workspace",
            "list_indexed_files",
            "search_code",
            "search_symbols",
            "service_status",
            "unwatch_workspace",
            "watch_workspace",
        ]
    );
    let search_tool = tools
        .iter()
        .find(|tool| tool["name"] == "search_code")
        .unwrap();
    let mut arguments: Vec<&str> = search_tool["inputSchema"]["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    arguments.sort_unstable();
    assert_eq!(
        arguments,
        [
            "exclude_paths",
            "include_paths",
            "languages",
            "query",
            "source_roles",
            "top_k",
            "workspace_path",
        ]
    );

    // `/mcp` is the default search: reranked, and no `search_profile` key.
    let default_hits = mcp_search_code(&client, &base, "/mcp", &default_session, &workspace).await;
    assert!(
        has_the_unprofiled_report_keys(&default_hits),
        "{default_hits}"
    );
    assert_eq!(default_hits["reranked"], true, "{default_hits}");
    assert_eq!(default_hits["candidate_count"], 5);

    // `/mcp/fast` reports itself and never calls the reranker.
    let reranks_before = fake.rerank_requests.load(Ordering::SeqCst);
    let fast_hits = mcp_search_code(&client, &base, "/mcp/fast", &fast_session, &workspace).await;
    assert_eq!(fast_hits["search_profile"], "fast", "{fast_hits}");
    assert_eq!(fast_hits["reranked"], false, "{fast_hits}");
    assert_eq!(fast_hits["candidate_count"], 5);
    assert_eq!(fake.rerank_requests.load(Ordering::SeqCst), reranks_before);

    // `/mcp/deep` shows the reranker three candidates.
    let deep_hits = mcp_search_code(&client, &base, "/mcp/deep", &deep_session, &workspace).await;
    assert_eq!(deep_hits["search_profile"], "deep", "{deep_hits}");
    assert_eq!(deep_hits["reranked"], true, "{deep_hits}");
    assert_eq!(deep_hits["candidate_count"], 3);
    assert_eq!(
        fake.rerank_requests.load(Ordering::SeqCst),
        reranks_before + 1
    );

    // Another search on `/mcp` after the profiled ones is still the default.
    let again = mcp_search_code(&client, &base, "/mcp", &default_session, &workspace).await;
    assert_eq!(without_clocks(again), without_clocks(default_hits));

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn an_unknown_mcp_profile_path_is_not_found_and_a_session_stays_on_its_endpoint() {
    let (_temp, config, _fake, models_task) = fixture().await;
    let app = Arc::new(
        App::open(profiled_config(config))
            .await
            .unwrap()
            .with_candidate_dump(None),
    );
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();

    // No such profile, so no such endpoint: the default endpoint does not
    // answer for it, with a profile name or anything else.
    for path in ["/mcp/nope", "/mcp/fast/extra", "/mcp/default"] {
        let response = mcp_post(
            &client,
            &format!("{base}{path}"),
            None,
            &initialize_message(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }

    // A session id is known to the endpoint that issued it and to no other.
    let (default_session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let (fast_session, _) = mcp_initialize(&client, &base, "/mcp/fast").await;
    let list = json!({"jsonrpc":"2.0","id":2,"method":"tools/list"});
    for (path, session) in [
        ("/mcp/deep", &fast_session),
        ("/mcp", &fast_session),
        ("/mcp/fast", &default_session),
        ("/mcp/deep", &default_session),
    ] {
        let response = mcp_post(&client, &format!("{base}{path}"), Some(session), &list).await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{path}");
    }
    for (path, session) in [("/mcp", &default_session), ("/mcp/fast", &fast_session)] {
        let response = mcp_post(&client, &format!("{base}{path}"), Some(session), &list).await;
        assert!(
            response.status().is_success(),
            "{path}: {}",
            response.status()
        );
    }

    // A browser origin that is not this server's is refused on a profile
    // endpoint as on `/mcp`.
    let rejected = client
        .post(format!("{base}/mcp/fast"))
        .header("Origin", "https://untrusted.example")
        .header("Accept", "application/json, text/event-stream")
        .json(&initialize_message())
        .send()
        .await
        .unwrap();
    assert_eq!(rejected.status(), StatusCode::FORBIDDEN);

    token.cancel();
    server.abort();
    models_task.abort();
}

/// What the default MCP endpoint and the unprofiled REST call return is what
/// the application's own `search_with_filters` returns, timings aside. With
/// `profiled` the configuration also has profiles, which must not change
/// that; without it the server has no profile endpoint and no profile name.
async fn assert_default_endpoints_return_what_the_application_does(profiled: bool) {
    use local_code_intelligence::filter::FilterRequest;

    let (temp, mut config, fake, models_task) = fixture().await;
    config = if profiled {
        profiled_config(config)
    } else {
        config.channels.lsp = false;
        config.rerank_candidate_count = 5;
        config
    };
    let workspace = temp.path().join("mcp-default-parity");
    write_profile_workspace(&workspace);
    let app = Arc::new(App::open(config).await.unwrap().with_candidate_dump(None));
    app.index(&workspace).await.unwrap();
    let (base, token, server) = serve_on_loopback(app.clone()).await;
    let client = reqwest::Client::new();

    let expected = app
        .search_with_filters(&workspace, "translator", Some(3), FilterRequest::default())
        .await
        .unwrap();
    assert!(expected.reranked, "{:?}", expected.warning);
    assert!(expected.warning.is_none(), "{:?}", expected.warning);
    let expected = serde_json::to_value(&expected).unwrap();
    assert!(has_the_unprofiled_report_keys(&expected), "{expected}");
    let expected = without_clocks(expected);

    // MCP `search_code` on `/mcp`.
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let by_mcp = mcp_search_code(&client, &base, "/mcp", &session, &workspace).await;
    assert_eq!(without_clocks(by_mcp), expected);

    // REST `search_code`, with no query string and with one that names no
    // profile.
    for url in [
        format!("{base}/v1/search_code"),
        format!("{base}/v1/search_code?unrelated=1"),
    ] {
        let response = client
            .post(&url)
            .json(&json!({"workspace_path": workspace, "query": "translator", "top_k": 3}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{url}");
        let by_rest = response.json::<Value>().await.unwrap();
        assert_eq!(without_clocks(by_rest), expected, "{url}");
    }

    // Each of the four searches reranked once.
    assert_eq!(fake.rerank_requests.load(Ordering::SeqCst), 4);

    if !profiled {
        // No profiles, so no endpoint under `/mcp/` and no name to pass.
        let response = mcp_post(
            &client,
            &format!("{base}/mcp/fast"),
            None,
            &initialize_message(),
        )
        .await;
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let response = client
            .post(format!("{base}/v1/search_code?search_profile=fast"))
            .json(&json!({"workspace_path": workspace, "query": "translator"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let error = response.json::<Value>().await.unwrap()["error"]
            .as_str()
            .unwrap()
            .to_owned();
        assert!(
            error.contains("unknown search profile") && error.contains("no search profiles"),
            "{error}"
        );
    }

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn default_endpoints_return_what_the_application_does_when_no_profiles_are_configured() {
    assert_default_endpoints_return_what_the_application_does(false).await;
}

#[tokio::test]
async fn default_endpoints_return_what_the_application_does_when_profiles_are_configured() {
    assert_default_endpoints_return_what_the_application_does(true).await;
}

#[tokio::test]
async fn rest_search_code_takes_an_optional_search_profile() {
    let (temp, config, fake, models_task) = fixture().await;
    let workspace = temp.path().join("rest-profiles");
    write_profile_workspace(&workspace);
    let app = Arc::new(
        App::open(profiled_config(config))
            .await
            .unwrap()
            .with_candidate_dump(None),
    );
    app.index(&workspace).await.unwrap();
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let search = |query: &str| {
        client
            .post(format!("{base}/v1/search_code{query}"))
            .json(&json!({"workspace_path": workspace, "query": "translator", "top_k": 3}))
            .send()
    };

    let plain = search("").await.unwrap();
    assert_eq!(plain.status(), StatusCode::OK);
    let plain = plain.json::<Value>().await.unwrap();
    assert!(plain.get("search_profile").is_none(), "{plain}");
    assert_eq!(plain["reranked"], true);
    assert_eq!(plain["candidate_count"], 5);

    let reranks_before = fake.rerank_requests.load(Ordering::SeqCst);
    let fast = search("?search_profile=fast").await.unwrap();
    assert_eq!(fast.status(), StatusCode::OK);
    let fast = fast.json::<Value>().await.unwrap();
    assert_eq!(fast["search_profile"], "fast", "{fast}");
    assert_eq!(fast["reranked"], false);
    assert_eq!(fake.rerank_requests.load(Ordering::SeqCst), reranks_before);

    let deep = search("?search_profile=deep").await.unwrap();
    assert_eq!(deep.status(), StatusCode::OK);
    let deep = deep.json::<Value>().await.unwrap();
    assert_eq!(deep["search_profile"], "deep", "{deep}");
    assert_eq!(deep["reranked"], true);
    assert_eq!(deep["candidate_count"], 3);

    // An unknown name is the application's error, which names the profiles
    // there are.
    let unknown = search("?search_profile=nope").await.unwrap();
    assert_eq!(unknown.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let error = unknown.json::<Value>().await.unwrap()["error"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(error.contains("unknown search profile \"nope\""), "{error}");
    assert!(error.contains("configured: deep, fast"), "{error}");

    // The OpenAPI document lists the parameter on this operation only.
    let openapi = client
        .get(format!("{base}/openapi.json"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let parameters = openapi["paths"]["/v1/search_code"]["post"]["parameters"]
        .as_array()
        .unwrap();
    assert_eq!(parameters.len(), 1, "{openapi}");
    assert_eq!(parameters[0]["name"], "search_profile");
    assert_eq!(parameters[0]["in"], "query");
    assert_eq!(parameters[0]["required"], false);
    assert_eq!(parameters[0]["schema"]["type"], "string");
    for (path, operations) in openapi["paths"].as_object().unwrap() {
        if path != "/v1/search_code" {
            assert!(
                operations
                    .as_object()
                    .unwrap()
                    .values()
                    .all(|operation| operation.get("parameters").is_none()),
                "{path}"
            );
        }
    }

    token.cancel();
    server.abort();
    models_task.abort();
}
