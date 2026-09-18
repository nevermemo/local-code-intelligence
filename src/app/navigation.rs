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
}

impl App {
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
        let mut failures = Vec::new();
        for adapter in applicable.values() {
            match self.analyzer.symbols(adapter, &workspace, query).await {
                Ok(mut found) => results.append(&mut found),
                Err(error) => failures.push(format!("{}: {error:#}", adapter.provider())),
            }
        }
        results.sort_by(|a, b| {
            a.relative_file_path
                .cmp(&b.relative_file_path)
                .then(a.start_line.cmp(&b.start_line))
                .then(a.language.cmp(&b.language))
        });
        results.dedup();
        if results.is_empty() && !failures.is_empty() {
            anyhow::bail!(
                "all applicable language servers failed: {}",
                failures.join("; ")
            );
        }
        Ok(NavigationReport { workspace, results })
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
        Ok(NavigationReport { workspace, results })
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
        Ok(NavigationReport { workspace, results })
    }
}
