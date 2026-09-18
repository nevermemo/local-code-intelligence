//! Ported from the C# family's `csharp_missing.rs`, adapted for Python.
//!
//! Missing-server and provider-isolation acceptance for the optional Python
//! (pyright) language server: starts an owned `serve` process configured
//! with a definitely-missing `pyright-langserver` executable (and a
//! definitely-missing rust-analyzer, so the final filtered-search assertion
//! measures retrieval-channel isolation without launching an unrelated
//! language server) and verifies:
//!   - `/ready` remains ready despite the missing optional Python tooling.
//!   - `search_code` for a Python-filtered query still returns results and
//!     reports a warning identifying the unavailable optional Python
//!     provider.
//!   - `find_definition` on a Python file returns a clear tooling error.
//!   - `service_status` reports Python tooling as optional/degraded.
//!   - A Rust-filtered search does not spawn a `node` descendant (pyright,
//!     like typescript-language-server, is an npm-installed tool that spawns
//!     through an intermediate shell wrapper on Windows, so isolation is
//!     checked via `ManagedServer::descendants_named("node")` rather than a
//!     direct-children check), does not mention pyright in its warnings, and
//!     still returns real Rust results.
//!
//! `search_code` and `find_definition` are exercised over the real MCP
//! `tools/call` endpoint (via `McpSession::call_tool`), not the CLI, so this
//! acceptance actually exercises the MCP tool-handler code path in
//! `src/server.rs` rather than only the CLI path in `src/main.rs`.

use super::{Evidence, Fixture, ManagedServer, McpSession, run_lci, wait_http_ok};
use anyhow::{Result, anyhow, bail};
use serde_json::json;
use std::time::Duration;

const CALCULATOR_PY: &str = r#"class Calculator:
    def add(self, a, b):
        return a + b
"#;

const LIB_RS: &str = r#"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}
"#;

const BASE: &str = "http://127.0.0.1:8768";

pub async fn run() -> Result<()> {
    let mut evidence = Evidence::new("python-lsp-missing-server")?;
    let fixture = Fixture::create("python-missing")?;
    let mut server: Option<ManagedServer> = None;

    let outcome = checks(&mut evidence, &fixture, &mut server).await;

    evidence.stage("cleanup")?;
    let lci_removed = match server {
        Some(mut managed) => {
            managed.kill_tree().await;
            managed.has_exited()
        }
        None => true,
    };
    fixture.cleanup();
    let fixture_removed = !fixture.dir.exists();
    let data_removed = !fixture.data_dir.exists();
    evidence.set(
        "cleanup",
        json!({
            "lci_removed": lci_removed,
            "fixture_removed": fixture_removed,
            "data_removed": data_removed,
        }),
    )?;

    let final_result = match outcome {
        Ok(()) if lci_removed && fixture_removed && data_removed => Ok(()),
        Ok(()) => Err(anyhow!(
            "cleanup incomplete: lci_removed={lci_removed}, fixture_removed={fixture_removed}, data_removed={data_removed}"
        )),
        Err(error) => Err(error),
    };

    match &final_result {
        Ok(()) => {
            evidence.pass()?;
            println!(
                "PASS: Python missing-server and provider-isolation acceptance. Evidence: {}",
                evidence.path().display()
            );
        }
        Err(error) => {
            evidence.fail(error)?;
            println!("FAIL: {error:#}. Evidence: {}", evidence.path().display());
        }
    }
    final_result
}

async fn checks(
    evidence: &mut Evidence,
    fixture: &Fixture,
    server: &mut Option<ManagedServer>,
) -> Result<()> {
    evidence.stage("create-fixture")?;
    fixture.write("src/__init__.py", "")?;
    fixture.write("src/calculator.py", CALCULATOR_PY)?;
    fixture.write("src/lib.rs", LIB_RS)?;

    evidence.stage("construct-config")?;
    let config_path = fixture.write_config(&[
        "lsp_timeout_seconds = 10".to_string(),
        "rust_analyzer_path = 'definitely-missing-rust-analyzer'".to_string(),
        "[python]".to_string(),
        "path = 'definitely-missing-pyright-langserver'".to_string(),
    ])?;

    // Index synchronously as fixture setup via the CLI. The behavior under
    // test is the long-lived server's degradation and provider isolation
    // (below, over real MCP calls); MCP indexing itself is covered by the
    // recovery acceptance.
    evidence.stage("prepare-index")?;
    let workspace = fixture.dir.to_string_lossy().to_string();
    let output = run_lci(&config_path, &["index", &workspace]).await?;
    if !output.success {
        bail!("fixture indexing failed: {}", output.combined().trim());
    }

    evidence.stage("start-serve")?;
    let managed = ManagedServer::spawn(&config_path, &fixture.dir, &fixture.dir).await?;
    let lci_pid = managed.pid;
    *server = Some(managed);
    evidence.set("lci_pid", lci_pid)?;

    evidence.stage("wait-ready")?;
    let ready = wait_http_ok(&format!("{BASE}/ready"), 30, Duration::from_millis(500)).await;
    evidence.set("ready_while_python_missing", ready)?;
    if !ready {
        bail!(
            "/ready did not report ready while python tooling was missing (required deps should still be healthy)"
        );
    }

    evidence.stage("mcp-initialize")?;
    let mut session = McpSession::connect(BASE).await?;

    evidence.stage("search-code-python")?;
    let search_body = session
        .call_tool(
            "search_code",
            json!({
                "workspace_path": workspace,
                "query": "Calculator add",
                "languages": ["python"],
            }),
        )
        .await?;
    let returned_results = search_body.to_lowercase().contains("calculator");
    let warning_mentions_pyright = mentions_pyright_provider(&search_body);
    evidence.set("search_code_returned_results", returned_results)?;
    evidence.set(
        "search_code_warning_mentions_pyright",
        warning_mentions_pyright,
    )?;
    if !returned_results {
        bail!(
            "search_code did not return semantic/lexical Python results while pyright was missing: {search_body}"
        );
    }
    if !warning_mentions_pyright {
        bail!("search_code did not report the unavailable optional Python provider: {search_body}");
    }

    evidence.stage("definition-python")?;
    let definition_result = session
        .call_tool(
            "find_definition",
            json!({
                "workspace_path": workspace,
                "relative_file_path": "src/calculator.py",
                "line": 2,
                "character": 8,
            }),
        )
        .await;
    let (definition_is_tooling_error, definition_detail) = match definition_result {
        Ok(body) => (false, body),
        Err(error) => {
            let message = format!("{error:#}");
            let lower = message.to_lowercase();
            let is_tooling_error = lower.contains("python")
                || lower.contains("pyright")
                || lower.contains("language server")
                || lower.contains("program not found");
            (is_tooling_error, message)
        }
    };
    evidence.set("definition_is_tooling_error", definition_is_tooling_error)?;
    if !definition_is_tooling_error {
        bail!(
            "find_definition did not return a clear tooling error while pyright was missing: {definition_detail}"
        );
    }

    evidence.stage("service-status")?;
    let status_body = session.call_tool("service_status", json!({})).await?;
    let lower_status = status_body.to_lowercase();
    let service_status_ok = (lower_status.contains("python") || lower_status.contains("pyright"))
        && (lower_status.contains("optional")
            || lower_status.contains("degraded")
            || lower_status.contains("unavailable")
            || lower_status.contains("missing"));
    evidence.set("service_status_python_optional_degraded", service_status_ok)?;
    if !service_status_ok {
        bail!("service_status did not report Python tooling as optional/degraded: {status_body}");
    }

    evidence.stage("rust-filtered-search")?;
    let managed = server.as_ref().expect("server spawned above");
    let before = managed.descendants_named("node").len();
    let rust_search_body = session
        .call_tool(
            "search_code",
            json!({
                "workspace_path": workspace,
                "query": "add",
                "languages": ["rust"],
            }),
        )
        .await?;
    tokio::time::sleep(Duration::from_millis(500)).await;
    let after = managed.descendants_named("node").len();
    let started_node = after > before;
    // Match the specific provider/tooling identifier, not a bare "python"
    // substring: the fixture's own temp directory name
    // (lci-python-missing-<pid>) is echoed back in file paths and would
    // otherwise cause a false positive on a plain substring match.
    let mentions_pyright = mentions_pyright_provider(&rust_search_body);
    let returned_rust_results = json_field_matches(&rust_search_body, "language", "rust");
    evidence.set("rust_filtered_search_started_node", started_node)?;
    evidence.set("rust_filtered_search_mentions_pyright", mentions_pyright)?;
    evidence.set(
        "rust_filtered_search_returned_rust_results",
        returned_rust_results,
    )?;
    if started_node {
        bail!("a non-Python filtered search unexpectedly started a node descendant process");
    }
    if mentions_pyright {
        bail!("a non-Python filtered search produced an irrelevant pyright warning");
    }
    if !returned_rust_results {
        bail!("the Rust-filtered search did not return Rust results: {rust_search_body}");
    }

    Ok(())
}

/// True when `text` names the optional Python provider directly. Unlike the
/// C# family's `mentions_csharp_provider` (which has to disambiguate a bare
/// "csharp" substring from the fixture's own temp directory name, e.g.
/// `lci-csharp-missing-<pid>`), a bare `pyright` substring is unambiguous
/// here: the fixture's temp directories are named `lci-python-missing-*`
/// (containing "python", never "pyright"), and `App::search` reports LSP
/// provider failures as `"Optional LSP provider unavailable: {}"` with
/// `adapter.provider()` (`"pyright"`) leading each joined failure message.
fn mentions_pyright_provider(text: &str) -> bool {
    let lower = text.to_lowercase();
    lower.contains("pyright")
        || lower.contains("python language server")
        || contains_near(&lower, "python", 20, &["provider", "tooling", "warning"])
}

fn contains_near(haystack: &str, needle: &str, max_gap: usize, options: &[&str]) -> bool {
    let mut start = 0;
    while let Some(idx) = haystack[start..].find(needle) {
        let hit_start = start + idx;
        let window_end = (hit_start + needle.len() + max_gap).min(haystack.len());
        let window = &haystack[hit_start..window_end];
        if options.iter().any(|option| window.contains(option)) {
            return true;
        }
        start = hit_start + needle.len();
    }
    false
}

/// Whitespace-insensitive check for a `"field":"value"` pair anywhere in a
/// JSON response body, approximating a `(?i)"language"\s*:\s*"rust"` regex
/// without a regex dependency.
fn json_field_matches(text: &str, field: &str, value: &str) -> bool {
    let compact: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    compact
        .to_lowercase()
        .contains(&format!("\"{field}\":\"{value}\"").to_lowercase())
}
