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
use std::{
    collections::{HashMap, HashSet},
    path::Path,
    sync::Arc,
};

#[derive(Debug, Serialize)]
pub struct NavigationReport {
    pub workspace: Workspace,
    pub results: Vec<lsp::Location>,
    /// One `Optional LSP provider unavailable: <provider>: <cause>` entry, in
    /// provider order, for each language server that failed to answer while
    /// the other servers' results were returned, so an empty or short
    /// `results` is not read as a clean "no match". Absent from the JSON when
    /// empty. Only `symbols` can set it: `definition` and `references` ask
    /// one server and report its failure as an error.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
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
        self.analyzer.shutdown_all().await
    }

    pub async fn symbols(&self, path: &Path, query: &str) -> Result<NavigationReport> {
        ensure!(!query.trim().is_empty(), "query must not be empty");
        let workspace = Workspace::resolve(path, &self.config.data_dir)?;
        let snapshot = self
            .store
            .snapshot(&workspace)
            .await?
            .context("workspace is not indexed; call index_workspace first")?;
        let chunks = Store::chunks(&snapshot, &workspace).await?;
        let indexed_languages: HashSet<&str> = chunks
            .iter()
            .map(|hit| hit.chunk.language.as_str())
            .collect();
        let mut applicable: HashMap<&'static str, Arc<dyn LspAdapter>> = HashMap::new();
        for language in indexed_languages {
            for adapter in self.analyzer.adapters_for_language(language) {
                if adapter.enabled() {
                    applicable
                        .entry(adapter.provider())
                        .or_insert_with(|| Arc::clone(adapter));
                }
            }
        }
        let mut results = Vec::new();
        let mut failures: Vec<(&'static str, String)> = Vec::new();
        for adapter in applicable.values() {
            match self.analyzer.symbols(adapter, &workspace, query).await {
                Ok(mut found) => results.append(&mut found),
                Err(error) => failures.push((adapter.provider(), format!("{error:#}"))),
            }
        }
        // `applicable` is a map, so sort for a stable message and log order.
        failures.sort();
        results.sort_by(|a, b| {
            a.relative_file_path
                .cmp(&b.relative_file_path)
                .then(a.start_line.cmp(&b.start_line))
                .then(a.language.cmp(&b.language))
        });
        results.dedup();
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
        let warnings = failures
            .iter()
            .map(|(provider, error)| {
                format!("Optional LSP provider unavailable: {provider}: {error}")
            })
            .collect();
        Ok(NavigationReport {
            workspace,
            results,
            warnings,
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
        let chunks = Store::chunks(&snapshot, workspace).await?;
        ensure!(
            chunks.iter().any(|hit| hit.chunk.language == language),
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
        let results = self
            .analyzer
            .definition(
                &adapter,
                &workspace,
                &file,
                relative_file_path,
                line - 1,
                character,
            )
            .await?;
        Ok(NavigationReport {
            workspace,
            results,
            warnings: Vec::new(),
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
        let results = self
            .analyzer
            .references(
                &adapter,
                &workspace,
                &file,
                relative_file_path,
                line - 1,
                character,
                include_declaration,
            )
            .await?;
        Ok(NavigationReport {
            workspace,
            results,
            warnings: Vec::new(),
        })
    }
}
