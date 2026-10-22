pub mod fake_lsp;

use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use local_code_intelligence::{
    app::App,
    config::{Config, IndexFreshness},
    evaluate::{
        EvalQuery, EvalWorkspace, EvaluationFile, QueryReport, ResultEvidence, aggregate,
        has_failures, load_definition, parse_workspace_mappings, query_metrics,
    },
    models::QUERY_INSTRUCTION,
};
use serde_json::{Value, json};
use std::{
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

/// Seconds a test may give something that is slow only on a loaded machine: a
/// fake language server's start (`lsp_timeout_seconds`, a timeout of
/// inactivity), a readiness probe of the fake model services, or the cap of a
/// wait on a real signal. Parallel cargo builds on a developer machine, and
/// the slow Windows CI runner, stretched 5 s and 10 s budgets past their limit.
/// It is for tests that assert behaviour, not speed: a test whose point is a
/// timeout keeps its own short value and widens only its outer wait.
const LOADED_BUDGET_SECONDS: u64 = 30;

/// [`LOADED_BUDGET_SECONDS`] as a duration.
const LOADED_BUDGET: Duration = Duration::from_secs(LOADED_BUDGET_SECONDS);

/// Ports a test chose and then released for a server or a probe to take. The
/// OS may hand a released port to the next `127.0.0.1:0` bind, and tests run
/// in parallel threads: a client polling the chosen port would then reach that
/// other test's socket (and a server would fail to bind). Every ephemeral bind
/// in this binary goes through [`bind_unreleased`], which skips these ports.
fn released_ports() -> &'static std::sync::Mutex<std::collections::HashSet<u16>> {
    static PORTS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<u16>>> =
        std::sync::OnceLock::new();
    PORTS.get_or_init(Default::default)
}

/// Calls `bind` (an ephemeral loopback bind) until the port it got is not one
/// [`free_ports`] released. Rejected sockets stay held while it retries, so
/// the same port is not drawn again.
fn bind_unreleased<T>(bind: impl Fn() -> T, port_of: impl Fn(&T) -> u16) -> T {
    let mut rejected = Vec::new();
    loop {
        let candidate = bind();
        let released = released_ports()
            .lock()
            .unwrap()
            .contains(&port_of(&candidate));
        if !released {
            return candidate;
        }
        rejected.push(candidate);
    }
}

/// A loopback listener on an ephemeral port no test has released.
fn loopback_listener() -> std::net::TcpListener {
    bind_unreleased(
        || std::net::TcpListener::bind("127.0.0.1:0").unwrap(),
        |listener| listener.local_addr().unwrap().port(),
    )
}

/// [`loopback_listener`] as a tokio listener; call it inside the runtime.
fn loopback_tokio_listener() -> tokio::net::TcpListener {
    let listener = loopback_listener();
    listener.set_nonblocking(true).unwrap();
    tokio::net::TcpListener::from_std(listener).unwrap()
}

/// Ports for a server or a probe to take, all distinct: they are held together
/// until every one is chosen, then released and recorded so that no later
/// ephemeral bind in this binary takes one. Nothing outside the test process
/// is guarded against.
fn free_ports<const N: usize>() -> [u16; N] {
    let held: Vec<std::net::TcpListener> = (0..N).map(|_| loopback_listener()).collect();
    let mut ports = [0; N];
    for (port, listener) in ports.iter_mut().zip(&held) {
        *port = listener.local_addr().unwrap().port();
    }
    // Recorded before `held` drops, while the OS still keeps the ports unique.
    released_ports().lock().unwrap().extend(ports);
    ports
}

/// A loopback port that refuses connections for as long as this value lives.
/// Its socket is bound but never listens: a connection to it is refused, as
/// to a port nothing holds, and no other socket can take the port meanwhile,
/// neither another test's `127.0.0.1:0` bind nor a server that wrongly chose
/// it. Binding a listener to learn a free port and dropping it guarantees
/// neither: the next ephemeral bind of any test may get that port.
///
/// On macOS a connection to a bound socket that does not listen is not
/// refused: the system drops it and the client waits for its timeout. There
/// the port is 1 (`tcpmux`) and nothing is held: no service listens on it and
/// an ephemeral bind never returns it, though a test cannot stop another
/// process from binding it.
struct ClosedPort {
    #[cfg(not(target_os = "macos"))]
    _socket: tokio::net::TcpSocket,
    port: u16,
}

impl ClosedPort {
    #[cfg(not(target_os = "macos"))]
    fn new() -> Self {
        let socket = bind_unreleased(
            || {
                let socket = tokio::net::TcpSocket::new_v4().unwrap();
                socket
                    .bind(std::net::SocketAddr::from(([127, 0, 0, 1], 0)))
                    .unwrap();
                socket
            },
            |socket| socket.local_addr().unwrap().port(),
        );
        let port = socket.local_addr().unwrap().port();
        Self {
            _socket: socket,
            port,
        }
    }

    #[cfg(target_os = "macos")]
    fn new() -> Self {
        Self { port: 1 }
    }
}

#[derive(Default)]
struct FakeModels {
    documents: AtomicUsize,
    document_requests: AtomicUsize,
    /// Every request to `POST /v1/embeddings`, served or failed.
    embed_requests: AtomicUsize,
    queries: AtomicUsize,
    embed_delay_milliseconds: AtomicUsize,
    hold_documents: AtomicBool,
    document_held: tokio::sync::Notify,
    release_documents: tokio::sync::Notify,
    rerank_fails: AtomicBool,
    embed_fails: AtomicBool,
    malformed_rerank: AtomicBool,
    /// When nonzero, any embedding input longer than this many bytes is
    /// rejected with HTTP 400, like vLLM's context-length validation.
    max_embed_input_bytes: AtomicUsize,
    /// When set, the reranker accepts a request but never answers in time.
    rerank_hangs: AtomicBool,
    rerank_requests: AtomicUsize,
    /// Longest single document (in bytes) any rerank request has carried.
    max_rerank_document_bytes: AtomicUsize,
    /// `POST /v1/systemone` (the typed rescorer): requests served, what the
    /// next ones do (a [`SystemOneMode`] as `usize`) and the latest body.
    system_one_requests: AtomicUsize,
    system_one_mode: AtomicUsize,
    system_one_last: std::sync::Mutex<Option<Value>>,
    /// Override the typed response to exercise malformed and echoed bodies.
    system_one_response: std::sync::Mutex<Option<(StatusCode, Value)>>,
    /// Override the typed response with an exact raw body (a status and
    /// text), for echoes a JSON serializer would not produce.
    system_one_raw_response: std::sync::Mutex<Option<(StatusCode, String)>>,
    /// When nonzero, every embedding is padded with zeros to this many
    /// dimensions (the default vectors have three), as if the service behind
    /// the same URL and model name started returning another width.
    embedding_width: AtomicUsize,
}

/// What the fake typed service does with a request.
#[repr(usize)]
#[derive(Clone, Copy)]
enum SystemOneMode {
    /// Answers every question: `unrelated_helper` 0.9, `translator` 0.2,
    /// anything else 0.1.
    Answer,
    /// HTTP 503.
    Fail,
    /// Answers every question but the last.
    Partial,
    /// Accepts the request and never answers in time.
    Hang,
    /// HTTP 422, the typed service's row or state limit.
    TooLong,
}

fn evaluation_query() -> EvalQuery {
    EvalQuery {
        query: "find evaluator".to_owned(),
        workspace: "self".to_owned(),
        top_k: Some(8),
        filters: None,
        expected_paths: vec!["src/evaluate.rs".to_owned()],
        preferred_paths: Vec::new(),
        disfavored_paths: Vec::new(),
        expected_roles: vec!["source".to_owned()],
        required_snippet: Some("query_metrics".to_owned()),
        tags: Vec::new(),
    }
}

fn evaluation_evidence(path: &str, role: &str, snippet: &str) -> ResultEvidence {
    ResultEvidence {
        rank: 1,
        relative_file_path: path.to_owned(),
        language: "rust".to_owned(),
        source_role: role.to_owned(),
        start_line: 1,
        end_line: 10,
        snippet: snippet.to_owned(),
        reranker_score: Some(0.9),
    }
}

async fn embed(
    State(state): State<Arc<FakeModels>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, String)> {
    state.embed_requests.fetch_add(1, Ordering::SeqCst);
    if state.embed_fails.load(Ordering::SeqCst) {
        return Err((StatusCode::SERVICE_UNAVAILABLE, String::new()));
    }
    let inputs = body["input"].as_array().unwrap();
    let max_input = state.max_embed_input_bytes.load(Ordering::SeqCst);
    if max_input > 0
        && inputs
            .iter()
            .any(|text| text.as_str().unwrap().len() > max_input)
    {
        return Err((
            StatusCode::BAD_REQUEST,
            format!("This model's maximum context length is {max_input} bytes"),
        ));
    }
    if inputs
        .iter()
        .any(|text| !text.as_str().unwrap().starts_with(QUERY_INSTRUCTION))
    {
        state.document_requests.fetch_add(1, Ordering::SeqCst);
        if state.hold_documents.load(Ordering::SeqCst) {
            state.document_held.notify_one();
            state.release_documents.notified().await;
        }
        let delay = state.embed_delay_milliseconds.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(Duration::from_millis(delay as u64)).await;
        }
    }
    let mut data = Vec::new();
    for (index, text) in inputs.iter().enumerate() {
        let text = text.as_str().unwrap();
        if text.starts_with(QUERY_INSTRUCTION) {
            state.queries.fetch_add(1, Ordering::SeqCst);
        } else {
            state.documents.fetch_add(1, Ordering::SeqCst);
            assert!(!text.starts_with("Instruct:"));
        }
        let mut vector = if text.contains("translator") {
            vec![1.0, 0.0, 0.0]
        } else if text.contains("buildProductionTelemetryPipeline") {
            vec![0.0, 1.0, 0.0]
        } else if let Some(step) = text
            .split("golden_rank_")
            .nth(1)
            .and_then(|rest| rest.chars().next()?.to_digit(10))
        {
            // A text marked `golden_rank_<d>` gets its own vector, so a test
            // can give every chunk a distinct cosine distance (no ties).
            vec![0.1 * step as f32, 0.0, 1.0]
        } else {
            vec![0.1, 0.1, 1.0]
        };
        let width = state.embedding_width.load(Ordering::SeqCst);
        if width > vector.len() {
            vector.resize(width, 0.0);
        }
        data.push(json!({"index":index, "embedding":vector}));
    }
    // OpenAI-compatible services may return embeddings out of order.
    data.reverse();
    Ok(Json(json!({"data":data})))
}

async fn model_endpoint() -> Json<Value> {
    Json(json!({"status":"ok"}))
}

async fn models() -> Json<Value> {
    Json(json!({"data":[
        {"id":"qwen3-embedding-4b"},
        {"id":"qwen3-reranker-4b"}
    ]}))
}

async fn rerank(
    State(state): State<Arc<FakeModels>>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, StatusCode> {
    state.rerank_requests.fetch_add(1, Ordering::SeqCst);
    if state.rerank_hangs.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    if state.rerank_fails.load(Ordering::SeqCst) {
        return Err(StatusCode::SERVICE_UNAVAILABLE);
    }
    if state.malformed_rerank.load(Ordering::SeqCst) {
        return Ok(Json(
            json!({"results":[{"index":999,"relevance_score":1.0}]}),
        ));
    }
    for document in body["documents"].as_array().unwrap() {
        state
            .max_rerank_document_bytes
            .fetch_max(document.as_str().unwrap().len(), Ordering::SeqCst);
    }
    let query = body["query"].as_str().unwrap_or("");
    let results: Vec<_> = body["documents"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let document = d.as_str().unwrap();
            let preferred_implementation = (query == "buildProductionTelemetryPipeline"
                && document.contains("src/telemetry.ts\n")
                && document.contains("function buildProductionTelemetryPipeline"))
                || (query == "build_production_feature_pipeline"
                    && document.contains("src/feature_pipeline.py\n")
                    && document.contains("def build_production_feature_pipeline"));
            let score = if preferred_implementation {
                0.995
            } else if document.contains("translator") {
                0.99
            } else {
                0.1
            };
            json!({"index":i,"relevance_score":score})
        })
        .collect();
    Ok(Json(json!({"results":results})))
}

async fn system_one(
    State(state): State<Arc<FakeModels>>,
    Json(body): Json<Value>,
) -> axum::response::Response {
    use axum::response::IntoResponse;

    state.system_one_requests.fetch_add(1, Ordering::SeqCst);
    *state.system_one_last.lock().unwrap() = Some(body.clone());
    if let Some((status, raw)) = state.system_one_raw_response.lock().unwrap().clone() {
        return (
            status,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            raw,
        )
            .into_response();
    }
    if let Some((status, body)) = state.system_one_response.lock().unwrap().clone() {
        return (status, Json(body)).into_response();
    }
    let mode = state.system_one_mode.load(Ordering::SeqCst);
    if mode == SystemOneMode::Fail as usize {
        return (StatusCode::SERVICE_UNAVAILABLE, Json(Value::Null)).into_response();
    }
    if mode == SystemOneMode::TooLong as usize {
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(Value::Null)).into_response();
    }
    if mode == SystemOneMode::Hang as usize {
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
    let mut answers = serde_json::Map::new();
    for (id, candidate) in body["state"]["candidates"].as_object().unwrap() {
        let snippet = candidate["snippet"].as_str().unwrap();
        let probability = if snippet.contains("unrelated_helper") {
            0.9
        } else if snippet.contains("translator") {
            0.2
        } else {
            0.1
        };
        answers.insert(format!("q_{id}"), json!({"noul": probability}));
    }
    if mode == SystemOneMode::Partial as usize {
        let last = answers.keys().next_back().unwrap().clone();
        answers.remove(&last);
    }
    Json(json!({"model": "fake-decider-1", "answers": answers})).into_response()
}

async fn fixture() -> (
    tempfile::TempDir,
    Config,
    Arc<FakeModels>,
    tokio::task::JoinHandle<()>,
) {
    let temp = tempfile::tempdir().unwrap();
    let state = Arc::new(FakeModels::default());
    let router = Router::new()
        .route("/v1", get(model_endpoint))
        .route("/v1/models", get(models))
        .route("/v1/embeddings", post(embed))
        .route("/rerank", get(model_endpoint).post(rerank))
        .route("/v1/systemone", post(system_one))
        .with_state(state.clone());
    let listener = loopback_tokio_listener();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let config = Config {
        data_dir: temp.path().join("data"),
        embedding_url: format!("http://{address}/v1"),
        reranker_url: format!("http://{address}/rerank"),
        rust_analyzer_path: "definitely-missing-rust-analyzer".into(),
        ..Config::default()
    };
    // The shipped defaults leave Markdown unindexed and the docs lane
    // unreranked; most tests exercise Markdown and the docs lane, so the
    // fixture opts into both. Tests of the defaults reset these two switches.
    let mut config = config;
    config.index.prose = true;
    config.docs_lane.reranker = true;
    (temp, config, state, task)
}

fn write(root: &Path, name: &str, content: &str) {
    let path = root.join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A two-file C# workspace for the fake language server (see
/// [`with_fake_csharp_server`]); its `Calculator` is what the fake answers.
fn write_csharp_workspace(root: &Path) {
    write(
        root,
        "src/Calculator.cs",
        "namespace Acceptance;\npublic sealed class Calculator { public int Add(int a, int b) => a + b; }\n",
    );
    write(
        root,
        "src/CallSite.cs",
        "namespace Acceptance;\npublic static class CallSite { public static int Run(Calculator value) => value.Add(1, 2); }\n",
    );
}

/// `config` with the C# language server replaced by the fake one (see
/// `fake_lsp.rs`) in `workspace-symbols` mode, recording what it saw under
/// `state_dir`. `extra_args` are more fake-server flags, for example
/// `--ignore-shutdown`. `timeout_seconds` is the request timeout: a test that
/// does not depend on it passes [`LOADED_BUDGET_SECONDS`], so that a slow
/// process start under load does not fail `initialize` (5 s and 10 s did), and
/// a test that measures timing passes the value it was written with. Keep
/// `config` from [`fixture`]: it points the model services at the fake ones,
/// never at a real port.
fn with_fake_csharp_server(
    mut config: Config,
    state_dir: &Path,
    extra_args: &[&str],
    timeout_seconds: u64,
) -> Config {
    config.lsp_timeout_seconds = timeout_seconds;
    // Also bounds the version probe of `lsp.live` (5 s failed under load).
    config.readiness_timeout_seconds = LOADED_BUDGET_SECONDS;
    config.csharp = local_code_intelligence::config::CSharpLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: ["--mode", "workspace-symbols", "--state"]
            .into_iter()
            .map(str::to_owned)
            .chain([state_dir.to_string_lossy().into_owned()])
            .chain(extra_args.iter().map(|arg| (*arg).to_owned()))
            .collect(),
        disabled: false,
    };
    config
}

/// [`with_fake_csharp_server`] in `documents` mode: the fake answers from the
/// text it was sent (a start-up walk of the workspace root, then the document
/// notifications) instead of canned ranges, and records `events.jsonl` and
/// `documents.json` under `state_dir` (see `fake_lsp_documents.rs`).
fn with_fake_documents_server(
    mut config: Config,
    state_dir: &Path,
    extra_args: &[&str],
    timeout_seconds: u64,
) -> Config {
    config.lsp_timeout_seconds = timeout_seconds;
    // Also bounds the version probe of `lsp.live` (5 s failed under load).
    config.readiness_timeout_seconds = LOADED_BUDGET_SECONDS;
    config.csharp = local_code_intelligence::config::CSharpLspConfig {
        path: Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned()),
        args: ["--mode", "documents", "--state"]
            .into_iter()
            .map(str::to_owned)
            .chain([state_dir.to_string_lossy().into_owned()])
            .chain(extra_args.iter().map(|arg| (*arg).to_owned()))
            .collect(),
        disabled: false,
    };
    config
}

/// [`with_fake_documents_server`] with the documents fake configured as the
/// `language` server (`"python"`) instead of csharp-ls, so a hook that opens
/// documents is exercised against the fake that answers from the text it was
/// sent. csharp-ls and rust-analyzer send no document at all. The workspace
/// keeps its `.cs` files (the fake's scanner reads those, the adapter checks
/// no extension) and the manager is called with the adapter directly.
fn with_fake_documents_server_as(
    mut config: Config,
    language: &str,
    state_dir: &Path,
    extra_args: &[&str],
    timeout_seconds: u64,
) -> Config {
    config.lsp_timeout_seconds = timeout_seconds;
    config.readiness_timeout_seconds = LOADED_BUDGET_SECONDS;
    let path = Some(env!("CARGO_BIN_EXE_fake-lsp-server").to_owned());
    let args = ["--mode", "documents", "--state"]
        .into_iter()
        .map(str::to_owned)
        .chain([state_dir.to_string_lossy().into_owned()])
        .chain(extra_args.iter().map(|arg| (*arg).to_owned()))
        .collect();
    match language {
        "python" => {
            config.python = local_code_intelligence::config::PythonLspConfig {
                path,
                args,
                disabled: false,
            };
        }
        other => panic!("no documents fake as {other}"),
    }
    config
}

/// `config` with rust-analyzer replaced by a copy of the fake language server in
/// `documents` mode with rust-analyzer's status notifications
/// (`--status-cycle --status-shape rust`: `experimental/serverStatus` with
/// `quiescent` true after `initialized`, which the adapter's readiness wait
/// ends on), recording under `state_dir`; `extra` are more fake-server flags.
/// rust-analyzer takes no command-line arguments, so the copy
/// `<temp>/rust-analyzer-fake(.exe)` reads its arguments from the sidecar
/// `rust-analyzer-fake.args` beside it (see `fake_lsp.rs`); the copy and its
/// sidecar are the test's own, so no two tests share either. Keep `config` from
/// [`fixture`].
#[allow(dead_code)]
fn with_fake_rust_analyzer(
    mut config: Config,
    temp: &Path,
    state_dir: &Path,
    extra: &[&str],
) -> Config {
    let suffix = if cfg!(windows) { ".exe" } else { "" };
    let copy = temp.join(format!("rust-analyzer-fake{suffix}"));
    std::fs::copy(env!("CARGO_BIN_EXE_fake-lsp-server"), &copy).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let state = state_dir.to_string_lossy().into_owned();
    let arguments: Vec<String> = [
        "--mode",
        "documents",
        "--state",
        state.as_str(),
        "--status-cycle",
        "--status-shape",
        "rust",
        "--ignore-watched-files",
    ]
    .into_iter()
    .chain(extra.iter().copied())
    .map(str::to_owned)
    .collect();
    std::fs::write(
        temp.join("rust-analyzer-fake.args"),
        arguments.join("\n") + "\n",
    )
    .unwrap();
    config.lsp_timeout_seconds = LOADED_BUDGET_SECONDS;
    config.readiness_timeout_seconds = LOADED_BUDGET_SECONDS;
    config.rust_analyzer_path = copy.to_string_lossy().into_owned();
    config
}

/// Removes every `LCI_*` variable the test process has from the environment
/// `command` will spawn with, so what a developer's own shell exports
/// (`LCI_CONFIG`, `LCI_PORT`, `LCI_DATA_DIR`, `LCI_CANDIDATE_DUMP`, any
/// future one) cannot change a CLI result. Call it before setting any
/// variable the test itself needs. Names are compared case-insensitively
/// because Windows treats them so.
fn strip_lci_env(command: &mut std::process::Command) -> &mut std::process::Command {
    strip_lci_env_named(command, std::env::vars_os().map(|(name, _)| name))
}

/// [`strip_lci_env`] over an explicit list of variable names.
fn strip_lci_env_named(
    command: &mut std::process::Command,
    names: impl IntoIterator<Item = std::ffi::OsString>,
) -> &mut std::process::Command {
    for name in names {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("LCI_")
        {
            command.env_remove(name);
        }
    }
    command
}

/// Notes without the ones the tree barrier adds for a language server a call
/// found running or restarted (`lsp: <provider> kept session; ...`,
/// `lsp: <provider> restarted: ...`, N104, N109): which of two calls starts
/// the server decides which of them carries such a note, so tests that compare
/// two calls' notes compare them without these.
#[allow(dead_code)]
fn without_barrier_notes(notes: &[String]) -> Vec<String> {
    notes
        .iter()
        .filter(|note| !note.starts_with("lsp: "))
        .cloned()
        .collect()
}
