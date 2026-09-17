//! Ported from `scripts/CSharpLspProbe.ps1`: a fully standalone LSP
//! JSON-RPC client, independent of the `local-code-intelligence` binary. It
//! spawns `csharp-ls` directly, hand-frames `Content-Length:`-delimited
//! JSON-RPC messages (mirroring `src/lsp/transport.rs`'s framing), and
//! drives `initialize` -> `initialized` -> polls `workspace/symbol` until a
//! `Calculator` symbol resolves -> `shutdown` -> `exit`, confirming the
//! process actually exits.
//!
//! Everything here runs strictly sequentially (one in-flight request at a
//! time), so no request/response ID correlation map is needed: each
//! `wait_for_response` call drains frames until it sees the response it
//! asked for, answering any server-to-client requests it encounters along
//! the way and recording notifications for evidence.

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::path::Path;
use std::time::Duration;
use sysinfo::{Pid, System};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, ChildStdout, Command};

use super::command_version;
use crate::acceptance::{Evidence, Fixture, run_dotnet, which};

const CSPROJ: &str = r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Library</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
  </PropertyGroup>
</Project>
"#;

const PRODUCTION_SOURCE: &str = "namespace Acceptance;\n\npublic interface ICalculator { int Add(int left, int right); }\n\npublic sealed class Calculator : ICalculator\n{\n    public int Add(int left, int right) => left + right;\n    public int Use() => Add(2, 3);\n}\n";

const CALLSITE_SOURCE: &str = "namespace Acceptance;\n\npublic static class CallSite\n{\n    public static int Run(ICalculator calculator) => calculator.Add(1, 2);\n}\n";

pub async fn run(csharp_ls: &Path) -> Result<()> {
    let mut evidence = Evidence::new("csharp-lsp-standalone-probe")?;
    evidence.set("executable", csharp_ls.to_string_lossy().into_owned())?;

    let fixture = Fixture::create("csharp-probe")?;
    let outcome = run_inner(csharp_ls, &fixture, &mut evidence).await;

    match &outcome {
        Ok(()) => {
            let _ = evidence.pass();
        }
        Err(err) => {
            let _ = evidence.fail(err);
        }
    }
    fixture.cleanup();
    let _ = evidence.set("cleanup_fixture_removed", fixture.removed());

    if outcome.is_ok() {
        println!(
            "PASS: standalone C# LSP probe. Evidence: {}",
            evidence.path().display()
        );
    } else {
        println!(
            "FAIL: standalone C# LSP probe. Evidence: {}",
            evidence.path().display()
        );
    }
    outcome
}

/// Minimal per-run state for the conservative server-to-client request
/// handling and notification bookkeeping, mirroring the .ps1 probe's
/// `$notifications` / `$serverRequests` / `$handledRequests` lists.
#[derive(Default)]
struct ProbeState {
    notifications: Vec<String>,
    server_requests: Vec<String>,
    handled_requests: Vec<String>,
}

impl ProbeState {
    fn add_notification(&mut self, method: &str) {
        if !self.notifications.iter().any(|m| m == method) {
            self.notifications.push(method.to_string());
        }
    }

    fn add_server_request(&mut self, method: &str) {
        if !self.server_requests.iter().any(|m| m == method) {
            self.server_requests.push(method.to_string());
        }
    }

    fn add_handled(&mut self, method: &str) {
        if !self.handled_requests.iter().any(|m| m == method) {
            self.handled_requests.push(method.to_string());
        }
    }
}

/// Conservative, bounded responses to the server-to-client requests the
/// original probe knows how to answer. Returns `None` for anything else
/// (left unanswered, same as the .ps1 script).
fn respond_server_request(
    method: &str,
    params: Option<&Value>,
    workspace_folders: &Value,
) -> Option<Value> {
    match method {
        "client/registerCapability" | "client/unregisterCapability" => Some(Value::Null),
        "workspace/configuration" => {
            let count = params
                .and_then(|p| p.get("items"))
                .and_then(Value::as_array)
                .map(|items| items.len())
                .unwrap_or(0);
            Some(Value::Array(vec![Value::Null; count]))
        }
        "workspace/workspaceFolders" => Some(workspace_folders.clone()),
        "window/workDoneProgress/create" => Some(json!({})),
        _ => None,
    }
}

/// Reads one `Content-Length:`-framed JSON-RPC message from `reader`, or
/// `Ok(None)` on a clean EOF. Mirrors `read_messages` in
/// `src/lsp/transport.rs`.
async fn read_frame(reader: &mut BufReader<ChildStdout>) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).await? == 0 {
            return Ok(None);
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = Some(value.trim().parse::<usize>()?);
        }
    }
    let length = content_length.context("LSP message missing Content-Length")?;
    let mut body = vec![0u8; length];
    reader.read_exact(&mut body).await?;
    let message: Value = serde_json::from_slice(&body)?;
    Ok(Some(message))
}

/// Writes one `Content-Length:`-framed JSON-RPC message. Mirrors `write` in
/// `src/lsp/transport.rs`.
async fn send_message(stdin: &mut ChildStdin, value: &Value) -> Result<()> {
    let body = serde_json::to_vec(value)?;
    stdin
        .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
        .await?;
    stdin.write_all(&body).await?;
    stdin.flush().await?;
    Ok(())
}

/// Drains frames (answering server-to-client requests, recording
/// notifications) until the response for `id` arrives or `timeout` elapses.
async fn wait_for_response(
    reader: &mut BufReader<ChildStdout>,
    stdin: &mut ChildStdin,
    id: u64,
    timeout: Duration,
    state: &mut ProbeState,
    workspace_folders: &Value,
) -> Result<Option<Value>> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Ok(None);
        }
        let frame = match tokio::time::timeout(remaining, read_frame(reader)).await {
            Ok(Ok(Some(message))) => message,
            Ok(Ok(None)) => bail!("csharp-ls stdout closed while waiting for response id {id}"),
            Ok(Err(error)) => return Err(error),
            Err(_) => return Ok(None),
        };
        let has_method = frame.get("method").is_some();
        let has_id = frame.get("id").is_some();
        let has_result_or_error = frame.get("result").is_some() || frame.get("error").is_some();
        if has_method && has_id {
            let method = frame
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            state.add_server_request(&method);
            if let Some(result) =
                respond_server_request(&method, frame.get("params"), workspace_folders)
            {
                let response = json!({
                    "jsonrpc": "2.0",
                    "id": frame.get("id").cloned().unwrap_or(Value::Null),
                    "result": result,
                });
                send_message(stdin, &response).await?;
                state.add_handled(&method);
            }
        } else if has_method {
            let method = frame
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or_default();
            state.add_notification(method);
        } else if has_id && has_result_or_error {
            let frame_id = frame.get("id").and_then(Value::as_u64);
            if frame_id == Some(id) {
                return Ok(Some(frame));
            }
            // A response for an id we're not currently waiting on; this
            // probe issues requests strictly sequentially, so this should
            // not happen in practice. Drop it and keep draining.
        }
    }
}

/// Minimal `%XX` percent-decoding (no URI crate dependency available to
/// xtask). Fixture paths never contain characters that need it in practice,
/// but this keeps `Get-RelativePath`'s decoding behavior for parity.
fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let Ok(byte) = u8::from_str_radix(&input[i + 1..i + 3], 16)
        {
            out.push(byte);
            i += 3;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Converts a `file://` URI to a path relative to `root` (forward slashes),
/// or `None` if it does not point inside `root`. Mirrors `Get-RelativePath`.
fn relative_from_uri(uri: &str, root: &Path) -> Option<String> {
    let local = uri.strip_prefix("file:///")?;
    let decoded = percent_decode(local);
    let local_path = if cfg!(windows) {
        decoded.replace('/', "\\")
    } else {
        format!("/{decoded}")
    };
    let root_norm = root
        .to_string_lossy()
        .trim_end_matches(['\\', '/'])
        .to_string();
    if local_path.len() >= root_norm.len()
        && local_path[..root_norm.len()].eq_ignore_ascii_case(&root_norm)
    {
        let rel = local_path[root_norm.len()..]
            .trim_start_matches(['\\', '/'])
            .replace('\\', "/");
        Some(rel)
    } else {
        None
    }
}

fn extract_symbol_uri(symbol: &Value) -> Option<String> {
    let location = symbol.get("location")?;
    if let Some(uri) = location.as_str() {
        return Some(uri.to_string());
    }
    location
        .get("uri")
        .and_then(Value::as_str)
        .map(str::to_string)
}

async fn run_inner(csharp_ls: &Path, fixture: &Fixture, evidence: &mut Evidence) -> Result<()> {
    evidence.stage("resolve-prerequisites")?;
    let version = command_version(csharp_ls, "--version").await;
    evidence.set("version", version)?;
    which("dotnet").context("dotnet SDK is required for the real C# fixture")?;

    evidence.stage("create-fixture")?;
    fixture.write("CSharpAcceptance.csproj", CSPROJ)?;
    fixture.write("src/Production.cs", PRODUCTION_SOURCE)?;
    fixture.write("src/CallSite.cs", CALLSITE_SOURCE)?;
    let fixture_dir = fixture.dir.to_string_lossy().into_owned();
    let project_path = fixture
        .dir
        .join("CSharpAcceptance.csproj")
        .to_string_lossy()
        .into_owned();
    let solution_path = fixture
        .dir
        .join("CSharpAcceptance.sln")
        .to_string_lossy()
        .into_owned();
    run_dotnet(
        &fixture.dir,
        &[
            "new",
            "sln",
            "--format",
            "sln",
            "--name",
            "CSharpAcceptance",
            "--output",
            &fixture_dir,
        ],
    )
    .await?;
    run_dotnet(
        &fixture.dir,
        &["solution", &solution_path, "add", &project_path],
    )
    .await?;
    run_dotnet(&fixture.dir, &["restore", &solution_path]).await?;
    run_dotnet(&fixture.dir, &["build", &solution_path, "--no-restore"]).await?;
    ensure!(
        Path::new(&solution_path).is_file(),
        "solution file not created: {solution_path}"
    );

    evidence.stage("start-process")?;
    let rpc_log_path = fixture.dir.join("csharp-ls-rpc.log");
    evidence.set("rpc_log_path", rpc_log_path.to_string_lossy().into_owned())?;
    evidence.set(
        "arguments",
        json!([
            "--solution",
            "CSharpAcceptance.sln",
            "--rpclog",
            rpc_log_path.to_string_lossy()
        ]),
    )?;
    let mut child = Command::new(csharp_ls)
        .arg("--solution")
        .arg("CSharpAcceptance.sln")
        .arg("--rpclog")
        .arg(&rpc_log_path)
        .current_dir(&fixture.dir)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .context("start csharp-ls")?;
    let owned_pid = child.id().context("csharp-ls process has no pid")?;
    evidence.set("owned_pid", owned_pid)?;

    let mut stdin = child.stdin.take().context("csharp-ls stdin unavailable")?;
    let stdout = child
        .stdout
        .take()
        .context("csharp-ls stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("csharp-ls stderr unavailable")?;
    let stderr_lines = std::sync::Arc::new(tokio::sync::Mutex::new(Vec::<String>::new()));
    let stderr_lines_writer = std::sync::Arc::clone(&stderr_lines);
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let mut buffer = stderr_lines_writer.lock().await;
            buffer.push(line);
            if buffer.len() > 500 {
                buffer.remove(0);
            }
        }
    });
    let mut reader = BufReader::new(stdout);
    let mut state = ProbeState::default();

    let fixture_uri_path = fixture.dir.to_string_lossy().replace('\\', "/");
    let root_uri = format!("file:///{fixture_uri_path}");
    let workspace_folders = json!([{"uri": root_uri, "name": "CSharpAcceptance"}]);

    let run_result = drive_protocol(
        &mut reader,
        &mut stdin,
        &mut state,
        &root_uri,
        &workspace_folders,
        fixture,
        evidence,
    )
    .await;

    let run_result: Result<()> = match run_result {
        Ok(()) => finish_process(&mut child, owned_pid, evidence).await,
        Err(error) => Err(error),
    };

    evidence.set("notification_methods_observed", json!(state.notifications))?;
    evidence.set(
        "server_request_methods_observed",
        json!(state.server_requests),
    )?;
    evidence.set("server_requests_handled", json!(state.handled_requests))?;

    // Safety net: whatever happened above, an owned csharp-ls must not be
    // left running. `kill_on_drop` on `child` covers the case where this
    // function unwinds without an explicit wait, but we drive it
    // synchronously here so evidence reflects the real outcome. Idempotent:
    // `force_kill` only acts if the process is still alive.
    if run_result.is_err() {
        let forced = force_kill(&mut child).await;
        if forced {
            let _ = evidence.set("forced_termination_required", true);
        }
    }

    {
        let lines = stderr_lines.lock().await;
        let joined = lines.join("\n");
        evidence.set("stderr_tail", super::bound_text(&joined, 2000))?;
    }
    if let Ok(rpc_log) = tokio::fs::read_to_string(&rpc_log_path).await {
        evidence.set("rpc_log_tail", super::bound_text(&rpc_log, 4000))?;
    }

    let mut system = System::new_all();
    system.refresh_all();
    let pid_confirmed_gone = system.process(Pid::from_u32(owned_pid)).is_none();
    evidence.set(
        "cleanup",
        json!({
            "owned_pid": owned_pid,
            "pid_confirmed_gone": pid_confirmed_gone,
        }),
    )?;

    run_result
}

/// Kills `child` if it hasn't already exited, waiting briefly, and reports
/// whether a forced kill was actually required.
async fn force_kill(child: &mut tokio::process::Child) -> bool {
    match child.try_wait() {
        Ok(Some(_)) => false,
        _ => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            true
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn drive_protocol(
    reader: &mut BufReader<ChildStdout>,
    stdin: &mut ChildStdin,
    state: &mut ProbeState,
    root_uri: &str,
    workspace_folders: &Value,
    fixture: &Fixture,
    evidence: &mut Evidence,
) -> Result<()> {
    evidence.stage("initialize")?;
    let initialize_params = json!({
        "processId": std::process::id(),
        "rootUri": root_uri,
        "workspaceFolders": workspace_folders,
        "capabilities": {
            "workspace": {
                "symbol": {"dynamicRegistration": false},
                "workspaceFolders": true,
            },
            "textDocument": {
                "synchronization": {"didSave": true},
                "definition": {"dynamicRegistration": false},
                "references": {"dynamicRegistration": false},
            },
        },
    });
    let init_request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": initialize_params,
    });
    let started = std::time::Instant::now();
    send_message(stdin, &init_request).await?;
    let init_response = wait_for_response(
        reader,
        stdin,
        1,
        Duration::from_secs(30),
        state,
        workspace_folders,
    )
    .await?;
    evidence.set(
        "initialize_elapsed_ms",
        started.elapsed().as_millis() as u64,
    )?;
    let init_response =
        init_response.context("initialize response (id=1) not received within 30s")?;
    evidence.set("initialize_response_received", true)?;
    let capabilities = init_response
        .get("result")
        .and_then(|result| result.get("capabilities"))
        .context("initialize result is missing the capabilities object")?;
    let capability_keys: Vec<String> = capabilities
        .as_object()
        .context("capabilities is not an object")?
        .keys()
        .cloned()
        .collect();
    evidence.set("server_capability_keys", json!(capability_keys))?;

    evidence.stage("initialized")?;
    send_message(
        stdin,
        &json!({"jsonrpc": "2.0", "method": "initialized", "params": {}}),
    )
    .await?;

    evidence.stage("workspace-symbol")?;
    let mut next_id: u64 = 100;
    let max_attempts = 30;
    let mut last_symbol_response: Option<Value> = None;
    let symbol_started = std::time::Instant::now();
    for attempt in 1..=max_attempts {
        evidence.set("workspace_symbol_attempts", attempt as u64)?;
        let id = next_id;
        next_id += 1;
        send_message(
            stdin,
            &json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "workspace/symbol",
                "params": {"query": "Calculator"},
            }),
        )
        .await?;
        let response = wait_for_response(
            reader,
            stdin,
            id,
            Duration::from_secs(5),
            state,
            workspace_folders,
        )
        .await?;
        if let Some(response) = response {
            let matched = response
                .get("result")
                .and_then(Value::as_array)
                .is_some_and(|results| {
                    results.iter().any(|symbol| {
                        symbol.get("name").and_then(Value::as_str) == Some("Calculator")
                    })
                });
            last_symbol_response = Some(response);
            if matched {
                break;
            }
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    evidence.set(
        "workspace_symbol_elapsed_ms",
        symbol_started.elapsed().as_millis() as u64,
    )?;

    evidence.stage("validate-symbols")?;
    let mut matching_names = Vec::new();
    let mut matching_paths = Vec::new();
    if let Some(response) = &last_symbol_response
        && let Some(results) = response.get("result").and_then(Value::as_array)
    {
        for symbol in results {
            if symbol.get("name").and_then(Value::as_str) != Some("Calculator") {
                continue;
            }
            if let Some(uri) = extract_symbol_uri(symbol)
                && let Some(relative) = relative_from_uri(&uri, &fixture.dir)
            {
                matching_names.push(
                    symbol
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                );
                matching_paths.push(relative);
            }
        }
    }
    evidence.set("matching_symbol_names", json!(matching_names))?;
    evidence.set("matching_repo_relative_paths", json!(matching_paths))?;
    ensure!(
        !matching_names.is_empty(),
        "no Calculator symbol inside the fixture after {max_attempts} workspace/symbol attempts"
    );

    evidence.stage("shutdown")?;
    let shutdown_id = next_id;
    next_id += 1;
    send_message(
        stdin,
        &json!({"jsonrpc": "2.0", "id": shutdown_id, "method": "shutdown"}),
    )
    .await?;
    let shutdown_response = wait_for_response(
        reader,
        stdin,
        shutdown_id,
        Duration::from_secs(10),
        state,
        workspace_folders,
    )
    .await?;
    evidence.set("shutdown_response_received", shutdown_response.is_some())?;
    ensure!(
        shutdown_response.is_some(),
        "shutdown response was not received within 10s"
    );
    let _ = next_id;

    evidence.stage("exit")?;
    send_message(stdin, &json!({"jsonrpc": "2.0", "method": "exit"})).await?;

    // The process handle lives in the caller; it drives wait-exit /
    // confirm-exit once this protocol sequence returns.
    Ok(())
}

/// Waits for the owned csharp-ls process to exit naturally after the `exit`
/// notification, then confirms its PID is actually gone. Mirrors the .ps1
/// probe's `wait-exit` / `confirm-exit` stages: a forced kill here is a
/// failure, not just diagnostic noise.
async fn finish_process(
    child: &mut tokio::process::Child,
    owned_pid: u32,
    evidence: &mut Evidence,
) -> Result<()> {
    evidence.stage("wait-exit")?;
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(status)) => {
            evidence.set("forced_termination_required", false)?;
            evidence.set("exit_code", status.code())?;
        }
        Ok(Err(error)) => {
            return Err(anyhow::Error::from(error)).context("waiting for csharp-ls to exit");
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
            evidence.set("forced_termination_required", true)?;
            bail!("csharp-ls did not exit naturally within 5s after the exit notification");
        }
    }

    evidence.stage("confirm-exit")?;
    let mut system = System::new_all();
    system.refresh_all();
    ensure!(
        system.process(Pid::from_u32(owned_pid)).is_none(),
        "owned csharp-ls PID {owned_pid} still running after exit"
    );
    Ok(())
}
