use crate::language;
use anyhow::{Context, Result, ensure};
use serde::Deserialize;
use std::{collections::HashMap, path::Path, process::Stdio};

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

pub async fn search(rg_path: &str, root: &Path, query: &str) -> Result<Vec<LexicalMatch>> {
    let terms = terms(query);
    ensure!(!terms.is_empty(), "query has no searchable lexical terms");
    let mut command = tokio::process::Command::new(rg_path);
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
    let output = command.output().await.context("start ripgrep")?;
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
}
