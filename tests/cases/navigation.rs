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

/// Builds a fake-clangd fixture for either `c` or `cpp` (`extension`: `"c"`
/// or `"cpp"`), since one clangd instance/config navigates both.
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
/// call finds the session stuck.
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

    for _ in 0..6 {
        let outcome = tokio::time::timeout(
            Duration::from_millis(1000),
            manager.symbols(&adapter, &workspace, "Calculator"),
        )
        .await;
        assert!(outcome.is_err(), "the call must be cancelled, not answered");
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

    let query = "x".repeat(4 * 1024 * 1024);
    let started = std::time::Instant::now();
    let results = tokio::time::timeout(
        Duration::from_secs(30),
        manager.symbols(&adapter, &workspace, &query),
    )
    .await
    .expect("the call waited on a pipe nobody reads")
    .unwrap();
    assert_eq!(results[0].name.as_deref(), Some("Calculator"));
    assert!(
        started.elapsed() < Duration::from_secs(20),
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
    config.lsp_timeout_seconds = 2;
    // Every `workspace/symbol` after the priming one stalls for 2.8 s: past
    // the 2 s timeout, so the call fails, yet inside the 2 s the discard's
    // shutdown waits for an answer, so the stalled server wakes up, reads the
    // `shutdown` and `exit` queued behind its request, and ends.
    config
        .csharp
        .args
        .extend(fake_args(&["--symbol-delay-ms", "2800"]));
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
        Duration::from_secs(15),
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
    assert_eq!(
        state_count(&state, "spawn_count.txt"),
        1,
        "five callers that arrived while the server was starting must share that one start"
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
/// under load fail their `initialize`, so this one allows 5 s.
fn fake_csharp_release_fixture(
    temp: &tempfile::TempDir,
) -> (Config, Workspace, std::path::PathBuf) {
    let (mut config, workspace, state) = fake_csharp_fixture(temp, "workspace-symbols");
    config.lsp_timeout_seconds = 5;
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
                .status();
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn sigterm_shuts_down_the_running_language_server() {
    let (temp, mut config, _models, models_task) = fixture().await;
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("src")).unwrap();
    std::fs::write(workspace.join("src/Calculator.cs"), "class Calculator {}\n").unwrap();

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    config.port = listener.local_addr().unwrap().port();
    drop(listener);
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
    std::fs::write(temp.path().join("serve.toml"), toml::to_string(&config).unwrap()).unwrap();

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
        .args(["-TERM", &serve.child.id().to_string()])
        .status()
        .await
        .unwrap();
    assert!(signal.success(), "could not send SIGTERM to serve");
    let server_exited = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if serve.child.try_wait().unwrap().is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .is_ok();

    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    while process_is_running(pid).await && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let language_server_exited = !process_is_running(pid).await;
    drop(models_task);
    assert!(server_exited, "serve did not exit after SIGTERM");
    assert!(
        language_server_exited,
        "language server process {pid} survived SIGTERM"
    );
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
    let app = App::open(with_fake_csharp_server(config, &state, &[], 5))
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
/// its own directory of `state_dir`.
fn with_fake_csharp_and_typescript_servers(
    mut config: Config,
    state_dir: &std::path::Path,
    csharp_mode: &str,
    typescript_mode: &str,
) -> Config {
    config.lsp_timeout_seconds = 5;
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

/// Confirms one shared ClangdServer instance correctly navigates both `c`
/// and `cpp` -- the same one-adapter-many-languages contract already
/// exercised for TypeScript's four ECMAScript-family languages.
#[tokio::test]
async fn fake_clangd_lsp_maps_symbols_definitions_and_references() {
    for extension in ["c", "cpp"] {
        for (mode, operation) in [
            ("workspace-symbols", "symbols"),
            ("definition", "definition"),
            ("references", "references"),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let (config, workspace, _) = fake_clangd_fixture(&temp, mode, extension);
            let manager = Manager::new(&config);
            let adapter = clangd_adapter(&manager, extension);
            let primary = format!("src/Calculator.{extension}");
            let secondary = format!("src/CallSite.{extension}");
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
                        1,
                        16,
                    )
                    .await
                    .unwrap(),
                "references" => manager
                    .references(
                        &adapter,
                        &workspace,
                        &workspace.path.join(&primary),
                        &primary,
                        1,
                        4,
                        true,
                    )
                    .await
                    .unwrap(),
                _ => unreachable!(),
            };
            assert!(!results.is_empty());
            assert!(results.iter().all(|result| {
                result.language.as_deref() == Some(extension)
                    && result.provider.as_deref() == Some("clangd")
                    && result.start_line >= 1
            }));
            assert_eq!(
                results[0].relative_file_path.as_deref(),
                Some(primary.as_str())
            );
            if operation == "references" {
                assert!(
                    results
                        .iter()
                        .any(|result| result.relative_file_path.as_deref()
                            == Some(secondary.as_str()))
                );
            }
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
async fn go_only_search_and_navigation_keep_go_lsp_optional() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("go-only");
    write(
        &workspace,
        "revenue.go",
        "package revenue\n\n// ComputeQuarterlyRevenueSummary sums quarterly revenue rows.\nfunc ComputeQuarterlyRevenueSummary(rows []int) int {\n\ttotal := 0\n\tfor _, row := range rows {\n\t\ttotal += row\n\t}\n\treturn total\n}\n",
    );
    let app = App::open(config).await.unwrap();
    let indexed = app.index(&workspace).await.unwrap();
    assert_eq!(indexed.files, 1);
    assert_eq!(indexed.parsed_files, 1);

    let report = app
        .search(&workspace, "ComputeQuarterlyRevenueSummary", Some(4))
        .await
        .unwrap();
    assert_eq!(report.results[0].chunk.language, "go");
    assert_eq!(report.results[0].chunk.relative_file_path, "revenue.go");
    assert!(
        report
            .warning
            .as_deref()
            .is_none_or(|warning| !warning.contains("LSP")),
        "unexpected warning: {:?}",
        report.warning
    );
    assert!(!app.status(&workspace).await.unwrap().go_analyzer_running);

    let definition_error = app
        .definition(&workspace, "revenue.go", 1, 0)
        .await
        .unwrap_err();
    assert!(
        definition_error.to_string().contains("gopls is disabled"),
        "unexpected error: {definition_error}"
    );
    assert!(!app.status(&workspace).await.unwrap().go_analyzer_running);

    let references_error = app
        .references(&workspace, "revenue.go", 1, 0, true)
        .await
        .unwrap_err();
    assert!(
        references_error.to_string().contains("gopls is disabled"),
        "unexpected error: {references_error}"
    );
    assert!(!app.status(&workspace).await.unwrap().go_analyzer_running);
    task.abort();
}

#[tokio::test]
async fn c_only_search_and_navigation_keep_clangd_lsp_optional() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("c-only");
    write(
        &workspace,
        "revenue.c",
        "// compute_quarterly_revenue_summary sums quarterly revenue rows.\nint compute_quarterly_revenue_summary(int *rows, int count) {\n    int total = 0;\n    for (int i = 0; i < count; i++) {\n        total += rows[i];\n    }\n    return total;\n}\n",
    );
    let app = App::open(config).await.unwrap();
    let indexed = app.index(&workspace).await.unwrap();
    assert_eq!(indexed.files, 1);
    assert_eq!(indexed.parsed_files, 1);

    let report = app
        .search(&workspace, "compute_quarterly_revenue_summary", Some(4))
        .await
        .unwrap();
    assert_eq!(report.results[0].chunk.language, "c");
    assert_eq!(report.results[0].chunk.relative_file_path, "revenue.c");
    assert!(
        report
            .warning
            .as_deref()
            .is_none_or(|warning| !warning.contains("LSP")),
        "unexpected warning: {:?}",
        report.warning
    );
    assert!(
        !app.status(&workspace)
            .await
            .unwrap()
            .clangd_analyzer_running
    );

    let definition_error = app
        .definition(&workspace, "revenue.c", 1, 0)
        .await
        .unwrap_err();
    assert!(
        definition_error.to_string().contains("clangd is disabled"),
        "unexpected error: {definition_error}"
    );
    assert!(
        !app.status(&workspace)
            .await
            .unwrap()
            .clangd_analyzer_running
    );

    let references_error = app
        .references(&workspace, "revenue.c", 1, 0, true)
        .await
        .unwrap_err();
    assert!(
        references_error.to_string().contains("clangd is disabled"),
        "unexpected error: {references_error}"
    );
    assert!(
        !app.status(&workspace)
            .await
            .unwrap()
            .clangd_analyzer_running
    );
    task.abort();
}

#[tokio::test]
async fn java_only_search_and_navigation_keep_java_lsp_optional() {
    let (temp, config, _fake, task) = fixture().await;
    let workspace = temp.path().join("java-only");
    write(
        &workspace,
        "RevenueService.java",
        "package revenue;\n\n// Computes quarterly revenue summaries.\npublic final class RevenueService {\n    public static int computeQuarterlyRevenueSummary(int[] rows) {\n        int total = 0;\n        for (int row : rows) {\n            total += row;\n        }\n        return total;\n    }\n}\n",
    );
    let app = App::open(config).await.unwrap();
    let indexed = app.index(&workspace).await.unwrap();
    assert_eq!(indexed.files, 1);
    assert_eq!(indexed.parsed_files, 1);

    let report = app
        .search(&workspace, "computeQuarterlyRevenueSummary", Some(4))
        .await
        .unwrap();
    assert_eq!(report.results[0].chunk.language, "java");
    assert_eq!(
        report.results[0].chunk.relative_file_path,
        "RevenueService.java"
    );
    assert!(
        report
            .warning
            .as_deref()
            .is_none_or(|warning| !warning.contains("LSP")),
        "unexpected warning: {:?}",
        report.warning
    );
    assert!(!app.status(&workspace).await.unwrap().java_analyzer_running);

    let definition_error = app
        .definition(&workspace, "RevenueService.java", 1, 0)
        .await
        .unwrap_err();
    assert!(
        definition_error.to_string().contains("jdtls is disabled"),
        "unexpected error: {definition_error}"
    );
    assert!(!app.status(&workspace).await.unwrap().java_analyzer_running);

    let references_error = app
        .references(&workspace, "RevenueService.java", 1, 0, true)
        .await
        .unwrap_err();
    assert!(
        references_error.to_string().contains("jdtls is disabled"),
        "unexpected error: {references_error}"
    );
    assert!(!app.status(&workspace).await.unwrap().java_analyzer_running);
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
