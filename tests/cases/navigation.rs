use super::*;
use local_code_intelligence::{
    config::{CSharpLspConfig, ClangdLspConfig, PythonLspConfig, TypeScriptLspConfig},
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

    // Stop waiting 300 ms after the server is up (and primed), i.e. while the
    // slow request is in flight. The call is dropped at the end of the block.
    let finished = {
        let call = manager.symbols(&adapter, &workspace, "Calculator");
        tokio::pin!(call);
        tokio::select! {
            result = &mut call => Some(result),
            () = async {
                while !manager.running("csharp-ls", &workspace.id).await {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                tokio::time::sleep(Duration::from_millis(300)).await;
            } => None,
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
    let heartbeat = |state: &std::path::Path| {
        std::fs::read_to_string(state.join("heartbeat.txt")).unwrap_or_default()
    };
    // The server is alive: its heartbeat moves.
    let beat = heartbeat(&state);
    wait_until(
        "the server's heartbeat to move",
        Duration::from_secs(10),
        async || heartbeat(&state) != beat,
    )
    .await;

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

    // Killed: the heartbeat stops. A live server rewrites it every 50 ms and a
    // dead one cannot, so two equal reads 300 ms apart mean it ended. The
    // kill is already done when `release` returns; the polling only absorbs
    // the operating system's delay in ending the process.
    let give_up = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let before = heartbeat(&state);
        tokio::time::sleep(Duration::from_millis(300)).await;
        let after = heartbeat(&state);
        if before == after {
            break;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the server that ignored shutdown was still alive 10 s after the release \
             (heartbeat {before:?}, then {after:?})"
        );
    }
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
    let app = App::open(with_fake_csharp_server(config, &state, &[]))
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
/// started and failed, the TypeScript one was asked once.
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
    models_task.abort();
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
    let heartbeat = |state: &std::path::Path| {
        std::fs::read_to_string(state.join("heartbeat.txt")).unwrap_or_default()
    };
    let beat = heartbeat(&state);
    wait_until(
        "the server's heartbeat to move",
        Duration::from_secs(10),
        async || heartbeat(&state) != beat,
    )
    .await;

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

    // The server that ignored both was killed: its heartbeat stops (a live one
    // rewrites it every 50 ms).
    let give_up = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let before = heartbeat(&state);
        tokio::time::sleep(Duration::from_millis(300)).await;
        if before == heartbeat(&state) {
            break;
        }
        assert!(
            std::time::Instant::now() < give_up,
            "the server that ignored shutdown was still alive 10 s after the service shut down"
        );
    }
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
