//! Language-server-backed symbol search, definition, and reference lookup.

use super::App;
use crate::{
    language,
    lsp::{self, adapter::LspAdapter},
    store::Store,
    workspace::Workspace,
};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use std::{collections::HashMap, path::Path, sync::Arc};

#[derive(Debug, Serialize)]
pub struct NavigationReport {
    pub workspace: Workspace,
    pub results: Vec<lsp::Location>,
    /// Why `results` may be shorter than a clean "no match" would be, so an
    /// empty answer is not read as one. Two kinds of entry, the first kind
    /// first and each kind in order: `No language server is enabled for
    /// <language> (<providers>): its symbols were not searched` for an indexed
    /// language none of whose servers is switched off or lacks a path, and
    /// `Optional LSP provider unavailable: <provider>: <cause>` for each
    /// language server that failed to answer while the other servers' results
    /// were returned. Absent from the JSON when empty. Only `symbols` can set
    /// it: `definition` and `references` ask one server and report its failure
    /// as an error.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// What the tree barrier did for the servers this call asked, which is
    /// information about the answer, not a failure (see [`barrier_notes`]):
    /// `lsp: <provider> restarted: <reason>` for a kept server the barrier
    /// stopped (the answer is from a fresh one; the reason is `the tree changed
    /// since its last call`, `the index has no file list to stamp` or `the tree
    /// stamp failed (<cause>)`, `no tree stamp was recorded for it`), then
    /// (the search report only)
    /// `lsp: <provider> kept session; <reason> (kept: it watches its workspace)`
    /// for a server that watches its workspace and was kept through a changed
    /// tree, then
    /// `lsp: <provider> kept session; tree stamp unchanged since <n> s ago`
    /// (or `; no tree stamp was recorded for it`) for a server this call found
    /// running (exact for what the stamp covers). A server this call started
    /// adds nothing. Absent from the JSON when empty.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

/// The notes of a call that passed the tree barrier
/// ([`lsp::Manager::tree_barrier`]) and made its requests inside a
/// [`lsp::WalkSessions`] scope: one `restarted` entry per provider whose kept
/// session the barrier stopped, one `kept session ... (kept: it watches its
/// workspace)` entry per watching server the search channel kept through a
/// changed stamp (`kept`, always empty for navigation), then one `kept session`
/// entry per provider whose answers came from a session the call found running.
/// The wording is the traversal's `lsp.live` notes with `lsp:` as the prefix.
pub(crate) fn barrier_notes(
    restarted: &[lsp::Restart],
    kept: &[lsp::Kept],
    reused: &[(&'static str, Option<std::time::Duration>)],
) -> Vec<String> {
    let restarted = restarted.iter().map(|restart| restart.note("lsp"));
    let kept = kept.iter().map(|kept| kept.note("lsp"));
    let reused = reused.iter().map(|(provider, unchanged)| match unchanged {
        Some(unchanged) => format!(
            "lsp: {provider} kept session; tree stamp unchanged since {} s ago",
            unchanged.as_secs()
        ),
        None => format!("lsp: {provider} kept session; no tree stamp was recorded for it"),
    });
    restarted.chain(kept).chain(reused).collect()
}

/// What [`App::release_language_servers`] stopped.
#[derive(Debug, Serialize)]
pub struct ReleaseReport {
    pub workspace: Workspace,
    /// The language servers (provider names such as `csharp-ls`, sorted) that
    /// were running for this workspace and have been shut down. Empty when
    /// the workspace had none, which is not an error.
    pub released: Vec<String>,
}

impl App {
    /// Shuts down every language server this service runs for the workspace
    /// (the protocol's `shutdown` and `exit`, bounded to about four seconds
    /// per server, then a kill) and forgets its session; the next navigation
    /// or search call starts a fresh server. Needs no index: a workspace that
    /// was never indexed, or has no server running, reports an empty list.
    /// The path is resolved like every other workspace call, so a directory
    /// that does not exist is an error.
    pub async fn release_language_servers(&self, path: &Path) -> Result<ReleaseReport> {
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        // A pending warm start would create a session after the release.
        self.stop_warm_starts(Some(&workspace.id)).await;
        let released = self
            .analyzer
            .release(&workspace.id)
            .await
            .into_iter()
            .map(str::to_owned)
            .collect();
        Ok(ReleaseReport {
            workspace,
            released,
        })
    }

    /// Shuts down the language servers of every workspace, for service exit.
    /// Returns how many servers were stopped. Bounded by about four seconds
    /// however many servers there are, because they shut down side by side.
    pub async fn shutdown_language_servers(&self) -> usize {
        self.stop_warm_starts(None).await;
        self.analyzer.shutdown_all().await
    }

    pub async fn symbols(&self, path: &Path, query: &str) -> Result<NavigationReport> {
        ensure!(!query.trim().is_empty(), "query must not be empty");
        ensure!(query.len() <= 16000, "query exceeds 16000 bytes");
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let snapshot = self
            .store
            .snapshot(&workspace)
            .await?
            .context("workspace is not indexed; call index_workspace first")?;
        let indexed_languages = Store::languages(&snapshot).await?;
        let mut applicable: HashMap<&'static str, Arc<dyn LspAdapter>> = HashMap::new();
        // A language none of whose servers is enabled is not searched, and
        // says so: its absence from `results` is not a clean "no match".
        let mut not_searched: Vec<String> = Vec::new();
        for language in indexed_languages {
            let adapters = self.analyzer.adapters_for_language(&language);
            let mut any_enabled = false;
            for adapter in adapters.iter().copied().filter(|adapter| adapter.enabled()) {
                any_enabled = true;
                applicable
                    .entry(adapter.provider())
                    .or_insert_with(|| Arc::clone(adapter));
            }
            if !any_enabled {
                let providers: Vec<&str> =
                    adapters.iter().map(|adapter| adapter.provider()).collect();
                let providers = if providers.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", providers.join(", "))
                };
                not_searched.push(format!(
                    "No language server is enabled for {language}{providers}: its symbols were not searched"
                ));
            }
        }
        not_searched.sort();
        let mut results = Vec::new();
        let mut failures: Vec<(&'static str, String)> = Vec::new();
        // A kept session passes the tree barrier before any request, once per
        // call (see `Manager::tree_barrier`); the requests run in one scope
        // that records which sessions the call found running.
        let applicable_refs: Vec<&Arc<dyn LspAdapter>> = applicable.values().collect();
        let restarted = self
            .analyzer
            .tree_barrier(&workspace, &applicable_refs)
            .await;
        let walk = Arc::new(lsp::WalkSessions::default());
        walk.scope(async {
            for adapter in applicable.values() {
                match self.analyzer.symbols(adapter, &workspace, query).await {
                    Ok(mut found) => results.append(&mut found),
                    Err(error) => failures.push((adapter.provider(), format!("{error:#}"))),
                }
            }
        })
        .await;
        let notes = barrier_notes(&restarted, &[], &walk.reused());
        // `applicable` is a map, so sort for a stable message and log order.
        failures.sort();
        order_symbols(&mut results);
        // Only an answer from no server at all is an error. A server that
        // answered with nothing is a correct "no match", even when another
        // server of a mixed-language workspace failed (a stray C# file whose
        // csharp-ls exits, a TypeScript server that finds no project).
        if !applicable.is_empty() && failures.len() == applicable.len() {
            let rendered: Vec<String> = failures
                .iter()
                .map(|(provider, error)| format!("{provider}: {error}"))
                .collect();
            anyhow::bail!(
                "all applicable language servers failed: {}",
                rendered.join("; ")
            );
        }
        for (provider, error) in &failures {
            tracing::warn!(
                provider = *provider,
                workspace = %workspace.path.display(),
                error = %error,
                "language server failed to answer workspace/symbol; the other servers' results are returned"
            );
        }
        // Same wording as the language-server entry of a search's `warning`.
        let warnings = not_searched
            .into_iter()
            .chain(failures.iter().map(|(provider, error)| {
                format!("Optional LSP provider unavailable: {provider}: {error}")
            }))
            .collect();
        Ok(NavigationReport {
            workspace,
            results,
            warnings,
            notes,
        })
    }

    fn source_path(workspace: &Workspace, relative_file_path: &str) -> Result<std::path::PathBuf> {
        ensure!(
            !relative_file_path.trim().is_empty(),
            "relative_file_path must not be empty"
        );
        let file = dunce::canonicalize(workspace.path.join(relative_file_path))?;
        ensure!(
            file.starts_with(&workspace.path),
            "source file must be inside the workspace"
        );
        ensure!(file.is_file(), "source path must be a file");
        Ok(file)
    }

    /// Resolves a source file to its Tree-sitter language identifier and the
    /// one LSP adapter that navigates it, or a clear error for an
    /// unrecognized or Tree-sitter-only (no navigation adapter) file.
    fn navigation_target(&self, file: &Path) -> Result<(&'static str, Arc<dyn LspAdapter>)> {
        let identifier = language::for_path(file)
            .map(|adapter| adapter.identifier())
            .context("language-server navigation requires a recognized source path")?;
        let adapter = self
            .analyzer
            .adapters_for_language(identifier)
            .into_iter()
            .next()
            .cloned()
            .with_context(|| {
                format!("language-server navigation does not support {identifier} source files")
            })?;
        Ok((identifier, adapter))
    }

    async fn ensure_indexed_language(
        workspace: &Workspace,
        language: &str,
        store: &Store,
    ) -> Result<()> {
        let snapshot = store
            .snapshot(workspace)
            .await?
            .context("workspace is not indexed; call index_workspace first")?;
        let indexed_languages = Store::languages(&snapshot).await?;
        ensure!(
            indexed_languages.contains(language),
            "workspace index contains no {language} chunks"
        );
        Ok(())
    }

    pub async fn definition(
        &self,
        path: &Path,
        relative_file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<NavigationReport> {
        ensure!(line > 0, "line must be one-based and positive");
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let file = Self::source_path(&workspace, relative_file_path)?;
        let (language, adapter) = self.navigation_target(&file)?;
        Self::ensure_indexed_language(&workspace, language, &self.store).await?;
        // The kept session passes the tree barrier first (see `symbols`).
        let restarted = self.analyzer.tree_barrier(&workspace, &[&adapter]).await;
        let walk = Arc::new(lsp::WalkSessions::default());
        let results = walk
            .scope(self.analyzer.definition(
                &adapter,
                &workspace,
                &file,
                relative_file_path,
                line - 1,
                character,
            ))
            .await?;
        Ok(NavigationReport {
            workspace,
            results,
            warnings: Vec::new(),
            notes: barrier_notes(&restarted, &[], &walk.reused()),
        })
    }

    pub async fn references(
        &self,
        path: &Path,
        relative_file_path: &str,
        line: u32,
        character: u32,
        include_declaration: bool,
    ) -> Result<NavigationReport> {
        ensure!(line > 0, "line must be one-based and positive");
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let file = Self::source_path(&workspace, relative_file_path)?;
        let (language, adapter) = self.navigation_target(&file)?;
        Self::ensure_indexed_language(&workspace, language, &self.store).await?;
        let restarted = self.analyzer.tree_barrier(&workspace, &[&adapter]).await;
        let walk = Arc::new(lsp::WalkSessions::default());
        let results = walk
            .scope(self.analyzer.references(
                &adapter,
                &workspace,
                &file,
                relative_file_path,
                line - 1,
                character,
                include_declaration,
            ))
            .await?;
        Ok(NavigationReport {
            workspace,
            results,
            warnings: Vec::new(),
            notes: barrier_notes(&restarted, &[], &walk.reused()),
        })
    }
}

/// Puts the answers of all servers in one order that does not depend on which
/// server answered first, and drops repeats. The order is file, line and
/// language first, then every other field: sorting by those three alone left
/// ties in the order the servers happened to answer, and `dedup` removes only
/// neighbours, so a repeat with another symbol between its copies survived.
fn order_symbols(results: &mut Vec<lsp::Location>) {
    results.sort_by(|a, b| {
        a.relative_file_path
            .cmp(&b.relative_file_path)
            .then(a.start_line.cmp(&b.start_line))
            .then(a.language.cmp(&b.language))
            .then(a.start_character.cmp(&b.start_character))
            .then(a.end_line.cmp(&b.end_line))
            .then(a.end_character.cmp(&b.end_character))
            .then(a.name.cmp(&b.name))
            .then(a.kind.cmp(&b.kind))
            .then(a.provider.cmp(&b.provider))
            .then(a.file_path.cmp(&b.file_path))
    });
    results.dedup();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn symbol(line: u32, character: u32, name: &str) -> lsp::Location {
        lsp::Location {
            file_path: std::path::PathBuf::from("/w/src/a.cs"),
            relative_file_path: Some("src/a.cs".to_owned()),
            start_line: line,
            start_character: character,
            end_line: line,
            end_character: character + 1,
            name: Some(name.to_owned()),
            kind: Some(5),
            language: Some("csharp".to_owned()),
            provider: Some("csharp-ls".to_owned()),
        }
    }

    #[test]
    fn answers_on_one_line_come_out_in_the_same_order_whatever_order_they_arrive_in() {
        let (late, early, first) = (symbol(3, 8, "b"), symbol(3, 2, "a"), symbol(1, 0, "z"));
        let mut one = vec![late.clone(), early.clone(), first.clone()];
        let mut other = vec![first.clone(), early.clone(), late.clone()];
        order_symbols(&mut one);
        order_symbols(&mut other);
        assert_eq!(one, other);
        assert_eq!(one, [first, early, late]);
    }

    fn report(notes: Vec<String>) -> NavigationReport {
        NavigationReport {
            workspace: Workspace {
                id: "w".to_owned(),
                path: std::path::PathBuf::from("/w"),
            },
            results: Vec::new(),
            warnings: Vec::new(),
            notes,
        }
    }

    /// The wire: an answer with no barrier note serialises as it did before
    /// `notes` existed.
    #[test]
    fn a_report_without_notes_serialises_without_the_key() {
        let json = serde_json::to_value(report(Vec::new())).unwrap();
        assert!(json.get("notes").is_none(), "{json}");
        assert!(json.get("warnings").is_none(), "{json}");
        let json =
            serde_json::to_value(report(vec!["lsp: csharp-ls restarted".to_owned()])).unwrap();
        assert_eq!(json["notes"][0], "lsp: csharp-ls restarted");
    }

    #[test]
    fn the_barrier_notes_list_restarts_first_then_kept_sessions() {
        use std::time::Duration;
        assert!(barrier_notes(&[], &[], &[]).is_empty());
        assert_eq!(
            barrier_notes(
                &[lsp::Restart {
                    provider: "csharp-ls",
                    reason: lsp::RestartReason::TreeChanged,
                }],
                &[lsp::Kept {
                    provider: "rust-analyzer",
                    reason: lsp::RestartReason::TreeChanged,
                }],
                &[("pyright", Some(Duration::from_secs(7))), ("clangd", None)]
            ),
            [
                "lsp: csharp-ls restarted: the tree changed since its last call",
                "lsp: rust-analyzer kept session; the tree changed since its last call (kept: it watches its workspace)",
                "lsp: pyright kept session; tree stamp unchanged since 7 s ago",
                "lsp: clangd kept session; no tree stamp was recorded for it",
            ]
        );
    }

    /// A restart says why it happened: a changed tree is not the only cause.
    #[test]
    fn a_restart_note_names_its_reason() {
        let note = |reason| {
            barrier_notes(
                &[lsp::Restart {
                    provider: "csharp-ls",
                    reason,
                }],
                &[],
                &[],
            )
        };
        assert_eq!(
            note(lsp::RestartReason::TreeChanged),
            ["lsp: csharp-ls restarted: the tree changed since its last call"]
        );
        assert_eq!(
            note(lsp::RestartReason::NoFileList),
            ["lsp: csharp-ls restarted: the index has no file list to stamp"]
        );
        assert_eq!(
            note(lsp::RestartReason::Unrecorded),
            ["lsp: csharp-ls restarted: no tree stamp was recorded for it"]
        );
        assert_eq!(
            note(lsp::RestartReason::StampFailed("src/x.cs: denied".into())),
            ["lsp: csharp-ls restarted: the tree stamp failed (src/x.cs: denied)"]
        );
    }

    #[test]
    fn a_repeated_answer_is_removed_even_with_another_symbol_between_the_copies() {
        let repeated = symbol(3, 8, "b");
        let mut results = vec![repeated.clone(), symbol(3, 2, "a"), repeated.clone()];
        order_symbols(&mut results);
        assert_eq!(results, [symbol(3, 2, "a"), repeated]);
    }
}
