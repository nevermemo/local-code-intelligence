//! Position requests that reach one shared language-server client at the same
//! time for the same file (ASYNC-2 commit A, review fix): a request that finds
//! the file's text already sent must not go out before that send has finished,
//! its wait for the server's diagnostics included (here a wait the test ends;
//! the real one is pinned in `lsp_diagnostics_wait`). The fake is the
//! documents-mode one, which answers
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
    started_fake_with(temp, &[]).await
}

/// [`started_fake`] with more fake-server flags.
async fn started_fake_with(
    temp: &tempfile::TempDir,
    extra: &[&str],
) -> (Arc<JsonRpcClient>, std::path::PathBuf) {
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
        ]
        .into_iter()
        .chain(extra.iter().map(|arg| (*arg).to_owned()))
        .collect(),
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
    /// Ends the sender's diagnostics wait.
    hold: Notify,
    /// Fired by the hook that sent the text, once it is in its wait.
    in_wait: Notify,
    /// What happened, in order.
    log: Mutex<Vec<&'static str>>,
}

/// A position request the way the manager makes one: the adapter's hook, then
/// the request. The hook is a language adapter's, with its wait for the
/// server's diagnostics replaced by a wait the test ends.
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
    if let Some(opened) = opened {
        shared.in_wait.notify_one();
        shared.hold.notified().await;
        // Like a real hook, finish the wait for the publication (the fake's
        // comes at once), so that the next caller is not handed it (N111).
        opened
            .wait_for_diagnostics(&shared.client, LOADED_BUDGET)
            .await;
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
            hold: Notify::new(),
            in_wait: Notify::new(),
            log: Mutex::new(Vec::new()),
        });

        // The first request sends the text and then holds its wait.
        let first = tokio::spawn({
            let (shared, file) = (Arc::clone(&shared), file.clone());
            async move { request(&shared, &file, "first").await }
        });
        shared.in_wait.notified().await;

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
            "no request went out during the diagnostics wait"
        );

        shared.hold.notify_one();
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

/// A caller cancelled while its `didOpen` is being written must not leave the
/// text recorded as sent when that write then fails: the server never got it,
/// so the next caller has to send it. The server stops reading after
/// `initialized` (`--never-read-stdin`) and the text is far larger than a pipe
/// holds, so the write is still going on when the first caller is cancelled
/// and ends only at the transport's write bound. The uri's gate is held by the
/// write itself, so the second caller waits for that end and then finds the
/// text unsent; recorded before the write, it would be taken for sent at once.
#[tokio::test]
async fn a_cancelled_open_whose_write_fails_is_not_taken_for_sent() {
    tokio::time::timeout(LOADED_BUDGET, async {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        write(
            &root,
            "src/Big.py",
            &format!("# {}\n", "x".repeat(4 * 1024 * 1024)),
        );
        let file = root.join("src/Big.py");
        let transport = TransportConfig {
            executable: env!("CARGO_BIN_EXE_fake-lsp-server").to_owned(),
            args: vec![
                "--mode".to_owned(),
                "definition".to_owned(),
                "--never-read-stdin".to_owned(),
                "--state".to_owned(),
                temp.path().join("state").to_string_lossy().into_owned(),
            ],
            working_dir: temp.path().to_owned(),
            // The write bound: a write the server does not take ends here.
            timeout: Duration::from_secs(2),
            label: "fake".to_owned(),
        };
        let client = JsonRpcClient::spawn(&transport, &[], Box::new(|_| false))
            .await
            .unwrap();
        // The server stops reading when it has this.
        client.notify("initialized", json!({})).await.unwrap();
        let sent = SentTexts::default();
        let open = || open_if_changed(&client, &sent, &file, "src/Big.py", "fake", "python");

        let cancelled = tokio::time::timeout(Duration::from_millis(100), open()).await;
        assert!(cancelled.is_err(), "the write was still going on");

        let second = open().await;
        assert!(
            !matches!(second, Ok(None)),
            "the text was taken for sent although its write never finished"
        );
    })
    .await
    .expect("within the outer bound");
}

/// A hook cancelled after its `didOpen` was written (here: the value dropped
/// without waiting) leaves the wait for the server's diagnostics to the next
/// caller of the file (N111). Caller 2 is handed the wait, finishes it, and its
/// request goes out after the publication, with no second `didOpen`; caller 3
/// finds the wait settled and gets nothing. Fails without the pending record
/// (caller 2 gets `None` and its request is read during the pending
/// publication) and without the settle (caller 3 gets the document again).
#[tokio::test]
async fn a_dropped_open_leaves_its_diagnostics_wait_to_the_next_caller() {
    tokio::time::timeout(LOADED_BUDGET, async {
        let temp = tempfile::tempdir().unwrap();
        let (client, root) = started_fake_with(&temp, &["--diagnostics-delay-ms", "300"]).await;
        let state = temp.path().join("state");
        write(&root, "src/Late.cs", LATE_CS);
        let file = root.join("src/Late.cs");
        let sent = SentTexts::default();
        let open = || open_if_changed(&client, &sent, &file, "src/Late.cs", "fake", "csharp");
        let lines = |name: &str| {
            std::fs::read_to_string(state.join(name)).map_or(0, |text| text.lines().count())
        };

        // Caller 1 sent the text and was cancelled before it waited.
        let first = open().await.unwrap();
        assert!(first.is_some(), "the first call sends the text");
        drop(first);

        // Caller 2 is handed the wait and ends it on the publication.
        let resumed = open()
            .await
            .unwrap()
            .expect("the unfinished wait is left to the next caller");
        assert!(
            resumed.wait_for_diagnostics(&client, LOADED_BUDGET).await,
            "the publication came within the cap"
        );
        let column = LATE_CS.lines().nth(1).unwrap().find("Add").unwrap();
        client
            .request(
                "textDocument/definition",
                json!({
                    "textDocument": {"uri": reqwest::Url::from_file_path(&file).unwrap().to_string()},
                    "position": {"line": 1, "character": column},
                }),
            )
            .await
            .unwrap();
        drop(resumed);
        assert_eq!(
            lines("request_during_pending_diagnostics.txt"),
            0,
            "the request went out after the publication"
        );
        assert_eq!(lines("did_open_seen.txt"), 1, "the text was sent once");

        // The wait is settled: nothing is left for caller 3.
        assert!(open().await.unwrap().is_none(), "the wait was settled");
        assert_eq!(client.diagnostics_wait_timeouts(), 0);
    })
    .await
    .expect("within the outer bound");
}

/// A server that publishes empty diagnostics when it closes a document
/// (pyright, clangd; the fake with `--close-diagnostics-on-reopen`, the
/// publication carries no version) must not end the diagnostics wait of the
/// open that follows the close. The 65th file closes the first; the fake holds
/// the close's publication until the first file's `didOpen` comes again, then
/// sends it at once and the re-open's own 1000 ms later. The order is made by
/// the fake's events, not by timing, so it holds under any load: the close's
/// publication is not in before the re-open is written, and the re-open's own
/// comes after it. The wait must end on the latter: three publications counted
/// for the file (open, close, re-open), not two. Fails when the re-open's wait
/// accepts a publication that carries no version.
#[tokio::test]
async fn a_close_publication_does_not_end_a_reopens_diagnostics_wait() {
    tokio::time::timeout(LOADED_BUDGET, async {
        let temp = tempfile::tempdir().unwrap();
        let (client, root) = started_fake_with(
            &temp,
            &[
                "--diagnostics-delay-ms",
                "1000",
                "--close-diagnostics-on-reopen",
            ],
        )
        .await;
        let relative = |number: u32| format!("src/F{number:03}.cs");
        for number in 1..=65 {
            write(
                &root,
                &relative(number),
                &format!(
                    "namespace Acceptance;\npublic sealed class F{number:03} {{ public int Run() => 1; }}\n"
                ),
            );
        }
        let sent = SentTexts::default();
        let open = |number: u32| {
            let (client, sent, root, relative) = (&client, &sent, root.clone(), relative(number));
            async move {
                open_if_changed(
                    client,
                    sent,
                    &root.join(&relative),
                    &relative,
                    "fake",
                    "csharp",
                )
                .await
                .unwrap()
            }
        };
        let first_uri = reqwest::Url::from_file_path(root.join(relative(1)))
            .unwrap()
            .to_string();

        // 64 files fill the table; the 65th closes the first.
        for number in 1..=65 {
            let opened = open(number).await;
            assert!(opened.is_some(), "F{number:03} is sent");
        }
        // The first file's first publication is in; the close's is held by
        // the fake until the re-open below.
        assert!(
            client
                .wait_diagnostics(&first_uri, 0, 1, LOADED_BUDGET)
                .await
        );
        assert_eq!(client.diagnostics_seen(&first_uri), 1);

        let reopened = open(1).await.expect("the closed file is opened again");
        assert!(
            reopened.wait_for_diagnostics(&client, LOADED_BUDGET).await,
            "the re-open's own publication came within the cap"
        );
        assert_eq!(
            client.diagnostics_seen(&first_uri),
            3,
            "open, close and re-open were each published: the wait did not end on the close's"
        );
        assert_eq!(client.diagnostics_wait_timeouts(), 0);
    })
    .await
    .expect("within the outer bound");
}
