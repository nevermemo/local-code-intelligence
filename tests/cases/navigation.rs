use super::*;
use local_code_intelligence::{
    config::{CSharpLspConfig, PythonLspConfig, TypeScriptLspConfig},
    lsp::{Manager, adapter::LspAdapter},
    workspace::Workspace,
};
use std::sync::Arc;

fn csharp_adapter(manager: &Manager) -> Arc<dyn LspAdapter> {
    manager
        .adapters_for_language("csharp")
        .into_iter()
        .next()
        .cloned()
        .expect("csharp-ls adapter is always constructed")
}

fn typescript_adapter(manager: &Manager) -> Arc<dyn LspAdapter> {
    manager
        .adapters_for_language("typescript")
        .into_iter()
        .next()
        .cloned()
        .expect("typescript-language-server adapter is always constructed")
}

fn python_adapter(manager: &Manager) -> Arc<dyn LspAdapter> {
    manager
        .adapters_for_language("python")
        .into_iter()
        .next()
        .cloned()
        .expect("pyright adapter is always constructed")
}

fn fake_csharp_fixture(
    temp: &tempfile::TempDir,
    mode: &str,
) -> (Config, Workspace, std::path::PathBuf) {
    let workspace_path = temp.path().join("workspace");
    let data_path = temp.path().join("data");
    let state_path = temp.path().join("state");
    std::fs::create_dir_all(workspace_path.join("src")).unwrap();
    std::fs::create_dir_all(&data_path).unwrap();
    write(
        &workspace_path,
        "src/Calculator.cs",
        "namespace Acceptance;\npublic sealed class Calculator { public int Add(int a, int b) => a + b; }\n",
    );
    write(
        &workspace_path,
        "src/CallSite.cs",
        "namespace Acceptance;\npublic static class CallSite { public static int Run(Calculator value) => value.Add(1, 2); }\n",
    );
    let workspace = Workspace::resolve(&workspace_path, &data_path).unwrap();
    let config = Config {
        data_dir: data_path,
        lsp_timeout_seconds: 1,
        csharp: CSharpLspConfig {
            path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
            args: vec![
                "--mode".to_owned(),
                mode.to_owned(),
                "--state".to_owned(),
                state_path.to_string_lossy().into_owned(),
            ],
            disabled: false,
        },
        ..Config::default()
    };
    (config, workspace, state_path)
}

fn fake_typescript_fixture(
    temp: &tempfile::TempDir,
    mode: &str,
) -> (Config, Workspace, std::path::PathBuf) {
    let workspace_path = temp.path().join("workspace");
    let data_path = temp.path().join("data");
    let state_path = temp.path().join("state");
    std::fs::create_dir_all(workspace_path.join("src")).unwrap();
    std::fs::create_dir_all(&data_path).unwrap();
    write(
        &workspace_path,
        "src/Calculator.ts",
        "export class Calculator {\n  add(a: number, b: number): number {\n    return a + b;\n  }\n}\n",
    );
    write(
        &workspace_path,
        "src/CallSite.ts",
        "import { Calculator } from './Calculator';\nexport function run(value: Calculator): number {\n  return value.add(1, 2);\n}\n",
    );
    let workspace = Workspace::resolve(&workspace_path, &data_path).unwrap();
    let config = Config {
        data_dir: data_path,
        lsp_timeout_seconds: 1,
        typescript: TypeScriptLspConfig {
            path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
            args: vec![
                "--mode".to_owned(),
                mode.to_owned(),
                "--state".to_owned(),
                state_path.to_string_lossy().into_owned(),
                "--primary-file".to_owned(),
                "src/Calculator.ts".to_owned(),
                "--secondary-file".to_owned(),
                "src/CallSite.ts".to_owned(),
            ],
            disabled: false,
        },
        ..Config::default()
    };
    (config, workspace, state_path)
}

fn fake_python_fixture(
    temp: &tempfile::TempDir,
    mode: &str,
) -> (Config, Workspace, std::path::PathBuf) {
    let workspace_path = temp.path().join("workspace");
    let data_path = temp.path().join("data");
    let state_path = temp.path().join("state");
    std::fs::create_dir_all(workspace_path.join("src")).unwrap();
    std::fs::create_dir_all(&data_path).unwrap();
    write(
        &workspace_path,
        "src/Calculator.py",
        "class Calculator:\n    def add(self, a, b):\n        return a + b\n",
    );
    write(
        &workspace_path,
        "src/CallSite.py",
        "from .Calculator import Calculator\n\ndef run(value):\n    return value.add(1, 2)\n",
    );
    let workspace = Workspace::resolve(&workspace_path, &data_path).unwrap();
    let config = Config {
        data_dir: data_path,
        lsp_timeout_seconds: 1,
        python: PythonLspConfig {
            path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
            args: vec![
                "--mode".to_owned(),
                mode.to_owned(),
                "--state".to_owned(),
                state_path.to_string_lossy().into_owned(),
                "--primary-file".to_owned(),
                "src/Calculator.py".to_owned(),
                "--secondary-file".to_owned(),
                "src/CallSite.py".to_owned(),
            ],
            disabled: false,
        },
        ..Config::default()
    };
    (config, workspace, state_path)
}

fn state_count(state: &std::path::Path, name: &str) -> u64 {
    std::fs::read_to_string(state.join(name))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}

#[tokio::test]
async fn fake_csharp_lsp_maps_symbols_definitions_and_references() {
    for (mode, operation) in [
        ("workspace-symbols", "symbols"),
        ("definition", "definition"),
        ("references", "references"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (config, workspace, _) = fake_csharp_fixture(&temp, mode);
        let manager = Manager::new(&config);
        let adapter = csharp_adapter(&manager);
        let results = match operation {
            "symbols" => manager
                .symbols(&adapter, &workspace, "Calculator")
                .await
                .unwrap(),
            "definition" => manager
                .definition(
                    &adapter,
                    &workspace,
                    &workspace.path.join("src/CallSite.cs"),
                    "src/CallSite.cs",
                    1,
                    72,
                )
                .await
                .unwrap(),
            "references" => manager
                .references(
                    &adapter,
                    &workspace,
                    &workspace.path.join("src/Calculator.cs"),
                    "src/Calculator.cs",
                    1,
                    49,
                    true,
                )
                .await
                .unwrap(),
            _ => unreachable!(),
        };
        assert!(!results.is_empty());
        assert!(results.iter().all(|result| {
            result.language.as_deref() == Some("csharp")
                && result.provider.as_deref() == Some("csharp-ls")
                && result.start_line >= 1
        }));
        assert_eq!(
            results[0].relative_file_path.as_deref(),
            Some("src/Calculator.cs")
        );
        if operation == "references" {
            assert!(
                results.iter().any(|result| {
                    result.relative_file_path.as_deref() == Some("src/CallSite.cs")
                })
            );
        }
    }
}

#[tokio::test]
async fn fake_csharp_lsp_correlates_responses_and_reuses_healthy_child() {
    for mode in ["unsolicited", "stderr"] {
        let temp = tempfile::tempdir().unwrap();
        let (config, workspace, state) = fake_csharp_fixture(&temp, mode);
        let manager = Manager::new(&config);
        let adapter = csharp_adapter(&manager);
        for _ in 0..2 {
            let results = manager
                .symbols(&adapter, &workspace, "Calculator")
                .await
                .unwrap();
            assert_eq!(results[0].name.as_deref(), Some("Calculator"));
        }
        assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    }
}

#[tokio::test]
async fn fake_csharp_lsp_discards_failed_session_and_restarts_once() {
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state) = fake_csharp_fixture(&temp, "restart-success");
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);
    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);
}

#[tokio::test]
async fn fake_csharp_lsp_timeout_and_malformed_protocol_return_bounded_errors() {
    for mode in ["timeout", "malformed", "exit-during-request"] {
        let temp = tempfile::tempdir().unwrap();
        let (config, workspace, state) = fake_csharp_fixture(&temp, mode);
        let manager = Manager::new(&config);
        let adapter = csharp_adapter(&manager);
        let error = manager
            .symbols(&adapter, &workspace, "Calculator")
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains("timed out")
                || message.contains("exited")
                || message.contains("restart after"),
            "unexpected {mode} error: {message}"
        );
        let expected_spawns = if mode == "timeout" { 1 } else { 2 };
        assert_eq!(
            state_count(&state, "spawn_count.txt"),
            expected_spawns,
            "unexpected spawn count for {mode}"
        );
    }
}

#[tokio::test]
async fn fake_typescript_lsp_maps_symbols_definitions_and_references() {
    for (mode, operation) in [
        ("workspace-symbols", "symbols"),
        ("definition", "definition"),
        ("references", "references"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (config, workspace, _) = fake_typescript_fixture(&temp, mode);
        let manager = Manager::new(&config);
        let adapter = typescript_adapter(&manager);
        let results = match operation {
            "symbols" => manager
                .symbols(&adapter, &workspace, "Calculator")
                .await
                .unwrap(),
            "definition" => manager
                .definition(
                    &adapter,
                    &workspace,
                    &workspace.path.join("src/CallSite.ts"),
                    "src/CallSite.ts",
                    1,
                    40,
                )
                .await
                .unwrap(),
            "references" => manager
                .references(
                    &adapter,
                    &workspace,
                    &workspace.path.join("src/Calculator.ts"),
                    "src/Calculator.ts",
                    1,
                    20,
                    true,
                )
                .await
                .unwrap(),
            _ => unreachable!(),
        };
        assert!(!results.is_empty());
        assert!(results.iter().all(|result| {
            result.language.as_deref() == Some("typescript")
                && result.provider.as_deref() == Some("typescript-language-server")
                && result.start_line >= 1
        }));
        assert_eq!(
            results[0].relative_file_path.as_deref(),
            Some("src/Calculator.ts")
        );
        if operation == "references" {
            assert!(
                results.iter().any(|result| {
                    result.relative_file_path.as_deref() == Some("src/CallSite.ts")
                })
            );
        }
    }
}

#[tokio::test]
async fn fake_python_lsp_maps_symbols_definitions_and_references() {
    for (mode, operation) in [
        ("workspace-symbols", "symbols"),
        ("definition", "definition"),
        ("references", "references"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (config, workspace, _) = fake_python_fixture(&temp, mode);
        let manager = Manager::new(&config);
        let adapter = python_adapter(&manager);
        let results = match operation {
            "symbols" => manager
                .symbols(&adapter, &workspace, "Calculator")
                .await
                .unwrap(),
            "definition" => manager
                .definition(
                    &adapter,
                    &workspace,
                    &workspace.path.join("src/CallSite.py"),
                    "src/CallSite.py",
                    1,
                    30,
                )
                .await
                .unwrap(),
            "references" => manager
                .references(
                    &adapter,
                    &workspace,
                    &workspace.path.join("src/Calculator.py"),
                    "src/Calculator.py",
                    1,
                    10,
                    true,
                )
                .await
                .unwrap(),
            _ => unreachable!(),
        };
        assert!(!results.is_empty());
        assert!(results.iter().all(|result| {
            result.language.as_deref() == Some("python")
                && result.provider.as_deref() == Some("pyright")
                && result.start_line >= 1
        }));
        assert_eq!(
            results[0].relative_file_path.as_deref(),
            Some("src/Calculator.py")
        );
        if operation == "references" {
            assert!(
                results.iter().any(|result| {
                    result.relative_file_path.as_deref() == Some("src/CallSite.py")
                })
            );
        }
    }
}

#[tokio::test]
async fn csharp_lsp_timeout_fails_open_for_search_and_errors_for_navigation() {
    let (temp, mut config, _fake, task) = fixture().await;
    let workspace = temp.path().join("csharp-timeout");
    let state = temp.path().join("csharp-timeout-state");
    write(
        &workspace,
        "src/Calculator.cs",
        "namespace Acceptance;\npublic sealed class Calculator { public int Add(int a, int b) => a + b; }\n",
    );
    config.lsp_timeout_seconds = 1;
    config.csharp = CSharpLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: vec![
            "--mode".to_owned(),
            "timeout".to_owned(),
            "--state".to_owned(),
            state.to_string_lossy().into_owned(),
        ],
        disabled: false,
    };
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app
        .search(&workspace, "Calculator Add", Some(4))
        .await
        .unwrap();
    assert!(report.results.iter().any(|hit| {
        hit.chunk.relative_file_path == "src/Calculator.cs" && hit.chunk.language == "csharp"
    }));
    assert!(
        report
            .warning
            .as_deref()
            .is_some_and(|warning| warning.contains("csharp-ls") && warning.contains("timed out")),
        "unexpected warning: {:?}",
        report.warning
    );

    let error = app
        .definition(&workspace, "src/Calculator.cs", 2, 28)
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("timed out"),
        "unexpected navigation error: {error:#}"
    );
    task.abort();
}

#[tokio::test]
async fn typescript_javascript_only_search_and_navigation_keep_typescript_lsp_optional() {
    let (temp, config, fake, task) = fixture().await;
    let workspace = temp.path().join("web-only");
    write(
        &workspace,
        "src/telemetry.ts",
        "export function buildProductionTelemetryPipeline(events: string[]) { return events; }\n",
    );
    write(
        &workspace,
        "src/runtime.js",
        "export const runtimeReady = () => true;\n",
    );
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app
        .search(&workspace, "buildProductionTelemetryPipeline", Some(4))
        .await
        .unwrap();
    assert_eq!(report.results[0].chunk.language, "typescript");
    assert!(
        report
            .warning
            .as_deref()
            .is_none_or(|warning| !warning.contains("LSP")),
        "unexpected warning: {:?}",
        report.warning
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    fake.embed_fails.store(true, Ordering::SeqCst);
    let lexical_only = app
        .search(&workspace, "runtimeReady", Some(4))
        .await
        .unwrap();
    assert!(lexical_only.results.iter().any(|hit| {
        hit.chunk.relative_file_path == "src/runtime.js"
            && hit.chunk.language == "javascript"
            && hit.retrieval_channels.contains(&"lexical".to_string())
    }));
    let warning = lexical_only.warning.as_deref().unwrap();
    assert!(warning.contains("Query embedding unavailable"));
    assert!(!warning.contains("LSP"));
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    let error = app
        .definition(&workspace, "src/telemetry.ts", 1, 0)
        .await
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("typescript-language-server is disabled"),
        "unexpected error: {error}"
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);
    task.abort();
}

#[tokio::test]
async fn python_only_search_and_navigation_keep_python_lsp_optional() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("python-only");
    write(
        &workspace,
        "src/revenue.py",
        "# Quarterly revenue summary implementation.\ndef compute_quarterly_revenue_summary(rows):\n    return [row for row in rows if row.get(\"amount\")]\n",
    );
    let app = App::open(config).await.unwrap();
    let indexed = app.index(&workspace).await.unwrap();
    assert_eq!(indexed.files, 1);
    assert_eq!(indexed.parsed_files, 1);
    assert_eq!(indexed.embedded_chunks, 1);

    let report = app
        .search(&workspace, "compute_quarterly_revenue_summary", Some(4))
        .await
        .unwrap();
    assert_eq!(report.results[0].chunk.language, "python");
    assert_eq!(report.results[0].chunk.relative_file_path, "src/revenue.py");
    assert!(
        report
            .warning
            .as_deref()
            .is_none_or(|warning| !warning.contains("LSP")),
        "unexpected warning: {:?}",
        report.warning
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    let definition_error = app
        .definition(&workspace, "src/revenue.py", 1, 0)
        .await
        .unwrap_err();
    assert!(
        definition_error.to_string().contains("pyright is disabled"),
        "unexpected error: {definition_error}"
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    let references_error = app
        .references(&workspace, "src/revenue.py", 1, 0, true)
        .await
        .unwrap_err();
    assert!(
        references_error.to_string().contains("pyright is disabled"),
        "unexpected error: {references_error}"
    );
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);
    task.abort();
}

#[tokio::test]
async fn csharp_only_search_and_navigation_keep_csharp_lsp_optional() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("csharp-only");
    write(
        &workspace,
        "src/RevenueService.cs",
        "namespace Billing;\npublic sealed class RevenueService { public decimal ComputeQuarterlyRevenue() => 42m; }\n",
    );
    let app = App::open(config).await.unwrap();
    assert_eq!(app.index(&workspace).await.unwrap().files, 1);
    let report = app
        .search(&workspace, "ComputeQuarterlyRevenue", Some(4))
        .await
        .unwrap();
    assert_eq!(report.results[0].chunk.language, "csharp");
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);

    for error in [
        app.definition(&workspace, "src/RevenueService.cs", 2, 28)
            .await
            .unwrap_err(),
        app.references(&workspace, "src/RevenueService.cs", 2, 28, true)
            .await
            .unwrap_err(),
    ] {
        assert!(
            error.to_string().contains("csharp-ls is disabled"),
            "unexpected error: {error}"
        );
    }
    assert!(!app.status(&workspace).await.unwrap().analyzer_running);
    task.abort();
}
