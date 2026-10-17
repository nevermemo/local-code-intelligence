//! C spec for the shared missing-server acceptance engine (`super::missing`).
//! See `missing.rs` for the behavior every language family shares. Shares
//! the `[clangd]` config section with `cpp_missing.rs`.

use super::missing::{MissingServerSpec, ProcessCheck};
use anyhow::Result;

const CALCULATOR_C: &str = "// Calculator performs basic arithmetic for acceptance testing.\ntypedef struct Calculator {\n    int placeholder;\n} Calculator;\n\n// add returns the sum of two integers.\nint add(Calculator c, int a, int b) {\n    return a + b;\n}\n";

// A `.h` header: `find_definition` on it must reach the (missing) clangd and
// fail with a tooling error naming it, not be rejected as an unrecognized file.
const COUNTER_H: &str = "// Counter tracks how many items an acceptance run has seen.\ntypedef struct Counter {\n    int value;\n} Counter;\n\n// counter_next returns the value that follows current.\nint counter_next(int current);\n";

const LIB_RS: &str = r"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}
";

pub async fn run() -> Result<()> {
    super::missing::run(MissingServerSpec {
        language_key: "c",
        config_section: "clangd",
        display_name: "C",
        missing_path: "definitely-missing-clangd",
        extra_config_lines: vec![],
        source_files: vec![
            ("calculator.c", CALCULATOR_C),
            ("counter.h", COUNTER_H),
            ("src/lib.rs", LIB_RS),
        ],
        search_query: "Calculator add",
        search_languages: &["c"],
        definition_file: "counter.h",
        definition_line: 7,
        definition_character: 4,
        // `near_word` uses plain substring matching (see missing.rs's
        // `contains_near`/`names_provider`), so the bare single-letter "c"
        // every other language's convention would suggest is unusably
        // broad -- it would match almost any English text. "clangd" (the
        // real provider name `App::search`'s warning message actually
        // contains) is precise and still real, not a workaround.
        exact_provider_terms: &["clangd", "c/c++ language server"],
        near_word: "clangd",
        extra_terms: &["clangd"],
        process_check: ProcessCheck::Children("clangd"),
    })
    .await
}
