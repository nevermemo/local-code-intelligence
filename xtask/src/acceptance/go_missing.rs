//! Go spec for the shared missing-server acceptance engine (`super::missing`).
//! See `missing.rs` for the behavior every language family shares.

use super::missing::{MissingServerSpec, ProcessCheck};
use anyhow::Result;

const CALCULATOR_GO: &str = "package acceptance\n\n// Calculator performs basic arithmetic for acceptance testing.\ntype Calculator struct{}\n\n// Add returns the sum of two integers.\nfunc (c Calculator) Add(a, b int) int {\n\treturn a + b\n}\n";

const LIB_RS: &str = r#"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}
"#;

pub async fn run() -> Result<()> {
    super::missing::run(MissingServerSpec {
        language_key: "go",
        display_name: "Go",
        missing_path: "definitely-missing-gopls",
        extra_config_lines: vec![],
        source_files: vec![
            ("go.mod", "module acceptance\n\ngo 1.21\n"),
            ("calculator.go", CALCULATOR_GO),
            ("src/lib.rs", LIB_RS),
        ],
        search_query: "Calculator Add",
        search_languages: &["go"],
        definition_file: "calculator.go",
        definition_line: 7,
        definition_character: 20,
        exact_provider_terms: &["gopls", "go language server"],
        near_word: "go",
        extra_terms: &["gopls"],
        process_check: ProcessCheck::Children("gopls"),
    })
    .await
}
