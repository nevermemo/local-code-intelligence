use std::collections::BTreeSet;
use std::fmt;

use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::language;

use super::SourceRole;
use super::source_role::normalize_path;
// ---------------------------------------------------------------------------
// Glob matching
// ---------------------------------------------------------------------------

/// A compiled glob pattern over normalized repository-relative paths.
///
/// Semantics (configured explicitly on the `globset` builder):
/// - `*` matches any sequence of characters that does not cross `/`
///   (`literal_separator(true)`).
/// - `**` matches any sequence of characters, including `/` (crosses
///   directories, zero or more segments).
/// - `?` matches exactly one character that is not `/`.
/// - Matching is case-sensitive (`case_insensitive(false)`).
#[derive(Debug, Clone)]
pub struct GlobPattern {
    raw: String,
    matcher: GlobMatcher,
}

impl GlobPattern {
    /// Compile a glob pattern. Returns an error for empty or malformed
    /// patterns, absolute Unix/Windows/UNC patterns, backslashes, or any
    /// `..` path component.
    pub fn compile(pattern: &str) -> Result<Self, FilterError> {
        let trimmed = pattern.trim();
        if trimmed.is_empty() {
            return Err(FilterError::EmptyPattern);
        }
        if trimmed.contains('\\') {
            return Err(FilterError::BackslashInPattern {
                pattern: pattern.to_owned(),
            });
        }
        if is_absolute_pattern(trimmed) {
            return Err(FilterError::AbsolutePath {
                pattern: pattern.to_owned(),
            });
        }
        for comp in trimmed.split('/') {
            if comp == ".." {
                return Err(FilterError::Traversal {
                    pattern: pattern.to_owned(),
                });
            }
        }
        let matcher = GlobBuilder::new(trimmed)
            .literal_separator(true)
            .case_insensitive(false)
            .build()
            .map_err(|_| FilterError::MalformedPattern {
                pattern: pattern.to_owned(),
            })?
            .compile_matcher();
        Ok(Self {
            raw: trimmed.to_owned(),
            matcher,
        })
    }

    /// The raw pattern string.
    pub fn raw(&self) -> &str {
        &self.raw
    }

    /// Whether `path` (a normalized repository-relative path) matches this
    /// pattern.
    pub fn matches(&self, path: &str) -> bool {
        let normalized = normalize_path(path);
        self.matcher.is_match(&normalized)
    }
}

fn is_absolute_pattern(pattern: &str) -> bool {
    // Unix absolute.
    if pattern.starts_with('/') {
        return true;
    }
    // Windows drive-letter absolute, e.g. `C:\...` or `C:/...`.
    if pattern.len() >= 3
        && pattern.as_bytes()[0].is_ascii_alphabetic()
        && pattern.as_bytes()[1] == b':'
    {
        return true;
    }
    // UNC, e.g. `//server/share`.
    if pattern.starts_with("//") {
        return true;
    }
    false
}

// ---------------------------------------------------------------------------
// Filter request / validation
// ---------------------------------------------------------------------------

/// A raw, optional filter request. `None` means the filter is absent;
/// `Some(vec![])` means an explicitly supplied empty list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FilterRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub languages: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exclude_paths: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_roles: Option<Vec<String>>,
}

/// A validated, effective filter. `None` means the filter is absent;
/// `Some(vec![])` is never produced for languages or source_roles (those
/// are rejected when explicitly empty), but an explicitly empty path list
/// is preserved as `Some(vec![])`: an empty include list matches no paths,
/// while an empty exclude list excludes nothing.
///
/// Path patterns are compiled once at validation time; only the raw
/// patterns are exposed, through [`EffectiveFilter::report`].
#[derive(Debug, Clone)]
pub struct EffectiveFilter {
    /// Registered language identifiers, deduplicated and sorted. `None`
    /// means no language filter.
    pub languages: Option<Vec<String>>,
    /// Compiled include-path patterns. `None` means no include filter;
    /// `Some(vec![])` matches no paths.
    include_paths: Option<Vec<GlobPattern>>,
    /// Compiled exclude-path patterns. `None` means no exclude filter;
    /// `Some(vec![])` excludes nothing.
    exclude_paths: Option<Vec<GlobPattern>>,
    /// Source roles, deduplicated and sorted. `None` means no role filter.
    pub source_roles: Option<Vec<SourceRole>>,
}

/// Error returned when a [`FilterRequest`] cannot be validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FilterError {
    /// An explicitly supplied language list is empty.
    EmptyLanguages,
    /// An explicitly supplied source-role list is empty.
    EmptySourceRoles,
    /// A language identifier is not registered.
    UnknownLanguage { value: String },
    /// A source-role name is not registered.
    UnknownSourceRole { value: String },
    /// A path pattern is empty.
    EmptyPattern,
    /// A path pattern contains a backslash.
    BackslashInPattern { pattern: String },
    /// A path pattern is an absolute Unix, Windows, or UNC path.
    AbsolutePath { pattern: String },
    /// A path pattern contains a `..` component.
    Traversal { pattern: String },
    /// A path pattern could not be compiled as a glob.
    MalformedPattern { pattern: String },
}

impl fmt::Display for FilterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterError::EmptyLanguages => write!(f, "language filter must not be an empty list"),
            FilterError::EmptySourceRoles => {
                write!(f, "source-role filter must not be an empty list")
            }
            FilterError::UnknownLanguage { value } => {
                write!(
                    f,
                    "unknown language '{value}'; registered identifiers: {}",
                    language::identifiers().collect::<Vec<_>>().join(", ")
                )
            }
            FilterError::UnknownSourceRole { value } => {
                write!(
                    f,
                    "unknown source role '{value}'; expected one of: source, test, benchmark, example, generated, documentation, configuration"
                )
            }
            FilterError::EmptyPattern => write!(f, "path pattern must not be empty"),
            FilterError::BackslashInPattern { pattern } => {
                write!(f, "path pattern '{pattern}' must not contain backslashes")
            }
            FilterError::AbsolutePath { pattern } => {
                write!(
                    f,
                    "path pattern '{pattern}' must be a repository-relative path, not an absolute path"
                )
            }
            FilterError::Traversal { pattern } => {
                write!(
                    f,
                    "path pattern '{pattern}' must not contain '..' components"
                )
            }
            FilterError::MalformedPattern { pattern } => {
                write!(f, "path pattern '{pattern}' is not a valid glob")
            }
        }
    }
}

impl std::error::Error for FilterError {}

impl EffectiveFilter {
    /// Validate a raw [`FilterRequest`] into an effective filter.
    pub fn from_request(request: &FilterRequest) -> Result<Self, FilterError> {
        let languages = match &request.languages {
            None => None,
            Some(list) => {
                if list.is_empty() {
                    return Err(FilterError::EmptyLanguages);
                }
                let mut set: BTreeSet<String> = BTreeSet::new();
                for lang in list {
                    if !language::is_registered_identifier(lang) {
                        return Err(FilterError::UnknownLanguage {
                            value: lang.clone(),
                        });
                    }
                    set.insert(lang.clone());
                }
                Some(set.into_iter().collect())
            }
        };

        let source_roles = match &request.source_roles {
            None => None,
            Some(list) => {
                if list.is_empty() {
                    return Err(FilterError::EmptySourceRoles);
                }
                let mut set: BTreeSet<SourceRole> = BTreeSet::new();
                for role in list {
                    let parsed =
                        SourceRole::parse(role).map_err(|_| FilterError::UnknownSourceRole {
                            value: role.clone(),
                        })?;
                    set.insert(parsed);
                }
                Some(set.into_iter().collect())
            }
        };

        let include_paths = compile_path_list(request.include_paths.as_deref())?;
        let exclude_paths = compile_path_list(request.exclude_paths.as_deref())?;

        Ok(Self {
            languages,
            include_paths,
            exclude_paths,
            source_roles,
        })
    }

    /// Whether a chunk with the given language identifier, normalized
    /// repository-relative path, and source role passes this filter.
    ///
    /// A chunk passes when:
    /// - it matches the language filter (or there is no language filter),
    ///   AND
    /// - it matches the include filter (or there is no include filter),
    ///   AND
    /// - it does not match any exclude pattern,
    ///   AND
    /// - it matches the source-role filter (or there is no role filter).
    pub fn matches(
        &self,
        language_identifier: &str,
        relative_path: &str,
        source_role: SourceRole,
    ) -> bool {
        if self
            .languages
            .as_ref()
            .is_some_and(|langs| !langs.iter().any(|language| language == language_identifier))
        {
            return false;
        }
        if self
            .source_roles
            .as_ref()
            .is_some_and(|roles| !roles.contains(&source_role))
        {
            return false;
        }
        let normalized = normalize_path(relative_path);
        if let Some(ref includes) = self.include_paths {
            let matched = includes.iter().any(|pattern| pattern.matches(&normalized));
            if !matched {
                return false;
            }
        }
        if let Some(ref excludes) = self.exclude_paths {
            let excluded = excludes.iter().any(|pattern| pattern.matches(&normalized));
            if excluded {
                return false;
            }
        }
        true
    }

    /// A serializable report of the effective filter. Absent filters are
    /// represented as `null` (i.e. `None`), not omitted.
    pub fn report(&self) -> EffectiveFilterReport {
        EffectiveFilterReport {
            languages: self.languages.clone(),
            include_paths: self
                .include_paths
                .as_ref()
                .map(|patterns| patterns.iter().map(|pattern| pattern.raw.clone()).collect()),
            exclude_paths: self
                .exclude_paths
                .as_ref()
                .map(|patterns| patterns.iter().map(|pattern| pattern.raw.clone()).collect()),
            source_roles: self
                .source_roles
                .as_ref()
                .map(|roles| roles.iter().map(|r| r.as_str().to_owned()).collect()),
        }
    }
}

/// A serializable report of an effective filter. Absent filters are
/// represented as `null` (`None`), not omitted from serialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectiveFilterReport {
    #[serde(default)]
    pub languages: Option<Vec<String>>,
    #[serde(default)]
    pub include_paths: Option<Vec<String>>,
    #[serde(default)]
    pub exclude_paths: Option<Vec<String>>,
    #[serde(default)]
    pub source_roles: Option<Vec<String>>,
}

fn compile_path_list(patterns: Option<&[String]>) -> Result<Option<Vec<GlobPattern>>, FilterError> {
    let Some(list) = patterns else {
        return Ok(None);
    };
    // Deduplicate deterministically: sort, then remove adjacent duplicates.
    let mut sorted = list.to_vec();
    sorted.sort();
    sorted.dedup();
    sorted
        .iter()
        .map(|pattern| GlobPattern::compile(pattern))
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}
