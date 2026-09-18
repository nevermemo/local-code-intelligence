//! Ported from the C# family's `csharp_recovery.rs`, adapted for Python.
//!
//! Acceptance for persistent `serve` process reuse and forced-death recovery
//! of the pyright child process tree, over the real MCP HTTP endpoint.
//! Builds a real Python package fixture, starts an owned `serve` process
//! pointed at a real `pyright-langserver`, performs the MCP handshake, and
//! calls `find_definition` (over real MCP `tools/call`) twice to prove the
//! same set of `node` descendant processes is reused across requests. It
//! then force-kills every `node` descendant, calls `find_definition` a third
//! time, and confirms a wholly new, non-overlapping set of `node`
//! descendants appears (no restart loop).
//!
//! `find_definition`/`find_references` are used here rather than
//! `search_symbols`: against a freshly-spawned pyright that has never had a
//! file opened, `search_symbols` (workspace/symbol) was observed to return
//! an empty result rather than exercising real navigation, whereas
//! `find_definition` reliably drives a real cross-file resolution once
//! `PythonServer::before_position_request` (src/lsp/python.rs) opens the
//! target file.
//!
//! Unlike csharp-ls (a native executable spawned directly as LCI's child),
//! `pyright-langserver` is an npm `.cmd` shim that, on Windows, spawns as
//! `local-code-intelligence.exe -> cmd.exe -> node.exe -> node.exe`, so the
//! real server process is a grandchild or deeper. `ManagedServer::
//! descendants_named("node")` (recursive, any depth) is used instead of
//! `children_named` for every "did a child process spawn" check, and the
//! reuse/recovery checks compare *sets* of PIDs rather than a single PID
//! since more than one `node` descendant can be present at once.

use super::{Evidence, Fixture, ManagedServer, McpSession, wait_http_ok, which};
use anyhow::{Result, anyhow, bail};
use serde_json::json;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

const INIT_PY: &str = "";

const CALCULATOR_PY: &str = r#"class Calculator:
    def add(self, a, b):
        return a + b
"#;

const CALL_SITE_PY: &str = "from .calculator import Calculator\n\n\ndef run(value: Calculator) -> int:\n    return value.add(1, 2)\n";

const BASE: &str = "http://127.0.0.1:8768";

pub async fn run(pyright: Option<PathBuf>) -> Result<()> {
    let mut evidence = Evidence::new("python-lsp-recovery")?;
    evidence.set("base_url", BASE)?;

    evidence.stage("resolve-prerequisites")?;
    let Some(pyright_path) = resolve_pyright(pyright) else {
        evidence.set("status", "prerequisite-unavailable")?;
        println!(
            "PREREQUISITE_UNAVAILABLE: pyright-langserver not found on PATH or via --pyright. Evidence: {}",
            evidence.path().display()
        );
        return Ok(());
    };

    let fixture = Fixture::create("python-recovery")?;
    let mut server: Option<ManagedServer> = None;

    let outcome = checks(&mut evidence, &fixture, &mut server, pyright_path.as_path()).await;

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
                "PASS: Python LSP persistent-reuse and recovery acceptance. Evidence: {}",
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

/// Resolves the `pyright-langserver` executable to use: the explicit
/// `--pyright` flag, then PATH. Returns `None` when neither resolves to a
/// real file, which callers treat as a graceful prerequisite-unavailable
/// skip.
fn resolve_pyright(pyright: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = pyright {
        return Some(path);
    }
    which("pyright-langserver")
}

async fn checks(
    evidence: &mut Evidence,
    fixture: &Fixture,
    server: &mut Option<ManagedServer>,
    pyright_path: &Path,
) -> Result<()> {
    evidence.stage("create-fixture")?;
    fixture.write("src/__init__.py", INIT_PY)?;
    fixture.write("src/calculator.py", CALCULATOR_PY)?;
    fixture.write("src/call_site.py", CALL_SITE_PY)?;

    let fixture_dir_str = fixture.dir.to_string_lossy().to_string();

    evidence.stage("construct-config")?;
    let normalized_pyright = pyright_path.to_string_lossy().replace('\\', "/");
    let config_path = fixture.write_config(&[
        "lsp_timeout_seconds = 30".to_string(),
        "[python]".to_string(),
        format!("path = '{normalized_pyright}'"),
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
    let definition_args = json!({
        "workspace_path": fixture_dir_str,
        "relative_file_path": "src/call_site.py",
        "line": 5,
        "character": 17,
    });
    let body1 = session
        .call_tool("find_definition", definition_args.clone())
        .await?;
    let resolved_first = body1.to_lowercase().contains("calculator.py");
    evidence.set("first_definition_resolved", resolved_first)?;
    if !resolved_first {
        bail!("first find_definition response did not resolve to calculator.py: {body1}");
    }

    let managed = server.as_ref().expect("server spawned above");
    let mut first_children: Vec<u32> = Vec::new();
    for _ in 0..30 {
        first_children = managed.descendants_named("node");
        if !first_children.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if first_children.is_empty() {
        bail!("no node descendant process appeared under the owned LCI process");
    }
    let first_set: HashSet<u32> = first_children.iter().copied().collect();
    evidence.set("first_node_descendants", json!(first_children))?;

    evidence.stage("second-navigation-reuse")?;
    let body2 = session
        .call_tool("find_definition", definition_args.clone())
        .await?;
    if !body2.to_lowercase().contains("calculator.py") {
        bail!("find_definition (reuse) did not resolve to calculator.py: {body2}");
    }
    let reused_children = managed.descendants_named("node");
    let reused_set: HashSet<u32> = reused_children.iter().copied().collect();
    evidence.set("reused_node_descendants", json!(reused_children))?;
    let reuse_confirmed = !reused_set.is_empty() && reused_set == first_set;
    evidence.set("reuse_confirmed", reuse_confirmed)?;
    if !reuse_confirmed {
        bail!("process set was not reused: first={first_set:?} reused={reused_set:?}");
    }

    evidence.stage("forced-death-recovery")?;
    evidence.set("killed_node_descendants", json!(first_children))?;
    for pid in &first_children {
        ManagedServer::kill_process(*pid);
    }

    let body3 = session
        .call_tool("find_definition", definition_args.clone())
        .await?;
    if !body3.to_lowercase().contains("calculator.py") {
        bail!("find_definition (recovery) did not resolve to calculator.py: {body3}");
    }
    let mut recovery_children: Vec<u32> = Vec::new();
    for _ in 0..30 {
        recovery_children = managed.descendants_named("node");
        let recovery_set: HashSet<u32> = recovery_children.iter().copied().collect();
        if !recovery_set.is_empty() && recovery_set.is_disjoint(&first_set) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let recovery_set: HashSet<u32> = recovery_children.iter().copied().collect();
    evidence.set("recovery_node_descendants", json!(recovery_children))?;
    let recovery_confirmed = !recovery_set.is_empty() && recovery_set.is_disjoint(&first_set);
    evidence.set("recovery_confirmed", recovery_confirmed)?;
    if !recovery_confirmed {
        bail!("recovery not confirmed: killed={first_set:?} candidates={recovery_set:?}");
    }

    Ok(())
}
