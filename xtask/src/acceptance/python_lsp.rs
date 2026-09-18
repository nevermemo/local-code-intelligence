//! Real pyright acceptance: definition/references resolution (the reliable,
//! verified-working navigation path — see the module-level notes below on
//! `search_symbols`) plus semantic search, against a real Python package
//! fixture.
//!
//! `search_symbols` (workspace/symbol) is not asserted here: against a
//! freshly-spawned pyright that has never had any file opened, it was
//! observed to return an empty result rather than a hard error, unlike
//! `find_definition`/`find_references` which reliably resolve real
//! cross-file references once the target file has been opened (handled by
//! `PythonServer::before_position_request` in `src/lsp/python.rs`).

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::csharp_lsp::{bound_text, is_build_output, results};
use super::{Evidence, Fixture, lci_binary, run_lci, which};

const INIT_PY: &str = "";

const CALCULATOR_PY: &str = r#"class Calculator:
    def add(self, a, b):
        return a + b
"#;

const CALL_SITE_PY: &str = "from .calculator import Calculator\n\n\ndef run(value: Calculator) -> int:\n    return value.add(1, 2)\n";

/// Resolves the `pyright-langserver` executable: the explicit `--pyright`
/// flag if given (either a literal file, or a bare command name looked up on
/// PATH), else a PATH lookup. Returns `None` when nothing resolves, which
/// callers treat as a graceful prerequisite-unavailable skip.
fn resolve_pyright(pyright: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = pyright {
        if path.is_file() {
            return Some(path);
        }
        if let Some(name) = path.to_str()
            && let Some(found) = which(name)
        {
            return Some(found);
        }
        return None;
    }
    which("pyright-langserver")
}

pub async fn run(pyright: Option<PathBuf>) -> Result<()> {
    let Some(resolved) = resolve_pyright(pyright) else {
        // Matches the C#/TypeScript families' PREREQUISITE_UNAVAILABLE /
        // graceful-skip convention: this is not a failure, just an unmet
        // prerequisite on this machine.
        if let Ok(mut evidence) = Evidence::new("python-lsp-acceptance") {
            let _ = evidence.set("status", "prerequisite-unavailable");
        }
        println!("PREREQUISITE_UNAVAILABLE: pyright-langserver not found on PATH or via --pyright");
        return Ok(());
    };

    let mut evidence = Evidence::new("python-lsp-acceptance")?;
    evidence.set("server", resolved.to_string_lossy().into_owned())?;

    let fixture = Fixture::create("python-lsp")?;
    let outcome = run_inner(&resolved, &fixture, &mut evidence).await;

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
            "PASS: Python LSP acceptance. Evidence: {}",
            evidence.path().display()
        );
    } else {
        println!(
            "FAIL: Python LSP acceptance. Evidence: {}",
            evidence.path().display()
        );
    }
    outcome
}

async fn run_json_stage(
    evidence: &mut Evidence,
    stage: &str,
    config_path: &Path,
    args: &[&str],
) -> Result<Value> {
    evidence.stage(stage)?;
    let output = run_lci(config_path, args).await?;
    if !output.success {
        let bounded = bound_text(&output.combined(), 2000);
        let _ = evidence.set("error", bounded.clone());
        bail!(
            "LCI command failed (stage {stage}, args [{}]): {bounded}",
            args.join(" ")
        );
    }
    output
        .json()
        .with_context(|| format!("parse JSON output for stage {stage}"))
}

async fn run_inner(pyright: &Path, fixture: &Fixture, evidence: &mut Evidence) -> Result<()> {
    evidence.stage("resolve-prerequisites")?;
    // Unlike csharp-ls/typescript-language-server, pyright-langserver has no
    // standalone `--version`/`--help` output: any argument other than a
    // transport flag (`--stdio`/`--node-ipc`/`--socket`) makes it print a
    // connection error and exit immediately, so no version probe is done
    // here (the resolved executable path is recorded above instead).
    let binary = lci_binary();
    if !binary.is_file() {
        bail!("debug binary missing: {}", binary.display());
    }

    evidence.stage("create-fixture")?;
    // A proper Python package (`__init__.py` present) is required for the
    // relative import in call_site.py to resolve at all; see the
    // module-level notes on why cross-file member resolution additionally
    // needs the explicit `value: Calculator` annotation below.
    fixture.write("src/__init__.py", INIT_PY)?;
    fixture.write("src/calculator.py", CALCULATOR_PY)?;
    fixture.write("src/call_site.py", CALL_SITE_PY)?;

    let fixture_dir = fixture.dir.to_string_lossy().into_owned();

    evidence.stage("construct-config")?;
    let extra_lines = vec![
        "lsp_timeout_seconds = 30".to_string(),
        "[python]".to_string(),
        format!("path = '{}'", pyright.to_string_lossy().replace('\\', "/")),
    ];
    let config_path = fixture.write_config(&extra_lines)?;

    let index_value =
        run_json_stage(evidence, "index", &config_path, &["index", &fixture_dir]).await?;
    let files = index_value
        .get("files")
        .and_then(Value::as_u64)
        .context("index result missing files count")?;
    evidence.set("index_files", files)?;
    ensure!(
        files >= 2,
        "expected at least 2 indexed Python files, got {files}"
    );

    // `def add(self, a, b):` on line 2 of calculator.py; character 8 is
    // where `add` starts (`    def ` is 8 characters).
    let definition_value = run_json_stage(
        evidence,
        "definition",
        &config_path,
        &["definition", &fixture_dir, "src/call_site.py", "5", "17"],
    )
    .await?;

    let references_value = run_json_stage(
        evidence,
        "references",
        &config_path,
        &[
            "references",
            &fixture_dir,
            "src/calculator.py",
            "2",
            "8",
            "--include-declaration",
        ],
    )
    .await?;

    let search_value = run_json_stage(
        evidence,
        "search",
        &config_path,
        &[
            "search",
            &fixture_dir,
            "calculator add two numbers",
            "--top-k",
            "8",
        ],
    )
    .await?;

    let definition_results = results(&definition_value);
    let reference_results = results(&references_value);
    let search_results = results(&search_value);

    evidence.set(
        "definition_position",
        serde_json::json!({"line": 5, "character": 17}),
    )?;
    evidence.set("definition", Value::Array(definition_results.clone()))?;
    evidence.set(
        "references_position",
        serde_json::json!({"line": 2, "character": 8}),
    )?;
    evidence.set("references", Value::Array(reference_results.clone()))?;
    evidence.set("search", Value::Array(search_results.clone()))?;

    evidence.stage("assert-results")?;
    ensure!(
        !definition_results.is_empty(),
        "definition returned no locations for value.add(...) in call_site.py"
    );
    ensure!(
        definition_results
            .iter()
            .any(|r| r.get("relative_file_path").and_then(Value::as_str)
                == Some("src/calculator.py")),
        "definition did not resolve to src/calculator.py: {definition_results:?}"
    );

    ensure!(
        !reference_results.is_empty(),
        "references returned no locations for calculator.py's add method"
    );
    ensure!(
        reference_results
            .iter()
            .any(|r| r.get("relative_file_path").and_then(Value::as_str)
                == Some("src/calculator.py")),
        "references did not include the declaration in src/calculator.py"
    );
    ensure!(
        reference_results.iter().any(
            |r| r.get("relative_file_path").and_then(Value::as_str) == Some("src/call_site.py")
        ),
        "references did not include the usage in src/call_site.py"
    );

    for location in definition_results.iter().chain(reference_results.iter()) {
        let language = location
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or_default();
        ensure!(
            language == "python",
            "location has unexpected language: {language}"
        );
        let provider = location
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default();
        ensure!(
            provider == "pyright",
            "location has unexpected provider: {provider}"
        );
        let start_line = location
            .get("start_line")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        ensure!(
            start_line >= 1,
            "location line is not one-based: {start_line}"
        );
        let relative_file_path = location
            .get("relative_file_path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let source_path = fixture.dir.join(relative_file_path);
        ensure!(
            source_path.is_file(),
            "location points outside fixture sources: {relative_file_path}"
        );
        let source_line_count = std::fs::read_to_string(&source_path)?.lines().count() as u64;
        let end_line = location
            .get("end_line")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        ensure!(
            end_line <= source_line_count,
            "location exceeds source line count: {relative_file_path}:{end_line}"
        );
    }

    for result in definition_results
        .iter()
        .chain(reference_results.iter())
        .chain(search_results.iter())
    {
        let relative_file_path = result
            .get("relative_file_path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        ensure!(
            !is_build_output(relative_file_path),
            "generated build output appeared in results: {relative_file_path}"
        );
    }

    ensure!(
        search_results
            .iter()
            .any(|r| r.get("relative_file_path").and_then(Value::as_str)
                == Some("src/calculator.py")),
        "semantic search did not surface src/calculator.py for a calculator/add query"
    );

    Ok(())
}
