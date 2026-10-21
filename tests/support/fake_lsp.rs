use serde_json::json;
use std::{
    env, fs,
    io::{self, Read, Write},
    path::{Path, PathBuf},
    process, str, thread,
    time::{Duration, Instant},
};

// Shared by the test crate (which includes this file through `support/mod.rs`)
// and the `fake-lsp-server` binary: `#[path]` on a `mod` declaration outside an
// inline module resolves next to this file in both.
#[path = "fake_lsp_documents.rs"]
mod documents;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeScenario {
    Initialized,
    WorkspaceSymbols,
    /// Answers every `workspace/symbol` request with an empty list: a healthy
    /// server with no match (`--mode no-symbols`).
    NoSymbols,
    Definition,
    References,
    /// Answers both `textDocument/definition` and `textDocument/references`
    /// with the canned locations of `definition` and `references` (`--mode
    /// definition-references`): one server for a test that asks both.
    DefinitionAndReferences,
    UnsolicitedNotification,
    Stderr,
    Timeout,
    /// Answers `workspace/symbol` slower than the configured request timeout
    /// in TOTAL wall-clock time, but never goes longer than the timeout
    /// between messages: several `$/progress` notifications, each timeout/2
    /// apart, followed by the real response. Exercises `JsonRpcClient`'s
    /// idle-bounded (not duration-bounded) request wait -- a single blind
    /// per-call timeout would fail this exchange; resetting the deadline on
    /// any observed server activity should not.
    SlowWithProgress,
    Malformed,
    ExitDuringRequest,
    RestartSuccess,
    /// Initializes normally, then answers every request after `initialized`
    /// with a JSON-RPC internal error and keeps running (`--mode
    /// error-response`): a server that fails without exiting, hanging, or
    /// breaking the protocol.
    ErrorResponse,
    /// Answers from the documents it was sent instead of canned ranges
    /// (`--mode documents`): a start-up walk of the workspace root, then
    /// `didOpen`/`didChange`/`didClose`/`didChangeWatchedFiles`, and a toy
    /// parser behind every navigation request. See `fake_lsp_documents.rs`.
    /// `--primary-file` and `--secondary-file` are not used.
    Documents,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeLspConfig {
    pub scenario: FakeScenario,
    pub state_dir: Option<PathBuf>,
    pub process_id: Option<u32>,
    pub exit_code: Option<i32>,
    /// Workspace-relative path used to build the primary canned location
    /// (definition target, first symbol/reference result). Defaults to a C#
    /// fixture path for backward compatibility; other adapters' tests pass
    /// a path with their own extension so `locations_for`'s per-file
    /// language derivation resolves to the language under test.
    pub primary_file: String,
    /// Workspace-relative path used to build the secondary canned location
    /// (the second `references` result, the call site).
    pub secondary_file: String,
    /// Milliseconds to sleep before answering `initialize`, in every
    /// scenario: a language server that is slow to start (`--initialize-delay-ms`).
    pub initialize_delay_ms: u64,
    /// Milliseconds to sleep before answering the FIRST `workspace/symbol`
    /// request of the process (`--first-symbol-delay-ms`): the request LCI
    /// sends to prime a server, like rust-analyzer building its symbol index.
    pub first_symbol_delay_ms: u64,
    /// Milliseconds to sleep before answering every LATER `workspace/symbol`
    /// request (`--symbol-delay-ms`).
    pub symbol_delay_ms: u64,
    /// Milliseconds to sleep before answering `textDocument/definition`
    /// (`--definition-delay-ms`).
    pub definition_delay_ms: u64,
    /// The first process exits on its first definition request
    /// (`--exit-on-definition-request`), leaving the replacement healthy.
    pub exit_on_definition_request: bool,
    /// A server that never shuts down (`--ignore-shutdown`): it still records
    /// the `shutdown` request and the `exit` notification, but sends no
    /// answer to the first and keeps running after the second, so only a
    /// kill ends it.
    pub ignore_shutdown: bool,
    /// A request the scenario does not handle (a `workspace/symbol` to a
    /// `definition` server, say) gets a `null` result (`--answer-unhandled`)
    /// instead of no answer, so a test that needs only some answer does not
    /// wait out the request timeout.
    pub answer_unhandled: bool,
    /// An error response for `workspace/symbol` requests (`--symbol-error
    /// CODE:MESSAGE`, with `--symbol-error-count N` to stop after N). The
    /// first `workspace/symbol` request of the process is the priming one and
    /// is always answered normally.
    pub symbol_error: Option<InjectedError>,
    /// The Nth `workspace/symbol` request of the process (the priming one is
    /// number 1) is answered normally although `symbol_error` is set
    /// (`--symbol-ok-at N`; 0 = none): a server that fails, answers once, then
    /// fails again.
    pub symbol_ok_at: u64,
    /// Milliseconds to sleep before answering the `symbol_ok_at` request
    /// (`--symbol-ok-delay-ms`): a result that arrives late, between two
    /// failing calls, on top of `symbol_delay_ms`.
    pub symbol_ok_delay_ms: u64,
    /// The `slow-with-progress` scenario's pacing: how many `$/progress`
    /// notifications come before the answer (`--progress-count`, default 4) and
    /// how many milliseconds apart (`--progress-gap-ms`, default 400).
    pub progress_count: u64,
    pub progress_gap_ms: u64,
    /// With `--progress-skip-priming` the first `workspace/symbol` request (the
    /// priming one) is answered at once and only the later ones are paced.
    pub progress_skip_priming: bool,
    /// Sends a `$/progress` notification before each injected `workspace/symbol`
    /// error (`--progress-before-error`): a server that is still loading and
    /// says so while it answers "ask again".
    pub progress_before_error: bool,
    /// An error response for `textDocument/definition` requests
    /// (`--definition-error CODE:MESSAGE`, `--definition-error-count N`).
    pub definition_error: Option<InjectedError>,
    /// A raw `error` member sent as given for `workspace/symbol` requests after
    /// the priming one, in the first process of a state directory only
    /// (`--symbol-raw-error JSON`): a response whose `error` is not a
    /// well-formed error object (`null`, no `code`, a string `code`, ...), which
    /// the client must not mistake for a server's answer.
    pub symbol_raw_error: Option<serde_json::Value>,
    /// The process exits when the Nth `workspace/symbol` request arrives,
    /// without answering it, but only in the first process of a state
    /// directory (`--exit-on-symbol-request N`; 0 = never): a server that dies
    /// while a request is in flight, replaced by a healthy one.
    pub exit_on_symbol_request: u64,
    /// The Nth `workspace/symbol` request of the first process of a state
    /// directory is answered with the header of a frame that announces 4 GiB
    /// and no body, and the process keeps running (`--huge-frame-on-symbol-request
    /// N`; 0 = never): a server whose output the client must refuse by its
    /// announced length, not by waiting for the body.
    pub huge_frame_on_symbol_request: u64,
    /// The first process of a state directory stops reading its input after
    /// `initialized` and exits 60 seconds later (`--never-read-stdin`): a
    /// server that is alive and silent while the client's writes fill the
    /// pipe.
    pub never_read_stdin: bool,
    /// Starts a descendant (`--spawn-grandchild`): a copy of this program in
    /// sleeper mode, the way a `.cmd` shim's `node.exe` or a `rustup` proxy's
    /// real binary is a child of the process the client spawned. Its heartbeat
    /// is `sleeper_<N>.txt` in the state directory for the Nth process started,
    /// and its process id is `sleeper_<N>.pid`.
    pub spawn_grandchild: bool,
    /// Keep running after stdin closes, to model servers that outlive their client.
    pub keep_alive_on_eof: bool,
    /// Sends the `custom/ready` notification after `initialized` in every
    /// scenario (`--ready-notification`), the way `Initialized` always does:
    /// for an adapter whose readiness predicate matches it.
    pub ready_notification: bool,
    /// After `initialized`, sends two requests whose ids are strings
    /// (`--string-id-server-requests`): `custom/ready` with the id `ready-1`
    /// and `workspace/configuration` (two items) with the id `cfg-1`. Each
    /// answer the client gives is appended to `server_request_replies.txt` in
    /// the state directory as `<id> <result>`.
    pub string_id_server_requests: bool,
}

/// An error response the fake sends instead of an answer: `code` and
/// `message`, for the first `remaining` requests that reach it (all of them
/// when `None`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InjectedError {
    pub code: i64,
    pub message: String,
    pub remaining: Option<u64>,
}

impl InjectedError {
    /// Reads `flag CODE:MESSAGE` and `count_flag N` from the command line.
    fn from_cli(args: &[String], flag: &str, count_flag: &str) -> Option<Self> {
        let spec = cli_arg(args, flag)?;
        let (code, message) = spec.split_once(':')?;
        Some(Self {
            code: code.parse().ok()?,
            message: message.to_owned(),
            remaining: cli_arg(args, count_flag).and_then(|count| count.parse().ok()),
        })
    }

    /// Whether this request gets the error, counting it against `remaining`.
    fn take(&mut self) -> bool {
        match &mut self.remaining {
            None => true,
            Some(0) => false,
            Some(left) => {
                *left -= 1;
                true
            }
        }
    }
}

pub fn run_from_env() -> io::Result<()> {
    let cli: Vec<String> = env::args().collect();
    // Sleeper mode is not a language server at all: see `spawn_sleeper`.
    if let Some(path) = cli_arg(&cli, "--sleeper") {
        run_sleeper(Path::new(&path));
        return Ok(());
    }
    // CLI args take precedence over env vars so that integration tests can
    // drive the scenario without polluting the process-wide environment (which
    // would leak across tests and is `unsafe` in edition 2024). The existing
    // env-var contract is preserved as a fallback for callers that do not pass
    // CLI args (e.g. the in-module smoke test).
    let mode = cli_arg(&cli, "--mode")
        .or_else(|| env::var("FAKE_LSP_MODE").ok())
        .unwrap_or_else(|| "initialized".to_string());
    let state_path = cli_arg(&cli, "--state")
        .map(PathBuf::from)
        .or_else(|| env::var_os("FAKE_LSP_STATE").map(PathBuf::from));
    let process_id = env::var("FAKE_LSP_PROCESS_ID")
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    let scenario = match mode.as_str() {
        "initialized" => FakeScenario::Initialized,
        "workspace-symbols" => FakeScenario::WorkspaceSymbols,
        "no-symbols" => FakeScenario::NoSymbols,
        "definition" => FakeScenario::Definition,
        "references" => FakeScenario::References,
        "definition-references" => FakeScenario::DefinitionAndReferences,
        "unsolicited" => FakeScenario::UnsolicitedNotification,
        "stderr" => FakeScenario::Stderr,
        "timeout" => FakeScenario::Timeout,
        "slow-with-progress" => FakeScenario::SlowWithProgress,
        "malformed" => FakeScenario::Malformed,
        "exit-during-request" => FakeScenario::ExitDuringRequest,
        "restart-success" => FakeScenario::RestartSuccess,
        "error-response" => FakeScenario::ErrorResponse,
        "documents" => FakeScenario::Documents,
        other => {
            eprintln!("unexpected FAKE_LSP_MODE: {other}");
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown mode: {other}"),
            ));
        }
    };
    let primary_file =
        cli_arg(&cli, "--primary-file").unwrap_or_else(|| "src/Calculator.cs".to_string());
    let secondary_file =
        cli_arg(&cli, "--secondary-file").unwrap_or_else(|| "src/CallSite.cs".to_string());
    let initialize_delay_ms = cli_arg(&cli, "--initialize-delay-ms")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let first_symbol_delay_ms = cli_arg(&cli, "--first-symbol-delay-ms")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let symbol_delay_ms = cli_arg(&cli, "--symbol-delay-ms")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let ignore_shutdown = cli.iter().any(|arg| arg == "--ignore-shutdown");
    let symbol_ok_at = cli_arg(&cli, "--symbol-ok-at")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let symbol_ok_delay_ms = cli_arg(&cli, "--symbol-ok-delay-ms")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let progress_count = cli_arg(&cli, "--progress-count")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(4);
    let progress_gap_ms = cli_arg(&cli, "--progress-gap-ms")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(400);
    let exit_on_symbol_request = cli_arg(&cli, "--exit-on-symbol-request")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let huge_frame_on_symbol_request = cli_arg(&cli, "--huge-frame-on-symbol-request")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let config = FakeLspConfig {
        scenario,
        state_dir: state_path,
        process_id,
        exit_code: None,
        primary_file,
        secondary_file,
        initialize_delay_ms,
        first_symbol_delay_ms,
        symbol_delay_ms,
        definition_delay_ms: cli_arg(&cli, "--definition-delay-ms")
            .and_then(|value| value.parse().ok())
            .unwrap_or(0),
        exit_on_definition_request: cli.iter().any(|arg| arg == "--exit-on-definition-request"),
        answer_unhandled: cli.iter().any(|arg| arg == "--answer-unhandled"),
        ignore_shutdown,
        symbol_error: InjectedError::from_cli(&cli, "--symbol-error", "--symbol-error-count"),
        symbol_ok_at,
        symbol_ok_delay_ms,
        progress_count,
        progress_gap_ms,
        progress_skip_priming: cli.iter().any(|arg| arg == "--progress-skip-priming"),
        progress_before_error: cli.iter().any(|arg| arg == "--progress-before-error"),
        definition_error: InjectedError::from_cli(
            &cli,
            "--definition-error",
            "--definition-error-count",
        ),
        symbol_raw_error: cli_arg(&cli, "--symbol-raw-error")
            .and_then(|raw| serde_json::from_str(&raw).ok()),
        exit_on_symbol_request,
        huge_frame_on_symbol_request,
        never_read_stdin: cli.iter().any(|arg| arg == "--never-read-stdin"),
        spawn_grandchild: cli.iter().any(|arg| arg == "--spawn-grandchild"),
        keep_alive_on_eof: cli.iter().any(|arg| arg == "--keep-alive-on-eof"),
        ready_notification: cli.iter().any(|arg| arg == "--ready-notification"),
        string_id_server_requests: cli.iter().any(|arg| arg == "--string-id-server-requests"),
    };
    run_server(config)
}

/// Overwrites `path` in place with fixed-width text, never truncating it:
/// `fs::write` would leave an empty file behind if a kill landed between its
/// truncate and its write.
fn write_heartbeat(path: &Path, beat: u64) {
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
    {
        let _ = file.write_all(format!("{beat:020}").as_bytes());
    }
}

/// Sleeper mode (`--sleeper FILE`): writes its process id to `FILE` with the
/// extension `pid` (before the first heartbeat, so a test that sees the
/// heartbeat can read it), then rewrites `FILE` every 50 ms. A test asks the
/// operating system whether that process still runs. It ends on its own after
/// two minutes, so a test that fails to end it does not leave it running for
/// good.
fn run_sleeper(path: &Path) {
    let _ = fs::write(path.with_extension("pid"), process::id().to_string());
    for beat in 1..=2400u64 {
        write_heartbeat(path, beat);
        thread::sleep(Duration::from_millis(50));
    }
}

/// Starts this program in sleeper mode as a child of the fake server and
/// returns once it has written its first heartbeat, so the test knows it ran
/// before anything was done to end it. `ordinal` numbers the server process
/// (its `spawn_count`) in the heartbeat file name. Like a real wrapper's
/// descendant it inherits whatever handles it can, among them the server's
/// pipes to the client.
fn spawn_sleeper(state_dir: &Path, ordinal: u64) -> io::Result<()> {
    let beat = state_dir.join(format!("sleeper_{ordinal}.txt"));
    process::Command::new(env::current_exe()?)
        .arg("--sleeper")
        .arg(&beat)
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::null())
        .stderr(process::Stdio::null())
        .spawn()?;
    let started = Instant::now();
    while !beat.exists() {
        if started.elapsed() > Duration::from_secs(10) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "the sleeper never wrote its heartbeat",
            ));
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

/// Appends one line (this process's id) to `name` in the state directory, so
/// a test can count how many processes saw the event. Best effort.
fn record_event(state_dir: Option<&Path>, name: &str) {
    let Some(path) = state_dir else { return };
    if let Ok(mut file) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path.join(name))
    {
        // One `write_all` of the whole line: `writeln!` may issue several
        // writes, which two servers appending at once could interleave.
        let _ = file.write_all(format!("{}\n", process::id()).as_bytes());
    }
}

/// Claims the lowest free start ordinal: creates `spawns/<n>` with
/// `create_new`, which only one process can do for a given `n`, so servers
/// started together each get their own ordinal (a read, increment and write
/// of one counter file would let two of them both see 0 and both write 1).
/// The ordinals have no gaps, so the number of entries in `spawns` is the
/// number of starts. Also writes the ordinal to `spawn_count.txt`, which the
/// tests whose starts never overlap read; when two starts overlap it holds
/// whichever ordinal was written last, so a test about concurrent starts
/// counts the entries in `spawns` instead.
fn claim_spawn_ordinal(state_dir: &Path) -> io::Result<u64> {
    let spawns = state_dir.join("spawns");
    fs::create_dir_all(&spawns)?;
    let mut ordinal = 1u64;
    loop {
        match fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(spawns.join(ordinal.to_string()))
        {
            Ok(_) => break,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => ordinal += 1,
            Err(err) => return Err(err),
        }
    }
    fs::write(state_dir.join("spawn_count.txt"), ordinal.to_string())?;
    Ok(ordinal)
}

pub fn run_server(config: FakeLspConfig) -> io::Result<()> {
    let mut stdin = io::stdin();
    let stdout = io::stdout();
    let mut stdout = stdout.lock();

    if let Some(path) = &config.state_dir {
        let _ = fs::create_dir_all(path);
    }

    let mut request_count = 0u64;
    if let Some(path) = &config.state_dir
        && let Ok(value) = fs::read_to_string(path.join("request_count.txt"))
        && !value.trim().is_empty()
    {
        request_count = value.trim().parse().unwrap_or(0);
    }

    // Count this process's start so tests can observe how many times the fake
    // binary was started (e.g. to verify a single fresh retry after a
    // failure). `spawn_count` is this process's ordinal (1 for the first).
    let mut spawn_count = 1u64;
    if let Some(path) = &config.state_dir {
        spawn_count = claim_spawn_ordinal(path)?;
    }

    let mut symbol_requests = 0u64;
    let mut paced_requests = 0u64;
    let mut symbol_error = config.symbol_error.clone();
    let mut definition_error = config.definition_error.clone();
    let mut pending_definition = None;
    let mut pid_written = false;
    let mut root_uri: Option<String> = None;
    let mut document_store = matches!(config.scenario, FakeScenario::Documents)
        .then(|| documents::Store::new(config.state_dir.clone(), spawn_count));

    loop {
        let Some(frame) = read_frame(&mut stdin)? else {
            if config.keep_alive_on_eof {
                loop {
                    thread::sleep(Duration::from_secs(60));
                }
            }
            break;
        };
        let body = str::from_utf8(&frame)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let request: serde_json::Value = serde_json::from_str(body)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        let method = request.get("method").and_then(serde_json::Value::as_str);
        let id = request.get("id").and_then(serde_json::Value::as_u64);

        // The `exit` is recorded before anything else that touches the disk: a
        // client that kills an unresponsive server a grace period after sending
        // it must find the record, and the writes below take seconds when the
        // disk is busy.
        if method == Some("exit") {
            record_event(config.state_dir.as_deref(), "exit_seen.txt");
        }

        request_count += 1;
        if let Some(path) = &config.state_dir {
            fs::write(path.join("request_count.txt"), request_count.to_string())?;
            // Record the process id on the first request so a test can confirm
            // that the same child process served multiple calls (persistent
            // reuse) without needing OS process introspection.
            if !pid_written {
                fs::write(path.join("pid.txt"), process::id().to_string())?;
                pid_written = true;
            }
        }

        // The client's answer to a request this server sent
        // (`--string-id-server-requests`): keep its id and result for the test
        // and do not take it for a request.
        if method.is_none()
            && let Some(reply_id) = request.get("id").filter(|id| id.is_string())
        {
            if let Some(path) = &config.state_dir
                && let Ok(mut file) = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path.join("server_request_replies.txt"))
            {
                let _ = file.write_all(format!("{reply_id} {}\n", request["result"]).as_bytes());
            }
            continue;
        }

        // Capture the workspace root URI from the initialize request so that
        // scenario responses can reference files inside the actual workspace.
        // `locations_for` drops results whose URI is outside the workspace, so
        // the fake server must echo URIs rooted at the real workspace path.
        if method == Some("initialize")
            && let Some(uri) = request["params"]["rootUri"].as_str()
        {
            root_uri = Some(uri.to_string());
        }

        // `--mode documents`: the store answers navigation requests and keeps
        // the documents; the handshake, `shutdown`, `exit` and `didOpen`
        // fall through to the standard handling below.
        if let Some(store) = document_store.as_mut()
            && let Some(method) = method
        {
            match store.handle(method, &request["params"], id.is_some()) {
                documents::Outcome::Pass => {}
                documents::Outcome::Handled => continue,
                documents::Outcome::Response(result) => {
                    if let Some(id) = id {
                        write_message(&mut stdout, &json_response(id, result))?;
                    }
                    continue;
                }
                documents::Outcome::Unsupported => {
                    if let Some(id) = id {
                        write_message(&mut stdout, &json_error(id, -32601, "method not found"))?;
                    }
                    continue;
                }
            }
        }

        // Every scenario must accept the initialize handshake, because the real
        // CSharpClient::spawn always sends initialize + initialized before any
        // scenario-specific request. Scenario-specific behavior on initialize
        // (unsolicited notification, stderr, timeout, malformed, exit) is
        // layered on top of the standard capabilities response.
        if method == Some("initialize") {
            // The Timeout scenario must delay before responding so that a
            // short configured lsp_timeout_seconds causes the initialize
            // request itself to time out (the intended fail-open trigger).
            if matches!(config.scenario, FakeScenario::Timeout) {
                thread::sleep(Duration::from_secs(2));
            }
            if config.initialize_delay_ms > 0 {
                thread::sleep(Duration::from_millis(config.initialize_delay_ms));
            }
            if let Some(id) = id {
                let capabilities = if document_store.is_some() {
                    documents::capabilities()
                } else {
                    json!({})
                };
                write_message(
                    &mut stdout,
                    &json_response(id, json!({"capabilities": capabilities})),
                )?;
            }
            match config.scenario {
                FakeScenario::UnsolicitedNotification => {
                    write_message(
                        &mut stdout,
                        &json_notification("$/progress", json!({"kind":"begin"})),
                    )?;
                }
                FakeScenario::Stderr => {
                    writeln!(io::stderr(), "server stderr")?;
                }
                _ => {}
            }
        } else if method == Some("initialized") {
            // Before anything below can end this process, so that a server
            // that dies right away still leaves its descendant behind.
            if config.spawn_grandchild
                && let Some(path) = &config.state_dir
            {
                spawn_sleeper(path, spawn_count)?;
            }
            // Accept the initialized notification. The Initialized scenario
            // additionally emits a ready notification (preserving existing
            // observable behavior).
            if config.ready_notification || matches!(config.scenario, FakeScenario::Initialized) {
                write_message(
                    &mut stdout,
                    &json_notification("custom/ready", json!({"ready": true})),
                )?;
            }
            if config.string_id_server_requests {
                write_message(
                    &mut stdout,
                    &json!({"jsonrpc": "2.0", "id": "ready-1", "method": "custom/ready", "params": {"ready": true}}),
                )?;
                write_message(
                    &mut stdout,
                    &json!({"jsonrpc": "2.0", "id": "cfg-1", "method": "workspace/configuration", "params": {"items": [{}, {}]}}),
                )?;
            }
            // Alive but deaf: nothing more is read from the input, so the
            // client's pipe fills up once it writes enough.
            if config.never_read_stdin && spawn_count == 1 {
                thread::sleep(Duration::from_secs(60));
                process::exit(0);
            }
            // ExitDuringRequest: the server dies right after accepting the
            // initialized notification, so the next request finds a dead
            // process. Acting on `initialized` (rather than immediately after
            // the initialize response) avoids a race with the parent's
            // initialized write and makes the failure deterministic.
            if matches!(config.scenario, FakeScenario::ExitDuringRequest)
                || (matches!(config.scenario, FakeScenario::RestartSuccess) && spawn_count == 1)
            {
                process::exit(0);
            }
            // Malformed: emit a Content-Length-framed frame whose body is not
            // valid JSON, so the client's reader fails to parse it and the
            // session is invalidated.
            if matches!(config.scenario, FakeScenario::Malformed) {
                let malformed = b"not-json";
                stdout
                    .write_all(format!("Content-Length: {}\r\n\r\n", malformed.len()).as_bytes())
                    .unwrap();
                stdout.write_all(malformed).unwrap();
                stdout.flush().unwrap();
                return Ok(());
            }
        } else if method == Some("shutdown") {
            // Recorded even when ignored, so a test can tell the request
            // arrived.
            record_event(config.state_dir.as_deref(), "shutdown_seen.txt");
            if !config.ignore_shutdown
                && let Some(id) = id
            {
                write_message(&mut stdout, &json_response(id, serde_json::Value::Null))?;
            }
        } else if method == Some("textDocument/didOpen") {
            // A notification: nothing to answer, only counted.
            record_event(config.state_dir.as_deref(), "did_open_seen.txt");
        } else if method == Some("exit") {
            if !config.ignore_shutdown {
                process::exit(0);
            }
        } else {
            // Scenario-specific request methods. Build file URIs from the
            // captured workspace root so results survive `locations_for`.
            let base = root_uri.as_deref().unwrap_or("file:///workspace/");
            let base = base.trim_end_matches('/');
            let calc_uri = format!("{base}/{}", config.primary_file);
            let call_uri = format!("{base}/{}", config.secondary_file);
            match (config.scenario, method) {
                (
                    FakeScenario::WorkspaceSymbols
                    | FakeScenario::UnsolicitedNotification
                    | FakeScenario::Stderr
                    | FakeScenario::RestartSuccess,
                    Some("workspace/symbol"),
                ) => {
                    symbol_requests += 1;
                    // One line per `workspace/symbol` request (priming
                    // included), so a test can tell which processes served.
                    record_event(config.state_dir.as_deref(), "symbol_request_pids.txt");
                    if symbol_requests == config.exit_on_symbol_request && spawn_count == 1 {
                        process::exit(0);
                    }
                    if symbol_requests == config.huge_frame_on_symbol_request && spawn_count == 1 {
                        stdout.write_all(b"Content-Length: 4294967296\r\n\r\n")?;
                        stdout.flush()?;
                        continue;
                    }
                    let delay_ms = if symbol_requests == 1 {
                        config.first_symbol_delay_ms
                    } else {
                        config.symbol_delay_ms
                    };
                    if delay_ms > 0 {
                        thread::sleep(Duration::from_millis(delay_ms));
                    }
                    if symbol_requests == config.symbol_ok_at && config.symbol_ok_delay_ms > 0 {
                        thread::sleep(Duration::from_millis(config.symbol_ok_delay_ms));
                    }
                    if let Some(id) = id {
                        if symbol_requests > 1
                            && spawn_count == 1
                            && let Some(raw) = &config.symbol_raw_error
                        {
                            write_message(
                                &mut stdout,
                                &json!({"jsonrpc":"2.0","id":id,"error":raw}),
                            )?;
                        } else if symbol_requests > 1
                            && symbol_requests != config.symbol_ok_at
                            && let Some(error) = symbol_error.as_mut()
                            && error.take()
                        {
                            if config.progress_before_error {
                                write_message(
                                    &mut stdout,
                                    &json_notification("$/progress", json!({"kind":"report"})),
                                )?;
                            }
                            write_message(
                                &mut stdout,
                                &json_error(id, error.code, &error.message),
                            )?;
                        } else {
                            write_message(
                                &mut stdout,
                                &json_response(
                                    id,
                                    json!([
                                        {"name":"Calculator","kind":12,"location":{"uri":calc_uri,"range":{"start":{"line":2,"character":4},"end":{"line":2,"character":16}}}}
                                    ]),
                                ),
                            )?;
                        }
                    }
                }
                (FakeScenario::NoSymbols, Some("workspace/symbol")) => {
                    if let Some(id) = id {
                        write_message(&mut stdout, &json_response(id, json!([])))?;
                    }
                }
                // Every request (a message with a method and an id) gets the
                // same error; notifications have no id and get no answer.
                (FakeScenario::ErrorResponse, Some(_)) => {
                    if let Some(id) = id {
                        write_message(
                            &mut stdout,
                            &json!({
                                "jsonrpc": "2.0",
                                "id": id,
                                "error": {"code": -32603, "message": "fake internal error"}
                            }),
                        )?;
                    }
                }
                (FakeScenario::SlowWithProgress, Some("workspace/symbol")) => {
                    // By default four notifications, 400ms apart (1.6s total),
                    // longer than a 1s `lsp_timeout_seconds` but each gap well
                    // under it; `--progress-gap-ms` and `--progress-count` change
                    // the pacing for a test whose timeout is wider.
                    paced_requests += 1;
                    let paced = !(config.progress_skip_priming && paced_requests == 1);
                    for _ in 0..if paced { config.progress_count } else { 0 } {
                        thread::sleep(Duration::from_millis(config.progress_gap_ms));
                        write_message(
                            &mut stdout,
                            &json_notification("$/progress", json!({"kind":"report"})),
                        )?;
                    }
                    if let Some(id) = id {
                        write_message(
                            &mut stdout,
                            &json_response(
                                id,
                                json!([
                                    {"name":"Calculator","kind":12,"location":{"uri":calc_uri,"range":{"start":{"line":2,"character":4},"end":{"line":2,"character":16}}}}
                                ]),
                            ),
                        )?;
                    }
                }
                (FakeScenario::Definition, Some("workspace/symbol"))
                    if config.exit_on_definition_request =>
                {
                    symbol_requests += 1;
                    if let Some(id) = id {
                        write_message(&mut stdout, &json_response(id, json!([])))?;
                    }
                    if let Some(response) = pending_definition.take() {
                        thread::sleep(Duration::from_millis(config.definition_delay_ms));
                        write_message(&mut stdout, &response)?;
                    }
                }
                (
                    FakeScenario::Definition | FakeScenario::DefinitionAndReferences,
                    Some("textDocument/definition"),
                ) => {
                    if config.exit_on_definition_request && spawn_count == 1 {
                        process::exit(0);
                    }
                    if let Some(id) = id {
                        let response = if let Some(error) = definition_error.as_mut()
                            && error.take()
                        {
                            json_error(id, error.code, &error.message)
                        } else {
                            json_response(
                                id,
                                json!({
                                    "uri": calc_uri,
                                    "range": {"start":{"line":2,"character":4},"end":{"line":2,"character":16}}
                                }),
                            )
                        };
                        // Position calls do not wait for priming. Answer it
                        // before sleeping even if the definition arrived first,
                        // so priming cannot shield the session from idle release.
                        if config.exit_on_definition_request && symbol_requests == 0 {
                            pending_definition = Some(response);
                        } else {
                            thread::sleep(Duration::from_millis(config.definition_delay_ms));
                            write_message(&mut stdout, &response)?;
                        }
                    }
                }
                (
                    FakeScenario::References | FakeScenario::DefinitionAndReferences,
                    Some("textDocument/references"),
                ) => {
                    if let Some(id) = id {
                        write_message(
                            &mut stdout,
                            &json_response(
                                id,
                                json!([
                                    {"uri":calc_uri,"range":{"start":{"line":2,"character":4},"end":{"line":2,"character":16}}},
                                    {"uri":call_uri,"range":{"start":{"line":4,"character":8},"end":{"line":4,"character":18}}}
                                ]),
                            ),
                        )?;
                    }
                }
                _ => {
                    if config.answer_unhandled
                        && let Some(id) = id
                    {
                        write_message(&mut stdout, &json_response(id, json!(null)))?;
                    }
                }
            }
        }
        stdout.flush()?;
    }

    Ok(())
}

fn read_frame<R: Read>(reader: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut length = None;
    let mut saw_header_byte = false;
    loop {
        let mut line = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            match reader.read(&mut byte) {
                Ok(0) if !saw_header_byte && line.is_empty() => return Ok(None),
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "LSP header ended before its blank-line terminator",
                    ));
                }
                Ok(_) => {
                    saw_header_byte = true;
                    line.push(byte[0]);
                    if byte[0] == b'\n' {
                        break;
                    }
                }
                Err(err) => return Err(err),
            }
        }
        if line == b"\r\n" || line == b"\n" {
            break;
        }
        let header =
            str::from_utf8(&line).map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let Some((name, value)) = header.trim_end().split_once(':') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("malformed LSP header: {header:?}"),
            ));
        };
        if name.eq_ignore_ascii_case("Content-Length") {
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?,
            );
        }
    }
    let length = length.ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidData, "missing Content-Length header")
    })?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body)?;
    Ok(Some(body))
}

/// Extract the value following `flag` from a CLI argument list.
fn cli_arg(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1).cloned())
}

fn json_response(id: u64, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","id":id,"result":result})
}

fn json_error(id: u64, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":code,"message":message}})
}

fn json_notification(method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","method":method,"params":params})
}

fn write_message<W: Write>(writer: &mut W, value: &serde_json::Value) -> io::Result<()> {
    let bytes = serde_json::to_vec(value)?;
    writer.write_all(format!("Content-Length: {}\r\n\r\n", bytes.len()).as_bytes())?;
    writer.write_all(&bytes)?;
    writer.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fake_lsp_reads_frames_and_returns_response() {
        let temp = tempfile::tempdir().unwrap();
        let request = serde_json::json!({
            "jsonrpc": "2.0",
            "id": 7,
            "method": "initialize",
            "params": {"rootUri": "file:///workspace"}
        });
        let body = serde_json::to_vec(&request).unwrap();
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_fake-lsp-server"))
            .env("FAKE_LSP_MODE", "initialized")
            .env("FAKE_LSP_STATE", temp.path().join("state"))
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();

        let stdin = child.stdin.as_mut().unwrap();
        stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .unwrap();
        stdin.write_all(&body).unwrap();
        stdin.flush().unwrap();
        let _ = stdin;

        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        let mut stdout = output.stdout.as_slice();
        let response = read_frame(&mut stdout).unwrap().unwrap();
        assert!(stdout.is_empty(), "unexpected trailing protocol bytes");
        let response: serde_json::Value = serde_json::from_slice(&response).unwrap();
        assert_eq!(response["jsonrpc"], "2.0");
        assert_eq!(response["id"], 7);
        assert!(response.get("result").is_some());
        let request_count =
            fs::read_to_string(temp.path().join("state/request_count.txt")).unwrap();
        assert_eq!(request_count.trim(), "1");
    }
}
