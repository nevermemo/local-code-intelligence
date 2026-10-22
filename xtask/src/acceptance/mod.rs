//! Shared helpers for acceptance subcommands: disposable fixtures, an owned
//! `serve` process with cross-platform child-process inspection (replacing
//! PowerShell's `Get-CimInstance Win32_Process`), an MCP-over-HTTP session,
//! and a JSON evidence report, matching the conventions the original
//! `scripts/*.ps1` acceptance harnesses used.

pub mod c_lsp;
pub mod c_missing;
pub mod c_recovery;
pub mod core;
pub mod cpp_lsp;
pub mod cpp_missing;
pub mod cpp_recovery;
pub mod csharp_lsp;
pub mod csharp_missing;
pub mod csharp_recovery;
pub mod go_lsp;
pub mod go_missing;
pub mod go_recovery;
pub mod java_lsp;
pub mod java_missing;
pub mod java_recovery;
pub mod lsp;
mod lsp_full;
pub mod markdown_lsp;
pub mod markdown_missing;
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

/// Removes every `LCI_*` variable of this process from the environment
/// `command` will start with. The acceptance harness always hands the service
/// its own `--config` and port, so a developer's exported `LCI_DATA_DIR`,
/// `LCI_EMBEDDING_URL`, `LCI_LOCAL_CONFIG` or `LCI_PORT` (any variable the
/// service reads now or later) must not redirect a live run to their data, a
/// different model service or a different overlay. Names are compared
/// case-insensitively because Windows treats them so. The harness's own
/// `LCI_ACCEPTANCE_*` variables are read by this process, never by the
/// service, so they are stripped along with the rest.
pub fn strip_lci_env(command: &mut Command) -> &mut Command {
    strip_lci_env_named(command, std::env::vars_os().map(|(name, _)| name))
}

/// [`strip_lci_env`] over an explicit list of variable names.
fn strip_lci_env_named(
    command: &mut Command,
    names: impl IntoIterator<Item = std::ffi::OsString>,
) -> &mut Command {
    for name in names {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("LCI_")
        {
            command.env_remove(name);
        }
    }
    command
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

/// Environment variable naming a jdtls *installation directory* to fall back
/// to when `jdtls` is not discoverable on `PATH` and no `--java` flag was
/// given. Deliberately an environment variable rather than a literal path:
/// where a developer extracted jdtls is a property of their machine, not of
/// this repository.
pub const JDTLS_FALLBACK_ENV: &str = "LCI_ACCEPTANCE_JDTLS";

/// Environment variable naming a `csharp-ls` executable to fall back to when
/// it is not on `PATH` and no `--csharp-ls` flag was given -- `dotnet tool
/// install` puts it in a per-user directory that is not always on `PATH`.
pub const CSHARP_LS_FALLBACK_ENV: &str = "LCI_ACCEPTANCE_CSHARP_LS";

/// Resolves an optional machine-local fallback path from `var`. Returns
/// `None` when the variable is unset, empty, or names a path that no longer
/// exists, so a stale value degrades to the normal PREREQUISITE_UNAVAILABLE
/// skip rather than a confusing spawn failure.
pub fn env_fallback(var: &str) -> Option<PathBuf> {
    let value = std::env::var_os(var)?;
    if value.is_empty() {
        return None;
    }
    let path = PathBuf::from(value);
    path.exists().then_some(path)
}

/// Which workspace an external-workspace acceptance run targets.
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
pub enum AcceptanceProfile {
    /// This repository itself -- the default, so `cargo xtask acceptance
    /// core`/`lsp` work on any clone with nothing installed beyond the
    /// embedding/reranking services the acceptance bar already requires.
    #[value(name = "self")]
    SelfRepo,
    /// The GUST Rust-to-Slang translator repository: an optional, external
    /// example, exactly the treatment `evaluations/core.toml` already gives
    /// its optional `gust` workspace. Has no default path -- this repository
    /// must not carry one that exists on a single machine.
    Gust,
}

/// Environment variable naming a GUST checkout, so a developer who runs the
/// optional profile regularly can set it once instead of passing
/// `--workspace` every time. Read only for `AcceptanceProfile::Gust`, so it
/// can never silently redirect a default `self` run.
pub const GUST_WORKSPACE_ENV: &str = "LCI_ACCEPTANCE_GUST_WORKSPACE";

/// Resolves the workspace for `profile`: the explicit `--workspace` flag,
/// else (for `gust`) `$LCI_ACCEPTANCE_GUST_WORKSPACE`, else (for `self`)
/// this repository.
pub fn resolve_acceptance_workspace(
    profile: AcceptanceProfile,
    explicit: Option<PathBuf>,
    command: &str,
) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    match profile {
        AcceptanceProfile::SelfRepo => Ok(workspace_root()),
        AcceptanceProfile::Gust => env_fallback(GUST_WORKSPACE_ENV).ok_or_else(|| {
            anyhow::anyhow!(
                "`cargo xtask acceptance {command} --profile gust` needs an external \
                 workspace: pass --workspace <path to a gpu-dialect-v0 checkout>, or set \
                 ${GUST_WORKSPACE_ENV}. Example:\n  \
                 cargo xtask acceptance {command} --profile gust --workspace /path/to/gpu-dialect-v0"
            )
        }),
    }
}

/// Environment variable naming the directory that holds a run's fixtures. The
/// orchestrator (`live_acceptance`, the live-acceptance workflow) sets it to a
/// run-unique directory and removes only that one at the end, so a cleanup
/// never touches another run's fixtures or any other `lci-*` directory. Unset
/// or empty, fixtures go straight under the OS temp dir.
pub const FIXTURE_ROOT_ENV: &str = "LCI_ACCEPTANCE_FIXTURE_ROOT";

/// Where [`Fixture::create`] puts its directories.
fn fixture_root() -> PathBuf {
    match std::env::var_os(FIXTURE_ROOT_ENV) {
        Some(root) if !root.is_empty() => PathBuf::from(root),
        _ => std::env::temp_dir(),
    }
}

/// A disposable acceptance fixture: an isolated workspace directory and an
/// isolated LanceDB data directory, both under [`fixture_root`], named with
/// this process's PID so concurrent runs never collide.
pub struct Fixture {
    pub dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Fixture {
    pub fn create(name: &str) -> Result<Self> {
        let pid = std::process::id();
        let root = fixture_root();
        let dir = root.join(format!("lci-{name}-{pid}"));
        let data_dir = root.join(format!("lci-{name}-data-{pid}"));
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

/// Port every acceptance-owned `serve` process binds to. Deliberately
/// distinct from the default 8768 a developer's own persistent `serve`
/// instance typically uses on the same machine (e.g. a self-hosted
/// live-acceptance runner) -- reusing 8768 here let a stale instance silently
/// swallow the acceptance harness's requests instead of the fixture server
/// actually failing to bind, producing confusing false failures.
pub const PORT: u16 = 8769;

/// Refuses a port something already listens on. The probe binds instead of
/// connecting: on Windows a connection to a closed loopback port is retried for
/// about a second before it fails, so a short connect timeout reports "timed
/// out" for a free port.
fn ensure_port_free(port: u16) -> Result<()> {
    match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            bail!("acceptance server port {port} is already in use")
        }
        Err(error) => Err(error).with_context(|| format!("check acceptance server port {port}")),
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
        ensure_port_free(PORT)?;
        std::fs::create_dir_all(log_dir)?;
        let stdout = std::fs::File::create(log_dir.join("serve.stdout.log"))?;
        let stderr = std::fs::File::create(log_dir.join("serve.stderr.log"))?;
        let mut command = Command::new(lci_binary());
        strip_lci_env(&mut command);
        let child = command
            .arg("--config")
            .arg(config_path)
            .arg("serve")
            .arg("--port")
            .arg(PORT.to_string())
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
    pub(crate) fn descendants_of(system: &System, root: u32) -> Vec<u32> {
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

/// Polls `GET url` until it returns a 2xx status, the owned child exits, or
/// `attempts` is exhausted.
pub async fn wait_http_ok(
    url: &str,
    attempts: u32,
    delay: Duration,
    mut exited: impl FnMut() -> bool,
) -> bool {
    let client = reqwest::Client::new();
    for _ in 0..attempts {
        if exited() {
            return false;
        }
        let response = client.get(url).timeout(Duration::from_secs(2)).send().await;
        if exited() {
            return false;
        }
        if let Ok(response) = response
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
    /// The `serverInfo` of the `initialize` result; `Null` after [`connect`],
    /// read by [`connect_with`].
    server_info: Value,
}

/// The JSON-RPC envelopes of an MCP HTTP response body: either one JSON
/// document or an event stream whose `data:` lines hold one envelope each.
pub fn mcp_envelopes(body: &str) -> Vec<Value> {
    if let Ok(value) = serde_json::from_str::<Value>(body.trim()) {
        return vec![value];
    }
    body.lines()
        .filter_map(|line| line.strip_prefix("data:"))
        .filter_map(|payload| serde_json::from_str::<Value>(payload.trim()).ok())
        .collect()
}

impl McpSession {
    pub async fn connect(base: &str) -> Result<Self> {
        Self::connect_inner(base, None, false).await
    }

    /// Like [`connect`], with a per-request `timeout` on the client and the
    /// server's `serverInfo` read from the `initialize` response.
    pub async fn connect_with(base: &str, timeout: Duration) -> Result<Self> {
        Self::connect_inner(base, Some(timeout), true).await
    }

    async fn connect_inner(base: &str, timeout: Option<Duration>, read_info: bool) -> Result<Self> {
        let mut builder = reqwest::Client::builder();
        if let Some(timeout) = timeout {
            builder = builder.timeout(timeout);
        }
        let client = builder.build().context("build the mcp http client")?;
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
        let server_info = if read_info {
            let body = response.text().await.context("read the initialize body")?;
            mcp_envelopes(&body)
                .iter()
                .find_map(|e| e.pointer("/result/serverInfo").cloned())
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let session = Self {
            client,
            base: base.to_string(),
            session_id,
            next_id: 2,
            server_info,
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

    /// The `serverInfo` the server answered `initialize` with (`Null` for a
    /// session made by [`McpSession::connect`]).
    pub fn server_info(&self) -> &Value {
        &self.server_info
    }

    /// The names of the tools the server offers (`tools/list`, first page).
    pub async fn tools_list(&mut self) -> Result<Vec<String>> {
        let id = self.next_id;
        self.next_id += 1;
        let response = self
            .client
            .post(format!("{}/mcp", self.base))
            .header("Accept", "application/json, text/event-stream")
            .header("mcp-session-id", &self.session_id)
            .json(&json!({"jsonrpc": "2.0", "id": id, "method": "tools/list"}))
            .send()
            .await
            .context("mcp tools/list")?;
        let body = response.text().await?;
        let tools = mcp_envelopes(&body)
            .iter()
            .find_map(|e| e.pointer("/result/tools").cloned())
            .with_context(|| format!("tools/list answered no tools: {body}"))?;
        Ok(tools
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|t| t["name"].as_str().map(str::to_owned))
            .collect())
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
    let mut command = Command::new(lci_binary());
    strip_lci_env(&mut command);
    let output = command
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn free_port_check_rejects_listener() {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let error = ensure_port_free(port).expect_err("an occupied loopback port must be rejected");
        assert_eq!(
            error.to_string(),
            format!("acceptance server port {port} is already in use")
        );
    }

    #[test]
    fn free_port_check_accepts_unoccupied_port() {
        let deadline = std::time::Instant::now() + Duration::from_millis(500);
        loop {
            let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            // Another test may claim the released port, so retry with a fresh one.
            match ensure_port_free(port) {
                Ok(()) => return,
                Err(error) => assert!(
                    std::time::Instant::now() < deadline,
                    "no unoccupied loopback port passed the check within 500 ms: {error}"
                ),
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[tokio::test]
    async fn readiness_wait_stops_without_polling_when_child_exited() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .unwrap();
        let address = listener.local_addr().unwrap();
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let received = requests.clone();
        let fake_server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            received.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .await
                .unwrap();
        });

        let result = tokio::time::timeout(
            Duration::from_millis(250),
            wait_http_ok(
                &format!("http://{address}/health"),
                30,
                Duration::from_secs(1),
                || true,
            ),
        )
        .await;
        fake_server.abort();
        assert!(
            !result.expect("an exited child must end the wait immediately"),
            "readiness must fail when the owned child has exited"
        );
        assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn readiness_wait_accepts_success_only_while_child_is_alive() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        for exits_during_request in [false, true] {
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
                .await
                .unwrap();
            let address = listener.local_addr().unwrap();
            let fake_server = tokio::spawn(async move {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut request = [0; 1024];
                assert!(stream.read(&mut request).await.unwrap() > 0);
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                    .await
                    .unwrap();
            });
            let mut checks = 0;
            let ready = wait_http_ok(
                &format!("http://{address}/health"),
                1,
                Duration::ZERO,
                || {
                    checks += 1;
                    exits_during_request && checks > 1
                },
            )
            .await;
            fake_server.await.unwrap();
            assert_eq!(ready, !exits_during_request);
        }
    }

    fn removed(command: &Command) -> Vec<String> {
        let mut removed: Vec<String> = command
            .as_std()
            .get_envs()
            .map(|(name, value)| {
                assert!(value.is_none(), "{name:?} is set, not removed");
                name.to_string_lossy().into_owned()
            })
            .collect();
        removed.sort();
        removed
    }

    #[test]
    fn the_service_never_inherits_an_lci_variable() {
        let mut command = Command::new("not-run");
        strip_lci_env_named(
            &mut command,
            [
                "LCI_DATA_DIR",
                "lci_local_config",
                "LCI_EMBEDDING_URL",
                "LCI_ACCEPTANCE_JDTLS",
                "LCI_SOMETHING_NEW",
                "PATH",
                "LCIX",
                "HOME",
            ]
            .map(OsString::from),
        );
        assert_eq!(
            removed(&command),
            [
                "LCI_ACCEPTANCE_JDTLS",
                "LCI_DATA_DIR",
                "LCI_EMBEDDING_URL",
                "LCI_SOMETHING_NEW",
                "lci_local_config"
            ]
        );

        // Applied to the real environment it covers whatever the shell exports.
        let mut command = Command::new("not-run");
        strip_lci_env(&mut command);
        let removed = removed(&command);
        for (name, _) in std::env::vars_os() {
            let name = name.to_string_lossy().into_owned();
            if name.to_ascii_uppercase().starts_with("LCI_") {
                assert!(removed.contains(&name), "{name} would reach the service");
            }
        }
    }
}
