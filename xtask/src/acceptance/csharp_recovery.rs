//! Ported from `scripts/CSharpLspRecovery.ps1`.
//!
//! Acceptance for persistent `serve` process reuse and forced-death recovery
//! of the C# language server child process, over the real MCP HTTP endpoint.
//! Builds a real `dotnet` C# fixture, starts an owned `serve` process pointed
//! at a real `csharp-ls`, performs the MCP handshake, and calls
//! `search_symbols` (over real MCP `tools/call`) twice to prove the same
//! `csharp-ls` child process is reused across requests. It then force-kills
//! that child, calls `search_symbols` a third time, and confirms exactly one
//! new replacement child process appears (no restart loop).

use super::{Evidence, Fixture, ManagedServer, McpSession, run_dotnet, wait_http_ok, which};
use anyhow::{Result, anyhow, bail};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;

const CSPROJ: &str = r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Library</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
  </PropertyGroup>
</Project>
"#;

const PRODUCTION_CS: &str = r#"namespace Acceptance;

public interface ICalculator { int Add(int left, int right); }

public sealed class Calculator : ICalculator
{
    public int Add(int left, int right) => left + right;
    public int Use() => Add(2, 3);
}
"#;

const BASE: &str = "http://127.0.0.1:8768";
const WINDOWS_CSHARP_LS_FALLBACK: &str = r"C:\Users\micro\.dotnet\tools\csharp-ls.exe";

pub async fn run(csharp_ls: Option<PathBuf>) -> Result<()> {
    let mut evidence = Evidence::new("csharp-lsp-recovery")?;
    evidence.set("base_url", BASE)?;

    evidence.stage("resolve-prerequisites")?;
    let Some(csharp_ls_path) = resolve_csharp_ls(csharp_ls) else {
        evidence.set("status", "prerequisite-unavailable")?;
        println!(
            "PREREQUISITE_UNAVAILABLE: csharp-ls not found on PATH, via --csharp-ls, or at the Windows fallback location. Evidence: {}",
            evidence.path().display()
        );
        return Ok(());
    };
    if which("dotnet").is_none() {
        bail!("dotnet SDK is required for the real C# fixture");
    }

    let fixture = Fixture::create("csharp-recovery")?;
    let mut server: Option<ManagedServer> = None;

    let outcome = checks(
        &mut evidence,
        &fixture,
        &mut server,
        csharp_ls_path.as_path(),
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
                "PASS: C# LSP persistent-reuse and recovery acceptance. Evidence: {}",
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

/// Resolves the `csharp-ls` executable to use: the explicit `--csharp-ls`
/// flag, then PATH, then (Windows only) a well-known dotnet-tools fallback
/// location. Returns `None` when none of those resolve to a real file, which
/// callers treat as a graceful prerequisite-unavailable skip.
fn resolve_csharp_ls(csharp_ls: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = csharp_ls {
        return Some(path);
    }
    if let Some(path) = which("csharp-ls") {
        return Some(path);
    }
    if cfg!(windows) {
        let fallback = PathBuf::from(WINDOWS_CSHARP_LS_FALLBACK);
        if fallback.is_file() {
            return Some(fallback);
        }
    }
    None
}

async fn checks(
    evidence: &mut Evidence,
    fixture: &Fixture,
    server: &mut Option<ManagedServer>,
    csharp_ls_path: &Path,
) -> Result<()> {
    evidence.stage("create-fixture")?;
    let project_path = fixture.write("CSharpAcceptance.csproj", CSPROJ)?;
    fixture.write("src/Production.cs", PRODUCTION_CS)?;
    let solution_path = fixture.dir.join("CSharpAcceptance.sln");

    let fixture_dir_str = fixture.dir.to_string_lossy().to_string();
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
            "CSharpAcceptance",
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
    run_dotnet(&fixture.dir, &["build", &solution_path_str, "--no-restore"]).await?;

    evidence.stage("construct-config")?;
    let normalized_csharp_ls = csharp_ls_path.to_string_lossy().replace('\\', "/");
    let config_path = fixture.write_config(&[
        "lsp_timeout_seconds = 60".to_string(),
        "[csharp]".to_string(),
        format!("path = '{normalized_csharp_ls}'"),
        "args = ['--solution', 'CSharpAcceptance.sln']".to_string(),
    ])?;

    evidence.stage("start-serve")?;
    let managed = ManagedServer::spawn(&config_path, &fixture.dir, &fixture.dir).await?;
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

    evidence.stage("index-workspace")?;
    session
        .call_tool(
            "index_workspace",
            json!({"workspace_path": fixture_dir_str}),
        )
        .await?;

    evidence.stage("first-navigation")?;
    let symbol_args = json!({"workspace_path": fixture_dir_str, "query": "Calculator"});
    let body1 = session
        .call_tool("search_symbols", symbol_args.clone())
        .await?;
    let workspace_symbol_results = body1.to_lowercase().matches("calculator").count();
    evidence.set("workspace_symbol_results", workspace_symbol_results as u64)?;
    if workspace_symbol_results < 1 {
        bail!("first search_symbols response did not contain Calculator: {body1}");
    }

    let managed = server.as_ref().expect("server spawned above");
    let mut first_child = None;
    for _ in 0..20 {
        let children = managed.children_named("csharp-ls");
        if let Some(&pid) = children.first() {
            first_child = Some(pid);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let Some(first_child) = first_child else {
        bail!("no csharp-ls child process appeared under the owned LCI process");
    };
    evidence.set("first_child_pid", first_child)?;

    evidence.stage("second-navigation-reuse")?;
    let body2 = session
        .call_tool("search_symbols", symbol_args.clone())
        .await?;
    if !body2.to_lowercase().contains("calculator") {
        bail!("search_symbols (reuse) did not contain Calculator: {body2}");
    }
    let reused_children = managed.children_named("csharp-ls");
    let reused_child_pid = reused_children.first().copied();
    evidence.set("reused_child_pid", reused_child_pid)?;
    let reuse_confirmed = reused_children.len() == 1 && reused_children[0] == first_child;
    evidence.set("reuse_confirmed", reuse_confirmed)?;
    if !reuse_confirmed {
        bail!("process was not reused: first={first_child} reused={reused_children:?}");
    }

    evidence.stage("forced-death-recovery")?;
    evidence.set("killed_child_pid", first_child)?;
    ManagedServer::kill_process(first_child);

    let body3 = session
        .call_tool("search_symbols", symbol_args.clone())
        .await?;
    if !body3.to_lowercase().contains("calculator") {
        bail!("search_symbols (recovery) did not contain Calculator: {body3}");
    }
    let mut recovery_children = Vec::new();
    for _ in 0..20 {
        recovery_children = managed.children_named("csharp-ls");
        if recovery_children
            .first()
            .is_some_and(|&pid| pid != first_child)
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let recovery_child_pid = recovery_children.first().copied();
    evidence.set("recovery_child_pid", recovery_child_pid)?;
    evidence.set("replacement_process_count", recovery_children.len() as u64)?;
    let recovery_confirmed = recovery_children.len() == 1 && recovery_children[0] != first_child;
    evidence.set("recovery_confirmed", recovery_confirmed)?;
    if !recovery_confirmed {
        bail!("recovery not confirmed: killed={first_child} candidates={recovery_children:?}");
    }

    Ok(())
}
