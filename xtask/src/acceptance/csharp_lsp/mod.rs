//! Ported from `scripts/Acceptance-CSharp-Lsp.ps1` and `scripts/CSharpLspProbe.ps1`.
//!
//! `full_flow` drives the compiled `local-code-intelligence` CLI against a
//! real `dotnet`-built fixture through csharp-ls (index/symbols/definition/
//! references/search). `probe` is a fully standalone JSON-RPC client that
//! speaks LSP directly to csharp-ls (no dependency on the LCI binary at all),
//! proving initialize -> initialized -> workspace/symbol -> shutdown -> exit.

mod full_flow;
mod probe;

use anyhow::Result;
use serde_json::Value;
use std::path::{Path, PathBuf};

use super::Evidence;

/// Resolves the csharp-ls executable: the explicit `--csharp-ls` flag if
/// given, else a PATH lookup, else (Windows only) the hardcoded default the
/// original PowerShell probe script fell back to.
fn resolve_csharp_ls(csharp_ls: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = csharp_ls {
        if path.is_file() {
            return Some(path);
        }
        // Not a literal file: treat it as a bare command name and look it up.
        if let Some(name) = path.to_str()
            && let Some(found) = super::which(name)
        {
            return Some(found);
        }
        return None;
    }
    if let Some(found) = super::which("csharp-ls") {
        return Some(found);
    }
    if cfg!(windows) {
        let fallback = PathBuf::from(r"C:\Users\micro\.dotnet\tools\csharp-ls.exe");
        if fallback.is_file() {
            return Some(fallback);
        }
    }
    None
}

pub async fn run(csharp_ls: Option<PathBuf>, probe_only: bool) -> Result<()> {
    let Some(resolved) = resolve_csharp_ls(csharp_ls) else {
        // Matches the .ps1 scripts' PREREQUISITE_UNAVAILABLE / exit-2 skip
        // convention: this is not a failure, just an unmet prerequisite.
        let name = if probe_only {
            "csharp-lsp-standalone-probe"
        } else {
            "csharp-lsp-acceptance"
        };
        if let Ok(mut evidence) = Evidence::new(name) {
            let _ = evidence.set("status", "prerequisite-unavailable");
        }
        println!("PREREQUISITE_UNAVAILABLE: csharp-ls not found");
        return Ok(());
    };
    if probe_only {
        probe::run(&resolved).await
    } else {
        full_flow::run(&resolved).await
    }
}

/// Runs `exe --version` (or similar) best-effort, returning the trimmed
/// combined stdout/stderr, or an empty string if the command could not run.
pub(crate) async fn command_version(exe: &Path, arg: &str) -> String {
    match tokio::process::Command::new(exe).arg(arg).output().await {
        Ok(output) => {
            let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
            text.push_str(&String::from_utf8_lossy(&output.stderr));
            text.trim().to_string()
        }
        Err(_) => String::new(),
    }
}

/// Bounds `text` to at most `max` characters, matching the .ps1 scripts'
/// bounded-evidence convention (never dump unbounded process output).
pub(crate) fn bound_text(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        trimmed.chars().take(max).collect()
    }
}

/// True if `relative_path` has a `bin/` or `obj/` path segment, matching the
/// .ps1 scripts' `(^|/)(bin|obj)/` regex check for leaked build output.
pub(crate) fn is_build_output(relative_path: &str) -> bool {
    relative_path
        .split('/')
        .any(|segment| segment == "bin" || segment == "obj")
}

/// Extracts the `results` array from a `local-code-intelligence` CLI JSON
/// report (symbols/definition/references/search all share this shape).
pub(crate) fn results(value: &Value) -> Vec<Value> {
    value
        .get("results")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}
