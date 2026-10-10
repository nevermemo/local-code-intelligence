use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::language;
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
    // Example/documentation/configuration are repository-layout
    // conventions. Directories below a JVM source root are package names
    // (`com/example`, `org/acme/config`), not layout, so they don't count.
    let layout_dirs = layout_components(&dir_components);
    if is_example(layout_dirs, file_name) {
        return SourceRole::Example;
    }
    if is_documentation(layout_dirs) {
        return SourceRole::Documentation;
    }
    if is_configuration(layout_dirs, file_name) {
        return SourceRole::Configuration;
    }
    SourceRole::Source
}

/// Directory components that describe repository layout: all of them,
/// except that in a Maven/Gradle tree (`src/<sourceSet>/<java|kotlin|scala|
/// groovy>/...`) the components after the language root are package names.
fn layout_components<'a>(dirs: &'a [&'a str]) -> &'a [&'a str] {
    let jvm_root = dirs.windows(3).position(|window| {
        window[0] == "src" && matches!(window[2], "java" | "kotlin" | "scala" | "groovy")
    });
    match jvm_root {
        Some(start) => &dirs[..start + 3],
        None => dirs,
    }
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
    // `androidTest`, `integrationTest`, and `testFixtures` are Gradle/Android
    // test source sets (`app/src/androidTest/java/...`).
    if dirs.iter().any(|c| {
        matches!(
            *c,
            "test" | "tests" | "__tests__" | "androidTest" | "integrationTest" | "testFixtures"
        )
    }) {
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
    // Go: `*_test.go` is the only test file convention, and such files sit
    // next to the production code they test.
    if file_name.ends_with("_test.go") {
        return true;
    }
    // Java: JUnit/TestNG `*Test`, `*Tests`, `*TestCase`, and Failsafe
    // integration tests `*IT` (case-sensitive, so `Latest.java` or
    // `Kit.java` stay source; an all-caps `GIT.java` is not `*IT`).
    if let Some(stem) = file_name.strip_suffix(".java") {
        if stem.ends_with("Test") || stem.ends_with("Tests") || stem.ends_with("TestCase") {
            return true;
        }
        if let Some(prefix) = stem.strip_suffix("IT")
            && prefix
                .chars()
                .last()
                .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        {
            return true;
        }
    }
    // C/C++: `*_test`, `*_tests`, `*_unittest(s)` (googletest/Chromium), and
    // `test_*` (Unity/CMocka) for the registered `.c`/`.cpp`/`.hpp` files.
    if let Some(stem) = [".c", ".cpp", ".hpp"]
        .iter()
        .find_map(|extension| file_name.strip_suffix(extension))
        && (["_test", "_tests", "_unittest", "_unittests"]
            .iter()
            .any(|suffix| stem.ends_with(suffix))
            || stem.starts_with("test_"))
    {
        return true;
    }
    // Rust: `tests.rs` (the out-of-line `#[cfg(test)] mod tests;`),
    // `test_*.rs`, `*_test.rs`, `*_tests.rs`. Integration tests under any
    // `tests/` directory are already caught by the directory rule, and
    // `benches/` / `examples/` map to the benchmark / example roles.
    //
    // A bare `test.rs` deliberately stays `source`: unlike `tests.rs` it has
    // no test-module convention, and it is a legitimate production module
    // name (this repository's `xtask/src/test.rs` implements the
    // `cargo xtask test` command). A `test.rs` inside a `test/` or `tests/`
    // directory is still a test through the directory rule. Test code in an
    // inline `#[cfg(test)] mod tests { .. }` block inside a source file
    // cannot be classified from the path; that is chunk-level information,
    // not a file role.
    if file_name.strip_suffix(".rs").is_some_and(|stem| {
        stem == "tests"
            || stem.starts_with("test_")
            || stem.ends_with("_test")
            || stem.ends_with("_tests")
    }) {
        return true;
    }
    if file_name.strip_suffix(".cs").is_some_and(|stripped| {
        stripped.ends_with("Tests") || stripped.ends_with("Test") || stripped.ends_with(".Tests")
    }) {
        return true;
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

fn is_configuration(dirs: &[&str], file_name: &str) -> bool {
    // Tooling directories hold build/maintenance code, whatever its language.
    if dirs
        .iter()
        .any(|c| matches!(*c, "scripts" | "tools" | "build"))
    {
        return true;
    }
    // A `config/` directory marks configuration *data*. Source code there --
    // `internal/config/loader.go`, `src/config/index.ts` -- is ordinary
    // production code that loads configuration, so it stays `source`.
    !is_supported_source_file_name(file_name)
        && dirs
            .iter()
            .any(|c| matches!(*c, "config" | "configs" | "configuration"))
}

// ---------------------------------------------------------------------------
// Path normalization
// ---------------------------------------------------------------------------

/// Normalize a repository-relative path to use `/` separators.
///
/// Backslashes are converted to forward slashes. Leading and trailing
/// separators are trimmed. Repeated separators are collapsed.
pub(super) fn normalize_path(path: &str) -> String {
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
