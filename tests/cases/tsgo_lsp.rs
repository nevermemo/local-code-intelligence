//! The typescript-go adapter (provider `typescript-go`, section `[tsgo]`)
//! through the application: navigation reaches it when it is the enabled
//! TypeScript server, the default typescript-language-server still answers
//! when `[tsgo]` is unset, a server that never publishes diagnostics does not
//! stall a request, and the two servers cannot be enabled together. The fake
//! language server answers canned locations; real tsgo is covered by the live
//! step (see the K12b plan), not here.

use super::*;
use local_code_intelligence::{
    app::{NavigationReport, Status},
    config::{
        TsgoLspConfig, TypeScriptLspConfig,
        layers::{Flags, ResolveSpec, resolve},
    },
};
use std::{collections::BTreeMap, path::PathBuf};

/// A two-file TypeScript workspace under `temp` and the state directory the
/// fake records into.
fn typescript_workspace(temp: &tempfile::TempDir) -> (PathBuf, PathBuf) {
    let workspace = temp.path().join("workspace");
    let state = temp.path().join("state");
    write(
        &workspace,
        "src/Calculator.ts",
        "export class Calculator {\n  add(a: number, b: number): number {\n    return a + b;\n  }\n}\n",
    );
    write(
        &workspace,
        "src/CallSite.ts",
        "import { Calculator } from './Calculator';\nexport function run(value: Calculator): number {\n  return value.add(1, 2);\n}\n",
    );
    (workspace, state)
}

/// The fake language server in `definition-references` mode with `extra`
/// arguments appended, recording into `state`.
fn fake_args(state: &std::path::Path, extra: &[&str]) -> Vec<String> {
    let mut args: Vec<String> = [
        "--mode",
        "definition-references",
        "--answer-unhandled",
        "--state",
        &state.to_string_lossy(),
        "--primary-file",
        "src/Calculator.ts",
        "--secondary-file",
        "src/CallSite.ts",
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend(extra.iter().map(|arg| (*arg).to_owned()));
    args
}

fn fake_tsgo(state: &std::path::Path, extra: &[&str]) -> TsgoLspConfig {
    TsgoLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: fake_args(state, extra),
        disabled: false,
    }
}

fn fake_typescript(state: &std::path::Path) -> TypeScriptLspConfig {
    TypeScriptLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: fake_args(state, &[]),
        disabled: false,
    }
}

fn running(status: &Status, provider: &str) -> bool {
    status.analyzers_running[provider]
}

/// Every location is a TypeScript location of `provider`.
fn assert_locations_of(report: &NavigationReport, provider: &str) {
    assert!(!report.results.is_empty());
    for location in &report.results {
        assert_eq!(
            location.language.as_deref(),
            Some("typescript"),
            "{location:?}"
        );
        assert_eq!(location.provider.as_deref(), Some(provider), "{location:?}");
    }
}

/// With only `[tsgo]` configured, `definition` and `references` are answered by
/// `typescript-go`, whose session shows in `analyzers_running`; the
/// unconfigured typescript-language-server (the first adapter of the
/// language) neither answers nor starts.
#[tokio::test]
async fn definition_and_references_reach_typescript_go_when_it_is_the_enabled_server() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    let (workspace, state) = typescript_workspace(&temp);
    config.lsp_timeout_seconds = LOADED_BUDGET_SECONDS;
    config.tsgo = fake_tsgo(&state, &[]);
    assert!(!config.typescript.enabled());
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();
    assert!(!running(
        &app.status(&workspace).await.unwrap(),
        "typescript-go"
    ));

    let definition = app
        .definition(&workspace, "src/CallSite.ts", 1, 40)
        .await
        .unwrap();
    assert_locations_of(&definition, "typescript-go");
    assert_eq!(
        definition.results[0].relative_file_path.as_deref(),
        Some("src/Calculator.ts")
    );

    let references = app
        .references(&workspace, "src/Calculator.ts", 1, 20, true)
        .await
        .unwrap();
    assert_locations_of(&references, "typescript-go");
    assert!(
        references
            .results
            .iter()
            .any(|location| location.relative_file_path.as_deref() == Some("src/CallSite.ts")),
        "{references:?}"
    );

    let status = app.status(&workspace).await.unwrap();
    assert!(running(&status, "typescript-go"), "{status:?}");
    assert!(
        !running(&status, "typescript-language-server"),
        "{:?}",
        status.analyzers_running
    );
    // The hook sent the file as a `didOpen` before the request.
    assert!(state.join("did_open_seen.txt").exists());
    app.shutdown_language_servers().await;
    models_task.abort();
}

/// With only `[typescript]` configured the provider is still
/// typescript-language-server: registering the second adapter changed no
/// existing answer, and `typescript-go` stays off.
#[tokio::test]
async fn typescript_language_server_still_answers_when_tsgo_is_unset() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    let (workspace, state) = typescript_workspace(&temp);
    config.lsp_timeout_seconds = LOADED_BUDGET_SECONDS;
    config.typescript = fake_typescript(&state);
    assert!(!config.tsgo.enabled());
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let definition = app
        .definition(&workspace, "src/CallSite.ts", 1, 40)
        .await
        .unwrap();
    assert_locations_of(&definition, "typescript-language-server");
    let references = app
        .references(&workspace, "src/Calculator.ts", 1, 20, true)
        .await
        .unwrap();
    assert_locations_of(&references, "typescript-language-server");

    let status = app.status(&workspace).await.unwrap();
    assert!(running(&status, "typescript-language-server"));
    assert!(!running(&status, "typescript-go"));
    app.shutdown_language_servers().await;
    models_task.abort();
}

/// A server that never publishes diagnostics after a `didOpen` (an open
/// question for the real tsgo) only costs the wait's cap: `definition` still
/// answers, well inside the loaded-machine budget.
#[tokio::test]
async fn a_tsgo_that_never_publishes_diagnostics_still_answers() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    let (workspace, state) = typescript_workspace(&temp);
    config.lsp_timeout_seconds = LOADED_BUDGET_SECONDS;
    config.tsgo = fake_tsgo(&state, &["--no-diagnostics"]);
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let definition = tokio::time::timeout(
        LOADED_BUDGET,
        app.definition(&workspace, "src/CallSite.ts", 1, 40),
    )
    .await
    .expect("the diagnostics wait is capped, not unbounded")
    .unwrap();
    assert_locations_of(&definition, "typescript-go");
    assert!(state.join("did_open_seen.txt").exists());
    app.shutdown_language_servers().await;
    models_task.abort();
}

/// Both TypeScript servers enabled is refused when the configuration loads,
/// whichever layer supplies the second one; disabling either makes it load.
#[test]
fn the_configuration_layers_refuse_both_typescript_servers_enabled() {
    let temp = tempfile::tempdir().unwrap();
    let data_dir = temp.path().join("data");
    let file = |name: &str, sections: &str| {
        let path = temp.path().join(name);
        std::fs::write(
            &path,
            format!(
                "data_dir = '{}'\n{sections}",
                data_dir.to_string_lossy().replace('\\', "/")
            ),
        )
        .unwrap();
        path
    };
    let spec = |path: PathBuf, env: &[(&str, &str)]| ResolveSpec {
        config_path: Some(path),
        env: env
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
        user_config_dir: None,
        flags: Flags {
            port: None,
            no_local_config: true,
        },
    };

    // Both in the file: `Config::load` and the layered resolve refuse it.
    let both = file(
        "both.toml",
        "[typescript]\npath = 'typescript-language-server'\n[tsgo]\npath = 'tsgo'\n",
    );
    let error = Config::load(Some(&both)).unwrap_err().to_string();
    assert!(
        error.contains("typescript.path") && error.contains("tsgo.path"),
        "{error}"
    );
    let error = resolve(&spec(both, &[])).unwrap_err().to_string();
    assert!(error.contains("tsgo.path"), "{error}");

    // One in the file, the other from the environment (`LCI_TSGO_PATH`).
    let one = file(
        "one.toml",
        "[typescript]\npath = 'typescript-language-server'\n",
    );
    resolve(&spec(one.clone(), &[])).unwrap();
    let error = resolve(&spec(one.clone(), &[("LCI_TSGO_PATH", "tsgo")]))
        .unwrap_err()
        .to_string();
    assert!(
        error.contains("typescript.path") && error.contains("tsgo.path"),
        "{error}"
    );
    // Disabling either one makes the pair load again.
    let resolved = resolve(&spec(
        one.clone(),
        &[("LCI_TSGO_PATH", "tsgo"), ("LCI_TSGO_DISABLED", "1")],
    ))
    .unwrap();
    assert!(resolved.config.typescript.enabled() && !resolved.config.tsgo.enabled());
    let resolved = resolve(&spec(
        one,
        &[
            ("LCI_TSGO_PATH", "tsgo"),
            ("LCI_TYPESCRIPT_DISABLED", "true"),
        ],
    ))
    .unwrap();
    assert!(!resolved.config.typescript.enabled() && resolved.config.tsgo.enabled());
}

/// Readiness lists `typescript_go_available` for an unconfigured tsgo as not
/// configured (optional: it never makes the service not ready), and names the
/// path once one is set.
#[tokio::test]
async fn readiness_lists_typescript_go_as_not_configured_until_a_path_is_set() {
    let (_temp, mut config, _fake, models_task) = fixture().await;
    config.readiness_timeout_seconds = LOADED_BUDGET_SECONDS;
    let app = App::open(config.clone()).await.unwrap();
    let status = serde_json::to_value(app.service_status().await).unwrap();
    let component = status["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|component| component["name"] == "typescript_go_available")
        .unwrap_or_else(|| panic!("no typescript_go_available component: {status}"));
    assert_eq!(component["required"], false, "{component}");
    assert_eq!(component["detail"], "typescript-go not configured");
    assert_eq!(status["ready"], true, "{status}");

    config.tsgo.path = Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned());
    let app = App::open(config).await.unwrap();
    let status = serde_json::to_value(app.service_status().await).unwrap();
    let detail = status["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|component| component["name"] == "typescript_go_available")
        .unwrap()["detail"]
        .as_str()
        .unwrap()
        .to_owned();
    assert!(
        detail.starts_with("configured typescript-go command "),
        "{detail}"
    );
    models_task.abort();
}
