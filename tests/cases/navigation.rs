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

/// The fake-server fixtures (C#, TypeScript, Python, clangd) allow a request
/// timeout that a slow process start under load does not exceed
/// ([`LOADED_BUDGET_SECONDS`]): a timed-out `initialize` is a transport
/// failure, and its retry spawns a server the test did not count. A test about
/// a timeout sets its own, and gives the fake something that really outlasts it.
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
        lsp_timeout_seconds: LOADED_BUDGET_SECONDS,
        // Also bounds the version probe of `lsp.live` (5 s failed under load).
        readiness_timeout_seconds: LOADED_BUDGET_SECONDS,
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
        lsp_timeout_seconds: LOADED_BUDGET_SECONDS,
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
        lsp_timeout_seconds: LOADED_BUDGET_SECONDS,
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
        lsp_timeout_seconds: LOADED_BUDGET_SECONDS,
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
    // An 8 s timeout, not 1 s: it also covers the process start, which takes
    // seconds under load. Three notifications 3 s apart (9 s in all, for the
    // request after the priming one) outlast it in total while every gap stays
    // 5 s under it.
    let (mut config, workspace, _) = fake_csharp_fixture(&temp, "slow-with-progress");
    config.lsp_timeout_seconds = 8;
    config.csharp.args.extend(fake_args(&[
        "--progress-skip-priming",
        "--progress-count",
        "3",
        "--progress-gap-ms",
        "3000",
    ]));
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);

    let started = std::time::Instant::now();
    let results = manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .expect("progress notifications should keep the request alive past the 8 s timeout");
    let elapsed = started.elapsed();

    assert!(!results.is_empty());
    assert!(
        elapsed > Duration::from_secs(config.lsp_timeout_seconds),
        "expected the exchange to genuinely take longer than the nominal \
         8 s timeout (proving this isn't trivially fast); took {elapsed:?}"
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
    // Nothing here waits on the timeout: the first server exits, and the
    // replacement answers.
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
        // Only `timeout` is about the request timeout, and it keeps a short
        // one: 5 s, with a server whose `initialize` answer is more than 30 s
        // away, so the timeout always ends the wait. It is not 1 s because a
        // process that starts later than the timeout is killed before it
        // counts its start (the spawn count below). The other two end on their
        // own (a bad frame, an exit), and a slow process start under load must
        // not turn their `initialize` into a timeout, which is not retried.
        let (mut config, workspace, state) = fake_csharp_fixture(&temp, mode);
        if mode == "timeout" {
            config.lsp_timeout_seconds = 5;
            config
                .csharp
                .args
                .extend(fake_args(&["--initialize-delay-ms", "30000"]));
        }
        let manager = Manager::new(&config);
        let adapter = csharp_adapter(&manager);
        let error = manager
            .symbols(&adapter, &workspace, "Calculator")
            .await
            .unwrap_err();
        let message = format!("{error:#}");
        if mode == "timeout" {
            assert!(
                message.contains("timed out after 5 seconds"),
                "the 5 s bound must be what ended the wait: {message}"
            );
        }
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

    // The server is started and primed first (its answer is the error, whose
    // outcome does not matter here): a 300 ms call would otherwise give up while
    // a loaded machine is still starting the process, before any request has
    // been sent, and the server would never be sent the requests it is to read.
    let _ = manager.symbols(&adapter, &workspace, "Calculator").await;

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
        // initialize, initialized, the priming request, the warm-up request,
        // the eight abandoned ones, and at least one `$/cancelRequest`.
        async || state_count_so_far(&state, "request_count.txt") >= 13,
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
/// answers once, and fails again is not stuck. The result takes 4 s to arrive,
/// so the errors of the two failing calls are 6 s apart, past the 5 s limit,
/// while each failing call alone spans about 2 s, so a loaded machine has 3 s
/// to spare before one call could end the span by itself. Without the reset on
/// a result the third call would end that span and replace the session.
#[tokio::test]
async fn a_result_between_transient_errors_ends_the_stuck_span() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_stuck_seconds = 5;
    // Requests 2 to 4 (the first call and its repeats) fail, request 5 is
    // answered, requests 6 to 8 (the third call) fail.
    config.csharp.args.extend(fake_args(&[
        "--symbol-error",
        "-32801:content modified",
        "--symbol-ok-at",
        "5",
        "--symbol-ok-delay-ms",
        "4000",
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
    fake_client_observing(temp, extra, timeout, ready, Box::new(|_| false)).await
}

/// [`fake_client_with`] with a busy predicate too.
async fn fake_client_observing(
    temp: &tempfile::TempDir,
    extra: &[&str],
    timeout: Duration,
    ready: Box<dyn Fn(&serde_json::Value) -> bool + Send>,
    busy: Box<dyn Fn(&serde_json::Value) -> bool + Send>,
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
    let client = JsonRpcClient::spawn_with_busy(&transport, &[], ready, busy)
        .await
        .unwrap();
    // The request timeout of a test about a short one (2 s) must not also have
    // to cover the process start, which takes seconds on a loaded machine: the
    // server claims its start ordinal as soon as it runs.
    wait_until("the fake server to start", LOADED_BUDGET, async || {
        state.join("spawns").join("1").exists()
    })
    .await;
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
    // The 2 s write bound is what is under test, so it stays; the waits around
    // it are caps for a hang, as wide as a loaded machine needs. What shows
    // that the bound ended the write (and that the next write was refused
    // without waiting out a bound of its own) is the product's reason text.
    let started = std::time::Instant::now();
    let error = tokio::time::timeout(
        LOADED_BUDGET,
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
        started.elapsed() < LOADED_BUDGET,
        "the write bound ended the request after {:?}: {error:#}",
        started.elapsed()
    );

    let again = std::time::Instant::now();
    let later = tokio::time::timeout(
        LOADED_BUDGET,
        client.request("workspace/symbol", json!({"query": "x"})),
    )
    .await
    .expect("a write after a timed-out write must fail at once")
    .unwrap_err();
    // A write that waited out a bound of its own would say "did not take a
    // message within 2 seconds" instead.
    assert!(
        format!("{later:#}").contains("stopped reading its input"),
        "refused after {:?}: {later:#}",
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
    fn busy_predicate(&self) -> Box<dyn Fn(&Value) -> bool + Send + Sync> {
        Box::new(|message| message.get("method").and_then(Value::as_str) == Some("custom/busy"))
    }
    fn is_loading_error(&self, error: &RpcError, ready: bool) -> bool {
        !ready && error.code == RpcError::INTERNAL_ERROR && error.message.contains("file not found")
    }
}

/// `Manager::readiness` follows the transport's busy report end to end: the
/// fake sends ready, then busy, after `initialized` (`--status-cycle`) and
/// holds the answer to the priming request for 3 s, so the session reads
/// `busy` with two changes until that answer arrives, and `ready` with three
/// after the ready notification that follows it. The test waits on the
/// session's own report, not on a fixed time.
#[tokio::test]
async fn a_busy_signal_makes_the_session_report_busy_and_then_ready_again() {
    use local_code_intelligence::lsp::ServerStateName;

    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, _state) = fake_csharp_release_fixture(&temp);
    config.csharp.args.extend(fake_args(&[
        "--status-cycle",
        "--first-symbol-delay-ms",
        "3000",
    ]));
    let manager = Manager::new(&config);
    let adapter: Arc<dyn LspAdapter> = Arc::new(LoadingAwareAdapter(csharp_adapter(&manager)));
    let observe = async {
        let mut busy = None;
        wait_until("the session to report busy", LOADED_BUDGET, async || {
            busy = manager
                .readiness("csharp-ls", &workspace.id)
                .await
                .filter(|readiness| readiness.state == ServerStateName::Busy);
            busy.is_some()
        })
        .await;
        busy.unwrap()
    };
    let (symbols, busy) =
        tokio::join!(manager.symbols(&adapter, &workspace, "Calculator"), observe);
    symbols.unwrap();
    assert_eq!(busy.ready_changes, 2);
    assert!(busy.changed_ms.is_some());
    wait_until("the session to be ready again", LOADED_BUDGET, async || {
        manager
            .readiness("csharp-ls", &workspace.id)
            .await
            .is_some_and(|readiness| {
                readiness.state == ServerStateName::Ready && readiness.ready_changes == 3
            })
    })
    .await;
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
    tasklist_shows(
        pid,
        output.status.success(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

/// Reads one `tasklist` answer. `tasklist` exits 0 and prints an information
/// line when nothing matches, so a nonzero exit status or any text on stderr
/// is a failed query, which must not read as "the process ended": it panics
/// with what the program said.
#[cfg(windows)]
fn tasklist_shows(pid: u32, succeeded: bool, stdout: &str, stderr: &str) -> bool {
    assert!(
        succeeded && stderr.trim().is_empty(),
        "tasklist failed for pid {pid} (succeeded: {succeeded}); stderr: {stderr:?}; stdout: {stdout:?}"
    );
    stdout.contains(&format!("\"{pid}\""))
}

#[cfg(windows)]
#[test]
fn tasklist_answers_are_read_strictly() {
    let row = "\"fake.exe\",\"4242\",\"Console\",\"1\",\"3,000 K\"\n";
    assert!(tasklist_shows(4242, true, row, ""));
    assert!(!tasklist_shows(
        4242,
        true,
        "INFO: No tasks are running which match the specified criteria.\n",
        ""
    ));
    for (succeeded, stderr) in [(false, ""), (false, "ERROR: boom"), (true, "ERROR: boom")] {
        let outcome = std::panic::catch_unwind(|| tasklist_shows(4242, succeeded, row, stderr));
        assert!(
            outcome.is_err(),
            "a failed query ({succeeded}, {stderr:?}) must panic, not read as an answer"
        );
    }
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
    ps_shows(
        pid,
        output.status.code(),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
}

/// Reads one `ps -o stat= -p` answer. `ps` exits 0 with the state when the
/// process exists and exits 1 with no output when it does not; any other exit
/// status, or text on stderr, is a failed query, which must not read as "the
/// process ended": it panics with what the program said.
#[cfg(not(windows))]
fn ps_shows(pid: u32, code: Option<i32>, stdout: &str, stderr: &str) -> bool {
    let state = stdout.trim();
    let gone = code == Some(1) && state.is_empty();
    assert!(
        (code == Some(0) || gone) && stderr.trim().is_empty(),
        "ps failed for pid {pid} (exit code {code:?}); stderr: {stderr:?}; stdout: {stdout:?}"
    );
    !state.is_empty() && !state.starts_with('Z')
}

#[cfg(not(windows))]
#[test]
fn ps_answers_are_read_strictly() {
    assert!(ps_shows(4242, Some(0), "S\n", ""));
    assert!(!ps_shows(4242, Some(0), "Z\n", ""));
    assert!(!ps_shows(4242, Some(1), "", ""));
    for (code, stdout, stderr) in [
        (Some(2), "", "ps: bad usage"),
        (Some(1), "", "ps: error"),
        (Some(1), "S\n", ""),
        (None, "", ""),
        (Some(0), "S\n", "ps: warning"),
    ] {
        let outcome = std::panic::catch_unwind(|| ps_shows(4242, code, stdout, stderr));
        assert!(
            outcome.is_err(),
            "a failed query ({code:?}, {stdout:?}, {stderr:?}) must panic"
        );
    }
}

/// Waits until process `pid` has ended, and fails with `what` after `deadline`.
async fn wait_until_process_ends(what: &str, pid: u32, deadline: Duration) {
    wait_until(what, deadline, async || !process_is_running(pid).await).await;
}

/// How long the operating system and `tasklist` get to show a process as ended
/// once the call that stopped it has returned (a cap, not a duration: `tasklist`
/// itself takes seconds on a loaded machine).
#[cfg(windows)]
const PROCESS_END_SLACK: Duration = LOADED_BUDGET;

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
    // The sleeper holds the dead server's pipes, so the first request waits
    // out the inactivity timeout before the restart: 12 s, not the 30 s
    // loaded budget, like the sibling test below.
    config.lsp_timeout_seconds = 12;
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
    // The abort ends the call as cancelled; a panic in it is a failure.
    if let Err(error) = call.await {
        assert!(error.is_cancelled(), "the stalled call panicked: {error}");
    }
    drop(manager);
    for ordinal in 1..=state_count(state, "spawn_count.txt") {
        // Every spawned server starts a sleeper and writes its pid first; a
        // missing file is a sleeper that went unchecked, so it fails the test
        // once the bounded wait for it is over.
        let pid_file = state.join(format!("sleeper_{ordinal}.pid"));
        wait_until(
            &format!("sleeper {ordinal} to have written its pid file"),
            LOADED_BUDGET,
            async || pid_file.exists(),
        )
        .await;
        let text = std::fs::read_to_string(&pid_file)
            .unwrap_or_else(|error| panic!("read the pid file of sleeper {ordinal}: {error}"));
        wait_until_process_ends(
            &format!("sleeper {ordinal} to end once the manager is dropped"),
            text.trim()
                .parse()
                .unwrap_or_else(|_| panic!("a process id in the pid file of sleeper {ordinal}")),
            PROCESS_END_SLACK,
        )
        .await;
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
/// server's pipes open, so the request fails by the 12 s inactivity timeout
/// rather than at once, and the session is discarded. The replacement stalls
/// on the repeated request for 30 s, so the call holds the discarded session's
/// client (a local of its retry) until its own 12 s timeout runs out, about
/// 10 s after the discard's shutdown (a 2 s wait for the answer it never gets)
/// has ended the tree.
#[cfg(windows)]
#[tokio::test]
async fn a_discarded_server_loses_its_processes_while_a_request_still_holds_its_client() {
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, state) = fake_csharp_release_fixture(&temp);
    config.lsp_timeout_seconds = 12;
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
        2 * LOADED_BUDGET,
        async || state_count_so_far(&state, "request_count.txt") >= 8,
    )
    .await;
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);
    wait_until_process_ends(
        "the process the discarded server started to end while a request still holds its client",
        discarded,
        LOADED_BUDGET,
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

/// A C# fixture for the idle and release tests; since the base fixtures allow
/// [`LOADED_BUDGET_SECONDS`], this is the one with the idle and release
/// tests' mode. A test about the timeout sets its own.
fn fake_csharp_release_fixture(
    temp: &tempfile::TempDir,
) -> (Config, Workspace, std::path::PathBuf) {
    fake_csharp_fixture(temp, "workspace-symbols")
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
        2 * local_code_intelligence::lsp::SHUTDOWN_GRACE + LOADED_BUDGET,
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

/// The busy predicate feeds a report and nothing else: the fake sends ready,
/// then busy, and after the next request's answer ready again
/// (`--status-cycle`). The ready latch stays set through the busy period, and
/// every change is counted. Each step waits on the server's own notification,
/// never on a timer.
#[tokio::test]
async fn a_busy_signal_flips_the_state_and_counts_the_changes() {
    let temp = tempfile::tempdir().unwrap();
    let (client, _state) = fake_client_observing(
        &temp,
        &["--status-cycle"],
        Duration::from_secs(30),
        Box::new(|message| message["method"] == "custom/ready"),
        Box::new(|message| message["method"] == "custom/busy"),
    )
    .await;
    assert!(client.wait_for_changes(2, LOADED_BUDGET).await);
    assert!(client.is_busy());
    assert!(client.is_ready(), "the ready latch holds while busy");
    assert_eq!(client.state().changes, 2);

    client
        .request("workspace/symbol", json!({"query": "Calculator"}))
        .await
        .unwrap();
    assert!(client.wait_for_changes(3, LOADED_BUDGET).await);
    assert!(!client.is_busy());
    assert!(client.is_ready());
    assert_eq!(client.state().changes, 3);
    assert_eq!(client.progress_count(), 0);
}

/// `Manager::readiness` and `Status.analyzers`: `starting` while the fake takes
/// 3 s to answer `initialize` (the server's own delay; the test sleeps for
/// nothing), `ready` and primed after it, `failed` for a server that cannot be
/// started, and `null` for the six adapters with no session.
/// `analyzers_running` keeps its entries and values.
#[tokio::test]
async fn manager_readiness_reports_starting_then_ready_and_the_status_shows_it() {
    use local_code_intelligence::lsp::ServerStateName;

    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    write_csharp_workspace(&workspace);
    let app = Arc::new(
        App::open(with_fake_csharp_server(
            config,
            &state,
            &["--initialize-delay-ms", "3000"],
            LOADED_BUDGET_SECONDS,
        ))
        .await
        .unwrap(),
    );
    app.index(&workspace).await.unwrap();
    assert!(
        app.status(&workspace)
            .await
            .unwrap()
            .analyzers
            .values()
            .all(Option::is_none),
        "no session yet"
    );

    let call = tokio::spawn({
        let app = Arc::clone(&app);
        let workspace = workspace.clone();
        async move { app.symbols(&workspace, "Calculator").await }
    });
    // The session is registered as soon as the call has asked for it.
    let starting = loop {
        let status = app.status(&workspace).await.unwrap();
        if let Some(readiness) = status.analyzers["csharp-ls"].clone() {
            break readiness;
        }
        tokio::task::yield_now().await;
    };
    assert_eq!(starting.state, ServerStateName::Starting);
    assert!(!starting.primed);
    call.await.unwrap().unwrap();

    let status = app.status(&workspace).await.unwrap();
    let ready = status.analyzers["csharp-ls"].clone().expect("a session");
    assert_eq!(ready.state, ServerStateName::Ready);
    assert!(ready.primed, "a symbols call waits for the priming");
    assert!(ready.age_ms > 0);
    assert_eq!(
        status.analyzers.keys().collect::<Vec<_>>(),
        status.analyzers_running.keys().collect::<Vec<_>>(),
        "one entry per adapter, like analyzers_running"
    );
    for (provider, report) in &status.analyzers {
        assert_eq!(report.is_some(), provider == "csharp-ls", "{provider}");
    }
    assert!(status.analyzers_running["csharp-ls"]);
    assert_eq!(
        status
            .analyzers_running
            .values()
            .filter(|running| **running)
            .count(),
        1
    );
    let wire = serde_json::to_value(&status).unwrap();
    assert_eq!(wire["analyzers"]["csharp-ls"]["state"], "ready", "{wire}");
    assert!(wire["analyzers"]["rust-analyzer"].is_null(), "{wire}");
    assert_eq!(wire["analyzers"].as_object().unwrap().len(), 7, "{wire}");
    models_task.abort();

    // A server that cannot be started is `failed`.
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, _state) = fake_csharp_release_fixture(&temp);
    config.csharp.path = Some("definitely-missing-csharp-ls".to_owned());
    let manager = Manager::new(&config);
    let adapter = csharp_adapter(&manager);
    assert!(
        manager
            .readiness("csharp-ls", &workspace.id)
            .await
            .is_none()
    );
    call_site_definition(&manager, &adapter, &workspace)
        .await
        .expect_err("the server does not exist");
    let failed = manager
        .readiness("csharp-ls", &workspace.id)
        .await
        .expect("the failed session stays registered");
    assert_eq!(failed.state, ServerStateName::Failed);
    assert!(!manager.running("csharp-ls", &workspace.id).await);
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
    outline_fixture_with(
        temp,
        outline_cache,
        local_code_intelligence::config::LspSync::Off,
    )
}

/// [`outline_fixture`] under `[run_profile.lsp] sync`.
fn outline_fixture_with(
    temp: &tempfile::TempDir,
    outline_cache: bool,
    sync: local_code_intelligence::config::LspSync,
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
        LOADED_BUDGET_SECONDS,
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
    config.run_profile.lsp.sync = sync;
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

/// Under `outline_cache` and `[run_profile.lsp] sync = "watch"` (d424), a
/// synced change drops the file's kept outline: an edit is answered with the
/// new outline, and even the same bytes written again (a change the server
/// was told about) are asked again instead of answered from memory.
#[tokio::test]
async fn a_synced_change_drops_the_files_kept_outline() {
    use local_code_intelligence::config::LspSync;
    let temp = tempfile::tempdir().unwrap();
    let (manager, workspace, states) = outline_fixture_with(&temp, true, LspSync::Watch);
    let python = python_adapter(&manager);
    let calculator = "src/calculator.py";
    let first = ["Calculator", "add"];
    for _ in 0..2 {
        assert_eq!(
            outline_names(&manager, &python, &workspace, calculator).await,
            first
        );
    }
    assert_eq!(outline_requests(&states.python, calculator), 1, "kept");
    // The sync started with the session and has walked the tree.
    assert!(manager.synced(&workspace.id, &[], LOADED_BUDGET).await);

    // An edit, synced: the new outline.
    let edited = format!("{CALCULATOR_PY}    def twice(self, a):\n        return a + a\n");
    write(&workspace.path, calculator, &edited);
    assert!(
        manager
            .synced(&workspace.id, &[calculator], LOADED_BUDGET)
            .await
    );
    let second = ["Calculator", "add", "twice"];
    for _ in 0..2 {
        assert_eq!(
            outline_names(&manager, &python, &workspace, calculator).await,
            second
        );
    }
    assert_eq!(outline_requests(&states.python, calculator), 2);

    // The same bytes again, with a modification time that is certainly new
    // (a rewrite within one clock tick could keep the old one): the sync tells
    // the server, the kept outline goes, and the server is asked.
    write(&workspace.path, calculator, &edited);
    std::fs::File::options()
        .write(true)
        .open(workspace.path.join(calculator))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(3600))
        .unwrap();
    assert!(
        manager
            .synced(&workspace.id, &[calculator], LOADED_BUDGET)
            .await
    );
    assert_eq!(
        outline_names(&manager, &python, &workspace, calculator).await,
        second
    );
    assert_eq!(
        outline_requests(&states.python, calculator),
        3,
        "a synced change is asked again"
    );
    manager.shutdown_all().await;
}

// ---- `lsp.live` under `[run_profile.lsp] sessions` (K7b-1) -----------------

/// A SCIP index over the C# fixture of [`fake_csharp_fixture`]: `Add`
/// (`src/Calculator.cs`, line 1) and `Run` (`src/CallSite.cs`, line 1), whose
/// text calls `Add`. It lives next to the workspace, never in it. The graph
/// has no call edge: the only way from `Add` to `Run` is `lsp.live`'s
/// `textDocument/references`.
fn csharp_scip_index(temp: &tempfile::TempDir, workspace: &Workspace) -> std::path::PathBuf {
    csharp_scip_index_of(
        &temp.path().join("index.scip"),
        workspace,
        &[
            ("src/Calculator.cs", 1, "Add"),
            ("src/CallSite.cs", 1, "Run"),
        ],
    )
}

/// A SCIP index at `index` that defines each `(file, zero-based line, name)`
/// as a function whose body is that whole line, as the tree is now.
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn csharp_scip_index_of(
    index: &std::path::Path,
    workspace: &Workspace,
    functions: &[(&str, usize, &str)],
) -> std::path::PathBuf {
    use scip::types::{Document, Index, Occurrence, SymbolInformation, symbol_information::Kind};
    let symbol = |name: &str| format!("rust-analyzer cargo demo 0.1.0 acceptance/{name}().");
    let mut documents: Vec<Document> = Vec::new();
    for (file, line_number, name) in functions {
        let text = std::fs::read_to_string(workspace.path.join(file)).unwrap();
        let line = text.lines().nth(*line_number).unwrap();
        let (row, col) = (*line_number as i32, line.find(name).unwrap() as i32);
        let mut definition = Occurrence::default();
        definition.range = vec![row, col, col + name.len() as i32];
        definition.symbol = symbol(name);
        definition.symbol_roles = 1;
        definition.enclosing_range = vec![row, 0, row, line.len() as i32];
        let mut info = SymbolInformation::default();
        info.symbol = symbol(name);
        info.kind = Kind::Function.into();
        let document = match documents.iter_mut().find(|d| d.relative_path == *file) {
            Some(document) => document,
            None => {
                let mut document = Document::default();
                document.relative_path = (*file).to_owned();
                documents.push(document);
                documents.last_mut().unwrap()
            }
        };
        document.occurrences.push(definition);
        document.symbols.push(info);
    }
    let mut message = Index::default();
    message.documents = documents;
    scip::write_message_to_file(index, message).unwrap();
    index.to_owned()
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

/// [`shared_sessions_fixture`] with `[run_profile.lsp] sync = "watch"`. The
/// default `sessions = "shared"` keeps a server across walks only when it sees
/// an edit on its own (`LspAdapter::watches_workspace`: rust-analyzer) or when
/// the sync tells it. csharp-ls is a non-watching adapter, so without `watch`
/// a walk starts its own csharp-ls, as under `per_call`; the tests that mean a
/// server shared across calls use this fixture.
fn watching_sessions_fixture(
    temp: &tempfile::TempDir,
) -> (Config, Workspace, std::path::PathBuf, std::path::PathBuf) {
    let (mut config, workspace, state, index) = shared_sessions_fixture(temp);
    config.run_profile.lsp.sync = local_code_intelligence::config::LspSync::Watch;
    (config, workspace, state, index)
}

/// The index's file list for the tree barrier, fixed when the manager is
/// built: the snapshot of the workspace as the fixture wrote it.
struct SnapshotFiles(Arc<Vec<String>>);

#[lci_maps::async_trait]
impl local_code_intelligence::lsp::stamp::IndexedFiles for SnapshotFiles {
    async fn indexed_files(&self, _workspace: &Workspace) -> Option<Arc<Vec<String>>> {
        Some(Arc::clone(&self.0))
    }
}

/// A manager of `config` that has `workspace`'s index snapshot, as the
/// service's has: without one the tree barrier restarts every shared session
/// it is asked about (no sharing without a stamp).
fn indexed_manager(config: &Config, workspace: &Workspace) -> Arc<Manager> {
    indexed_manager_listing(config, workspace, &[])
}

/// [`indexed_manager`] whose snapshot also lists `extra` (paths the disk may
/// not have, or refuses).
fn indexed_manager_listing(config: &Config, workspace: &Workspace, extra: &[&str]) -> Arc<Manager> {
    let files = local_code_intelligence::chunk::source_files(&workspace.path, true)
        .unwrap()
        .into_iter()
        .map(|file| file.relative)
        .chain(extra.iter().map(|path| (*path).to_owned()))
        .collect();
    Arc::new(Manager::new(config).with_indexed_files(Arc::new(SnapshotFiles(Arc::new(files)))))
}

/// Gives every file and directory under `root` (and `root`) an old
/// modification time, so that a change made after it cannot share its clock
/// tick (a directory's modification time moves when an entry is added).
fn age_tree(root: &std::path::Path) {
    fn set_modified(path: &std::path::Path, time: std::time::SystemTime) {
        let mut options = std::fs::OpenOptions::new();
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            // FILE_FLAG_BACKUP_SEMANTICS opens a directory.
            options.write(true).custom_flags(0x0200_0000);
        }
        #[cfg(not(windows))]
        options.read(true);
        options.open(path).unwrap().set_modified(time).unwrap();
    }
    fn visit(path: &std::path::Path, time: std::time::SystemTime) {
        if path.is_dir() {
            for entry in std::fs::read_dir(path).unwrap() {
                visit(&entry.unwrap().path(), time);
            }
        }
        set_modified(path, time);
    }
    visit(
        root,
        std::time::SystemTime::now() - std::time::Duration::from_secs(3600),
    );
}

/// Whether the walk noted that the tree barrier restarted `provider`'s kept
/// session.
fn restart_noted(answer: &Value, provider: &str) -> bool {
    let wanted = format!("lsp.live: {provider} restarted: the tree changed since its last call");
    answer["notes"].as_array().is_some_and(|notes| {
        notes
            .iter()
            .any(|note| note.as_str() == Some(wanted.as_str()))
    })
}

/// Whether the walk labelled its answer as coming from a kept session of
/// `provider`, whose tree stamp has not changed (the answer of a reused server
/// is not a fresh server's).
fn kept_noted(answer: &Value, provider: &str) -> bool {
    let wanted = format!("lsp.live: {provider} kept session; tree stamp unchanged since ");
    answer["notes"].as_array().is_some_and(|notes| {
        notes
            .iter()
            .any(|note| note.as_str().is_some_and(|note| note.starts_with(&wanted)))
    })
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
        // Stamped first, as the service's warm start does: a session started
        // by a call that passes no barrier has no stamp and would be
        // restarted by the walk.
        manager.tree_barrier(workspace, &[&adapter]).await;
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

/// `config` as a service that warms the language servers at the first touch of
/// a workspace: shared sessions and a traverse policy that adds `lsp.live`.
/// `sync = "watch"` because csharp-ls is a non-watching adapter: under
/// `shared` alone a walk starts its own csharp-ls and the warm start primes
/// nothing for it (see [`watching_sessions_fixture`]); `watch` keeps it shared.
fn with_lsp_live_warm_start(mut config: Config) -> Config {
    config.run_profile.lsp.sessions = local_code_intelligence::config::LspSessions::Shared;
    config.run_profile.lsp.sync = local_code_intelligence::config::LspSync::Watch;
    config.traverse.enabled = true;
    config.traverse.policies.insert(
        "explore".to_owned(),
        local_code_intelligence::config::TraversePolicy {
            extra_maps: Some(vec![lci_maps::lsp_live::LSP_LIVE_ID.to_owned()]),
            ..Default::default()
        },
    );
    config
}

/// The finished outcome of the warm start `app` began for `workspace_id`,
/// waiting at most [`LOADED_BUDGET`] on the signal itself.
async fn finished_warm_start(
    app: &App,
    workspace_id: &str,
) -> Arc<local_code_intelligence::app::WarmOutcome> {
    let mut warm = app
        .warm_start(workspace_id)
        .expect("the first touch began a warm start");
    let finished = tokio::time::timeout(LOADED_BUDGET, warm.wait_for(Option::is_some))
        .await
        .expect("the warm start finishes within the loaded budget")
        .expect("the warm start was not dropped");
    finished.clone().expect("a finished warm start")
}

/// A service that warms its language servers (see [`with_lsp_live_warm_start`])
/// over the `documents`-mode fake, with the C# workspace indexed (a warm start
/// waits for an index). `extra_args` are more fake-server flags; `tweak`
/// changes the configuration before the service opens.
struct WarmService {
    temp: tempfile::TempDir,
    config: Config,
    app: App,
    workspace: Workspace,
    state: std::path::PathBuf,
    _fake: Arc<FakeModels>,
    models_task: tokio::task::JoinHandle<()>,
}

async fn indexed_warm_service(extra_args: &[&str], tweak: impl FnOnce(&mut Config)) -> WarmService {
    let (temp, config, fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let path = temp.path().join("workspace");
    write_csharp_workspace(&path);
    let mut config = with_lsp_live_warm_start(with_fake_documents_server(
        config,
        &state,
        extra_args,
        LOADED_BUDGET_SECONDS,
    ));
    config.rust_analyzer_path = "definitely-missing-rust-analyzer".into();
    tweak(&mut config);
    let app = App::open(config.clone()).await.unwrap();
    let workspace = app.index(&path).await.unwrap().workspace;
    WarmService {
        temp,
        config,
        app,
        workspace,
        state,
        _fake: fake,
        models_task,
    }
}

#[tokio::test]
async fn a_shared_service_warms_the_servers_at_the_first_status_call() {
    use local_code_intelligence::lsp::ServerStateName;

    // A server that takes 30 s to answer `initialize`: `status` returns at
    // once (it does not wait for the warm start) and shows it `starting`.
    let held = indexed_warm_service(&["--initialize-delay-ms", "30000"], |_| {}).await;
    let (app, workspace) = (&held.app, &held.workspace);
    let began = std::time::Instant::now();
    let status = app.status(&workspace.path).await.unwrap();
    assert!(
        began.elapsed() < std::time::Duration::from_secs(15),
        "the first status call waited for the warm start: {:?}",
        began.elapsed()
    );
    let starting = tokio::time::timeout(LOADED_BUDGET, async {
        loop {
            let status = app.status(&workspace.path).await.unwrap();
            if let Some(readiness) = status.analyzers["csharp-ls"].clone() {
                break readiness;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the warm start begins to start the server");
    assert_eq!(starting.state, ServerStateName::Starting);
    assert!(!starting.primed);
    assert!(
        app.warm_start(&status.workspace.id)
            .unwrap()
            .borrow()
            .is_none(),
        "the warm start is still running"
    );
    app.shutdown_language_servers().await;
    held.models_task.abort();

    // A server that starts: the warm start finishes with an `Ok` for it, the
    // session is primed, and the first walk reuses it.
    let warm = indexed_warm_service(&[], |_| {}).await;
    let (app, workspace) = (&warm.app, &warm.workspace);
    let index = csharp_scip_index(&warm.temp, workspace);
    let id = app.status(&workspace.path).await.unwrap().workspace.id;
    let outcome = finished_warm_start(app, &id).await;
    assert!(outcome["csharp-ls"].is_ok(), "{outcome:?}");
    let ready = app.status(&workspace.path).await.unwrap().analyzers["csharp-ls"]
        .clone()
        .expect("a session");
    assert!(ready.primed, "{ready:?}");
    assert_eq!(state_count(&warm.state, "spawn_count.txt"), 1);
    let answer = traverse_add(
        &warm.config,
        workspace,
        &index,
        local_code_intelligence::config::LspSessions::Shared,
        &app.language_servers(),
    )
    .await;
    assert!(reached_run_through_lsp_live(&answer), "{answer}");
    assert_eq!(
        state_count(&warm.state, "spawn_count.txt"),
        1,
        "the walk reused the warmed server, and a second status call began no second warm start"
    );
    app.shutdown_language_servers().await;
    warm.models_task.abort();
}

#[tokio::test]
async fn releasing_a_workspace_stops_its_pending_warm_start() {
    // The server is still starting (30 s `initialize`) when the workspace is
    // released: the release removes the session and the warm task creates
    // none afterwards, whether the workspace is touched again or not.
    let held = indexed_warm_service(&["--initialize-delay-ms", "30000"], |_| {}).await;
    let (app, workspace) = (&held.app, &held.workspace);
    app.status(&workspace.path).await.unwrap();
    tokio::time::timeout(LOADED_BUDGET, async {
        loop {
            let status = app.status(&workspace.path).await.unwrap();
            if status.analyzers["csharp-ls"].is_some() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the warm start begins to start the server");
    let released = app.release_language_servers(&workspace.path).await.unwrap();
    assert_eq!(released.released, ["csharp-ls"]);
    for _ in 0..50 {
        tokio::task::yield_now().await;
    }
    let status = app.status(&workspace.path).await.unwrap();
    assert!(
        status.analyzers.values().all(Option::is_none),
        "a released workspace has no session, and its next touch warms nothing: {:?}",
        status.analyzers
    );
    held.models_task.abort();
}

#[tokio::test]
async fn overlapping_stops_all_wait_for_the_pending_warm_start() {
    // Two releases, then a release with a shutdown, race over one warm task
    // held by a 30 s `initialize`: whichever returns, the task is gone and
    // left no session, and exactly one server was ever started.
    for shutdown_too in [false, true] {
        let held = indexed_warm_service(&["--initialize-delay-ms", "30000"], |_| {}).await;
        let (app, workspace) = (&held.app, &held.workspace);
        app.status(&workspace.path).await.unwrap();
        tokio::time::timeout(LOADED_BUDGET, async {
            loop {
                let status = app.status(&workspace.path).await.unwrap();
                if status.analyzers["csharp-ls"].is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("the warm start begins to start the server");
        if shutdown_too {
            let (release, _) = tokio::join!(
                app.release_language_servers(&workspace.path),
                app.shutdown_language_servers()
            );
            release.unwrap();
        } else {
            let (first, second) = tokio::join!(
                app.release_language_servers(&workspace.path),
                app.release_language_servers(&workspace.path)
            );
            first.unwrap();
            second.unwrap();
        }
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        let status = app.status(&workspace.path).await.unwrap();
        assert!(
            status.analyzers.values().all(Option::is_none),
            "shutdown_too {shutdown_too}: a session survived: {:?}",
            status.analyzers
        );
        assert!(
            state_count_so_far(&held.state, "spawn_count.txt") <= 1,
            "shutdown_too {shutdown_too}: more than one server started"
        );
        held.models_task.abort();
    }
}

#[tokio::test]
async fn a_release_before_the_first_status_leaves_no_warm_start() {
    // The release finds no warm start and reserves the workspace: the status
    // call racing it (here: right after it) begins none, so no server starts
    // after the release returned.
    let held = indexed_warm_service(&[], |_| {}).await;
    let (app, workspace) = (&held.app, &held.workspace);
    assert!(
        app.release_language_servers(&workspace.path)
            .await
            .unwrap()
            .released
            .is_empty()
    );
    let status = app.status(&workspace.path).await.unwrap();
    assert!(app.warm_start(&status.workspace.id).is_none());
    app.search(&workspace.path, "Calculator", Some(3))
        .await
        .unwrap();
    assert!(app.warm_start(&status.workspace.id).is_none());
    assert!(
        status.analyzers.values().all(Option::is_none),
        "{:?}",
        status.analyzers
    );
    held.models_task.abort();
}

#[tokio::test]
async fn the_warm_start_skips_adapters_for_languages_the_index_lacks() {
    // A workspace of C# files only, with a second fake server configured for
    // TypeScript: the warm start primes csharp-ls and never starts the other.
    let typescript_state = tempfile::tempdir().unwrap();
    let state = typescript_state.path().to_owned();
    let service = indexed_warm_service(&[], move |config| {
        config.typescript = TypeScriptLspConfig {
            path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
            args: vec![
                "--mode".to_owned(),
                "workspace-symbols".to_owned(),
                "--state".to_owned(),
                state.to_string_lossy().into_owned(),
            ],
            disabled: false,
        };
    })
    .await;
    let id = service
        .app
        .status(&service.workspace.path)
        .await
        .unwrap()
        .workspace
        .id;
    let outcome = finished_warm_start(&service.app, &id).await;
    assert_eq!(
        outcome.keys().copied().collect::<Vec<_>>(),
        ["csharp-ls"],
        "{outcome:?}"
    );
    assert!(outcome["csharp-ls"].is_ok(), "{outcome:?}");
    assert!(
        !typescript_state.path().join("spawn_count.txt").exists(),
        "the TypeScript server was started for a workspace with no TypeScript"
    );
    service.app.shutdown_language_servers().await;
    service.models_task.abort();
}

#[tokio::test]
async fn no_warm_start_under_per_call_or_without_an_lsp_live_policy() {
    use local_code_intelligence::config::LspSessions;

    for (label, tweak) in [
        (
            "per_call sessions",
            Box::new(|config: &mut Config| config.run_profile.lsp.sessions = LspSessions::PerCall)
                as Box<dyn FnOnce(&mut Config)>,
        ),
        (
            "no policy naming lsp.live",
            Box::new(|config: &mut Config| config.traverse.policies.clear()),
        ),
    ] {
        let service = indexed_warm_service(&[], tweak).await;
        let status = service.app.status(&service.workspace.path).await.unwrap();
        service
            .app
            .search(&service.workspace.path, "Calculator", Some(3))
            .await
            .unwrap();
        assert!(
            service.app.warm_start(&status.workspace.id).is_none(),
            "{label}: no warm start began"
        );
        assert!(
            status.analyzers["csharp-ls"].is_none(),
            "{label}: no server was started: {:?}",
            status.analyzers
        );
        service.app.shutdown_language_servers().await;
        service.models_task.abort();
    }

    // The warm configuration: neither `status` before an index nor `index`
    // is a touch; `status` and `search` over an index are.
    let (temp, config, _fake, models_task) = fixture().await;
    let state = temp.path().join("state");
    let workspace = temp.path().join("workspace");
    write_csharp_workspace(&workspace);
    let mut config = with_lsp_live_warm_start(with_fake_csharp_server(
        config,
        &state,
        &[],
        LOADED_BUDGET_SECONDS,
    ));
    config.rust_analyzer_path = "definitely-missing-rust-analyzer".into();
    let app = App::open(config).await.unwrap();
    let id = app.status(&workspace).await.unwrap().workspace.id;
    assert!(app.warm_start(&id).is_none(), "no index yet: no touch");
    app.index(&workspace).await.unwrap();
    assert!(app.warm_start(&id).is_none(), "an index build is no touch");
    app.search(&workspace, "Calculator", Some(3)).await.unwrap();
    let outcome = finished_warm_start(&app, &id).await;
    assert!(outcome["csharp-ls"].is_ok(), "{outcome:?}");
    app.shutdown_language_servers().await;
    models_task.abort();
}

#[tokio::test]
async fn two_traverse_calls_share_one_language_server_under_shared_sessions() {
    use local_code_intelligence::config::LspSessions;
    for (sessions, spawned) in [(LspSessions::Shared, 1), (LspSessions::PerCall, 2)] {
        let temp = tempfile::tempdir().unwrap();
        let (config, workspace, state, index) = watching_sessions_fixture(&temp);
        // The same manager is offered in both arms; only `shared` uses it.
        let manager = indexed_manager(&config, &workspace);
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
    let (config, workspace, state, index) = watching_sessions_fixture(&temp);
    let manager = indexed_manager(&config, &workspace);
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
    let manager = indexed_manager(&config, &workspace);
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

// ---- `lsp.live` implementation edges (K9a) ---------------------------------

/// At an interface member the documents-mode fake answers the same-named
/// members of the implementing types, in document order; at an implementing
/// member, nothing. (At a type name it answers its subtypes, as before.)
#[tokio::test]
async fn documents_fake_answers_implementation_at_an_interface_member() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    write(&root, "src/Shapes.cs", SHAPES_CS);
    let fake = DocumentsFake::start_on(&temp, root).await;
    let lines: Vec<&str> = SHAPES_CS.lines().collect();
    let area = |line: usize| (line as u32, lines[line].find("Area").unwrap() as u32);

    let at_interface = fake
        .at("textDocument/implementation", "src/Shapes.cs", area(0))
        .await;
    assert_eq!(
        fake.places(&at_interface),
        [
            place("src/Shapes.cs", area(1)),
            place("src/Shapes.cs", area(2))
        ]
    );
    let at_circle = fake
        .at("textDocument/implementation", "src/Shapes.cs", area(1))
        .await;
    assert!(fake.places(&at_circle).is_empty(), "{at_circle}");
    fake.finish().await;
}

/// A SCIP index over `src/Shapes.cs` ([`SHAPES_CS`]) with the three `Area`
/// methods (`IShape`, `Circle`, `Square`), one per line. The three share a
/// name, kind and file, so their node ids differ by the disambiguator, in
/// graph order: `Area/method`, `Area/method~1`, `Area/method~2`. It lives next
/// to the workspace, never in it.
#[allow(clippy::field_reassign_with_default)] // the generated messages have no builders
fn shapes_scip_index(temp: &tempfile::TempDir) -> std::path::PathBuf {
    use scip::types::{Document, Index, Occurrence, SymbolInformation, symbol_information::Kind};
    let mut document = Document::default();
    document.relative_path = "src/Shapes.cs".to_owned();
    for (line, owner) in ["IShape", "Circle", "Square"].into_iter().enumerate() {
        let text = SHAPES_CS.lines().nth(line).unwrap();
        let col = text.find("Area").unwrap() as i32;
        let symbol = format!("rust-analyzer cargo demo 0.1.0 acceptance/{owner}#Area().");
        let mut occurrence = Occurrence::default();
        occurrence.range = vec![line as i32, col, col + 4];
        occurrence.symbol = symbol.clone();
        occurrence.symbol_roles = 1;
        occurrence.enclosing_range = vec![line as i32, 0, line as i32, text.len() as i32];
        document.occurrences.push(occurrence);
        let mut info = SymbolInformation::default();
        info.symbol = symbol;
        info.kind = Kind::Method.into();
        document.symbols.push(info);
    }
    let mut message = Index::default();
    message.documents = vec![document];
    let index = temp.path().join("shapes.scip");
    scip::write_message_to_file(&index, message).unwrap();
    index
}

/// The C# fixture with `src/Shapes.cs` written into the workspace and the
/// `documents`-mode fake (a request timeout a slow start under load does not
/// exceed; rust-analyzer is pointed at a binary that does not exist).
fn shapes_fixture(
    temp: &tempfile::TempDir,
) -> (Config, Workspace, std::path::PathBuf, std::path::PathBuf) {
    let (mut config, workspace, state) = fake_csharp_fixture(temp, "documents");
    config.lsp_timeout_seconds = LOADED_BUDGET_SECONDS;
    config.rust_analyzer_path = "definitely-missing-rust-analyzer".into();
    write(&workspace.path, "src/Shapes.cs", SHAPES_CS);
    let index = shapes_scip_index(temp);
    (config, workspace, state, index)
}

/// What the documents fake answers for `textDocument/implementation` at the
/// `Area` of `IShape`, asked raw (0-based line, column).
async fn raw_implementations_of_area(workspace: &Workspace) -> Vec<(String, u64, u64)> {
    let temp = tempfile::tempdir().unwrap();
    let fake = DocumentsFake::start_on(&temp, workspace.path.clone()).await;
    let answer = fake
        .at(
            "textDocument/implementation",
            "src/Shapes.cs",
            position_of(SHAPES_CS, "Area"),
        )
        .await;
    let places = fake.places(&answer);
    fake.finish().await;
    places
}

/// How many requests of `method` the fake server at `state` received.
fn requests_of(state: &std::path::Path, method: &str) -> usize {
    std::fs::read_to_string(state.join("events.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap())
        .filter(|event| event["method"] == method)
        .count()
}

const AREA_SEED: &str = "sym:rust:Shapes.cs::Area/method";

/// One `lsp.live` walk of the seed alone (`max_depth` 1: the seed is expanded
/// once, what it reaches is not), under `profile`.
async fn traverse_area(
    config: &Config,
    workspace: &Workspace,
    index: &std::path::Path,
    profile: &local_code_intelligence::config::RunProfileConfig,
) -> local_code_intelligence::app::traverse::Report {
    let maps = [lci_maps::lsp_live::LSP_LIVE_ID.to_owned()];
    traverse_area_routed(config, workspace, index, profile, Some(&maps), None, &[]).await
}

/// [`traverse_area`] with the maps and the policy left to the caller: `maps`
/// as `--maps` (none: the call's policy picks them), `policy` as
/// `steer.policy`, and `routes` as `[traverse.policies.<name>] extra_maps`.
async fn traverse_area_routed(
    config: &Config,
    workspace: &Workspace,
    index: &std::path::Path,
    profile: &local_code_intelligence::config::RunProfileConfig,
    maps: Option<&[String]>,
    policy: Option<&str>,
    routes: &[(&str, &[&str])],
) -> local_code_intelligence::app::traverse::Report {
    use local_code_intelligence::{
        app::traverse::{BudgetArgs, Call, Dirs, SteerArgs, run},
        config::{TraverseConfig, TraverseFirstStage},
    };
    let mut limits = TraverseConfig {
        enabled: true,
        first_stage: TraverseFirstStage::Off,
        answer_seeds: false,
        exact_seeds: false,
        deadline_ms: 60_000,
        ..TraverseConfig::default()
    };
    let (budget, steer) = (
        BudgetArgs {
            deadline_ms: Some(60_000),
            max_depth: Some(1),
            ..BudgetArgs::default()
        },
        SteerArgs {
            policy: policy.map(str::to_owned),
            ..SteerArgs::default()
        },
    );
    for (name, extra_maps) in routes {
        limits.policies.insert(
            (*name).to_owned(),
            lci_core::config::TraversePolicy {
                extra_maps: Some(extra_maps.iter().map(|id| (*id).to_owned()).collect()),
                ..lci_core::config::TraversePolicy::default()
            },
        );
    }
    let seeds = [AREA_SEED.to_owned()];
    let call = Call {
        scip_index: index,
        scip_root: Some(&workspace.path),
        question: "who implements Area",
        seeds: &seeds,
        seed_search: None,
        top_k: 8,
        maps,
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: Some(config),
        lsp_sessions: None,
    };
    run(&limits, profile, &Dirs::under(&config.data_dir), &call)
        .await
        .unwrap()
}

/// The destinations of the `lsp.live` hops out of the seed in a trace. A hop
/// carries the best edge's kind only, and the documents fake answers
/// `definition` by simple name (it names the implementing members too), so
/// the walk cannot tell the kinds apart here; the edges are checked in
/// `lsp_live_implements_edges_sit_at_the_places_the_server_answered`.
fn lsp_live_hops(report: &local_code_intelligence::app::traverse::Report) -> Vec<String> {
    let events = lci_maps::traverse::trace::read_trace(report.trace.as_ref().unwrap()).unwrap();
    let mut hops: Vec<String> = events
        .iter()
        .filter(|event| {
            event["ev"] == "hop" && event["map"] == "lsp.live" && event["src"] == AREA_SEED
        })
        .map(|event| event["dst"].as_str().unwrap().to_owned())
        .collect();
    hops.sort();
    hops
}

/// The ceiling of the implementation request: with the key on, a walk of the
/// interface member asks the server `textDocument/implementation` once, pays
/// the third external call, and reaches the places the server answers; with
/// it off (the default) the same walk asks none and pays two.
#[tokio::test]
async fn lsp_live_implementations_ask_once_and_pay_the_third_call() {
    use local_code_intelligence::config::{LspProfile, RunProfileConfig};
    let profile = |implementations| RunProfileConfig {
        lsp: LspProfile {
            implementations,
            ..LspProfile::default()
        },
        ..RunProfileConfig::default()
    };

    // The server's own answer: Circle.Area and Square.Area, at lines 1 and 2.
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shapes_fixture(&temp);
    let raw = raw_implementations_of_area(&workspace).await;
    let lines: Vec<&str> = SHAPES_CS.lines().collect();
    let area = |line: usize| lines[line].find("Area").unwrap() as u64;
    assert_eq!(
        raw,
        [
            ("src/Shapes.cs".to_owned(), 1, area(1)),
            ("src/Shapes.cs".to_owned(), 2, area(2))
        ]
    );

    // Key on: the destinations are the two implementing members, no other.
    let report = traverse_area(&config, &workspace, &index, &profile(true)).await;
    assert_eq!(
        lsp_live_hops(&report),
        [
            "sym:rust:Shapes.cs::Area/method~1",
            "sym:rust:Shapes.cs::Area/method~2"
        ],
        "{}",
        report.response
    );
    assert_eq!(
        requests_of(&state, "textDocument/implementation"),
        1,
        "one implementation request for the one expansion"
    );
    assert_eq!(requests_of(&state, "textDocument/definition"), 1);
    assert_eq!(requests_of(&state, "textDocument/references"), 1);
    assert_eq!(report.response["totals"]["external_calls"], 3);
    // Each reached card sits where the server said (the cards' sites count
    // from 1).
    let cards = report.response["results"].as_array().unwrap();
    let sites: Vec<(String, u64, u64)> = raw
        .iter()
        .map(|(path, line, col)| (path.clone(), line + 1, col + 1))
        .collect();
    let mut reached: Vec<(String, u64, u64)> = cards
        .iter()
        .filter(|card| card.to_string().contains("lsp.live"))
        .map(|card| {
            let site = &card["site"];
            (
                site["path"].as_str().unwrap().to_owned(),
                site["line"].as_u64().unwrap(),
                site["col"].as_u64().unwrap(),
            )
        })
        .collect();
    reached.sort();
    assert_eq!(reached, sites, "{}", report.response);

    // Key off: the same walk makes no implementation request.
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shapes_fixture(&temp);
    let report = traverse_area(&config, &workspace, &index, &profile(false)).await;
    // The hops are the same: the fake's `definition` names the same places and
    // `CallsOrRefs` wins the tie. The request count and `external_calls` are
    // what tell the two runs apart.
    assert_eq!(
        lsp_live_hops(&report),
        [
            "sym:rust:Shapes.cs::Area/method~1",
            "sym:rust:Shapes.cs::Area/method~2"
        ],
        "{}",
        report.response
    );
    assert_eq!(report.response["totals"]["external_calls"], 2);
    assert_eq!(requests_of(&state, "textDocument/definition"), 1);
    assert_eq!(requests_of(&state, "textDocument/references"), 1);
    assert_eq!(
        requests_of(&state, "textDocument/implementation"),
        0,
        "no implementation request with the key off"
    );
}

/// The edges themselves, not only where the walk goes: `lsp.live` over the
/// real manager and the documents fake gives `Implements` edges from the
/// interface member to the two members, each at the place the server's raw
/// answer names, and no other `Implements` edge; with the key off it gives
/// none and asks nothing more.
#[tokio::test]
async fn lsp_live_implements_edges_sit_at_the_places_the_server_answered() {
    use lci_maps::{
        Budget, EdgeKind, Map, SnapshotRef, SnapshotView,
        lsp_live::{LspLive, ManagerBackend, ScipPlaces},
        scip_graph::{ScipGraph, ScipMap},
    };
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shapes_fixture(&temp);
    let raw = raw_implementations_of_area(&workspace).await;
    assert_eq!(raw.len(), 2, "{raw:?}");

    let scip = ScipMap::with_default_ids(ScipGraph::load(&index, None).unwrap());
    let places = Arc::new(ScipPlaces::from_scip(&scip));
    let manager = Arc::new(Manager::new(&config));
    let backend = ManagerBackend::open_with_probe_timeout(
        workspace.clone(),
        Arc::clone(&manager),
        LOADED_BUDGET,
    )
    .await
    .unwrap();
    let map = LspLive::new(Box::new(backend), places).implementations(true);
    let seed = scip.node_id(0).clone();
    assert_eq!(seed.to_string(), AREA_SEED);
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
    let wide = || Budget {
        deadline: std::time::Instant::now() + Duration::from_secs(60),
        max_external_calls: 10,
        max_nodes_touched: u64::MAX,
    };
    let found = map.expand(&seed, wide(), &view).await;
    assert!(map.last_error().is_none(), "{:?}", map.last_error());
    let got: Vec<(EdgeKind, String, u64, u64)> = found
        .edges
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Implements)
        .map(|edge| {
            let site = edge.site.as_ref().unwrap();
            assert_eq!(site.path, "src/Shapes.cs");
            (
                edge.kind,
                edge.dst.to_string(),
                u64::from(site.line),
                u64::from(site.col),
            )
        })
        .collect();
    let want: Vec<(EdgeKind, String, u64, u64)> = raw
        .iter()
        .zip(["~1", "~2"])
        .map(|((_, line, col), suffix)| {
            (
                EdgeKind::Implements,
                format!("{AREA_SEED}{suffix}"),
                *line,
                *col,
            )
        })
        .collect();
    assert_eq!(got, want);
    assert_eq!(found.cost.external_calls, 3);
    assert_eq!(requests_of(&state, "textDocument/implementation"), 1);

    // The default: no Implements edge, no implementation request.
    let backend = ManagerBackend::open_with_probe_timeout(
        workspace.clone(),
        Arc::clone(&manager),
        LOADED_BUDGET,
    )
    .await
    .unwrap();
    let off = LspLive::new(Box::new(backend), Arc::new(ScipPlaces::from_scip(&scip)));
    let found = off.expand(&seed, wide(), &view).await;
    assert!(
        found
            .edges
            .iter()
            .all(|edge| edge.kind != EdgeKind::Implements)
    );
    assert_eq!(found.cost.external_calls, 2);
    assert_eq!(requests_of(&state, "textDocument/implementation"), 1);
    manager.shutdown_all().await;
}

// ---- `lsp.live` call hierarchy (K9b) ----------------------------------------

const ADD_SEED: &str = "sym:rust:Calculator.cs::Add/fn";
const RUN_SEED: &str = "sym:rust:CallSite.cs::Run/fn";

/// What the documents fake answers, asked raw, for the call hierarchy of `Run`
/// (`src/CallSite.cs`): the 0-based `(line, character)` of every call site of
/// its outgoing calls (`fromRanges`, in `Run`'s file) and the number of its
/// incoming callers.
async fn raw_hierarchy_of_run(workspace: &Workspace) -> (Vec<(u64, u64)>, usize) {
    let temp = tempfile::tempdir().unwrap();
    let fake = DocumentsFake::start_on(&temp, workspace.path.clone()).await;
    let run = position_of(CALL_SITE_CS, "Run");
    let items = fake
        .at("textDocument/prepareCallHierarchy", "src/CallSite.cs", run)
        .await;
    assert_eq!(names(&items), ["Run"]);
    let outgoing = fake
        .ask("callHierarchy/outgoingCalls", json!({"item": items[0]}))
        .await;
    assert_eq!(names_of(&outgoing, "to"), ["Add"], "{outgoing}");
    let sites = outgoing[0]["fromRanges"]
        .as_array()
        .unwrap()
        .iter()
        .map(|range| {
            (
                range["start"]["line"].as_u64().unwrap(),
                range["start"]["character"].as_u64().unwrap(),
            )
        })
        .collect();
    let incoming = fake
        .ask("callHierarchy/incomingCalls", json!({"item": items[0]}))
        .await;
    fake.finish().await;
    (sites, incoming.as_array().unwrap().len())
}

/// The `key` item names of a call-hierarchy answer.
fn names_of<'a>(calls: &'a Value, key: &str) -> Vec<&'a str> {
    calls
        .as_array()
        .unwrap()
        .iter()
        .map(|call| call[key]["name"].as_str().unwrap())
        .collect()
}

/// One `lsp.live` walk of `seed` alone to `max_depth`, under `profile`, with
/// the loaded-machine budget.
async fn traverse_symbol_to(
    config: &Config,
    workspace: &Workspace,
    index: &std::path::Path,
    profile: &local_code_intelligence::config::RunProfileConfig,
    seed: &str,
    max_depth: u32,
) -> local_code_intelligence::app::traverse::Report {
    use local_code_intelligence::{
        app::traverse::{BudgetArgs, Call, Dirs, SteerArgs, run},
        config::{TraverseConfig, TraverseFirstStage},
    };
    let limits = TraverseConfig {
        enabled: true,
        first_stage: TraverseFirstStage::Off,
        answer_seeds: false,
        exact_seeds: false,
        deadline_ms: LOADED_BUDGET_SECONDS * 1000,
        ..TraverseConfig::default()
    };
    let (budget, steer) = (
        BudgetArgs {
            deadline_ms: Some(LOADED_BUDGET_SECONDS * 1000),
            max_depth: Some(max_depth),
            ..BudgetArgs::default()
        },
        SteerArgs::default(),
    );
    let maps = [lci_maps::lsp_live::LSP_LIVE_ID.to_owned()];
    let seeds = [seed.to_owned()];
    let call = Call {
        scip_index: index,
        scip_root: Some(&workspace.path),
        question: "what does it call",
        seeds: &seeds,
        seed_search: None,
        top_k: 8,
        maps: Some(&maps),
        budget: &budget,
        steer: &steer,
        handle: None,
        lsp: Some(config),
        lsp_sessions: None,
    };
    run(&limits, profile, &Dirs::under(&config.data_dir), &call)
        .await
        .unwrap()
}

/// [`traverse_symbol_to`] to `max_depth` 1.
async fn traverse_symbol(
    config: &Config,
    workspace: &Workspace,
    index: &std::path::Path,
    profile: &local_code_intelligence::config::RunProfileConfig,
    seed: &str,
) -> local_code_intelligence::app::traverse::Report {
    traverse_symbol_to(config, workspace, index, profile, seed, 1).await
}

/// The destinations of the `lsp.live` hops out of `seed` in a trace, sorted.
fn lsp_live_hops_from(
    report: &local_code_intelligence::app::traverse::Report,
    seed: &str,
) -> Vec<String> {
    let events = lci_maps::traverse::trace::read_trace(report.trace.as_ref().unwrap()).unwrap();
    let mut hops: Vec<String> = events
        .iter()
        .filter(|event| event["ev"] == "hop" && event["map"] == "lsp.live" && event["src"] == seed)
        .map(|event| event["dst"].as_str().unwrap().to_owned())
        .collect();
    hops.sort();
    hops
}

/// The ceiling of the call-hierarchy request: edges equal what the server
/// answers. With the key on, a walk of `Run` asks prepare, incoming and
/// outgoing once each, pays the three calls, and reaches exactly `Add` (the
/// call site the server names is checked on the edge, in
/// `lsp_live_call_hierarchy_edges_sit_at_the_call_sites`); with it off (the default) the
/// same walk asks none, pays two and reaches nothing (`definition` answers
/// `Run` itself, nothing refers to it). Without the slice nothing reaches
/// `Add` from `Run`.
#[tokio::test]
async fn lsp_live_call_hierarchy_edges_equal_the_servers_answer() {
    use local_code_intelligence::config::{LspProfile, RunProfileConfig};
    let profile = |call_hierarchy| RunProfileConfig {
        lsp: LspProfile {
            call_hierarchy,
            ..LspProfile::default()
        },
        ..RunProfileConfig::default()
    };

    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shared_sessions_fixture(&temp);
    let (sites, callers) = raw_hierarchy_of_run(&workspace).await;
    let call = position_of(CALL_SITE_CS, "Add");
    assert_eq!(sites, [(u64::from(call.0), u64::from(call.1))]);
    assert_eq!(callers, 0, "nothing calls Run");

    // Key on.
    let report = traverse_symbol(&config, &workspace, &index, &profile(true), RUN_SEED).await;
    assert_eq!(
        lsp_live_hops_from(&report, RUN_SEED),
        [ADD_SEED],
        "{}",
        report.response
    );
    assert_eq!(report.response["totals"]["external_calls"], 5);
    assert_eq!(requests_of(&state, "textDocument/prepareCallHierarchy"), 1);
    assert_eq!(requests_of(&state, "callHierarchy/incomingCalls"), 1);
    assert_eq!(requests_of(&state, "callHierarchy/outgoingCalls"), 1);
    // A card sits at its node's own definition (the cards' sites count from
    // 1), not at the edge's site: the call site is checked on the edges, in
    // `lsp_live_call_hierarchy_edges_sit_at_the_call_sites`.
    let reached: Vec<(String, u64, u64)> = report.response["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|card| card.to_string().contains("lsp.live"))
        .map(|card| {
            let site = &card["site"];
            (
                site["path"].as_str().unwrap().to_owned(),
                site["line"].as_u64().unwrap(),
                site["col"].as_u64().unwrap(),
            )
        })
        .collect();
    let add = position_of(CALCULATOR_CS, "Add");
    assert_eq!(
        reached,
        [(
            "src/Calculator.cs".to_owned(),
            u64::from(add.0) + 1,
            u64::from(add.1) + 1
        )],
        "{}",
        report.response
    );

    // Key off: no hierarchy request, no hop.
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shared_sessions_fixture(&temp);
    let report = traverse_symbol(&config, &workspace, &index, &profile(false), RUN_SEED).await;
    assert!(
        lsp_live_hops_from(&report, RUN_SEED).is_empty(),
        "{}",
        report.response
    );
    assert_eq!(report.response["totals"]["external_calls"], 2);
    for method in [
        "textDocument/prepareCallHierarchy",
        "callHierarchy/incomingCalls",
        "callHierarchy/outgoingCalls",
    ] {
        assert_eq!(requests_of(&state, method), 0, "{method} with the key off");
    }
}

/// The edges themselves: `lsp.live` over the real manager and the documents
/// fake. From `Run` the `CallsOrRefs` edge to `Add` sits at the call (the raw
/// outgoing answer's `fromRanges[0]`); from `Add` the edge to `Run` exists
/// once, though `references` and the incoming calls both name it.
#[tokio::test]
async fn lsp_live_call_hierarchy_edges_sit_at_the_call_sites() {
    use lci_maps::{
        Budget, EdgeKind, Map, SnapshotRef, SnapshotView,
        lsp_live::{LspLive, ManagerBackend, ScipPlaces},
        scip_graph::{ScipGraph, ScipMap},
    };
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shared_sessions_fixture(&temp);
    let (sites, _) = raw_hierarchy_of_run(&workspace).await;

    let scip = ScipMap::with_default_ids(ScipGraph::load(&index, None).unwrap());
    let places = Arc::new(ScipPlaces::from_scip(&scip));
    let manager = Arc::new(Manager::new(&config));
    let backend = ManagerBackend::open_with_probe_timeout(
        workspace.clone(),
        Arc::clone(&manager),
        LOADED_BUDGET,
    )
    .await
    .unwrap();
    let map = LspLive::new(Box::new(backend), places).call_hierarchy(true);
    let seed_of = |id: &str| {
        (0..scip.graph().nodes.len())
            .map(|i| scip.node_id(i).clone())
            .find(|node| node.to_string() == id)
            .unwrap_or_else(|| panic!("{id} is not in the index"))
    };
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
    let wide = || Budget {
        deadline: std::time::Instant::now() + LOADED_BUDGET,
        max_external_calls: 10,
        max_nodes_touched: u64::MAX,
    };

    // `Run` calls `Add` once.
    let found = map.expand(&seed_of(RUN_SEED), wide(), &view).await;
    assert!(map.last_error().is_none(), "{:?}", map.last_error());
    let got: Vec<(EdgeKind, String, String, u64, u64)> = found
        .edges
        .iter()
        .map(|edge| {
            let site = edge.site.as_ref().unwrap();
            (
                edge.kind,
                edge.dst.to_string(),
                site.path.clone(),
                u64::from(site.line),
                u64::from(site.col),
            )
        })
        .collect();
    assert_eq!(
        got,
        [(
            EdgeKind::CallsOrRefs,
            ADD_SEED.to_owned(),
            "src/CallSite.cs".to_owned(),
            sites[0].0,
            sites[0].1
        )]
    );
    assert_eq!(found.cost.external_calls, 5);

    // `Add` is called by `Run`: one edge, from the references request.
    let found = map.expand(&seed_of(ADD_SEED), wide(), &view).await;
    let to_run: Vec<_> = found
        .edges
        .iter()
        .filter(|edge| edge.dst.to_string() == RUN_SEED)
        .collect();
    assert_eq!(to_run.len(), 1, "{:?}", found.edges);
    assert_eq!(to_run[0].kind, EdgeKind::CallsOrRefs);
    assert_eq!(found.edges.len(), 1, "{:?}", found.edges);
    assert_eq!(found.cost.external_calls, 5);
    assert_eq!(requests_of(&state, "callHierarchy/incomingCalls"), 2);
    manager.shutdown_all().await;
}

/// Whether the answer reached `node` through `lsp.live`.
fn reached_through_lsp_live(answer: &Value, node: &str) -> bool {
    answer["results"].as_array().is_some_and(|cards| {
        cards.iter().any(|card| {
            card["node"].as_str() == Some(node) && card.to_string().contains("lsp.live")
        })
    })
}

/// `CallSite.cs` with a second caller of `Add` on a line of its own.
const CALL_SITE_WITH_TWICE_CS: &str = "namespace Acceptance;\npublic static class CallSite { public static int Run(Calculator value) => value.Add(1, 2); }\npublic static class More { public static int Twice(Calculator value) => value.Add(2, 2); }\n";

/// Under the default `sessions = "shared"` a server that is shared (here
/// csharp-ls under `sync = "watch"`) outlives the walk that started it. The
/// tree barrier of a walk compares the tree stamp (the index's files of the
/// server's languages, their directories and ancestors, the server's
/// manifests) with the one recorded at the server's last check: an unchanged
/// stamp keeps the server, whatever the server's own watcher or the sync
/// would have done.
#[tokio::test]
async fn a_shared_server_is_kept_while_the_tree_does_not_change() {
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = watching_sessions_fixture(&temp);
    let manager = indexed_manager(&config, &workspace);
    for call in 1..=3 {
        let answer = traverse_add_past_a_cold_start(
            &config,
            &workspace,
            &index,
            &state,
            local_code_intelligence::config::LspSessions::Shared,
            &manager,
        )
        .await;
        assert!(
            reached_run_through_lsp_live(&answer),
            "call {call}: {answer}"
        );
        assert!(
            !restart_noted(&answer, "csharp-ls"),
            "call {call}: {answer}"
        );
    }
    // The reused server is not a fresh server's: the last answer says so.
    let answer = traverse_add(
        &config,
        &workspace,
        &index,
        local_code_intelligence::config::LspSessions::Shared,
        &manager,
    )
    .await;
    assert!(kept_noted(&answer, "csharp-ls"), "{answer}");
    assert_eq!(state_count(&state, "spawn_count.txt"), 1, "one server");
    manager.shutdown_all().await;
}

/// An edit between two walks restarts the kept server (`spawn_count` 2), the
/// second walk says so in its notes and finds the new caller of `Add` through
/// `lsp.live`. `sync = "watch"` no longer avoids the restart: the sync (which
/// forwards the edit to a running server) is not what the barrier relies on.
#[tokio::test]
async fn a_shared_server_is_restarted_after_an_edit_between_two_walks() {
    use local_code_intelligence::config::LspSessions;
    const TWICE: &str = "sym:rust:CallSite.cs::Twice/fn";
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = watching_sessions_fixture(&temp);
    let manager = indexed_manager(&config, &workspace);
    let answer = traverse_add_past_a_cold_start(
        &config,
        &workspace,
        &index,
        &state,
        LspSessions::Shared,
        &manager,
    )
    .await;
    assert!(!reached_through_lsp_live(&answer, TWICE), "{answer}");
    assert!(!restart_noted(&answer, "csharp-ls"), "{answer}");

    // A second caller of `Add` (a new length for `CallSite.cs`), and a SCIP
    // index written for the tree as it is now (an input of the walk, which
    // places `Twice` by it).
    write(&workspace.path, "src/CallSite.cs", CALL_SITE_WITH_TWICE_CS);
    let index = csharp_scip_index_of(
        &temp.path().join("index-twice.scip"),
        &workspace,
        &[
            ("src/Calculator.cs", 1, "Add"),
            ("src/CallSite.cs", 1, "Run"),
            ("src/CallSite.cs", 2, "Twice"),
        ],
    );
    let answer = traverse_add(&config, &workspace, &index, LspSessions::Shared, &manager).await;
    assert!(restart_noted(&answer, "csharp-ls"), "{answer}");
    assert!(
        !kept_noted(&answer, "csharp-ls"),
        "a restarted server is a fresh one: {answer}"
    );
    assert!(reached_through_lsp_live(&answer, TWICE), "{answer}");
    assert!(reached_run_through_lsp_live(&answer), "{answer}");
    assert_eq!(
        state_count(&state, "spawn_count.txt"),
        2,
        "the first server was stopped and a second started from the disk"
    );

    // Unchanged since the restart: the second server is kept.
    let answer = traverse_add(&config, &workspace, &index, LspSessions::Shared, &manager).await;
    assert!(!restart_noted(&answer, "csharp-ls"), "{answer}");
    assert!(kept_noted(&answer, "csharp-ls"), "{answer}");
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);
    manager.shutdown_all().await;
}

/// A new file in a directory that holds an indexed file of the server's
/// language is not in the snapshot, but it moves its directory's modification
/// time: the stamp changes and the server restarts.
#[tokio::test]
async fn a_new_file_in_an_indexed_directory_restarts_the_shared_server() {
    use local_code_intelligence::config::LspSessions;
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = watching_sessions_fixture(&temp);
    let manager = indexed_manager(&config, &workspace);
    age_tree(&workspace.path);
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
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);

    write(
        &workspace.path,
        "src/Extra.cs",
        "namespace Acceptance;\npublic static class Extra { }\n",
    );
    let answer = traverse_add(&config, &workspace, &index, LspSessions::Shared, &manager).await;
    assert!(restart_noted(&answer, "csharp-ls"), "{answer}");
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);
    manager.shutdown_all().await;
}

/// A new file in an existing child directory of a stamped directory that holds
/// no indexed file of the server's language (what a rust `#[path]` attribute
/// can name, here `src/generated/`) moves that child's modification time: the
/// stamp covers one level of children, so the server restarts.
#[tokio::test]
async fn a_new_file_in_an_unindexed_child_directory_restarts_the_shared_server() {
    use local_code_intelligence::config::LspSessions;
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = watching_sessions_fixture(&temp);
    write(&workspace.path, "src/generated/README", "generated\n");
    let manager = indexed_manager(&config, &workspace);
    age_tree(&workspace.path);
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
    let answer = traverse_add(&config, &workspace, &index, LspSessions::Shared, &manager).await;
    assert!(!restart_noted(&answer, "csharp-ls"), "{answer}");
    assert!(kept_noted(&answer, "csharp-ls"), "{answer}");

    write(
        &workspace.path,
        "src/generated/Api.cs",
        "namespace Acceptance;\npublic static class Api { }\n",
    );
    let answer = traverse_add(&config, &workspace, &index, LspSessions::Shared, &manager).await;
    assert!(restart_noted(&answer, "csharp-ls"), "{answer}");
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);
    manager.shutdown_all().await;
}

/// An edit of a project file the server reads (csharp-ls: `*.csproj`) changes
/// no source file and no directory's modification time, but the stamp covers
/// the server's configuration files by name in every stamped directory.
#[tokio::test]
async fn a_configuration_file_edit_restarts_the_shared_server() {
    use local_code_intelligence::config::LspSessions;
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = watching_sessions_fixture(&temp);
    write(&workspace.path, "src/App.csproj", "<Project />\n");
    let manager = indexed_manager(&config, &workspace);
    age_tree(&workspace.path);
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

    write(
        &workspace.path,
        "src/App.csproj",
        "<Project Sdk=\"Microsoft.NET.Sdk\" />\n",
    );
    let answer = traverse_add(&config, &workspace, &index, LspSessions::Shared, &manager).await;
    assert!(restart_noted(&answer, "csharp-ls"), "{answer}");
    assert_eq!(state_count(&state, "spawn_count.txt"), 2);
    manager.shutdown_all().await;
}

/// A stat that fails for a reason other than "not found" fails the stamp: the
/// barrier restarts the server on every walk and records nothing, as it does
/// with no snapshot. (A path with an embedded NUL is refused by every
/// platform.)
#[tokio::test]
async fn a_stamp_that_cannot_be_taken_restarts_the_shared_server_every_walk() {
    use local_code_intelligence::config::LspSessions;
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = watching_sessions_fixture(&temp);
    let manager = indexed_manager_listing(&config, &workspace, &["src/bad\0name.cs"]);
    age_tree(&workspace.path);
    let adapter = csharp_adapter(&manager);
    manager.tree_barrier(&workspace, &[&adapter]).await;
    manager
        .symbols(&adapter, &workspace, "Calculator")
        .await
        .expect("the shared server starts and answers");
    assert_eq!(state_count(&state, "spawn_count.txt"), 1);

    let answer = traverse_add(&config, &workspace, &index, LspSessions::Shared, &manager).await;
    assert!(restart_noted(&answer, "csharp-ls"), "{answer}");
    assert!(!kept_noted(&answer, "csharp-ls"), "{answer}");
    assert_eq!(
        state_count(&state, "spawn_count.txt"),
        2,
        "nothing unchanged can be shown: a fresh server"
    );
    let answer = traverse_add(&config, &workspace, &index, LspSessions::Shared, &manager).await;
    assert!(restart_noted(&answer, "csharp-ls"), "{answer}");
    assert!(!kept_noted(&answer, "csharp-ls"), "{answer}");
    assert_eq!(state_count(&state, "spawn_count.txt"), 3);
    manager.shutdown_all().await;
}

/// A change inside a directory the index does not list (an ignored one, whose
/// files are not indexed) and that holds none of the server's indexed files
/// does not move the stamp: the server is kept.
#[tokio::test]
async fn a_change_in_an_ignored_directory_keeps_the_shared_server() {
    use local_code_intelligence::config::LspSessions;
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = watching_sessions_fixture(&temp);
    write(&workspace.path, ".gitignore", "obj/\n");
    write(
        &workspace.path,
        "obj/Generated.cs",
        "namespace Acceptance;\npublic static class Generated { }\n",
    );
    let manager = indexed_manager(&config, &workspace);
    age_tree(&workspace.path);
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

    write(
        &workspace.path,
        "obj/Generated.cs",
        "namespace Acceptance;\npublic static class Generated { public static int More = 1; }\n",
    );
    write(
        &workspace.path,
        "obj/Another.cs",
        "namespace Acceptance;\npublic static class Another { }\n",
    );
    let answer = traverse_add(&config, &workspace, &index, LspSessions::Shared, &manager).await;
    assert!(reached_run_through_lsp_live(&answer), "{answer}");
    assert!(!restart_noted(&answer, "csharp-ls"), "{answer}");
    assert_eq!(state_count(&state, "spawn_count.txt"), 1, "one server");
    manager.shutdown_all().await;
}

/// Without an index file list there is no stamp and so no sharing: every walk
/// restarts the shared server it is asked about.
#[tokio::test]
async fn without_an_index_snapshot_a_shared_server_is_not_kept() {
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = watching_sessions_fixture(&temp);
    let manager = Arc::new(Manager::new(&config));
    for _ in 0..2 {
        let answer =
            traverse_add_by_default_until(&config, &workspace, &index, &manager, |answer| {
                reached_run_through_lsp_live(answer)
            })
            .await
            .0;
        assert!(reached_run_through_lsp_live(&answer), "{answer}");
    }
    assert!(
        state_count(&state, "spawn_count.txt") >= 2,
        "a server per walk"
    );
    manager.shutdown_all().await;
}

/// [`traverse_add`] under the default profile (no `sessions` line), for a server
/// the walk starts on its own manager (csharp-ls is a non-watching adapter).
/// A walk that only ran out of time on a cold fake server (a loaded machine
/// can exceed `lsp_live::CALL_TIMEOUT`) and has not reached `done` is asked
/// again, up to three walks in all; any other error note fails the test.
/// Returns the last answer and the walks made.
async fn traverse_add_by_default_until(
    config: &Config,
    workspace: &Workspace,
    index: &std::path::Path,
    manager: &Arc<Manager>,
    done: impl Fn(&Value) -> bool,
) -> (Value, u64) {
    use local_code_intelligence::config::{LspProfile, LspSessions};
    let sessions = LspProfile::default().sessions;
    assert_eq!(sessions, LspSessions::Shared, "the default is shared");
    let mut walks = 0;
    loop {
        walks += 1;
        let answer = traverse_add(config, workspace, index, sessions, manager).await;
        // Only a clean deadline miss may be retried: any other error note
        // (a failed call, an error for a node, an unavailable map) fails the
        // test.
        let errors: Vec<&str> = answer["notes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|note| {
                note.contains("calls failed")
                    || note.contains("reported an error")
                    || note.contains("is unavailable")
            })
            .collect();
        let ran_out_of_time = answer["notes"].as_array().is_some_and(|notes| {
            notes.iter().any(|note| {
                note.as_str()
                    .is_some_and(|text| text.contains(lci_maps::lsp_live::DEADLINE_NOTE))
            })
        });
        assert!(
            errors
                .iter()
                .all(|note| note.contains(lci_maps::lsp_live::DEADLINE_NOTE)),
            "walk {walks} failed on more than the deadline: {errors:?}"
        );
        if done(&answer) || walks == 3 || !ran_out_of_time {
            return (answer, walks);
        }
    }
}

/// The rule behind the default `sessions = "shared"`: no answer is staler than
/// under `per_call`. csharp-ls is a server that is told nothing about an edit
/// (a non-watching adapter), so under the default profile with `sync = "off"`
/// every walk starts its own server on the tree as it is, and the same edit
/// that `a_shared_server_is_restarted_after_an_edit_between_two_walks` needs
/// `sync = "watch"` and a restart for is seen without either. Nothing stays in
/// the service's manager.
#[tokio::test]
async fn the_default_profile_sees_an_edit_on_a_server_that_does_not_watch() {
    use local_code_intelligence::config::{LspSessions, LspSync};
    const TWICE: &str = "sym:rust:CallSite.cs::Twice/fn";
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shared_sessions_fixture(&temp);
    // The default: no `sessions` line, no sync.
    assert_eq!(config.run_profile.lsp.sessions, LspSessions::default());
    assert_eq!(config.run_profile.lsp.sync, LspSync::Off);
    let manager = Arc::new(Manager::new(&config));
    let (answer, first) =
        traverse_add_by_default_until(&config, &workspace, &index, &manager, |answer| {
            reached_run_through_lsp_live(answer)
        })
        .await;
    assert!(reached_run_through_lsp_live(&answer), "{answer}");
    assert!(!reached_through_lsp_live(&answer, TWICE), "{answer}");

    // A second caller of `Add`, and a SCIP index written for the tree as it
    // is now. No sync, no wait: the next walk's server reads the tree.
    write(&workspace.path, "src/CallSite.cs", CALL_SITE_WITH_TWICE_CS);
    let index = csharp_scip_index_of(
        &temp.path().join("index-twice.scip"),
        &workspace,
        &[
            ("src/Calculator.cs", 1, "Add"),
            ("src/CallSite.cs", 1, "Run"),
            ("src/CallSite.cs", 2, "Twice"),
        ],
    );
    let (answer, second) =
        traverse_add_by_default_until(&config, &workspace, &index, &manager, |answer| {
            reached_through_lsp_live(answer, TWICE)
        })
        .await;
    assert!(reached_through_lsp_live(&answer, TWICE), "{answer}");
    assert!(reached_run_through_lsp_live(&answer), "{answer}");
    assert_eq!(
        state_count(&state, "spawn_count.txt"),
        first + second,
        "one server per walk: the call's own, never the offered manager's"
    );
    assert!(
        manager.release(&workspace.id).await.is_empty(),
        "nothing stayed in the service's manager"
    );
    manager.shutdown_all().await;
}

/// Under the default profile a service primes no csharp-ls (a walk would not
/// share it), and a walk through the service's manager leaves no csharp-ls in
/// it: the server is the call's own and ends with the walk.
#[tokio::test]
async fn the_default_profile_keeps_nothing_in_the_service_manager_for_a_non_watching_server() {
    use local_code_intelligence::config::{LspProfile, LspSessions};
    let service = indexed_warm_service(&[], |config| {
        // The default profile: no `sessions` line, `sync = "off"`.
        config.run_profile.lsp = LspProfile::default();
    })
    .await;
    let (app, workspace) = (&service.app, &service.workspace);
    assert_eq!(service.config.run_profile.lsp.sessions, LspSessions::Shared);
    let index = csharp_scip_index(&service.temp, workspace);
    let id = app.status(&workspace.path).await.unwrap().workspace.id;
    let outcome = finished_warm_start(app, &id).await;
    assert!(outcome.is_empty(), "csharp-ls was primed: {outcome:?}");
    let status = app.status(&workspace.path).await.unwrap();
    assert!(
        status.analyzers["csharp-ls"].is_none(),
        "{:?}",
        status.analyzers
    );

    let (answer, walks) = traverse_add_by_default_until(
        &service.config,
        workspace,
        &index,
        &app.language_servers(),
        reached_run_through_lsp_live,
    )
    .await;
    assert!(reached_run_through_lsp_live(&answer), "{answer}");
    let status = app.status(&workspace.path).await.unwrap();
    assert!(
        status.analyzers["csharp-ls"].is_none(),
        "{:?}",
        status.analyzers
    );
    assert!(!status.analyzers_running["csharp-ls"]);
    assert_eq!(
        state_count(&service.state, "spawn_count.txt"),
        walks,
        "one server per walk, none from the warm start"
    );
    app.shutdown_language_servers().await;
    service.models_task.abort();
}

/// The real-server edit check of `[run_profile.lsp] sync` (live, skipped when
/// `gopls` or `go` is not on `PATH`): a running gopls session is told about an
/// edit, and its next `workspace/symbol` answer lists the new function. gopls
/// has no file watcher of its own and this client registers none, so without
/// the sync the session keeps answering the tree it loaded.
#[tokio::test]
async fn a_real_gopls_session_lists_a_function_added_after_it_started() {
    use local_code_intelligence::config::{GoLspConfig, LspSync};
    let (Some(gopls), Some(_)) = (
        lci_core::lexical::find_on_path("gopls"),
        lci_core::lexical::find_on_path("go"),
    ) else {
        eprintln!("skipped: gopls or go is not on PATH");
        return;
    };
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    let data = temp.path().join("data");
    std::fs::create_dir_all(&data).unwrap();
    write(&root, "go.mod", "module example.com/syncprobe\n\ngo 1.21\n");
    write(
        &root,
        "calc.go",
        "package syncprobe\n\nfunc Add(a, b int) int { return a + b }\n",
    );
    let workspace = Workspace::resolve(&root, &data).unwrap();
    let mut config = Config {
        data_dir: data,
        lsp_timeout_seconds: LOADED_BUDGET_SECONDS,
        go: GoLspConfig {
            path: Some(gopls.to_string_lossy().into_owned()),
            args: Vec::new(),
            disabled: false,
        },
        ..Config::default()
    };
    config.run_profile.lsp.sync = LspSync::Watch;
    let manager = Manager::new(&config);
    let go = manager.adapters_for_language("go")[0].clone();
    let names = |found: Vec<Location>| -> Vec<String> {
        found
            .into_iter()
            .filter_map(|location| location.name)
            .collect()
    };
    let added = names(manager.symbols(&go, &workspace, "Add").await.unwrap());
    assert!(added.contains(&"Add".to_owned()), "{added:?}");
    assert!(manager.synced(&workspace.id, &[], LOADED_BUDGET).await);

    write(
        &root,
        "calc.go",
        "package syncprobe\n\nfunc Add(a, b int) int { return a + b }\n\nfunc SyncProbeEight() int { return 8 }\n",
    );
    assert!(
        manager
            .synced(&workspace.id, &["calc.go"], LOADED_BUDGET)
            .await
    );
    // gopls applies a change on its own schedule: ask until it lists the
    // function, every 100 ms within the budget (a condition wait on the
    // server's answers).
    let deadline = tokio::time::Instant::now() + LOADED_BUDGET;
    let listed = loop {
        let listed = names(
            manager
                .symbols(&go, &workspace, "SyncProbeEight")
                .await
                .unwrap(),
        );
        if listed.contains(&"SyncProbeEight".to_owned()) || tokio::time::Instant::now() >= deadline
        {
            break listed;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert!(
        listed.contains(&"SyncProbeEight".to_owned()),
        "the running session does not list the new function: {listed:?}, {:?}",
        manager.sync_status(&workspace.id)
    );
    manager.shutdown_all().await;
}

/// Spins (no sleep, no await, so a current-thread runtime runs nothing
/// else) until the workspace's watcher has queued more than `seen` events:
/// the change is then reported but, with the forwarding task never polled,
/// not yet drained.
fn wait_for_a_queued_event(manager: &Manager, workspace: &Workspace, seen: u64) {
    let deadline = std::time::Instant::now() + LOADED_BUDGET;
    while manager.sync_status(&workspace.id).unwrap().events <= seen {
        assert!(
            std::time::Instant::now() < deadline,
            "the watcher reported nothing"
        );
        std::hint::spin_loop();
    }
}

/// Under `[run_profile.lsp] sync = "watch"` a request drains what the watcher
/// has already reported before it is sent, without anyone waiting for the
/// sync: a change queued but not yet forwarded (the test runs on one thread
/// and never yields between the edit and the request, so the forwarding task
/// cannot run) is in the next answer (`Manager::client`). The second part is a
/// behaviour check only: a reported change drops a kept outline without a
/// wait, but `document_symbols` reads the file asynchronously before its
/// cache lookup, which lets the forwarding task drain first, so this does not
/// isolate the drain `document_symbols` makes itself.
#[tokio::test]
async fn a_request_sends_the_changes_already_reported_before_it_is_sent() {
    use local_code_intelligence::config::LspSync;
    assert_eq!(
        tokio::runtime::Handle::current().runtime_flavor(),
        tokio::runtime::RuntimeFlavor::CurrentThread
    );
    // A request.
    let temp = tempfile::tempdir().unwrap();
    let (mut config, workspace, _) = fake_csharp_fixture(&temp, "documents");
    config.lsp_timeout_seconds = LOADED_BUDGET_SECONDS;
    config.rust_analyzer_path = "definitely-missing-rust-analyzer".into();
    config.run_profile.lsp.sync = LspSync::Watch;
    let manager = Manager::new(&config);
    let csharp = csharp_adapter(&manager);
    assert!(
        !manager
            .symbols(&csharp, &workspace, "Calculator")
            .await
            .unwrap()
            .is_empty()
    );
    assert!(manager.synced(&workspace.id, &[], LOADED_BUDGET).await);
    let seen = manager.sync_status(&workspace.id).unwrap().events;
    write(
        &workspace.path,
        "src/Calculator.cs",
        "namespace Acceptance;\npublic sealed class Calculator { public int Add(int a, int b) => a + b; public int Mul(int a, int b) => a * b; }\n",
    );
    wait_for_a_queued_event(&manager, &workspace, seen);
    let found = manager.symbols(&csharp, &workspace, "Mul").await.unwrap();
    assert!(
        found
            .iter()
            .any(|symbol| symbol.name.as_deref() == Some("Mul")),
        "{found:?}"
    );
    manager.shutdown_all().await;

    // An outline the cache holds.
    let temp = tempfile::tempdir().unwrap();
    let (manager, workspace, states) = outline_fixture_with(&temp, true, LspSync::Watch);
    let python = python_adapter(&manager);
    let calculator = "src/calculator.py";
    for _ in 0..2 {
        outline_names(&manager, &python, &workspace, calculator).await;
    }
    assert_eq!(outline_requests(&states.python, calculator), 1, "kept");
    assert!(manager.synced(&workspace.id, &[], LOADED_BUDGET).await);
    let seen = manager.sync_status(&workspace.id).unwrap().events;
    // The same bytes, with a modification time that is certainly new.
    write(&workspace.path, calculator, CALCULATOR_PY);
    std::fs::File::options()
        .write(true)
        .open(workspace.path.join(calculator))
        .unwrap()
        .set_modified(std::time::SystemTime::now() + Duration::from_secs(3600))
        .unwrap();
    wait_for_a_queued_event(&manager, &workspace, seen);
    assert_eq!(
        outline_names(&manager, &python, &workspace, calculator).await,
        ["Calculator", "add"]
    );
    assert_eq!(
        outline_requests(&states.python, calculator),
        2,
        "the reported change dropped the kept outline before the lookup"
    );
    manager.shutdown_all().await;
}

// ---- `lsp.live` places from the outline (K10a) ------------------------------

const BOX_CS: &str = "public class Box {\n    public int Size() {\n        return 1;\n    }\n    public int Count() => 2;\n}\n";

/// The documents fake's outline answers each declaration with a range from its
/// name to the end of the declaration (a flat `SymbolInformation` has no
/// `selectionRange`, so the range's start is the name), while `definition`
/// still answers the name token. A two-line method ends on its closing line, a
/// `=>` member on its own line, the type on its closing brace.
#[tokio::test]
async fn documents_fake_outline_ranges_run_to_the_declarations_end() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("workspace");
    write(&root, "src/Box.cs", BOX_CS);
    let fake = DocumentsFake::start_on(&temp, root).await;
    let outline = fake
        .ask(
            "textDocument/documentSymbol",
            json!({"textDocument": {"uri": fake.uri("src/Box.cs")}}),
        )
        .await;
    let ranges: Vec<(String, u64, u64, u64)> = outline
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            let range = &item["location"]["range"];
            (
                item["name"].as_str().unwrap().to_owned(),
                range["start"]["line"].as_u64().unwrap(),
                range["start"]["character"].as_u64().unwrap(),
                range["end"]["line"].as_u64().unwrap(),
            )
        })
        .collect();
    let lines: Vec<&str> = BOX_CS.lines().collect();
    let column = |line: usize, name: &str| lines[line].find(name).unwrap() as u64;
    assert_eq!(
        ranges,
        [
            ("Box".to_owned(), 0, column(0, "Box"), 5),
            ("Size".to_owned(), 1, column(1, "Size"), 3),
            ("Count".to_owned(), 4, column(4, "Count"), 4),
        ]
    );
    // `definition` still answers the name token.
    let size = position_of(BOX_CS, "Size");
    let answer = fake.at("textDocument/definition", "src/Box.cs", size).await;
    let range = &answer[0]["range"];
    assert_eq!(range["start"]["line"], 1);
    assert_eq!(range["end"]["line"], 1);
    assert_eq!(
        range["end"]["character"].as_u64().unwrap() - range["start"]["character"].as_u64().unwrap(),
        "Size".len() as u64
    );
    fake.finish().await;
}

/// The profile of the K10a walks: no SCIP index read (`off`), the live outline
/// as the place table, implementations on.
fn outline_profile(
    mode: local_code_intelligence::config::profile::ScipMode,
    places: local_code_intelligence::config::LspPlaces,
) -> local_code_intelligence::config::RunProfileConfig {
    use local_code_intelligence::config::{LspProfile, RunProfileConfig, profile::ScipProfile};
    RunProfileConfig {
        scip: ScipProfile { mode },
        lsp: LspProfile {
            places,
            implementations: true,
            ..LspProfile::default()
        },
        ..RunProfileConfig::default()
    }
}

/// Ceiling check 1: with no SCIP index at all (`scip.mode = off`) `lsp.live`
/// walks out of a file once its places come from the server's outline. The
/// walk of `src/Shapes.cs` outlines the file once, names its six declarations
/// with ids of the file's real language, and reaches the implementing members
/// from the interface member. Under `places = scip` the same walk asks the
/// file's outline (as before) but names nothing and asks nothing else (d411).
/// Without the slice there is no place, so no request at any symbol and no edge.
#[tokio::test]
async fn lsp_live_outline_places_walk_without_a_scip_index() {
    use local_code_intelligence::config::{LspPlaces, profile::ScipMode};
    const FILE: &str = "file:src/Shapes.cs";
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shapes_fixture(&temp);

    // The server's own outline: IShape, Circle, Square and the three `Area`
    // methods (kind 6), by document order.
    let raw = {
        let fake_temp = tempfile::tempdir().unwrap();
        let fake = DocumentsFake::start_on(&fake_temp, workspace.path.clone()).await;
        let answer = fake
            .ask(
                "textDocument/documentSymbol",
                json!({"textDocument": {"uri": fake.uri("src/Shapes.cs")}}),
            )
            .await;
        fake.finish().await;
        answer
    };
    let listed: Vec<(String, u64)> = raw
        .as_array()
        .unwrap()
        .iter()
        .map(|item| {
            (
                item["name"].as_str().unwrap().to_owned(),
                item["location"]["range"]["start"]["line"].as_u64().unwrap(),
            )
        })
        .collect();
    let area_kinds: Vec<u64> = raw
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["name"] == "Area")
        .map(|item| item["kind"].as_u64().unwrap())
        .collect();
    assert_eq!(area_kinds, [6, 6, 6], "{listed:?}");
    assert_eq!(listed.len(), 6, "{listed:?}");

    let profile = outline_profile(ScipMode::Off, LspPlaces::Outline);
    let report = traverse_symbol_to(&config, &workspace, &index, &profile, FILE, 2).await;
    let from_file = lsp_live_hops_from(&report, FILE);
    assert_eq!(
        from_file,
        [
            "sym:csharp:Shapes.cs::Area/method",
            "sym:csharp:Shapes.cs::Area/method~1",
            "sym:csharp:Shapes.cs::Area/method~2",
            "sym:csharp:Shapes.cs::Circle/class",
            "sym:csharp:Shapes.cs::IShape/interface",
            "sym:csharp:Shapes.cs::Square/class",
        ],
        "{}",
        report.response
    );
    assert_eq!(
        requests_of(&state, "textDocument/documentSymbol"),
        1,
        "the file is outlined once"
    );
    // Each of the six symbols is asked at the place the outline gave it, for
    // its definition, references and implementations: the interface member's
    // implementation request is what names the two members that implement it
    // (their hops are the file's, so a trace shows no second hop to them).
    for method in [
        "textDocument/definition",
        "textDocument/references",
        "textDocument/implementation",
    ] {
        assert_eq!(requests_of(&state, method), 6, "{method}");
    }
    // One outline, then definition, references and implementation for each of
    // the six symbols the walk expands.
    assert_eq!(report.response["totals"]["external_calls"], 1 + 6 * 3);

    // `places = scip` under `off`: the file's own outline is asked as it
    // always was (one call), but it names nothing the empty index defines, so
    // there is no hop, no symbol is placed and no other request is made.
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shapes_fixture(&temp);
    let profile = outline_profile(ScipMode::Off, LspPlaces::Scip);
    let report = traverse_symbol_to(&config, &workspace, &index, &profile, FILE, 2).await;
    assert!(
        lsp_live_hops_from(&report, FILE).is_empty(),
        "{}",
        report.response
    );
    assert_eq!(report.response["totals"]["external_calls"], 1);
    assert_eq!(requests_of(&state, "textDocument/documentSymbol"), 1);
    assert_eq!(requests_of(&state, "textDocument/definition"), 0);
}

/// Ceiling check 2: with the index present the layers agree. A walk of the
/// index's `Area` under `places = outline` asks the file's outline once (one
/// more external call), and every id it names is the index's own, not a minted
/// one: the outline does not shadow the index.
#[tokio::test]
async fn lsp_live_outline_places_keep_the_indexs_ids() {
    use local_code_intelligence::config::{LspPlaces, profile::ScipMode};
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shapes_fixture(&temp);
    let profile = outline_profile(ScipMode::Authoritative, LspPlaces::Outline);
    let report = traverse_symbol_to(&config, &workspace, &index, &profile, AREA_SEED, 1).await;
    assert_eq!(
        lsp_live_hops_from(&report, AREA_SEED),
        [
            "sym:rust:Shapes.cs::Area/method~1",
            "sym:rust:Shapes.cs::Area/method~2"
        ],
        "{}",
        report.response
    );
    assert_eq!(report.response["totals"]["external_calls"], 4);
    assert_eq!(requests_of(&state, "textDocument/documentSymbol"), 1);
    assert_eq!(requests_of(&state, "textDocument/definition"), 1);
    assert_eq!(requests_of(&state, "textDocument/references"), 1);
    assert_eq!(requests_of(&state, "textDocument/implementation"), 1);

    // The default asks no outline: the K9a walk, three calls.
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shapes_fixture(&temp);
    let profile = outline_profile(ScipMode::Authoritative, LspPlaces::Scip);
    let report = traverse_symbol_to(&config, &workspace, &index, &profile, AREA_SEED, 1).await;
    assert_eq!(report.response["totals"]["external_calls"], 3);
    assert_eq!(requests_of(&state, "textDocument/documentSymbol"), 0);
}

/// The edges themselves, over the real manager and the documents fake, with an
/// empty index and `places = outline`: expanding the file gives `Contains`
/// edges to its six declarations (the sites are the outline's name places),
/// and expanding the interface's `Area` by the id the outline gave it gives
/// `Implements` edges to the two implementing members, both symbols (no file
/// destination), at the places the server's own answer names.
#[tokio::test]
async fn lsp_live_outline_places_give_contains_and_implements_edges_without_an_index() {
    use lci_maps::{
        Budget, EdgeKind, Map, SnapshotRef, SnapshotView,
        lsp_live::{LspLive, ManagerBackend, OutlinePlaces, ScipPlaces},
        scip_graph::{ScipGraph, ScipMap},
    };
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, _index) = shapes_fixture(&temp);
    let raw = raw_implementations_of_area(&workspace).await;
    assert_eq!(raw.len(), 2, "{raw:?}");

    let empty = ScipMap::with_default_ids(ScipGraph::empty());
    let places = Arc::new(OutlinePlaces::over(ScipPlaces::from_scip(&empty)));
    let manager = Arc::new(Manager::new(&config));
    let backend = ManagerBackend::open_with_probe_timeout(
        workspace.clone(),
        Arc::clone(&manager),
        LOADED_BUDGET,
    )
    .await
    .unwrap();
    let map = LspLive::new(Box::new(backend), places).implementations(true);
    let view = SnapshotView::new(SnapshotRef::default(), Vec::new());
    let wide = || Budget {
        deadline: std::time::Instant::now() + Duration::from_secs(60),
        max_external_calls: 10,
        max_nodes_touched: u64::MAX,
    };

    let file = lci_core::node::NodeId::file("src/Shapes.cs").unwrap();
    let found = map.expand(&file, wide(), &view).await;
    assert!(map.last_error().is_none(), "{:?}", map.last_error());
    let mut contained: Vec<String> = found
        .edges
        .iter()
        .inspect(|edge| assert_eq!(edge.kind, EdgeKind::Contains))
        .map(|edge| edge.dst.to_string())
        .collect();
    contained.sort();
    assert_eq!(
        contained,
        [
            "sym:csharp:Shapes.cs::Area/method",
            "sym:csharp:Shapes.cs::Area/method~1",
            "sym:csharp:Shapes.cs::Area/method~2",
            "sym:csharp:Shapes.cs::Circle/class",
            "sym:csharp:Shapes.cs::IShape/interface",
            "sym:csharp:Shapes.cs::Square/class",
        ]
    );
    assert_eq!(found.cost.external_calls, 1);

    let interface_area = lci_core::node::NodeId::Symbol(lci_core::node::SymbolKey {
        language: "csharp".into(),
        module_path: vec!["Shapes.cs".into()],
        container: Vec::new(),
        name: "Area".into(),
        kind: lci_core::node::SymbolKind::Method,
        disambiguator: 0,
    });
    let found = map.expand(&interface_area, wide(), &view).await;
    assert!(map.last_error().is_none(), "{:?}", map.last_error());
    // No second outline: the file is learned; definition, references and
    // implementation are the three calls.
    assert_eq!(found.cost.external_calls, 3);
    assert_eq!(requests_of(&state, "textDocument/documentSymbol"), 1);
    let got: Vec<(String, u64, u64)> = found
        .edges
        .iter()
        .filter(|edge| edge.kind == EdgeKind::Implements)
        .map(|edge| {
            assert!(
                matches!(edge.dst, lci_core::node::NodeId::Symbol(_)),
                "{}",
                edge.dst
            );
            let site = edge.site.as_ref().unwrap();
            (
                edge.dst.to_string(),
                u64::from(site.line),
                u64::from(site.col),
            )
        })
        .collect();
    let want: Vec<(String, u64, u64)> = raw
        .iter()
        .zip(["~1", "~2"])
        .map(|((_, line, col), suffix)| {
            (
                format!("sym:csharp:Shapes.cs::Area/method{suffix}"),
                *line,
                *col,
            )
        })
        .collect();
    assert_eq!(got, want);
    manager.shutdown_all().await;
}

/// The router over a language server: a policy table that lists `lsp.live`
/// brings it into a walk of the intent that names that policy, with no
/// `--maps` on the call, and the hop is a real request to the (documents)
/// fake. The same call under another intent, with no table entry, asks the
/// server nothing.
#[tokio::test]
async fn a_policy_table_routes_an_intent_to_lsp_live_without_naming_maps() {
    use local_code_intelligence::config::RunProfileConfig;
    let routes: &[(&str, &[&str])] = &[("flow", &["lsp.live"])];

    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shapes_fixture(&temp);
    let profile = RunProfileConfig::default();
    let routed = traverse_area_routed(
        &config,
        &workspace,
        &index,
        &profile,
        None,
        Some("flow"),
        routes,
    )
    .await;
    assert!(!lsp_live_hops(&routed).is_empty(), "{}", routed.response);
    let policy = &routed.response["effective_policy"];
    assert_eq!(policy["preset"], "flow", "{}", routed.response);
    assert!(
        policy["maps"]
            .as_array()
            .unwrap()
            .iter()
            .any(|map| map["id"] == "lsp.live"),
        "{}",
        routed.response
    );
    assert!(requests_of(&state, "textDocument/definition") >= 1);

    // Another intent: the table has no entry for it, so no language server.
    let temp = tempfile::tempdir().unwrap();
    let (config, workspace, state, index) = shapes_fixture(&temp);
    let unrouted = traverse_area_routed(
        &config,
        &workspace,
        &index,
        &profile,
        None,
        Some("explore"),
        routes,
    )
    .await;
    assert!(lsp_live_hops(&unrouted).is_empty(), "{}", unrouted.response);
    assert_eq!(unrouted.response["effective_policy"]["preset"], "explore");
    assert_eq!(requests_of(&state, "textDocument/definition"), 0);
}
