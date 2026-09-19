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
use std::path::{Path, PathBuf};

use super::Evidence;

/// Resolves the csharp-ls executable: the explicit `--csharp-ls` flag if
/// given, else a PATH lookup, else the path named by `$LCI_ACCEPTANCE_CSHARP_LS`
/// if it exists -- `dotnet tool install` puts csharp-ls in a per-user
/// directory that isn't always on `PATH`.
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
    super::env_fallback(super::CSHARP_LS_FALLBACK_ENV)
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
        println!(
            "PREREQUISITE_UNAVAILABLE: csharp-ls not found on PATH, via --csharp-ls, or via ${}",
            super::CSHARP_LS_FALLBACK_ENV
        );
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
