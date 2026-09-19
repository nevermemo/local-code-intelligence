//! Generic engine for the "positive-path" LSP acceptance shared by every
//! language family: builds a real fixture, points a `local-code-intelligence`
//! CLI config at a real language server, and drives `index`/`definition`/
//! `references`/`search` (plus, where applicable, `symbols`) through the
//! compiled CLI, asserting real cross-file LSP-derived locations and
//! line/column correctness against the actual `App::search_with_filters`
//! retrieval pipeline.
//!
//! The three language families agree on the overall shape but genuinely
//! differ in ways this module models explicitly:
//!   - `workspace/symbol` is fully asserted for C# ([`SymbolsStage::Asserted`]),
//!     recorded but not asserted for TypeScript (a fresh
//!     typescript-language-server process has no file open yet --
//!     [`SymbolsStage::Informational`]), and skipped entirely for Python
//!     ([`SymbolsStage::Skipped`]) -- see `src/lsp/{typescript,python}.rs`
//!     for the underlying, documented limitation.
//!   - The semantic-search assertion is either a full rank-vs-test-file plus
//!     retrieval-channel/score check ([`SearchAssertion::RankedWithScores`],
//!     C#/TypeScript) or a bare "did the production file appear"
//!     check ([`SearchAssertion::ContainsFile`], Python -- which has no
//!     separate test-file fixture).
//!   - Fixture scaffolding reuses [`super::recovery::FixtureScaffold`]: a
//!     real `dotnet` solution/build for C#, `npm init` plus a pinned
//!     `typescript` install for TypeScript, neither for Python.
//!
//! csharp-ls's standalone raw-JSON-RPC probe (`csharp_lsp/probe.rs`) is not
//! part of this module -- it proves a different thing (the wire protocol in
//! isolation, no dependency on the LCI binary) and has no TypeScript/Python
//! analog.

use super::recovery::FixtureScaffold;
use super::{Evidence, Fixture, LciOutput, lci_binary, run_dotnet, run_lci, run_npm, which};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::path::{Path, PathBuf};

pub enum IndexedFilesAssertion {
    Exact(u64),
    AtLeast(u64),
}

impl IndexedFilesAssertion {
    fn check(&self, files: u64) -> Result<()> {
        match self {
            IndexedFilesAssertion::Exact(expected) => ensure!(
                files == *expected,
                "expected exactly {expected} indexed files, got {files}"
            ),
            IndexedFilesAssertion::AtLeast(minimum) => ensure!(
                files >= *minimum,
                "expected at least {minimum} indexed files, got {files}"
            ),
        }
        Ok(())
    }
}

pub enum SymbolsStage {
    /// `workspace/symbol` is part of the pass/fail contract: must return a
    /// result naming `expect_name`.
    Asserted {
        query: &'static str,
        expect_name: &'static str,
    },
    /// Run and recorded for diagnostic value, but a failure (or empty
    /// result) is expected and not a test failure.
    Informational { query: &'static str },
    /// Not run at all.
    Skipped,
}

pub enum SearchAssertion {
    /// The production file must outrank `test_file` (if it also appears),
    /// and the production hit's scores (and, if `assert_lsp_channel`, its
    /// "lsp" retrieval channel) are checked.
    RankedWithScores {
        test_file: &'static str,
        assert_lsp_channel: bool,
    },
    /// The production file just needs to appear somewhere in the results.
    ContainsFile,
}

pub struct LspFullFlowSpec {
    pub language_key: &'static str,
    pub display_name: &'static str,
    pub which_name: &'static str,
    pub cli_flag_display: &'static str,
    pub windows_fallback: Option<&'static str>,
    /// Argument passed to `<server> <version_arg>` to record a version in
    /// evidence, or `None` when the server has no usable standalone version
    /// probe (pyright-langserver exits with a connection error for any
    /// argument other than a transport flag).
    pub version_arg: Option<&'static str>,
    pub scaffold: FixtureScaffold,
    /// Fixture source files (relative path, content), including any
    /// `.gitignore` needed before a scaffold install step.
    pub source_files: Vec<(&'static str, &'static str)>,
    pub config_section: &'static str,
    pub lsp_timeout_seconds: u32,
    pub extra_config_lines: Vec<String>,
    pub expected_indexed_files: IndexedFilesAssertion,
    pub symbols_stage: SymbolsStage,
    /// The file a `find_definition` call targets, and its content/position
    /// (one-based line, 0-based-search needle) used to locate the call site.
    pub call_site_file: &'static str,
    pub call_site_source: &'static str,
    pub call_site_line: usize,
    pub call_site_needle: &'static str,
    /// The file `find_definition` must resolve into, and where a
    /// `find_references` call is made from (its declaration site).
    pub declaration_file: &'static str,
    pub declaration_source: &'static str,
    pub declaration_line: usize,
    pub declaration_needle: &'static str,
    pub search_query: &'static str,
    pub search_assertion: SearchAssertion,
    pub language_identifier: &'static str,
    pub provider_name: &'static str,
    /// Path segments identifying generated/vendored output that must never
    /// leak into results (`["bin", "obj"]` for C#/Python,
    /// `["node_modules", "dist", "build"]` for TypeScript).
    pub generated_dirs: &'static [&'static str],
    /// Whether `references` is expected to include the usage in
    /// `call_site_file`, not just the declaration in `declaration_file`.
    /// True for every language except clangd: each `run_lci` stage is a
    /// separate one-shot CLI process with its own fresh server, and every
    /// other adapter's server does whole-project/package auto-discovery
    /// independent of which specific file that process opened (Cargo.toml,
    /// go.mod, .csproj, tsconfig/directory scan). clangd, confirmed live,
    /// does not: without a persistent session or a project-wide background-
    /// index scan completing (neither of which this two-separate-processes
    /// harness gives it time for, even with a real compile_commands.json --
    /// empirically confirmed, not assumed), a freshly spawned clangd that
    /// only opened `declaration_file` has no way to know `call_site_file`
    /// exists. A real, persistent `serve` process reuses one session across
    /// calls (`Manager`'s session cache) and does not have this limitation;
    /// this is a property of this acceptance harness's per-stage process
    /// model, not of LCI's actual navigation behavior for a real user.
    pub cross_file_references: bool,
}

pub async fn run(spec: LspFullFlowSpec, explicit: Option<PathBuf>) -> Result<()> {
    let mut evidence = Evidence::new(&format!("{}-lsp-acceptance", spec.language_key))?;
    let Some(resolved) = resolve_server(&spec, explicit) else {
        evidence.set("status", "prerequisite-unavailable")?;
        let fallback_note = if spec.windows_fallback.is_some() {
            ", or at the Windows fallback location"
        } else {
            ""
        };
        println!(
            "PREREQUISITE_UNAVAILABLE: {} not found on PATH or via --{}{fallback_note}. Evidence: {}",
            spec.which_name,
            spec.cli_flag_display,
            evidence.path().display()
        );
        return Ok(());
    };
    evidence.set("server", resolved.to_string_lossy().into_owned())?;

    let fixture = Fixture::create(&format!("{}-lsp", spec.language_key))?;
    let outcome = run_inner(&spec, &resolved, &fixture, &mut evidence).await;

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
            "PASS: {} LSP acceptance. Evidence: {}",
            spec.display_name,
            evidence.path().display()
        );
    } else {
        println!(
            "FAIL: {} LSP acceptance. Evidence: {}",
            spec.display_name,
            evidence.path().display()
        );
    }
    outcome
}

/// Resolves the server executable: the explicit CLI flag (either a literal
/// file/directory, or a bare command name looked up on PATH), else a PATH
/// lookup, else (Windows only) a well-known fallback install location.
///
/// Accepts directories as well as files: every family resolves to a single
/// executable except Java, where `path` in the written config is a jdtls
/// *installation directory* (`JavaServer` finds the actual launcher jar and
/// spawns `java` itself -- see `config.rs`'s `JavaLspConfig` doc comment
/// for why).
fn resolve_server(spec: &LspFullFlowSpec, explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        if path.exists() {
            return Some(path);
        }
        if let Some(name) = path.to_str()
            && let Some(found) = which(name)
        {
            return Some(found);
        }
        return None;
    }
    if let Some(found) = which(spec.which_name) {
        return Some(found);
    }
    if cfg!(windows)
        && let Some(fallback) = spec.windows_fallback
    {
        let fallback = PathBuf::from(fallback);
        if fallback.exists() {
            return Some(fallback);
        }
    }
    None
}

async fn command_version(exe: &Path, arg: &str) -> String {
    match tokio::process::Command::new(exe).arg(arg).output().await {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            text.trim().to_string()
        }
        Err(_) => String::new(),
    }
}

fn bound_text(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        trimmed.chars().take(max).collect()
    }
}

fn results(value: &Value) -> Vec<Value> {
    value
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn is_generated(relative_path: &str, generated_dirs: &[&str]) -> bool {
    relative_path
        .split('/')
        .any(|segment| generated_dirs.contains(&segment))
}

/// The zero-based character offset of `needle` on the given one-based source
/// line (fixture sources are ASCII, so byte/char/UTF-16 offsets coincide).
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

async fn run_scaffold(scaffold: &FixtureScaffold, fixture: &Fixture) -> Result<()> {
    match scaffold {
        FixtureScaffold::None => Ok(()),
        FixtureScaffold::Npm { install } => {
            run_npm(&fixture.dir, &["init", "-y"]).await?;
            let mut args = vec!["install"];
            args.extend(install.iter().copied());
            run_npm(&fixture.dir, &args).await
        }
        FixtureScaffold::DotNetSolution {
            csproj_filename,
            csproj,
            solution_name,
        } => {
            let project_path = fixture.write(csproj_filename, csproj)?;
            let fixture_dir_str = fixture.dir.to_string_lossy().to_string();
            let solution_path = fixture.dir.join(format!("{solution_name}.sln"));
            let project_path_str = project_path.to_string_lossy().to_string();
            let solution_path_str = solution_path.to_string_lossy().to_string();
            run_dotnet(
                &fixture.dir,
                &[
                    "new",
                    "sln",
                    "--format",
                    "sln",
                    "--name",
                    solution_name,
                    "--output",
                    &fixture_dir_str,
                ],
            )
            .await?;
            run_dotnet(
                &fixture.dir,
                &["solution", &solution_path_str, "add", &project_path_str],
            )
            .await?;
            run_dotnet(&fixture.dir, &["restore", &solution_path_str]).await?;
            run_dotnet(&fixture.dir, &["build", &solution_path_str, "--no-restore"]).await
        }
    }
}

fn required_tool(scaffold: &FixtureScaffold) -> Option<&'static str> {
    match scaffold {
        FixtureScaffold::None => None,
        FixtureScaffold::Npm { .. } => Some("npm"),
        FixtureScaffold::DotNetSolution { .. } => Some("dotnet"),
    }
}

async fn run_inner(
    spec: &LspFullFlowSpec,
    server: &Path,
    fixture: &Fixture,
    evidence: &mut Evidence,
) -> Result<()> {
    evidence.stage("resolve-prerequisites")?;
    if let Some(arg) = spec.version_arg {
        let version = command_version(server, arg).await;
        evidence.set("version", version)?;
    }
    if let Some(tool) = required_tool(&spec.scaffold) {
        which(tool).with_context(|| {
            format!(
                "{tool} is required to scaffold the real {} fixture",
                spec.display_name
            )
        })?;
    }
    let binary = lci_binary();
    if !binary.is_file() {
        bail!("debug binary missing: {}", binary.display());
    }

    evidence.stage("create-fixture")?;
    for (relative, content) in &spec.source_files {
        fixture.write(relative, content)?;
    }
    run_scaffold(&spec.scaffold, fixture).await?;

    let fixture_dir = fixture.dir.to_string_lossy().into_owned();

    evidence.stage("construct-config")?;
    let mut extra_lines = vec![
        format!("lsp_timeout_seconds = {}", spec.lsp_timeout_seconds),
        format!("[{}]", spec.config_section),
        format!("path = '{}'", server.to_string_lossy().replace('\\', "/")),
    ];
    extra_lines.extend(spec.extra_config_lines.iter().cloned());
    let config_path = fixture.write_config(&extra_lines)?;

    let index_value =
        run_json_stage(evidence, "index", &config_path, &["index", &fixture_dir]).await?;
    let files = index_value
        .get("files")
        .and_then(Value::as_u64)
        .context("index result missing files count")?;
    evidence.set("index_files", files)?;
    spec.expected_indexed_files.check(files)?;

    run_symbols_stage(evidence, &spec.symbols_stage, &config_path, &fixture_dir).await?;

    let (def_line, def_char) = call_site_position(
        spec.call_site_source,
        spec.call_site_line,
        spec.call_site_needle,
    )?;
    let def_line_s = def_line.to_string();
    let def_char_s = def_char.to_string();
    let definition_value = run_json_stage(
        evidence,
        "definition",
        &config_path,
        &[
            "definition",
            &fixture_dir,
            spec.call_site_file,
            &def_line_s,
            &def_char_s,
        ],
    )
    .await?;

    let (ref_line, ref_char) = call_site_position(
        spec.declaration_source,
        spec.declaration_line,
        spec.declaration_needle,
    )?;
    let ref_line_s = ref_line.to_string();
    let ref_char_s = ref_char.to_string();
    let references_value = run_json_stage(
        evidence,
        "references",
        &config_path,
        &[
            "references",
            &fixture_dir,
            spec.declaration_file,
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
        &["search", &fixture_dir, spec.search_query, "--top-k", "8"],
    )
    .await?;

    let definition_results = results(&definition_value);
    let reference_results = results(&references_value);
    let search_results = results(&search_value);

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
        !definition_results.is_empty(),
        "definition returned no locations"
    );
    ensure!(
        definition_results
            .iter()
            .any(|r| r.get("relative_file_path").and_then(Value::as_str)
                == Some(spec.declaration_file)),
        "definition did not resolve to {}",
        spec.declaration_file
    );
    ensure!(
        !reference_results.is_empty(),
        "references returned no locations"
    );
    ensure!(
        reference_results
            .iter()
            .any(|r| r.get("relative_file_path").and_then(Value::as_str)
                == Some(spec.declaration_file)),
        "references did not include the declaration in {}",
        spec.declaration_file
    );
    if spec.cross_file_references {
        ensure!(
            reference_results
                .iter()
                .any(|r| r.get("relative_file_path").and_then(Value::as_str)
                    == Some(spec.call_site_file)),
            "references did not include the usage in {}",
            spec.call_site_file
        );
    }

    for location in definition_results.iter().chain(reference_results.iter()) {
        let language = location
            .get("language")
            .and_then(Value::as_str)
            .unwrap_or_default();
        ensure!(
            language == spec.language_identifier,
            "location has unexpected language: {language}"
        );
        let provider = location
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or_default();
        ensure!(
            provider == spec.provider_name,
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
            !is_generated(relative_file_path, spec.generated_dirs),
            "generated/vendored output appeared in results: {relative_file_path}"
        );
    }

    assert_search(
        &spec.search_assertion,
        spec.declaration_file,
        &search_results,
        evidence,
    )?;

    Ok(())
}

async fn run_symbols_stage(
    evidence: &mut Evidence,
    stage: &SymbolsStage,
    config_path: &Path,
    fixture_dir: &str,
) -> Result<()> {
    match stage {
        SymbolsStage::Asserted { query, expect_name } => {
            let symbols_value = run_json_stage(
                evidence,
                "workspace-symbol",
                config_path,
                &["symbols", fixture_dir, query],
            )
            .await?;
            let symbol_results = results(&symbols_value);
            evidence.set("workspace_symbols", symbol_results.len() as u64)?;
            ensure!(
                !symbol_results.is_empty(),
                "workspace symbols returned no results"
            );
            ensure!(
                symbol_results
                    .iter()
                    .any(|r| r.get("name").and_then(Value::as_str) == Some(*expect_name)),
                "workspace symbols did not contain {expect_name}"
            );
        }
        SymbolsStage::Informational { query } => {
            // Informational only: a fresh one-shot CLI process has no file
            // open yet when workspace/symbol is requested, so a failure or
            // empty result here is expected and documented, not a failure of
            // this acceptance.
            evidence.stage("workspace-symbol-attempt")?;
            let symbol_output: LciOutput =
                run_lci(config_path, &["symbols", fixture_dir, query]).await?;
            let note = if symbol_output.success {
                match symbol_output.json() {
                    Ok(value) => format!("succeeded with {} result(s)", results(&value).len()),
                    Err(_) => "succeeded but returned non-JSON output".to_string(),
                }
            } else {
                format!(
                    "failed as expected (documented limitation: workspace/symbol on a fresh server process with no files opened yet): {}",
                    bound_text(&symbol_output.combined(), 300)
                )
            };
            evidence.set("workspace_symbol_note", note)?;
        }
        SymbolsStage::Skipped => {}
    }
    Ok(())
}

fn assert_search(
    assertion: &SearchAssertion,
    production_file: &str,
    search_results: &[Value],
    evidence: &mut Evidence,
) -> Result<()> {
    match assertion {
        SearchAssertion::RankedWithScores {
            test_file,
            assert_lsp_channel,
        } => {
            let mut production_rank: Option<usize> = None;
            let mut test_rank: Option<usize> = None;
            for (rank, result) in search_results.iter().enumerate() {
                let path = result
                    .get("relative_file_path")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if path == production_file && production_rank.is_none() {
                    production_rank = Some(rank);
                }
                if path == *test_file && test_rank.is_none() {
                    test_rank = Some(rank);
                }
            }
            ensure!(
                production_rank.is_some(),
                "production implementation was absent from search results"
            );
            if let (Some(production), Some(test)) = (production_rank, test_rank) {
                ensure!(
                    production < test,
                    "test/example result outranked the production implementation"
                );
            }
            let production_hit = search_results
                .iter()
                .find(|r| {
                    r.get("relative_file_path").and_then(Value::as_str) == Some(production_file)
                })
                .context("missing production hit in search results")?;
            let channels = production_hit
                .get("retrieval_channels")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let has_lsp_channel = channels.iter().any(|c| c.as_str() == Some("lsp"));
            if *assert_lsp_channel {
                ensure!(
                    has_lsp_channel,
                    "production result did not include the LSP retrieval channel"
                );
            } else {
                // Informational only: `search`'s own "lsp" channel is powered
                // by workspace/symbol queries internally, which share the
                // same fresh-process limitation as the dedicated symbols
                // stage -- not a regression when absent here.
                evidence.set("production_hit_includes_lsp_channel", has_lsp_channel)?;
            }
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
        }
        SearchAssertion::ContainsFile => {
            ensure!(
                search_results
                    .iter()
                    .any(|r| r.get("relative_file_path").and_then(Value::as_str)
                        == Some(production_file)),
                "semantic search did not surface {production_file}"
            );
        }
    }
    Ok(())
}
