use super::*;
use local_code_intelligence::{
    config::{CSharpLspConfig, ClangdLspConfig, PythonLspConfig, TypeScriptLspConfig},
    lsp::{
        Location, Manager,
        adapter::LspAdapter,
        transport::{JsonRpcClient, RpcError, TransportConfig},
    },
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

fn clangd_adapter(manager: &Manager, language: &str) -> Arc<dyn LspAdapter> {
    manager
        .adapters_for_language(language)
        .into_iter()
        .next()
        .cloned()
        .expect("clangd adapter is always constructed")
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

/// Builds a fake-clangd fixture for any registered C or C++ extension
/// (`extension`: `"c"`, `"h"`, `"cpp"`, `"cc"`, ...), since one clangd
/// instance/config navigates both languages.
fn fake_clangd_fixture(
    temp: &tempfile::TempDir,
    mode: &str,
    extension: &str,
) -> (Config, Workspace, std::path::PathBuf) {
    let workspace_path = temp.path().join("workspace");
    let data_path = temp.path().join("data");
    let state_path = temp.path().join("state");
    std::fs::create_dir_all(workspace_path.join("src")).unwrap();
    std::fs::create_dir_all(&data_path).unwrap();
    let primary_file = format!("src/Calculator.{extension}");
    let secondary_file = format!("src/CallSite.{extension}");
    write(
        &workspace_path,
        &primary_file,
        "// Calculator performs basic arithmetic for acceptance testing.\nint add(int a, int b) { return a + b; }\n",
    );
    write(
        &workspace_path,
        &secondary_file,
        "int run() { return add(1, 2); }\n",
    );
    let workspace = Workspace::resolve(&workspace_path, &data_path).unwrap();
    let config = Config {
        data_dir: data_path,
        lsp_timeout_seconds: 1,
        clangd: ClangdLspConfig {
            path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
            args: vec![
                "--mode".to_owned(),
                mode.to_owned(),
                "--state".to_owned(),
                state_path.to_string_lossy().into_owned(),
                "--primary-file".to_owned(),
                primary_file,
                "--secondary-file".to_owned(),
                secondary_file,
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

/// `state_count` for a poll that may run before the fake server has written
/// the file (or while it is half written): 0 until there is a whole number.
fn state_count_so_far(state: &std::path::Path, name: &str) -> u64 {
    std::fs::read_to_string(state.join(name))
        .ok()
        .and_then(|text| text.trim().parse().ok())
        .unwrap_or(0)
}

/// What one language's fake server must answer in each of its three modes.
struct NavigationExpectation {
    language: &'static str,
    provider: &'static str,
    /// The declaring file, which every answer starts with, and the file that
    /// calls it, which the references must include.
    primary: String,
    secondary: String,
    /// Where the definition (in `secondary`) and the references (in
    /// `primary`) are asked for: (line, character).
    definition_at: (u32, u32),
    references_at: (u32, u32),
}

/// Symbols, definition and references through the fake server of one
/// language, each in its own mode, all mapped to the expected file, language
/// and provider.
async fn assert_fake_lsp_maps_symbols_definitions_and_references(
    expected: NavigationExpectation,
    fixture: impl Fn(&tempfile::TempDir, &str) -> (Config, Workspace, std::path::PathBuf),
    adapter_of: impl Fn(&Manager) -> Arc<dyn LspAdapter>,
) {
    let NavigationExpectation {
        language,
        provider,
        primary,
        secondary,
        definition_at,
        references_at,
    } = expected;
    for (mode, operation) in [
        ("workspace-symbols", "symbols"),
        ("definition", "definition"),
        ("references", "references"),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (mut config, workspace, _) = fixture(&temp, mode);
        // Nothing here is about the timeout: the fixtures' 1 s failed a slow
        // `initialize` under load. Each fake answers what its mode does not
        // handle (the priming `workspace/symbol` of a `definition` server, say)
        // with `null`, so no call waits on an unanswered request either.
        config.lsp_timeout_seconds = LOADED_BUDGET_SECONDS;
        for args in [
            &mut config.csharp.args,
            &mut config.typescript.args,
            &mut config.python.args,
            &mut config.clangd.args,
        ] {
            args.push("--answer-unhandled".to_owned());
        }
        let manager = Manager::new(&config);
        let adapter = adapter_of(&manager);
        let results = match operation {
            "symbols" => manager
                .symbols(&adapter, &workspace, "Calculator")
                .await
                .unwrap(),
            "definition" => manager
                .definition(
                    &adapter,
                    &workspace,
                    &workspace.path.join(&secondary),
                    &secondary,
                    definition_at.0,
                    definition_at.1,
                )
                .await
                .unwrap(),
            "references" => manager
                .references(
                    &adapter,
                    &workspace,
                    &workspace.path.join(&primary),
                    &primary,
                    references_at.0,
                    references_at.1,
                    true,
                )
                .await
                .unwrap(),
            _ => unreachable!(),
        };
        assert!(!results.is_empty(), "{language} {operation}");
        assert!(
            results.iter().all(|result| {
                result.language.as_deref() == Some(language)
                    && result.provider.as_deref() == Some(provider)
                    && result.start_line >= 1
            }),
            "{language} {operation}"
        );
        assert_eq!(
            results[0].relative_file_path.as_deref(),
            Some(primary.as_str()),
            "{language} {operation}"
        );
        if operation == "references" {
            assert!(
                results
                    .iter()
                    .any(|result| result.relative_file_path.as_deref() == Some(secondary.as_str())),
                "{language} {operation}"
            );
        }
    }
}

/// C# and the ECMAScript and Python servers, and one shared ClangdServer
/// instance navigating both `c` and `cpp` (the same one-adapter-many-languages
/// contract as TypeScript's four ECMAScript-family languages).
#[tokio::test]
async fn fake_lsps_map_symbols_definitions_and_references() {
    let csharp = assert_fake_lsp_maps_symbols_definitions_and_references(
        NavigationExpectation {
            language: "csharp",
            provider: "csharp-ls",
            primary: "src/Calculator.cs".to_owned(),
            secondary: "src/CallSite.cs".to_owned(),
            definition_at: (1, 72),
            references_at: (1, 49),
        },
        fake_csharp_fixture,
        csharp_adapter,
    );
    let typescript = assert_fake_lsp_maps_symbols_definitions_and_references(
        NavigationExpectation {
            language: "typescript",
            provider: "typescript-language-server",
            primary: "src/Calculator.ts".to_owned(),
            secondary: "src/CallSite.ts".to_owned(),
            definition_at: (1, 40),
            references_at: (1, 20),
        },
        fake_typescript_fixture,
        typescript_adapter,
    );
    let python = assert_fake_lsp_maps_symbols_definitions_and_references(
        NavigationExpectation {
            language: "python",
            provider: "pyright",
            primary: "src/Calculator.py".to_owned(),
            secondary: "src/CallSite.py".to_owned(),
            definition_at: (1, 30),
            references_at: (1, 10),
        },
        fake_python_fixture,
        python_adapter,
    );
    // (file extension, indexed language): `.h` is `c`, the others `cpp`.
    let clangd = |extension: &'static str, language: &'static str| {
        assert_fake_lsp_maps_symbols_definitions_and_references(
            NavigationExpectation {
                language,
                provider: "clangd",
                primary: format!("src/Calculator.{extension}"),
                secondary: format!("src/CallSite.{extension}"),
                definition_at: (1, 16),
                references_at: (1, 4),
            },
            |temp, mode| fake_clangd_fixture(temp, mode, extension),
            |manager| clangd_adapter(manager, language),
        )
    };
    // The servers are separate processes, so the languages run side by side.
    // The extra clangd extensions run as a second batch: eleven fake servers
    // at once load the machine more than the check needs.
    tokio::join!(
        csharp,
        typescript,
        python,
        clangd("c", "c"),
        clangd("cpp", "cpp"),
    );
    tokio::join!(
        clangd("h", "c"),
        clangd("cc", "cpp"),
        clangd("cxx", "cpp"),
        clangd("hpp", "cpp"),
        clangd("hh", "cpp"),
        clangd("hxx", "cpp"),
    );
}

/// `JsonRpcClient::request` bounds its wait by inactivity, not total call
/// duration (see its doc comment in lci-core/src/lsp/transport.rs): a
/// single request's response can legitimately arrive well past the
/// configured `lsp_timeout_seconds` in total wall-clock time, as long as
/// the server keeps sending *something* (any message) within that timeout
/// of each other. A blind fixed-duration timeout would fail this exchange
/// outright; the idle-reset behavior should not.
#[tokio::test]
async fn fake_csharp_lsp_survives_a_slow_response_paced_by_progress_notifications() {
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, _) = fake_csharp_fixture(&temp, "slow-with-progress");
    assert_eq!(config.lsp_timeout_seconds, 1);
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let started = std::time::Instant::now();
    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .expect("progress notifications should keep the request alive past 1s");
    let elapsed = started.elapsed();

    assert!(!results.is_empty());
    assert!(
        elapsed >= Duration::from_millis(1500),
        "expected the exchange to genuinely take longer than the nominal \
         1s timeout (proving this isn't trivially fast); took {elapsed:?}"
    );
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
                || message.contains("protocol failure")
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

/// `extra` as owned fake-server arguments, to append to a fixture's.
fn fake_args(extra: &[&str]) -> Vec<String> {
    extra.iter().map(|arg| (*arg).to_owned()).collect()
}

/// A server that answers a request with a transient error response
/// (ContentModified, as rust-analyzer does while its workspace reloads) is
/// alive and only not ready yet: the request is repeated on the same server
/// after a short wait and succeeds, with no restart and no shutdown.
#[tokio::test]
async fn a_content_modified_answer_is_repeated_on_the_same_server() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.csharp.args.extend(fake_args(&[
        "--symbol-error",
        "-32801:content modified",
        "--symbol-error-count",
        "2",
    ]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let started = std::time::Instant::now();
    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert!(
        started.elapsed() >= Duration::from_millis(1900),
        "the repeats wait 0.5 s and 1.5 s before asking again: {:?}",
        started.elapsed()
    );
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    // initialize, initialized, the priming request, the request, two repeats.
    assert_eq!(state_count(&state, "request_count.txt"), 6);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);
    assert!(manager.running("csharp-ls", &workspace.id).await);
}

/// A server that never stops answering with a transient error must still be
/// answered eventually: the error comes back after the bounded repeats, as the
/// provider's error with its code, and the session is kept (the server is
/// alive; a restart would only be as cold).
#[tokio::test]
async fn a_server_that_keeps_answering_a_transient_error_is_answered_with_it_and_kept() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config
        .csharp
        .args
        .extend(fake_args(&["--symbol-error", "-32801:content modified"]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let started = std::time::Instant::now();
    let error = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap_err();
    // The repeats wait 2 s in all; a loose bound for the server start.
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "the same-server repeats must be bounded, took {:?}",
        started.elapsed()
    );
    let response = error
        .downcast_ref::<RpcError>()
        .expect("the server's own error response, not a transport failure");
    assert_eq!(response.code, -32801);
    assert_eq!(response.message, "content modified");
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    // initialize, initialized, the priming request, the request, two repeats.
    assert_eq!(state_count(&state, "request_count.txt"), 6);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);
    assert!(manager.running("csharp-ls", &workspace.id).await);
}

/// A server that answers nothing but transient errors is wedged, not loading
/// (N35): once it has done so for `lsp_stuck_seconds`, its session is replaced.
/// Each call still returns the server's own error, the server is asked to shut
/// down, and the next call starts a fresh one, whose span starts over.
///
/// The span counts every error response, each repeat's too: one failed call
/// is three answers over about 2 s (the repeats wait 0.5 s and 1.5 s, and a
/// repeat may start at most 5 s after the call's first answer, however late the
/// timers run), so a limit of 6 s cannot be reached during the first call, and
/// is reached by the first answer of the second call after a pause of 4.1 s
/// (2 s + 4.1 s at the least).
#[tokio::test]
async fn a_server_that_answers_only_transient_errors_for_the_stuck_span_is_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_stuck_seconds = 6;
    config
        .csharp
        .args
        .extend(fake_args(&["--symbol-error", "-32801:content modified"]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    // The first call starts the span; its three answers cover about 2 s.
    let error = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<RpcError>().map(|e| e.code),
        Some(-32801)
    );
    assert!(manager.running("csharp-ls", &workspace.id).await);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);

    // The second call's first answer comes at least 6.1 s after the first
    // call's first: the caller still gets the server's error, and the session is
    // discarded.
    tokio::time::sleep(Duration::from_millis(4100)).await;
    let error = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<RpcError>().map(|e| e.code),
        Some(-32801)
    );
    wait_until(
        "the stuck server to be shut down",
        Duration::from_secs(30),
        async || {
            !manager.running("csharp-ls", &workspace.id).await
                && event_count(&state, "exit_seen.txt") == 1
        },
    )
    .await;
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 1);
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);

    // The next call is served by a fresh server (which answers the same way),
    // and its session is kept: its own span has only just begun.
    let error = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<RpcError>().map(|e| e.code),
        Some(-32801)
    );
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);
    assert!(manager.running("csharp-ls", &workspace.id).await);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 1);
}

/// A server that crosses the stuck span and then answers with a result is not
/// wedged: the result takes the mark back, and its session is kept (one spawn,
/// no shutdown) instead of being discarded with the load the server has just
/// finished. The first call's three errors start the span; the second call's
/// first answer (at least 6.1 s after the first call's first, over the 6 s
/// limit) marks the session, and its repeat (the 6th `workspace/symbol`, the
/// priming request counted) is a result.
#[tokio::test]
async fn a_server_that_answers_with_a_result_after_the_stuck_span_keeps_its_session() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_stuck_seconds = 6;
    config.csharp.args.extend(fake_args(&[
        "--symbol-error",
        "-32801:content modified",
        "--symbol-ok-at",
        "6",
    ]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let error = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<RpcError>().map(|e| e.code),
        Some(-32801)
    );
    tokio::time::sleep(Duration::from_millis(4100)).await;
    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    // initialize, initialized, priming, three answers, then an error and a result.
    assert_eq!(state_count(&state, "request_count.txt"), 8);
    assert!(manager.running("csharp-ls", &workspace.id).await);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
}

/// A call that is cancelled before its repeats end (a search past its latency
/// budget, which is how most calls to a wedged server end) still counts: the
/// answers it received extended the span, and a later call for the key replaces
/// the session although no call ever saw the end of a request.
///
/// Each call is dropped after 1 s, between its second answer (about 0.5 s) and
/// its third (about 2 s), so none of them returns. The first answers of five
/// calls are about 1 s apart: the span passes the 2 s limit during the fourth
/// call at the latest (the first one only starts the server), and some later
/// call finds the session stuck. On a loaded machine the start takes longer and
/// the span can pass the limit only during the last call, so the calls go on
/// (up to 30) until one has started the replacement, rather than a fixed six.
#[tokio::test]
async fn cancelled_calls_still_count_toward_the_stuck_span() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_stuck_seconds = 2;
    config
        .csharp
        .args
        .extend(fake_args(&["--symbol-error", "-32801:content modified"]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let mut calls = 0;
    while state_count_so_far(&state, "spawn_count.txt") < 2 {
        assert!(
            calls < 30,
            "no call replaced the stuck session in {calls} calls"
        );
        let outcome = tokio::time::timeout(
            Duration::from_millis(1000),
            manager.symbols(&adapter, &workspace, "Calculator"),
        )
        .await;
        assert!(outcome.is_err(), "the call must be cancelled, not answered");
        calls += 1;
    }
    wait_until(
        "the stuck server to be shut down",
        Duration::from_secs(30),
        async || event_count(&state, "exit_seen.txt") >= 1,
    )
    .await;
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 1);
    assert_eq!(
        state_count(&state, "spawn_count.txt"),
        2,
        "one replacement: the fresh session's own span is still short"
    );
}

/// `lsp_stuck_seconds = 0` never replaces a session for transient errors: the
/// same server that is replaced above keeps its session across any number of
/// calls (the behaviour before N35).
#[tokio::test]
async fn lsp_stuck_seconds_zero_never_replaces_a_server_for_transient_errors() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_stuck_seconds = 0;
    config
        .csharp
        .args
        .extend(fake_args(&["--symbol-error", "-32801:content modified"]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    for _ in 0..3 {
        let error = manager
            .symbols(&adapter, &workspace, "Calculator")
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<RpcError>().map(|e| e.code),
            Some(-32801)
        );
    }
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);
    assert!(manager.running("csharp-ls", &workspace.id).await);
}

/// A server that reports progress while it answers "ask again" is loading, not
/// wedged: every answer that finds new progress starts the span over, so the
/// same server that is replaced after 2 s without progress keeps its session
/// through three calls of about 2 s each.
#[tokio::test]
async fn progress_notifications_keep_a_loading_server_from_being_replaced() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_stuck_seconds = 2;
    config.csharp.args.extend(fake_args(&[
        "--symbol-error",
        "-32801:content modified",
        "--progress-before-error",
    ]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    for _ in 0..3 {
        let error = manager
            .symbols(&adapter, &workspace, "Calculator")
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<RpcError>().map(|e| e.code),
            Some(-32801)
        );
    }
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);
    assert!(manager.running("csharp-ls", &workspace.id).await);
}

/// `RequestCancelled` (-32800) is the answer to a request that was cancelled,
/// ours or the server's own: it says nothing about the server's health. Calls
/// that this client cancels (a search past its latency budget) get that answer
/// late, after the pending entry is gone, and the reader drops it; the server
/// answers every one with -32800 for longer than `lsp_stuck_seconds` here, and
/// the session is kept.
#[tokio::test]
async fn answers_to_cancelled_requests_do_not_make_a_server_stuck() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_stuck_seconds = 1;
    config.csharp.args.extend(fake_args(&[
        "--symbol-error",
        "-32800:request cancelled",
        "--symbol-delay-ms",
        "400",
    ]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    for _ in 0..8 {
        let outcome = tokio::time::timeout(
            Duration::from_millis(300),
            manager.symbols(&adapter, &workspace, "Calculator"),
        )
        .await;
        assert!(outcome.is_err(), "the call must be cancelled, not answered");
    }
    // The server works through its backlog, answering each cancelled request
    // 400 ms after the last: wait until it has read the cancellations too.
    wait_until(
        "the server to read the cancelled requests",
        Duration::from_secs(30),
        async || state_count_so_far(&state, "request_count.txt") >= 12,
    )
    .await;
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);
    assert!(manager.running("csharp-ls", &workspace.id).await);
}

/// An uncancelled -32800 is an error that is not transient: the caller gets it
/// at once (no repeats) and it ends the span like any answer that is not "ask
/// again", so a server answering that way over more than `lsp_stuck_seconds`
/// keeps its session.
#[tokio::test]
async fn a_request_cancelled_error_is_not_transient_and_does_not_make_a_server_stuck() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_stuck_seconds = 1;
    config
        .csharp
        .args
        .extend(fake_args(&["--symbol-error", "-32800:request cancelled"]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    for _ in 0..4 {
        let error = manager
            .symbols(&adapter, &workspace, "Calculator")
            .await
            .unwrap_err();
        assert_eq!(
            error.downcast_ref::<RpcError>().map(|e| e.code),
            Some(-32800)
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // initialize, initialized, priming, and one request per call: no repeats.
    assert_eq!(state_count(&state, "request_count.txt"), 7);
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);
    assert!(manager.running("csharp-ls", &workspace.id).await);
}

/// A result ends the span: a server that fails (three answers over about 2 s),
/// answers once, and fails again is not stuck, although the errors of the two
/// failing calls together span about 4 s, past the 3 s limit. Without the
/// reset on a result the third call would end that span and replace the session.
#[tokio::test]
async fn a_result_between_transient_errors_ends_the_stuck_span() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_stuck_seconds = 3;
    // Requests 2 to 4 (the first call and its repeats) fail, request 5 is
    // answered, requests 6 to 8 (the third call) fail.
    config.csharp.args.extend(fake_args(&[
        "--symbol-error",
        "-32801:content modified",
        "--symbol-ok-at",
        "5",
    ]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let error = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<RpcError>().map(|e| e.code),
        Some(-32801)
    );
    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    let error = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap_err();
    assert_eq!(
        error.downcast_ref::<RpcError>().map(|e| e.code),
        Some(-32801)
    );
    // initialize, initialized, priming, three failing answers, the result, and
    // three more failing answers.
    assert_eq!(state_count(&state, "request_count.txt"), 10);
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);
    assert!(manager.running("csharp-ls", &workspace.id).await);
}

/// A slow answer is not an error: a server that takes longer than the stuck
/// span to answer every request, successfully, is never replaced.
#[tokio::test]
async fn a_slow_successful_answer_is_not_a_stuck_server() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_stuck_seconds = 1;
    config
        .csharp
        .args
        .extend(fake_args(&["--symbol-delay-ms", "1500"]));
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
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);
    assert!(manager.running("csharp-ls", &workspace.id).await);
}

/// tsserver answers `workspace/symbol` with "No Project" for every call until
/// a file is opened. That is an answer, not a dead server: each call returns
/// the provider's error (the caller fails open across providers), nothing is
/// repeated, and the server is never discarded or restarted.
#[tokio::test]
async fn an_error_response_that_is_not_transient_never_restarts_the_server() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_typescript_fixture(&temp, "workspace-symbols");
    config.lsp_timeout_seconds = 5;
    config.typescript.args.extend(fake_args(&[
        "--symbol-error",
        "1:No Project. Please open a file first",
    ]));
    let manager = Manager::new(&config);
    let adapter = typescript_adapter(&manager);

    for _ in 0..3 {
        let error = manager
            .symbols(&adapter, &workspace, "Calculator")
            .await
            .unwrap_err();
        assert!(format!("{error:#}").contains("No Project"), "{error:#}");
        assert_eq!(error.downcast_ref::<RpcError>().map(|e| e.code), Some(1));
    }
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    // initialize, initialized, the priming request, one request per call.
    assert_eq!(state_count(&state, "request_count.txt"), 6);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);
    assert!(
        manager
            .running("typescript-language-server", &workspace.id)
            .await
    );
}

/// Through the application: the TypeScript server answering "No Project" for
/// every call does not fail `symbols` while another applicable server
/// answered, and it is started once however many searches ask.
#[tokio::test]
async fn a_server_that_finds_no_project_does_not_fail_the_symbols_of_a_mixed_workspace() {
    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    write_csharp_and_typescript_workspace(&workspace);
    let mut config =
        with_fake_csharp_and_typescript_servers(config, &state, "no-symbols", "workspace-symbols");
    config
        .typescript
        .args
        .extend(fake_args(&["--symbol-error", "1:No Project"]));
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    for _ in 0..2 {
        let report = app
            .symbols(&workspace, "NoSuchSymbolAnywhere")
            .await
            .expect("one server answered, so the call must not fail");
        assert!(report.results.is_empty(), "{:?}", report.results);
    }
    assert_eq!(state_count(&state.join("typescript"), "spawn_count.txt"), 1);
    assert_eq!(state_count(&state.join("csharp"), "spawn_count.txt"), 1);
    models_task.abort();
}

/// A server that dies while a request is in flight is a transport failure:
/// the session is discarded and the request is served by one fresh server.
#[tokio::test]
async fn a_server_that_dies_mid_request_is_replaced_once() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    // The 2nd `workspace/symbol` of the first process (the request after
    // the priming one) kills it.
    config
        .csharp
        .args
        .extend(fake_args(&["--exit-on-symbol-request", "2"]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    for _ in 0..2 {
        let results = manager
            .symbols(&adapter, &workspace, "Calculator")
            .await
            .unwrap();
        assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    }
    assert_eq!(
        state_count(&state, "spawn_count.txt"),
        2,
        "one restart for the death, none for the call after it"
    );
}

/// A client for the fake server started directly, past its `initialize`
/// handshake, with the transport's inactivity and write bound at `timeout`.
async fn fake_client(
    temp: &tempfile::TempDir,
    extra: &[&str],
    timeout: Duration,
) -> (Arc<JsonRpcClient>, std::path::PathBuf) {
    fake_client_with(temp, extra, timeout, Box::new(|_| false)).await
}

/// [`fake_client`] with a readiness predicate.
async fn fake_client_with(
    temp: &tempfile::TempDir,
    extra: &[&str],
    timeout: Duration,
    ready: Box<dyn Fn(&serde_json::Value) -> bool + Send>,
) -> (Arc<JsonRpcClient>, std::path::PathBuf) {
    let state = temp.path().join("state");
    let mut args = fake_args(&["--mode", "workspace-symbols", "--state"]);
    args.push(state.to_string_lossy().into_owned());
    args.extend(fake_args(extra));
    let transport = TransportConfig {
        executable: env!("CARGO_BIN_EXE_fake-lsp-server").to_owned(),
        args,
        working_dir: temp.path().to_owned(),
        timeout,
        label: "fake".to_owned(),
    };
    let client = JsonRpcClient::spawn(&transport, &[], ready).await.unwrap();
    client
        .request(
            "initialize",
            json!({"rootUri": "file:///workspace/", "capabilities": {}}),
        )
        .await
        .unwrap();
    client.notify("initialized", json!({})).await.unwrap();
    (client, state)
}

/// A message that announces more than the client will ever read is refused by
/// its header: the body is not allocated and not waited for (the server here
/// sends the header of a 4 GiB message and then nothing). The request fails at
/// once, naming the limit, and so does every later one, instead of each
/// waiting out the inactivity timeout (30 s here) behind a reader that has
/// stopped.
#[tokio::test]
async fn a_frame_that_announces_too_much_fails_the_request_and_the_ones_after_it_at_once() {
    let temp = tempfile::tempdir().unwrap();
    let (client, _state) = fake_client(
        &temp,
        &["--huge-frame-on-symbol-request", "1"],
        Duration::from_secs(30),
    )
    .await;
    let started = std::time::Instant::now();
    let first = tokio::time::timeout(
        Duration::from_secs(10),
        client.request("workspace/symbol", json!({"query": "Calculator"})),
    )
    .await
    .expect("the oversize header must fail the request, not leave it waiting for 4 GiB")
    .unwrap_err();
    assert!(format!("{first:#}").contains("over the limit"), "{first:#}");
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "{:?}",
        started.elapsed()
    );

    let again = std::time::Instant::now();
    let later = tokio::time::timeout(
        Duration::from_secs(10),
        client.request("workspace/symbol", json!({"query": "Calculator"})),
    )
    .await
    .expect("a request after the reader stopped must not wait for an answer")
    .unwrap_err();
    assert!(format!("{later:#}").contains("over the limit"), "{later:#}");
    assert!(
        later.downcast_ref::<RpcError>().is_none(),
        "a dead transport is not a server's error answer"
    );
    assert!(
        again.elapsed() < Duration::from_secs(1),
        "{:?}",
        again.elapsed()
    );
    client.shutdown(Duration::from_secs(1)).await;
}

/// The same server through the manager: whether the oversize header answers
/// the priming request (1) or the caller's request (2), the session is
/// discarded and one fresh server serves the call, within a few seconds
/// although the inactivity timeout is 30 s.
#[tokio::test]
async fn a_server_that_announces_a_huge_frame_is_replaced_once() {
    for at_request in ["1", "2"] {
        let temp = tempfile::tempdir().unwrap();
        let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
        config.lsp_timeout_seconds = 30;
        config
            .csharp
            .args
            .extend(fake_args(&["--huge-frame-on-symbol-request", at_request]));
        let manager = Manager::new(&config);
        let adapter = csharp_adapter(&manager);

        let started = std::time::Instant::now();
        let results = tokio::time::timeout(
            Duration::from_secs(20),
            manager.symbols(&adapter, &workspace, "Calculator"),
        )
        .await
        .unwrap_or_else(|_| panic!("request {at_request}: the call waited for the huge frame"))
        .unwrap();
        assert_eq!(results[0].name.as_deref(), Some("Calculator"));
        assert!(
            started.elapsed() < Duration::from_secs(15),
            "request {at_request}: {:?}",
            started.elapsed()
        );
        assert_eq!(
            state_count(&state, "spawn_count.txt"),
            2,
            "request {at_request}: one restart"
        );
    }
}

/// A request that the server makes with a string for an id (JSON-RPC allows
/// one) is a request, not a notification: it is answered with the same id, and
/// a `custom/ready` that arrives as a request does not make the client ready
/// (the predicate is for notifications).
#[tokio::test]
async fn a_server_request_with_a_string_id_is_answered_with_that_id_and_is_not_a_notification() {
    let temp = tempfile::tempdir().unwrap();
    let (client, state) = fake_client_with(
        &temp,
        &["--string-id-server-requests"],
        Duration::from_secs(30),
        Box::new(|message| message["method"] == "custom/ready"),
    )
    .await;
    let replies = state.join("server_request_replies.txt");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let text = loop {
        let text = std::fs::read_to_string(&replies).unwrap_or_default();
        if text.lines().count() >= 2 || std::time::Instant::now() >= deadline {
            break text;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [r#""ready-1" null"#, r#""cfg-1" [null,null]"#],
        "the client's answers, with the ids as the server wrote them"
    );
    assert!(
        !client.is_ready(),
        "a request is not the ready notification"
    );
    client.shutdown(Duration::from_secs(1)).await;
}

/// A timeout too long to add to the clock (`Duration::MAX`) is a request that
/// is never given up on for inactivity, not a panic of the calling task.
#[tokio::test]
async fn a_transport_timeout_too_long_for_the_clock_does_not_panic_a_request() {
    let temp = tempfile::tempdir().unwrap();
    let (client, _state) = fake_client(&temp, &[], Duration::MAX).await;
    let answer = client
        .request("workspace/symbol", json!({"query": "x"}))
        .await
        .unwrap();
    assert!(answer.is_array(), "{answer}");
    client.shutdown(Duration::from_secs(1)).await;
}

/// A server that is alive but has stopped reading its input: a request big
/// enough to fill the pipe (4 MiB) cannot be written, and the write is bounded
/// by the client timeout (2 s) instead of holding the caller, and the write
/// lock, forever. The client then refuses further writes at once, because the
/// message that timed out may have left half a frame in the pipe.
#[tokio::test]
async fn a_server_that_stops_reading_fails_the_write_within_the_timeout() {
    let temp = tempfile::tempdir().unwrap();
    let (client, _state) =
        fake_client(&temp, &["--never-read-stdin"], Duration::from_secs(2)).await;
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(
        Duration::from_secs(20),
        client.request(
            "workspace/symbol",
            json!({"query": "x".repeat(4 * 1024 * 1024)}),
        ),
    )
    .await
    .expect("a full pipe must not hold the request past the write bound")
    .unwrap_err();
    assert!(
        format!("{error:#}").contains("did not take a message within 2 seconds"),
        "{error:#}"
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );

    let again = std::time::Instant::now();
    let later = tokio::time::timeout(
        Duration::from_secs(10),
        client.request("workspace/symbol", json!({"query": "x"})),
    )
    .await
    .expect("a write after a timed-out write must fail at once")
    .unwrap_err();
    assert!(
        format!("{later:#}").contains("stopped reading its input"),
        "{later:#}"
    );
    assert!(
        again.elapsed() < Duration::from_secs(1),
        "{:?}",
        again.elapsed()
    );
    client.shutdown(Duration::from_secs(1)).await;
}

/// A write that queued behind a write which then ran out of time does not
/// start: the earlier message may be half written, and the checks before the
/// wait for the lock cannot know that. Here the second call arrives 1.5 s after
/// the first, while the first holds the pipe, and its own bound would end 1.5 s
/// after the first's; it fails when the first one gives up, with the refusal
/// and not with its own timeout.
#[tokio::test]
async fn a_write_that_waited_behind_a_timed_out_write_does_not_start() {
    let temp = tempfile::tempdir().unwrap();
    let (client, _state) =
        fake_client(&temp, &["--never-read-stdin"], Duration::from_secs(3)).await;
    let first = {
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            client
                .request(
                    "workspace/symbol",
                    json!({"query": "x".repeat(4 * 1024 * 1024)}),
                )
                .await
        })
    };
    tokio::time::sleep(Duration::from_millis(1500)).await;
    let second = tokio::time::timeout(
        Duration::from_secs(20),
        client.request("workspace/symbol", json!({"query": "x"})),
    )
    .await
    .expect("a queued write must not wait on a pipe nobody reads")
    .unwrap_err();
    assert!(
        format!("{second:#}").contains("stopped reading its input"),
        "{second:#}"
    );
    let first = first.await.unwrap().unwrap_err();
    assert!(
        format!("{first:#}").contains("did not take a message within 3 seconds"),
        "{first:#}"
    );
    client.shutdown(Duration::from_secs(1)).await;
}

/// The same deaf server through the manager: the write that cannot finish is a
/// transport failure, so the session is discarded and one fresh server serves
/// the call, instead of the call failing or hanging.
#[tokio::test]
async fn a_server_that_stops_reading_is_replaced_once() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_timeout_seconds = 2;
    config
        .csharp
        .args
        .extend(fake_args(&["--never-read-stdin"]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    // The 2 s timeout is the product's bound on the deaf server's write; the
    // rest of the call is a fresh start that reads the 4 MiB query, which took
    // 26 s on a loaded machine. A write waiting on the deaf pipe never ends,
    // so the wide bounds below still catch it.
    let query = "x".repeat(4 * 1024 * 1024);
    let started = std::time::Instant::now();
    let results = tokio::time::timeout(
        3 * LOADED_BUDGET,
        manager.symbols(&adapter, &workspace, &query),
    )
    .await
    .expect("the call waited on a pipe nobody reads")
    .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert!(
        started.elapsed() < 2 * LOADED_BUDGET,
        "{:?}",
        started.elapsed()
    );
    assert_eq!(state_count(&state, "spawn_count.txt"), 2, "one restart");
}

/// An `error` member that is not a well-formed error object (`null`, no
/// `code`, a string `code`, no `message`, a bare string) is not an answer the
/// client can act on: it is a protocol failure, handled like a transport
/// failure. The session is discarded (its server is shut down) and the request
/// is served once by a fresh server, instead of the garbage being taken for an
/// error response with code 0 and the confused server being kept.
#[tokio::test]
async fn a_server_that_answers_with_a_malformed_error_is_replaced_once() {
    for raw in [
        "null",
        r#"{"message":"no code"}"#,
        r#"{"code":"1","message":"string code"}"#,
        r#"{"code":1}"#,
        r#""just text""#,
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
        // The first process answers its 2nd `workspace/symbol` (the request
        // after the priming one) with the raw `error` member; the replacement
        // answers normally.
        config
            .csharp
            .args
            .extend(fake_args(&["--symbol-raw-error", raw]));
        let manager = Manager::new(&config);
        let adapter = csharp_adapter(&manager);

        let results = manager
            .symbols(&adapter, &workspace, "Calculator")
            .await
            .unwrap_or_else(|error| {
                panic!("{raw}: the replacement server must serve the request: {error:#}")
            });
        assert_eq!(results[0].name.as_deref(), Some("Calculator"), "{raw}");
        assert_eq!(
            state_count(&state, "spawn_count.txt"),
            2,
            "{raw}: one restart, not a kept session"
        );
        wait_until(
            "the server that sent the malformed error to be shut down",
            Duration::from_secs(15),
            async || event_count(&state, "shutdown_seen.txt") >= 1,
        )
        .await;
        assert!(manager.running("csharp-ls", &workspace.id).await, "{raw}");
    }
}

/// A discarded server is stopped the way a released one is, through the
/// protocol: it gets `shutdown` and `exit`. The server here is alive but stalls
/// on a request, so the call fails by the inactivity timeout, which is a
/// transport failure, and the first server is asked to shut down even though
/// nobody calls `release`.
#[tokio::test]
async fn a_discarded_server_is_asked_to_shut_down() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    // 4 s, not 2: the same bound covers writing `initialize` to a server that
    // is still starting, which took over 2 s under a parallel build.
    config.lsp_timeout_seconds = 4;
    // Every `workspace/symbol` after the priming one stalls for 5.6 s: past
    // the 4 s timeout, so the call fails, yet inside the 4 s the discard's
    // shutdown waits for an answer, so the stalled server wakes up, reads the
    // `shutdown` and `exit` queued behind its request, and ends.
    config
        .csharp
        .args
        .extend(fake_args(&["--symbol-delay-ms", "5600"]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let error = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap_err();
    assert!(
        format!("{error:#}").contains("timed out"),
        "unexpected error: {error:#}"
    );
    assert_eq!(
        state_count(&state, "spawn_count.txt"),
        2,
        "the stalled server is replaced once, and the replacement stalls too"
    );
    wait_until(
        "the discarded server to be shut down",
        Duration::from_secs(30),
        async || {
            event_count(&state, "shutdown_seen.txt") >= 1
                && event_count(&state, "exit_seen.txt") >= 1
        },
    )
    .await;
    // Only the first server was stopped; the replacement still serves the
    // session.
    assert!(manager.running("csharp-ls", &workspace.id).await);
}

/// What `textDocument/definition` on the fixture's call site returns.
async fn call_site_definition(
    manager: &Manager,
    adapter: &Arc<dyn LspAdapter>,
    workspace: &Workspace,
) -> anyhow::Result<Vec<Location>> {
    manager
        .definition(
            adapter,
            workspace,
            &workspace.path.join("src/CallSite.cs"),
            "src/CallSite.cs",
            1,
            72,
        )
        .await
}

/// Wraps the C# adapter with rust-analyzer's loading rule and a readiness
/// predicate that the fake's `custom/ready` notification satisfies, to test
/// what the manager does with an adapter's [`LspAdapter::is_loading_error`].
struct LoadingAwareAdapter(Arc<dyn LspAdapter>);

impl LspAdapter for LoadingAwareAdapter {
    fn provider(&self) -> &'static str {
        self.0.provider()
    }
    fn languages(&self) -> &'static [&'static str] {
        self.0.languages()
    }
    fn enabled(&self) -> bool {
        self.0.enabled()
    }
    fn configured_command(&self) -> Option<String> {
        self.0.configured_command()
    }
    fn readiness_component_name(&self) -> &'static str {
        self.0.readiness_component_name()
    }
    fn transport_config(&self, root: &std::path::Path) -> Option<TransportConfig> {
        self.0.transport_config(root)
    }
    fn initialize_params(&self, root: &std::path::Path) -> Option<Value> {
        self.0.initialize_params(root)
    }
    fn ready_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        Box::new(|message| message.get("method").and_then(Value::as_str) == Some("custom/ready"))
    }
    fn is_loading_error(&self, error: &RpcError, ready: bool) -> bool {
        !ready && error.code == RpcError::INTERNAL_ERROR && error.message.contains("file not found")
    }
}

/// An error that only means "still loading" while the server is not ready
/// (rust-analyzer's "file not found") is repeated on the same server until it
/// loads, and returned at once after the server reports it is ready, when the
/// same text means the file really is unknown. A server with no such rule
/// (the plain C# adapter) gets the error back at once either way. The fake
/// answers the error for the first N definition requests only, so a call that
/// gets the error back was not repeated, and one that gets the definition was.
#[tokio::test]
async fn an_adapters_loading_error_is_repeated_only_until_the_server_is_ready() {
    let not_found = "-32603:file not found: src/CallSite.cs";

    // Not ready: two "file not found" answers, then the definition.
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_fixture(&temp, "definition");
    config.csharp.args.extend(fake_args(&[
        "--definition-error",
        not_found,
        "--definition-error-count",
        "2",
    ]));
    let manager = Manager::new(&config);
    let adapter: Arc<dyn LspAdapter> = Arc::new(LoadingAwareAdapter(csharp_adapter(&manager)));
    let started = std::time::Instant::now();
    let results = call_site_definition(&manager, &adapter, &workspace)
        .await
        .unwrap();
    assert_eq!(
        results[0].relative_file_path.as_deref(),
        Some("src/Calculator.cs")
    );
    assert!(
        started.elapsed() >= Duration::from_millis(1900),
        "two repeats wait 0.5 s and 1.5 s: {:?}",
        started.elapsed()
    );
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);

    // Ready: the same error is the real answer, returned at once.
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_fixture(&temp, "definition");
    config.csharp.args.extend(fake_args(&[
        "--ready-notification",
        "--definition-error",
        not_found,
        "--definition-error-count",
        "1",
    ]));
    let manager = Manager::new(&config);
    let adapter: Arc<dyn LspAdapter> = Arc::new(LoadingAwareAdapter(csharp_adapter(&manager)));
    let error = call_site_definition(&manager, &adapter, &workspace)
        .await
        .expect_err("a ready server's answer is not repeated");
    assert!(format!("{error:#}").contains("file not found"), "{error:#}");
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);

    // No loading rule at all: the plain C# adapter returns it at once.
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_fixture(&temp, "definition");
    config.csharp.args.extend(fake_args(&[
        "--definition-error",
        not_found,
        "--definition-error-count",
        "1",
    ]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);
    let error = call_site_definition(&manager, &adapter, &workspace)
        .await
        .expect_err("an error with no loading rule is not repeated");
    assert!(format!("{error:#}").contains("file not found"), "{error:#}");
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
}

/// The process id that the sleeper behind heartbeat file `beat` (a fake
/// server's descendant, see `--spawn-grandchild` in `fake_lsp.rs`) wrote when
/// it started.
#[cfg(windows)]
fn sleeper_pid(beat: &std::path::Path) -> u32 {
    std::fs::read_to_string(beat.with_extension("pid"))
        .expect("the sleeper writes its process id before its first heartbeat")
        .trim()
        .parse()
        .expect("a process id")
}

/// Whether the operating system lists process `pid` as running. Asks
/// `tasklist`, which prints one CSV row (`"image","pid",...`) per match and an
/// information line when there is none. The program runs on the runtime
/// (`tokio::process`) instead of blocking it, so the tasks that are stopping
/// the servers keep going while a test polls.
#[cfg(windows)]
async fn process_is_running(pid: u32) -> bool {
    let output = tokio::process::Command::new("tasklist")
        .arg("/FI")
        .arg(format!("PID eq {pid}"))
        .args(["/FO", "CSV", "/NH"])
        .output()
        .await
        .expect("tasklist runs");
    String::from_utf8_lossy(&output.stdout).contains(&format!("\"{pid}\""))
}

/// Whether the operating system shows process `pid` as running. `ps -o stat=`
/// prints its state, and nothing once the process is gone; one that has ended
/// but is not yet reaped shows `Z` (zombie) and no longer runs. The program
/// runs on the runtime (`tokio::process`), as the Windows variant does.
#[cfg(not(windows))]
async fn process_is_running(pid: u32) -> bool {
    let output = tokio::process::Command::new("ps")
        .args(["-o", "stat=", "-p"])
        .arg(pid.to_string())
        .output()
        .await
        .expect("ps runs");
    let state = String::from_utf8_lossy(&output.stdout);
    let state = state.trim();
    !state.is_empty() && !state.starts_with('Z')
}

/// Waits until process `pid` has ended, and fails with `what` after `deadline`.
async fn wait_until_process_ends(what: &str, pid: u32, deadline: Duration) {
    wait_until(what, deadline, async || !process_is_running(pid).await).await;
}

/// How long the operating system and `tasklist` get to show a process as ended
/// once the call that stopped it has returned.
#[cfg(windows)]
const PROCESS_END_SLACK: Duration = Duration::from_secs(5);

/// A language server is often not the process that does the work: a `.cmd`
/// shim, a `rustup` proxy or a `node.exe` behind `cmd.exe` start it, and
/// killing the direct child leaves the rest running. When a server is
/// discarded after a transport failure, the processes it started end with it,
/// not only the server itself. The first fake server (with its sleeper, the
/// descendant) dies mid-request but leaves the sleeper behind; the discard's
/// shutdown ends the sleeper, and the replacement's sleeper keeps running.
#[cfg(windows)]
#[tokio::test]
async fn discarding_a_session_ends_the_processes_its_server_started() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_timeout_seconds = 2;
    config.csharp.args.extend(fake_args(&[
        "--spawn-grandchild",
        "--exit-on-symbol-request",
        "2",
    ]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);
    let (discarded, serving) = (
        sleeper_pid(&state.join("sleeper_1.txt")),
        sleeper_pid(&state.join("sleeper_2.txt")),
    );

    wait_until_process_ends(
        "the process the discarded server started to end",
        discarded,
        PROCESS_END_SLACK,
    )
    .await;
    assert!(
        process_is_running(serving).await,
        "the replacement server's process must not be touched"
    );

    manager.release(&workspace.id).await;
    wait_until_process_ends(
        "the released server's process to end",
        serving,
        PROCESS_END_SLACK,
    )
    .await;
}

/// Releasing a session ends the processes its server started, whether the
/// server leaves on `exit` (they are still running then) or ignores `shutdown`
/// and `exit` and is killed.
#[cfg(windows)]
#[tokio::test]
async fn releasing_a_session_ends_the_processes_its_server_started() {
    for extra in [&[][..], &["--ignore-shutdown"][..]] {
        let temp = tempfile::tempdir().unwrap();
        let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
        config.csharp.args.push("--spawn-grandchild".to_owned());
        config.csharp.args.extend(fake_args(extra));
        let manager = Manager::new(&config);
        let adapter = csharp_adapter(&manager);
        manager
            .symbols(&adapter, &workspace, "Calculator")
            .await
            .unwrap();
        let sleeper = sleeper_pid(&state.join("sleeper_1.txt"));
        assert!(
            process_is_running(sleeper).await,
            "the server's process must run before the release ({extra:?})"
        );

        assert_eq!(manager.release(&workspace.id).await, vec!["csharp-ls"]);
        wait_until_process_ends(
            &format!("the process the released server started to end ({extra:?})"),
            sleeper,
            PROCESS_END_SLACK,
        )
        .await;
    }
}

/// Dropping the manager (a service that exits without releasing) ends the
/// processes the servers started too.
#[cfg(windows)]
#[tokio::test]
async fn dropping_the_manager_ends_the_processes_its_servers_started() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.csharp.args.push("--spawn-grandchild".to_owned());
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);
    manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    let sleeper = sleeper_pid(&state.join("sleeper_1.txt"));
    assert!(process_is_running(sleeper).await);

    drop(manager);
    wait_until_process_ends(
        "the process of a dropped manager's server to end",
        sleeper,
        PROCESS_END_SLACK,
    )
    .await;
}

/// How long a dropped manager's server gets to end. The kill is sent as the
/// manager drops; this bounds only how long the operating system takes to show
/// the process as gone, and the poll ends the wait as soon as it does.
const DROP_KILL_DEADLINE: Duration = Duration::from_secs(10);

/// Dropping the manager (a service that exits without releasing) ends the
/// server it started, on every platform. The server is the fake's direct
/// child, and its process id is the one it recorded on its first request.
/// Several layers end it and any one is enough (the child's `kill_on_drop`, the
/// client's own `Drop`, and on Windows the job's explicit terminate and its
/// close), so removing one of them turns nothing red; removing all of them
/// does. The processes a server starts in turn are the Windows test above.
#[tokio::test]
async fn dropping_the_manager_ends_the_server_it_started() {
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state) = fake_csharp_release_fixture(&temp);
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);
    manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    let server: u32 = std::fs::read_to_string(state.join("pid.txt"))
        .expect("the fake server records its process id on its first request")
        .trim()
        .parse()
        .expect("a process id");
    assert!(process_is_running(server).await, "the server is running");

    drop(manager);
    wait_until_process_ends(
        "the server of a dropped manager to end",
        server,
        DROP_KILL_DEADLINE,
    )
    .await;
}

/// Starts `symbols` on its own task, the way a request in flight holds the
/// session and client it was sent on.
#[cfg(windows)]
fn spawn_symbols_call(
    manager: &Arc<Manager>,
    adapter: &Arc<dyn LspAdapter>,
    workspace: &Workspace,
) -> tokio::task::JoinHandle<anyhow::Result<Vec<Location>>> {
    let (manager, adapter, workspace) =
        (Arc::clone(manager), Arc::clone(adapter), workspace.clone());
    tokio::spawn(async move { manager.symbols(&adapter, &workspace, "Calculator").await })
}

/// Ends a holder test: stops the stalled call, drops the manager (the last
/// holder of every client), and checks that no sleeper is left running.
#[cfg(windows)]
async fn finish_holder_test(
    call: tokio::task::JoinHandle<anyhow::Result<Vec<Location>>>,
    manager: Arc<Manager>,
    state: &std::path::Path,
) {
    call.abort();
    let _ = call.await;
    drop(manager);
    for ordinal in 1..=state_count(state, "spawn_count.txt") {
        if let Ok(text) = std::fs::read_to_string(state.join(format!("sleeper_{ordinal}.pid"))) {
            wait_until_process_ends(
                &format!("sleeper {ordinal} to end once the manager is dropped"),
                text.trim().parse().expect("a process id"),
                PROCESS_END_SLACK,
            )
            .await;
        }
    }
}

/// The processes a server started end with the shutdown itself, not only when
/// the last holder of its client lets go. A request in flight holds the client
/// (and so the job object that closes on its drop) for as long as it waits,
/// which is far longer than a shutdown takes; the unit under test is the
/// shutdown's kill of the whole tree, which `Child::kill` alone does not do.
///
/// The fake stalls for a minute on the request after the priming one: it reads
/// nothing, so it answers neither `shutdown` nor `exit` and is killed. The
/// call, with a 20 s inactivity timeout, holds the client through the whole
/// release and the wait after it. Without the shutdown's own tree kill the
/// sleeper lives until that call lets go (20 s or more), which is longer than
/// the wait below.
#[cfg(windows)]
#[tokio::test]
async fn a_released_server_loses_its_processes_while_a_request_still_holds_its_client() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_timeout_seconds = 20;
    config.csharp.args.extend(fake_args(&[
        "--spawn-grandchild",
        "--symbol-delay-ms",
        "60000",
    ]));
    let manager = Arc::new(Manager::new(&config));
    let adapter = csharp_adapter(&manager);
    let call = spawn_symbols_call(&manager, &adapter, &workspace);
    // initialize, initialized, the priming request, and the stalled request.
    wait_until(
        "the stalled request to reach the server",
        Duration::from_secs(15),
        async || state_count_so_far(&state, "request_count.txt") >= 4,
    )
    .await;
    let sleeper = sleeper_pid(&state.join("sleeper_1.txt"));
    assert!(
        process_is_running(sleeper).await,
        "the server's process must run before the release"
    );
    assert!(!call.is_finished());

    let started = std::time::Instant::now();
    assert_eq!(manager.release(&workspace.id).await, vec!["csharp-ls"]);
    let released_in = started.elapsed();
    // About 4.2 s alone (2 s for the answer it never gives, 2 s for the exit it
    // never makes), over 6 s once when the whole binary's tests ran together on
    // a loaded machine. The upper bound only has to show that the release does
    // not wait for the request that holds the client (20 s), so it leaves room.
    assert!(
        released_in >= local_code_intelligence::lsp::SHUTDOWN_GRACE
            && released_in
                < 2 * local_code_intelligence::lsp::SHUTDOWN_GRACE + Duration::from_secs(8),
        "the release waits out the grace period for a server that never answers, and does not \
         wait for the request that holds its client: {released_in:?}"
    );
    wait_until_process_ends(
        "the process the released server started to end while a request still holds its client",
        sleeper,
        PROCESS_END_SLACK,
    )
    .await;
    assert!(
        !call.is_finished(),
        "the request must still hold the client, or the job's drop could have ended the process"
    );

    finish_holder_test(call, manager, &state).await;
}

/// The same for a discard whose shutdown finds the server already gone (the
/// cooperative branch of the shutdown, which also ends the tree). The first
/// server dies on the request after the priming one, but its sleeper keeps the
/// server's pipes open, so the request fails by the 6 s inactivity timeout
/// rather than at once, and the session is discarded. The replacement stalls
/// on the repeated request for 30 s, so the call holds the discarded session's
/// client (a local of its retry) until its own 6 s timeout runs out, about
/// 4 s after the discard's shutdown (a 2 s wait for the answer it never gets)
/// has ended the tree.
#[cfg(windows)]
#[tokio::test]
async fn a_discarded_server_loses_its_processes_while_a_request_still_holds_its_client() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_timeout_seconds = 6;
    config.csharp.args.extend(fake_args(&[
        "--spawn-grandchild",
        "--exit-on-symbol-request",
        "2",
        "--symbol-delay-ms",
        "30000",
    ]));
    let manager = Arc::new(Manager::new(&config));
    let adapter = csharp_adapter(&manager);
    let call = spawn_symbols_call(&manager, &adapter, &workspace);
    wait_until(
        "the first server's sleeper to start",
        Duration::from_secs(15),
        async || state.join("sleeper_1.txt").exists(),
    )
    .await;
    let discarded = sleeper_pid(&state.join("sleeper_1.txt"));
    assert!(
        process_is_running(discarded).await,
        "the server's process must run before the discard"
    );

    // The first server's four messages, then the replacement's four, the last
    // one the repeated request it stalls on.
    wait_until(
        "the repeated request to reach the replacement server",
        Duration::from_secs(30),
        async || state_count_so_far(&state, "request_count.txt") >= 8,
    )
    .await;
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);
    wait_until_process_ends(
        "the process the discarded server started to end while a request still holds its client",
        discarded,
        Duration::from_secs(4),
    )
    .await;
    assert!(
        !call.is_finished(),
        "the request must still hold the client, or the job's drop could have ended the process"
    );
    assert!(
        process_is_running(sleeper_pid(&state.join("sleeper_2.txt"))).await,
        "the replacement server's process must not be touched"
    );

    finish_holder_test(call, manager, &state).await;
}

/// A C# fixture whose fake server takes `delay_ms` to answer `initialize`,
/// with a request timeout long enough that the delay is never a failure.
fn fake_slow_start_csharp_fixture(
    temp: &tempfile::TempDir,
    mode: &str,
    delay_ms: u64,
) -> (Config, Workspace, std::path::PathBuf) {
    let (mut config, workspace, state) = fake_csharp_fixture(temp, mode);
    config.lsp_timeout_seconds = 10;
    config
        .csharp
        .args
        .extend(["--initialize-delay-ms".to_owned(), delay_ms.to_string()]);
    (config, workspace, state)
}

#[tokio::test]
async fn concurrent_requests_share_one_language_server_start() {
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state) =
        fake_slow_start_csharp_fixture(&temp, "workspace-symbols", 500);
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let (first, second, third, fourth, fifth) = tokio::join!(
        manager.symbols(&adapter, &workspace, "Calculator"),
        manager.symbols(&adapter, &workspace, "Calculator"),
        manager.symbols(&adapter, &workspace, "Calculator"),
        manager.symbols(&adapter, &workspace, "Calculator"),
        manager.symbols(&adapter, &workspace, "Calculator"),
    );
    for results in [first, second, third, fourth, fifth] {
        assert_eq!(results.unwrap()[0].name.as_deref(), Some("Calculator"));
    }
    // `spawns` holds one entry per start, claimed atomically, so it counts
    // overlapping starts that `spawn_count.txt` (last writer wins) can hide.
    assert_eq!(
        std::fs::read_dir(state.join("spawns")).unwrap().count(),
        1,
        "five callers that arrived while the server was starting must share that one start"
    );
    let served: std::collections::BTreeSet<String> =
        std::fs::read_to_string(state.join("symbol_request_pids.txt"))
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect();
    assert_eq!(
        served.len(),
        1,
        "one server process must answer every symbol request: {served:?}"
    );
}

/// A caller that gives up while the server is still starting (a search past
/// its latency budget) must not take the start down with it: the next caller
/// finds the same server finishing, instead of a killed child and a second
/// spawn.
#[tokio::test]
async fn a_caller_that_stops_waiting_does_not_kill_the_starting_server() {
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state) =
        fake_slow_start_csharp_fixture(&temp, "workspace-symbols", 800);
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let abandoned = tokio::time::timeout(
        Duration::from_millis(150),
        manager.symbols(&adapter, &workspace, "Calculator"),
    )
    .await;
    assert!(
        abandoned.is_err(),
        "the first caller must give up while the server is still starting"
    );
    assert!(
        !manager.running("csharp-ls", &workspace.id).await,
        "a server that has not finished starting is not running yet"
    );

    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert!(manager.running("csharp-ls", &workspace.id).await);
    assert_eq!(
        state_count(&state, "spawn_count.txt"),
        1,
        "the abandoned start must finish, not be killed and respawned"
    );
}

/// Abandoning an in-flight request (rather than a start) leaves the session
/// and the protocol stream intact: the server's late answer to the dropped
/// request is ignored and the next request is answered normally on the same
/// process.
#[tokio::test]
async fn an_abandoned_request_leaves_the_session_usable() {
    let temp = tempfile::tempdir().unwrap();
    // Every `workspace/symbol` after the priming one takes a second.
    let (mut config, workspace, state) = fake_csharp_fixture(&temp, "workspace-symbols");
    config.lsp_timeout_seconds = 10;
    config
        .csharp
        .args
        .extend(["--symbol-delay-ms".to_owned(), "1000".to_owned()]);
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    // Stop waiting once the server has read the slow request, i.e. while it is
    // in flight: the fourth frame after initialize, initialized and the
    // priming request. `running` is true as soon as the server is up, before
    // priming settles, so on a slow machine a fixed wait after it can end
    // before the request has even been sent. The call is dropped at the end
    // of the block.
    let finished = {
        let call = manager.symbols(&adapter, &workspace, "Calculator");
        tokio::pin!(call);
        tokio::select! {
            result = &mut call => Some(result),
            () = wait_until(
                "the slow request to reach the server",
                Duration::from_secs(30),
                async || state_count_so_far(&state, "request_count.txt") >= 4,
            ) => None,
        }
    };
    assert!(finished.is_none(), "the slow request must outlast the wait");
    assert!(
        manager.running("csharp-ls", &workspace.id).await,
        "dropping a request must not discard the session"
    );

    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    // initialize, initialized, the priming request, the abandoned request,
    // its `$/cancelRequest`, and the retry.
    assert_eq!(state_count(&state, "request_count.txt"), 6);
}

/// Polls `done` every 50 ms until it returns `true`, and fails with `what`
/// when `deadline` passes first. Waiting on the observable change rather than
/// sleeping a fixed time keeps the idle and release tests from depending on
/// how fast the machine is.
async fn wait_until(what: &str, deadline: Duration, mut done: impl AsyncFnMut() -> bool) {
    let started = std::time::Instant::now();
    while !done().await {
        assert!(
            started.elapsed() < deadline,
            "timed out after {deadline:?} waiting for {what}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// How many lines the fake server has appended to `name` in its state
/// directory (one per process that saw the event); 0 when there is no file.
fn event_count(state: &std::path::Path, name: &str) -> usize {
    std::fs::read_to_string(state.join(name)).map_or(0, |text| text.lines().count())
}

/// A second workspace next to the fixture's, in the same data directory. The
/// fixture's one configured fake server (and so its state directory) serves
/// both, which makes `spawn_count.txt` the number of sessions ever started.
fn second_workspace(temp: &tempfile::TempDir, config: &Config) -> Workspace {
    let path = temp.path().join("workspace-two");
    std::fs::create_dir_all(path.join("src")).unwrap();
    Workspace::resolve(&path, &config.data_dir).unwrap()
}

/// A C# fixture for the idle and release tests. They never depend on a 1 s
/// request timeout, and the fixture's 1 s would make a slow process start
/// under load fail their `initialize` (so would 5 s: a timed-out start is a
/// transport failure, and its retry spawns a server the test did not count),
/// so this one allows [`LOADED_BUDGET_SECONDS`]. A test about the timeout sets
/// its own.
fn fake_csharp_release_fixture(
    temp: &tempfile::TempDir,
) -> (Config, Workspace, std::path::PathBuf) {
    let (mut config, workspace, state) = fake_csharp_fixture(temp, "workspace-symbols");
    config.lsp_timeout_seconds = LOADED_BUDGET_SECONDS;
    (config, workspace, state)
}

#[cfg(unix)]
struct SigtermServe {
    child: std::process::Child,
    language_server_pid_file: std::path::PathBuf,
}

#[cfg(unix)]
impl Drop for SigtermServe {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Ok(pid) = std::fs::read_to_string(&self.language_server_pid_file) {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", pid.trim()])
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status();
        }
    }
}

#[cfg(unix)]
async fn signal_shuts_down_the_running_language_server(signal_name: &str, expected_exit_code: i32) {
    let (temp, mut config, _models, models_task) = fixture().await;
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/Calculator.cs"), "class Calculator {}\n").unwrap();

    let [port] = free_ports();
    config.port = port;
    config.csharp = CSharpLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: vec![
            "--mode".to_owned(),
            "workspace-symbols".to_owned(),
            "--state".to_owned(),
            temp.path().join("lsp-state").to_string_lossy().into_owned(),
            "--keep-alive-on-eof".to_owned(),
        ],
        disabled: false,
    };
    config.lsp_timeout_seconds = 5;
    let state = temp.path().join("lsp-state");
    std::fs::write(
        temp.path().join("serve.toml"),
        toml::to_string(&config).unwrap(),
    )
    .unwrap();

    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
    command
        .arg("--config")
        .arg(temp.path().join("serve.toml"))
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    strip_lci_env(&mut command);
    let child = command.spawn().unwrap();
    let mut serve = SigtermServe {
        child,
        language_server_pid_file: state.join("pid.txt"),
    };

    let port = config.port;
    let client = reqwest::Client::new();
    let base = format!("http://127.0.0.1:{port}");
    let mut healthy = false;
    for _ in 0..200 {
        if serve.child.try_wait().unwrap().is_some() {
            break;
        }
        if let Ok(response) = client.get(format!("{base}/health")).send().await
            && response.status().is_success()
        {
            healthy = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(healthy, "serve did not become healthy");

    let indexed = client
        .post(format!("{base}/v1/index_workspace"))
        .json(&serde_json::json!({"workspace_path": workspace}))
        .send()
        .await
        .unwrap();
    assert!(
        indexed.status().is_success(),
        "indexing failed: {}",
        indexed.text().await.unwrap()
    );
    let response = client
        .post(format!("{base}/v1/search_symbols"))
        .json(&serde_json::json!({
            "workspace_path": workspace,
            "query": "Calculator"
        }))
        .send()
        .await
        .unwrap();
    assert!(
        response.status().is_success(),
        "navigation failed: {}",
        response.text().await.unwrap()
    );
    let pid: u32 = std::fs::read_to_string(&serve.language_server_pid_file)
        .expect("fake language server wrote its pid")
        .trim()
        .parse()
        .unwrap();
    assert!(process_is_running(pid).await);

    let signal = tokio::process::Command::new("kill")
        .args([&format!("-{signal_name}"), &serve.child.id().to_string()])
        .status()
        .await
        .unwrap();
    assert!(signal.success(), "could not send {signal_name} to serve");
    let status = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = serve.child.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("serve did not exit within 15 s after SIGTERM"));

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while process_is_running(pid).await && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let language_server_exited = !process_is_running(pid).await;
    drop(models_task);
    assert_eq!(
        status.code(),
        Some(expected_exit_code),
        "serve exit status after {signal_name}: {status}"
    );
    assert!(
        language_server_exited,
        "language server process {pid} survived {signal_name}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_shuts_down_the_running_language_server() {
    signal_shuts_down_the_running_language_server("TERM", 143).await;
}

#[cfg(unix)]
#[tokio::test]
async fn sigint_shuts_down_the_running_language_server() {
    signal_shuts_down_the_running_language_server("INT", 130).await;
}

/// A command-line command that starts a language server, stopped by SIGTERM
/// while the server is still answering `initialize`: the command must exit
/// and take the server with it. The fake server keeps running at stdin EOF
/// and answers `initialize` only after a minute, so it outlives a client that
/// the signal ends at once. `args` gets the workspace and the temporary
/// directory and returns the command's arguments.
#[cfg(unix)]
async fn sigterm_during_a_command_ends_its_language_server(
    signal_name: &str,
    expected_exit_code: i32,
    args: impl FnOnce(&std::path::Path, &std::path::Path) -> Vec<String>,
) {
    let (temp, mut config, _models, models_task) = fixture().await;
    let workspace = temp.path().join("workspace");
    write_csharp_workspace(&workspace);
    let state = temp.path().join("lsp-state");
    config.csharp = CSharpLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: fake_args(&[
            "--mode",
            "workspace-symbols",
            "--state",
            &state.to_string_lossy(),
            "--keep-alive-on-eof",
            "--initialize-delay-ms",
            "60000",
        ]),
        disabled: false,
    };
    config.lsp_timeout_seconds = 120;
    // `search` and `evaluate` would otherwise stop waiting for the server
    // after the default 1.5 s and end on their own.
    config.lsp_search_budget_ms = 60_000;
    let config_path = temp.path().join("cli.toml");
    std::fs::write(&config_path, toml::to_string(&config).unwrap()).unwrap();
    let cli = |args: &[String]| {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_local-code-intelligence"));
        command
            .arg("--config")
            .arg(&config_path)
            .args(args)
            .stdin(std::process::Stdio::null());
        strip_lci_env(&mut command);
        command
    };

    // `symbols`, `definition` and `references` need an index. The command
    // runs off the runtime, which serves the fake model service.
    let mut index = cli(&["index".to_owned(), workspace.display().to_string()]);
    let indexed = tokio::task::spawn_blocking(move || index.output().unwrap())
        .await
        .unwrap();
    assert!(
        indexed.status.success(),
        "{}",
        String::from_utf8_lossy(&indexed.stderr)
    );
    assert!(
        !state.join("pid.txt").exists(),
        "indexing must not start the language server"
    );

    let log = temp.path().join("cli.log");
    let child = cli(&args(&workspace, temp.path()))
        .stdout(std::process::Stdio::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .spawn()
        .unwrap();
    let mut command = SigtermServe {
        child,
        language_server_pid_file: state.join("pid.txt"),
    };
    let started = std::time::Instant::now();
    let pid = loop {
        if let Some(pid) = std::fs::read_to_string(&command.language_server_pid_file)
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
        {
            break pid;
        }
        assert!(
            command.child.try_wait().unwrap().is_none(),
            "the command ended before starting its language server: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the language server never received initialize"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(process_is_running(pid).await);

    let signal = tokio::process::Command::new("kill")
        .args([&format!("-{signal_name}"), &command.child.id().to_string()])
        .status()
        .await
        .unwrap();
    assert!(
        signal.success(),
        "could not send {signal_name} to the command"
    );
    let exited = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = command.child.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while process_is_running(pid).await && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let language_server_exited = !process_is_running(pid).await;
    drop(models_task);
    let log = std::fs::read_to_string(&log).unwrap_or_default();
    let status =
        exited.unwrap_or_else(|_| panic!("the command did not exit after {signal_name}: {log}"));
    assert!(
        language_server_exited,
        "language server process {pid} survived SIGTERM ({status}): {log}"
    );
    assert!(
        log.contains("stopped by a shutdown signal"),
        "the command must say why it stopped ({status}): {log}"
    );
    assert_eq!(
        status.code(),
        Some(expected_exit_code),
        "the command's exit status after {signal_name}: {status}; {log}"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_during_search_ends_its_language_server() {
    sigterm_during_a_command_ends_its_language_server("TERM", 143, |workspace, _| {
        vec![
            "search".to_owned(),
            workspace.display().to_string(),
            "Calculator".to_owned(),
        ]
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn sigint_during_search_ends_its_language_server() {
    sigterm_during_a_command_ends_its_language_server("INT", 130, |workspace, _| {
        vec![
            "search".to_owned(),
            workspace.display().to_string(),
            "Calculator".to_owned(),
        ]
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_during_evaluate_ends_its_language_server() {
    sigterm_during_a_command_ends_its_language_server("TERM", 143, |workspace, temp| {
        let definition = temp.join("definition.toml");
        std::fs::write(
            &definition,
            "[[workspaces]]\nkey = \"ws\"\nrequired = true\n\n[[queries]]\nworkspace = \"ws\"\nquery = \"Calculator\"\ntop_k = 3\nexpected_paths = [\"src/Calculator.cs\"]\n",
        )
        .unwrap();
        vec![
            "evaluate".to_owned(),
            definition.display().to_string(),
            "--workspace".to_owned(),
            format!("ws={}", workspace.display()),
        ]
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_during_symbols_ends_its_language_server() {
    sigterm_during_a_command_ends_its_language_server("TERM", 143, |workspace, _| {
        vec![
            "symbols".to_owned(),
            workspace.display().to_string(),
            "Calculator".to_owned(),
        ]
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_during_definition_ends_its_language_server() {
    sigterm_during_a_command_ends_its_language_server("TERM", 143, |workspace, _| {
        vec![
            "definition".to_owned(),
            workspace.display().to_string(),
            "src/CallSite.cs".to_owned(),
            "1".to_owned(),
            "60".to_owned(),
        ]
    })
    .await;
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_during_references_ends_its_language_server() {
    sigterm_during_a_command_ends_its_language_server("TERM", 143, |workspace, _| {
        vec![
            "references".to_owned(),
            workspace.display().to_string(),
            "src/Calculator.cs".to_owned(),
            "1".to_owned(),
            "20".to_owned(),
        ]
    })
    .await;
}

/// Checks liveness only while the answer is pending: after it returns, an idle
/// release is legitimate because the request has outlasted the idle timeout.
async fn answer_without_idle_release(
    manager: &Manager,
    workspace: &Workspace,
    state: &std::path::Path,
    answer: impl std::future::Future<Output = anyhow::Result<Vec<Location>>>,
) -> Vec<Location> {
    tokio::pin!(answer);
    let mut replacement_running = false;
    loop {
        tokio::select! {
            result = &mut answer => {
                assert!(replacement_running, "the replacement must be observed while answering");
                return result.expect("the replacement must finish the retried request");
            }
            () = tokio::time::sleep(Duration::from_millis(50)) => {
                if state_count_so_far(state, "spawn_count.txt") == 2 {
                    let running = manager.running("csharp-ls", &workspace.id).await;
                    assert!(
                        !replacement_running || running,
                        "the idle sweep must not release the replacement during its answer"
                    );
                    replacement_running |= running;
                    assert_eq!(event_count(state, "shutdown_seen.txt"), 0);
                    assert_eq!(event_count(state, "exit_seen.txt"), 0);
                }
            }
        }
    }
}

/// Both retry paths must keep the replacement session in use while its answer
/// outlasts the idle timeout. Priming answers immediately; only the real request
/// is slow, so priming cannot mask a missing session handle.
#[tokio::test]
async fn a_retried_symbol_request_is_not_cut_off_by_the_idle_sweep() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_idle_timeout_seconds = 1;
    config.csharp.args.extend(fake_args(&[
        "--exit-on-symbol-request",
        "2",
        "--symbol-delay-ms",
        "3000",
    ]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let started = std::time::Instant::now();
    let results = answer_without_idle_release(
        &manager,
        &workspace,
        &state,
        manager.symbols(&adapter, &workspace, "Calculator"),
    )
    .await;
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "the retried answer must outlast the idle timeout"
    );
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert_eq!(state_count(&state, "spawn_count.txt"), 2, "one restart");
    manager.shutdown_all().await;
}

#[tokio::test]
async fn a_retried_position_request_is_not_cut_off_by_the_idle_sweep() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_fixture(&temp, "definition");
    config.lsp_timeout_seconds = 5;
    config.lsp_idle_timeout_seconds = 1;
    config.csharp.args.extend(fake_args(&[
        "--exit-on-definition-request",
        "--definition-delay-ms",
        "3000",
    ]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let started = std::time::Instant::now();
    let results = answer_without_idle_release(
        &manager,
        &workspace,
        &state,
        manager.definition(
            &adapter,
            &workspace,
            &workspace.path.join("src/CallSite.cs"),
            "src/CallSite.cs",
            1,
            82,
        ),
    )
    .await;
    assert!(
        started.elapsed() >= Duration::from_secs(3),
        "the retried answer must outlast the idle timeout"
    );
    assert_eq!(
        results[0].relative_file_path.as_deref(),
        Some("src/Calculator.cs")
    );
    assert_eq!(state_count(&state, "spawn_count.txt"), 2, "one restart");
    manager.shutdown_all().await;
}

/// The idle timeout (2 s here, checked about every second) shuts an unused
/// server down through the protocol -- `shutdown`, then `exit` -- and the next
/// call starts a fresh process instead of failing.
#[tokio::test]
async fn an_idle_session_is_shut_down_and_restarted_on_the_next_call() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_idle_timeout_seconds = 2;
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert!(manager.running("csharp-ls", &workspace.id).await);
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    let first_process = std::fs::read_to_string(state.join("pid.txt")).unwrap();

    wait_until(
        "the idle server to be released",
        Duration::from_secs(30),
        async || {
            !manager.running("csharp-ls", &workspace.id).await
                && event_count(&state, "exit_seen.txt") == 1
        },
    )
    .await;
    assert_eq!(
        event_count(&state, "shutdown_seen.txt"),
        1,
        "the server must be asked to shut down before it is told to exit"
    );
    assert_eq!(
        state_count(&state, "spawn_count.txt"),
        1,
        "releasing an idle session must not start a replacement"
    );

    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert!(manager.running("csharp-ls", &workspace.id).await);
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);
    assert_ne!(
        std::fs::read_to_string(state.join("pid.txt")).unwrap(),
        first_process,
        "the next call must be served by a new process"
    );
}

/// `lsp_idle_timeout_seconds = 0` means never: the same wait that ends the
/// session in the test above leaves it running here.
#[tokio::test]
async fn idle_timeout_zero_never_releases() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_idle_timeout_seconds = 0;
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    // A 2 s timeout is released within 3 s of its last use (see above).
    tokio::time::sleep(Duration::from_secs(4)).await;
    assert!(manager.running("csharp-ls", &workspace.id).await);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);

    manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
}

/// An explicit release stops the servers of one workspace, reports them, and
/// leaves every other workspace's server serving.
#[tokio::test]
async fn release_shuts_down_only_that_workspace() {
    let temp = tempfile::tempdir().unwrap();
    let (config, first, state) = fake_csharp_release_fixture(&temp);
    let second = second_workspace(&temp, &config);
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);
    for workspace in [&first, &second] {
        manager
            .symbols(&adapter, workspace, "Calculator")
            .await
            .unwrap();
    }
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);

    assert_eq!(manager.release(&first.id).await, vec!["csharp-ls"]);
    assert!(!manager.running("csharp-ls", &first.id).await);
    assert!(manager.running("csharp-ls", &second.id).await);
    // `release` returns once the process is gone, so the whole exchange has
    // happened: one server was asked to shut down and told to exit.
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 1);
    assert_eq!(event_count(&state, "exit_seen.txt"), 1);

    assert!(
        manager.release(&first.id).await.is_empty(),
        "a workspace with no session has nothing to release"
    );
    assert!(manager.release("no-such-workspace").await.is_empty());

    // The other workspace is still served by its original process; the
    // released one starts again.
    manager
        .symbols(&adapter, &second, "Calculator")
        .await
        .unwrap();
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);
    manager
        .symbols(&adapter, &first, "Calculator")
        .await
        .unwrap();
    assert_eq!(state_count(&state, "spawn_count.txt"), 3);
    assert!(manager.running("csharp-ls", &first.id).await);
}

/// A server that answers nothing to `shutdown` and survives `exit` must not
/// hang the release: it is killed once the grace period has passed.
#[tokio::test]
async fn a_server_that_ignores_shutdown_is_killed_within_the_grace_period() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.csharp.args.push("--ignore-shutdown".to_owned());
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);
    manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    let server: u32 = std::fs::read_to_string(state.join("pid.txt"))
        .expect("the fake server records its process id on its first request")
        .trim()
        .parse()
        .expect("a process id");
    assert!(process_is_running(server).await, "the server is running");

    let started = std::time::Instant::now();
    let released = manager.release(&workspace.id).await;
    let elapsed = started.elapsed();
    assert_eq!(released, vec!["csharp-ls"]);
    assert!(
        elapsed >= local_code_intelligence::lsp::SHUTDOWN_GRACE,
        "the release must have waited out the grace period for an exit that never came: {elapsed:?}"
    );
    assert!(
        elapsed < Duration::from_secs(6),
        "the release must be bounded by the grace period, took {elapsed:?}"
    );
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 1);
    assert_eq!(event_count(&state, "exit_seen.txt"), 1);

    // Killed: the process is gone. The kill is already done when `release`
    // returns; the polling only absorbs the operating system's delay in
    // ending the process.
    wait_until_process_ends(
        "the server that ignored shutdown to end after the release",
        server,
        DROP_KILL_DEADLINE,
    )
    .await;
}

/// Cancelling release must not cancel shutdown, even while an in-flight
/// request holds another `Arc` to the removed session and its client.
#[tokio::test]
async fn cancelling_release_still_stops_a_server_with_an_in_flight_request() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.csharp.args.push("--ignore-shutdown".to_owned());
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);
    manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    let server: u32 = std::fs::read_to_string(state.join("pid.txt"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(process_is_running(server).await);

    // This scenario answers symbols but not definitions. Keep the pending
    // future alive, without polling it again, until the process has ended.
    let file = workspace.path.join("src/Calculator.cs");
    let mut request =
        Box::pin(manager.definition(&adapter, &workspace, &file, "src/Calculator.cs", 2, 4));
    tokio::select! {
        result = &mut request => panic!("the definition must remain pending: {result:?}"),
        () = wait_until(
            "the definition request to reach the server",
            Duration::from_secs(30),
            async || state_count_so_far(&state, "request_count.txt") >= 5,
        ) => {}
    }

    {
        let release = manager.release(&workspace.id);
        tokio::pin!(release);
        tokio::select! {
            result = &mut release => panic!("shutdown must still be waiting: {result:?}"),
            () = async {
                wait_until(
                    "the shutdown request to reach the server",
                    Duration::from_secs(30),
                    async || event_count(&state, "shutdown_seen.txt") == 1,
                ).await;
                tokio::time::sleep(Duration::from_millis(100)).await;
            } => {}
        }
    }
    assert!(!manager.running("csharp-ls", &workspace.id).await);
    wait_until_process_ends(
        "the server to end after release was cancelled with a request still holding the session",
        server,
        // The grace window plus the same slack the other process-end waits
        // give a loaded machine to show the process as ended.
        2 * local_code_intelligence::lsp::SHUTDOWN_GRACE + Duration::from_secs(5),
    )
    .await;
    assert_eq!(event_count(&state, "exit_seen.txt"), 1);
    drop(request);
}

/// Shutting the manager down (service exit) releases every workspace's
/// server, not only one.
#[tokio::test]
async fn shutdown_all_releases_every_session() {
    let temp = tempfile::tempdir().unwrap();
    let (config, first, state) = fake_csharp_release_fixture(&temp);
    let second = second_workspace(&temp, &config);
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);
    for workspace in [&first, &second] {
        manager
            .symbols(&adapter, workspace, "Calculator")
            .await
            .unwrap();
    }
    assert!(manager.running("csharp-ls", &first.id).await);
    assert!(manager.running("csharp-ls", &second.id).await);

    assert_eq!(manager.shutdown_all().await, 2);
    assert!(!manager.running("csharp-ls", &first.id).await);
    assert!(!manager.running("csharp-ls", &second.id).await);
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 2);
    assert_eq!(event_count(&state, "exit_seen.txt"), 2);
    assert_eq!(manager.shutdown_all().await, 0);

    // The manager is still usable: the next call starts a new server.
    manager
        .symbols(&adapter, &first, "Calculator")
        .await
        .unwrap();
    assert_eq!(state_count(&state, "spawn_count.txt"), 3);
}

/// `Status.analyzers_running` has one entry per registered language server,
/// `true` only for a started, ready session of that workspace, and the seven
/// older per-server booleans keep reporting the same facts.
#[tokio::test]
async fn analyzers_running_has_an_entry_per_language_server_and_follows_the_session() {
    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    write_csharp_workspace(&workspace);
    let other = temp.path().join("other-workspace");
    std::fs::create_dir_all(&other).unwrap();
    let app = App::open(with_fake_csharp_server(
        config,
        &state,
        &[],
        LOADED_BUDGET_SECONDS,
    ))
    .await
    .unwrap();
    let providers = [
        "clangd",
        "csharp-ls",
        "gopls",
        "jdtls",
        "pyright",
        "rust-analyzer",
        "typescript-language-server",
    ];
    let running_providers = |status: &local_code_intelligence::app::Status| {
        status
            .analyzers_running
            .iter()
            .filter(|(_, running)| **running)
            .map(|(provider, _)| provider.clone())
            .collect::<Vec<_>>()
    };

    // Before anything has started (the workspace is not even indexed) every
    // server is listed, none running.
    let before = app.status(&workspace).await.unwrap();
    assert_eq!(
        before.analyzers_running.keys().collect::<Vec<_>>(),
        providers
    );
    assert!(running_providers(&before).is_empty());

    app.index(&workspace).await.unwrap();
    app.symbols(&workspace, "Calculator").await.unwrap();
    let during = app.status(&workspace).await.unwrap();
    assert_eq!(
        during.analyzers_running.keys().collect::<Vec<_>>(),
        providers
    );
    assert_eq!(running_providers(&during), ["csharp-ls"]);
    // The older booleans: the one for C# is true, the one for rust-analyzer
    // (`analyzer_running`, the fixture has none) and the rest are false.
    assert!(during.csharp_analyzer_running);
    assert!(!during.analyzer_running);
    assert!(!during.typescript_analyzer_running);
    assert!(!during.python_analyzer_running);
    assert!(!during.go_analyzer_running);
    assert!(!during.java_analyzer_running);
    assert!(!during.clangd_analyzer_running);
    // The wire form carries both.
    let wire = serde_json::to_value(&during).unwrap();
    assert_eq!(wire["analyzers_running"]["csharp-ls"], true, "{wire}");
    assert_eq!(wire["analyzers_running"]["rust-analyzer"], false, "{wire}");
    assert_eq!(wire["csharp_analyzer_running"], true, "{wire}");
    assert_eq!(wire["analyzer_running"], false, "{wire}");

    // The map is per workspace.
    let elsewhere = app.status(&other).await.unwrap();
    assert!(running_providers(&elsewhere).is_empty());
    assert!(!elsewhere.csharp_analyzer_running);

    // Releasing the workspace turns the entry off, not out.
    let report = app.release_language_servers(&workspace).await.unwrap();
    assert_eq!(report.released, ["csharp-ls"]);
    assert_eq!(report.workspace.id, during.workspace.id);
    let after = app.status(&workspace).await.unwrap();
    assert_eq!(
        after.analyzers_running.keys().collect::<Vec<_>>(),
        providers
    );
    assert!(running_providers(&after).is_empty());
    assert!(!after.csharp_analyzer_running);
    assert!(
        app.release_language_servers(&workspace)
            .await
            .unwrap()
            .released
            .is_empty()
    );
    models_task.abort();
}

/// A workspace that mixes C# and TypeScript, so `App::symbols` asks both
/// csharp-ls and typescript-language-server, each replaced by the fake server
/// in the given mode (see `fake_lsp.rs`). Each fake records what it saw under
/// its own directory of `state_dir`. No caller depends on the request timeout,
/// so it is [`LOADED_BUDGET_SECONDS`]: 5 s failed a slow `initialize` under
/// load.
fn with_fake_csharp_and_typescript_servers(
    mut config: Config,
    state_dir: &std::path::Path,
    csharp_mode: &str,
    typescript_mode: &str,
) -> Config {
    config.lsp_timeout_seconds = LOADED_BUDGET_SECONDS;
    let args = |mode: &str, name: &str| {
        vec![
            "--mode".to_owned(),
            mode.to_owned(),
            "--state".to_owned(),
            state_dir.join(name).to_string_lossy().into_owned(),
            "--primary-file".to_owned(),
            "src/Calculator.ts".to_owned(),
        ]
    };
    config.csharp = CSharpLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: args(csharp_mode, "csharp"),
        disabled: false,
    };
    config.typescript = TypeScriptLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: args(typescript_mode, "typescript"),
        disabled: false,
    };
    config
}

fn write_csharp_and_typescript_workspace(root: &std::path::Path) {
    write_csharp_workspace(root);
    write(
        root,
        "src/Calculator.ts",
        "export class Calculator {\n  add(a: number, b: number): number {\n    return a + b;\n  }\n}\n",
    );
}

/// In a mixed-language workspace one server failing must not turn another
/// server's correct empty answer into an error: csharp-ls exits (as it does
/// for a stray C# file in a Rust repository) while the TypeScript server
/// answers that nothing matches. The query is a correct "no match", so the
/// call succeeds with no results. Both servers really ran: the C# one was
/// started and failed, the TypeScript one was asked once. The answer carries
/// one warning for the failed server, so it is not mistaken for a clean miss.
#[tokio::test]
async fn symbols_return_an_empty_answer_when_another_applicable_server_failed() {
    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    write_csharp_and_typescript_workspace(&workspace);
    let app = App::open(with_fake_csharp_and_typescript_servers(
        config,
        &state,
        "exit-during-request",
        "no-symbols",
    ))
    .await
    .unwrap();
    app.index(&workspace).await.unwrap();

    let report = app
        .symbols(&workspace, "NoSuchSymbolAnywhere")
        .await
        .expect("one server answered, so the call must not fail");
    assert!(report.results.is_empty(), "{:?}", report.results);
    // An exiting server is started once and retried once with a fresh session.
    assert_eq!(
        state_count(&state.join("csharp"), "spawn_count.txt"),
        2,
        "csharp-ls must have been started and failed, not skipped"
    );
    assert_eq!(state_count(&state.join("typescript"), "spawn_count.txt"), 1);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(
        report.warnings[0].starts_with("Optional LSP provider unavailable: csharp-ls: "),
        "{:?}",
        report.warnings
    );
    assert!(
        !report.warnings[0].contains("typescript-language-server"),
        "{:?}",
        report.warnings
    );
    let wire = serde_json::to_value(&report).unwrap();
    assert_eq!(wire["warnings"].as_array().map(Vec::len), Some(1), "{wire}");
    models_task.abort();
}

/// An indexed language whose only server is switched off in the configuration
/// is not searched, and the answer says so: empty results with a warning that
/// names the language and the provider, and the server is never started. A
/// clean "no match" has no warning.
#[tokio::test]
async fn symbols_name_an_indexed_language_whose_only_server_is_disabled() {
    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    write_csharp_workspace(&workspace);
    let mut config =
        with_fake_csharp_and_typescript_servers(config, &state, "workspace-symbols", "no-symbols");
    config.csharp.disabled = true;
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app.symbols(&workspace, "Calculator").await.unwrap();
    assert!(report.results.is_empty(), "{:?}", report.results);
    assert_eq!(
        report.warnings,
        ["No language server is enabled for csharp (csharp-ls): its symbols were not searched"]
    );
    assert!(
        !state.join("csharp").join("spawn_count.txt").exists(),
        "a disabled server must not be started"
    );
    let wire = serde_json::to_value(&report).unwrap();
    assert_eq!(wire["warnings"].as_array().map(Vec::len), Some(1), "{wire}");
    models_task.abort();
}

/// The same in a mixed workspace: the language with a server that is switched
/// off is named, and the other language is still searched.
#[tokio::test]
async fn symbols_still_search_the_enabled_language_when_another_one_is_disabled() {
    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    write_csharp_and_typescript_workspace(&workspace);
    let mut config =
        with_fake_csharp_and_typescript_servers(config, &state, "workspace-symbols", "no-symbols");
    config.csharp.disabled = true;
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app.symbols(&workspace, "NoSuchSymbol").await.unwrap();
    assert!(report.results.is_empty(), "{:?}", report.results);
    assert_eq!(
        report.warnings,
        ["No language server is enabled for csharp (csharp-ls): its symbols were not searched"]
    );
    assert_eq!(state_count(&state.join("typescript"), "spawn_count.txt"), 1);
    assert!(!state.join("csharp").join("spawn_count.txt").exists());
    models_task.abort();
}

/// The warning does not depend on how the server failed: csharp-ls answers
/// with a JSON-RPC error response to every request (`--mode error-response`,
/// the process keeps running and no timeout is involved) while the TypeScript
/// server answers that nothing matches. The call still succeeds with no
/// results and one warning for csharp-ls.
#[tokio::test]
async fn symbols_warn_when_another_applicable_server_answered_with_an_error() {
    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    write_csharp_and_typescript_workspace(&workspace);
    let app = App::open(with_fake_csharp_and_typescript_servers(
        config,
        &state,
        "error-response",
        "no-symbols",
    ))
    .await
    .unwrap();
    app.index(&workspace).await.unwrap();

    let report = app
        .symbols(&workspace, "NoSuchSymbolAnywhere")
        .await
        .expect("the TypeScript server answered, so the call must not fail");
    assert!(report.results.is_empty(), "{:?}", report.results);
    // An error response is an answer, not a dead server (N9): csharp-ls is
    // started once, its session is kept, and the warning carries its error.
    assert_eq!(state_count(&state.join("csharp"), "spawn_count.txt"), 1);
    assert_eq!(state_count(&state.join("typescript"), "spawn_count.txt"), 1);
    assert_eq!(report.warnings.len(), 1, "{:?}", report.warnings);
    assert!(
        report.warnings[0].starts_with("Optional LSP provider unavailable: csharp-ls: "),
        "{:?}",
        report.warnings
    );
    assert!(
        report.warnings[0].contains("fake internal error"),
        "{:?}",
        report.warnings
    );
    let wire = serde_json::to_value(&report).unwrap();
    assert_eq!(wire["warnings"].as_array().map(Vec::len), Some(1), "{wire}");
    models_task.abort();
}

/// Each failed server gets its own warning, in provider order, however many
/// servers answered: csharp-ls and pyright exit while the TypeScript server
/// answers with nothing, so the two warnings name `csharp-ls` then `pyright`.
#[tokio::test]
async fn symbols_warn_once_per_failed_server_in_provider_order() {
    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    write_csharp_and_typescript_workspace(&workspace);
    write(
        &workspace,
        "src/Calculator.py",
        "class Calculator:\n    def add(self, a, b):\n        return a + b\n",
    );
    let mut config = with_fake_csharp_and_typescript_servers(
        config,
        &state,
        "exit-during-request",
        "no-symbols",
    );
    config.python = PythonLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: vec![
            "--mode".to_owned(),
            "exit-during-request".to_owned(),
            "--state".to_owned(),
            state.join("python").to_string_lossy().into_owned(),
            "--primary-file".to_owned(),
            "src/Calculator.py".to_owned(),
        ],
        disabled: false,
    };
    let app = App::open(config).await.unwrap();
    app.index(&workspace).await.unwrap();

    let report = app
        .symbols(&workspace, "NoSuchSymbolAnywhere")
        .await
        .expect("the TypeScript server answered, so the call must not fail");
    assert!(report.results.is_empty(), "{:?}", report.results);
    assert_eq!(report.warnings.len(), 2, "{:?}", report.warnings);
    assert!(
        report.warnings[0].starts_with("Optional LSP provider unavailable: csharp-ls: "),
        "{:?}",
        report.warnings
    );
    assert!(
        report.warnings[1].starts_with("Optional LSP provider unavailable: pyright: "),
        "{:?}",
        report.warnings
    );
    models_task.abort();
}

/// A navigation answer that no server failed to give has no `warnings` key at
/// all, so the response of `search_symbols`, `find_definition`, and
/// `find_references` is what it was before the field existed.
#[tokio::test]
async fn navigation_answers_without_a_server_failure_have_no_warnings_key() {
    for (mode, operation) in [
        ("workspace-symbols", "symbols"),
        ("definition", "definition"),
        ("references", "references"),
    ] {
        let (temp, mut config, _fake, models_task) = fixture().await;
        let workspace = temp.path().join("workspace");
        write_csharp_workspace(&workspace);
        config.lsp_timeout_seconds = 5;
        config.csharp = CSharpLspConfig {
            path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
            args: vec![
                "--mode".to_owned(),
                mode.to_owned(),
                "--state".to_owned(),
                temp.path().join("state").to_string_lossy().into_owned(),
            ],
            disabled: false,
        };
        let app = App::open(config).await.unwrap();
        app.index(&workspace).await.unwrap();

        let report = match operation {
            "symbols" => app.symbols(&workspace, "Calculator").await.unwrap(),
            "definition" => app
                .definition(&workspace, "src/CallSite.cs", 2, 72)
                .await
                .unwrap(),
            "references" => app
                .references(&workspace, "src/Calculator.cs", 2, 49, true)
                .await
                .unwrap(),
            _ => unreachable!(),
        };
        assert!(!report.results.is_empty(), "{operation}: no results");
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let wire = serde_json::to_value(&report).unwrap();
        assert!(
            wire.get("warnings").is_none(),
            "{operation}: unexpected warnings key in {wire}"
        );
        assert!(wire.get("results").is_some(), "{operation}: {wire}");
        models_task.abort();
    }
}

/// When every applicable server fails there is no answer to return: the call
/// is an error that names each failed provider.
#[tokio::test]
async fn symbols_fail_when_every_applicable_server_failed() {
    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    write_csharp_and_typescript_workspace(&workspace);
    let app = App::open(with_fake_csharp_and_typescript_servers(
        config,
        &state,
        "exit-during-request",
        "exit-during-request",
    ))
    .await
    .unwrap();
    app.index(&workspace).await.unwrap();

    let error = app
        .symbols(&workspace, "Calculator")
        .await
        .expect_err("no server answered, so the call must fail");
    let message = format!("{error:#}");
    assert!(
        message.contains("all applicable language servers failed"),
        "{message}"
    );
    assert!(message.contains("csharp-ls:"), "{message}");
    assert!(message.contains("typescript-language-server:"), "{message}");
    models_task.abort();
}

/// Service exit stops every language server within the bound the grace period
/// gives it, even a server that answers neither `shutdown` nor `exit`, which
/// is then killed.
#[tokio::test]
async fn shutting_the_service_down_finishes_within_the_shutdown_bound_for_a_server_that_ignores_it()
{
    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    write_csharp_workspace(&workspace);
    let app = App::open(with_fake_csharp_server(
        config,
        &state,
        &["--ignore-shutdown"],
        5,
    ))
    .await
    .unwrap();
    app.index(&workspace).await.unwrap();
    app.symbols(&workspace, "Calculator").await.unwrap();
    assert!(
        app.status(&workspace)
            .await
            .unwrap()
            .csharp_analyzer_running
    );
    let server: u32 = std::fs::read_to_string(state.join("pid.txt"))
        .expect("the fake server records its process id on its first request")
        .trim()
        .parse()
        .expect("a process id");
    assert!(process_is_running(server).await, "the server is running");

    let started = std::time::Instant::now();
    // 10 s is only a hard stop against a hang. The bound is the construction
    // one, as in `a_server_that_ignores_shutdown_is_killed_within_the_grace_period`;
    // exceeding `serve`'s 5 s budget would degrade to a warning plus
    // kill-on-exit, not a failure.
    let stopped = tokio::time::timeout(Duration::from_secs(10), app.shutdown_language_servers())
        .await
        .expect("shutting the language servers down must not hang");
    let elapsed = started.elapsed();
    assert_eq!(stopped, 1);
    assert!(
        elapsed < 2 * local_code_intelligence::lsp::SHUTDOWN_GRACE + Duration::from_secs(2),
        "the shutdown must be bounded by the grace period, took {elapsed:?}"
    );
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 1);
    assert_eq!(event_count(&state, "exit_seen.txt"), 1);
    assert!(
        !app.status(&workspace)
            .await
            .unwrap()
            .csharp_analyzer_running
    );
    assert_eq!(app.shutdown_language_servers().await, 0);

    // The server that ignored both was killed: its process is gone.
    wait_until_process_ends(
        "the server that ignored shutdown to end after the service shut down",
        server,
        DROP_KILL_DEADLINE,
    )
    .await;
    models_task.abort();
}

/// The priming request (3 s here) outlasts the idle timeout (1 s), and the
/// `workspace/symbol` caller is waiting for it: the server must not be
/// released from under that caller, which would fail its request and cost a
/// second start.
#[tokio::test]
async fn a_symbol_call_waiting_for_priming_is_not_cut_off_by_the_idle_sweep() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_fixture(&temp, "workspace-symbols");
    config.lsp_timeout_seconds = 5;
    config.lsp_idle_timeout_seconds = 1;
    config
        .csharp
        .args
        .extend(["--first-symbol-delay-ms".to_owned(), "3000".to_owned()]);
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert_eq!(
        state_count(&state, "spawn_count.txt"),
        1,
        "the call must be served by the server it started, not by a restart"
    );
    assert_eq!(event_count(&state, "shutdown_seen.txt"), 0);
}

/// Nobody is waiting on this server (the caller gave up, as a search past its
/// budget does), and it is still priming for much longer (5 s) than the idle
/// timeout (1 s): the sweep must leave it alone until priming has settled,
/// because releasing it mid-priming throws the index build away. Without the
/// check the first sweep that finds the session idle (about 2 s in) releases
/// it inside the 2.5 s the test watches.
#[tokio::test]
async fn the_idle_sweep_leaves_a_server_alone_while_it_is_priming() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_fixture(&temp, "workspace-symbols");
    config.lsp_timeout_seconds = 10;
    config.lsp_idle_timeout_seconds = 1;
    config
        .csharp
        .args
        .extend(["--first-symbol-delay-ms".to_owned(), "5000".to_owned()]);
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let abandoned = tokio::time::timeout(
        Duration::from_millis(300),
        manager.symbols(&adapter, &workspace, "Calculator"),
    )
    .await;
    assert!(
        abandoned.is_err(),
        "the call must give up while the server is still priming"
    );
    wait_until("the server to start", Duration::from_secs(10), async || {
        manager.running("csharp-ls", &workspace.id).await
    })
    .await;

    let watching_since = std::time::Instant::now();
    while watching_since.elapsed() < Duration::from_millis(2500) {
        assert!(
            manager.running("csharp-ls", &workspace.id).await
                && event_count(&state, "shutdown_seen.txt") == 0,
            "the server was released after {:?}, while it was still priming",
            watching_since.elapsed()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

/// Releasing a session whose server is still starting has nothing to shut
/// down, so it only lets go of the session; a caller already waiting on that
/// start is still served by it (fail-open), without a second spawn.
#[tokio::test]
async fn releasing_a_server_that_is_still_starting_does_not_fail_the_waiting_call() {
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state) =
        fake_slow_start_csharp_fixture(&temp, "workspace-symbols", 2000);
    let manager = Arc::new(Manager::new(&config));
    let adapter = csharp_adapter(&manager);
    let waiting = tokio::spawn({
        let manager = Arc::clone(&manager);
        let adapter = Arc::clone(&adapter);
        let workspace = workspace.clone();
        async move { manager.symbols(&adapter, &workspace, "Calculator").await }
    });
    wait_until(
        "the server process to be spawned",
        Duration::from_secs(10),
        async || state.join("spawn_count.txt").exists(),
    )
    .await;
    assert!(
        !manager.running("csharp-ls", &workspace.id).await,
        "the server must still be starting when it is released"
    );

    assert_eq!(manager.release(&workspace.id).await, vec!["csharp-ls"]);
    assert!(!manager.running("csharp-ls", &workspace.id).await);

    let results = waiting.await.unwrap().unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);
    assert!(
        !manager.running("csharp-ls", &workspace.id).await,
        "the released session is not back in the registry"
    );
}

#[tokio::test]
async fn a_file_above_the_did_open_cap_is_navigated_without_sending_its_text() {
    use local_code_intelligence::lsp::adapter::MAX_DID_OPEN_BYTES;
    let did_opens = |state: &std::path::Path| {
        std::fs::read_to_string(state.join("did_open_seen.txt"))
            .map(|text| text.lines().count())
            .unwrap_or(0)
    };
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state) = fake_typescript_fixture(&temp, "definition");
    let cap = MAX_DID_OPEN_BYTES as usize;
    write(&workspace.path, "src/AtCap.ts", &"x".repeat(cap));
    write(&workspace.path, "src/Generated.ts", &"x".repeat(cap + 1));
    let manager = Manager::new(&config);
    let adapter = typescript_adapter(&manager);
    let at_cap = workspace.path.join("src/AtCap.ts");
    let above_cap = workspace.path.join("src/Generated.ts");

    // A file of exactly the cap is still opened, which also shows the count
    // below is not vacuously unchanged. At least one: a restart after a slow
    // first attempt runs the hook again.
    let results = manager
        .definition(&adapter, &workspace, &at_cap, "src/AtCap.ts", 1, 1)
        .await
        .unwrap();
    assert!(!results.is_empty());
    let opened = did_opens(&state);
    assert!(opened >= 1, "a file of exactly the cap is opened");

    let results = manager
        .definition(&adapter, &workspace, &above_cap, "src/Generated.ts", 1, 1)
        .await
        .unwrap();
    assert!(
        !results.is_empty(),
        "the request is still sent and answered"
    );
    assert_eq!(did_opens(&state), opened, "the oversized file was not sent");
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
    // The server's own timeout is what this test exercises; keep the search
    // budget out of its way.
    config.lsp_search_budget_ms = 30_000;
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

/// One workspace of a single optional-server language: search works without
/// the language server and never starts it, and navigation names the
/// disabled server.
struct OptionalServerCase {
    language: &'static str,
    directory: &'static str,
    file: &'static str,
    source: &'static str,
    query: &'static str,
    /// Where the navigation calls point (line, character).
    position: (u32, u32),
    /// What the disabled server is called in the navigation error.
    server: &'static str,
    running: fn(&local_code_intelligence::app::Status) -> bool,
    /// Only the Python workspace also checks the embedded chunk count.
    embedded_chunks: Option<usize>,
}

#[tokio::test]
async fn single_language_search_and_navigation_keep_each_optional_lsp_optional() {
    let cases = [
        OptionalServerCase {
            language: "python",
            directory: "python-only",
            file: "src/revenue.py",
            source: "# Quarterly revenue summary implementation.\ndef compute_quarterly_revenue_summary(rows):\n    return [row for row in rows if row.get(\"amount\")]\n",
            query: "compute_quarterly_revenue_summary",
            position: (1, 0),
            server: "pyright",
            running: |status| status.python_analyzer_running,
            embedded_chunks: Some(1),
        },
        OptionalServerCase {
            language: "go",
            directory: "go-only",
            file: "revenue.go",
            source: "package revenue\n\n// ComputeQuarterlyRevenueSummary sums quarterly revenue rows.\nfunc ComputeQuarterlyRevenueSummary(rows []int) int {\n\ttotal := 0\n\tfor _, row := range rows {\n\t\ttotal += row\n\t}\n\treturn total\n}\n",
            query: "ComputeQuarterlyRevenueSummary",
            position: (1, 0),
            server: "gopls",
            running: |status| status.go_analyzer_running,
            embedded_chunks: None,
        },
        OptionalServerCase {
            language: "c",
            directory: "c-only",
            file: "revenue.c",
            source: "// compute_quarterly_revenue_summary sums quarterly revenue rows.\nint compute_quarterly_revenue_summary(int *rows, int count) {\n    int total = 0;\n    for (int i = 0; i < count; i++) {\n        total += rows[i];\n    }\n    return total;\n}\n",
            query: "compute_quarterly_revenue_summary",
            position: (1, 0),
            server: "clangd",
            running: |status| status.clangd_analyzer_running,
            embedded_chunks: None,
        },
        OptionalServerCase {
            language: "java",
            directory: "java-only",
            file: "RevenueService.java",
            source: "package revenue;\n\n// Computes quarterly revenue summaries.\npublic final class RevenueService {\n    public static int computeQuarterlyRevenueSummary(int[] rows) {\n        int total = 0;\n        for (int row : rows) {\n            total += row;\n        }\n        return total;\n    }\n}\n",
            query: "computeQuarterlyRevenueSummary",
            position: (1, 0),
            server: "jdtls",
            running: |status| status.java_analyzer_running,
            embedded_chunks: None,
        },
        OptionalServerCase {
            language: "csharp",
            directory: "csharp-only",
            file: "src/RevenueService.cs",
            source: "namespace Billing;\npublic sealed class RevenueService { public decimal ComputeQuarterlyRevenue() => 42m; }\n",
            query: "ComputeQuarterlyRevenue",
            position: (2, 28),
            server: "csharp-ls",
            running: |status| status.analyzer_running,
            embedded_chunks: None,
        },
    ];
    // Each case has its own fixture, so they run side by side.
    futures::future::join_all(cases.into_iter().map(assert_optional_server_stays_off)).await;
}

async fn assert_optional_server_stays_off(case: OptionalServerCase) {
    let name = case.language;
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join(case.directory);
    write(&workspace, case.file, case.source);
    let app = App::open(config).await.unwrap();
    let indexed = app.index(&workspace).await.unwrap();
    assert_eq!(indexed.files, 1, "{name}");
    assert_eq!(indexed.parsed_files, 1, "{name}");
    if let Some(embedded) = case.embedded_chunks {
        assert_eq!(indexed.embedded_chunks, embedded, "{name}");
    }

    let report = app.search(&workspace, case.query, Some(4)).await.unwrap();
    assert_eq!(report.results[0].chunk.language, case.language, "{name}");
    assert_eq!(
        report.results[0].chunk.relative_file_path, case.file,
        "{name}"
    );
    assert!(
        report
            .warning
            .as_deref()
            .is_none_or(|warning| !warning.contains("LSP")),
        "{name}: unexpected warning: {:?}",
        report.warning
    );
    assert!(
        !(case.running)(&app.status(&workspace).await.unwrap()),
        "{name}"
    );

    let (line, character) = case.position;
    let definition_error = app
        .definition(&workspace, case.file, line, character)
        .await
        .unwrap_err();
    let after_definition = (case.running)(&app.status(&workspace).await.unwrap());
    let references_error = app
        .references(&workspace, case.file, line, character, true)
        .await
        .unwrap_err();
    let after_references = (case.running)(&app.status(&workspace).await.unwrap());
    for (what, error, running) in [
        ("definition", definition_error, after_definition),
        ("references", references_error, after_references),
    ] {
        assert!(
            error
                .to_string()
                .contains(&format!("{} is disabled", case.server)),
            "{name} {what}: unexpected error: {error}"
        );
        assert!(!running, "{name} {what}: the server was started");
    }
    task.abort();
}

/// With the embedder down, `index_workspace` still builds a vectorless index,
/// so the navigation tools, which need an index but no vectors, answer.
#[tokio::test]
async fn navigation_answers_after_index_workspace_with_the_embedder_down() {
    let (temp, mut config, _fake, models_task) = fixture().await;
    // A port that refuses connections, held for the whole test.
    let closed = ClosedPort::new();
    config.embedding_url = format!("http://127.0.0.1:{}/v1", closed.port);
    let state = temp.path().join("state");
    let config = with_fake_csharp_server(config, &state, &[], LOADED_BUDGET_SECONDS);
    let workspace = temp.path().join("workspace");
    write_csharp_workspace(&workspace);
    let app = App::open(config).await.unwrap();

    let error = app.symbols(&workspace, "Calculator").await.unwrap_err();
    assert!(
        format!("{error:#}").contains("workspace is not indexed"),
        "{error:#}"
    );
    let indexed = app.index(&workspace).await.unwrap();
    assert!(indexed.vectors_missing);
    assert_eq!(indexed.embedded_chunks, 0);
    assert!(indexed.chunks > 0);
    assert_eq!(
        serde_json::to_value(&indexed).unwrap()["vectors_missing"],
        true
    );
    assert!(app.status(&workspace).await.unwrap().vectors_missing);

    let report = app.symbols(&workspace, "Calculator").await.unwrap();
    assert!(
        report
            .results
            .iter()
            .any(|location| location.name.as_deref() == Some("Calculator")),
        "{:?}",
        report.results
    );
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    models_task.abort();
}

// ---------------------------------------------------------------------------
// The fake language server in `documents` mode (`fake_lsp_documents.rs`): it
// answers from the text it was sent, so these tests drive it with disk edits
// and document notifications and read back what it answers and records.
// ---------------------------------------------------------------------------

const CALCULATOR_CS: &str = "namespace Acceptance;\npublic sealed class Calculator { public int Add(int a, int b) => a + b; }\n";
const CALL_SITE_CS: &str = "namespace Acceptance;\npublic static class CallSite { public static int Run(Calculator value) => value.Add(1, 2); }\n";
const SHAPES_CS: &str = "public interface IShape { double Area(); }\npublic class Circle : IShape { public double Area() => 3.14; }\npublic class Square : IShape { public double Area() => 1.0; }\n";

/// `(line, character)` of the first `needle` in `text`.
fn position_of(text: &str, needle: &str) -> (u32, u32) {
    text.lines()
        .enumerate()
        .find_map(|(line, content)| {
            content
                .find(needle)
                .map(|column| (line as u32, column as u32))
        })
        .unwrap_or_else(|| panic!("{needle:?} not in {text:?}"))
}

/// A raw client for the documents-mode fake, started past its handshake.
struct DocumentsFake {
    client: Arc<JsonRpcClient>,
    root: std::path::PathBuf,
    state: std::path::PathBuf,
}

impl DocumentsFake {
    /// The fake on a temporary C# workspace (`src/Calculator.cs`,
    /// `src/CallSite.cs`).
    async fn start(temp: &tempfile::TempDir) -> Self {
        let root = temp.path().join("workspace");
        write_csharp_workspace(&root);
        Self::start_on(temp, root).await
    }

    /// The fake on `root`, whose files are already written: the start-up walk
    /// reads them when `initialized` arrives.
    async fn start_on(temp: &tempfile::TempDir, root: std::path::PathBuf) -> Self {
        let state = temp.path().join("state");
        let transport = TransportConfig {
            executable: env!("CARGO_BIN_EXE_fake-lsp-server").to_owned(),
            args: vec![
                "--mode".to_owned(),
                "documents".to_owned(),
                "--state".to_owned(),
                state.to_string_lossy().into_owned(),
            ],
            working_dir: temp.path().to_owned(),
            timeout: Duration::from_secs(20),
            label: "fake".to_owned(),
        };
        let client = JsonRpcClient::spawn(&transport, &[], Box::new(|_| false))
            .await
            .unwrap();
        let root_uri = reqwest::Url::from_directory_path(&root)
            .unwrap()
            .to_string();
        client
            .request(
                "initialize",
                json!({"rootUri": root_uri, "capabilities": {}}),
            )
            .await
            .unwrap();
        client.notify("initialized", json!({})).await.unwrap();
        Self {
            client,
            root,
            state,
        }
    }

    fn uri(&self, relative: &str) -> String {
        reqwest::Url::from_file_path(self.root.join(relative))
            .unwrap()
            .to_string()
    }

    /// The workspace-relative path of an answer's uri.
    fn relative(&self, uri: &str) -> String {
        let path = reqwest::Url::parse(uri).unwrap().to_file_path().unwrap();
        path.strip_prefix(&self.root)
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/")
    }

    async fn ask(&self, method: &str, params: Value) -> Value {
        self.client.request(method, params).await.unwrap()
    }

    async fn at(&self, method: &str, relative: &str, at: (u32, u32)) -> Value {
        self.ask(
            method,
            json!({
                "textDocument": {"uri": self.uri(relative)},
                "position": {"line": at.0, "character": at.1},
            }),
        )
        .await
    }

    async fn definition(&self, relative: &str, at: (u32, u32)) -> Vec<(String, u64, u64)> {
        let answer = self.at("textDocument/definition", relative, at).await;
        self.places(&answer)
    }

    async fn references(
        &self,
        relative: &str,
        at: (u32, u32),
        declaration: bool,
    ) -> Vec<(String, u64, u64)> {
        let answer = self
            .ask(
                "textDocument/references",
                json!({
                    "textDocument": {"uri": self.uri(relative)},
                    "position": {"line": at.0, "character": at.1},
                    "context": {"includeDeclaration": declaration},
                }),
            )
            .await;
        self.places(&answer)
    }

    async fn symbols(&self, query: &str) -> Value {
        self.ask("workspace/symbol", json!({"query": query})).await
    }

    /// `(file, line, character)` of every location (or symbol) in an answer,
    /// in the order answered.
    fn places(&self, answer: &Value) -> Vec<(String, u64, u64)> {
        answer
            .as_array()
            .unwrap_or_else(|| panic!("not a list: {answer}"))
            .iter()
            .map(|item| {
                let location = item.get("location").unwrap_or(item);
                (
                    self.relative(location["uri"].as_str().unwrap()),
                    location["range"]["start"]["line"].as_u64().unwrap(),
                    location["range"]["start"]["character"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    async fn open(&self, relative: &str, version: i64, text: &str) {
        self.client
            .notify(
                "textDocument/didOpen",
                json!({"textDocument": {
                    "uri": self.uri(relative), "languageId": "csharp",
                    "version": version, "text": text,
                }}),
            )
            .await
            .unwrap();
    }

    async fn change(&self, relative: &str, version: i64, text: &str) {
        self.client
            .notify(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": self.uri(relative), "version": version},
                    "contentChanges": [{"text": text}],
                }),
            )
            .await
            .unwrap();
    }

    async fn close(&self, relative: &str) {
        self.client
            .notify(
                "textDocument/didClose",
                json!({"textDocument": {"uri": self.uri(relative)}}),
            )
            .await
            .unwrap();
    }

    /// `workspace/didChangeWatchedFiles` with `(file, type)` pairs: 1 created,
    /// 2 changed, 3 deleted.
    async fn watched(&self, changes: &[(&str, u32)]) {
        let changes: Vec<Value> = changes
            .iter()
            .map(|(relative, kind)| json!({"uri": self.uri(relative), "type": kind}))
            .collect();
        self.client
            .notify(
                "workspace/didChangeWatchedFiles",
                json!({"changes": changes}),
            )
            .await
            .unwrap();
    }

    /// `events.jsonl`: one object per message the fake received, in order.
    fn events(&self) -> Vec<Value> {
        std::fs::read_to_string(self.state.join("events.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    /// The documents the fake answers from, as it recorded them.
    fn documents(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.state.join("documents.json")).unwrap())
            .unwrap()
    }

    /// What `documents.json` says about one file (`Null` when it is absent).
    fn document(&self, relative: &str) -> Value {
        self.documents()[self.uri(relative)].clone()
    }

    async fn finish(self) {
        self.client.shutdown(Duration::from_secs(1)).await;
    }
}

fn place(file: &str, at: (u32, u32)) -> (String, u64, u64) {
    (file.to_owned(), u64::from(at.0), u64::from(at.1))
}

fn names(items: &Value) -> Vec<&str> {
    items
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["name"].as_str().unwrap())
        .collect()
}

/// A definition and the references come from the text the start-up walk read,
/// and a disk edit that no notification reports stays unseen: the answer still
/// has the old line.
#[tokio::test]
async fn documents_fake_answers_definition_and_references_from_the_walked_text() {
    let temp = tempfile::tempdir().unwrap();
    let fake = DocumentsFake::start(&temp).await;
    let declared = place("src/Calculator.cs", position_of(CALCULATOR_CS, "Add"));
    let call = position_of(CALL_SITE_CS, "Add");
    let called = place("src/CallSite.cs", call);

    assert_eq!(
        fake.definition("src/CallSite.cs", call).await,
        std::slice::from_ref(&declared)
    );
    let mut references = fake.references("src/CallSite.cs", call, true).await;
    references.sort();
    assert_eq!(references, [declared.clone(), called.clone()]);
    assert_eq!(
        fake.references("src/CallSite.cs", call, false).await,
        [called],
        "the declaration only with includeDeclaration"
    );

    // The file moves `Add` one line lower on disk; nothing tells the fake.
    write(
        &fake.root,
        "src/Calculator.cs",
        &format!("// moved\n{CALCULATOR_CS}"),
    );
    assert_eq!(
        fake.definition("src/CallSite.cs", call).await,
        [declared],
        "the snapshot of the walk is answered, not the disk"
    );
    let calculator = fake.document("src/Calculator.cs");
    assert_eq!(calculator["source"], "walk");
    assert_eq!(calculator["lines"], 2, "the walked text, not the edit");
    fake.finish().await;
}

/// `didOpen` text wins over the walked snapshot; `didClose` brings the
/// snapshot back. The log has the open, the request that saw it and the close
/// in the order they arrived.
#[tokio::test]
async fn documents_fake_did_change_moves_the_answer_and_did_close_restores_the_snapshot() {
    let temp = tempfile::tempdir().unwrap();
    let fake = DocumentsFake::start(&temp).await;
    let walked = place("src/Calculator.cs", position_of(CALCULATOR_CS, "Add"));
    let call = position_of(CALL_SITE_CS, "Add");
    let moved_text = format!("\n{CALCULATOR_CS}");
    let moved = place("src/Calculator.cs", position_of(&moved_text, "Add"));
    assert_ne!(moved, walked);

    fake.open("src/Calculator.cs", 1, &moved_text).await;
    assert_eq!(
        fake.definition("src/CallSite.cs", call).await,
        [moved],
        "the open text"
    );
    assert_eq!(fake.document("src/Calculator.cs")["source"], "open");

    fake.close("src/Calculator.cs").await;
    assert_eq!(
        fake.definition("src/CallSite.cs", call).await,
        [walked],
        "the walked snapshot again"
    );
    assert_eq!(fake.document("src/Calculator.cs")["source"], "walk");

    let events = fake.events();
    let log: Vec<(&str, Option<i64>)> = events
        .iter()
        .map(|event| (event["method"].as_str().unwrap(), event["version"].as_i64()))
        .filter(|(method, _)| {
            matches!(
                *method,
                "textDocument/didOpen" | "textDocument/definition" | "textDocument/didClose"
            )
        })
        .collect();
    assert_eq!(
        log,
        [
            ("textDocument/didOpen", Some(1)),
            ("textDocument/definition", None),
            ("textDocument/didClose", None),
            ("textDocument/definition", None),
        ]
    );
    let seqs: Vec<u64> = events
        .iter()
        .map(|event| event["seq"].as_u64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|pair| pair[0] < pair[1]), "{seqs:?}");
    fake.finish().await;
}

/// Full sync with a version that does not exceed the stored one is ignored
/// (a reordered notification), and so is a change for a document that was
/// never opened. The answers show it and the log names the reason.
#[tokio::test]
async fn documents_fake_refuses_a_stale_or_unopened_did_change() {
    let temp = tempfile::tempdir().unwrap();
    let fake = DocumentsFake::start(&temp).await;
    let call = position_of(CALL_SITE_CS, "Add");
    let v3_text = format!("\n{CALCULATOR_CS}");
    let v2_text = format!("\n\n\n{CALCULATOR_CS}");
    let v3 = place("src/Calculator.cs", position_of(&v3_text, "Add"));

    fake.open("src/Calculator.cs", 1, CALCULATOR_CS).await;
    fake.change("src/Calculator.cs", 3, &v3_text).await;
    fake.change("src/Calculator.cs", 2, &v2_text).await;
    // Never opened: the walked text of the call site stays.
    fake.change("src/CallSite.cs", 1, &format!("\n\n{CALL_SITE_CS}"))
        .await;

    assert_eq!(
        fake.definition("src/CallSite.cs", call).await,
        std::slice::from_ref(&v3),
        "version 3 stays after the stale version 2"
    );
    let mut references = fake
        .references("src/Calculator.cs", (v3.1 as u32, v3.2 as u32), true)
        .await;
    references.sort();
    assert_eq!(
        references,
        [v3, place("src/CallSite.cs", call)],
        "the unopened change did not move the call site"
    );

    let events = fake.events();
    let notes: Vec<(String, Option<&str>)> = events
        .iter()
        .filter(|event| event["method"] == "textDocument/didChange")
        .map(|event| (event["version"].to_string(), event["note"].as_str()))
        .collect();
    assert_eq!(
        notes,
        [
            ("3".to_owned(), None),
            ("2".to_owned(), Some("change-stale-version")),
            ("1".to_owned(), Some("change-unopened")),
        ]
    );
    assert_eq!(fake.document("src/Calculator.cs")["version"], 3);
    fake.finish().await;
}

/// `didChangeWatchedFiles` makes the fake read the disk then, not before and
/// not at answer time: a changed file moves, a deleted one disappears from
/// `workspace/symbol`, a created one is listed only after its event.
#[tokio::test]
async fn documents_fake_did_change_watched_files_re_reads_the_disk() {
    let temp = tempfile::tempdir().unwrap();
    let fake = DocumentsFake::start(&temp).await;
    let call = position_of(CALL_SITE_CS, "Add");

    // Changed.
    let moved_text = format!("\n{CALCULATOR_CS}");
    write(&fake.root, "src/Calculator.cs", &moved_text);
    fake.watched(&[("src/Calculator.cs", 2)]).await;
    assert_eq!(
        fake.definition("src/CallSite.cs", call).await,
        [place("src/Calculator.cs", position_of(&moved_text, "Add"))]
    );
    assert_eq!(fake.document("src/Calculator.cs")["source"], "watched");

    // Created: on disk but not announced is not listed; announced is.
    write(
        &fake.root,
        "src/Circle.cs",
        "public class Circle { public int Area() => 1; }\n",
    );
    assert_eq!(fake.symbols("Circle").await, json!([]));
    fake.watched(&[("src/Circle.cs", 1)]).await;
    assert_eq!(
        fake.places(&fake.symbols("Circle").await),
        [place("src/Circle.cs", (0, 13))]
    );

    // Deleted.
    assert_eq!(names(&fake.symbols("Calculator").await), ["Calculator"]);
    std::fs::remove_file(fake.root.join("src/Calculator.cs")).unwrap();
    fake.watched(&[("src/Calculator.cs", 3)]).await;
    assert_eq!(fake.symbols("Calculator").await, json!([]));
    assert_eq!(fake.symbols("Add").await, json!([]));
    assert_eq!(fake.document("src/Calculator.cs"), Value::Null);

    let types: Vec<Value> = fake
        .events()
        .iter()
        .filter(|event| event["method"] == "workspace/didChangeWatchedFiles")
        .map(|event| event["changes"][0]["type"].clone())
        .collect();
    assert_eq!(types, [json!(2), json!(1), json!(3)]);
    let events = fake.events();
    let queries: Vec<&str> = events
        .iter()
        .filter(|event| event["method"] == "workspace/symbol")
        .map(|event| event["query"].as_str().unwrap())
        .collect();
    assert_eq!(
        queries,
        ["Circle", "Circle", "Calculator", "Calculator", "Add"]
    );
    fake.finish().await;
}

/// `implementation` and the type hierarchy follow the supertype lists of the
/// store, so an opened text that drops a base type changes them.
#[tokio::test]
async fn documents_fake_answers_implementation_and_type_hierarchy_from_the_store() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    write(&root, "src/Shapes.cs", SHAPES_CS);
    let fake = DocumentsFake::start_on(&temp, root).await;
    let interface = position_of(SHAPES_CS, "IShape");
    let circle = position_of(SHAPES_CS, "Circle");
    let square = position_of(SHAPES_CS, "Square");

    let implementations = fake
        .at("textDocument/implementation", "src/Shapes.cs", interface)
        .await;
    assert_eq!(
        fake.places(&implementations),
        [
            place("src/Shapes.cs", circle),
            place("src/Shapes.cs", square)
        ]
    );
    let circle_item = fake
        .at("textDocument/prepareTypeHierarchy", "src/Shapes.cs", circle)
        .await;
    assert_eq!(names(&circle_item), ["Circle"]);
    let supertypes = fake
        .ask("typeHierarchy/supertypes", json!({"item": circle_item[0]}))
        .await;
    assert_eq!(names(&supertypes), ["IShape"]);
    let interface_item = fake
        .at(
            "textDocument/prepareTypeHierarchy",
            "src/Shapes.cs",
            interface,
        )
        .await;
    let subtypes = fake
        .ask("typeHierarchy/subtypes", json!({"item": interface_item[0]}))
        .await;
    assert_eq!(names(&subtypes), ["Circle", "Square"]);

    // An opened text in which Square no longer implements IShape.
    fake.open(
        "src/Shapes.cs",
        1,
        &SHAPES_CS.replace("public class Square : IShape", "public class Square"),
    )
    .await;
    let implementations = fake
        .at("textDocument/implementation", "src/Shapes.cs", interface)
        .await;
    assert_eq!(
        fake.places(&implementations),
        [place("src/Shapes.cs", circle)]
    );
    let subtypes = fake
        .ask("typeHierarchy/subtypes", json!({"item": interface_item[0]}))
        .await;
    assert_eq!(names(&subtypes), ["Circle"]);
    fake.finish().await;
}

/// Incoming calls are the call sites of the name grouped by the function that
/// holds them; outgoing calls are the calls inside a function resolved to
/// declarations. A `didChange` that adds a second call line shows in both.
#[tokio::test]
async fn documents_fake_answers_call_hierarchy_in_and_out_from_the_store() {
    let temp = tempfile::tempdir().unwrap();
    let fake = DocumentsFake::start(&temp).await;
    let add = position_of(CALCULATOR_CS, "Add");

    let add_item = fake
        .at(
            "textDocument/prepareCallHierarchy",
            "src/Calculator.cs",
            add,
        )
        .await;
    assert_eq!(names(&add_item), ["Add"]);
    let incoming = fake
        .ask("callHierarchy/incomingCalls", json!({"item": add_item[0]}))
        .await;
    assert_eq!(incoming.as_array().unwrap().len(), 1, "{incoming}");
    assert_eq!(incoming[0]["from"]["name"], "Run");
    assert_eq!(incoming[0]["fromRanges"].as_array().unwrap().len(), 1);

    let run = position_of(CALL_SITE_CS, "Run");
    let run_item = fake
        .at("textDocument/prepareCallHierarchy", "src/CallSite.cs", run)
        .await;
    assert_eq!(names(&run_item), ["Run"]);
    let outgoing = fake
        .ask("callHierarchy/outgoingCalls", json!({"item": run_item[0]}))
        .await;
    assert_eq!(outgoing.as_array().unwrap().len(), 1, "{outgoing}");
    assert_eq!(outgoing[0]["to"]["name"], "Add");
    assert_eq!(outgoing[0]["fromRanges"].as_array().unwrap().len(), 1);

    // A call site resolves to its callee, in the file that declares it.
    let call = position_of(CALL_SITE_CS, "Add");
    let callee = fake
        .at("textDocument/prepareCallHierarchy", "src/CallSite.cs", call)
        .await;
    assert_eq!(names(&callee), ["Add"]);
    assert_eq!(
        fake.relative(callee[0]["uri"].as_str().unwrap()),
        "src/Calculator.cs"
    );

    // `Run` now calls `Add` twice.
    let two_calls = "namespace Acceptance;\npublic static class CallSite {\n    public static int Run(Calculator value) {\n        var first = value.Add(1, 2);\n        return value.Add(first, 3);\n    }\n}\n";
    fake.open("src/CallSite.cs", 1, CALL_SITE_CS).await;
    fake.change("src/CallSite.cs", 2, two_calls).await;
    let incoming = fake
        .ask("callHierarchy/incomingCalls", json!({"item": add_item[0]}))
        .await;
    assert_eq!(incoming.as_array().unwrap().len(), 1, "{incoming}");
    assert_eq!(incoming[0]["from"]["name"], "Run");
    assert_eq!(incoming[0]["fromRanges"].as_array().unwrap().len(), 2);
    let run_item = fake
        .at(
            "textDocument/prepareCallHierarchy",
            "src/CallSite.cs",
            position_of(two_calls, "Run"),
        )
        .await;
    let outgoing = fake
        .ask("callHierarchy/outgoingCalls", json!({"item": run_item[0]}))
        .await;
    assert_eq!(outgoing.as_array().unwrap().len(), 1, "{outgoing}");
    assert_eq!(outgoing[0]["fromRanges"].as_array().unwrap().len(), 2);

    // A drifted position (indentation, or past the end) is on no identifier.
    for drifted in [(3, 2), (99, 0)] {
        let none = fake
            .at(
                "textDocument/prepareCallHierarchy",
                "src/CallSite.cs",
                drifted,
            )
            .await;
        assert_eq!(none, json!([]), "{drifted:?}");
    }
    // The log names the item of a hierarchy request.
    let events = fake.events();
    let incoming_items: Vec<&Value> = events
        .iter()
        .filter(|event| event["method"] == "callHierarchy/incomingCalls")
        .map(|event| &event["item"]["name"])
        .collect();
    assert_eq!(incoming_items, ["Add", "Add"]);
    fake.finish().await;
}

const COUNTER_RS: &str = "pub trait Shape { fn area(&self) -> f64; }\npub struct Counter { n: u32 }\nimpl Counter {\n    pub fn new() -> Counter { Counter { n: 0 } }\n}\npub fn make() -> Counter { Counter::new() }\n";

/// In Rust `fn new(` declares a function and `Counter::new()` is a call of it;
/// a bodiless trait member (`fn area(&self) -> f64;`) is a declaration too. A
/// disk edit without a notification changes nothing.
#[tokio::test]
async fn documents_fake_reads_fn_new_as_a_declaration_and_a_path_call_as_a_call() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    write(&root, "src/lib.rs", COUNTER_RS);
    let fake = DocumentsFake::start_on(&temp, root).await;
    let new = position_of(COUNTER_RS, "new");
    assert_eq!(new.0, 3, "the first `new` is the declaration");

    let symbols = fake.symbols("new").await;
    assert_eq!(fake.places(&symbols), [place("src/lib.rs", new)]);
    assert_eq!(symbols[0]["kind"], 6);
    assert_eq!(symbols[0]["containerName"], "Counter");
    let area = fake.symbols("area").await;
    assert_eq!(
        fake.places(&area),
        [place("src/lib.rs", position_of(COUNTER_RS, "area"))],
        "a bodiless member is a declaration"
    );

    let last_line = COUNTER_RS.lines().nth(5).unwrap();
    let call = (5, last_line.find("new").unwrap() as u32);
    let callee = fake
        .at("textDocument/prepareCallHierarchy", "src/lib.rs", call)
        .await;
    assert_eq!(fake.places(&callee), [place("src/lib.rs", new)]);
    let incoming = fake
        .ask("callHierarchy/incomingCalls", json!({"item": callee[0]}))
        .await;
    assert_eq!(incoming.as_array().unwrap().len(), 1, "{incoming}");
    assert_eq!(incoming[0]["from"]["name"], "make");
    assert_eq!(incoming[0]["fromRanges"].as_array().unwrap().len(), 1);

    // `fn new` is renamed on disk; no notification.
    write(
        &fake.root,
        "src/lib.rs",
        &COUNTER_RS.replace("fn new", "fn create"),
    );
    assert_eq!(
        fake.places(&fake.symbols("new").await),
        [place("src/lib.rs", new)]
    );
    fake.finish().await;
}

/// Odd notifications are applied or refused, never silently: a second
/// `didOpen` of an open document still applies, and a `didChange` without
/// changes, without text or with a range is ignored, each with its own note.
#[tokio::test]
async fn documents_fake_names_the_notifications_it_ignores_or_applies_oddly() {
    let temp = tempfile::tempdir().unwrap();
    let fake = DocumentsFake::start(&temp).await;
    let call = position_of(CALL_SITE_CS, "Add");
    let second_text = format!("\n{CALCULATOR_CS}");
    let uri = fake.uri("src/Calculator.cs");

    fake.open("src/Calculator.cs", 1, CALCULATOR_CS).await;
    fake.open("src/Calculator.cs", 1, &second_text).await;
    for (version, changes) in [
        (2, json!([])),
        (3, json!([{}])),
        (
            4,
            json!([{"range": {"start": {"line": 0, "character": 0},
                              "end": {"line": 0, "character": 0}}, "text": "x"}]),
        ),
    ] {
        fake.client
            .notify(
                "textDocument/didChange",
                json!({
                    "textDocument": {"uri": uri, "version": version},
                    "contentChanges": changes,
                }),
            )
            .await
            .unwrap();
    }

    assert_eq!(
        fake.definition("src/CallSite.cs", call).await,
        [place("src/Calculator.cs", position_of(&second_text, "Add"))],
        "the second didOpen applied and no odd change did"
    );
    let events = fake.events();
    let notes: Vec<(&str, i64, Option<&str>)> = events
        .iter()
        .filter(|event| {
            matches!(
                event["method"].as_str(),
                Some("textDocument/didOpen" | "textDocument/didChange")
            )
        })
        .map(|event| {
            (
                event["method"].as_str().unwrap(),
                event["version"].as_i64().unwrap(),
                event["note"].as_str(),
            )
        })
        .collect();
    assert_eq!(
        notes,
        [
            ("textDocument/didOpen", 1, None),
            ("textDocument/didOpen", 1, Some("open-reopened")),
            ("textDocument/didChange", 2, Some("change-empty")),
            ("textDocument/didChange", 3, Some("change-no-text")),
            (
                "textDocument/didChange",
                4,
                Some("change-incremental-ignored")
            ),
        ]
    );
    assert!(
        events.iter().all(|event| event["spawn"] == 1),
        "every event carries the ordinal of the server process"
    );
    fake.finish().await;
}

/// The mode works behind the product's client: the manager's `symbols`,
/// `definition` and `document_symbols` return locations derived from the
/// store, mapped to workspace files with kinds.
#[tokio::test]
async fn documents_fake_serves_manager_navigation_and_outline() {
    let temp = tempfile::tempdir().unwrap();
    let workspace_path = temp.path().join("workspace");
    let data_path = temp.path().join("data");
    let state = temp.path().join("state");
    std::fs::create_dir_all(&data_path).unwrap();
    write_csharp_workspace(&workspace_path);
    let workspace = Workspace::resolve(&workspace_path, &data_path).unwrap();
    let config = with_fake_documents_server(
        Config {
            data_dir: data_path,
            ..Config::default()
        },
        &state,
        &[],
        10,
    );
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);
    let add = position_of(CALCULATOR_CS, "Add");

    let symbols = manager.symbols(&adapter, &workspace, "Add").await.unwrap();
    assert_eq!(symbols.len(), 1, "{symbols:?}");
    assert_eq!(symbols[0].name.as_deref(), Some("Add"));
    assert_eq!(symbols[0].kind, Some(6));
    assert_eq!(
        symbols[0].relative_file_path.as_deref(),
        Some("src/Calculator.cs")
    );
    assert_eq!(
        (symbols[0].start_line, symbols[0].start_character),
        (add.0 + 1, add.1)
    );
    assert_eq!(symbols[0].provider.as_deref(), Some("csharp-ls"));

    // The file moves `Add` one line lower on disk. The csharp adapter sends no
    // notification, so the fake still answers from the text it walked.
    write(
        &workspace_path,
        "src/Calculator.cs",
        &format!("\n{CALCULATOR_CS}"),
    );
    let call = position_of(CALL_SITE_CS, "Add");
    let definitions = manager
        .definition(
            &adapter,
            &workspace,
            &workspace.path.join("src/CallSite.cs"),
            "src/CallSite.cs",
            call.0,
            call.1,
        )
        .await
        .unwrap();
    assert_eq!(definitions.len(), 1, "{definitions:?}");
    assert_eq!(
        definitions[0].relative_file_path.as_deref(),
        Some("src/Calculator.cs")
    );
    assert_eq!(
        (definitions[0].start_line, definitions[0].start_character),
        (add.0 + 1, add.1)
    );

    let outline = manager
        .document_symbols(
            &adapter,
            &workspace,
            &workspace.path.join("src/Calculator.cs"),
            "src/Calculator.cs",
        )
        .await
        .unwrap();
    let outline: Vec<(Option<&str>, Option<u64>)> = outline
        .iter()
        .map(|symbol| (symbol.name.as_deref(), symbol.kind))
        .collect();
    assert_eq!(
        outline,
        [(Some("Calculator"), Some(5)), (Some("Add"), Some(6))]
    );
    manager.shutdown_all().await;
}

// ---- `[run_profile.lsp] outline_cache` (K7b-2) ------------------------------

const CALCULATOR_PY: &str = "class Calculator:\n    def add(self, a, b):\n        return a + b\n";

/// The C# fixture plus `src/calculator.py` and two files that declare nothing
/// (`src/Notes.cs`, `src/notes.py`), with the documents-mode fake standing in
/// for csharp-ls (sent no document text) and for pyright (sent the file's
/// text in a `didOpen` before every position request), each recording under
/// its own state directory, and a manager under `outline_cache`. Returns the
/// manager, the workspace and the state directory of each fake.
fn outline_fixture(
    temp: &tempfile::TempDir,
    outline_cache: bool,
) -> (Manager, Workspace, OutlineStates) {
    let workspace_path = temp.path().join("workspace");
    let data_path = temp.path().join("data");
    let states = OutlineStates {
        csharp: temp.path().join("state-csharp"),
        python: temp.path().join("state-python"),
    };
    std::fs::create_dir_all(&data_path).unwrap();
    write_csharp_workspace(&workspace_path);
    write(&workspace_path, "src/Notes.cs", "namespace Acceptance;\n");
    write(&workspace_path, "src/calculator.py", CALCULATOR_PY);
    write(
        &workspace_path,
        "src/notes.py",
        "# nothing is declared here\n",
    );
    let workspace = Workspace::resolve(&workspace_path, &data_path).unwrap();
    let mut config = with_fake_documents_server(
        Config {
            data_dir: data_path,
            rust_analyzer_path: "definitely-missing-rust-analyzer".into(),
            ..Config::default()
        },
        &states.csharp,
        &[],
        10,
    );
    config.python = PythonLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: vec![
            "--mode".to_owned(),
            "documents".to_owned(),
            "--state".to_owned(),
            states.python.to_string_lossy().into_owned(),
        ],
        disabled: false,
    };
    config.run_profile.lsp.outline_cache = outline_cache;
    (Manager::new(&config), workspace, states)
}

struct OutlineStates {
    csharp: std::path::PathBuf,
    python: std::path::PathBuf,
}

/// The names of `relative`'s outline as the manager answers it.
async fn outline_names(
    manager: &Manager,
    adapter: &Arc<dyn LspAdapter>,
    workspace: &Workspace,
    relative: &str,
) -> Vec<String> {
    manager
        .document_symbols(adapter, workspace, &workspace.path.join(relative), relative)
        .await
        .unwrap()
        .into_iter()
        .map(|symbol| symbol.name.unwrap_or_default())
        .collect()
}

/// How many `textDocument/documentSymbol` requests for `relative` the fake
/// recording under `state` received (every server process it ran, from
/// `events.jsonl`).
fn outline_requests(state: &std::path::Path, relative: &str) -> usize {
    std::fs::read_to_string(state.join("events.jsonl"))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| {
            event["method"] == "textDocument/documentSymbol"
                && event["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.ends_with(&format!("/{relative}")))
        })
        .count()
}

/// Under `outline_cache`, for a server sent the file's text before each
/// request (pyright's adapter), a file's outline is asked once per content
/// hash and server session: the same bytes are answered from the cache, an
/// edit asks again and gets the new outline, and a new session asks again for
/// unchanged bytes. An empty outline is never kept. A server told nothing
/// about edits (csharp-ls) is asked every time, before and after an edit.
/// Without the key every call asks.
#[tokio::test]
async fn the_outline_cache_answers_by_content_hash_and_never_stores_an_empty_outline() {
    let temp = tempfile::tempdir().unwrap();
    let (manager, workspace, states) = outline_fixture(&temp, true);
    let python = python_adapter(&manager);
    let (calculator, first) = ("src/calculator.py", ["Calculator", "add"]);

    assert_eq!(
        outline_names(&manager, &python, &workspace, calculator).await,
        first
    );
    assert_eq!(
        outline_names(&manager, &python, &workspace, calculator).await,
        first
    );
    assert_eq!(
        outline_requests(&states.python, calculator),
        1,
        "the same bytes"
    );

    // A method is appended: the bytes changed, so the server is asked again,
    // and the didOpen before the request gave it the new text.
    let edited = format!("{CALCULATOR_PY}    def twice(self, a):\n        return a + a\n");
    write(&workspace.path, calculator, &edited);
    let second = ["Calculator", "add", "twice"];
    assert_eq!(
        outline_names(&manager, &python, &workspace, calculator).await,
        second
    );
    assert_eq!(
        outline_requests(&states.python, calculator),
        2,
        "an edit asks again"
    );
    assert_eq!(
        outline_names(&manager, &python, &workspace, calculator).await,
        second
    );
    assert_eq!(outline_requests(&states.python, calculator), 2);

    // A new session (after a release, started by a symbol search so it is
    // live when the outline is looked up) does not reuse the old session's
    // answer for the same bytes.
    assert!(manager.release(&workspace.id).await.contains(&"pyright"));
    let twice = manager.symbols(&python, &workspace, "twice").await.unwrap();
    assert_eq!(twice.len(), 1, "{twice:?}");
    assert_eq!(
        outline_names(&manager, &python, &workspace, calculator).await,
        second
    );
    assert_eq!(
        outline_requests(&states.python, calculator),
        3,
        "a new session asks"
    );
    assert_eq!(
        outline_names(&manager, &python, &workspace, calculator).await,
        second
    );
    assert_eq!(outline_requests(&states.python, calculator), 3);

    // An empty outline is never kept.
    for _ in 0..3 {
        assert!(
            outline_names(&manager, &python, &workspace, "src/notes.py")
                .await
                .is_empty()
        );
    }
    assert_eq!(
        outline_requests(&states.python, "src/notes.py"),
        3,
        "empty: asked every time"
    );

    // csharp-ls is told nothing about edits, so nothing it answers is kept:
    // after an edit (which it does not see) every ask is a request.
    let csharp = csharp_adapter(&manager);
    let cs = "src/Calculator.cs";
    let old = ["Calculator", "Add"];
    assert_eq!(outline_names(&manager, &csharp, &workspace, cs).await, old);
    write(
        &workspace.path,
        cs,
        &format!("{CALCULATOR_CS}public static class More {{ }}\n"),
    );
    for _ in 0..3 {
        assert_eq!(outline_names(&manager, &csharp, &workspace, cs).await, old);
    }
    assert_eq!(
        outline_requests(&states.csharp, cs),
        4,
        "not synced: never kept"
    );
    manager.shutdown_all().await;

    // Without the key every call asks.
    let temp = tempfile::tempdir().unwrap();
    let (manager, workspace, states) = outline_fixture(&temp, false);
    let python = python_adapter(&manager);
    for _ in 0..3 {
        assert_eq!(
            outline_names(&manager, &python, &workspace, calculator).await,
            first
        );
    }
    assert_eq!(
        outline_requests(&states.python, calculator),
        3,
        "off: asked every time"
    );
    manager.shutdown_all().await;
}

// ---- `lsp.live` under `[run_profile.lsp] sessions` (K7b-1) -----------------

/// A SCIP index over the C# fixture of [`fake_csharp_fixture`]: `Add`
/// (`src/Calculator.cs`, line 1) and `Run` (`src/CallSite.cs`, line 1), whose
/// text calls `Add`. It lives next to the workspace, never in it. The graph
/// has no call edge: the only way from `Add` to `Run` is `lsp.live`'s
/// `textDocument/references`.
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn csharp_scip_index(temp: &tempfile::TempDir, workspace: &Workspace) -> std::path::PathBuf {
    use scip::types::{Document, Index, Occurrence, SymbolInformation, symbol_information::Kind};
    let symbol = |name: &str| format!("rust-analyzer cargo demo 0.1.0 acceptance/{name}().");
    let line_of = |file: &str| {
        let text = std::fs::read_to_string(workspace.path.join(file)).unwrap();
        text.lines().nth(1).unwrap().to_owned()
    };
    let definition = |name: &str, line: &str| {
        let col = line.find(name).unwrap() as i32;
        let mut o = Occurrence::default();
        o.range = vec![1, col, col + name.len() as i32];
        o.symbol = symbol(name);
        o.symbol_roles = 1;
        o.enclosing_range = vec![1, 0, 1, line.len() as i32];
        o
    };
    let info = |name: &str| {
        let mut i = SymbolInformation::default();
        i.symbol = symbol(name);
        i.kind = Kind::Function.into();
        i
    };
    let (calculator, call_site) = (line_of("src/Calculator.cs"), line_of("src/CallSite.cs"));
    let mut add = Document::default();
    add.relative_path = "src/Calculator.cs".to_owned();
    add.occurrences = vec![definition("Add", &calculator)];
    add.symbols = vec![info("Add")];
    let mut run = Document::default();
    run.relative_path = "src/CallSite.cs".to_owned();
    run.occurrences = vec![definition("Run", &call_site)];
    run.symbols = vec![info("Run")];
    let mut message = Index::default();
    message.documents = vec![add, run];
    let index = temp.path().join("index.scip");
    scip::write_message_to_file(&index, message).unwrap();
    index
}

/// The C# fixture with the `documents`-mode fake and a request timeout that a
/// slow start under load does not exceed; rust-analyzer is pointed at a binary
/// that does not exist, so no test depends on one being installed.
fn shared_sessions_fixture(
    temp: &tempfile::TempDir,
) -> (Config, Workspace, std::path::PathBuf, std::path::PathBuf) {
    let (mut config, workspace, state) = fake_csharp_fixture(temp, "documents");
    config.lsp_timeout_seconds = LOADED_BUDGET_SECONDS;
    config.rust_analyzer_path = "definitely-missing-rust-analyzer".into();
    let index = csharp_scip_index(temp, &workspace);
    (config, workspace, state, index)
}

/// One `lsp.live` traversal of `Add` over the fixture, under `sessions`, with
/// `manager` offered as the shared manager. The budgets are wide: a cold fake
/// server starts well inside them even on a loaded machine.
async fn traverse_add(
    config: &Config,
    workspace: &Workspace,
    index: &std::path::Path,
    sessions: local_code_intelligence::config::LspSessions,
    manager: &Arc<Manager>,
) -> Value {
    use local_code_intelligence::{
        app::traverse::{BudgetArgs, Call, Dirs, SteerArgs, run},
        config::{LspProfile, RunProfileConfig, TraverseConfig, TraverseFirstStage},
    };
    let limits = TraverseConfig {
        enabled: true,
        first_stage: TraverseFirstStage::Off,
        answer_seeds: false,
        exact_seeds: false,
        deadline_ms: 60_000,
        ..TraverseConfig::default()
    };
    let profile = RunProfileConfig {
        lsp: LspProfile {
            sessions,
            ..LspProfile::default()
        },
        ..RunProfileConfig::default()
    };
    let (budget, steer) = (
        BudgetArgs {
            deadline_ms: Some(60_000),
            ..BudgetArgs::default()
        },
        SteerArgs::default(),
    );
    let maps = [lci_maps::lsp_live::LSP_LIVE_ID.to_owned()];
    let seeds = ["sym:rust:Calculator.cs::Add/fn".to_owned()];
    let call = Call {
        scip_index: index,
        scip_root: Some(&workspace.path),
        question: "who uses Add",
        seeds: &seeds,
        seed_search: None,
        top_k: 8,
        maps: Some(&maps),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: Some(config),
        lsp_sessions: Some(Arc::clone(manager)),
    };
    run(&limits, &profile, &Dirs::under(&config.data_dir), &call)
        .await
        .unwrap()
        .response
}

/// [`traverse_add`] past a cold language server. However wide the budget, one
/// expansion waits at most `lsp_live::CALL_TIMEOUT` (15 s, a product
/// constant), and a cold fake server on a loaded machine took 21 s to answer:
/// a walk that starts a cold server may then end with the deadline note and no
/// edges from the map, as the map documents.
///
/// Under `shared` the server is started first by one `symbols` call, which
/// returns once it answers, so the single walk that follows must reach `Run`
/// through `lsp.live` on its own: a product bug that makes a cold-start walk
/// miss the deadline cannot hide behind a retry. Under `per_call` every walk
/// starts its own server and no warm-up is possible, so a walk that ran out of
/// time is asked again, up to three walks in all; each retried walk must have
/// failed only on the deadline, and its server must have ended with it.
async fn traverse_add_past_a_cold_start(
    config: &Config,
    workspace: &Workspace,
    index: &std::path::Path,
    state: &std::path::Path,
    sessions: local_code_intelligence::config::LspSessions,
    manager: &Arc<Manager>,
) -> Value {
    use local_code_intelligence::config::LspSessions;
    if sessions == LspSessions::Shared {
        let adapter = csharp_adapter(manager);
        manager
            .symbols(&adapter, workspace, "Calculator")
            .await
            .expect("the shared server starts and answers");
        let answer = traverse_add(config, workspace, index, sessions, manager).await;
        assert!(
            reached_run_through_lsp_live(&answer),
            "the first walk over a warm shared server reached no lsp.live edge to Run: {answer}"
        );
        assert_eq!(
            state_count(state, "spawn_count.txt"),
            1,
            "the walk reused the warmed server"
        );
        return answer;
    }
    let ran_out_of_time = |answer: &Value| {
        answer["notes"].as_array().is_some_and(|notes| {
            notes.iter().any(|note| {
                note.as_str()
                    .is_some_and(|text| text.contains(lci_maps::lsp_live::DEADLINE_NOTE))
            })
        })
    };
    let mut retried_servers = Vec::new();
    let mut walks = 1;
    loop {
        let answer = traverse_add(config, workspace, index, sessions, manager).await;
        if walks == 3 || !ran_out_of_time(&answer) {
            eprintln!("per_call: {walks} walk(s) to reach an answer");
            for server in retried_servers {
                wait_until_process_ends(
                    "the server of a retried per-call walk to end",
                    server,
                    DROP_KILL_DEADLINE,
                )
                .await;
            }
            return answer;
        }
        let failures: Vec<&str> = answer["notes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|note| note.contains("calls failed"))
            .collect();
        assert!(
            failures
                .iter()
                .all(|note| note.contains(lci_maps::lsp_live::DEADLINE_NOTE)),
            "walk {walks} failed on more than the deadline: {failures:?}"
        );
        if let Some(server) = std::fs::read_to_string(state.join("pid.txt"))
            .ok()
            .and_then(|text| text.trim().parse::<u32>().ok())
        {
            retried_servers.push(server);
        }
        walks += 1;
    }
}

/// Whether the answer reached `Run` through `lsp.live`.
fn reached_run_through_lsp_live(answer: &Value) -> bool {
    answer["results"].as_array().is_some_and(|cards| {
        cards.iter().any(|card| {
            card["node"].as_str() == Some("sym:rust:CallSite.cs::Run/fn")
                && card.to_string().contains("lsp.live")
        })
    })
}

#[tokio::test]
async fn two_traverse_calls_share_one_language_server_under_shared_sessions() {
    use local_code_intelligence::config::LspSessions;
    for (sessions, spawned) in [(LspSessions::Shared, 1), (LspSessions::PerCall, 2)] {
        let temp = tempfile::tempdir().unwrap();
        let (config, workspace, state, index) = shared_sessions_fixture(&temp);
        // The same manager is offered in both arms; only `shared` uses it.
        let manager = Arc::new(Manager::new(&config));
        for call in 1..=2 {
            let answer = traverse_add(&config, &workspace, &index, sessions, &manager).await;
            assert!(
                reached_run_through_lsp_live(&answer),
                "{sessions:?}, call {call}: no lsp.live edge to Run: {answer}"
            );
        }
        assert_eq!(
            state_count(&state, "spawn_count.txt"),
            spawned,
            "{sessions:?}: language servers started by two calls"
        );
        manager.shutdown_all().await;
    }
}

#[tokio::test]
async fn a_shared_traverse_server_outlives_the_walk_and_goes_with_release() {
    use local_code_intelligence::config::LspSessions;
    let server_of = |state: &std::path::Path| -> u32 {
        std::fs::read_to_string(state.join("pid.txt"))
            .expect("the fake server records its process id on its first request")
            .trim()
            .parse()
            .expect("a process id")
    };

    // `shared`: the walk is over, the server is not; releasing the workspace
    // of the shared manager stops it.
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shared_sessions_fixture(&temp);
    let manager = Arc::new(Manager::new(&config));
    let answer = traverse_add_past_a_cold_start(
        &config,
        &workspace,
        &index,
        &state,
        LspSessions::Shared,
        &manager,
    )
    .await;
    assert!(reached_run_through_lsp_live(&answer), "{answer}");
    let server = server_of(&state);
    assert!(
        process_is_running(server).await,
        "the shared server keeps running after the walk"
    );
    assert_eq!(manager.release(&workspace.id).await, ["csharp-ls"]);
    wait_until_process_ends(
        "the released shared server to end",
        server,
        DROP_KILL_DEADLINE,
    )
    .await;
    assert!(manager.release(&workspace.id).await.is_empty());

    // `per_call`: the server is the walk's own and ends with it, whatever
    // manager the call was offered.
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shared_sessions_fixture(&temp);
    let manager = Arc::new(Manager::new(&config));
    let answer = traverse_add_past_a_cold_start(
        &config,
        &workspace,
        &index,
        &state,
        LspSessions::PerCall,
        &manager,
    )
    .await;
    assert!(reached_run_through_lsp_live(&answer), "{answer}");
    wait_until_process_ends(
        "the per-call server to end with its walk",
        server_of(&state),
        DROP_KILL_DEADLINE,
    )
    .await;
    assert!(
        manager.release(&workspace.id).await.is_empty(),
        "the offered manager never held a session"
    );
}
