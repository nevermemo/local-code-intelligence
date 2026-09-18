//! Shared helpers for acceptance subcommands: disposable fixtures, an owned
//! `serve` process with cross-platform child-process inspection (replacing
//! PowerShell's `Get-CimInstance Win32_Process`), an MCP-over-HTTP session,
//! and a JSON evidence report, matching the conventions the original
//! `scripts/*.ps1` acceptance harnesses used.

pub mod core;
pub mod csharp_lsp;
pub mod csharp_missing;
pub mod csharp_recovery;
pub mod lsp;
mod lsp_full;
mod missing;
pub mod multilingual;
pub mod python_lsp;
pub mod python_missing;
pub mod python_recovery;
mod recovery;
pub mod typescript_lsp;
pub mod typescript_missing;
pub mod typescript_recovery;

use anyhow::{Context, Result, bail};
use serde_json::{Map, Value, json};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use sysinfo::{Pid, System};
use tokio::process::{Child, Command};
use tokio::time::sleep;

pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask is a workspace member with a parent root")
        .to_path_buf()
}

pub fn lci_binary() -> PathBuf {
    workspace_root().join("target/debug").join(format!(
        "local-code-intelligence{}",
        std::env::consts::EXE_SUFFIX
    ))
}

pub fn test_results_dir() -> Result<PathBuf> {
    let dir = workspace_root().join("test-results");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Cross-platform PATH lookup, trying `name` and (on Windows) `name.exe`
/// and `name.cmd` — npm and npm-installed shims (typescript-language-server,
/// pyright-langserver) are batch-file wrappers on Windows, not native `.exe`.
pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    // `.exe`/`.cmd` are tried before the bare name on Windows: several
    // npm-ecosystem installs (confirmed with nvm-for-windows' `npm`) ship a
    // bare, extensionless file that is a POSIX shell script, not a valid
    // Win32 executable, alongside the real `name.cmd` shim in the same PATH
    // directory. Preferring the bare name first would silently resolve to
    // that broken script instead of the working one.
    let candidates: Vec<String> = if cfg!(windows) && !name.eq_ignore_ascii_case("") {
        vec![
            format!("{name}.exe"),
            format!("{name}.cmd"),
            name.to_string(),
        ]
    } else {
        vec![name.to_string()]
    };
    std::env::split_paths(&path).find_map(|dir| {
        candidates
            .iter()
            .map(|candidate| dir.join(candidate))
            .find(|p| p.is_file())
    })
}

/// A disposable acceptance fixture: an isolated workspace directory and an
/// isolated LanceDB data directory, both under the OS temp dir, named with
/// this process's PID so concurrent runs never collide.
pub struct Fixture {
    pub dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Fixture {
    pub fn create(name: &str) -> Result<Self> {
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("lci-{name}-{pid}"));
        let data_dir = std::env::temp_dir().join(format!("lci-{name}-data-{pid}"));
        std::fs::create_dir_all(&dir)
            .with_context(|| format!("create fixture dir {}", dir.display()))?;
        Ok(Self { dir, data_dir })
    }

    pub fn write(&self, relative: &str, content: &str) -> Result<PathBuf> {
        let path = self.dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)
            .with_context(|| format!("write fixture file {}", path.display()))?;
        Ok(path)
    }

    /// Writes config.toml with a forward-slash-normalized data_dir plus any
    /// extra TOML lines the caller supplies (rust_analyzer_path overrides,
    /// `[csharp]` sections, etc.), matching the .ps1 scripts' config authoring.
    pub fn write_config(&self, extra_lines: &[String]) -> Result<PathBuf> {
        let normalized = self.data_dir.to_string_lossy().replace('\\', "/");
        let mut lines = vec![format!("data_dir = '{normalized}'")];
        lines.extend(extra_lines.iter().cloned());
        self.write("config.toml", &(lines.join("\n") + "\n"))
    }

    /// Removes both fixture directories. Callers should check `removed()`
    /// afterward and fail the acceptance run if cleanup did not fully land,
    /// matching the .ps1 scripts' verified-cleanup convention.
    pub fn cleanup(&self) {
        let _ = std::fs::remove_dir_all(&self.dir);
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }

    pub fn removed(&self) -> bool {
        !self.dir.exists() && !self.data_dir.exists()
    }
}

/// A `local-code-intelligence serve` process this acceptance run owns, plus
/// cross-platform helpers to inspect and kill its child language-server
/// processes.
pub struct ManagedServer {
    child: Child,
    pub pid: u32,
}

impl ManagedServer {
    pub async fn spawn(config_path: &Path, working_dir: &Path, log_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(log_dir)?;
        let stdout = std::fs::File::create(log_dir.join("serve.stdout.log"))?;
        let stderr = std::fs::File::create(log_dir.join("serve.stderr.log"))?;
        let child = Command::new(lci_binary())
            .arg("--config")
            .arg(config_path)
            .arg("serve")
            .current_dir(working_dir)
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .kill_on_drop(true)
            .spawn()
            .context("start local-code-intelligence serve")?;
        let pid = child.id().context("owned server process has no pid")?;
        Ok(Self { child, pid })
    }

    /// Direct children of this server matching `process_name` (e.g.
    /// "csharp-ls"), compared case-insensitively and without a platform
    /// executable suffix. Only useful for a server that spawns as a native
    /// executable directly; see [`Self::descendants_named`] for npm-style
    /// tools that spawn through an intermediate shell wrapper on Windows.
    pub fn children_named(&self, process_name: &str) -> Vec<u32> {
        let mut system = System::new_all();
        system.refresh_all();
        system
            .processes()
            .values()
            .filter(|process| {
                process.parent().map(|parent| parent.as_u32()) == Some(self.pid)
                    && process
                        .name()
                        .to_string_lossy()
                        .trim_end_matches(".exe")
                        .eq_ignore_ascii_case(process_name)
            })
            .map(|process| process.pid().as_u32())
            .collect()
    }

    /// Every descendant of this server at any depth matching `process_name`
    /// (case-insensitive, without a platform executable suffix). On Windows,
    /// an npm-installed tool invoked via its `.cmd` shim spawns as
    /// `local-code-intelligence.exe -> cmd.exe -> node.exe [-> node.exe...]`
    /// — the real server process is a grandchild or deeper, not a direct
    /// child — so `children_named` alone will not find it. Prefer this for
    /// typescript-language-server/pyright; `children_named` remains correct
    /// and sufficient for csharp-ls, a native executable spawned directly.
    pub fn descendants_named(&self, process_name: &str) -> Vec<u32> {
        let mut system = System::new_all();
        system.refresh_all();
        Self::descendants_of(&system, self.pid)
            .into_iter()
            .filter(|pid| {
                system
                    .process(Pid::from_u32(*pid))
                    .map(|process| {
                        process
                            .name()
                            .to_string_lossy()
                            .trim_end_matches(".exe")
                            .eq_ignore_ascii_case(process_name)
                    })
                    .unwrap_or(false)
            })
            .collect()
    }

    /// BFS over the process table for every descendant PID of `root`, at any
    /// depth, using one already-refreshed `System` snapshot.
    fn descendants_of(system: &System, root: u32) -> Vec<u32> {
        let mut frontier = vec![root];
        let mut found = Vec::new();
        let mut seen = std::collections::HashSet::new();
        while let Some(parent) = frontier.pop() {
            for process in system.processes().values() {
                if process.parent().map(|p| p.as_u32()) != Some(parent) {
                    continue;
                }
                let pid = process.pid().as_u32();
                if seen.insert(pid) {
                    found.push(pid);
                    frontier.push(pid);
                }
            }
        }
        found
    }

    pub fn kill_process(pid: u32) {
        let mut system = System::new_all();
        system.refresh_all();
        if let Some(process) = system.process(Pid::from_u32(pid)) {
            process.kill();
        }
    }

    /// Kills every descendant of the owned server at any depth, then the
    /// server itself, mirroring the .ps1 cleanup order (children first, then
    /// parent) but extended to the full tree — a shallow direct-children-only
    /// kill can leave an npm-style tool's real process (a grandchild via an
    /// intermediate `cmd.exe` on Windows) orphaned and running.
    pub async fn kill_tree(&mut self) {
        let mut system = System::new_all();
        system.refresh_all();
        let descendants = Self::descendants_of(&system, self.pid);
        for pid in descendants {
            Self::kill_process(pid);
        }
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.child.wait()).await;
    }

    pub fn has_exited(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

/// Polls `GET url` until it returns a 2xx status or `attempts` is exhausted.
pub async fn wait_http_ok(url: &str, attempts: u32, delay: Duration) -> bool {
    let client = reqwest::Client::new();
    for _ in 0..attempts {
        if let Ok(response) = client.get(url).timeout(Duration::from_secs(2)).send().await
            && response.status().is_success()
        {
            return true;
        }
        sleep(delay).await;
    }
    false
}

/// A live MCP JSON-RPC-over-HTTP session against an owned server: performs
/// the initialize/initialized handshake once, then exposes `call_tool`.
pub struct McpSession {
    client: reqwest::Client,
    base: String,
    session_id: String,
    next_id: i64,
}

impl McpSession {
    pub async fn connect(base: &str) -> Result<Self> {
        let client = reqwest::Client::new();
        let response = client
            .post(format!("{base}/mcp"))
            .header("Accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-03-26",
                    "capabilities": {},
                    "clientInfo": {"name": "xtask-acceptance", "version": "1"}
                }
            }))
            .send()
            .await
            .context("mcp initialize request")?;
        let session_id = response
            .headers()
            .get("mcp-session-id")
            .context("initialize response missing mcp-session-id")?
            .to_str()?
            .to_string();
        let session = Self {
            client,
            base: base.to_string(),
            session_id,
            next_id: 2,
        };
        session
            .client
            .post(format!("{base}/mcp"))
            .header("Accept", "application/json, text/event-stream")
            .header("mcp-session-id", &session.session_id)
            .json(&json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .send()
            .await
            .context("mcp initialized notification")?;
        Ok(session)
    }

    /// Calls an MCP tool and returns its raw JSON-RPC response body as text.
    /// Errors (with the raw body attached) on `"isError":true`.
    pub async fn call_tool(&mut self, name: &str, arguments: Value) -> Result<String> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("mcp-session-id", &self.session_id)
            .json(&json!({
                "jsonrpc": "2.0",
                "id": id,
                "method": "tools/call",
                "params": {"name": name, "arguments": arguments}
            }))
            .send()
            .await
            .with_context(|| format!("mcp tools/call {name}"))?;
        let body = response.text().await?;
        if body.contains("\"isError\":true") {
            bail!("{name} returned an MCP tool error: {body}");
        }
        Ok(body)
    }
}

/// A JSON evidence report saved to `test-results/<name>.json` after every
/// stage transition, matching the .ps1 scripts' `Save-Evidence` convention
/// so a run's progress is inspectable even if it's later killed mid-stage.
pub struct Evidence {
    map: Map<String, Value>,
    path: PathBuf,
}

impl Evidence {
    pub fn new(name: &str) -> Result<Self> {
        let path = test_results_dir()?.join(format!("{name}.json"));
        let mut map = Map::new();
        map.insert("status".into(), json!("running"));
        map.insert("stage".into(), json!("start"));
        let evidence = Self { map, path };
        evidence.save()?;
        Ok(evidence)
    }

    pub fn set(&mut self, key: &str, value: impl Into<Value>) -> Result<()> {
        self.map.insert(key.to_string(), value.into());
        self.save()
    }

    pub fn stage(&mut self, name: &str) -> Result<()> {
        self.set("stage", json!(name))
    }

    pub fn fail(&mut self, error: impl std::fmt::Display) -> Result<()> {
        let stage = self.map.get("stage").cloned().unwrap_or(Value::Null);
        self.map.insert("status".into(), json!("failed"));
        self.map.insert("failed_stage".into(), stage);
        self.map.insert("error".into(), json!(error.to_string()));
        self.save()
    }

    pub fn pass(&mut self) -> Result<()> {
        self.set("status", json!("passed"))
    }

    pub fn save(&self) -> Result<()> {
        let text = serde_json::to_string_pretty(&self.map)?;
        std::fs::write(&self.path, text)
            .with_context(|| format!("write evidence file {}", self.path.display()))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// Runs `dotnet <args>` in `dir`, bailing with combined stdout/stderr on a
/// nonzero exit — the shared pattern every real-C#-fixture acceptance uses.
pub async fn run_dotnet(dir: &Path, args: &[&str]) -> Result<()> {
    let dotnet = which("dotnet").context("dotnet SDK not found on PATH")?;
    let output = Command::new(dotnet)
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .with_context(|| format!("run dotnet {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "dotnet {} failed ({}): {}{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// The result of running the compiled `local-code-intelligence` CLI.
/// stdout and stderr are kept separate because `main.rs` sends tracing logs
/// to stderr specifically so stdout carries only the CLI's single JSON
/// report — callers can parse `stdout` directly instead of stripping logs.
pub struct LciOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

impl LciOutput {
    /// stdout and stderr concatenated, for error/diagnostic messages.
    pub fn combined(&self) -> String {
        format!("{}{}", self.stdout, self.stderr)
    }

    /// Parses stdout as the CLI's JSON report.
    pub fn json(&self) -> Result<Value> {
        serde_json::from_str(self.stdout.trim())
            .with_context(|| format!("invalid JSON on stdout: {}", self.stdout))
    }
}

/// Runs `npm <args>` in `dir`, bailing with combined stdout/stderr on a
/// nonzero exit — the shared pattern for scaffolding real TypeScript/Python
/// fixtures (`npm init`, `npm install typescript`, etc.).
pub async fn run_npm(dir: &Path, args: &[&str]) -> Result<()> {
    let npm = which("npm").context("npm not found on PATH")?;
    let output = Command::new(npm)
        .args(args)
        .current_dir(dir)
        .output()
        .await
        .with_context(|| format!("run npm {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "npm {} failed ({}): {}{}",
            args.join(" "),
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(())
}

/// Runs the compiled `local-code-intelligence` CLI (not the MCP server) with
/// the given config and arguments.
pub async fn run_lci(config_path: &Path, args: &[&str]) -> Result<LciOutput> {
    let output = Command::new(lci_binary())
        .arg("--config")
        .arg(config_path)
        .args(args)
        .output()
        .await
        .with_context(|| format!("run local-code-intelligence {}", args.join(" ")))?;
    Ok(LciOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}
