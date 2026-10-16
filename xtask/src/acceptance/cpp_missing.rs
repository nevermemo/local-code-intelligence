//! C++ spec for the shared missing-server acceptance engine
//! (`super::missing`). See `missing.rs` for the behavior every language
//! family shares. Shares the `[clangd]` config section with `c_missing.rs`.

use super::missing::{MissingServerSpec, ProcessCheck};
use anyhow::Result;

const CALCULATOR_CPP: &str = "namespace acceptance {\n// Calculator performs basic arithmetic for acceptance testing.\nclass Calculator {\npublic:\n    // add returns the sum of two integers.\n    int add(int a, int b) { return a + b; }\n};\n}\n";

// A `.hxx` header: `find_definition` on it must reach the (missing) clangd and
// fail with a tooling error naming it, not be rejected as an unrecognized file.
const TALLY_HXX: &str = "namespace acceptance {\n// Tally counts acceptance steps.\nclass Tally {\npublic:\n    // bump increments the tally.\n    int bump(int n) { return n + 1; }\n};\n}\n";

const LIB_RS: &str = r#"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}
"#;

pub async fn run() -> Result<()> {
    super::missing::run(MissingServerSpec {
        language_key: "cpp",
        config_section: "clangd",
        display_name: "C++",
        missing_path: "definitely-missing-clangd",
        extra_config_lines: vec![],
        source_files: vec![
            ("calculator.cpp", CALCULATOR_CPP),
            ("tally.hxx", TALLY_HXX),
            ("src/lib.rs", LIB_RS),
        ],
        search_query: "Calculator add",
        search_languages: &["cpp"],
        definition_file: "tally.hxx",
        definition_line: 6,
        definition_character: 8,
        // See c_missing.rs for why this uses the real provider name rather
        // than a bare language word: `near_word`/`names_provider` do plain
        // substring matching, and while "cpp" is less collision-prone than
        // C's single-letter "c", "clangd" is still the precise, real term
        // that actually appears in `App::search`'s warning message.
        exact_provider_terms: &["clangd", "c/c++ language server"],
        near_word: "clangd",
        extra_terms: &["clangd"],
        process_check: ProcessCheck::Children("clangd"),
    })
    .await
}
