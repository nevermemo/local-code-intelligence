use crate::language;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{
    collections::HashMap,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::Stdio,
};

#[derive(Debug, Clone)]
pub struct LexicalMatch {
    pub relative_path: String,
    pub line: u32,
}

#[derive(Deserialize)]
struct Event {
    #[serde(rename = "type")]
    kind: String,
    data: Option<EventData>,
}
#[derive(Deserialize)]
struct EventData {
    path: Option<PathValue>,
    line_number: Option<u32>,
}
#[derive(Deserialize)]
struct PathValue {
    text: Option<String>,
}

const STOP_WORDS: &[&str] = &[
    "the",
    "and",
    "into",
    "from",
    "that",
    "this",
    "with",
    "where",
    "code",
    "generated",
    "given",
    "retrieve",
    "relevant",
];

pub fn terms(query: &str) -> Vec<String> {
    let mut counts = HashMap::<String, usize>::new();
    for raw in query.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '#')) {
        let term = raw.trim_matches('#').to_lowercase();
        if term.len() >= 3 && !STOP_WORDS.contains(&term.as_str()) {
            *counts.entry(term).or_default() += 1;
        }
    }
    let mut terms: Vec<_> = counts.into_keys().collect();
    terms.sort_by(|a, b| b.len().cmp(&a.len()).then(a.cmp(b)));
    terms.truncate(12);
    terms
}

/// Resolves the default ripgrep command on Windows, including the copy bundled
/// with common VS Code installations. Explicit custom commands and paths are
/// returned unchanged.
pub fn resolve_ripgrep_path(configured: &str) -> PathBuf {
    resolve_ripgrep_path_from(
        configured,
        std::env::var_os("PATH").as_deref(),
        std::env::var_os("LOCALAPPDATA").as_deref(),
        std::env::var_os("ProgramFiles").as_deref(),
        cfg!(windows),
    )
}

fn resolve_ripgrep_path_from(
    configured: &str,
    path: Option<&OsStr>,
    local_app_data: Option<&OsStr>,
    program_files: Option<&OsStr>,
    windows: bool,
) -> PathBuf {
    let configured_path = PathBuf::from(configured);
    if !matches!(configured.to_ascii_lowercase().as_str(), "rg" | "rg.exe") {
        return configured_path;
    }

    if let Some(found) = find_on_path(path, windows) {
        return found;
    }
    if windows {
        for root in vscode_install_roots(local_app_data, program_files) {
            if let Some(found) = find_vscode_ripgrep(&root) {
                return found;
            }
        }
    }
    configured_path
}

fn find_on_path(path: Option<&OsStr>, windows: bool) -> Option<PathBuf> {
    let names: &[&str] = if windows { &["rg.exe", "rg"] } else { &["rg"] };
    for directory in std::env::split_paths(path?) {
        for name in names {
            let candidate = directory.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

fn vscode_install_roots(
    local_app_data: Option<&OsStr>,
    program_files: Option<&OsStr>,
) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(base) = local_app_data {
        let programs = Path::new(base).join("Programs");
        roots.push(programs.join("Microsoft VS Code"));
        roots.push(programs.join("Microsoft VS Code Insiders"));
        roots.push(programs.join("VSCodium"));
    }
    if let Some(base) = program_files {
        let base = Path::new(base);
        roots.push(base.join("Microsoft VS Code"));
        roots.push(base.join("Microsoft VS Code Insiders"));
        roots.push(base.join("VSCodium"));
    }
    roots
}

fn find_vscode_ripgrep(install_root: &Path) -> Option<PathBuf> {
    let mut app_roots = vec![install_root.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(install_root) {
        let mut children: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        children.sort_by(|a, b| b.cmp(a));
        app_roots.extend(children);
    }

    const RELATIVE_PATHS: &[&str] = &[
        "resources/app/node_modules.asar.unpacked/@vscode/ripgrep/bin/rg.exe",
        "resources/app/node_modules.asar.unpacked/@vscode/ripgrep-universal/bin/win32-x64/rg.exe",
    ];
    app_roots.into_iter().find_map(|root| {
        RELATIVE_PATHS
            .iter()
            .map(|relative| root.join(relative))
            .find(|candidate| candidate.is_file())
    })
}

pub async fn search(rg_path: &str, root: &Path, query: &str) -> Result<Vec<LexicalMatch>> {
    let terms = terms(query);
    ensure!(!terms.is_empty(), "query has no searchable lexical terms");
    let resolved_rg = resolve_ripgrep_path(rg_path);
    let mut command = tokio::process::Command::new(&resolved_rg);
    command.args([
        "--json",
        "--line-number",
        "--ignore-case",
        "--fixed-strings",
    ]);
    for adapter in language::ADAPTERS {
        command.arg("--glob").arg(adapter.glob());
    }
    for term in terms {
        command.arg("-e").arg(term);
    }
    command
        .arg("--")
        .arg(root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = command.output().await.with_context(|| {
        format!(
            "start ripgrep at {}; install rg, add it to PATH, or set ripgrep_path explicitly",
            resolved_rg.display()
        )
    })?;
    ensure!(
        output.status.success() || output.status.code() == Some(1),
        "ripgrep failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut matches = Vec::new();
    for line in output
        .stdout
        .split(|&b| b == b'\n')
        .filter(|l| !l.is_empty())
    {
        let event: Event = serde_json::from_slice(line).context("invalid ripgrep JSON")?;
        if event.kind != "match" {
            continue;
        }
        let Some(data) = event.data else { continue };
        let Some(path) = data.path.and_then(|path| path.text) else {
            continue;
        };
        let reported = Path::new(&path);
        let normalized = if reported.is_absolute() {
            dunce::canonicalize(reported).unwrap_or_else(|_| reported.to_path_buf())
        } else {
            root.join(reported)
        };
        let relative = normalized
            .strip_prefix(root)
            .unwrap_or(&normalized)
            .to_string_lossy()
            .replace('\\', "/");
        if let Some(line) = data.line_number {
            matches.push(LexicalMatch {
                relative_path: relative,
                line,
            });
        }
    }
    Ok(matches)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    #[test]
    fn extracts_useful_terms() {
        assert_eq!(
            terms("lower syn AST expressions into generated Slang compute shader code"),
            vec![
                "expressions",
                "compute",
                "shader",
                "lower",
                "slang",
                "ast",
                "syn"
            ]
        );
    }

    #[test]
    fn preserves_an_explicit_ripgrep_path() {
        let resolved = resolve_ripgrep_path_from(r"D:\tools\custom-rg.exe", None, None, None, true);
        assert_eq!(resolved, PathBuf::from(r"D:\tools\custom-rg.exe"));
    }

    #[test]
    fn resolves_ripgrep_from_path_first() {
        let temp = TempDir::new().unwrap();
        let path_rg = temp.path().join("rg.exe");
        std::fs::write(&path_rg, b"").unwrap();
        let resolved =
            resolve_ripgrep_path_from("rg", Some(temp.path().as_os_str()), None, None, true);
        assert_eq!(resolved, path_rg);
    }

    #[test]
    fn resolves_versioned_vscode_bundled_ripgrep() {
        let temp = TempDir::new().unwrap();
        let bundled = temp
            .path()
            .join("Programs/Microsoft VS Code/645f29cc31/resources/app")
            .join("node_modules.asar.unpacked/@vscode/ripgrep-universal/bin/win32-x64/rg.exe");
        std::fs::create_dir_all(bundled.parent().unwrap()).unwrap();
        std::fs::write(&bundled, b"").unwrap();

        let resolved =
            resolve_ripgrep_path_from("rg.exe", None, Some(temp.path().as_os_str()), None, true);
        assert_eq!(resolved, bundled);
    }

    #[test]
    fn leaves_default_command_when_ripgrep_is_not_found() {
        let temp = TempDir::new().unwrap();
        let resolved = resolve_ripgrep_path_from(
            "rg",
            Some(temp.path().as_os_str()),
            Some(temp.path().as_os_str()),
            None,
            true,
        );
        assert_eq!(resolved, PathBuf::from("rg"));
    }
}
