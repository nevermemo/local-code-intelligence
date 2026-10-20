use super::*;

#[tokio::test]
async fn http_mcp_initialization_tools_and_health() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("mcp-repository");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    let token = tokio_util::sync::CancellationToken::new();
    let listener = loopback_tokio_listener();
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
    let listener = loopback_tokio_listener();
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
    assert_eq!(openapi["openapi"], "3.1.0");
    for (path, method) in [
        ("/v1/index_workspace", "post"),
        ("/v1/index_status", "post"),
        ("/v1/list_indexed_files", "post"),
        ("/v1/watch_workspace", "post"),
        ("/v1/unwatch_workspace", "post"),
        ("/v1/release_workspace", "post"),
        ("/v1/search_symbols", "post"),
        ("/v1/find_definition", "post"),
        ("/v1/find_references", "post"),
        ("/v1/search_code", "post"),
        ("/v1/service_status", "get"),
    ] {
        let operation = &openapi["paths"][path][method];
        assert!(operation.is_object(), "{path}: {openapi}");
        assert!(
            operation["responses"]["403"].is_object(),
            "{path}: {operation}"
        );
        if method == "post" {
            let schema = &operation["requestBody"]["content"]["application/json"]["schema"];
            assert!(schema.get("$schema").is_none(), "{path}: {schema}");
            for status in ["400", "415", "422"] {
                assert!(
                    operation["responses"][status]["content"]["text/plain"].is_object(),
                    "{path} response {status}: {operation}"
                );
            }
        }
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

/// How many lines the fake language server appended to `name` in its state
/// directory (one per process that saw the event); 0 when there is no file.
fn lsp_event_count(state: &Path, name: &str) -> usize {
    std::fs::read_to_string(state.join(name)).map_or(0, |text| text.lines().count())
}

/// How many fake language-server processes have been started under `state`.
fn lsp_spawn_count(state: &Path) -> u64 {
    std::fs::read_to_string(state.join("spawn_count.txt"))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

/// `POST /v1/release_workspace` stops the language servers of the workspace
/// it names, says which, and changes nothing else: `index_status` then shows
/// them not running, a repeat finds nothing to stop, the next navigation call
/// starts a server again, and there is still no MCP tool for it.
#[tokio::test]
async fn rest_release_workspace_stops_the_language_servers_of_that_workspace() {
    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("lsp-state");
    let workspace = temp.path().join("release-repository");
    write_csharp_workspace(&workspace);
    let app = Arc::new(
        App::open(with_fake_csharp_server(config, &state, &[], 5))
            .await
            .unwrap()
            .with_candidate_dump(None),
    );
    app.index(&workspace).await.unwrap();
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let call = |path: &str, workspace: &Path| {
        client
            .post(format!("{base}{path}"))
            .json(&json!({"workspace_path": workspace}))
            .send()
    };
    let navigate = || {
        client
            .post(format!("{base}/v1/search_symbols"))
            .json(&json!({"workspace_path": workspace, "query": "Calculator"}))
            .send()
    };

    // A navigation call starts the server.
    let symbols = navigate().await.unwrap();
    assert_eq!(symbols.status(), StatusCode::OK);
    let status = call("/v1/index_status", &workspace)
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(status["analyzers_running"]["csharp-ls"], true, "{status}");
    assert_eq!(status["csharp_analyzer_running"], true, "{status}");
    assert_eq!(lsp_spawn_count(&state), 1);

    // The release names the workspace and the server it stopped, and has
    // finished the whole protocol exchange by the time it answers.
    let released = call("/v1/release_workspace", &workspace).await.unwrap();
    assert_eq!(released.status(), StatusCode::OK);
    let released = released.json::<Value>().await.unwrap();
    assert_eq!(released["released"], json!(["csharp-ls"]), "{released}");
    assert_eq!(released["workspace"], status["workspace"], "{released}");
    let mut keys: Vec<&str> = released
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["profile_hash", "released", "workspace"]);
    assert_eq!(lsp_event_count(&state, "shutdown_seen.txt"), 1);
    assert_eq!(lsp_event_count(&state, "exit_seen.txt"), 1);

    // Nothing is running any more, in the map and in the older booleans.
    let status = call("/v1/index_status", &workspace)
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(status["analyzers_running"]["csharp-ls"], false, "{status}");
    assert_eq!(status["csharp_analyzer_running"], false, "{status}");

    // A repeat has nothing to stop; that is not an error.
    let again = call("/v1/release_workspace", &workspace).await.unwrap();
    assert_eq!(again.status(), StatusCode::OK);
    assert_eq!(again.json::<Value>().await.unwrap()["released"], json!([]));
    assert_eq!(lsp_event_count(&state, "shutdown_seen.txt"), 1);

    // No index is needed: an existing directory that was never indexed (and
    // so has no server) is an empty release, a directory that does not exist
    // is the usual workspace error.
    let unindexed = temp.path().join("never-indexed");
    std::fs::create_dir_all(&unindexed).unwrap();
    let empty = call("/v1/release_workspace", &unindexed).await.unwrap();
    assert_eq!(empty.status(), StatusCode::OK);
    assert_eq!(empty.json::<Value>().await.unwrap()["released"], json!([]));
    let missing = call(
        "/v1/release_workspace",
        &temp.path().join("no-such-directory"),
    )
    .await
    .unwrap();
    assert_eq!(missing.status(), StatusCode::INTERNAL_SERVER_ERROR);
    assert!(missing.json::<Value>().await.unwrap()["error"].is_string());

    // The next navigation call starts a fresh server on its own.
    let symbols = navigate().await.unwrap();
    assert_eq!(symbols.status(), StatusCode::OK);
    assert_eq!(lsp_spawn_count(&state), 2);
    let status = call("/v1/index_status", &workspace)
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(status["analyzers_running"]["csharp-ls"], true, "{status}");

    // The OpenAPI document lists the endpoint with the workspace body.
    let openapi = client
        .get(format!("{base}/openapi.json"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    let operation = &openapi["paths"]["/v1/release_workspace"]["post"];
    assert_eq!(operation["operationId"], "release_workspace", "{openapi}");
    assert!(
        operation["requestBody"]["content"]["application/json"]["schema"]["properties"]["workspace_path"]
            .is_object(),
        "{operation}"
    );

    // REST only: the MCP tool list does not grow.
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let tools = mcp_result(&client, &base, "/mcp", &session, "tools/list", json!({})).await;
    assert!(
        tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| !tool["name"].as_str().unwrap().contains("release")),
        "{tools}"
    );

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
    let listener = loopback_tokio_listener();
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
        ("POST", "/v1/release_workspace"),
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
pub(super) async fn serve_on_loopback(
    app: Arc<App>,
) -> (
    String,
    tokio_util::sync::CancellationToken,
    tokio::task::JoinHandle<()>,
) {
    let token = tokio_util::sync::CancellationToken::new();
    let listener = loopback_tokio_listener();
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
pub(super) async fn mcp_initialize(
    client: &reqwest::Client,
    base: &str,
    path: &str,
) -> (String, Value) {
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
pub(super) async fn mcp_result(
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
        "profile_hash",
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
    let mut expected = serde_json::to_value(&expected).unwrap();
    // The wire result is the report plus the profile hash, and nothing else.
    local_code_intelligence::app::stamp(&mut expected, app.profile_hash());
    assert!(has_the_unprofiled_report_keys(&expected), "{expected}");
    let expected = without_clocks(expected);

    // MCP `search_code` on `/mcp`.
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let by_mcp = mcp_search_code(&client, &base, "/mcp", &session, &workspace).await;
    assert_eq!(without_clocks(by_mcp), expected);

    // REST `search_code` with no query string.
    let body = json!({"workspace_path": workspace, "query": "translator", "top_k": 3});
    let response = client
        .post(format!("{base}/v1/search_code"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let by_rest = response.json::<Value>().await.unwrap();
    assert_eq!(without_clocks(by_rest), expected);
    // A query parameter that names no profile is refused before any search
    // (d388; it was ignored before).
    let response = client
        .post(format!("{base}/v1/search_code?unrelated=1"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::BAD_REQUEST);

    // Each of the three searches reranked once; the refused one did not run.
    assert_eq!(fake.rerank_requests.load(Ordering::SeqCst), 3);

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

// ---------------------------------------------------------------------------
// Search-only tool list and compact `search_code` result (roadmap N28 to N31),
// chosen by `[search_profiles.<name>]` keys and so by endpoint. Retrieval is
// the profile's own and these keys never change it; `/mcp` and every profile
// that sets none of the keys stay exactly what they were.
// ---------------------------------------------------------------------------

/// Profiles over the default retrieval (no language server, a rerank window
/// of ten, wider than the eight chunks) that differ only in their presentation:
/// `compact`, `capped` (10 lines per hit), `tight` (3000 characters in all),
/// `starved` (1000 characters, which cannot hold eight locators), `search`
/// (search-only tool list, full result) and `lean` (search-only and compact).
fn presentation_config(mut config: Config) -> Config {
    use local_code_intelligence::config::{McpResponse, McpToolList, SearchProfile};
    config.channels.lsp = false;
    config.rerank_candidate_count = 10;
    let compact = SearchProfile {
        response: Some(McpResponse::Compact),
        ..SearchProfile::default()
    };
    config.search_profiles = std::collections::BTreeMap::from([
        ("compact".to_owned(), compact),
        (
            "capped".to_owned(),
            SearchProfile {
                compact_max_hit_lines: Some(10),
                ..compact
            },
        ),
        (
            "tight".to_owned(),
            SearchProfile {
                compact_max_hit_lines: Some(1000),
                compact_max_response_chars: Some(3000),
                ..compact
            },
        ),
        (
            "starved".to_owned(),
            SearchProfile {
                compact_max_response_chars: Some(1000),
                ..compact
            },
        ),
        (
            "search".to_owned(),
            SearchProfile {
                tools: Some(McpToolList::Search),
                ..SearchProfile::default()
            },
        ),
        (
            "lean".to_owned(),
            SearchProfile {
                tools: Some(McpToolList::Search),
                ..compact
            },
        ),
    ]);
    config
}

/// The profile workspace plus `src/long.rs`: one hundred-line function, which
/// is one chunk, so a line cap has something to cut.
fn write_presentation_workspace(root: &Path) {
    write_profile_workspace(root);
    let mut long = String::from("fn translator_long() {\n");
    for step in 1..=98 {
        long.push_str(&format!(
            "    let step_{step} = {step}; // translator_long\n"
        ));
    }
    long.push_str("}\n");
    write(root, "src/long.rs", &long);
}

/// The whole JSON-RPC response of `tools/call`, errors included.
async fn mcp_call(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    session: &str,
    name: &str,
    arguments: Value,
) -> Value {
    let response = mcp_post(
        client,
        &format!("{base}{path}"),
        Some(session),
        &json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
    )
    .await;
    assert!(response.status().is_success(), "{path} {name}");
    rpc_response(&response.text().await.unwrap())
}

/// A `search_code` call for `translator` with every chunk of the workspace
/// asked for (`top_k` 8); the JSON-RPC response.
async fn mcp_search_everything(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    session: &str,
    workspace: &Path,
) -> Value {
    mcp_call(
        client,
        base,
        path,
        session,
        "search_code",
        json!({"workspace_path":workspace,"query":"translator","top_k":8}),
    )
    .await
}

fn structured(response: &Value) -> &Value {
    assert_ne!(response["result"]["isError"], true, "{response}");
    &response["result"]["structuredContent"]
}

fn tool_names(tools: &Value) -> Vec<&str> {
    let mut names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    names.sort_unstable();
    names
}

/// The keys of a JSON object, sorted: a parsed `Value` does not promise to
/// keep the order of the keys, so the SET is compared here and the order of
/// a response is checked on its text with [`assert_key_order`].
fn object_keys(value: &Value) -> Vec<&str> {
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys
}

/// The text content of a tool call response, exactly as the server wrote it.
fn text_content(response: &Value) -> &str {
    response["result"]["content"][0]["text"].as_str().unwrap()
}

/// Asserts that each of `keys` occurs as a JSON key of `text`, in this order.
fn assert_key_order(text: &str, keys: &[&str]) {
    let mut from = 0;
    for key in keys {
        let needle = format!("\"{key}\":");
        match text[from..].find(&needle) {
            Some(at) => from += at + needle.len(),
            None => panic!("key {key} not found in order after byte {from} of {text}"),
        }
    }
}

#[tokio::test]
async fn a_compact_profile_endpoint_serves_locators_and_code_in_the_order_of_the_full_result() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("mcp-compact");
    write_presentation_workspace(&workspace);
    let app = Arc::new(
        App::open(presentation_config(config))
            .await
            .unwrap()
            .with_candidate_dump(None),
    );
    app.index(&workspace).await.unwrap();
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (default_session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let (compact_session, _) = mcp_initialize(&client, &base, "/mcp/compact").await;
    let (capped_session, _) = mcp_initialize(&client, &base, "/mcp/capped").await;

    // `/mcp` is unchanged: the full report with every retrieval key.
    let full = mcp_search_everything(&client, &base, "/mcp", &default_session, &workspace).await;
    let full = structured(&full);
    assert!(has_the_unprofiled_report_keys(full), "{full}");
    let full_hits = full["results"].as_array().unwrap();
    assert_eq!(full_hits.len(), 8, "{full}");
    assert!(full_hits[0]["semantic_score"].is_number() || full_hits[0]["lexical_rank"].is_number());
    let locator = |hit: &Value| {
        (
            hit["relative_file_path"].clone(),
            hit["start_line"].clone(),
            hit["end_line"].clone(),
        )
    };
    let full_locators: Vec<_> = full_hits.iter().map(locator).collect();

    // `/mcp/compact` returns the same hits in the same order, each as a
    // locator, language, role and byte-identical code; nothing of the
    // retrieval metadata, and a short envelope.
    let compact_response =
        mcp_search_everything(&client, &base, "/mcp/compact", &compact_session, &workspace).await;
    let compact = structured(&compact_response);
    assert_eq!(
        object_keys(compact),
        [
            "filters",
            "index",
            "profile_hash",
            "results",
            "search_profile",
            "workspace"
        ],
        "{compact}"
    );
    // The text of the response keeps the key order the code writes: the
    // envelope, then per hit the locator first.
    let compact_text = text_content(&compact_response);
    assert_key_order(
        compact_text,
        &[
            "workspace",
            "search_profile",
            "filters",
            "index",
            "results",
            "relative_file_path",
            "start_line",
            "end_line",
            "language",
            "source_role",
            "code",
            "profile_hash",
        ],
    );
    assert!(
        compact_text.ends_with(&format!(
            ",\"profile_hash\":\"{}\"}}",
            compact["profile_hash"].as_str().unwrap()
        )),
        "{compact_text}"
    );
    let truncated_hit = &compact_text[compact_text
        .find("\"relative_file_path\":\"src/long.rs\"")
        .expect(compact_text)..];
    assert_key_order(
        truncated_hit,
        &[
            "relative_file_path",
            "start_line",
            "end_line",
            "language",
            "source_role",
            "code",
            "code_truncated",
            "excerpt_end_line",
        ],
    );
    assert_eq!(compact["search_profile"], "compact");
    assert_eq!(compact["workspace"]["path"], full["workspace"]["path"]);
    assert_eq!(object_keys(&compact["workspace"]), ["path"]);
    assert_eq!(compact["filters"], full["filters"]);
    let compact_hits = compact["results"].as_array().unwrap();
    assert_eq!(
        compact_hits.iter().map(locator).collect::<Vec<_>>(),
        full_locators
    );
    let mut long_hits = 0;
    for (compact_hit, full_hit) in compact_hits.iter().zip(full_hits) {
        let mut expected = vec![
            "code",
            "end_line",
            "language",
            "relative_file_path",
            "source_role",
            "start_line",
        ];
        if compact_hit.get("code_truncated").is_some() {
            expected.extend(["code_truncated", "excerpt_end_line"]);
            expected.sort_unstable();
        }
        assert_eq!(object_keys(compact_hit), expected, "{compact_hit}");
        assert_eq!(compact_hit["language"], full_hit["language"]);
        assert_eq!(compact_hit["source_role"], full_hit["source_role"]);
        let code = full_hit["code"].as_str().unwrap();
        if code.lines().count() <= 40 {
            assert_eq!(compact_hit["code"], full_hit["code"]);
            assert!(compact_hit.get("code_truncated").is_none());
        } else {
            long_hits += 1;
        }
    }
    // The hundred-line function is over the default 40-line cap.
    assert_eq!(long_hits, 1);
    let long = compact_hits
        .iter()
        .find(|hit| hit["relative_file_path"] == "src/long.rs")
        .unwrap();
    assert_eq!(long["code_truncated"], true);
    assert_eq!(long["excerpt_end_line"], 40);
    assert_eq!(long["end_line"], 100);
    let shown = long["code"].as_str().unwrap();
    assert!(shown.contains("lines 41-100"), "{shown}");
    assert!(shown.contains("read them from the file"), "{shown}");
    assert_eq!(shown.lines().count(), 41);

    // A smaller cap on another profile cuts at its own line count; short
    // chunks (one line) are untouched.
    let capped =
        mcp_search_everything(&client, &base, "/mcp/capped", &capped_session, &workspace).await;
    let capped = structured(&capped);
    let capped_long = capped["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|hit| hit["relative_file_path"] == "src/long.rs")
        .unwrap();
    assert_eq!(capped_long["excerpt_end_line"], 10);
    assert!(
        capped_long["code"]
            .as_str()
            .unwrap()
            .contains("lines 11-100")
    );
    assert_eq!(
        capped["results"]
            .as_array()
            .unwrap()
            .iter()
            .map(locator)
            .collect::<Vec<_>>(),
        full_locators
    );

    // The compact result is much smaller than the full one.
    assert!(compact.to_string().len() * 2 < full.to_string().len() + 3000);

    // Presentation is not retrieval: the REST endpoint of the same profile
    // is still the full report, and so is the plain `/mcp` afterwards.
    let rest = client
        .post(format!("{base}/v1/search_code?search_profile=compact"))
        .json(&json!({"workspace_path": workspace, "query": "translator", "top_k": 8}))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    assert_eq!(rest["search_profile"], "compact");
    assert!(rest["timings"].is_object(), "{rest}");
    assert!(rest["results"][0]["fusion_score"].is_number(), "{rest}");

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn markdown_comes_back_in_a_docs_list_beside_the_results_in_the_full_and_compact_shapes() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("mcp-docs");
    write_presentation_workspace(&workspace);
    write(
        &workspace,
        "README.md",
        "# Readme\nThe translator overview.\n\n## Layout\ntranslator translator in src.\n",
    );
    write(
        &workspace,
        "docs/guide.md",
        "# Guide\nHow to run the translator.\n",
    );
    write(&workspace, "docs/more.md", "# More\nA translator note.\n");
    let app = Arc::new(
        App::open(presentation_config(config))
            .await
            .unwrap()
            .with_candidate_dump(None),
    );
    app.index(&workspace).await.unwrap();
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (default_session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let (compact_session, _) = mcp_initialize(&client, &base, "/mcp/compact").await;

    // The full report: the old keys plus `docs`, no Markdown in `results`.
    let full = mcp_search_everything(&client, &base, "/mcp", &default_session, &workspace).await;
    let full = structured(&full);
    assert_eq!(
        object_keys(full),
        [
            "candidate_count",
            "docs",
            "filters",
            "index",
            "profile_hash",
            "query",
            "reranked",
            "results",
            "timings",
            "warning",
            "workspace"
        ],
        "{full}"
    );
    assert!(
        full["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit["language"] != "markdown")
    );
    assert_eq!(
        object_keys(&full["docs"]),
        [
            "candidate_count",
            "reranked",
            "results",
            "timings",
            "warning"
        ],
        "{full}"
    );
    let docs = full["docs"]["results"].as_array().unwrap();
    assert_eq!(docs.len(), 2);
    assert!(docs.iter().all(|hit| hit["language"] == "markdown"));

    // The compact shape lists the docs after the results.
    let compact_response =
        mcp_search_everything(&client, &base, "/mcp/compact", &compact_session, &workspace).await;
    let compact = structured(&compact_response);
    assert_eq!(
        object_keys(compact),
        [
            "docs",
            "filters",
            "index",
            "profile_hash",
            "results",
            "search_profile",
            "workspace"
        ],
        "{compact}"
    );
    assert_key_order(
        text_content(&compact_response),
        &[
            "workspace",
            "filters",
            "index",
            "results",
            "docs",
            "profile_hash",
        ],
    );
    let compact_docs = compact["docs"].as_array().unwrap();
    assert_eq!(compact_docs.len(), 2);
    for (compact_hit, full_hit) in compact_docs.iter().zip(docs) {
        assert_eq!(
            compact_hit["relative_file_path"],
            full_hit["relative_file_path"]
        );
        assert_eq!(compact_hit["code"], full_hit["code"]);
        assert_eq!(compact_hit["language"], "markdown");
    }
    assert!(
        compact["results"]
            .as_array()
            .unwrap()
            .iter()
            .all(|hit| hit["language"] != "markdown")
    );

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn the_compact_response_ceiling_shortens_code_but_keeps_every_locator_or_fails_with_advice() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("mcp-ceiling");
    write_presentation_workspace(&workspace);
    let app = Arc::new(
        App::open(presentation_config(config))
            .await
            .unwrap()
            .with_candidate_dump(None),
    );
    app.index(&workspace).await.unwrap();
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (default_session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let (tight_session, _) = mcp_initialize(&client, &base, "/mcp/tight").await;
    let (starved_session, _) = mcp_initialize(&client, &base, "/mcp/starved").await;

    let full = mcp_search_everything(&client, &base, "/mcp", &default_session, &workspace).await;
    let full_locators: Vec<(Value, Value, Value)> = structured(&full)["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|hit| {
            (
                hit["relative_file_path"].clone(),
                hit["start_line"].clone(),
                hit["end_line"].clone(),
            )
        })
        .collect();
    assert_eq!(full_locators.len(), 8);

    // 3000 characters cannot hold the hundred-line function in full (no
    // line cap on this profile), so it is shortened and the response says so.
    let tight =
        mcp_search_everything(&client, &base, "/mcp/tight", &tight_session, &workspace).await;
    let tight = structured(&tight);
    assert!(tight.to_string().chars().count() <= 3000, "{tight}");
    assert_eq!(tight["response_truncated"], true, "{tight}");
    let tight_hits = tight["results"].as_array().unwrap();
    assert_eq!(
        tight_hits
            .iter()
            .map(|hit| {
                (
                    hit["relative_file_path"].clone(),
                    hit["start_line"].clone(),
                    hit["end_line"].clone(),
                )
            })
            .collect::<Vec<_>>(),
        full_locators
    );
    let long = tight_hits
        .iter()
        .find(|hit| hit["relative_file_path"] == "src/long.rs")
        .unwrap();
    assert_eq!(long["code_truncated"], true, "{long}");
    assert!(
        long["code"]
            .as_str()
            .unwrap()
            .contains("read them from the file")
    );

    // 1000 characters cannot hold the locators of eight results: an error
    // that says what to change.
    let starved =
        mcp_search_everything(&client, &base, "/mcp/starved", &starved_session, &workspace).await;
    assert_eq!(starved["result"]["isError"], true, "{starved}");
    let text = starved["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("compact_max_response_chars"), "{text}");
    assert!(text.contains("top_k"), "{text}");
    assert!(text.len() < 600, "{text}");

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn a_search_only_profile_lists_four_tools_and_refuses_the_others() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("mcp-search-only");
    write_presentation_workspace(&workspace);
    let unindexed = temp.path().join("never-indexed");
    write(&unindexed, "lib.rs", "fn untouched() {}\n");
    let app = Arc::new(
        App::open(presentation_config(config))
            .await
            .unwrap()
            .with_candidate_dump(None),
    );
    app.index(&workspace).await.unwrap();
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();

    let (default_session, default_info) = mcp_initialize(&client, &base, "/mcp").await;
    let (search_session, search_info) = mcp_initialize(&client, &base, "/mcp/search").await;
    let (lean_session, _) = mcp_initialize(&client, &base, "/mcp/lean").await;
    let (compact_session, _) = mcp_initialize(&client, &base, "/mcp/compact").await;
    // The instructions are the same everywhere.
    assert_eq!(search_info, default_info);

    // The default endpoint and a profile that does not ask for it keep all
    // ten tools; the search-only ones list exactly four.
    let list = |path: &'static str, session: String| {
        let (client, base) = (client.clone(), base.clone());
        async move { mcp_result(&client, &base, path, &session, "tools/list", json!({})).await }
    };
    let all_ten = [
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
    ];
    let default_tools = list("/mcp", default_session.clone()).await;
    assert_eq!(tool_names(&default_tools), all_ten);
    let compact_tools = list("/mcp/compact", compact_session).await;
    assert_eq!(compact_tools, default_tools);
    let four = [
        "find_definition",
        "find_references",
        "search_code",
        "search_symbols",
    ];
    for (path, session) in [
        ("/mcp/search", &search_session),
        ("/mcp/lean", &lean_session),
    ] {
        let tools = list(path, session.clone()).await;
        assert_eq!(tool_names(&tools), four, "{path}");
    }
    // The tools it keeps are the default endpoint's, schemas included.
    let search_tools = list("/mcp/search", search_session.clone()).await;
    for tool in search_tools["tools"].as_array().unwrap() {
        let name = tool["name"].as_str().unwrap();
        let same = default_tools["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|candidate| candidate["name"] == name)
            .unwrap();
        assert_eq!(tool, same, "{name}");
    }

    // The others are not callable on it, and still are on `/mcp`.
    for name in [
        "index_workspace",
        "index_status",
        "list_indexed_files",
        "watch_workspace",
        "unwatch_workspace",
    ] {
        let response = mcp_call(
            &client,
            &base,
            "/mcp/search",
            &search_session,
            name,
            json!({"workspace_path": workspace}),
        )
        .await;
        assert!(
            response.get("error").is_some() || response["result"]["isError"] == true,
            "{name}: {response}"
        );
        assert!(
            response["result"]["structuredContent"].is_null(),
            "{name}: {response}"
        );
    }
    let service = mcp_call(
        &client,
        &base,
        "/mcp/lean",
        &lean_session,
        "service_status",
        json!({}),
    )
    .await;
    assert!(
        service.get("error").is_some() || service["result"]["isError"] == true,
        "{service}"
    );
    let status = mcp_call(
        &client,
        &base,
        "/mcp",
        &default_session,
        "index_status",
        json!({"workspace_path": workspace}),
    )
    .await;
    assert_ne!(status["result"]["isError"], true, "{status}");

    // The search tool works on it, and (with no `response` key) returns the
    // full report with the profile's name.
    let search =
        mcp_search_everything(&client, &base, "/mcp/search", &search_session, &workspace).await;
    let search = structured(&search);
    assert_eq!(search["search_profile"], "search");
    assert!(search["timings"].is_object() && search["results"][0]["fusion_score"].is_number());
    // Both keys together: a compact result behind the short tool list.
    let lean = mcp_search_everything(&client, &base, "/mcp/lean", &lean_session, &workspace).await;
    assert!(
        structured(&lean)["results"][0]
            .get("fusion_score")
            .is_none()
    );

    // A navigation tool on a workspace that was never indexed fails by
    // naming a tool the endpoint has; on `/mcp` the text is unchanged.
    let arguments = json!({"workspace_path": unindexed, "query": "untouched"});
    let on_search_only = mcp_call(
        &client,
        &base,
        "/mcp/search",
        &search_session,
        "search_symbols",
        arguments.clone(),
    )
    .await;
    assert_eq!(
        on_search_only["result"]["isError"], true,
        "{on_search_only}"
    );
    let text = on_search_only["result"]["content"][0]["text"]
        .as_str()
        .unwrap();
    assert!(
        text.contains("not indexed") && text.contains("search_code"),
        "{text}"
    );
    assert!(!text.contains("index_workspace"), "{text}");
    let on_default = mcp_call(
        &client,
        &base,
        "/mcp",
        &default_session,
        "search_symbols",
        arguments,
    )
    .await;
    let text = on_default["result"]["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("call index_workspace first"), "{text}");

    token.cancel();
    server.abort();
    models_task.abort();
}

// ---- search_code mode = "traverse" (multimap spine S6) ----

/// A function and the functions it calls.
type Function<'a> = (&'a str, &'a [&'a str]);

/// The wire form of a SCIP index (`scip.proto`: Index.documents = 2,
/// Document.relative_path = 1 and .occurrences = 2, Occurrence.range = 1,
/// .symbol = 2, .symbol_roles = 3, .enclosing_range = 7), one function per
/// line that calls the functions after its name. The tests need a real file
/// and the root crate has no protobuf writer.
fn scip_bytes(documents: &[(&str, &[Function<'_>])]) -> Vec<u8> {
    fn varint(mut value: u64, out: &mut Vec<u8>) {
        loop {
            let byte = (value & 0x7f) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }
    fn bytes(tag: u64, payload: &[u8], out: &mut Vec<u8>) {
        varint(tag << 3 | 2, out);
        varint(payload.len() as u64, out);
        out.extend_from_slice(payload);
    }
    fn ints(tag: u64, values: &[u64], out: &mut Vec<u8>) {
        let mut packed = Vec::new();
        values.iter().for_each(|v| varint(*v, &mut packed));
        bytes(tag, &packed, out);
    }
    let symbol = |name: &str| format!("rust-analyzer cargo demo 0.1.0 m/{name}().");
    let occurrence = |range: [u64; 4], name: &str, roles: u64, enclosing: Option<[u64; 4]>| {
        let mut out = Vec::new();
        ints(1, &range, &mut out);
        bytes(2, symbol(name).as_bytes(), &mut out);
        if roles != 0 {
            varint(3 << 3, &mut out);
            varint(roles, &mut out);
        }
        if let Some(enclosing) = enclosing {
            ints(7, &enclosing, &mut out);
        }
        out
    };
    let mut index = Vec::new();
    for (path, functions) in documents {
        let mut document = Vec::new();
        bytes(1, path.as_bytes(), &mut document);
        for (line, (name, callees)) in functions.iter().enumerate() {
            let line = line as u64;
            let definition = occurrence([line, 3, line, 6], name, 1, Some([line, 0, line, 200]));
            bytes(2, &definition, &mut document);
            for (i, callee) in callees.iter().enumerate() {
                let col = 10 + 5 * i as u64;
                let call = occurrence([line, col, line, col + 4], callee, 0, None);
                bytes(2, &call, &mut document);
            }
        }
        bytes(2, &document, &mut index);
    }
    index
}

/// A workspace with an `index.scip` for a small call graph: `search` calls
/// `parse_query`, `rank_hits` and `mock_ranker` (test code); `rank_hits`
/// calls `score_hit`.
fn write_scip_workspace(root: &Path) {
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("index.scip"),
        scip_bytes(&[
            (
                "src/search.rs",
                &[
                    ("search", &["parse_query", "rank_hits", "mock_ranker"]),
                    ("parse_query", &["tokenize_query"]),
                    ("rank_hits", &["score_hit"]),
                    ("tokenize_query", &[]),
                    ("score_hit", &["boost_exact"]),
                    ("boost_exact", &[]),
                ],
            ),
            (
                "tests/support.rs",
                &[("mock_ranker", &["fixture_hits"]), ("fixture_hits", &[])],
            ),
        ]),
    )
    .unwrap();
}

/// The `inputSchema` of `search_code` in a `tools/list` result.
fn search_code_schema(tools: &Value) -> Value {
    tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|tool| tool["name"] == "search_code")
        .unwrap()["inputSchema"]
        .clone()
}

/// Compares `actual` with the checked-in golden file `name` (rewritten when
/// `BLESS_GOLDEN` is set).
fn assert_golden(name: &str, actual: &Value) {
    let text = serde_json::to_string_pretty(actual).unwrap() + "\n";
    let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name);
    if std::env::var_os("BLESS_GOLDEN").is_some() {
        std::fs::write(&golden, &text).unwrap();
    }
    let expected = std::fs::read_to_string(&golden)
        .unwrap_or_else(|error| panic!("{}: {error}; run with BLESS_GOLDEN=1", golden.display()))
        .replace("\r\n", "\n");
    assert_eq!(text, expected, "{name} changed");
}

/// One `search_code` call over MCP: the whole `result` (so an error shows).
async fn mcp_search_call(
    client: &reqwest::Client,
    base: &str,
    session: &str,
    arguments: Value,
) -> Value {
    mcp_result(
        client,
        base,
        "/mcp",
        session,
        "tools/call",
        json!({"name":"search_code","arguments":arguments}),
    )
    .await
}

#[tokio::test]
async fn with_traverse_off_search_code_is_the_tool_it_was() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("scip-off");
    write_scip_workspace(&workspace);
    assert!(!config.traverse.enabled, "off is the default");
    let app = Arc::new(App::open(config).await.unwrap());
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, info) = mcp_initialize(&client, &base, "/mcp").await;

    // The instructions are those of a server without the mode.
    let instructions = info["instructions"].as_str().unwrap();
    assert!(!instructions.contains("traverse"), "{instructions}");
    assert!(
        instructions.ends_with("Line ranges are one-based and inclusive."),
        "{instructions}"
    );

    // The tool's schema is the one `SearchArgs` alone derives, which is the
    // one it had before the mode: pinned in a golden.
    let tools = mcp_result(&client, &base, "/mcp", &session, "tools/list", json!({})).await;
    let schema = search_code_schema(&tools);
    let plain = rmcp::handler::server::common::schema_for_input::<
        local_code_intelligence::server::SearchArgs,
    >()
    .unwrap();
    assert_eq!(schema, Value::Object((*plain).clone()));
    assert_golden("search_code_input_schema.json", &schema);
    for field in ["mode", "budget", "steer", "handle"] {
        assert!(schema["properties"].get(field).is_none(), "{field}");
    }

    // The mode is refused by name, and so are its fields without it.
    let refused = mcp_search_call(
        &client,
        &base,
        &session,
        json!({"workspace_path":workspace,"query":"search","mode":"traverse"}),
    )
    .await;
    assert_eq!(refused["isError"], true, "{refused}");
    let text = refused["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("switched off") && text.contains("[traverse]"),
        "{text}"
    );
    let stray = mcp_search_call(
        &client,
        &base,
        &session,
        json!({"workspace_path":workspace,"query":"search","steer":{"rules":[]}}),
    )
    .await;
    assert_eq!(stray["isError"], true, "{stray}");
    assert!(
        stray["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("mode"),
        "{stray}"
    );
    // Nothing was written for either.
    assert!(!temp.path().join("data/traces").exists());

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn with_traverse_on_the_schema_and_instructions_describe_the_mode() {
    let (_temp, mut config, _fake, models_task) = fixture().await;
    config.traverse.enabled = true;
    let app = Arc::new(App::open(config).await.unwrap());
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, info) = mcp_initialize(&client, &base, "/mcp").await;
    let instructions = info["instructions"].as_str().unwrap();
    assert!(instructions.contains("mode=\"traverse\""), "{instructions}");
    assert!(
        instructions.starts_with("Local Rust, TypeScript"),
        "{instructions}"
    );

    let tools = mcp_result(&client, &base, "/mcp", &session, "tools/list", json!({})).await;
    let schema = search_code_schema(&tools);
    // Every field of the plain search is still there, then the four of the
    // mode (the key order of the parsed object is not the declared order).
    let mut names: Vec<&str> = schema["properties"]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    names.sort_unstable();
    assert_eq!(
        names,
        [
            "budget",
            "exclude_paths",
            "handle",
            "include_paths",
            "languages",
            "mode",
            "query",
            "source_roles",
            "steer",
            "top_k",
            "workspace_path",
        ]
    );
    assert_eq!(schema["required"], json!(["workspace_path", "query"]));
    assert_golden("search_code_input_schema_traverse.json", &schema);

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn a_traverse_call_walks_the_scip_graph_and_a_checkpoint_is_answered_with_its_handle() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    config.traverse.enabled = true;
    let workspace = temp.path().join("scip-on");
    write_scip_workspace(&workspace);
    let data_dir = config.data_dir.clone();
    let app = Arc::new(App::open(config).await.unwrap());
    let data_dir = data_dir.canonicalize().unwrap_or(data_dir);
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let question = "how does search rank hits";

    // One call, one answer: no rules, so no handler and no checkpoint.
    let first = mcp_search_call(
        &client,
        &base,
        &session,
        json!({"workspace_path":workspace,"query":question,"top_k":20,"mode":"traverse"}),
    )
    .await;
    assert_ne!(first["isError"], true, "{first}");
    let plain = first["structuredContent"].clone();
    assert_eq!(plain["mode"], "traverse");
    assert_eq!(plain["status"], "complete", "{plain}");
    // `[run_profile.traverse] map_freshness` is off by default: no `maps` key.
    assert!(plain.get("maps").is_none(), "{plain}");
    // The index-only workspace has nothing to search, so the stand-in seeds
    // it, and the answer says so.
    assert_eq!(plain["seed_source"], "name_tokens", "{plain}");
    assert!(plain["seed_fallback"].is_string(), "{plain}");
    // The seeds (name matches) and what the walk reached from them.
    let reached: Vec<&str> = plain["seeds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|seed| seed.as_str().unwrap())
        .chain(
            plain["results"]
                .as_array()
                .unwrap()
                .iter()
                .map(|card| card["node"].as_str().unwrap()),
        )
        .collect();
    for name in [
        "parse_query",
        "rank_hits",
        "score_hit",
        "mock_ranker",
        "boost_exact",
    ] {
        assert!(
            reached.iter().any(|node| node.contains(name)),
            "{name} in {reached:?}"
        );
    }
    assert!(
        plain.get("handle").is_none() && plain.get("checkpoint").is_none(),
        "{plain}"
    );
    assert!(plain["totals"]["pops"].as_u64().unwrap() > 0, "{plain}");
    assert_eq!(
        plain["trace"]["hash"].as_str().unwrap().len(),
        16,
        "{plain}"
    );
    let trace = data_dir
        .join("traces")
        .join(plain["trace"]["file"].as_str().unwrap());
    assert!(trace.is_file(), "{}", trace.display());
    assert!(
        !plain.to_string().contains(data_dir.to_str().unwrap()),
        "no path in the answer"
    );

    // The same question again gives the same trace.
    let again = mcp_search_call(
        &client,
        &base,
        &session,
        json!({"workspace_path":workspace,"query":question,"top_k":20,"mode":"traverse"}),
    )
    .await;
    assert_eq!(
        again["structuredContent"]["trace"]["hash"],
        plain["trace"]["hash"]
    );

    // Rules that prune test code decide the stops they match; a stop no
    // rule matches goes to the agent: handle and checkpoint come back.
    let rules = json!([{"when":{"path_glob":"tests/**"},"then":"prune"}]);
    let stopped = mcp_search_call(
        &client,
        &base,
        &session,
        json!({
            "workspace_path":workspace,"query":question,"mode":"traverse",
            "steer":{"rules":rules,"stop_every":2,"max_stops":4},
        }),
    )
    .await;
    assert_ne!(stopped["isError"], true, "{stopped}");
    let stopped = stopped["structuredContent"].clone();
    assert_eq!(stopped["status"], "stopped", "{stopped}");
    let handle = stopped["handle"].as_str().unwrap().to_owned();
    assert_eq!(stopped["checkpoint"]["handle"], stopped["handle"]);
    assert!(
        stopped["checkpoint"]["frontier"].as_array().is_some(),
        "{stopped}"
    );
    // The checkpoint's cards say where their nodes are, as the results do.
    let cards: Vec<&Value> = ["answer", "visited_cards", "frontier"]
        .iter()
        .flat_map(|key| stopped["checkpoint"][key].as_array().unwrap())
        .chain(stopped["results"].as_array().unwrap())
        .collect();
    assert!(!cards.is_empty(), "{stopped}");
    for card in cards {
        let path = card["site"]["path"]
            .as_str()
            .unwrap_or_else(|| panic!("{card}"));
        assert!(
            path == "src/search.rs" || path == "tests/support.rs",
            "{card}"
        );
        assert!(card["site"]["line"].as_u64().unwrap() >= 1, "{card}");
        assert!(card["site"]["col"].as_u64().unwrap() >= 1, "{card}");
    }
    assert_eq!(
        stopped["steering"].as_array().unwrap().last().unwrap()["rung"],
        3
    );

    // The agent's answer: continue, with more pops, and no more stops.
    let finished = mcp_search_call(
        &client,
        &base,
        &session,
        json!({
            "workspace_path":workspace,"query":question,"mode":"traverse",
            "handle":handle,
            "steer":{"actions":[{"action":{"budget":{"pops":20}}}],"max_stops":0,"rules":[]},
        }),
    )
    .await;
    assert_ne!(finished["isError"], true, "{finished}");
    let finished = finished["structuredContent"].clone();
    assert_eq!(finished["status"], "complete", "{finished}");
    assert_eq!(finished["question"], question);
    assert!(finished.get("handle").is_none(), "{finished}");
    assert!(
        data_dir
            .join("traverse/handles")
            .join(format!("{handle}.json"))
            .is_file()
    );

    // Mistakes are errors that say what to fix, not empty answers.
    for (arguments, needle) in [
        (json!({"handle":"hdeadbeefdeadbeef"}), "handle"),
        (
            json!({"steer":{"rules":[{"when":{"nope":1},"then":"prune"}]}}),
            "steer.rules",
        ),
        (
            json!({"steer":{"ladder":["rules","system_one","agent"]}}),
            "system_one",
        ),
        (json!({"steer":{"script":"1 + 1"}}), "script"),
        (json!({"steer":{"actions":[]}}), "handle"),
        (json!({"top_k":0}), "top_k"),
    ] {
        let mut full = json!({"workspace_path":workspace,"query":"search rank","mode":"traverse"});
        full.as_object_mut()
            .unwrap()
            .extend(arguments.as_object().unwrap().clone());
        let result = mcp_search_call(&client, &base, &session, full).await;
        assert_eq!(result["isError"], true, "{needle}: {result}");
        let text = result["content"][0]["text"].as_str().unwrap();
        assert!(text.contains(needle), "{needle}: {text}");
    }
    // A workspace without an index says how to make one.
    let empty = temp.path().join("no-scip");
    std::fs::create_dir_all(&empty).unwrap();
    let missing = mcp_search_call(
        &client,
        &base,
        &session,
        json!({"workspace_path":empty,"query":"search rank","mode":"traverse"}),
    )
    .await;
    assert_eq!(missing["isError"], true, "{missing}");
    assert!(
        missing["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("rust-analyzer scip")
    );
    // A question that names nothing in the graph is an answer with no seeds.
    let nothing = mcp_search_call(
        &client,
        &base,
        &session,
        json!({"workspace_path":workspace,"query":"zebra xylophone","mode":"traverse"}),
    )
    .await;
    assert_ne!(nothing["isError"], true, "{nothing}");
    assert_eq!(nothing["structuredContent"]["status"], "no_seeds");

    token.cancel();
    server.abort();
    models_task.abort();
}

/// The first line of a trace file: its `start` event.
fn start_event_json(data_dir: &Path, response: &Value) -> Value {
    let file = response["trace"]["file"].as_str().unwrap();
    let text = std::fs::read_to_string(data_dir.join("traces").join(file)).unwrap();
    serde_json::from_str(text.lines().next().unwrap()).unwrap()
}

#[tokio::test]
async fn with_map_freshness_on_a_tree_edit_after_the_index_marks_the_scip_maps_stale() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    config.traverse.enabled = true;
    config.run_profile.traverse.map_freshness = true;
    let workspace = temp.path().join("scip-fresh");
    // The source is written before the index, so it is not newer than it.
    let source = workspace.join("src").join("search.rs");
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    std::fs::write(&source, "pub fn search() {}\n").unwrap();
    write_scip_workspace(&workspace);
    let app = Arc::new(App::open(config).await.unwrap());
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let ask = || async {
        let answer = mcp_search_call(
            &client,
            &base,
            &session,
            json!({"workspace_path":workspace,"query":"how does search rank hits","mode":"traverse"}),
        )
        .await;
        assert_ne!(answer["isError"], true, "{answer}");
        answer["structuredContent"].clone()
    };
    let states = |answer: &Value| -> Vec<(String, String, bool, Value)> {
        answer["maps"]
            .as_array()
            .unwrap_or_else(|| panic!("no maps in {answer}"))
            .iter()
            .map(|map| {
                (
                    map["id"].as_str().unwrap().to_owned(),
                    map["observed"].as_str().unwrap().to_owned(),
                    map["stale"].as_bool().unwrap(),
                    map["lag"].clone(),
                )
            })
            .collect()
    };
    let at = |stale: bool, lag: u64| -> Vec<(String, String, bool, Value)> {
        ["seeds.scip_names", "graph.scip", "structure.contains"]
            .into_iter()
            .map(|id| (id.to_owned(), "snapshot".to_owned(), stale, json!(lag)))
            .collect()
    };

    // The default maps are `MAPS`: `exact.closure` and the other maps that read
    // the tree are opt-in and not registered by a plain call. Of the defaults
    // the three built from the index state a freshness; `seeds.hybrid` states
    // none. The index is not older than its tree.
    use local_code_intelligence::app::traverse::{MAPS, OPT_IN_MAPS};
    assert_eq!(
        MAPS,
        [
            "seeds.hybrid",
            "seeds.scip_names",
            "graph.scip",
            "structure.contains"
        ]
    );
    assert!(OPT_IN_MAPS.contains(&"exact.closure"));
    assert_eq!(states(&ask().await), at(false, 0));
    // A tree edit after that (the byte length always changes) makes them
    // stale by one file; the index file was not touched.
    std::fs::write(&source, "pub fn search() {}\npub fn appended() {}\n").unwrap();
    assert_eq!(states(&ask().await), at(true, 1));

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn a_traverse_call_echoes_the_policy_it_ran_and_lints_the_steer() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    config.traverse.enabled = true;
    let workspace = temp.path().join("scip-on");
    write_scip_workspace(&workspace);
    let data_dir = config.data_dir.clone();
    let app = Arc::new(App::open(config).await.unwrap());
    let data_dir = data_dir.canonicalize().unwrap_or(data_dir);
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let call = |steer: Option<Value>| {
        let mut arguments = json!({
            "workspace_path": workspace, "query": "how does search rank hits",
            "top_k": 20, "mode": "traverse",
        });
        if let Some(steer) = steer {
            arguments["steer"] = steer;
        }
        mcp_search_call(&client, &base, &session, arguments)
    };
    let answer = |result: Value| {
        assert_ne!(result["isError"], true, "{result}");
        result["structuredContent"].clone()
    };

    // No policy: the first preset, nothing to warn about, and the stops,
    // rules and ladder of a walk without a handler.
    let plain = answer(call(None).await);
    let effective = &plain["effective_policy"];
    assert_eq!(effective["preset"], "explore", "{plain}");
    assert_eq!(effective["hash"].as_str().unwrap().len(), 16);
    assert_eq!(effective["stops"], Value::Null);
    assert_eq!(effective["rules"], json!([]));
    assert_eq!(effective["ladder"], json!([]));
    let maps: Vec<&str> = effective["maps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|map| map["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        maps,
        [
            "seeds.hybrid",
            "seeds.scip_names",
            "graph.scip",
            "structure.contains"
        ]
    );
    assert!(plain.get("warnings").is_none(), "{plain}");
    // The start event names the preset and carries the hash, not the object.
    assert_eq!(
        start_event_json(&data_dir, &plain)["effective_policy"],
        json!({"preset": "explore", "hash": effective["hash"]})
    );

    // The name of a preset gives the answer of the policy it holds; `explore`
    // is the same call as no policy.
    let explore = answer(call(Some(json!({"policy": "explore"}))).await);
    assert_eq!(explore["trace"]["hash"], plain["trace"]["hash"]);
    // `flow` walks and `where-to` walks at twenty answers (10 first-stage plus
    // top_k 20 here), so both run the call `explore` does.
    for name in ["flow", "where-to"] {
        let named = answer(call(Some(json!({"policy": name}))).await);
        assert_eq!(named["effective_policy"]["preset"], name);
        assert_eq!(named["effective_policy"]["hash"], effective["hash"]);
        assert_eq!(named["results"], plain["results"], "{name}");
        assert_eq!(named["totals"]["pops"], plain["totals"]["pops"], "{name}");
        assert!(named.get("warnings").is_none(), "{named}");
    }

    // `impact` adds the closure map and answers from the first stage alone: the
    // walk takes its one pop and returns no cards.
    let impact = answer(call(Some(json!({"policy": "impact"}))).await);
    assert_eq!(impact["effective_policy"]["preset"], "impact");
    assert_ne!(impact["effective_policy"]["hash"], effective["hash"]);
    let impact_maps: Vec<&str> = impact["effective_policy"]["maps"]
        .as_array()
        .unwrap()
        .iter()
        .map(|map| map["id"].as_str().unwrap())
        .collect();
    assert!(impact_maps.contains(&"exact.closure"), "{impact_maps:?}");
    assert!(impact["totals"]["pops"].as_u64().unwrap() <= 1, "{impact}");
    let impact_results = impact["results"].as_array().unwrap();
    assert!(!impact_results.is_empty(), "{impact}");
    assert!(
        impact_results.iter().all(|card| card["origin"] != "walk"),
        "{impact}"
    );
    assert!(
        impact["handle"].is_string(),
        "the walk can be continued: {impact}"
    );

    // Without top_k the MCP default (8) asks for 18 answers, below where-to's
    // measured 20: the first stage answers alone.
    let default_k = answer(
        mcp_search_call(
            &client,
            &base,
            &session,
            json!({
                "workspace_path": workspace, "query": "how does search rank hits",
                "mode": "traverse", "steer": {"policy": "where-to"},
            }),
        )
        .await,
    );
    assert!(
        default_k["notes"]
            .to_string()
            .contains("first stage alone answers"),
        "{default_k}"
    );

    // Fields beside the name replace the preset's, and the response shows
    // the stops, rules and ladder after the defaults.
    let over = answer(
        call(Some(json!({
            "policy": "impact", "stop_every": 3, "max_stops": 2,
            "rules": [{"when": {"kind": "test"}, "then": "prune"}],
        })))
        .await,
    );
    let effective = &over["effective_policy"];
    assert_eq!(effective["preset"], "impact");
    assert_eq!(effective["stops"]["every_pops"], 3);
    assert_eq!(effective["stops"]["max_stops"], 2);
    assert_eq!(effective["ladder"], json!(["rules", "agent"]));
    assert_eq!(effective["tau"], 0.8);
    assert_eq!(effective["rules"][0]["then"], "prune");
    assert_ne!(effective["hash"], plain["effective_policy"]["hash"]);
    assert_eq!(
        start_event_json(&data_dir, &over)["effective_policy"]["hash"],
        effective["hash"]
    );

    // A steer that cannot do what it says still runs, with warnings.
    let bad = answer(
        call(Some(json!({
            "stop_every": 500,
            "rules": [
                {"when": {"map": "nope"}, "then": "prune"},
                {"when": {"budget_left": {"ge": 0}}, "then": "prune"},
            ],
        })))
        .await,
    );
    assert_eq!(bad["effective_policy"]["preset"], Value::Null);
    let warnings = bad["warnings"].to_string();
    for needle in [
        "rule 0 cannot fire",
        "rule 1 prunes every node",
        "stop_every 500",
    ] {
        assert!(warnings.contains(needle), "{needle}: {warnings}");
    }
    assert!(bad["results"].as_array().is_some(), "{bad}");

    // A budget above the cap is a warning, no longer a note.
    let raised = mcp_search_call(
        &client,
        &base,
        &session,
        json!({
            "workspace_path": workspace, "query": "search rank", "mode": "traverse",
            "budget": {"pops": 5000},
        }),
    )
    .await;
    let raised = answer(raised);
    assert!(
        raised["warnings"].to_string().contains("budget.pops 5000"),
        "{raised}"
    );

    // Every answer has the field, an answer without seeds too.
    let nothing = mcp_search_call(
        &client,
        &base,
        &session,
        json!({"workspace_path": workspace, "query": "zebra xylophone", "mode": "traverse"}),
    )
    .await;
    let nothing = answer(nothing);
    assert_eq!(nothing["status"], "no_seeds");
    assert_eq!(nothing["effective_policy"]["preset"], "explore");

    // An unknown preset is an error that names the presets.
    let unknown = call(Some(json!({"policy": "nope"}))).await;
    assert_eq!(unknown["isError"], true, "{unknown}");
    let text = unknown["content"][0]["text"].as_str().unwrap();
    assert!(
        text.contains("explore, impact, flow, where-to, history"),
        "{text}"
    );

    token.cancel();
    server.abort();
    models_task.abort();
}

/// A workspace whose sources and `index.scip` agree: function `i` of
/// `src/lib.rs` is on line `i` (0-based), as `scip_bytes` writes them. Three
/// functions mention "translator" (the fake models rank those first); the
/// fourth does not.
fn write_hybrid_workspace(root: &Path) {
    write(
        root,
        "src/lib.rs",
        "fn translator_entry() { translator_helper(); }\n\
         fn translator_helper() { translator_leaf(); }\n\
         fn translator_leaf() {}\n\
         fn other_thing() {}\n",
    );
    std::fs::write(
        root.join("index.scip"),
        scip_bytes(&[(
            "src/lib.rs",
            &[
                ("translator_entry", &["translator_helper"]),
                ("translator_helper", &["translator_leaf"]),
                ("translator_leaf", &[]),
                ("other_thing", &[]),
            ],
        )]),
    )
    .unwrap();
}

/// The `start` event of a trace file.
fn start_event(data_dir: &Path, response: &Value) -> lci_maps::traverse::trace::StartEvent {
    let file = response["trace"]["file"].as_str().unwrap();
    let events = lci_maps::traverse::trace::read_trace(data_dir.join("traces").join(file)).unwrap();
    lci_maps::traverse::trace::StartEvent::from_json(&events[0]).unwrap()
}

#[tokio::test]
async fn traverse_starts_from_the_hybrid_search_and_records_what_found_each_seed() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    config.traverse.enabled = true;
    // This test reads the walk's own cards; the first stage is covered by the unit tests of the traverse module.
    config.traverse.first_stage = lci_core::config::TraverseFirstStage::Off;
    config.traverse.answer_seeds = false;
    config.traverse.exact_seeds = false;
    let workspace = temp.path().join("scip-hybrid");
    write_hybrid_workspace(&workspace);
    let data_dir = config.data_dir.clone();
    let app = Arc::new(App::open(config).await.unwrap());
    let data_dir = data_dir.canonicalize().unwrap_or(data_dir);
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;

    // No function of the question's words is named "the question": only the
    // search can find these functions.
    let answer = mcp_search_call(
        &client,
        &base,
        &session,
        json!({"workspace_path":workspace,"query":"translator","top_k":20,"mode":"traverse"}),
    )
    .await;
    assert_ne!(answer["isError"], true, "{answer}");
    let answer = answer["structuredContent"].clone();
    assert_eq!(answer["status"], "complete", "{answer}");
    assert_eq!(answer["seed_source"], "hybrid", "{answer}");
    assert!(answer.get("seed_fallback").is_none(), "{answer}");

    // Every seed is the symbol node of the function its hit's chunk holds,
    // with the lines of that chunk (1-based) and who found it.
    let hits = answer["seed_hits"].as_array().unwrap();
    let seeds: Vec<&str> = answer["seeds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|seed| seed.as_str().unwrap())
        .collect();
    assert_eq!(seeds.len(), hits.len(), "{answer}");
    for (seed, hit) in seeds.iter().zip(hits) {
        assert_eq!(hit["node"], *seed);
        assert_eq!(hit["kind"], "symbol", "{hit}");
        assert_eq!(hit["how"], "defined", "{hit}");
        assert_eq!(hit["path"], "src/lib.rs", "{hit}");
        assert!(
            !hit["retrievers"].as_array().unwrap().is_empty(),
            "which retriever found it: {hit}"
        );
        assert!(
            hit["score"].is_number() && hit["score_kind"].is_string(),
            "{hit}"
        );
    }
    for (name, line) in [
        ("translator_entry", 1),
        ("translator_helper", 2),
        ("translator_leaf", 3),
    ] {
        let hit = hits
            .iter()
            .find(|hit| hit["node"].as_str().unwrap().contains(name))
            .unwrap_or_else(|| panic!("{name} is a seed: {hits:?}"));
        assert_eq!(hit["lines"], json!([line, line]), "{name}");
        assert_eq!(
            hit["symbol"].as_str().unwrap().rsplit("::").next(),
            Some(name)
        );
        // The fake reranker scores a chunk that mentions the word high.
        assert_eq!(hit["score_kind"], "reranker", "{hit}");
        assert!(hit["score"].as_f64().unwrap() > 0.9, "{hit}");
        assert!(
            hit["retrievers"]
                .as_array()
                .unwrap()
                .contains(&json!("semantic")),
            "{hit}"
        );
    }
    let ranks: Vec<u64> = hits
        .iter()
        .map(|hit| hit["hit_rank"].as_u64().unwrap())
        .collect();
    assert!(
        ranks.windows(2).all(|w| w[0] < w[1]),
        "best hit first: {ranks:?}"
    );

    // A card shows the rank beside the potential, and how many independent
    // maps reached the node (`structure.contains` and `graph.scip` count once).
    for card in answer["results"].as_array().unwrap() {
        let (potential, ranked) = (
            card["potential"].as_f64().unwrap(),
            card["ranked_potential"].as_f64().unwrap(),
        );
        let n_maps = card["n_maps"].as_u64().unwrap();
        let listed: Vec<&str> = card["maps"]
            .as_array()
            .unwrap()
            .iter()
            .map(|map| map.as_str().unwrap())
            .collect();
        assert!(n_maps >= 1 && n_maps <= listed.len() as u64, "{card}");
        assert!(
            (ranked - potential * (1.0 + 0.1 * (n_maps - 1) as f64)).abs() < 1e-9,
            "{card}"
        );
        // Both maps read the one SCIP index: reached by them alone, a node
        // has one independent map, however many are listed (the engine test
        // `graph_scip_and_structure_contains_corroborate_a_node_once` has a
        // node that both reach).
        if listed
            .iter()
            .all(|map| ["graph.scip", "structure.contains"].contains(map))
        {
            assert_eq!(n_maps, 1, "{card}");
        }
    }

    // The walk goes on from the seeds along the call edges.
    let reached: Vec<&str> = answer["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|card| card["node"].as_str().unwrap())
        .collect();
    assert!(
        reached
            .iter()
            .chain(&seeds)
            .any(|n| n.contains("translator_leaf")),
        "{answer}"
    );
    // `structure.contains` is on by default: the walk also climbs to the file.
    assert!(reached.contains(&"file:src/lib.rs"), "{answer}");
    assert!(
        answer["totals"]["per_map"]["structure.contains"]["expands"]
            .as_u64()
            .is_some_and(|calls| calls > 0),
        "{answer}"
    );
    assert!(
        answer["totals"]["per_map"]["structure.contains"]["ms"].is_number(),
        "{answer}"
    );
    // Every call names why it ended.
    assert!(
        ["complete", "bound", "pops", "deadline", "budget"]
            .contains(&answer["stop_reason"].as_str().unwrap_or_default()),
        "{answer}"
    );

    // Every result item says where its node is: the repository-relative file
    // and the one-based line and column of the name (`scip_bytes` puts the
    // name of function `i` at line `i`, column 3).
    // A file node (`structure.contains` climbs from a symbol to its file) is at
    // the first line.
    for card in answer["results"].as_array().unwrap() {
        let node = card["node"].as_str().unwrap();
        if node == "file:src/lib.rs" {
            assert_eq!(card["via"], "structure.contains", "{card}");
            assert_eq!(
                card["site"],
                json!({"path": "src/lib.rs", "line": 1, "col": 1}),
                "{card}"
            );
            continue;
        }
        let line = [
            ("translator_entry", 1),
            ("translator_helper", 2),
            ("translator_leaf", 3),
            ("other_thing", 4),
        ]
        .iter()
        .find(|(name, _)| node.contains(name))
        .unwrap()
        .1;
        assert_eq!(
            card["site"],
            json!({"path": "src/lib.rs", "line": line, "col": 4}),
            "{card}"
        );
    }

    // The trace names the source, the maps and the policy.
    let start = start_event(&data_dir, &answer);
    assert_eq!(start.seed_source.as_deref(), Some("hybrid"));
    assert_eq!(start.seed_hits.len(), hits.len());
    let ids: Vec<&str> = start.maps.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(
        ids,
        [
            "seeds.hybrid",
            "seeds.scip_names",
            "graph.scip",
            "structure.contains"
        ]
    );
    assert!(start.maps.iter().all(|m| m.enabled && m.weight == 1.0));
    let policy = start.policy.unwrap();
    assert!(policy.ladder.is_empty() && policy.tau.is_none() && policy.rules == 0);

    // With rules, the policy in the trace is theirs.
    let ruled = mcp_search_call(
        &client,
        &base,
        &session,
        json!({
            "workspace_path":workspace,"query":"translator","mode":"traverse",
            "steer":{"rules":[{"when":{"path_glob":"tests/**"},"then":"prune"}],"tau":0.5,"stop_every":1},
        }),
    )
    .await;
    assert_ne!(ruled["isError"], true, "{ruled}");
    let start = start_event(&data_dir, &ruled["structuredContent"]);
    let policy = start.policy.unwrap();
    assert_eq!(policy.ladder, ["rules", "agent"]);
    assert_eq!(policy.tau, Some(0.5));
    assert_eq!(policy.rules, 1);
    assert_eq!(policy.stops["every_pops"], 1);

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn traverse_answers_with_the_embedder_down_on_a_fresh_workspace() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    config.traverse.enabled = true;
    config.traverse.first_stage = lci_core::config::TraverseFirstStage::Off;
    config.traverse.answer_seeds = false;
    config.traverse.exact_seeds = false;
    // A port that refuses connections, held for the whole test.
    let dead = ClosedPort::new();
    config.embedding_url = format!("http://127.0.0.1:{}/v1", dead.port);
    let workspace = temp.path().join("scip-fresh-down");
    write_hybrid_workspace(&workspace);
    let app = Arc::new(App::open(config).await.unwrap());
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;

    // The workspace has no snapshot: the hybrid search indexes it without
    // vectors and the lexical channel supplies the seeds, so the stand-in
    // does not take over.
    let answer = mcp_search_call(
        &client,
        &base,
        &session,
        json!({"workspace_path":workspace,"query":"translator","top_k":20,"mode":"traverse"}),
    )
    .await;
    assert_ne!(answer["isError"], true, "{answer}");
    let answer = answer["structuredContent"].clone();
    assert_eq!(answer["status"], "complete", "{answer}");
    assert_eq!(answer["seed_source"], "hybrid", "{answer}");
    assert!(answer.get("seed_fallback").is_none(), "{answer}");
    let hits = answer["seed_hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "{answer}");
    for name in ["translator_entry", "translator_helper", "translator_leaf"] {
        let hit = hits
            .iter()
            .find(|hit| hit["node"].as_str().unwrap().contains(name))
            .unwrap_or_else(|| panic!("{name} is a seed: {hits:?}"));
        assert_eq!(hit["retrievers"], json!(["lexical"]), "{hit}");
    }

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn search_vector_skips_without_embedding_on_a_vectorless_index() {
    let (temp, mut config, fake, models_task) = fixture().await;
    config.traverse.enabled = true;
    config.traverse.first_stage = lci_core::config::TraverseFirstStage::Off;
    config.traverse.answer_seeds = false;
    config.traverse.exact_seeds = false;
    // A search never refreshes under `manual`, so the index stays vectorless
    // after the service is back.
    config.index.freshness = lci_core::config::IndexFreshness::Manual;
    config.traverse.policies.insert(
        "flow".into(),
        lci_core::config::TraversePolicy {
            extra_maps: Some(vec!["search.vector".into()]),
            ..Default::default()
        },
    );
    let workspace = temp.path().join("scip-vectorless");
    write_hybrid_workspace(&workspace);
    let app = Arc::new(App::open(config).await.unwrap());
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let call = || {
        mcp_search_call(
            &client,
            &base,
            &session,
            json!({
                "workspace_path":workspace,"query":"translator","top_k":20,
                "mode":"traverse","steer":{"policy":"flow"},
            }),
        )
    };

    // The first call indexes the fresh workspace while the embedder answers
    // 503: it has no vectors.
    fake.embed_fails.store(true, Ordering::SeqCst);
    let first = call().await;
    assert_ne!(first["isError"], true, "{first}");

    // The embedder is back, so an embedding request would be served: the map
    // must send none.
    fake.embed_fails.store(false, Ordering::SeqCst);
    let requests = fake.embed_requests.load(Ordering::SeqCst);
    let answer = call().await;
    assert_ne!(answer["isError"], true, "{answer}");
    let answer = answer["structuredContent"].clone();
    assert_eq!(answer["status"], "complete", "{answer}");
    assert!(
        !answer["results"].as_array().unwrap().is_empty(),
        "{answer}"
    );
    assert_eq!(
        fake.embed_requests.load(Ordering::SeqCst),
        requests,
        "search.vector embedded on a vectorless index: {answer}"
    );
    let note = answer["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|note| note.as_str())
        .find(|note| note.starts_with("search.vector:"))
        .unwrap_or_else(|| panic!("no search.vector note: {answer}"));
    assert!(
        note.contains("the index has no vectors") && note.contains("fail-open"),
        "{note}"
    );
    // The skipped embedding is not an external call.
    assert_eq!(
        answer["totals"]["external_calls"], 0,
        "search.vector counted a call it did not make: {answer}"
    );

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn traverse_says_so_when_every_search_channel_is_off_and_the_stand_in_seeds_it() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    config.traverse.enabled = true;
    // A port nothing listens on: the embedder is unreachable.
    // Held for the whole test, so no parallel test's listener can take it.
    let dead = ClosedPort::new();
    config.embedding_url = format!("http://127.0.0.1:{}/v1", dead.port);
    config.embedding_timeout_seconds = 2;
    // With the embedder down the index is still built without vectors and the
    // lexical channel answers, so the hybrid search is only unavailable when
    // no channel can answer: the lexical and LSP channels are switched off
    // here and the semantic one has no vectors.
    config.channels.lexical = false;
    config.channels.lsp = false;
    let workspace = temp.path().join("scip-down");
    write_hybrid_workspace(&workspace);
    let data_dir = config.data_dir.clone();
    let app = Arc::new(App::open(config).await.unwrap());
    let data_dir = data_dir.canonicalize().unwrap_or(data_dir);
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;

    let answer = mcp_search_call(
        &client,
        &base,
        &session,
        json!({"workspace_path":workspace,"query":"how does translator helper work","mode":"traverse"}),
    )
    .await;
    assert_ne!(answer["isError"], true, "{answer}");
    let answer = answer["structuredContent"].clone();
    assert_eq!(answer["seed_source"], "name_tokens", "{answer}");
    let reason = answer["seed_fallback"].as_str().unwrap();
    assert!(reason.contains("hybrid search failed"), "{reason}");
    assert!(
        answer["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|note| note.as_str().unwrap().contains("name_tokens")),
        "{answer}"
    );
    let hits = answer["seed_hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "the stand-in still found names: {answer}");
    assert!(
        hits.iter()
            .all(|h| h["retrievers"] == json!(["name_tokens"]))
    );
    assert!(
        answer["seeds"]
            .as_array()
            .unwrap()
            .iter()
            .any(|seed| seed.as_str().unwrap().contains("translator_helper")),
        "{answer}"
    );
    let start = start_event(&data_dir, &answer);
    assert_eq!(start.seed_source.as_deref(), Some("name_tokens"));

    token.cancel();
    server.abort();
    models_task.abort();
}

/// Every tool refuses a key its arguments do not have, naming the key, before
/// it touches the workspace: one table over the tools, with the recipe and
/// compose tools switched on. A misspelt optional argument (`top_kk`) must
/// not be dropped silently.
#[tokio::test]
async fn every_tool_rejects_an_unknown_argument_naming_it() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    config.recipes.enabled = true;
    config.compose.enabled = true;
    config.traverse.enabled = true;
    let app = Arc::new(App::open(config).await.unwrap());
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;
    let workspace = temp.path().join("never-created");

    let tools = mcp_result(&client, &base, "/mcp", &session, "tools/list", json!({})).await;
    let listed: std::collections::BTreeSet<&str> = tool_names(&tools).into_iter().collect();

    let w = json!(workspace);
    let cases = [
        ("service_status", json!({})),
        ("index_workspace", json!({"workspace_path": w})),
        ("index_status", json!({"workspace_path": w})),
        ("list_indexed_files", json!({"workspace_path": w})),
        ("watch_workspace", json!({"workspace_path": w})),
        ("unwatch_workspace", json!({"workspace_path": w})),
        ("search_code", json!({"workspace_path": w, "query": "q"})),
        (
            "search_code",
            json!({"workspace_path": w, "query": "q", "mode": "traverse"}),
        ),
        ("search_symbols", json!({"workspace_path": w, "query": "q"})),
        (
            "find_definition",
            json!({"workspace_path": w, "relative_file_path": "a.rs", "line": 1, "character": 0}),
        ),
        (
            "find_references",
            json!({"workspace_path": w, "relative_file_path": "a.rs", "line": 1, "character": 0}),
        ),
        ("config_readers", json!({"workspace_path": w, "key": "a.b"})),
        ("impact_rings", json!({"workspace_path": w, "symbol": "s"})),
        (
            "flow_path",
            json!({"workspace_path": w, "from": "a", "to": "b"}),
        ),
        ("flow_from", json!({"workspace_path": w, "entry": "a"})),
        (
            "config_neighbours",
            json!({"workspace_path": w, "symbol": "s"}),
        ),
        (
            "lci_compose",
            json!({"workspace_path": w, "steps": [{"id": "s1", "op": "pick", "in": {}}]}),
        ),
    ];
    // Even the tool with no arguments must reject extra keys.
    let covered: std::collections::BTreeSet<&str> = cases.iter().map(|(name, _)| *name).collect();
    for name in &listed {
        assert!(
            covered.contains(name),
            "{name} has no unknown-argument case"
        );
    }
    for tool in tools["tools"].as_array().unwrap() {
        assert_eq!(
            tool["inputSchema"]["additionalProperties"], false,
            "{} must have a closed input schema",
            tool["name"]
        );
    }
    let openapi = client
        .get(format!("{base}/openapi.json"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    for (path, operations) in openapi["paths"].as_object().unwrap() {
        if let Some(post) = operations.get("post") {
            assert_eq!(
                post["requestBody"]["content"]["application/json"]["schema"]["additionalProperties"],
                false,
                "{path} must have a closed request schema"
            );
        }
    }
    for params in [
        json!({"name": "service_status"}),
        json!({"name": "service_status", "arguments": {}}),
    ] {
        let response = mcp_result(&client, &base, "/mcp", &session, "tools/call", params).await;
        assert_ne!(response["isError"], true, "{response}");
        assert!(response["structuredContent"].is_object());
    }

    // Unknown keys at the top level, and inside the nested argument objects.
    let mut calls: Vec<(String, Value, &str)> = Vec::new();
    for (name, arguments) in &cases {
        let mut with_extra = arguments.clone();
        with_extra["top_kk"] = json!(3);
        calls.push((name.to_string(), with_extra, "top_kk"));
    }
    calls.push((
        "search_code".to_owned(),
        json!({"workspace_path": w, "query": "q", "mode": "traverse", "budget": {"pop": 1}}),
        "pop",
    ));
    calls.push((
        "search_code".to_owned(),
        json!({"workspace_path": w, "query": "q", "mode": "traverse", "steer": {"polcy": "impact"}}),
        "polcy",
    ));
    calls.push((
        "lci_compose".to_owned(),
        json!({"workspace_path": w, "steps": [{"id": "s1", "op": "pick", "inn": {}}]}),
        "inn",
    ));

    for (name, arguments, key) in calls {
        let response = mcp_post(
            &client,
            &format!("{base}/mcp"),
            Some(&session),
            &json!({"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":name,"arguments":arguments}}),
        )
        .await;
        let status = response.status();
        let body = response.text().await.unwrap();
        let message = rpc_response(&body);
        // rmcp refuses the call before the tool runs: a tool error result
        // (`isError`) that names the field. This is what it already did for a
        // wrong type or a missing argument.
        assert_eq!(
            message["result"]["isError"], true,
            "{name} {key}: {status} {body}"
        );
        let text = message["result"]["content"][0]["text"]
            .as_str()
            .unwrap_or_default();
        assert!(
            text.starts_with("failed to deserialize parameters: unknown field")
                && text.contains(key),
            "{name} {key}: {status} {body}"
        );
    }

    // The REST bodies share the structs, so they refuse it too (axum answers
    // an unusable JSON body with 422). The `search_profile` query parameter is
    // a separate strict struct, covered by its own test.
    let rest = client
        .post(format!("{base}/v1/search_symbols"))
        .json(&json!({"workspace_path": w, "query": "q", "top_kk": 3}))
        .send()
        .await
        .unwrap();
    assert_eq!(rest.status(), 422);
    assert!(
        rest.text()
            .await
            .unwrap()
            .contains("unknown field `top_kk`")
    );

    // Nothing was created for any of them.
    assert!(!workspace.exists());
    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn misspelled_search_arguments_are_rejected_by_mcp_and_rest_before_indexing() {
    let (temp, config, fake, models_task) = fixture().await;
    let workspace = temp.path().join("strict-search");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    let (base, token, server) = serve_on_loopback(app.clone()).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;

    for (key, value) in [
        ("include_path", json!(["src/**"])),
        ("topk", json!(1)),
        ("language", json!("rust")),
    ] {
        let mut arguments = json!({"workspace_path": workspace, "query": "translator"});
        arguments[key] = value;
        let result = mcp_result(
            &client,
            &base,
            "/mcp",
            &session,
            "tools/call",
            json!({"name": "search_code", "arguments": arguments}),
        )
        .await;
        assert_eq!(result["isError"], true, "{key}: {result}");
        let expected = format!("unknown field `{key}`");
        assert!(
            result["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains(&expected),
            "{result}"
        );

        let response = client
            .post(format!("{base}/v1/search_code"))
            .json(&arguments)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::UNPROCESSABLE_ENTITY);
        let error = response.text().await.unwrap();
        assert!(error.contains(&expected), "{error}");
    }

    assert_eq!(fake.document_requests.load(Ordering::SeqCst), 0);
    assert_eq!(fake.queries.load(Ordering::SeqCst), 0);
    assert_eq!(fake.rerank_requests.load(Ordering::SeqCst), 0);
    assert!(!app.status(&workspace).await.unwrap().indexed);
    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn rest_search_query_parameters_reject_unknown_and_duplicate_keys() {
    let (temp, config, fake, models_task) = fixture().await;
    let workspace = temp.path().join("strict-query");
    write(&workspace, "lib.rs", "fn translator() {}\n");
    let app = Arc::new(App::open(profiled_config(config)).await.unwrap());
    let (base, token, server) = serve_on_loopback(app.clone()).await;
    let client = reqwest::Client::new();

    for (query, expected) in [
        ("include_path=src", "unknown field `include_path`"),
        ("topk=1", "unknown field `topk`"),
        ("language=rust", "unknown field `language`"),
        ("search_profle=fast", "unknown field `search_profle`"),
        (
            "search_profile=fast&include_path=src",
            "unknown field `include_path`",
        ),
        (
            "search_profile=fast&search_profile=deep",
            "duplicate field `search_profile`",
        ),
    ] {
        let response = client
            .post(format!("{base}/v1/search_code?{query}"))
            .json(&json!({"workspace_path": workspace, "query": "translator"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{query}");
        let error = response.text().await.unwrap();
        assert!(error.contains(expected), "{query}: {error}");
    }

    assert_eq!(fake.document_requests.load(Ordering::SeqCst), 0);
    assert_eq!(fake.queries.load(Ordering::SeqCst), 0);
    assert_eq!(fake.rerank_requests.load(Ordering::SeqCst), 0);
    assert!(!app.status(&workspace).await.unwrap().indexed);
    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn symbol_query_byte_limit_matches_code_search_over_mcp_and_rest() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    // No real language server is needed to accept a valid symbol query.
    config.csharp.disabled = true;
    let workspace = temp.path().join("symbol-query-limit");
    write(&workspace, "Example.cs", "class Example {}\n");
    let app = Arc::new(App::open(config).await.unwrap());
    app.index(&workspace).await.unwrap();
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;

    for query in ["q".repeat(16000), "\u{00e9}".repeat(8000)] {
        assert_eq!(query.len(), 16000);
        let arguments = json!({"workspace_path": workspace, "query": query});
        let accepted = mcp_result(
            &client,
            &base,
            "/mcp",
            &session,
            "tools/call",
            json!({"name": "search_symbols", "arguments": arguments}),
        )
        .await;
        assert_ne!(accepted["isError"], true, "{accepted}");
        assert!(accepted["structuredContent"]["results"].is_array());
        let response = client
            .post(format!("{base}/v1/search_symbols"))
            .json(&arguments)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.json::<Value>().await.unwrap()["results"].is_array());

        let over_limit = format!("{query}q");
        assert_eq!(over_limit.len(), 16001);
        for name in ["search_symbols", "search_code"] {
            let arguments = json!({"workspace_path": workspace, "query": over_limit});
            let rejected = mcp_result(
                &client,
                &base,
                "/mcp",
                &session,
                "tools/call",
                json!({"name": name, "arguments": arguments}),
            )
            .await;
            assert_eq!(rejected["isError"], true, "{name}: {rejected}");
            assert_eq!(rejected["content"][0]["text"], "query exceeds 16000 bytes");
            let response = client
                .post(format!("{base}/v1/{name}"))
                .json(&arguments)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(
                response.json::<Value>().await.unwrap()["error"],
                "query exceeds 16000 bytes"
            );
        }
    }

    token.cancel();
    server.abort();
    models_task.abort();
}

// ---------------------------------------------------------------------------
// The run-profile hash on every result: `"profile_hash"`, the
// `Config::run_profile().hash` of the server's configuration, stamped where
// MCP and REST build a result so that a new tool or route cannot ship without
// it. Errors are never stamped.
// ---------------------------------------------------------------------------

/// The sixteen tools of a server with the recipes, compose and traverse on.
const STAMPED_TOOLS: [&str; 16] = [
    "config_neighbours",
    "config_readers",
    "find_definition",
    "find_references",
    "flow_from",
    "flow_path",
    "impact_rings",
    "index_status",
    "index_workspace",
    "lci_compose",
    "list_indexed_files",
    "search_code",
    "search_symbols",
    "service_status",
    "unwatch_workspace",
    "watch_workspace",
];

/// A config with every optional tool on and a fake language server per
/// language, each in the one mode its tool needs: C# answers `find_definition`,
/// Python `find_references`, TypeScript `search_symbols`. The `compact` profile
/// serves `search_code` in the compact shape.
fn stamped_config(mut config: Config, state: &Path) -> Config {
    use local_code_intelligence::config::{
        CSharpLspConfig, McpResponse, PythonLspConfig, SearchProfile, TypeScriptLspConfig,
    };
    config.channels.lsp = false;
    config.traverse.enabled = true;
    config.recipes.enabled = true;
    config.compose.enabled = true;
    config.lsp_timeout_seconds = 5;
    let args = |mode: &str, name: &str, primary: &str, secondary: &str| {
        vec![
            "--mode".to_owned(),
            mode.to_owned(),
            "--state".to_owned(),
            state.join(name).to_string_lossy().into_owned(),
            "--primary-file".to_owned(),
            primary.to_owned(),
            "--secondary-file".to_owned(),
            secondary.to_owned(),
        ]
    };
    config.csharp = CSharpLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: args(
            "definition",
            "csharp",
            "src/Calculator.cs",
            "src/CallSite.cs",
        ),
        disabled: false,
    };
    config.python = PythonLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: args(
            "references",
            "python",
            "src/Calculator.py",
            "src/CallSite.py",
        ),
        disabled: false,
    };
    config.typescript = TypeScriptLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: args(
            "workspace-symbols",
            "typescript",
            "src/Calculator.ts",
            "src/CallSite.ts",
        ),
        disabled: false,
    };
    config.search_profiles.insert(
        "compact".to_owned(),
        SearchProfile {
            response: Some(McpResponse::Compact),
            ..SearchProfile::default()
        },
    );
    config
}

/// The workspace the tools are called on: the recipe tree (Rust sources and
/// their `index.scip`) plus a C#, a Python and a TypeScript file pair for the
/// fake language servers.
fn write_stamped_workspace(root: &Path) {
    super::recipes::write_recipe_tree(root);
    // The key file the config recipes read.
    write(root, "config.example.toml", "port = 8768\n");
    write_csharp_workspace(root);
    write(
        root,
        "src/Calculator.py",
        "class Calculator:\n    def add(self, a, b):\n        return a + b\n",
    );
    write(
        root,
        "src/CallSite.py",
        "from .Calculator import Calculator\n\ndef run(value):\n    return value.add(1, 2)\n",
    );
    write(
        root,
        "src/Calculator.ts",
        "export class Calculator {\n  add(a: number, b: number): number {\n    return a + b;\n  }\n}\n",
    );
    write(
        root,
        "src/CallSite.ts",
        "import { Calculator } from './Calculator';\nexport const run = (value: Calculator) => value.add(1, 2);\n",
    );
}

/// The arguments of a valid call of each REST operation or MCP tool on
/// `workspace`; `None` for a name that has none, which fails the test that asks.
fn stamped_arguments(name: &str, workspace: &Path) -> Option<Value> {
    Some(match name {
        "index_workspace" | "index_status" | "list_indexed_files" | "watch_workspace"
        | "unwatch_workspace" | "release_workspace" => json!({"workspace_path":workspace}),
        "search_symbols" => json!({"workspace_path":workspace,"query":"Calculator"}),
        "find_definition" => json!({
            "workspace_path":workspace,"relative_file_path":"src/CallSite.cs",
            "line":2,"character":72,
        }),
        "find_references" => json!({
            "workspace_path":workspace,"relative_file_path":"src/Calculator.py",
            "line":1,"character":10,"include_declaration":true,
        }),
        "search_code" => json!({"workspace_path":workspace,"query":"Foo alpha beta"}),
        "service_status" => json!({}),
        "config_readers" => json!({"workspace_path":workspace,"key":"port"}),
        "impact_rings" => json!({"workspace_path":workspace,"symbol":"Foo"}),
        "flow_path" => json!({"workspace_path":workspace,"from":"beta","to":"alpha"}),
        "flow_from" => json!({"workspace_path":workspace,"entry":"beta","k":5}),
        "config_neighbours" => json!({"workspace_path":workspace,"symbol":"alpha"}),
        "lci_compose" => json!({"workspace_path":workspace,"steps":[
            {"id":"s1","op":"seeds.scip_names","in":{"names":["Foo"]}},
            {"id":"s2","op":"exact.closure","in":{"symbols":"$s1.symbols"}},
        ]}),
        _ => return None,
    })
}

/// `response` (a `tools/call` response) is a success that carries `hash` in
/// its structured content and in its text; returns the structured result.
fn assert_stamped<'a>(what: &str, response: &'a Value, hash: &str) -> &'a Value {
    assert_ne!(response["result"]["isError"], true, "{what}: {response}");
    let result = structured(response);
    assert_eq!(result["profile_hash"], hash, "{what}: {result}");
    assert!(text_content(response).contains(hash), "{what}: {response}");
    result
}

#[tokio::test]
async fn every_tool_result_carries_the_profile_hash() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("stamped");
    write_stamped_workspace(&workspace);
    let scip_only = temp.path().join("stamped-traverse");
    write_scip_workspace(&scip_only);
    let app = Arc::new(
        App::open(stamped_config(config, &temp.path().join("lsp-state")))
            .await
            .unwrap(),
    );
    let hash = app.config.run_profile().hash;
    assert_eq!(hash.len(), 64);
    assert_eq!(app.profile_hash(), hash);
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;

    // A new tool fails here until it has arguments in `stamped_arguments`.
    let tools = mcp_result(&client, &base, "/mcp", &session, "tools/list", json!({})).await;
    assert_eq!(tool_names(&tools), STAMPED_TOOLS);

    // Order matters: index before status and files, watch before unwatch.
    let mut called = Vec::new();
    for name in [
        "index_workspace",
        "index_status",
        "list_indexed_files",
        "search_symbols",
        "find_definition",
        "find_references",
        "search_code",
        "watch_workspace",
        "unwatch_workspace",
        "service_status",
        "config_readers",
        "impact_rings",
        "flow_path",
        "flow_from",
        "config_neighbours",
        "lci_compose",
    ] {
        let arguments = stamped_arguments(name, &workspace)
            .unwrap_or_else(|| panic!("no arguments for the tool {name}"));
        let response = mcp_call(&client, &base, "/mcp", &session, name, arguments).await;
        assert_stamped(name, &response, &hash);
        called.push(name);
    }
    called.sort_unstable();
    assert_eq!(called, STAMPED_TOOLS);

    // The traversal: a plain answer, an answer that stops at a checkpoint with
    // a handle, the continuation of that handle, and one with no seeds.
    let traverse = |arguments: Value| {
        let mut full = json!({"workspace_path":scip_only,"mode":"traverse"});
        full.as_object_mut()
            .unwrap()
            .extend(arguments.as_object().unwrap().clone());
        full
    };
    let question = "how does search rank hits";
    let plain = mcp_call(
        &client,
        &base,
        "/mcp",
        &session,
        "search_code",
        traverse(json!({"query":question,"top_k":20})),
    )
    .await;
    assert_eq!(
        assert_stamped("traverse", &plain, &hash)["status"],
        "complete"
    );
    let stopped = mcp_call(
        &client,
        &base,
        "/mcp",
        &session,
        "search_code",
        traverse(json!({
            "query":question,
            "steer":{
                "rules":[{"when":{"path_glob":"tests/**"},"then":"prune"}],
                "stop_every":2,"max_stops":4,
            },
        })),
    )
    .await;
    let stopped = assert_stamped("traverse checkpoint", &stopped, &hash);
    assert_eq!(stopped["status"], "stopped", "{stopped}");
    assert!(stopped["checkpoint"].is_object(), "{stopped}");
    let handle = stopped["handle"].as_str().unwrap();
    let finished = mcp_call(
        &client,
        &base,
        "/mcp",
        &session,
        "search_code",
        traverse(json!({
            "query":question,"handle":handle,
            "steer":{"actions":[{"action":{"budget":{"pops":20}}}],"max_stops":0,"rules":[]},
        })),
    )
    .await;
    assert_eq!(
        assert_stamped("traverse handle", &finished, &hash)["status"],
        "complete"
    );
    let nothing = mcp_call(
        &client,
        &base,
        "/mcp",
        &session,
        "search_code",
        traverse(json!({"query":"zebra xylophone"})),
    )
    .await;
    assert_eq!(
        assert_stamped("traverse no_seeds", &nothing, &hash)["status"],
        "no_seeds"
    );

    // The compact endpoint writes the key itself, last, once.
    let (compact_session, _) = mcp_initialize(&client, &base, "/mcp/compact").await;
    let compact = mcp_call(
        &client,
        &base,
        "/mcp/compact",
        &compact_session,
        "search_code",
        stamped_arguments("search_code", &workspace).unwrap(),
    )
    .await;
    assert_stamped("compact", &compact, &hash);
    let text = text_content(&compact);
    let tail = format!(",\"profile_hash\":\"{hash}\"}}");
    assert!(text.ends_with(&tail), "{text}");
    assert_eq!(text.matches("profile_hash").count(), 1, "{text}");
    // The key costs 82 bytes in a result (the closing brace is not its own).
    assert_eq!(tail.len() - 1, 82);

    // An error is never stamped, in the structured content or in the text.
    for (name, arguments) in [
        ("index_status", json!({"workspace_path":"   "})),
        (
            "search_code",
            json!({"workspace_path":scip_only,"query":"x","mode":"traverse","handle":"hdeadbeefdeadbeef"}),
        ),
        (
            "search_code",
            json!({"workspace_path":workspace,"query":"x","top_k":0}),
        ),
    ] {
        let response = mcp_call(&client, &base, "/mcp", &session, name, arguments).await;
        assert_eq!(response["result"]["isError"], true, "{name}: {response}");
        assert!(!response.to_string().contains("profile_hash"), "{response}");
    }

    token.cancel();
    server.abort();
    models_task.abort();
}

#[tokio::test]
async fn every_openapi_route_result_carries_the_profile_hash() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("stamped-rest");
    write_stamped_workspace(&workspace);
    let app = Arc::new(
        App::open(stamped_config(config, &temp.path().join("lsp-state")))
            .await
            .unwrap(),
    );
    let hash = app.config.run_profile().hash;
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let openapi = client
        .get(format!("{base}/openapi.json"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();

    // The schema of a success says what the routes do.
    let schema = &openapi["paths"]["/v1/index_status"]["post"]["responses"]["200"]["content"]["application/json"]
        ["schema"];
    assert_eq!(schema["required"], json!(["profile_hash"]), "{schema}");
    assert_eq!(
        schema["properties"]["profile_hash"]["pattern"], "^[0-9a-f]{64}$",
        "{schema}"
    );

    // Order matters: index before status and files, watch before unwatch,
    // release last.
    let order = [
        "index_workspace",
        "index_status",
        "list_indexed_files",
        "search_symbols",
        "find_definition",
        "find_references",
        "search_code",
        "watch_workspace",
        "unwatch_workspace",
        "service_status",
        "release_workspace",
    ];
    let mut routes: Vec<(String, String, String)> = Vec::new();
    for (path, item) in openapi["paths"].as_object().unwrap() {
        for (method, operation) in item.as_object().unwrap() {
            routes.push((
                path.clone(),
                method.clone(),
                operation["operationId"].as_str().unwrap().to_owned(),
            ));
        }
    }
    // A route that is not a tool of the MCP list or `release_workspace` fails
    // here until it is added to this test.
    let position = |operation: &str| {
        order
            .iter()
            .position(|name| *name == operation)
            .or_else(|| {
                STAMPED_TOOLS
                    .iter()
                    .position(|name| *name == operation)
                    .map(|n| n + order.len())
            })
            .unwrap_or_else(|| panic!("route {operation} is not in the stamped test"))
    };
    routes.sort_by_key(|(_, _, operation)| position(operation));
    assert_eq!(routes.len(), STAMPED_TOOLS.len() + 1, "{routes:?}");
    for (path, method, operation) in &routes {
        let request = match method.as_str() {
            "get" => client.get(format!("{base}{path}")),
            "post" => client.post(format!("{base}{path}")).json(
                &stamped_arguments(operation, &workspace)
                    .unwrap_or_else(|| panic!("no arguments for the route {operation}")),
            ),
            other => panic!("{path}: method {other}"),
        };
        let response = request.send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["profile_hash"], hash.as_str(), "{path}: {body}");
    }

    // Liveness and readiness are not results of the profile.
    for path in ["/health", "/ready"] {
        let response = client.get(format!("{base}{path}")).send().await.unwrap();
        let body: Value = response.json().await.unwrap();
        assert!(body.get("profile_hash").is_none(), "{path}: {body}");
    }

    // An error is never stamped.
    let failed = client
        .post(format!("{base}/v1/index_status"))
        .json(&json!({"workspace_path": "   "}))
        .send()
        .await
        .unwrap();
    assert_eq!(failed.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let failed: Value = failed.json().await.unwrap();
    assert!(failed["error"].is_string() && failed.get("profile_hash").is_none());

    token.cancel();
    server.abort();
    models_task.abort();
}

// ---- `[run_profile.scip] mode` (slice K20a) ----

/// An MCP server over the fake models whose `[run_profile.scip] mode` is
/// `mode`, with the walk alone (no first stage), and the workspace
/// `scip-mode` under its temporary directory.
struct ScipModeServer {
    workspace: std::path::PathBuf,
    base: String,
    session: String,
    client: reqwest::Client,
    stop: (
        tokio_util::sync::CancellationToken,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    ),
    _temp: tempfile::TempDir,
}

impl ScipModeServer {
    async fn start(mode: lci_core::config::profile::ScipMode) -> Self {
        let (temp, mut config, _fake, models_task) = fixture().await;
        config.traverse.enabled = true;
        config.traverse.first_stage = lci_core::config::TraverseFirstStage::Off;
        config.traverse.answer_seeds = false;
        config.traverse.exact_seeds = false;
        // One seed (the best hit), so the walk has a callee left to reach.
        config.traverse.seed_k = 1;
        config.run_profile.scip.mode = mode;
        let workspace = temp.path().join("scip-mode");
        let app = Arc::new(App::open(config).await.unwrap());
        let (base, token, server) = serve_on_loopback(app).await;
        let client = reqwest::Client::new();
        let (session, _) = mcp_initialize(&client, &base, "/mcp").await;
        Self {
            workspace,
            base,
            session,
            client,
            stop: (token, server, models_task),
            _temp: temp,
        }
    }

    /// The whole `result` of one `mode = "traverse"` call about
    /// "translator".
    async fn traverse(&self) -> Value {
        mcp_search_call(
            &self.client,
            &self.base,
            &self.session,
            json!({"workspace_path":self.workspace,"query":"translator","top_k":20,"mode":"traverse"}),
        )
        .await
    }

    fn stop(self) {
        self.stop.0.cancel();
        self.stop.1.abort();
        self.stop.2.abort();
    }
}

fn notes_text(answer: &Value) -> String {
    answer["notes"].to_string()
}

#[tokio::test]
async fn traverse_answers_without_reading_a_corrupt_scip_index_under_scip_mode_off() {
    use lci_core::config::profile::ScipMode;
    // Authoritative: the unreadable index fails the call, naming the file.
    let strict = ScipModeServer::start(ScipMode::Authoritative).await;
    write_hybrid_workspace(&strict.workspace);
    std::fs::write(strict.workspace.join("index.scip"), [0xff_u8; 32]).unwrap();
    let refused = strict.traverse().await;
    assert_eq!(refused["isError"], true, "{refused}");
    let text = refused["content"][0]["text"].as_str().unwrap();
    assert!(text.contains("index.scip"), "{text}");
    strict.stop();

    // Off: the path is never read, so the same workspace answers.
    let off = ScipModeServer::start(ScipMode::Off).await;
    write_hybrid_workspace(&off.workspace);
    std::fs::write(off.workspace.join("index.scip"), [0xff_u8; 32]).unwrap();
    let answer = off.traverse().await;
    assert_ne!(answer["isError"], true, "{answer}");
    let answer = answer["structuredContent"].clone();
    assert_eq!(answer["mode"], "traverse", "{answer}");
    assert_eq!(answer["seed_source"], "hybrid", "{answer}");
    let hits = answer["seed_hits"].as_array().unwrap();
    assert!(!hits.is_empty(), "{answer}");
    for hit in hits {
        assert_eq!(hit["kind"], "chunk", "{hit}");
    }
    assert!(notes_text(&answer).contains("scip.mode = off"), "{answer}");
    off.stop();
}

#[tokio::test]
async fn scip_mode_cache_answers_without_the_file_and_walks_it_once_it_exists() {
    use lci_core::config::profile::ScipMode;
    let server = ScipModeServer::start(ScipMode::Cache).await;
    write_hybrid_workspace(&server.workspace);
    std::fs::remove_file(server.workspace.join("index.scip")).unwrap();

    // No file: the call answers, and the notes say the index was not read.
    let without = server.traverse().await;
    assert_ne!(without["isError"], true, "{without}");
    let without = without["structuredContent"].clone();
    assert!(
        notes_text(&without).contains("(scip.mode = cache)"),
        "{without}"
    );
    // Without the graph the search hits stay chunks.
    let kinds = |answer: &Value| -> Vec<String> {
        answer["seed_hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|hit| hit["kind"].as_str().unwrap().to_owned())
            .collect()
    };
    let chunks = kinds(&without);
    assert!(
        !chunks.is_empty() && chunks.iter().all(|kind| kind == "chunk"),
        "{without}"
    );

    // The file appears: the next call walks the graph from the seeds.
    write_hybrid_workspace(&server.workspace);
    let with = server.traverse().await;
    assert_ne!(with["isError"], true, "{with}");
    let with = with["structuredContent"].clone();
    assert!(!notes_text(&with).contains("scip.mode = cache"), "{with}");
    // With it the hits are mapped to the graph's symbols and the graph is
    // walked from them.
    let symbols = kinds(&with);
    assert!(
        !symbols.is_empty() && symbols.iter().all(|kind| kind == "symbol"),
        "{with}"
    );
    assert!(
        with["totals"]["per_map"]["graph.scip"]["expands"]
            .as_u64()
            .is_some_and(|calls| calls > 0),
        "{with}"
    );
    assert!(
        with["results"].as_array().unwrap().iter().any(|card| {
            card["via"] == "graph.scip"
                && card["node"].as_str().unwrap().contains("translator_helper")
        }),
        "a symbol reached through graph.scip: {with}"
    );
    server.stop();
}
