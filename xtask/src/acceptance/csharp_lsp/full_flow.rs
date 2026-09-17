//! Ported from the non-`-ProbeOnly` branch of `scripts/Acceptance-CSharp-Lsp.ps1`.
//!
//! Builds a real `dotnet` fixture, points a `[csharp]`-configured
//! `local-code-intelligence` at it, and drives `index`/`symbols`/
//! `definition`/`references`/`search` through the compiled CLI, asserting
//! the same real LSP-derived locations and line/column correctness the
//! original script did.

use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::path::Path;

use super::{bound_text, command_version, is_build_output, results};
use crate::acceptance::{Evidence, Fixture, lci_binary, run_dotnet, run_lci, which};

const CSPROJ: &str = r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Library</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
  </PropertyGroup>
</Project>
"#;

const PRODUCTION_SOURCE: &str = "namespace Acceptance;\n\npublic interface ICalculator { int Add(int left, int right); }\n\npublic sealed class Calculator : ICalculator\n{\n    // Production billing arithmetic implementation.\n    public int Add(int left, int right) => left + right;\n    public int Use() => Add(2, 3);\n}\n";

const CALLSITE_SOURCE: &str = "namespace Acceptance;\n\npublic static class CallSite\n{\n    public static int Run(ICalculator calculator) => calculator.Add(1, 2);\n}\n";

const TESTS_SOURCE: &str = "namespace Acceptance.Tests;\n\n// Test-only Calculator usage and documentation example.\npublic static class CalculatorTests\n{\n    public static bool Example() => new Acceptance.Calculator().Add(1, 2) == 3;\n}\n";

/// The zero-based UTF-16-equivalent character offset of `needle` on the
/// given one-based source line (ASCII fixture sources, so byte/char/UTF-16
/// offsets coincide). Mirrors `Get-CallSitePosition` from the .ps1 script.
fn call_site_position(content: &str, line: usize, needle: &str) -> Result<(u32, u32)> {
    let lines: Vec<&str> = content.split('\n').collect();
    let line_text = *lines
        .get(line - 1)
        .with_context(|| format!("line {line} out of range"))?;
    let index = line_text
        .find(needle)
        .with_context(|| format!("needle '{needle}' not found on line {line}"))?;
    Ok((line as u32, index as u32))
}

pub async fn run(csharp_ls: &Path) -> Result<()> {
    let mut evidence = Evidence::new("csharp-lsp-acceptance")?;
    evidence.set("server", csharp_ls.to_string_lossy().into_owned())?;

    let fixture = Fixture::create("csharp-lsp")?;
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
            "PASS: C# LSP acceptance. Evidence: {}",
            evidence.path().display()
        );
    } else {
        println!(
            "FAIL: C# LSP acceptance. Evidence: {}",
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

async fn run_inner(csharp_ls: &Path, fixture: &Fixture, evidence: &mut Evidence) -> Result<()> {
    evidence.stage("resolve-prerequisites")?;
    let version = command_version(csharp_ls, "--version").await;
    evidence.set("version", version)?;
    which("dotnet").context("dotnet SDK is required for the real C# fixture")?;
    let dotnet_version = command_version(Path::new("dotnet"), "--version").await;
    evidence.set("dotnet", dotnet_version)?;
    let binary = lci_binary();
    if !binary.is_file() {
        bail!("debug binary missing: {}", binary.display());
    }

    evidence.stage("create-fixture")?;
    fixture.write(".gitignore", "bin/\nobj/\n")?;
    fixture.write("CSharpAcceptance.csproj", CSPROJ)?;
    fixture.write("src/Production.cs", PRODUCTION_SOURCE)?;
    fixture.write("src/CallSite.cs", CALLSITE_SOURCE)?;
    fixture.write("tests/CalculatorTests.cs", TESTS_SOURCE)?;

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

    evidence.stage("construct-config")?;
    let extra_lines = vec![
        "lsp_timeout_seconds = 60".to_string(),
        "[csharp]".to_string(),
        format!(
            "path = '{}'",
            csharp_ls.to_string_lossy().replace('\\', "/")
        ),
        "args = ['--solution', 'CSharpAcceptance.sln']".to_string(),
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
        files == 3,
        "expected exactly 3 indexed C# files, got {files}"
    );

    let symbols_value = run_json_stage(
        evidence,
        "workspace-symbol",
        &config_path,
        &["symbols", &fixture_dir, "Calculator"],
    )
    .await?;

    let (def_line, def_char) = call_site_position(CALLSITE_SOURCE, 5, "Add")?;
    let def_line_s = def_line.to_string();
    let def_char_s = def_char.to_string();
    let definition_value = run_json_stage(
        evidence,
        "definition",
        &config_path,
        &[
            "definition",
            &fixture_dir,
            "src/CallSite.cs",
            &def_line_s,
            &def_char_s,
        ],
    )
    .await?;

    let (ref_line, ref_char) = call_site_position(PRODUCTION_SOURCE, 8, "Add")?;
    let ref_line_s = ref_line.to_string();
    let ref_char_s = ref_char.to_string();
    let references_value = run_json_stage(
        evidence,
        "references",
        &config_path,
        &[
            "references",
            &fixture_dir,
            "src/Production.cs",
            &ref_line_s,
            &ref_char_s,
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
            "production billing arithmetic Calculator Add implementation",
            "--top-k",
            "8",
        ],
    )
    .await?;

    let symbol_results = results(&symbols_value);
    let definition_results = results(&definition_value);
    let reference_results = results(&references_value);
    let search_results = results(&search_value);

    evidence.set("workspace_symbols", symbol_results.len() as u64)?;
    evidence.set(
        "definition_position",
        serde_json::json!({"line": def_line, "character": def_char}),
    )?;
    evidence.set("definition", Value::Array(definition_results.clone()))?;
    evidence.set(
        "references_position",
        serde_json::json!({"line": ref_line, "character": ref_char}),
    )?;
    evidence.set("references", Value::Array(reference_results.clone()))?;
    evidence.set("search", Value::Array(search_results.clone()))?;

    evidence.stage("assert-results")?;
    ensure!(
        !symbol_results.is_empty(),
        "workspace symbols returned no results"
    );
    ensure!(
        symbol_results
            .iter()
            .any(|r| r.get("name").and_then(Value::as_str) == Some("Calculator")),
        "workspace symbols did not contain Calculator"
    );
    ensure!(
        !definition_results.is_empty(),
        "definition returned no locations"
    );
    ensure!(
        definition_results
            .iter()
            .any(|r| r.get("relative_file_path").and_then(Value::as_str)
                == Some("src/Production.cs")),
        "definition did not resolve to src/Production.cs"
    );
    ensure!(
        reference_results
            .iter()
            .any(|r| r.get("relative_file_path").and_then(Value::as_str)
                == Some("src/Production.cs")),
        "references did not include src/Production.cs"
    );
    ensure!(
        reference_results.iter().any(|r| r
            .get("relative_file_path")
            .and_then(Value::as_str)
            == Some("src/CallSite.cs")),
        "references did not include src/CallSite.cs"
    );

    for location in definition_results.iter().chain(reference_results.iter()) {
        let language = location
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or_default();
        ensure!(
            language == "csharp",
            "location has unexpected language: {language}"
        );
        let provider = location
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default();
        ensure!(
            provider == "csharp-ls",
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

    let mut production_rank: Option<usize> = None;
    let mut test_rank: Option<usize> = None;
    for (rank, result) in search_results.iter().enumerate() {
        let path = result
            .get("relative_file_path")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if path == "src/Production.cs" && production_rank.is_none() {
            production_rank = Some(rank);
        }
        if path == "tests/CalculatorTests.cs" && test_rank.is_none() {
            test_rank = Some(rank);
        }
    }
    ensure!(
        production_rank.is_some(),
        "production Calculator implementation was absent from search results"
    );
    if let (Some(production), Some(test)) = (production_rank, test_rank) {
        ensure!(
            production < test,
            "test/example result outranked the production Calculator implementation"
        );
    }
    let production_hit = search_results
        .iter()
        .find(|r| r.get("relative_file_path").and_then(Value::as_str) == Some("src/Production.cs"))
        .context("missing production hit in search results")?;
    let channels = production_hit
        .get("retrieval_channels")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    ensure!(
        channels.iter().any(|c| c.as_str() == Some("lsp")),
        "production result did not include the LSP retrieval channel"
    );
    ensure!(
        production_hit
            .get("semantic_score")
            .is_some_and(|v| !v.is_null()),
        "production result did not contain a semantic score"
    );
    ensure!(
        production_hit
            .get("reranker_score")
            .is_some_and(|v| !v.is_null()),
        "production result did not contain a reranker score"
    );

    Ok(())
}
