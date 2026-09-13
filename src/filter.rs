//! Centralized source-role classification and shared retrieval filter
//! request/validation/matching types.
//!
//! Source role is derived deterministically from a normalized
//! repository-relative path; it is never persisted in `Chunk` or LanceDB.
//! This keeps existing stores and manifests intact without a schema
//! migration.

use std::collections::BTreeSet;
use std::fmt;
use std::str::FromStr;

use globset::{GlobBuilder, GlobMatcher};
use serde::{Deserialize, Serialize};

use crate::language;

// ---------------------------------------------------------------------------
// SourceRole
// ---------------------------------------------------------------------------

/// The role a source file plays in a repository, derived from its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SourceRole {
    Source,
    Test,
    Benchmark,
    Example,
    Generated,
    Documentation,
    Configuration,
}

impl SourceRole {
    /// All registered roles, in declaration order.
    pub const ALL: [SourceRole; 7] = [
        SourceRole::Source,
        SourceRole::Test,
        SourceRole::Benchmark,
        SourceRole::Example,
        SourceRole::Generated,
        SourceRole::Documentation,
        SourceRole::Configuration,
    ];

    /// The lowercase wire/CLI name for this role.
    pub const fn as_str(self) -> &'static str {
        match self {
            SourceRole::Source => "source",
            SourceRole::Test => "test",
            SourceRole::Benchmark => "benchmark",
            SourceRole::Example => "example",
            SourceRole::Generated => "generated",
            SourceRole::Documentation => "documentation",
            SourceRole::Configuration => "configuration",
        }
    }

    /// Parse a role from its lowercase name.
    pub fn parse(name: &str) -> Result<Self, SourceRoleError> {
        match name {
            "source" => Ok(SourceRole::Source),
            "test" => Ok(SourceRole::Test),
            "benchmark" => Ok(SourceRole::Benchmark),
            "example" => Ok(SourceRole::Example),
            "generated" => Ok(SourceRole::Generated),
            "documentation" => Ok(SourceRole::Documentation),
            "configuration" => Ok(SourceRole::Configuration),
            other => Err(SourceRoleError::Unknown {
                value: other.to_owned(),
            }),
        }
    }
}

impl fmt::Display for SourceRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for SourceRole {
    type Err = SourceRoleError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

/// Error returned when a source-role name cannot be parsed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SourceRoleError {
    /// The supplied value is not a registered role name.
    Unknown { value: String },
}

impl fmt::Display for SourceRoleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SourceRoleError::Unknown { value } => write!(
                f,
                "unknown source role '{value}'; expected one of: source, test, benchmark, example, generated, documentation, configuration"
            ),
        }
    }
}

impl std::error::Error for SourceRoleError {}

// ---------------------------------------------------------------------------
// Source-role classification
// ---------------------------------------------------------------------------

/// Classify the source role of a normalized repository-relative path.
///
/// Precedence: generated > test > benchmark > example > documentation >
/// configuration > source. Path separators are treated as normalized `/`.
/// Directory conventions use exact component matching only.
pub fn classify_source_role(relative_path: &str) -> SourceRole {
    let normalized = normalize_path(relative_path);
    let components: Vec<&str> = normalized.split('/').filter(|c| !c.is_empty()).collect();
    let file_name = components.last().copied().unwrap_or("");
    let dir_components: Vec<&str> = components
        .iter()
        .copied()
        .take(components.len().saturating_sub(1))
        .collect();

    if is_generated(&dir_components, file_name) {
        return SourceRole::Generated;
    }
    if is_test(&dir_components, file_name) {
        return SourceRole::Test;
    }
    if is_benchmark(&dir_components, file_name) {
        return SourceRole::Benchmark;
    }
    if is_example(&dir_components, file_name) {
        return SourceRole::Example;
    }
    if is_documentation(&dir_components) {
        return SourceRole::Documentation;
    }
    if is_configuration(&dir_components) {
        return SourceRole::Configuration;
    }
    SourceRole::Source
}

fn is_generated(dirs: &[&str], file_name: &str) -> bool {
    if dirs.iter().any(|c| matches!(*c, "generated" | "gen")) {
        return true;
    }
    is_generated_file_name(file_name)
}

fn is_generated_file_name(file_name: &str) -> bool {
    // Filename markers only apply to supported source files. Each marker is
    // a literal dot-delimited substring so it cannot match as part of a
    // longer token (e.g. `foo.gx.rs` has no `.g.` substring).
    if !is_supported_source_file_name(file_name) {
        return false;
    }
    file_name.contains(".generated.")
        || file_name.contains("_generated.")
        || file_name.contains(".g.")
}

/// Whether `file_name` carries a registered source-file extension.
fn is_supported_source_file_name(file_name: &str) -> bool {
    file_name
        .rsplit('.')
        .next()
        .is_some_and(|extension| language::for_extension(extension).is_some())
}

fn is_test(dirs: &[&str], file_name: &str) -> bool {
    if dirs
        .iter()
        .any(|c| matches!(*c, "test" | "tests" | "__tests__"))
    {
        return true;
    }
    is_test_file_name(file_name)
}

fn is_test_file_name(file_name: &str) -> bool {
    // Python: `*_test.py`, `test_*.py`.
    if file_name
        .strip_suffix(".py")
        .is_some_and(|stripped| stripped.ends_with("_test") || stripped.starts_with("test_"))
    {
        return true;
    }
    // JS/TS: `*.test.ts`, `*.spec.ts`, `*.test.tsx`, `*.spec.tsx`,
    // `*.test.js`, `*.spec.js`, `*.test.jsx`, `*.spec.jsx`.
    for suffix in [
        ".test.ts",
        ".spec.ts",
        ".test.tsx",
        ".spec.tsx",
        ".test.js",
        ".spec.js",
        ".test.jsx",
        ".spec.jsx",
    ] {
        if file_name.ends_with(suffix) {
            return true;
        }
    }
    false
}

fn is_benchmark(dirs: &[&str], file_name: &str) -> bool {
    if dirs
        .iter()
        .any(|c| matches!(*c, "bench" | "benches" | "benchmark" | "benchmarks"))
    {
        return true;
    }
    is_benchmark_file_name(file_name)
}

fn is_benchmark_file_name(file_name: &str) -> bool {
    // Conservative, clear filename conventions.
    if file_name
        .strip_suffix(".rs")
        .is_some_and(|stripped| stripped.ends_with("_bench") || stripped.starts_with("bench_"))
    {
        return true;
    }
    if file_name
        .strip_suffix(".py")
        .is_some_and(|stripped| stripped.ends_with("_bench") || stripped.starts_with("bench_"))
    {
        return true;
    }
    for suffix in [".bench.ts", ".bench.js", ".bench.tsx", ".bench.jsx"] {
        if file_name.ends_with(suffix) {
            return true;
        }
    }
    false
}

fn is_example(dirs: &[&str], _file_name: &str) -> bool {
    dirs.iter()
        .any(|c| matches!(*c, "example" | "examples" | "samples"))
}

fn is_documentation(dirs: &[&str]) -> bool {
    dirs.iter()
        .any(|c| matches!(*c, "doc" | "docs" | "documentation"))
}

fn is_configuration(dirs: &[&str]) -> bool {
    dirs.iter().any(|c| {
        matches!(
            *c,
            "config" | "configs" | "configuration" | "scripts" | "tools" | "build"
        )
    })
}

// ---------------------------------------------------------------------------
// Path normalization
// ---------------------------------------------------------------------------

/// Normalize a repository-relative path to use `/` separators.
///
/// Backslashes are converted to forward slashes. Leading and trailing
/// separators are trimmed. Repeated separators are collapsed.
fn normalize_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    let mut prev_sep = false;
    for ch in path.chars() {
        let is_sep = ch == '/' || ch == '\\';
        if is_sep {
            if !prev_sep && !out.is_empty() {
                out.push('/');
            }
            prev_sep = true;
        } else {
            out.push(ch);
            prev_sep = false;
        }
    }
    // Trim trailing separator.
    while out.ends_with('/') {
        out.pop();
    }
    out
}

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -- SourceRole parsing / display ------------------------------------

    #[test]
    fn source_role_round_trips_all_variants() {
        for role in SourceRole::ALL {
            let name = role.as_str();
            let parsed = SourceRole::from_str(name).unwrap();
            assert_eq!(parsed, role);
            assert_eq!(parsed.to_string(), name);
        }
    }

    #[test]
    fn source_role_unknown_errors_clearly() {
        let err = SourceRole::from_str("bogus").unwrap_err();
        assert!(matches!(err, SourceRoleError::Unknown { ref value } if value == "bogus"));
        assert!(err.to_string().contains("bogus"));
    }

    // -- classify_source_role ---------------------------------------------

    #[test]
    fn classifies_generated_by_directory() {
        assert_eq!(
            classify_source_role("generated/foo.rs"),
            SourceRole::Generated
        );
        assert_eq!(classify_source_role("gen/bar.py"), SourceRole::Generated);
    }

    #[test]
    fn classifies_generated_by_file_name_markers() {
        assert_eq!(
            classify_source_role("src/foo.generated.rs"),
            SourceRole::Generated
        );
        assert_eq!(
            classify_source_role("src/foo_generated.py"),
            SourceRole::Generated
        );
        assert_eq!(classify_source_role("src/foo.g.rs"), SourceRole::Generated);
    }

    #[test]
    fn generated_g_marker_is_unambiguous() {
        // `.g.` must not match as part of a longer token.
        assert_eq!(classify_source_role("src/foo.gx.rs"), SourceRole::Source);
        assert_eq!(classify_source_role("src/generate.rs"), SourceRole::Source);
        assert_eq!(classify_source_role("src/flag.rs"), SourceRole::Source);
    }

    #[test]
    fn classifies_test_by_directory() {
        assert_eq!(classify_source_role("tests/foo.rs"), SourceRole::Test);
        assert_eq!(classify_source_role("test/bar.py"), SourceRole::Test);
        assert_eq!(classify_source_role("__tests__/baz.js"), SourceRole::Test);
    }

    #[test]
    fn classifies_test_by_file_name() {
        assert_eq!(classify_source_role("src/foo_test.py"), SourceRole::Test);
        assert_eq!(classify_source_role("src/test_foo.py"), SourceRole::Test);
        assert_eq!(classify_source_role("src/foo.test.ts"), SourceRole::Test);
        assert_eq!(classify_source_role("src/foo.spec.tsx"), SourceRole::Test);
        assert_eq!(classify_source_role("src/foo.test.js"), SourceRole::Test);
        assert_eq!(classify_source_role("src/foo.spec.jsx"), SourceRole::Test);
    }

    #[test]
    fn classifies_benchmark_by_directory() {
        assert_eq!(
            classify_source_role("benches/foo.rs"),
            SourceRole::Benchmark
        );
        assert_eq!(classify_source_role("bench/bar.py"), SourceRole::Benchmark);
        assert_eq!(
            classify_source_role("benchmark/baz.rs"),
            SourceRole::Benchmark
        );
        assert_eq!(
            classify_source_role("benchmarks/qux.rs"),
            SourceRole::Benchmark
        );
    }

    #[test]
    fn classifies_benchmark_by_file_name() {
        assert_eq!(
            classify_source_role("src/foo_bench.rs"),
            SourceRole::Benchmark
        );
        assert_eq!(
            classify_source_role("src/bench_foo.py"),
            SourceRole::Benchmark
        );
        assert_eq!(
            classify_source_role("src/foo.bench.ts"),
            SourceRole::Benchmark
        );
    }

    #[test]
    fn classifies_example_by_directory() {
        assert_eq!(classify_source_role("examples/foo.rs"), SourceRole::Example);
        assert_eq!(classify_source_role("example/bar.py"), SourceRole::Example);
        assert_eq!(classify_source_role("samples/baz.js"), SourceRole::Example);
    }

    #[test]
    fn classifies_documentation_by_directory() {
        assert_eq!(
            classify_source_role("docs/guide.md"),
            SourceRole::Documentation
        );
        assert_eq!(
            classify_source_role("doc/api.md"),
            SourceRole::Documentation
        );
        assert_eq!(
            classify_source_role("documentation/overview.md"),
            SourceRole::Documentation
        );
    }

    #[test]
    fn classifies_configuration_by_directory() {
        assert_eq!(
            classify_source_role("config/app.toml"),
            SourceRole::Configuration
        );
        assert_eq!(
            classify_source_role("scripts/build.sh"),
            SourceRole::Configuration
        );
        assert_eq!(
            classify_source_role("tools/lint.py"),
            SourceRole::Configuration
        );
        assert_eq!(
            classify_source_role("build/Makefile"),
            SourceRole::Configuration
        );
    }

    #[test]
    fn defaults_to_source() {
        assert_eq!(classify_source_role("src/lib.rs"), SourceRole::Source);
        assert_eq!(classify_source_role("main.py"), SourceRole::Source);
        assert_eq!(classify_source_role("app.ts"), SourceRole::Source);
    }

    #[test]
    fn precedence_generated_over_test() {
        // A file in a `generated` directory that also looks like a test
        // should be classified as generated.
        assert_eq!(
            classify_source_role("generated/foo_test.py"),
            SourceRole::Generated
        );
    }

    #[test]
    fn precedence_test_over_benchmark() {
        // A file in a `tests` directory with a benchmark-looking name
        // should be classified as test.
        assert_eq!(classify_source_role("tests/foo_bench.rs"), SourceRole::Test);
    }

    #[test]
    fn src_config_files_remain_source() {
        assert_eq!(classify_source_role("src/config.rs"), SourceRole::Source);
        assert_eq!(classify_source_role("src/config.py"), SourceRole::Source);
    }

    #[test]
    fn contest_and_latest_are_not_test_or_generated() {
        // `contest` and `latest` are not in the test or generated
        // component lists, so they should fall through to source.
        assert_eq!(
            classify_source_role("contest/solution.rs"),
            SourceRole::Source
        );
        assert_eq!(
            classify_source_role("latest/release.rs"),
            SourceRole::Source
        );
    }

    #[test]
    fn backslash_separators_are_normalized() {
        assert_eq!(classify_source_role(r"tests\foo.rs"), SourceRole::Test);
        assert_eq!(classify_source_role(r"src\lib.rs"), SourceRole::Source);
    }

    // -- GlobPattern -------------------------------------------------------

    #[test]
    fn glob_single_star_does_not_cross_separator() {
        let g = GlobPattern::compile("src/*.rs").unwrap();
        assert!(g.matches("src/lib.rs"));
        assert!(!g.matches("src/deep/lib.rs"));
    }

    #[test]
    fn glob_double_star_crosses_directories() {
        let g = GlobPattern::compile("src/**/*.rs").unwrap();
        assert!(g.matches("src/lib.rs"));
        assert!(g.matches("src/deep/lib.rs"));
        assert!(g.matches("src/a/b/c/lib.rs"));
    }

    #[test]
    fn glob_question_mark_matches_one_non_separator() {
        let g = GlobPattern::compile("src/?.rs").unwrap();
        assert!(g.matches("src/a.rs"));
        assert!(!g.matches("src/ab.rs"));
        assert!(!g.matches("src/a/b.rs"));
    }

    #[test]
    fn glob_is_case_sensitive() {
        let g = GlobPattern::compile("src/*.rs").unwrap();
        assert!(!g.matches("src/LIB.RS"));
    }

    #[test]
    fn glob_rejects_empty_pattern() {
        assert!(matches!(
            GlobPattern::compile(""),
            Err(FilterError::EmptyPattern)
        ));
        assert!(matches!(
            GlobPattern::compile("   "),
            Err(FilterError::EmptyPattern)
        ));
    }

    #[test]
    fn glob_rejects_backslash() {
        assert!(matches!(
            GlobPattern::compile(r"src\lib.rs"),
            Err(FilterError::BackslashInPattern { .. })
        ));
    }

    #[test]
    fn glob_rejects_absolute_unix() {
        assert!(matches!(
            GlobPattern::compile("/etc/passwd"),
            Err(FilterError::AbsolutePath { .. })
        ));
    }

    #[test]
    fn glob_rejects_absolute_windows() {
        assert!(matches!(
            GlobPattern::compile("C:/Users/foo"),
            Err(FilterError::AbsolutePath { .. })
        ));
    }

    #[test]
    fn glob_rejects_unc() {
        assert!(matches!(
            GlobPattern::compile("//server/share"),
            Err(FilterError::AbsolutePath { .. })
        ));
    }

    #[test]
    fn glob_rejects_traversal() {
        assert!(matches!(
            GlobPattern::compile("../etc/passwd"),
            Err(FilterError::Traversal { .. })
        ));
        assert!(matches!(
            GlobPattern::compile("src/../../etc"),
            Err(FilterError::Traversal { .. })
        ));
    }

    #[test]
    fn glob_rejects_malformed_pattern() {
        // An unbalanced bracket is not a valid glob.
        assert!(matches!(
            GlobPattern::compile("src/[a.rs"),
            Err(FilterError::MalformedPattern { .. })
        ));
    }

    // -- EffectiveFilter validation ----------------------------------------

    #[test]
    fn rejects_explicitly_empty_languages() {
        let req = FilterRequest {
            languages: Some(vec![]),
            ..Default::default()
        };
        assert!(matches!(
            EffectiveFilter::from_request(&req),
            Err(FilterError::EmptyLanguages)
        ));
    }

    #[test]
    fn rejects_explicitly_empty_source_roles() {
        let req = FilterRequest {
            source_roles: Some(vec![]),
            ..Default::default()
        };
        assert!(matches!(
            EffectiveFilter::from_request(&req),
            Err(FilterError::EmptySourceRoles)
        ));
    }

    #[test]
    fn rejects_unknown_language() {
        let req = FilterRequest {
            languages: Some(vec!["cobol".into()]),
            ..Default::default()
        };
        let error = EffectiveFilter::from_request(&req).unwrap_err();
        assert_eq!(
            error,
            FilterError::UnknownLanguage {
                value: "cobol".into()
            }
        );
    }

    #[test]
    fn rejects_unknown_source_role() {
        let req = FilterRequest {
            source_roles: Some(vec!["bogus".into()]),
            ..Default::default()
        };
        let error = EffectiveFilter::from_request(&req).unwrap_err();
        assert_eq!(
            error,
            FilterError::UnknownSourceRole {
                value: "bogus".into()
            }
        );
    }

    #[test]
    fn accepts_all_six_registered_languages() {
        for lang in ["rust", "typescript", "tsx", "javascript", "jsx", "python"] {
            let req = FilterRequest {
                languages: Some(vec![lang.into()]),
                ..Default::default()
            };
            let eff = EffectiveFilter::from_request(&req).unwrap();
            assert_eq!(
                eff.languages.as_deref(),
                Some(vec![lang.to_owned()].as_slice())
            );
        }
    }

    #[test]
    fn rejects_extension_as_language_identifier() {
        let req = FilterRequest {
            languages: Some(vec!["rs".into()]),
            ..Default::default()
        };
        assert!(matches!(
            EffectiveFilter::from_request(&req),
            Err(FilterError::UnknownLanguage { .. })
        ));
    }

    #[test]
    fn deduplicates_languages_deterministically() {
        let req = FilterRequest {
            languages: Some(vec![
                "python".into(),
                "rust".into(),
                "python".into(),
                "rust".into(),
            ]),
            ..Default::default()
        };
        let eff = EffectiveFilter::from_request(&req).unwrap();
        assert_eq!(
            eff.languages.as_deref(),
            Some(vec!["python".to_owned(), "rust".to_owned()].as_slice())
        );
    }

    #[test]
    fn deduplicates_source_roles_deterministically() {
        let req = FilterRequest {
            source_roles: Some(vec!["test".into(), "source".into(), "test".into()]),
            ..Default::default()
        };
        let eff = EffectiveFilter::from_request(&req).unwrap();
        assert_eq!(
            eff.source_roles.as_deref(),
            Some([SourceRole::Source, SourceRole::Test].as_slice())
        );
    }

    #[test]
    fn deduplicates_path_patterns() {
        let req = FilterRequest {
            include_paths: Some(vec![
                "src/*.rs".into(),
                "src/*.rs".into(),
                "lib/*.rs".into(),
            ]),
            ..Default::default()
        };
        let eff = EffectiveFilter::from_request(&req).unwrap();
        assert_eq!(
            eff.report().include_paths,
            Some(vec!["lib/*.rs".to_owned(), "src/*.rs".to_owned()])
        );
    }

    #[test]
    fn rejects_malformed_path_pattern() {
        let req = FilterRequest {
            include_paths: Some(vec!["".into()]),
            ..Default::default()
        };
        assert!(matches!(
            EffectiveFilter::from_request(&req),
            Err(FilterError::EmptyPattern)
        ));
    }

    #[test]
    fn rejects_traversal_in_path_pattern() {
        let req = FilterRequest {
            exclude_paths: Some(vec!["../secret".into()]),
            ..Default::default()
        };
        assert!(matches!(
            EffectiveFilter::from_request(&req),
            Err(FilterError::Traversal { .. })
        ));
    }

    // -- EffectiveFilter matching ------------------------------------------

    #[test]
    fn no_filters_matches_everything() {
        let eff = EffectiveFilter::from_request(&FilterRequest::default()).unwrap();
        assert!(eff.matches("rust", "src/lib.rs", SourceRole::Source));
        assert!(eff.matches("python", "tests/foo_test.py", SourceRole::Test));
    }

    #[test]
    fn language_filter_restricts() {
        let req = FilterRequest {
            languages: Some(vec!["rust".into()]),
            ..Default::default()
        };
        let eff = EffectiveFilter::from_request(&req).unwrap();
        assert!(eff.matches("rust", "src/lib.rs", SourceRole::Source));
        assert!(!eff.matches("python", "src/lib.py", SourceRole::Source));
    }

    #[test]
    fn source_role_filter_restricts() {
        let req = FilterRequest {
            source_roles: Some(vec!["test".into()]),
            ..Default::default()
        };
        let eff = EffectiveFilter::from_request(&req).unwrap();
        assert!(eff.matches("rust", "tests/foo.rs", SourceRole::Test));
        assert!(!eff.matches("rust", "src/lib.rs", SourceRole::Source));
    }

    #[test]
    fn include_then_exclude_precedence() {
        let req = FilterRequest {
            include_paths: Some(vec!["src/**/*.rs".into()]),
            exclude_paths: Some(vec!["src/generated/**".into()]),
            ..Default::default()
        };
        let eff = EffectiveFilter::from_request(&req).unwrap();
        // Included and not excluded.
        assert!(eff.matches("rust", "src/lib.rs", SourceRole::Source));
        // Included but excluded.
        assert!(!eff.matches("rust", "src/generated/foo.rs", SourceRole::Generated));
        // Not included.
        assert!(!eff.matches("rust", "lib/foo.rs", SourceRole::Source));
    }

    #[test]
    fn exclude_only_filters() {
        let req = FilterRequest {
            exclude_paths: Some(vec!["**/*.generated.rs".into()]),
            ..Default::default()
        };
        let eff = EffectiveFilter::from_request(&req).unwrap();
        assert!(eff.matches("rust", "src/lib.rs", SourceRole::Source));
        assert!(!eff.matches("rust", "src/foo.generated.rs", SourceRole::Generated));
    }

    // -- Report serialization ----------------------------------------------

    #[test]
    fn report_represents_absent_filters_as_null() {
        let req = FilterRequest {
            languages: Some(vec!["rust".into()]),
            ..Default::default()
        };
        let eff = EffectiveFilter::from_request(&req).unwrap();
        let report = eff.report();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["languages"], serde_json::json!(["rust"]));
        assert!(json["include_paths"].is_null());
        assert!(json["exclude_paths"].is_null());
        assert!(json["source_roles"].is_null());
    }

    #[test]
    fn report_serializes_all_filters_when_present() {
        let req = FilterRequest {
            languages: Some(vec!["rust".into(), "python".into()]),
            include_paths: Some(vec!["src/**/*.rs".into()]),
            exclude_paths: Some(vec!["**/generated/**".into()]),
            source_roles: Some(vec!["source".into(), "test".into()]),
        };
        let eff = EffectiveFilter::from_request(&req).unwrap();
        let report = eff.report();
        let json = serde_json::to_value(&report).unwrap();
        assert_eq!(json["languages"], serde_json::json!(["python", "rust"]));
        assert_eq!(json["include_paths"], serde_json::json!(["src/**/*.rs"]));
        assert_eq!(
            json["exclude_paths"],
            serde_json::json!(["**/generated/**"])
        );
        assert_eq!(json["source_roles"], serde_json::json!(["source", "test"]));
    }
}
