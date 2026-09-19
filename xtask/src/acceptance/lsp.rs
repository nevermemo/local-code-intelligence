//! Ported from `scripts/Acceptance-Lsp.ps1`.
//!
//! Runs the real rust-analyzer navigation flow (`search_symbols`/
//! `find_definition`/`find_references`) against a real external workspace
//! over a persistent `serve` process and MCP session, matching the pattern
//! `missing.rs`/`recovery.rs`/`lsp_full.rs` already use -- rather than the
//! one-shot CLI invocations this file used before, each of which spawned
//! and cold-started an entirely separate rust-analyzer process.
//!
//! `workspace/symbol` needs rust-analyzer's full crate-graph index ready,
//! which can legitimately take longer than one bounded timeout allows under
//! system load; with a one-shot CLI invocation, that shows up as flaky
//! failures (observed directly in this project's history: repeated,
//! load-correlated "did not resolve emit_expression" failures against the
//! same unmodified code). Against a persistent server, retrying a query
//! doesn't restart rust-analyzer -- it just gives its still-in-progress
//! background indexing more wall-clock time before the next attempt sees a
//! more complete result. `symbols`/`definition`/`references` all poll for
//! this reason, not just `symbols`, for the same consistency the rest of
//! this acceptance suite already has.

use super::{Evidence, Fixture, ManagedServer, McpSession, test_results_dir, wait_http_ok};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const DEFAULT_WORKSPACE: &str = r"C:\Users\micro\Desktop\gpu-dialect-v0";
const TARGET_FILE: &str = "crates/gust-macros/src/slang/mod.rs";
const BASE: &str = "http://127.0.0.1:8769"; // matches acceptance::PORT

/// Saves a tool's structured JSON report to `test-results/<name>.json`,
/// matching the `Run-Report` convention in the original `.ps1` scripts.
fn save_report(name: &str, value: &Value) -> Result<()> {
    let path = test_results_dir()?.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(value)?)
        .map_err(|error| anyhow!("write report {}: {error}", path.display()))
}

pub async fn run(workspace: Option<PathBuf>) -> Result<()> {
    let workspace = workspace.unwrap_or_else(|| PathBuf::from(DEFAULT_WORKSPACE));
    let workspace_str = workspace.to_string_lossy().into_owned();

    let fixture = Fixture::create("acceptance-lsp")?;
    // Generous headroom over the 60s production default: this indexes a
    // real external Cargo workspace's full crate graph, which can
    // legitimately take longer under system load than one editor session's
    // LSP timeout should have to allow for.
    let config_path = fixture.write_config(&["lsp_timeout_seconds = 120".to_string()])?;

    let mut evidence = Evidence::new("acceptance-lsp")?;
    evidence.set("workspace", workspace_str.clone())?;
    let mut server: Option<ManagedServer> = None;

    let outcome = run_inner(
        &workspace_str,
        &config_path,
        &fixture,
        &mut server,
        &mut evidence,
    )
    .await;

    evidence.stage("cleanup")?;
    let lci_removed = match server {
        Some(mut managed) => {
            managed.kill_tree().await;
            managed.has_exited()
        }
        None => true,
    };
    fixture.cleanup();
    let fixture_removed = fixture.removed();
    evidence.set(
        "cleanup",
        json!({"lci_removed": lci_removed, "fixture_removed": fixture_removed}),
    )?;

    let final_result = match outcome {
        Ok(summary) if lci_removed && fixture_removed => Ok(summary),
        Ok(_) => Err(anyhow!(
            "cleanup incomplete: lci_removed={lci_removed}, fixture_removed={fixture_removed}"
        )),
        Err(error) => Err(error),
    };

    match &final_result {
        Ok(summary) => {
            evidence.pass()?;
            println!("{summary}");
        }
        Err(error) => {
            evidence.fail(error)?;
            println!("FAIL: {error:#}. Evidence: {}", evidence.path().display());
        }
    }
    final_result.map(|_| ())
}

async fn run_inner(
    workspace: &str,
    config_path: &Path,
    fixture: &Fixture,
    server: &mut Option<ManagedServer>,
    evidence: &mut Evidence,
) -> Result<String> {
    evidence.stage("start-serve")?;
    let managed = ManagedServer::spawn(config_path, &fixture.dir, &fixture.dir).await?;
    let lci_pid = managed.pid;
    *server = Some(managed);
    evidence.set("lci_pid", lci_pid)?;

    evidence.stage("wait-health")?;
    let healthy = wait_http_ok(&format!("{BASE}/health"), 30, Duration::from_millis(500)).await;
    if !healthy {
        bail!("/health did not report ok within the bounded wait");
    }

    evidence.stage("mcp-initialize")?;
    let mut session = McpSession::connect(BASE).await?;

    // Cold-indexing a real external workspace this size is a genuinely
    // slow, throughput-variable operation (confirmed directly: embedding
    // 1,125 chunks took anywhere from ~2 to 10+ minutes across repeated
    // runs, unrelated to any fixed timeout) -- and `McpSession`'s client
    // sets no request timeout, so a single attempt already waits as long
    // as the server takes. Indexing also has no partial-progress concept
    // across separate calls: `App::index_locked` only commits once, at the
    // very end, so a failed attempt's embedding work is not reusable by
    // the next attempt. That makes a short, patient retry count the right
    // shape here -- a safety net for a genuine transient connection error,
    // not a mechanism to wait out a slow embedding backend (each retry of
    // an expensive, non-resumable operation has real cost, so more isn't
    // automatically better).
    let indexed = poll_tool(
        &mut session,
        evidence,
        "index-workspace",
        "index_workspace",
        json!({"workspace_path": workspace}),
        5,
        Duration::from_secs(5),
        |_value| true,
    )
    .await?;
    save_report("gust-lsp-index", &indexed)?;

    let symbols = poll_tool(
        &mut session,
        evidence,
        "symbols",
        "search_symbols",
        json!({"workspace_path": workspace, "query": "emit_expression#"}),
        24,
        Duration::from_secs(5),
        |value| {
            value
                .get("results")
                .and_then(Value::as_array)
                .is_some_and(|results| {
                    results.iter().any(|location| {
                        location.get("name").and_then(Value::as_str) == Some("emit_expression")
                            && location.get("start_line").and_then(Value::as_u64) == Some(581)
                    })
                })
        },
    )
    .await?;
    save_report("gust-lsp-symbols", &symbols)?;

    let definition = poll_tool(
        &mut session,
        evidence,
        "definition",
        "find_definition",
        json!({
            "workspace_path": workspace,
            "relative_file_path": TARGET_FILE,
            "line": 266,
            "character": 24,
        }),
        12,
        Duration::from_secs(5),
        |value| {
            value
                .get("results")
                .and_then(Value::as_array)
                .is_some_and(|results| {
                    results
                        .iter()
                        .any(|loc| loc.get("start_line").and_then(Value::as_u64) == Some(581))
                })
        },
    )
    .await?;
    save_report("gust-lsp-definition", &definition)?;

    let references = poll_tool(
        &mut session,
        evidence,
        "references",
        "find_references",
        json!({
            "workspace_path": workspace,
            "relative_file_path": TARGET_FILE,
            "line": 581,
            "character": 8,
            "include_declaration": true,
        }),
        12,
        Duration::from_secs(5),
        |value| {
            value
                .get("results")
                .and_then(Value::as_array)
                .is_some_and(|results| results.len() >= 2)
        },
    )
    .await?;
    save_report("gust-lsp-references", &references)?;

    let reference_count = references
        .get("results")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or_default();

    Ok(format!(
        "PASS: symbol, definition, and {reference_count} reference locations. Reports: {}",
        test_results_dir()?.display()
    ))
}

/// Calls `tool` with `args` against `session`, retrying up to `attempts`
/// times (`delay` apart) until `ready` accepts the parsed response, or
/// bailing with the last response once exhausted. Retrying here re-queries
/// the same persistent, possibly-still-indexing rust-analyzer process
/// rather than restarting it, so later attempts see more of its background
/// indexing progress, not a fresh cold start.
#[allow(clippy::too_many_arguments)]
async fn poll_tool(
    session: &mut McpSession,
    evidence: &mut Evidence,
    stage: &str,
    tool: &str,
    args: Value,
    attempts: u32,
    delay: Duration,
    ready: impl Fn(&Value) -> bool,
) -> Result<Value> {
    evidence.stage(stage)?;
    let mut last_value = Value::Null;
    let mut last_error: Option<anyhow::Error> = None;
    for attempt in 1..=attempts {
        // A transient call/parse failure (e.g. an SSE response cut short
        // under heavy system load) is just as retryable as "call succeeded
        // but the LSP server hasn't finished indexing yet" -- neither means
        // the underlying condition can never become true, so both fall
        // through to the same retry-with-delay path rather than aborting
        // the loop via `?`.
        match session
            .call_tool(tool, args.clone())
            .await
            .and_then(|body| call_tool_json(&body))
        {
            Ok(value) if ready(&value) => {
                evidence.set(&format!("{stage}_attempts"), attempt)?;
                return Ok(value);
            }
            Ok(value) => {
                last_value = value;
                last_error = None;
            }
            Err(error) => last_error = Some(error),
        }
        if attempt < attempts {
            tokio::time::sleep(delay).await;
        }
    }
    evidence.set(&format!("{stage}_last_response"), last_value.clone())?;
    match last_error {
        Some(error) => Err(error.context(format!(
            "{stage} did not succeed after {attempts} attempts ({delay:?} apart)"
        ))),
        None => bail!(
            "{stage} did not converge after {attempts} attempts ({delay:?} apart); last response: {last_value}"
        ),
    }
}

/// Extracts the `result.structuredContent` value from a raw MCP
/// `tools/call` response body. `McpSession::call_tool` returns the raw
/// SSE-framed HTTP body (`data: {...}` lines), not bare JSON, so this scans
/// for the payload line rather than parsing the body directly.
fn call_tool_json(body: &str) -> Result<Value> {
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            continue;
        }
        let Ok(envelope) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        if let Some(structured) = envelope.pointer("/result/structuredContent") {
            return Ok(structured.clone());
        }
        if let Some(error) = envelope.get("error") {
            bail!("MCP call returned an error: {error}");
        }
    }
    bail!("no structuredContent found in MCP response: {body}");
}
