//! The recipe and compose tools over the command line, MCP and REST.

use super::mcp::{mcp_initialize, mcp_result, serve_on_loopback};
use super::*;

const RECIPE_TOOLS: [&str; 6] = [
    "config_readers",
    "impact_rings",
    "flow_path",
    "flow_from",
    "config_neighbours",
    "lci_compose",
];

/// A tree with an `index.scip` whose symbols have kinds (the recipes anchor
/// at declarations): `alpha` references `Foo`, `beta` calls `alpha`.
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn write_recipe_tree(root: &Path) {
    use scip::types::{Document, Occurrence, SymbolInformation, symbol_information::Kind};
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("a.rs"),
        "struct Foo;\nfn alpha() { Foo }\nfn beta() { alpha() }\n",
    )
    .unwrap();
    let symbol = |path: &str| format!("rust-analyzer cargo demo 0.1.0 {path}");
    let occurrence = |range: &[i32], path: &str, roles: i32, enclosing: &[i32]| {
        let mut o = Occurrence::default();
        o.range = range.to_vec();
        o.symbol = symbol(path);
        o.symbol_roles = roles;
        o.enclosing_range = enclosing.to_vec();
        o
    };
    let info = |path: &str, kind: Kind| {
        let mut i = SymbolInformation::default();
        i.symbol = symbol(path);
        i.kind = kind.into();
        i
    };
    let mut document = Document::default();
    document.relative_path = "a.rs".to_owned();
    document.occurrences = vec![
        occurrence(&[0, 7, 10], "a/Foo#", 1, &[0, 0, 0, 11]),
        occurrence(&[1, 3, 8], "a/alpha().", 1, &[1, 0, 1, 27]),
        occurrence(&[1, 21, 24], "a/Foo#", 0, &[]),
        occurrence(&[2, 3, 7], "a/beta().", 1, &[2, 0, 2, 25]),
        occurrence(&[2, 12, 17], "a/alpha().", 0, &[]),
    ];
    document.symbols = vec![
        info("a/Foo#", Kind::Struct),
        info("a/alpha().", Kind::Function),
        info("a/beta().", Kind::Function),
    ];
    let mut message = scip::types::Index::default();
    message.documents = vec![document];
    scip::write_message_to_file(root.join("index.scip"), message).unwrap();
}

fn result_names(answer: &Value, group: &str) -> Vec<String> {
    answer["results"]
        .as_array()
        .unwrap_or_else(|| panic!("no results in {answer}"))
        .iter()
        .filter(|row| row["group"] == group)
        .map(|row| row["name"].as_str().unwrap().to_owned())
        .collect()
}

#[test]
fn the_recipe_command_prints_the_answer_without_a_server() {
    let temp = tempfile::tempdir().unwrap();
    let tree = temp.path().join("tree");
    write_recipe_tree(&tree);
    let run = |name: &str, args: &str| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
        command
            .current_dir(temp.path())
            .args(["recipe", name, "--root"])
            .arg(&tree)
            .args(["--args", args, "--out"])
            .arg(temp.path().join("out"));
        strip_lci_env(&mut command);
        let output = command.output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };

    let (ok, stdout, stderr) = run("impact_rings", r#"{"symbol":"Foo"}"#);
    assert!(ok, "{stderr}");
    let rings: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(result_names(&rings, "ring_1"), ["alpha"], "{rings}");
    assert_eq!(result_names(&rings, "ring_2"), ["beta"], "{rings}");
    assert_eq!(rings["stop"]["reason"], "complete", "{rings}");

    let (ok, stdout, stderr) = run("flow_path", r#"{"from":"beta","to":"alpha"}"#);
    assert!(ok, "{stderr}");
    let path: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(result_names(&path, "path"), ["beta", "alpha"], "{path}");

    let (ok, stdout, stderr) = run("flow_from", r#"{"entry":"beta","k":5}"#);
    assert!(ok, "{stderr}");
    let walk: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(result_names(&walk, "walk"), ["alpha"], "{walk}");
    assert_eq!(result_names(&walk, "file"), ["file:a.rs"], "{walk}");

    // An unknown recipe and a stray argument are refused by name.
    let (ok, _, stderr) = run("no_such_recipe", "{}");
    assert!(!ok && stderr.contains("no_such_recipe"), "{stderr}");
    let (ok, _, stderr) = run("impact_rings", r#"{"symbol":"Foo","bogus":1}"#);
    assert!(!ok && stderr.contains("bogus"), "{stderr}");
}

#[tokio::test]
async fn with_the_flags_off_no_recipe_tool_or_route_exists() {
    let (temp, config, _fake, models_task) = fixture().await;
    let workspace = temp.path().join("recipes-off");
    write_recipe_tree(&workspace);
    assert!(!config.recipes.enabled && !config.compose.enabled);
    let app = Arc::new(App::open(config).await.unwrap());
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;

    let tools = mcp_result(&client, &base, "/mcp", &session, "tools/list", json!({})).await;
    let listed: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    for name in RECIPE_TOOLS {
        assert!(
            !listed.contains(&name),
            "{name} listed with the flags off: {listed:?}"
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
    for name in RECIPE_TOOLS {
        assert!(
            openapi["paths"].get(format!("/v1/{name}")).is_none(),
            "{name}"
        );
        let response = client
            .post(format!("{base}/v1/{name}"))
            .json(&json!({"workspace_path": workspace}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND, "{name}");
    }

    token.cancel();
    server.abort();
    models_task.abort();
}

/// Removes every `"additionalProperties": false` member, at any depth.
fn strip_additional_properties_false(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if map.get("additionalProperties") == Some(&Value::Bool(false)) {
                map.remove("additionalProperties");
            }
            map.values_mut().for_each(strip_additional_properties_false);
        }
        Value::Array(items) => items.iter_mut().for_each(strip_additional_properties_false),
        _ => {}
    }
}

/// What a client sees of a server's surface: the instructions sent at
/// `initialize`, the tools listed on `/mcp` (by name) and the OpenAPI
/// document, as one value.
async fn surface_of(app: Arc<App>) -> Value {
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, initialized) = mcp_initialize(&client, &base, "/mcp").await;
    let listed = mcp_result(&client, &base, "/mcp", &session, "tools/list", json!({})).await;
    let mut tools = listed["tools"].as_array().unwrap().clone();
    tools.sort_by_key(|tool| tool["name"].as_str().unwrap().to_owned());
    let openapi = client
        .get(format!("{base}/openapi.json"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    token.cancel();
    server.abort();
    json!({"instructions": initialized["instructions"], "tools": tools, "openapi": openapi})
}

/// With both flags off the server's surface is the surface of the commit
/// before the recipes (b46786e): the same instructions, the same tools with
/// the same descriptions and schemas, the same OpenAPI document.
///
/// A server built without the recipe and compose code paths cannot be
/// constructed from this tree: the code is compiled in and the flags only
/// decide whether the routers are merged. So the comparison is against a
/// golden file taken from the binary of b46786e, produced by `surface_of`
/// with the default configuration (`tests/golden/surface_flags_off.json`).
/// Do not re-bless it from this tree; it is the reference, not an output.
#[tokio::test]
async fn with_the_flags_off_the_surface_is_that_of_the_commit_before_the_recipes() {
    let (_temp, config, _fake, models_task) = fixture().await;
    assert!(!config.recipes.enabled && !config.compose.enabled);
    let app = Arc::new(App::open(config).await.unwrap());
    let actual = surface_of(app).await;
    let golden = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/surface_flags_off.json");
    let expected: Value = serde_json::from_str(
        &std::fs::read_to_string(&golden)
            .unwrap_or_else(|error| panic!("{}: {error}", golden.display())),
    )
    .unwrap();
    // Compare the parts one by one so a failure names the part that moved.
    assert_eq!(
        actual["instructions"], expected["instructions"],
        "server instructions changed"
    );
    // The one recorded change since b46786e (d363): tool input schemas
    // and REST bodies deny unknown keys. Strip exactly that marker before
    // comparing.
    let mut tools = actual["tools"].clone();
    strip_additional_properties_false(&mut tools);
    let mut openapi = actual["openapi"].clone();
    strip_additional_properties_false(&mut openapi);
    assert_eq!(tools, expected["tools"], "tools/list changed");
    assert_eq!(openapi, expected["openapi"], "openapi.json changed");
    models_task.abort();
}

#[tokio::test]
async fn with_the_flags_on_the_recipes_answer_over_mcp_and_rest() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    config.recipes.enabled = true;
    config.compose.enabled = true;
    let workspace = temp.path().join("recipes-on");
    write_recipe_tree(&workspace);
    let app = Arc::new(App::open(config).await.unwrap());
    let (base, token, server) = serve_on_loopback(app).await;
    let client = reqwest::Client::new();
    let (session, _) = mcp_initialize(&client, &base, "/mcp").await;

    let tools = mcp_result(&client, &base, "/mcp", &session, "tools/list", json!({})).await;
    let listed = tools["tools"].as_array().unwrap();
    for name in RECIPE_TOOLS {
        let tool = listed
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("{name} is not listed: {tools}"));
        assert!(
            tool["inputSchema"]["properties"]["workspace_path"].is_object(),
            "{name}: {tool}"
        );
    }
    // The existing tools stay.
    assert!(listed.iter().any(|tool| tool["name"] == "search_code"));

    // The six descriptions say the same thing about the index: it is the
    // server's, not a file the caller has to find in the workspace.
    for name in RECIPE_TOOLS {
        let tool = listed.iter().find(|tool| tool["name"] == name).unwrap();
        let description = tool["description"].as_str().unwrap();
        assert!(
            description.contains("needs the server's SCIP index"),
            "{name}: {description}"
        );
        assert!(!description.contains("index.scip"), "{name}: {description}");
    }

    // One call over MCP.
    let called = mcp_result(
        &client,
        &base,
        "/mcp",
        &session,
        "tools/call",
        json!({"name":"impact_rings","arguments":{"workspace_path":workspace,"symbol":"Foo"}}),
    )
    .await;
    assert_ne!(called["isError"], true, "{called}");
    let answer = &called["structuredContent"];
    assert_eq!(result_names(answer, "ring_1"), ["alpha"], "{answer}");
    assert_eq!(result_names(answer, "ring_2"), ["beta"], "{answer}");

    // The same call over REST gives the same answer.
    let rest = client
        .post(format!("{base}/v1/impact_rings"))
        .json(&json!({"workspace_path": workspace, "symbol": "Foo"}))
        .send()
        .await
        .unwrap();
    assert_eq!(rest.status(), StatusCode::OK);
    let rest: Value = rest.json().await.unwrap();
    assert_eq!(result_names(&rest, "ring_1"), ["alpha"], "{rest}");

    // A composition over REST.
    let composed = client
        .post(format!("{base}/v1/lci_compose"))
        .json(&json!({"workspace_path": workspace, "steps": [
            {"id": "s1", "op": "seeds.scip_names", "in": {"names": ["Foo"]}},
            {"id": "s2", "op": "exact.closure", "in": {"symbols": "$s1.symbols"}},
        ]}))
        .send()
        .await
        .unwrap();
    assert_eq!(composed.status(), StatusCode::OK);
    let composed: Value = composed.json().await.unwrap();
    assert!(composed.to_string().contains("alpha"), "{composed}");

    // OpenAPI lists the paths.
    let openapi = client
        .get(format!("{base}/openapi.json"))
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap();
    for name in RECIPE_TOOLS {
        let operation = &openapi["paths"][format!("/v1/{name}")]["post"];
        assert_eq!(operation["operationId"], name, "{openapi}");
    }

    token.cancel();
    server.abort();
    models_task.abort();
}
