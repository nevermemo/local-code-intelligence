//! Ported in spirit from `csharp_recovery.rs`.
//!
//! Acceptance for persistent `serve` process reuse and forced-death recovery
//! of the TypeScript/JavaScript language server child process(es), over the
//! real MCP HTTP endpoint. Builds a real npm-scaffolded TypeScript fixture,
//! starts an owned `serve` process pointed at a real
//! `typescript-language-server`, performs the MCP handshake, and calls
//! `find_definition` (not `search_symbols` -- see `typescript_lsp.rs` for why
//! workspace/symbol is not part of this application's TypeScript navigation
//! contract) twice to prove the same set of `node` descendant processes is
//! reused across requests. It then force-kills every `node` descendant, calls
//! `find_definition` a third time, and confirms an entirely fresh replacement
//! set of `node` descendants appears (no restart loop, no leftover old PIDs).
//!
//! On Windows, a real `typescript-language-server` invoked via its npm
//! `.cmd` shim spawns as
//! `local-code-intelligence.exe -> cmd.exe -> node.exe [-> node.exe]`: the
//! real server process (and tsserver's own child) are grandchildren or
//! deeper, not a direct child, so this uses `ManagedServer::descendants_named`
//! (recursive, any depth) throughout instead of `children_named`
//! (direct-children-only, which would never match here).

use super::{Evidence, Fixture, ManagedServer, McpSession, run_npm, wait_http_ok, which};
use anyhow::{Result, anyhow, bail};
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

const TSCONFIG: &str = r#"{
  "compilerOptions": {
    "target": "es2020",
    "module": "commonjs"
  }
}
"#;

const CALCULATOR_SOURCE: &str = "export class Calculator {\n  add(a: number, b: number): number {\n    return a + b;\n  }\n\n  use(): number {\n    return this.add(2, 3);\n  }\n}\n";

const CALLSITE_SOURCE: &str = "import { Calculator } from './Calculator';\n\nexport function run(value: Calculator): number {\n  return value.add(1, 2);\n}\n";

const BASE: &str = "http://127.0.0.1:8768";

pub async fn run(typescript_language_server: Option<PathBuf>) -> Result<()> {
    let mut evidence = Evidence::new("typescript-lsp-recovery")?;
    evidence.set("base_url", BASE)?;

    evidence.stage("resolve-prerequisites")?;
    let Some(server_path) = resolve_typescript_language_server(typescript_language_server) else {
        evidence.set("status", "prerequisite-unavailable")?;
        println!(
            "PREREQUISITE_UNAVAILABLE: typescript-language-server not found on PATH or via --typescript-language-server. Evidence: {}",
            evidence.path().display()
        );
        return Ok(());
    };
    if which("npm").is_none() {
        bail!("npm is required to scaffold the real TypeScript fixture");
    }

    let fixture = Fixture::create("typescript-recovery")?;
    let mut server: Option<ManagedServer> = None;

    let outcome = checks(&mut evidence, &fixture, &mut server, server_path.as_path()).await;

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
                "PASS: TypeScript LSP persistent-reuse and recovery acceptance. Evidence: {}",
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

/// Resolves the `typescript-language-server` executable to use: the explicit
/// `--typescript-language-server` flag, then PATH. Returns `None` when
/// neither resolves to a real file, which callers treat as a graceful
/// prerequisite-unavailable skip.
fn resolve_typescript_language_server(explicit: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = explicit {
        return Some(path);
    }
    which("typescript-language-server")
}

async fn checks(
    evidence: &mut Evidence,
    fixture: &Fixture,
    server: &mut Option<ManagedServer>,
    server_path: &Path,
) -> Result<()> {
    evidence.stage("create-fixture")?;
    // Written before `npm install`/indexing so both npm and the indexer skip
    // the tens of thousands of files a real `typescript` install places
    // under node_modules/.
    fixture.write(".gitignore", "node_modules/\n")?;
    fixture.write("tsconfig.json", TSCONFIG)?;
    fixture.write("src/Calculator.ts", CALCULATOR_SOURCE)?;
    fixture.write("src/CallSite.ts", CALLSITE_SOURCE)?;

    run_npm(&fixture.dir, &["init", "-y"]).await?;
    // Pinned to a 5.x release: TypeScript 7's native-compiler rewrite has no
    // classic tsserver.js, and typescript-language-server cannot initialize
    // against it.
    run_npm(&fixture.dir, &["install", "typescript@5.7.3"]).await?;

    let fixture_dir_str = fixture.dir.to_string_lossy().to_string();

    evidence.stage("construct-config")?;
    let normalized_server = server_path.to_string_lossy().replace('\\', "/");
    let config_path = fixture.write_config(&[
        "lsp_timeout_seconds = 60".to_string(),
        "[typescript]".to_string(),
        format!("path = '{normalized_server}'"),
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
        "relative_file_path": "src/CallSite.ts",
        "line": 4,
        "character": 15,
    });
    let body1 = session
        .call_tool("find_definition", definition_args.clone())
        .await?;
    let resolved_to_calculator = body1.to_lowercase().contains("calculator.ts");
    evidence.set("first_definition_resolved", resolved_to_calculator)?;
    if !resolved_to_calculator {
        bail!("first find_definition response did not resolve into Calculator.ts: {body1}");
    }

    let managed = server.as_ref().expect("server spawned above");
    let mut first_children: Vec<u32> = Vec::new();
    for _ in 0..40 {
        first_children = managed.descendants_named("node");
        if !first_children.is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    if first_children.is_empty() {
        bail!("no node descendant process appeared under the owned LCI process");
    }
    let first_set: BTreeSet<u32> = first_children.iter().copied().collect();
    evidence.set(
        "first_node_descendants",
        serde_json::to_value(&first_children)?,
    )?;

    evidence.stage("second-navigation-reuse")?;
    let body2 = session
        .call_tool("find_definition", definition_args.clone())
        .await?;
    if !body2.to_lowercase().contains("calculator.ts") {
        bail!("find_definition (reuse) did not resolve into Calculator.ts: {body2}");
    }
    let reused_children = managed.descendants_named("node");
    let reused_set: BTreeSet<u32> = reused_children.iter().copied().collect();
    evidence.set(
        "reused_node_descendants",
        serde_json::to_value(&reused_children)?,
    )?;
    let reuse_confirmed = !reused_set.is_empty() && reused_set == first_set;
    evidence.set("reuse_confirmed", reuse_confirmed)?;
    if !reuse_confirmed {
        bail!("process set was not reused: first={first_set:?} reused={reused_set:?}");
    }

    evidence.stage("forced-death-recovery")?;
    evidence.set(
        "killed_node_descendants",
        serde_json::to_value(&first_children)?,
    )?;
    for pid in &first_children {
        ManagedServer::kill_process(*pid);
    }

    let body3 = session
        .call_tool("find_definition", definition_args.clone())
        .await?;
    if !body3.to_lowercase().contains("calculator.ts") {
        bail!("find_definition (recovery) did not resolve into Calculator.ts: {body3}");
    }
    let mut recovery_children: Vec<u32> = Vec::new();
    let mut recovery_set: BTreeSet<u32> = BTreeSet::new();
    for _ in 0..40 {
        recovery_children = managed.descendants_named("node");
        recovery_set = recovery_children.iter().copied().collect();
        if !recovery_set.is_empty() && recovery_set.is_disjoint(&first_set) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    evidence.set(
        "recovery_node_descendants",
        serde_json::to_value(&recovery_children)?,
    )?;
    let recovery_confirmed = !recovery_set.is_empty() && recovery_set.is_disjoint(&first_set);
    evidence.set("recovery_confirmed", recovery_confirmed)?;
    if !recovery_confirmed {
        bail!("recovery not confirmed: killed={first_set:?} candidates={recovery_set:?}");
    }

    Ok(())
}
