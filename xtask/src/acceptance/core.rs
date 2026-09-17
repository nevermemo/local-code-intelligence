//! Ported from `scripts/Acceptance.ps1`.

use crate::acceptance::{Evidence, Fixture, run_lci, test_results_dir};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

const DEFAULT_WORKSPACE: &str = r"C:\Users\micro\Desktop\gpu-dialect-v0";
const QUERY: &str = "lower syn AST expressions into generated Slang compute shader code";
const TRANSLATOR_PREFIX: &str = "crates/gust-macros/src/slang/";

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

    let fixture = Fixture::create("acceptance-core")?;
    let config_path = fixture.write_config(&[])?;

    let mut evidence = Evidence::new("acceptance-core")?;
    evidence.set("workspace", workspace_str.clone())?;

    let outcome = run_inner(&workspace_str, &config_path, &mut evidence).await;

    fixture.cleanup();
    if !fixture.removed() {
        bail!("acceptance core: fixture cleanup did not fully remove temp directories");
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
    evidence.stage("index")?;
    let output = run_lci(config_path, &["index", workspace]).await?;
    if !output.success {
        bail!("gust-index failed: {}", output.combined());
    }
    let indexed = output.json()?;
    save_report("gust-index", &indexed)?;

    evidence.stage("search")?;
    let output = run_lci(config_path, &["search", workspace, QUERY]).await?;
    if !output.success {
        bail!("gust-search failed: {}", output.combined());
    }
    let search = output.json()?;
    save_report("gust-search", &search)?;

    evidence.stage("reindex")?;
    let output = run_lci(config_path, &["index", workspace]).await?;
    if !output.success {
        bail!("gust-reindex failed: {}", output.combined());
    }
    let repeat = output.json()?;
    save_report("gust-reindex", &repeat)?;

    evidence.stage("assertions")?;

    let embedded_chunks = repeat
        .get("embedded_chunks")
        .and_then(Value::as_i64)
        .context("gust-reindex report missing embedded_chunks")?;
    if embedded_chunks != 0 {
        bail!("Unchanged reindex recomputed embeddings");
    }

    let reranked = search
        .get("reranked")
        .and_then(Value::as_bool)
        .context("gust-search report missing reranked")?;
    if !reranked {
        let warning = search
            .get("warning")
            .and_then(Value::as_str)
            .unwrap_or_default();
        bail!("Live reranking did not succeed: {warning}");
    }

    let results = search
        .get("results")
        .and_then(Value::as_array)
        .context("gust-search report missing results")?;

    let implementation: Vec<&Value> = results
        .iter()
        .filter(|hit| {
            hit.get("relative_file_path")
                .and_then(Value::as_str)
                .map(|path| path.replace('\\', "/").starts_with(TRANSLATOR_PREFIX))
                .unwrap_or(false)
        })
        .collect();

    if (implementation.len() as f64) <= (results.len() as f64) / 2.0 {
        bail!("Translator implementation did not dominate top results");
    }

    let has_expression_translator = implementation.iter().any(|hit| {
        hit.get("code")
            .and_then(Value::as_str)
            .map(|code| code.contains("fn emit_expression("))
            .unwrap_or(false)
    });
    if !has_expression_translator {
        bail!("Expression translator definition missing from top results");
    }

    let chunks = indexed
        .get("chunks")
        .and_then(Value::as_i64)
        .context("gust-index report missing chunks")?;

    Ok(format!(
        "PASS: {chunks} chunks; {} translator results; zero new embeddings on repeat. Reports: {}",
        implementation.len(),
        test_results_dir()?.display()
    ))
}
