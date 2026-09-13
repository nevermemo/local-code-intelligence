use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::metrics::validate_pattern;
use super::types::EvaluationFile;
// ---------------------------------------------------------------------------
// TOML definition loading and validation
// ---------------------------------------------------------------------------

/// Load and validate a TOML evaluation definition file.
///
/// Validates:
/// - workspace keys and paths are non-empty,
/// - workspace keys are unique,
/// - query workspace keys resolve to a declared workspace,
/// - all path patterns (expected, preferred, disfavored) are valid
///   repository-relative globs,
/// - filter path patterns are valid repository-relative globs.
pub fn load_definition(path: &Path) -> Result<EvaluationFile> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read evaluation definition: {}", path.display()))?;
    let file: EvaluationFile = toml::from_str(&raw)
        .with_context(|| format!("failed to parse TOML: {}", path.display()))?;
    validate_definition(&file)?;
    Ok(file)
}

/// Validate an evaluation definition for structural correctness.
pub fn validate_definition(file: &EvaluationFile) -> Result<()> {
    let mut seen_keys: BTreeSet<&str> = BTreeSet::new();
    for ws in &file.workspaces {
        if ws.key.trim().is_empty() {
            bail!("workspace key must not be empty");
        }
        if !seen_keys.insert(ws.key.as_str()) {
            bail!("duplicate workspace key: {:?}", ws.key);
        }
    }

    // Validate queries.
    let declared_keys: BTreeSet<&str> = file.workspaces.iter().map(|ws| ws.key.as_str()).collect();
    for (i, query) in file.queries.iter().enumerate() {
        if query.query.trim().is_empty() {
            bail!("query at index {i} has empty query text");
        }
        if query.workspace.trim().is_empty() {
            bail!("query at index {i} has empty workspace key");
        }
        if !declared_keys.contains(query.workspace.as_str()) {
            bail!(
                "query at index {i} references undeclared workspace key {:?}",
                query.workspace
            );
        }
        for pattern in &query.expected_paths {
            validate_pattern(pattern)
                .with_context(|| format!("query at index {i}: invalid expected_paths pattern"))?;
        }
        for pattern in &query.preferred_paths {
            validate_pattern(pattern)
                .with_context(|| format!("query at index {i}: invalid preferred_paths pattern"))?;
        }
        for pattern in &query.disfavored_paths {
            validate_pattern(pattern)
                .with_context(|| format!("query at index {i}: invalid disfavored_paths pattern"))?;
        }
        if let Some(filters) = &query.filters {
            if let Some(ref paths) = filters.include_paths {
                for pattern in paths {
                    validate_pattern(pattern).with_context(|| {
                        format!("query at index {i}: invalid include_paths pattern")
                    })?;
                }
            }
            if let Some(ref paths) = filters.exclude_paths {
                for pattern in paths {
                    validate_pattern(pattern).with_context(|| {
                        format!("query at index {i}: invalid exclude_paths pattern")
                    })?;
                }
            }
        }
    }
    Ok(())
}
