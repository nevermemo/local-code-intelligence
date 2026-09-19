//! Ported from `scripts/Acceptance-Lsp.ps1`.
//!
//! Runs the real rust-analyzer navigation flow (`search_symbols`/
//! `find_definition`/`find_references`) against a real workspace over a
//! persistent `serve` process and MCP session, matching the pattern
//! `missing.rs`/`recovery.rs`/`lsp_full.rs` already use -- rather than
//! one-shot CLI invocations, each of which would spawn and cold-start an
//! entirely separate rust-analyzer process.
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

use super::{
    AcceptanceProfile, Evidence, Fixture, ManagedServer, McpSession, resolve_acceptance_workspace,
    test_results_dir, wait_http_ok,
};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

const BASE: &str = "http://127.0.0.1:8769"; // matches acceptance::PORT

/// How a position in a real source file is located.
enum Anchor {
    /// A fixed one-based line and zero-based UTF-16 character, for a
    /// workspace whose source this repository does not contain.
    Fixed { line: u64, character: u64 },
    /// Located at run time: the first line containing `line_needle`, and the
    /// offset of `symbol` within that line. Used for this repository's own
    /// source so ordinary edits above these lines never invalidate the run.
    Located {
        line_needle: &'static str,
        symbol: &'static str,
    },
}

impl Anchor {
    /// Byte offsets are used as UTF-16 offsets: every anchored line in this
    /// repository is ASCII, and a non-ASCII line would fail the needle
    /// lookup loudly rather than resolve to a wrong column.
    fn resolve(&self, workspace: &Path, relative_file: &str) -> Result<(u64, u64)> {
        match *self {
            Anchor::Fixed { line, character } => Ok((line, character)),
            Anchor::Located {
                line_needle,
                symbol,
            } => {
                let path = workspace.join(relative_file);
                let text = std::fs::read_to_string(&path)
                    .map_err(|error| anyhow!("read anchor file {}: {error}", path.display()))?;
                for (index, line) in text.lines().enumerate() {
                    if line.contains(line_needle)
                        && let Some(column) = line.find(symbol)
                    {
                        return Ok((index as u64 + 1, column as u64));
                    }
                }
                bail!("anchor {line_needle:?} not found in {}", path.display())
            }
        }
    }
}

struct LspProfile {
    key: &'static str,
    symbol_query: &'static str,
    expect_symbol_name: &'static str,
    declaration_file: &'static str,
    declaration: Anchor,
    call_site_file: &'static str,
    call_site: Anchor,
    min_references: usize,
}

/// This repository. `openapi_document` has exactly two occurrences of
/// interest: its declaration in `src/rest.rs`, and the one router line that
/// calls it -- a small but real cross-checkable navigation target.
const SELF_PROFILE: LspProfile = LspProfile {
    key: "self",
    symbol_query: "openapi_document#",
    expect_symbol_name: "openapi_document",
    declaration_file: "src/rest.rs",
    declaration: Anchor::Located {
        line_needle: "fn openapi_document() -> Value {",
        symbol: "openapi_document",
    },
    call_site_file: "src/rest.rs",
    call_site: Anchor::Located {
        line_needle: "\"/openapi.json\"",
        symbol: "openapi_document",
    },
    // The declaration plus the one router wiring that calls it.
    min_references: 2,
};

/// The optional external GUST example: unchanged from what this harness
/// asserted before -- only the machine-specific default path is gone.
const GUST_PROFILE: LspProfile = LspProfile {
    key: "gust",
    symbol_query: "emit_expression#",
    expect_symbol_name: "emit_expression",
    declaration_file: "crates/gust-macros/src/slang/mod.rs",
    declaration: Anchor::Fixed {
        line: 581,
        character: 8,
    },
    call_site_file: "crates/gust-macros/src/slang/mod.rs",
    call_site: Anchor::Fixed {
        line: 266,
        character: 24,
    },
    min_references: 2,
};

/// Saves a tool's structured JSON report to `test-results/<name>.json`,
/// matching the `Run-Report` convention in the original `.ps1` scripts.
fn save_report(name: &str, value: &Value) -> Result<()> {
    let path = test_results_dir()?.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(value)?)
        .map_err(|error| anyhow!("write report {}: {error}", path.display()))
}

pub async fn run(profile: AcceptanceProfile, workspace: Option<PathBuf>) -> Result<()> {
    let spec = match profile {
        AcceptanceProfile::SelfRepo => &SELF_PROFILE,
        AcceptanceProfile::Gust => &GUST_PROFILE,
    };
    let workspace = resolve_acceptance_workspace(profile, workspace, "lsp")?;
    let workspace_str = workspace.to_string_lossy().into_owned();

    let fixture = Fixture::create("acceptance-lsp")?;
    // Generous headroom over the 60s production default: this indexes a
    // real Cargo workspace's full crate graph, which can legitimately take
    // longer under system load than one editor session's LSP timeout
    // should have to allow for.
    let config_path = fixture.write_config(&["lsp_timeout_seconds = 120".to_string()])?;

    let mut evidence = Evidence::new("acceptance-lsp")?;
    evidence.set("workspace", workspace_str.clone())?;
    let mut server: Option<ManagedServer> = None;

    let outcome = run_inner(
        spec,
        &workspace,
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

#[allow(clippy::too_many_arguments)]
async fn run_inner(
    spec: &LspProfile,
    workspace_path: &Path,
    workspace: &str,
    config_path: &Path,
    fixture: &Fixture,
    server: &mut Option<ManagedServer>,
    evidence: &mut Evidence,
) -> Result<String> {
    let (decl_line, decl_char) = spec
        .declaration
        .resolve(workspace_path, spec.declaration_file)?;
    let (call_line, call_char) = spec
        .call_site
        .resolve(workspace_path, spec.call_site_file)?;

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

    // Cold-indexing a real workspace this size is a genuinely slow,
    // throughput-variable operation (confirmed directly against the
    // optional GUST profile: embedding over a thousand chunks took anywhere
    // from ~2 to 10+ minutes across repeated runs, unrelated to any fixed
    // timeout) -- and `McpSession`'s client sets no request timeout, so a
    // single attempt already waits as long as the server takes. Indexing
    // also has no partial-progress concept across separate calls:
    // `App::index_locked` only commits once, at the very end, so a failed
    // attempt's embedding work is not reusable by the next attempt. That
    // makes a short, patient retry count the right shape here -- a safety
    // net for a genuine transient connection error, not a mechanism to wait
    // out a slow embedding backend (each retry of an expensive,
    // non-resumable operation has real cost, so more isn't automatically
    // better).
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
    save_report(&format!("{}-lsp-index", spec.key), &indexed)?;

    let symbols = poll_tool(
        &mut session,
        evidence,
        "symbols",
        "search_symbols",
        json!({"workspace_path": workspace, "query": spec.symbol_query}),
        24,
        Duration::from_secs(5),
        |value| {
            value
                .get("results")
                .and_then(Value::as_array)
                .is_some_and(|results| {
                    results.iter().any(|location| {
                        location.get("name").and_then(Value::as_str)
                            == Some(spec.expect_symbol_name)
                            && location.get("start_line").and_then(Value::as_u64) == Some(decl_line)
                    })
                })
        },
    )
    .await?;
    save_report(&format!("{}-lsp-symbols", spec.key), &symbols)?;

    let definition = poll_tool(
        &mut session,
        evidence,
        "definition",
        "find_definition",
        json!({
            "workspace_path": workspace,
            "relative_file_path": spec.call_site_file,
            "line": call_line,
            "character": call_char,
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
                        .any(|loc| loc.get("start_line").and_then(Value::as_u64) == Some(decl_line))
                })
        },
    )
    .await?;
    save_report(&format!("{}-lsp-definition", spec.key), &definition)?;

    let references = poll_tool(
        &mut session,
        evidence,
        "references",
        "find_references",
        json!({
            "workspace_path": workspace,
            "relative_file_path": spec.declaration_file,
            "line": decl_line,
            "character": decl_char,
            "include_declaration": true,
        }),
        12,
        Duration::from_secs(5),
        |value| {
            value
                .get("results")
                .and_then(Value::as_array)
                .is_some_and(|results| results.len() >= spec.min_references)
        },
    )
    .await?;
    save_report(&format!("{}-lsp-references", spec.key), &references)?;

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
