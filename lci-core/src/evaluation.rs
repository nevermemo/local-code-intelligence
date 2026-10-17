//! Portable evaluation definitions and validation, independent of storage.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::filter::{FilterRequest, GlobPattern};

/// A portable evaluation file: the workspaces it references plus the
/// queries to run against them.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationFile {
    /// Workspaces referenced by key.
    #[serde(default)]
    pub workspaces: Vec<EvalWorkspace>,
    /// Queries to evaluate.
    #[serde(default)]
    pub queries: Vec<EvalQuery>,
}

/// A workspace referenced by an evaluation file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalWorkspace {
    /// Stable key used by [`EvalQuery::workspace`].
    pub key: String,
    /// Required workspaces must be present; when an optional workspace is
    /// missing, its queries are skipped rather than failing the run.
    /// Defaults to `true` when omitted in a definition file.
    #[serde(default = "default_required")]
    pub required: bool,
}

const fn default_required() -> bool {
    true
}

/// A single evaluation query.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalQuery {
    /// Query text sent to search.
    pub query: String,
    /// Key of the workspace this query runs against.
    pub workspace: String,
    /// Result window size; `None` uses the service default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_k: Option<usize>,
    /// Optional retrieval filters, shared with the service filter request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<FilterRequest>,
    /// Repository-relative path patterns that count as a hit. When several
    /// patterns match the same result, the result still counts as one hit.
    #[serde(default)]
    pub expected_paths: Vec<String>,
    /// Path patterns that should appear in the results; each matching
    /// result counts once toward `QueryMetrics::preferred_count`
    /// regardless of how many patterns match it.
    #[serde(default)]
    pub preferred_paths: Vec<String>,
    /// Path patterns that should not appear before the first expected hit.
    #[serde(default)]
    pub disfavored_paths: Vec<String>,
    /// Source-role names (lowercase wire names) expected on the first
    /// expected hit. Empty means no role expectation.
    #[serde(default)]
    pub expected_roles: Vec<String>,
    /// A snippet that must appear in any returned chunk's bounded snippet;
    /// `None` means no snippet expectation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_snippet: Option<String>,
    /// Free-form labels that stratify the aggregate: the report carries one
    /// `TagAggregate` per distinct tag. Generated query sets use
    /// `keyword` or `keyword-agnostic`, optionally followed by `gold-drift`
    /// and one of `short-query` / `long-query`. Tags never affect search. The
    /// only tags the evaluation reads are the two lane tags
    /// ([`EvalLane::DOCS_TAG`], [`EvalLane::UNION_TAG`]), which choose the
    /// result list the query is scored on (see [`EvalQuery::lane`]).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

/// Which list of a search report an evaluation query is scored on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EvalLane {
    /// The code `results` (the default).
    #[default]
    Code,
    /// The separate `docs` list.
    Docs,
    /// The code `results` followed by the `docs` list.
    Union,
}

impl EvalLane {
    /// The tag that puts a query on the docs lane.
    pub const DOCS_TAG: &'static str = "lane:docs";
    /// The tag that puts a query on the code results followed by the docs.
    pub const UNION_TAG: &'static str = "lane:union";
}

impl EvalQuery {
    /// The list this query is scored on: [`EvalLane::Docs`] with the tag
    /// `lane:docs`, [`EvalLane::Union`] with `lane:union` (the docs tag wins
    /// when both are present), otherwise [`EvalLane::Code`].
    pub fn lane(&self) -> EvalLane {
        let tagged = |tag: &str| self.tags.iter().any(|candidate| candidate == tag);
        if tagged(EvalLane::DOCS_TAG) {
            EvalLane::Docs
        } else if tagged(EvalLane::UNION_TAG) {
            EvalLane::Union
        } else {
            EvalLane::Code
        }
    }
}

/// Validate a repository-relative path pattern.
///
/// Patterns must be non-empty, use `/` separators, and stay inside the
/// repository: no backslashes, no absolute or drive/UNC paths, no `.` or
/// `..` components, and no leading `/`. Glob metacharacters `*`, `?`, and
/// `[...]` are allowed.
pub fn validate_pattern(pattern: &str) -> Result<()> {
    GlobPattern::compile(pattern)
        .map(|_| ())
        .map_err(Into::into)
}

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
        if query.expected_paths.is_empty() {
            bail!("query at index {i} has no expected_paths");
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
