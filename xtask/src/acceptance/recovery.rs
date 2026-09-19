//! Generic engine for the "persistent process reuse and forced-death
//! recovery" acceptance check shared by every language family: builds a
//! real fixture, starts an owned `serve` process pointed at a real language
//! server, performs the MCP handshake, and calls a navigation tool twice to
//! prove the same server process(es) are reused across requests. It then
//! force-kills them, calls the navigation tool a third time, and confirms a
//! wholly new replacement process (or process set) appears -- no restart
//! loop, no leftover old PIDs.
//!
//! The three language families diverge in two structural ways this module
//! models explicitly rather than papering over:
//!   - csharp-ls is a native executable spawned directly as a single child
//!     process ([`ProcessCheck::SingleChild`]); typescript-language-server
//!     and pyright are npm `.cmd` shims that spawn through an intermediate
//!     `cmd.exe` on Windows, landing as a *set* of `node` descendants at
//!     varying depth ([`ProcessCheck::DescendantSet`]).
//!   - Against a freshly-spawned server with no file open yet,
//!     `search_symbols` reliably proves csharp-ls navigation, but was
//!     observed to return empty for typescript/pyright; those two use
//!     `find_definition` instead ([`NavigationProbe`]).
//!
//! Fixture scaffolding also differs ([`FixtureScaffold`]): C# needs a real
//! `dotnet` solution/build, TypeScript needs `npm init` plus a pinned
//! `typescript` install, and Python needs neither.

use super::{
    Evidence, Fixture, ManagedServer, McpSession, run_dotnet, run_npm, wait_http_ok, which,
};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

const BASE: &str = "http://127.0.0.1:8769"; // matches acceptance::PORT

pub enum ProcessCheck {
    /// A native executable spawned directly as a child (csharp-ls). Exactly
    /// one is ever expected to exist at a time.
    SingleChild(&'static str),
    /// An npm-installed tool that spawns through an intermediate shell
    /// wrapper on Windows, landing two or more levels deep; more than one
    /// matching process can coexist (typescript-language-server, pyright).
    DescendantSet(&'static str),
}

impl ProcessCheck {
    fn current(&self, managed: &ManagedServer) -> Vec<u32> {
        match self {
            ProcessCheck::SingleChild(name) => managed.children_named(name),
            ProcessCheck::DescendantSet(name) => managed.descendants_named(name),
        }
    }

    /// Reduces a raw snapshot to what gets tracked as "first": just the one
    /// pid for `SingleChild` (matching the single-native-process
    /// assumption), or the whole list for `DescendantSet`.
    fn track(&self, current: &[u32]) -> Vec<u32> {
        match self {
            ProcessCheck::SingleChild(_) => current.first().into_iter().copied().collect(),
            ProcessCheck::DescendantSet(_) => current.to_vec(),
        }
    }

    fn reuse_confirmed(&self, first: &[u32], current: &[u32]) -> bool {
        match self {
            ProcessCheck::SingleChild(_) => current.len() == 1 && first.first() == current.first(),
            ProcessCheck::DescendantSet(_) => {
                !current.is_empty() && as_set(current) == as_set(first)
            }
        }
    }

    fn recovery_confirmed(&self, first: &[u32], current: &[u32]) -> bool {
        match self {
            ProcessCheck::SingleChild(_) => current.len() == 1 && first.first() != current.first(),
            ProcessCheck::DescendantSet(_) => {
                !current.is_empty() && as_set(current).is_disjoint(&as_set(first))
            }
        }
    }
}

fn as_set(pids: &[u32]) -> HashSet<u32> {
    pids.iter().copied().collect()
}

pub enum FixtureScaffold {
    /// Source files are already real, buildable source; no extra setup.
    None,
    /// `npm init -y`, then `npm install <install...>`.
    Npm { install: &'static [&'static str] },
    /// `dotnet new sln`, `dotnet solution add`, `dotnet restore`,
    /// `dotnet build`.
    DotNetSolution {
        csproj_filename: &'static str,
        csproj: &'static str,
        solution_name: &'static str,
    },
}

pub enum NavigationProbe {
    Symbols {
        query: &'static str,
        expect_substring: &'static str,
    },
    Definition {
        relative_file_path: &'static str,
        line: u32,
        character: u32,
        expect_substring: &'static str,
    },
}

impl NavigationProbe {
    fn tool_name(&self) -> &'static str {
        match self {
            NavigationProbe::Symbols { .. } => "search_symbols",
            NavigationProbe::Definition { .. } => "find_definition",
        }
    }

    fn args(&self, workspace: &str) -> Value {
        match self {
            NavigationProbe::Symbols { query, .. } => {
                json!({"workspace_path": workspace, "query": query})
            }
            NavigationProbe::Definition {
                relative_file_path,
                line,
                character,
                ..
            } => json!({
                "workspace_path": workspace,
                "relative_file_path": relative_file_path,
                "line": line,
                "character": character,
            }),
        }
    }

    fn expect_substring(&self) -> &'static str {
        match self {
            NavigationProbe::Symbols {
                expect_substring, ..
            }
            | NavigationProbe::Definition {
                expect_substring, ..
            } => expect_substring,
        }
    }
}

pub struct RecoverySpec {
    /// Short identifier used to name the fixture, evidence file, and config
    /// section (e.g. `"csharp"`, `"typescript"`, `"python"`).
    pub language_key: &'static str,
    /// Human-readable name for log messages (e.g. `"C#"`).
    pub display_name: &'static str,
    /// Executable name passed to [`super::which`] to resolve the real
    /// server (e.g. `"csharp-ls"`, `"pyright-langserver"`).
    pub which_name: &'static str,
    /// How the resolver is named in the `--flag` the CLI accepts, for the
    /// prerequisite-unavailable message (e.g. `"pyright"` for
    /// `--pyright`, distinct from the `pyright-langserver` executable name).
    pub cli_flag_display: &'static str,
    /// Name of an environment variable holding a fallback path, checked
    /// after `PATH` and before giving up.
    pub fallback_env: Option<&'static str>,
    /// An external tool that must additionally be on PATH to scaffold the
    /// fixture (`"dotnet"` for C#), checked after the server itself
    /// resolves.
    pub requires_tool: Option<&'static str>,
    pub scaffold: FixtureScaffold,
    pub source_files: Vec<(&'static str, &'static str)>,
    pub config_section: &'static str,
    pub extra_config_lines: Vec<String>,
    pub lsp_timeout_seconds: u32,
    /// Attempts (500ms apart) allowed for the server's process(es) to first
    /// appear, and separately for a replacement to appear after a forced
    /// kill.
    pub attempts: u32,
    pub probe: NavigationProbe,
    pub process_check: ProcessCheck,
    /// Label for the "no process appeared" failure message (e.g.
    /// `"csharp-ls"`, `"node descendant"`).
    pub process_label: &'static str,
}

pub async fn run(spec: RecoverySpec, explicit: Option<PathBuf>) -> Result<()> {
    let mut evidence = Evidence::new(&format!("{}-lsp-recovery", spec.language_key))?;
    evidence.set("base_url", BASE)?;

    evidence.stage("resolve-prerequisites")?;
    let Some(server_path) = resolve_server(&spec, explicit) else {
        evidence.set("status", "prerequisite-unavailable")?;
        let fallback_note = match spec.fallback_env {
            Some(var) => format!(", or via ${var}"),
            None => String::new(),
        };
        println!(
            "PREREQUISITE_UNAVAILABLE: {} not found on PATH or via --{}{fallback_note}. Evidence: {}",
            spec.which_name,
            spec.cli_flag_display,
            evidence.path().display()
        );
        return Ok(());
    };
    if let Some(tool) = spec.requires_tool
        && which(tool).is_none()
    {
        bail!(
            "{tool} is required for the real {} fixture",
            spec.display_name
        );
    }

    let fixture = Fixture::create(&format!("{}-recovery", spec.language_key))?;
    let mut server: Option<ManagedServer> = None;

    let outcome = checks(&mut evidence, &fixture, &mut server, &spec, &server_path).await;

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
                "PASS: {} LSP persistent-reuse and recovery acceptance. Evidence: {}",
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

/// Resolves the server executable/install directory for a recovery spec.
/// Accepts directories as well as files in the fallback check -- Java's
/// `path` is a jdtls installation directory, not an executable (see
/// `config.rs`'s `JavaLspConfig` doc comment).
fn resolve_server(spec: &RecoverySpec, explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path);
    }
    if let Some(path) = which(spec.which_name) {
        return Some(path);
    }
    if let Some(var) = spec.fallback_env
        && let Some(found) = super::env_fallback(var)
    {
        return Some(found);
    }
    None
}

async fn checks(
    evidence: &mut Evidence,
    fixture: &Fixture,
    server: &mut Option<ManagedServer>,
    spec: &RecoverySpec,
    server_path: &std::path::Path,
) -> Result<()> {
    evidence.stage("create-fixture")?;
    for (relative, content) in &spec.source_files {
        fixture.write(relative, content)?;
    }
    let fixture_dir_str = fixture.dir.to_string_lossy().to_string();

    match &spec.scaffold {
        FixtureScaffold::None => {}
        FixtureScaffold::Npm { install } => {
            run_npm(&fixture.dir, &["init", "-y"]).await?;
            let mut args = vec!["install"];
            args.extend(install.iter().copied());
            run_npm(&fixture.dir, &args).await?;
        }
        FixtureScaffold::DotNetSolution {
            csproj_filename,
            csproj,
            solution_name,
        } => {
            let project_path = fixture.write(csproj_filename, csproj)?;
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
            run_dotnet(&fixture.dir, &["build", &solution_path_str, "--no-restore"]).await?;
        }
    }

    evidence.stage("construct-config")?;
    let normalized_server = server_path.to_string_lossy().replace('\\', "/");
    let mut lines = vec![
        format!("lsp_timeout_seconds = {}", spec.lsp_timeout_seconds),
        format!("[{}]", spec.config_section),
        format!("path = '{normalized_server}'"),
    ];
    lines.extend(spec.extra_config_lines.iter().cloned());
    let config_path = fixture.write_config(&lines)?;

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
    let probe_args = spec.probe.args(&fixture_dir_str);
    let body1 = session
        .call_tool(spec.probe.tool_name(), probe_args.clone())
        .await?;
    let resolved_first = body1
        .to_lowercase()
        .contains(&spec.probe.expect_substring().to_lowercase());
    evidence.set("first_navigation_resolved", resolved_first)?;
    if !resolved_first {
        bail!(
            "first {} response did not contain {:?}: {body1}",
            spec.probe.tool_name(),
            spec.probe.expect_substring()
        );
    }

    let managed = server.as_ref().expect("server spawned above");
    let mut first: Vec<u32> = Vec::new();
    for _ in 0..spec.attempts {
        let current = spec.process_check.current(managed);
        if !current.is_empty() {
            first = spec.process_check.track(&current);
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if first.is_empty() {
        bail!(
            "no {} process appeared under the owned LCI process",
            spec.process_label
        );
    }
    evidence.set("first_processes", json!(first))?;

    evidence.stage("second-navigation-reuse")?;
    let body2 = session
        .call_tool(spec.probe.tool_name(), probe_args.clone())
        .await?;
    if !body2
        .to_lowercase()
        .contains(&spec.probe.expect_substring().to_lowercase())
    {
        bail!(
            "{} (reuse) did not contain {:?}: {body2}",
            spec.probe.tool_name(),
            spec.probe.expect_substring()
        );
    }
    let reused = spec.process_check.current(managed);
    evidence.set("reused_processes", json!(reused))?;
    let reuse_confirmed = spec.process_check.reuse_confirmed(&first, &reused);
    evidence.set("reuse_confirmed", reuse_confirmed)?;
    if !reuse_confirmed {
        bail!("process(es) not reused: first={first:?} reused={reused:?}");
    }

    evidence.stage("forced-death-recovery")?;
    evidence.set("killed_processes", json!(first))?;
    for pid in &first {
        ManagedServer::kill_process(*pid);
    }

    let body3 = session
        .call_tool(spec.probe.tool_name(), probe_args.clone())
        .await?;
    if !body3
        .to_lowercase()
        .contains(&spec.probe.expect_substring().to_lowercase())
    {
        bail!(
            "{} (recovery) did not contain {:?}: {body3}",
            spec.probe.tool_name(),
            spec.probe.expect_substring()
        );
    }
    let mut recovery: Vec<u32> = Vec::new();
    for _ in 0..spec.attempts {
        let current = spec.process_check.current(managed);
        if spec.process_check.recovery_confirmed(&first, &current) {
            recovery = current;
            break;
        }
        recovery = current;
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    evidence.set("recovery_processes", json!(recovery))?;
    let recovery_confirmed = spec.process_check.recovery_confirmed(&first, &recovery);
    evidence.set("recovery_confirmed", recovery_confirmed)?;
    if !recovery_confirmed {
        bail!("recovery not confirmed: killed={first:?} candidates={recovery:?}");
    }

    Ok(())
}
