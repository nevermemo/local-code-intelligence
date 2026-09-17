//! Ported from `scripts/Acceptance-Lsp.ps1`.

use crate::acceptance::{Evidence, Fixture, run_lci, test_results_dir};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

const DEFAULT_WORKSPACE: &str = r"C:\Users\micro\Desktop\gpu-dialect-v0";
const TARGET_FILE: &str = "crates/gust-macros/src/slang/mod.rs";

/// Saves a raw CLI report to `test-results/<name>.json`, matching the
/// `Run-Report` convention in the original `.ps1` scripts.
fn save_report(name: &str, value: &Value) -> Result<()> {
    let path = test_results_dir()?.join(format!("{name}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(value)?)
        .with_context(|| format!("write report {}", path.display()))
}

pub async fn run(workspace: Option<PathBuf>) -> Result<()> {
    let workspace = workspace.unwrap_or_else(|| PathBuf::from(DEFAULT_WORKSPACE));
    let workspace_str = workspace.to_string_lossy().into_owned();

    let fixture = Fixture::create("acceptance-lsp")?;
    let config_path = fixture.write_config(&[])?;

    let mut evidence = Evidence::new("acceptance-lsp")?;
    evidence.set("workspace", workspace_str.clone())?;

    let outcome = run_inner(&workspace_str, &config_path, &mut evidence).await;

    fixture.cleanup();
    if !fixture.removed() {
        bail!("acceptance lsp: fixture cleanup did not fully remove temp directories");
    }

    match outcome {
        Ok(summary) => {
            evidence.pass()?;
            println!("{summary}");
            Ok(())
        }
        Err(err) => {
            let _ = evidence.fail(&err);
            Err(err)
        }
    }
}

async fn run_inner(workspace: &str, config_path: &Path, evidence: &mut Evidence) -> Result<String> {
    // The original `.ps1` script skipped indexing here because it shared the
    // real, persistent default data directory with `Acceptance.ps1`, which
    // always ran first and left the workspace already indexed. `symbols`,
    // `definition`, and `references` all require an existing store snapshot
    // (see `App::symbols`'s "workspace is not indexed" check) even though
    // navigation itself goes through rust-analyzer, not the vector store. This
    // acceptance command uses an isolated fixture data directory instead (so
    // it can run standalone, in any order, without depending on another
    // acceptance command having run first), so it must index explicitly.
    evidence.stage("index")?;
    let output = run_lci(config_path, &["index", workspace]).await?;
    if !output.success {
        bail!("gust-lsp-index failed: {}", output.combined());
    }
    let indexed = output.json()?;
    save_report("gust-lsp-index", &indexed)?;

    evidence.stage("symbols")?;
    let output = run_lci(config_path, &["symbols", workspace, "emit_expression#"]).await?;
    if !output.success {
        bail!("gust-lsp-symbols failed: {}", output.combined());
    }
    let symbols = output.json()?;
    save_report("gust-lsp-symbols", &symbols)?;

    evidence.stage("definition")?;
    let output = run_lci(
        config_path,
        &["definition", workspace, TARGET_FILE, "266", "24"],
    )
    .await?;
    if !output.success {
        bail!("gust-lsp-definition failed: {}", output.combined());
    }
    let definition = output.json()?;
    save_report("gust-lsp-definition", &definition)?;

    evidence.stage("references")?;
    let output = run_lci(
        config_path,
        &[
            "references",
            workspace,
            TARGET_FILE,
            "581",
            "8",
            "--include-declaration",
        ],
    )
    .await?;
    if !output.success {
        bail!("gust-lsp-references failed: {}", output.combined());
    }
    let references = output.json()?;
    save_report("gust-lsp-references", &references)?;

    evidence.stage("assertions")?;

    let symbols_results = symbols
        .get("results")
        .and_then(Value::as_array)
        .context("gust-lsp-symbols report missing results")?;
    let symbol_found = symbols_results.iter().any(|location| {
        location.get("name").and_then(Value::as_str) == Some("emit_expression")
            && location.get("start_line").and_then(Value::as_u64) == Some(581)
    });
    if !symbol_found {
        bail!("workspace symbol search did not resolve emit_expression at line 581");
    }

    let definition_results = definition
        .get("results")
        .and_then(Value::as_array)
        .context("gust-lsp-definition report missing results")?;
    let definition_found = definition_results
        .iter()
        .any(|location| location.get("start_line").and_then(Value::as_u64) == Some(581));
    if !definition_found {
        bail!("definition lookup did not resolve the call at line 266 to line 581");
    }

    let reference_count = references
        .get("results")
        .and_then(Value::as_array)
        .context("gust-lsp-references report missing results")?
        .len();
    if reference_count < 2 {
        bail!("reference lookup returned too few locations");
    }

    Ok(format!(
        "PASS: symbol, definition, and {reference_count} reference locations. Reports: {}",
        test_results_dir()?.display()
    ))
}
