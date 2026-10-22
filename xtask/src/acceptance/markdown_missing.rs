//! Markdown spec for the shared missing-server acceptance engine
//! (`super::missing`). See `missing.rs` for the behavior every language
//! family shares. A search does not report a missing marksman (it does not
//! join the search's LSP channel); the service status, the navigation error
//! and Rust isolation are checked as for every server.

use super::missing::{MissingServerSpec, ProcessCheck};
use anyhow::Result;

const CALCULATOR_MD: &str = "# Calculator guide\n\n## Adding numbers\n\nThe calculator add function returns the sum of two integers.\n";

const LIB_RS: &str = r"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}
";

pub async fn run() -> Result<()> {
    super::missing::run(MissingServerSpec {
        language_key: "markdown",
        config_section: "markdown",
        display_name: "Markdown",
        missing_path: "definitely-missing-marksman",
        // Prose is not indexed by default; the navigation and status checks
        // need the document indexed. The lines land under `[markdown]`.
        extra_config_lines: vec!["[index]".to_string(), "prose = true".to_string()],
        source_files: vec![
            ("docs/calculator.md", CALCULATOR_MD),
            ("src/lib.rs", LIB_RS),
        ],
        search_query: "Calculator adding numbers",
        search_languages: &["markdown"],
        definition_file: "docs/calculator.md",
        definition_line: 3,
        definition_character: 4,
        exact_provider_terms: &["marksman", "markdown language server"],
        near_word: "markdown",
        extra_terms: &["marksman"],
        process_check: ProcessCheck::Children("marksman"),
    })
    .await
}
