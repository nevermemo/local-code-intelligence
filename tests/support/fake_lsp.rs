use serde_json::json;
use std::{
    env, fs,
    io::{self, Read, Write},
    path::PathBuf,
    process, str, thread,
    time::Duration,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FakeScenario {
    Initialized,
    WorkspaceSymbols,
    Definition,
    References,
    UnsolicitedNotification,
    Stderr,
    Timeout,
    Malformed,
    ExitDuringRequest,
    RestartSuccess,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FakeLspConfig {
    pub scenario: FakeScenario,
    pub state_dir: Option<PathBuf>,
    pub process_id: Option<u32>,
    pub exit_code: Option<i32>,
}

pub fn run_from_env() -> io::Result<()> {
    let cli: Vec<String> = env::args().collect();
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
        "definition" => FakeScenario::Definition,
        "references" => FakeScenario::References,
        "unsolicited" => FakeScenario::UnsolicitedNotification,
        "stderr" => FakeScenario::Stderr,
        "timeout" => FakeScenario::Timeout,
        "malformed" => FakeScenario::Malformed,
        "exit-during-request" => FakeScenario::ExitDuringRequest,
        "restart-success" => FakeScenario::RestartSuccess,
        other => {
            eprintln!("unexpected FAKE_LSP_MODE: {other}");
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("unknown mode: {other}"),
            ));
        }
    };
    let config = FakeLspConfig {
        scenario,
        state_dir: state_path,
        process_id,
        exit_code: None,
    };
    run_server(config)
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

    // Increment the spawn counter once per process invocation so tests can
    // observe how many times the fake binary was started (e.g. to verify a
    // single fresh retry after a failure).
    let mut spawn_count = 1u64;
    if let Some(path) = &config.state_dir {
        let spawn_file = path.join("spawn_count.txt");
        let current = fs::read_to_string(&spawn_file).unwrap_or_default();
        spawn_count = current.trim().parse::<u64>().unwrap_or(0) + 1;
        fs::write(&spawn_file, spawn_count.to_string())?;
    }

    let mut pid_written = false;
    let mut root_uri: Option<String> = None;

    while let Some(frame) = read_frame(&mut stdin)? {
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
        let body = str::from_utf8(&frame)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let request: serde_json::Value = serde_json::from_str(body)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;

        let method = request.get("method").and_then(serde_json::Value::as_str);
        let id = request.get("id").and_then(serde_json::Value::as_u64);

        // Capture the workspace root URI from the initialize request so that
        // scenario responses can reference files inside the actual workspace.
        // `locations_for` drops results whose URI is outside the workspace, so
        // the fake server must echo URIs rooted at the real workspace path.
        if method == Some("initialize")
            && let Some(uri) = request["params"]["rootUri"].as_str()
        {
            root_uri = Some(uri.to_string());
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
            if let Some(id) = id {
                write_message(&mut stdout, &json_response(id, json!({"capabilities": {}})))?;
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
            // Accept the initialized notification. The Initialized scenario
            // additionally emits a ready notification (preserving existing
            // observable behavior).
            if matches!(config.scenario, FakeScenario::Initialized) {
                write_message(
                    &mut stdout,
                    &json_notification("custom/ready", json!({"ready": true})),
                )?;
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
        } else {
            // Scenario-specific request methods. Build file URIs from the
            // captured workspace root so results survive `locations_for`.
            let base = root_uri.as_deref().unwrap_or("file:///workspace/");
            let base = base.trim_end_matches('/');
            let calc_uri = format!("{base}/src/Calculator.cs");
            let call_uri = format!("{base}/src/CallSite.cs");
            match (config.scenario, method) {
                (
                    FakeScenario::WorkspaceSymbols
                    | FakeScenario::UnsolicitedNotification
                    | FakeScenario::Stderr
                    | FakeScenario::RestartSuccess,
                    Some("workspace/symbol"),
                ) => {
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
                (FakeScenario::Definition, Some("textDocument/definition")) => {
                    if let Some(id) = id {
                        write_message(
                            &mut stdout,
                            &json_response(
                                id,
                                json!({
                                    "uri": calc_uri,
                                    "range": {"start":{"line":2,"character":4},"end":{"line":2,"character":16}}
                                }),
                            ),
                        )?;
                    }
                }
                (FakeScenario::References, Some("textDocument/references")) => {
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
                _ => {}
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
