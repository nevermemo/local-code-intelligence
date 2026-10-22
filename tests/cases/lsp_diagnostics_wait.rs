//! What a position request waits for after it sent a file to the language
//! server (ASYNC-2 commit B): the server's `publishDiagnostics` for that file,
//! not a fixed head start. The fake publishes empty diagnostics for every
//! `didOpen` after `--diagnostics-delay-ms` (default 0) and records each in
//! `diagnostics_sent.txt`; with a delay it reads on while it waits, and a
//! document request it reads before the publication is due is recorded as
//! `request_during_pending_diagnostics.txt`. A client that waits for the
//! publication never causes that record; one that does not wait does.

use super::*;
use local_code_intelligence::{
    config::PythonLspConfig,
    lsp::{
        Manager,
        adapter::{LspAdapter, SentTexts},
        python::PythonServer,
        transport::{JsonRpcClient, TransportConfig},
    },
    workspace::Workspace,
};
use std::path::PathBuf;

const CALCULATOR: &str = "class Calculator:\n    def add(self, a, b):\n        return a + b\n";
const CALL_SITE: &str =
    "from .Calculator import Calculator\n\ndef run(value):\n    return value.add(1, 2)\n";

/// How many lines the fake appended to `name` in its state directory (one per
/// event); 0 when there is no file.
fn events(state: &Path, name: &str) -> usize {
    std::fs::read_to_string(state.join(name)).map_or(0, |text| text.lines().count())
}

/// The arguments of the fake as a Python language server (`definition` mode).
fn fake_args(state: &Path, extra: &[&str]) -> Vec<String> {
    ["--mode", "definition", "--answer-unhandled", "--state"]
        .into_iter()
        .map(str::to_owned)
        .chain([state.to_string_lossy().into_owned()])
        .chain(["--primary-file", "src/Calculator.py"].map(str::to_owned))
        .chain(["--secondary-file", "src/CallSite.py"].map(str::to_owned))
        .chain(extra.iter().map(|arg| (*arg).to_owned()))
        .collect()
}

/// A two-file Python workspace, a manager configured with the fake as its
/// Python server, and the fake's state directory.
fn python_fixture(temp: &tempfile::TempDir, extra: &[&str]) -> (Config, Workspace, PathBuf) {
    let workspace_path = temp.path().join("workspace");
    let data_path = temp.path().join("data");
    let state = temp.path().join("state");
    std::fs::create_dir_all(&data_path).unwrap();
    write(&workspace_path, "src/Calculator.py", CALCULATOR);
    write(&workspace_path, "src/CallSite.py", CALL_SITE);
    let workspace = Workspace::resolve(&workspace_path, &data_path).unwrap();
    let config = Config {
        data_dir: data_path,
        lsp_timeout_seconds: LOADED_BUDGET_SECONDS,
        python: PythonLspConfig {
            path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
            args: fake_args(&state, extra),
            disabled: false,
        },
        ..Config::default()
    };
    (config, workspace, state)
}

/// The fake past its handshake, and the Python adapter that waits at most
/// `timeout` for diagnostics.
async fn started_client(
    temp: &tempfile::TempDir,
    extra: &[&str],
    timeout: Duration,
) -> (Arc<JsonRpcClient>, PythonServer, PathBuf) {
    let (_, workspace, state) = python_fixture(temp, extra);
    let transport = TransportConfig {
        executable: env!("CARGO_BIN_EXE_fake-lsp-server").to_owned(),
        args: fake_args(&state, extra),
        working_dir: temp.path().to_owned(),
        timeout: LOADED_BUDGET,
        label: "fake".to_owned(),
    };
    let client = JsonRpcClient::spawn(&transport, &[], Box::new(|_| false))
        .await
        .unwrap();
    let root_uri = reqwest::Url::from_directory_path(&workspace.path)
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
    let adapter = PythonServer::new(
        &PythonLspConfig {
            path: Some(transport.executable.clone()),
            args: Vec::new(),
            disabled: false,
        },
        timeout,
    );
    (client, adapter, workspace.path)
}

fn definition_at(file: &Path) -> Value {
    json!({
        "textDocument": {"uri": reqwest::Url::from_file_path(file).unwrap().to_string()},
        "position": {"line": 1, "character": 1},
    })
}

/// A definition through the manager with a server that publishes 300 ms after
/// each `didOpen`: the request goes out only after its own file's publication
/// (the fake read no document request while one was due), and a second file
/// is waited for on its own publication.
#[tokio::test]
async fn the_request_follows_the_opened_files_diagnostics_not_a_timer() {
    tokio::time::timeout(LOADED_BUDGET, async {
        let temp = tempfile::tempdir().unwrap();
        let (config, workspace, state) = python_fixture(&temp, &["--diagnostics-delay-ms", "300"]);
        let manager = Manager::new(&config);
        let adapter = manager
            .adapters_for_language("python")
            .into_iter()
            .next()
            .cloned()
            .expect("pyright adapter is always constructed");

        for (index, (name, line, column)) in
            [("src/CallSite.py", 3, 22), ("src/Calculator.py", 1, 9)]
                .into_iter()
                .enumerate()
        {
            let results = manager
                .definition(
                    &adapter,
                    &workspace,
                    &workspace.path.join(name),
                    name,
                    line,
                    column,
                )
                .await
                .unwrap();
            assert!(!results.is_empty(), "{name}: the answer is correct");
            assert_eq!(events(&state, "did_open_seen.txt"), index + 1, "{name}");
            assert_eq!(
                events(&state, "diagnostics_sent.txt"),
                index + 1,
                "{name}: the request waited for its own file's publication"
            );
            assert_eq!(
                events(&state, "request_during_pending_diagnostics.txt"),
                0,
                "{name}: a document request reached the server before the publication"
            );
        }
        assert_eq!(
            std::fs::read_dir(state.join("spawns")).unwrap().count(),
            1,
            "one session served both"
        );
    })
    .await
    .expect("within the outer bound");
}

/// Two concurrent requests for one file, the first sending it: the second's
/// definition does not reach the server before the publication either, since
/// the first hook keeps the file's gate through its diagnostics wait.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_request_for_the_file_waits_out_the_diagnostics_wait() {
    tokio::time::timeout(LOADED_BUDGET, async {
        let temp = tempfile::tempdir().unwrap();
        let (client, adapter, root) =
            started_client(&temp, &["--diagnostics-delay-ms", "300"], LOADED_BUDGET).await;
        let state = temp.path().join("state");
        let sent = Arc::new(SentTexts::default());
        let file = root.join("src/CallSite.py");
        let request = || {
            let (client, sent, file) = (Arc::clone(&client), Arc::clone(&sent), file.clone());
            let adapter = &adapter;
            async move {
                adapter
                    .before_position_request(&client, &sent, &file, "src/CallSite.py")
                    .await
                    .unwrap();
                client
                    .request("textDocument/definition", definition_at(&file))
                    .await
                    .unwrap()
            }
        };

        let (first, second) = tokio::join!(request(), request());
        for answer in [first, second] {
            assert!(
                answer["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.ends_with("/src/Calculator.py")),
                "{answer}"
            );
        }
        assert_eq!(events(&state, "did_open_seen.txt"), 1);
        assert_eq!(events(&state, "diagnostics_sent.txt"), 1);
        assert_eq!(
            events(&state, "request_during_pending_diagnostics.txt"),
            0,
            "a definition reached the server before the publication"
        );
        assert_eq!(client.diagnostics_wait_timeouts(), 0);
    })
    .await
    .expect("within the outer bound");
}

/// A regression guard, not a pin: a publication that follows the `didOpen` at
/// once is still the one the hook waits for, because the count it compares
/// against is read before the `didOpen` is written. A hook that read it after
/// the write would lose a publication that beat it back from the write and
/// wait out its cap; this test passed against such an ordering in practice (the
/// server answers much later than the write returns), so it only catches a
/// change that breaks the wait or the uri matching for a fast server.
#[tokio::test]
async fn a_publication_that_beats_the_hook_back_from_the_write_still_ends_its_wait() {
    tokio::time::timeout(LOADED_BUDGET, async {
        let temp = tempfile::tempdir().unwrap();
        let (client, adapter, root) = started_client(&temp, &[], Duration::from_secs(5)).await;
        let sent = SentTexts::default();
        for index in 0..10 {
            let name = format!("src/Extra{index}.py");
            write(&root, &name, &format!("value_{index} = {index}\n"));
            adapter
                .before_position_request(&client, &sent, &root.join(&name), &name)
                .await
                .unwrap();
            assert_eq!(client.diagnostics_wait_timeouts(), 0, "{name}");
            assert_eq!(
                client.diagnostics_seen(
                    reqwest::Url::from_file_path(root.join(&name))
                        .unwrap()
                        .as_str()
                ),
                1,
                "{name}: the reader counted the publication under the file's uri"
            );
        }
    })
    .await
    .expect("within the outer bound");
}

/// A server that never publishes costs one bounded wait for the session, not
/// one per file. The counter proves it; nothing here measures time.
#[tokio::test]
async fn a_server_that_never_publishes_costs_one_wait_per_session() {
    tokio::time::timeout(LOADED_BUDGET, async {
        let temp = tempfile::tempdir().unwrap();
        let (client, adapter, root) =
            started_client(&temp, &["--no-diagnostics"], Duration::from_millis(200)).await;
        let state = temp.path().join("state");
        let sent = SentTexts::default();

        for (index, name) in ["src/Calculator.py", "src/CallSite.py"]
            .into_iter()
            .enumerate()
        {
            adapter
                .before_position_request(&client, &sent, &root.join(name), name)
                .await
                .unwrap();
            // The server has read the `didOpen` once it answers a request
            // that came after it.
            client
                .request("textDocument/definition", definition_at(&root.join(name)))
                .await
                .unwrap();
            assert_eq!(events(&state, "did_open_seen.txt"), index + 1, "{name}");
            assert_eq!(
                client.diagnostics_wait_timeouts(),
                1,
                "{name}: the first file used the session's one wait"
            );
        }
        assert_eq!(events(&state, "diagnostics_sent.txt"), 0);
    })
    .await
    .expect("within the outer bound");
}
