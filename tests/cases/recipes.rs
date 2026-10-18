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
/// at declarations): `alpha` references `Foo`, `beta` calls `alpha`, and
/// `Foo` implements the trait `Speak` (its `say` is the trait method's
/// implementation).
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn write_recipe_tree(root: &Path) {
    use scip::types::{Document, Occurrence, SymbolInformation, symbol_information::Kind};
    std::fs::create_dir_all(root).unwrap();
    std::fs::write(
        root.join("a.rs"),
        "struct Foo;\nfn alpha() { Foo }\nfn beta() { alpha() }\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.rs"),
        "trait Speak { fn say(&self); }\nimpl Speak for Foo { fn say(&self) {} }\n",
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
    // A second file, so the trait and the impl are not siblings of `beta`.
    let mut traits = Document::default();
    traits.relative_path = "b.rs".to_owned();
    traits.occurrences = vec![
        occurrence(&[0, 6, 11], "b/Speak#", 1, &[0, 0, 0, 30]),
        occurrence(&[0, 17, 20], "b/Speak#say().", 1, &[0, 14, 0, 28]),
        occurrence(&[1, 5, 10], "b/Speak#", 0, &[]),
        occurrence(&[1, 15, 18], "a/Foo#", 0, &[]),
        occurrence(
            &[1, 24, 27],
            "b/impl#[Foo][Speak]say().",
            1,
            &[1, 21, 1, 37],
        ),
    ];
    traits.symbols = vec![
        info("b/Speak#", Kind::Trait),
        info("b/Speak#say().", Kind::Method),
        info("b/impl#[Foo][Speak]say().", Kind::Method),
    ];
    let mut message = scip::types::Index::default();
    message.documents = vec![document, traits];
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

    // A trait-impl member is linked to the trait's method declaration by an
    // `implements` edge, but a path over calls does not cross it.
    let (ok, stdout, stderr) = run("flow_path", r#"{"from":"Foo::say","to":"Speak::say"}"#);
    assert!(ok, "{stderr}");
    let implemented: Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(implemented["stop"]["reason"], "empty", "{implemented}");
    assert!(
        result_names(&implemented, "path").is_empty(),
        "{implemented}"
    );

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

/// Pin the default MCP and REST surface, including closed input schemas.
/// The baseline is the pre-recipes surface of b46786e plus the recorded
/// strict-argument changes (d363, d388): with the recipe and compose flags off
/// the surface must equal it, closed input schemas included, so neither the
/// flags nor a loss of strictness can change it unnoticed.
#[tokio::test]
async fn with_the_flags_off_the_surface_matches_the_strict_argument_golden() {
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
    assert_eq!(actual["tools"], expected["tools"], "tools/list changed");
    assert_eq!(
        actual["openapi"], expected["openapi"],
        "openapi.json changed"
    );
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

#[test]
fn traverse_over_scip_implements_reaches_the_member_from_the_trait_method() {
    let temp = tempfile::tempdir().unwrap();
    let tree = temp.path().join("tree");
    write_recipe_tree(&tree);
    let run = |args: &[&str]| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
        command.current_dir(temp.path()).args(args);
        strip_lci_env(&mut command);
        let output = command.output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    // The node ids of the two methods, from a recipe's anchors.
    let (ok, stdout, stderr) = run(&[
        "recipe",
        "flow_path",
        "--root",
        tree.to_str().unwrap(),
        "--args",
        r#"{"from":"Speak::say","to":"Foo::say"}"#,
        "--out",
        temp.path().join("out").to_str().unwrap(),
    ]);
    assert!(ok, "{stderr}");
    let anchors: Value = serde_json::from_str(&stdout).unwrap();
    let declaration = anchors["anchor"]["from"]["resolved"].as_str().unwrap();
    let member = anchors["anchor"]["to"]["resolved"].as_str().unwrap();

    let (ok, stdout, stderr) = run(&[
        "traverse",
        "--scip",
        tree.join("index.scip").to_str().unwrap(),
        "--root",
        tree.to_str().unwrap(),
        "--seeds",
        declaration,
        "--maps",
        "scip.implements",
        "--out",
        temp.path().join("out").to_str().unwrap(),
    ]);
    assert!(ok, "{stderr}");
    let walk: Value = serde_json::from_str(&stdout).unwrap();
    assert!(
        walk["notes"][0]
            .as_str()
            .unwrap()
            .starts_with("scip.implements: 1 impl headers read (1 with a trait"),
        "{walk}"
    );
    // The member is reached in one hop from the trait method, over the
    // `implements` edge (the only edge the one enabled map has).
    let reached = walk["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|card| card["node"] == member)
        .unwrap_or_else(|| panic!("{member} not reached: {walk}"));
    assert_eq!(reached["depth"], 1, "{walk}");
    assert_eq!(reached["via"], "scip.implements", "{walk}");
    assert_eq!(reached["witness"], "compiler", "{walk}");
}

#[test]
fn traverse_starts_at_the_trait_method_a_question_names_and_checks_trait_seeds() {
    let temp = tempfile::tempdir().unwrap();
    let tree = temp.path().join("tree");
    write_recipe_tree(&tree);
    let run = |extra: &[&str]| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
        command.current_dir(temp.path()).args([
            "traverse",
            "--scip",
            tree.join("index.scip").to_str().unwrap(),
            "--root",
            tree.to_str().unwrap(),
            "--question",
            "What must change if `Speak::say` changes?",
            "--maps",
            "scip.implements",
            "--out",
            temp.path().join("out").to_str().unwrap(),
        ]);
        command.args(extra);
        strip_lci_env(&mut command);
        let output = command.output().unwrap();
        (
            output.status.success(),
            String::from_utf8_lossy(&output.stdout).into_owned(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    };
    // The default and both modes start at the declaration and reach the member.
    for extra in [
        &[][..],
        &["--trait-seeds", "replace"],
        &["--trait-seeds", "prepend"],
    ] {
        let (ok, stdout, stderr) = run(extra);
        assert!(ok, "{extra:?}: {stderr}");
        let walk: Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(walk["seed_source"], "traits", "{extra:?}: {walk}");
        assert_eq!(walk["trait_seeds"], 1, "{extra:?}: {walk}");
        assert_eq!(walk["seed_hits"][0]["how"], "trait_method", "{walk}");
        assert_eq!(walk["results"][1]["via"], "scip.implements", "{walk}");
    }
    // A bad value is refused with the choices.
    let (ok, _, stderr) = run(&["--trait-seeds", "both"]);
    assert!(!ok);
    assert!(stderr.contains("auto, replace or prepend"), "{stderr}");
}
