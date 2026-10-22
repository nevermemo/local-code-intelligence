//! Position requests that reach one shared language-server client at the same
//! time for the same file (ASYNC-2 commit A, review fix): a request that finds
//! the file's text already sent must not go out before that send has finished,
//! its head start included. The fake is the documents-mode one, which answers
//! from the text it was sent, so a request that overtakes the `didOpen` is
//! answered empty.

use super::*;
use local_code_intelligence::lsp::{
    adapter::{SentTexts, open_if_changed},
    transport::{JsonRpcClient, TransportConfig},
};
use std::sync::Mutex;
use tokio::sync::Notify;

/// A file the fake's start-up walk never sees: only a `didOpen` makes it known.
const LATE_CS: &str = "namespace Acceptance;\npublic static class Late { public static int Run(Calculator value) => value.Add(1, 2); }\n";

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The documents-mode fake past its handshake and start-up walk, on a
/// workspace that holds `src/Calculator.cs` only.
async fn started_fake(temp: &tempfile::TempDir) -> (Arc<JsonRpcClient>, std::path::PathBuf) {
    let root = temp.path().join("workspace");
    write(
        &root,
        "src/Calculator.cs",
        "namespace Acceptance;\npublic sealed class Calculator { public int Add(int a, int b) => a + b; }\n",
    );
    let transport = TransportConfig {
        executable: env!("CARGO_BIN_EXE_fake-lsp-server").to_owned(),
        args: vec![
            "--mode".to_owned(),
            "documents".to_owned(),
            "--state".to_owned(),
            temp.path().join("state").to_string_lossy().into_owned(),
        ],
        working_dir: temp.path().to_owned(),
        timeout: LOADED_BUDGET,
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
    // The walk runs when `initialized` arrives; a round trip proves it is done.
    client
        .request("workspace/symbol", json!({"query": ""}))
        .await
        .unwrap();
    (client, root)
}

/// What the hooks of both requests share.
struct Shared {
    client: Arc<JsonRpcClient>,
    sent: SentTexts,
    /// Ends the sender's head start.
    head_start: Notify,
    /// Fired by the hook that sent the text, once it holds its head start.
    in_head_start: Notify,
    /// What happened, in order.
    log: Mutex<Vec<&'static str>>,
}

/// A position request the way the manager makes one: the adapter's hook, then
/// the request. The hook is a language adapter's, with its fixed head start
/// replaced by a wait the test ends.
async fn request(shared: &Shared, file: &std::path::Path, name: &'static str) -> Value {
    let opened = open_if_changed(
        &shared.client,
        &shared.sent,
        file,
        "src/Late.cs",
        "fake",
        "csharp",
    )
    .await
    .unwrap();
    if let Some(_opened) = opened {
        shared.in_head_start.notify_one();
        shared.head_start.notified().await;
    }
    lock(&shared.log).push(name);
    let column = LATE_CS.lines().nth(1).unwrap().find("Add").unwrap();
    shared
        .client
        .request(
            "textDocument/definition",
            json!({
                "textDocument": {"uri": reqwest::Url::from_file_path(file).unwrap().to_string()},
                "position": {"line": 1, "character": column},
            }),
        )
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_request_for_a_file_being_opened_waits_for_the_open_to_finish() {
    tokio::time::timeout(LOADED_BUDGET, async {
        let temp = tempfile::tempdir().unwrap();
        let (client, root) = started_fake(&temp).await;
        // Created after the start-up walk: unknown to the server until opened.
        write(&root, "src/Late.cs", LATE_CS);
        let file = root.join("src/Late.cs");
        let uri = reqwest::Url::from_file_path(&file).unwrap().to_string();
        let shared = Arc::new(Shared {
            client,
            sent: SentTexts::default(),
            head_start: Notify::new(),
            in_head_start: Notify::new(),
            log: Mutex::new(Vec::new()),
        });

        // The first request sends the text and then holds its head start.
        let first = tokio::spawn({
            let (shared, file) = (Arc::clone(&shared), file.clone());
            async move { request(&shared, &file, "first").await }
        });
        shared.in_head_start.notified().await;

        // The second finds the text sent. It must park behind the open, not
        // go on to its request.
        let second = tokio::spawn({
            let (shared, file) = (Arc::clone(&shared), file.clone());
            async move { request(&shared, &file, "second").await }
        });
        while shared.sent.waiting(&uri) == 0 && !second.is_finished() {
            tokio::task::yield_now().await;
        }
        assert!(
            !second.is_finished(),
            "the second request ran while the open of its file was in flight"
        );
        assert_eq!(
            *lock(&shared.log),
            Vec::<&str>::new(),
            "no request went out during the head start"
        );

        shared.head_start.notify_one();
        let (first, second) = (first.await.unwrap(), second.await.unwrap());
        for (name, answer) in [("first", &first), ("second", &second)] {
            let places = answer.as_array().unwrap();
            assert!(
                places.iter().any(|place| place["uri"]
                    .as_str()
                    .is_some_and(|uri| uri.ends_with("/src/Calculator.cs"))),
                "{name} answer came from the opened document: {answer}"
            );
        }

        // On the wire: one didOpen, ahead of both definitions.
        let events = std::fs::read_to_string(temp.path().join("state/events.jsonl")).unwrap();
        let methods: Vec<String> = events
            .lines()
            .map(|line| {
                serde_json::from_str::<Value>(line).unwrap()["method"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        let after_walk: Vec<&str> = methods
            .iter()
            .map(String::as_str)
            .skip_while(|method| *method != "workspace/symbol")
            .skip(1)
            .collect();
        assert_eq!(
            after_walk,
            [
                "textDocument/didOpen",
                "textDocument/definition",
                "textDocument/definition"
            ]
        );
    })
    .await
    .expect("within the outer bound");
}
