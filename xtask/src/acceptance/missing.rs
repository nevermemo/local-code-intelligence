//! Generic engine for the "optional language server is missing" acceptance
//! check shared by every language family: starts an owned `serve` process
//! configured with a definitely-missing server executable (and a
//! definitely-missing rust-analyzer, so the final filtered-search assertion
//! measures retrieval-channel isolation without launching an unrelated
//! language server) and verifies:
//!   - `/ready` remains ready despite the missing optional tooling.
//!   - `search_code` filtered to this language still returns results and
//!     reports a warning identifying the unavailable optional provider.
//!   - `find_definition` on a source file returns a clear tooling error.
//!   - `service_status` reports this language's tooling as optional/degraded.
//!   - A Rust-filtered search does not start the missing server's process,
//!     does not mention this provider in its warnings, and still returns
//!     real Rust results.
//!
//! `search_code` and `find_definition` are exercised over the real MCP
//! `tools/call` endpoint (via `McpSession::call_tool`), not the CLI, so this
//! acceptance actually exercises the MCP tool-handler code path in
//! `src/server.rs` rather than only the CLI path in `src/main.rs`.
//!
//! Every language family's `*_missing.rs` file previously duplicated this
//! whole flow near-verbatim, differing only in fixture content, config, and
//! the substrings identifying each provider. This module holds the shared
//! logic once; per-language files supply a [`MissingServerSpec`].

use super::{Evidence, Fixture, ManagedServer, McpSession, run_lci, wait_http_ok};
use anyhow::{Result, anyhow, bail};
use serde_json::json;
use std::time::Duration;

const BASE: &str = "http://127.0.0.1:8769"; // matches acceptance::PORT

/// How to detect whether the missing server's process ever started, for the
/// final Rust-filtered-search isolation check.
pub enum ProcessCheck {
    /// A native executable spawned directly as a child (csharp-ls).
    Children(&'static str),
    /// An npm-installed tool that spawns through an intermediate shell
    /// wrapper on Windows, landing two or more levels deep
    /// (typescript-language-server, pyright).
    Descendants(&'static str),
}

impl ProcessCheck {
    fn count(&self, managed: &ManagedServer) -> usize {
        match self {
            ProcessCheck::Children(name) => managed.children_named(name).len(),
            ProcessCheck::Descendants(name) => managed.descendants_named(name).len(),
        }
    }
}

pub struct MissingServerSpec {
    /// Short identifier used to name the fixture and evidence file (e.g.
    /// `"csharp"`, `"typescript"`, `"python"`, or `"c"`/`"cpp"` when two
    /// language keys share one `config_section` -- see `config_section`).
    pub language_key: &'static str,
    /// Human-readable name for log/error messages (e.g. `"C#"`).
    pub display_name: &'static str,
    /// TOML config section this server's settings live under (e.g.
    /// `"go"`). Usually equal to `language_key`, but distinct when one
    /// server navigates more than one language key under a single shared
    /// section -- clangd's `c`/`cpp` both write `[clangd]`.
    pub config_section: &'static str,
    /// Value written as `path = '...'` under `[language_key]` to guarantee
    /// the configured server cannot resolve to a real executable.
    pub missing_path: &'static str,
    /// Extra lines appended to the `[language_key]` config section (e.g.
    /// C#'s `args = ['--solution', ...]`).
    pub extra_config_lines: Vec<String>,
    /// Fixture source files as (relative path, content) pairs, including the
    /// unrelated `src/lib.rs` every family indexes to prove Rust isolation.
    pub source_files: Vec<(&'static str, &'static str)>,
    pub search_query: &'static str,
    pub search_languages: &'static [&'static str],
    pub definition_file: &'static str,
    pub definition_line: u32,
    pub definition_character: u32,
    /// Exact substrings that identify this provider unambiguously in a
    /// message (e.g. `["csharp-ls", "csharp language server"]`).
    pub exact_provider_terms: &'static [&'static str],
    /// The bare language/provider word checked for proximity to a
    /// provider/tooling/warning keyword (e.g. `"csharp"`).
    pub near_word: &'static str,
    /// Extra terms accepted anywhere (not just near a keyword) for the
    /// tooling-error and service-status checks -- Python additionally
    /// accepts "pyright" there, since `App::search` reports LSP failures
    /// using the adapter's provider name.
    pub extra_terms: &'static [&'static str],
    pub process_check: ProcessCheck,
}

pub async fn run(spec: MissingServerSpec) -> Result<()> {
    let mut evidence = Evidence::new(&format!("{}-lsp-missing-server", spec.language_key))?;
    let fixture = Fixture::create(&format!("{}-missing", spec.language_key))?;
    let mut server: Option<ManagedServer> = None;

    let outcome = checks(&mut evidence, &fixture, &mut server, &spec).await;

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
                "PASS: {} missing-server and provider-isolation acceptance. Evidence: {}",
                spec.display_name,
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
    spec: &MissingServerSpec,
) -> Result<()> {
    evidence.stage("create-fixture")?;
    for (relative, content) in &spec.source_files {
        fixture.write(relative, content)?;
    }

    evidence.stage("construct-config")?;
    let mut lines = vec![
        "lsp_timeout_seconds = 10".to_string(),
        "rust_analyzer_path = 'definitely-missing-rust-analyzer'".to_string(),
        format!("[{}]", spec.config_section),
        format!("path = '{}'", spec.missing_path),
    ];
    lines.extend(spec.extra_config_lines.iter().cloned());
    let config_path = fixture.write_config(&lines)?;

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
    evidence.set("ready_while_missing", ready)?;
    if !ready {
        bail!(
            "/ready did not report ready while {} tooling was missing (required deps should still be healthy)",
            spec.display_name
        );
    }

    evidence.stage("mcp-initialize")?;
    let mut session = McpSession::connect(BASE).await?;

    evidence.stage("search-code")?;
    let search_body = session
        .call_tool(
            "search_code",
            json!({
                "workspace_path": workspace,
                "query": spec.search_query,
                "languages": spec.search_languages,
            }),
        )
        .await?;
    let returned_results = search_body.to_lowercase().contains("calculator");
    let warning_mentions_provider = mentions_provider(&search_body, spec);
    evidence.set("search_code_returned_results", returned_results)?;
    evidence.set(
        "search_code_warning_mentions_provider",
        warning_mentions_provider,
    )?;
    if !returned_results {
        bail!(
            "search_code did not return semantic/lexical {} results while the server was missing: {search_body}",
            spec.display_name
        );
    }
    if !warning_mentions_provider {
        bail!(
            "search_code did not report the unavailable optional {} provider: {search_body}",
            spec.display_name
        );
    }

    evidence.stage("definition")?;
    let definition_result = session
        .call_tool(
            "find_definition",
            json!({
                "workspace_path": workspace,
                "relative_file_path": spec.definition_file,
                "line": spec.definition_line,
                "character": spec.definition_character,
            }),
        )
        .await;
    let (definition_is_tooling_error, definition_detail) = match definition_result {
        Ok(body) => (false, body),
        Err(error) => {
            let message = format!("{error:#}");
            let lower = message.to_lowercase();
            let is_tooling_error = names_provider(&lower, spec)
                || lower.contains("language server")
                || lower.contains("program not found");
            (is_tooling_error, message)
        }
    };
    evidence.set("definition_is_tooling_error", definition_is_tooling_error)?;
    if !definition_is_tooling_error {
        bail!(
            "find_definition did not return a clear tooling error while the {} server was missing: {definition_detail}",
            spec.display_name
        );
    }

    evidence.stage("service-status")?;
    let status_body = session.call_tool("service_status", json!({})).await?;
    let lower_status = status_body.to_lowercase();
    let service_status_ok = names_provider(&lower_status, spec)
        && (lower_status.contains("optional")
            || lower_status.contains("degraded")
            || lower_status.contains("unavailable")
            || lower_status.contains("missing"));
    evidence.set(
        "service_status_provider_optional_degraded",
        service_status_ok,
    )?;
    if !service_status_ok {
        bail!(
            "service_status did not report {} tooling as optional/degraded: {status_body}",
            spec.display_name
        );
    }

    evidence.stage("rust-filtered-search")?;
    let managed = server.as_ref().expect("server spawned above");
    let before = spec.process_check.count(managed);
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
    let after = spec.process_check.count(managed);
    let started_process = after > before;
    // Match the specific provider/tooling identifier, not a bare language
    // substring: the fixture's own temp directory name (e.g.
    // lci-csharp-missing-<pid>) is echoed back in file paths and would
    // otherwise cause a false positive on a plain substring match.
    let mentions_provider_in_rust_search = mentions_provider(&rust_search_body, spec);
    let returned_rust_results = json_field_matches(&rust_search_body, "language", "rust");
    evidence.set("rust_filtered_search_started_process", started_process)?;
    evidence.set(
        "rust_filtered_search_mentions_provider",
        mentions_provider_in_rust_search,
    )?;
    evidence.set(
        "rust_filtered_search_returned_rust_results",
        returned_rust_results,
    )?;
    if started_process {
        bail!(
            "a non-{} filtered search unexpectedly started the {} server process",
            spec.display_name,
            spec.display_name
        );
    }
    if mentions_provider_in_rust_search {
        bail!(
            "a non-{} filtered search produced an irrelevant provider warning",
            spec.display_name
        );
    }
    if !returned_rust_results {
        bail!("the Rust-filtered search did not return Rust results: {rust_search_body}");
    }

    Ok(())
}

/// True when `text` names the optional provider directly, or mentions the
/// bare language word within `max_gap` characters of a
/// provider/tooling/warning word -- approximating the original PowerShell
/// scripts' `name|name language server|name.{0,20}(provider|tooling|warning)`
/// regexes without a regex dependency.
fn mentions_provider(text: &str, spec: &MissingServerSpec) -> bool {
    let lower = text.to_lowercase();
    spec.exact_provider_terms
        .iter()
        .any(|term| lower.contains(term))
        || contains_near(
            &lower,
            spec.near_word,
            20,
            &["provider", "tooling", "warning"],
        )
}

/// Broader than [`mentions_provider`]: also accepts a bare mention of the
/// language word or any `extra_terms` anywhere in the text, for the
/// tooling-error and service-status checks (which have no risk of matching
/// an unrelated fixture path, unlike the rust-filtered-search isolation
/// check above).
fn names_provider(lower: &str, spec: &MissingServerSpec) -> bool {
    lower.contains(spec.near_word) || spec.extra_terms.iter().any(|term| lower.contains(term))
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
