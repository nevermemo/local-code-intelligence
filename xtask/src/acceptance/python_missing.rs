//! Python spec for the shared missing-server acceptance engine
//! (`super::missing`). See `missing.rs` for the behavior every language
//! family shares.
//!
//! `extra_terms: &["pyright"]` because `App::search` reports LSP provider
//! failures as `"Optional LSP provider unavailable: {}"` with
//! `adapter.provider()` (`"pyright"`) leading each joined failure message --
//! the tooling-error and service-status checks need to recognize that name
//! too, not just the bare "python" language word.

use super::missing::{MissingServerSpec, ProcessCheck};
use anyhow::Result;

const CALCULATOR_PY: &str = r#"class Calculator:
    def add(self, a, b):
        return a + b
"#;

const LIB_RS: &str = r#"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}
"#;

pub async fn run() -> Result<()> {
    super::missing::run(MissingServerSpec {
        language_key: "python",
        config_section: "python",
        display_name: "Python",
        missing_path: "definitely-missing-pyright-langserver",
        extra_config_lines: vec![],
        source_files: vec![
            ("src/__init__.py", ""),
            ("src/calculator.py", CALCULATOR_PY),
            ("src/lib.rs", LIB_RS),
        ],
        search_query: "Calculator add",
        search_languages: &["python"],
        definition_file: "src/calculator.py",
        definition_line: 2,
        definition_character: 8,
        exact_provider_terms: &["pyright", "python language server"],
        near_word: "python",
        extra_terms: &["pyright"],
        process_check: ProcessCheck::Descendants("node"),
    })
    .await
}
