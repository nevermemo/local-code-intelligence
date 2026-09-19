//! Java spec for the shared missing-server acceptance engine
//! (`super::missing`). See `missing.rs` for the behavior every language
//! family shares.
//!
//! `missing_path` simulates a missing/nonexistent jdtls *installation
//! directory* (`[java].path` -- see `java_lsp.rs`'s module doc): with no
//! `plugins/` directory to find a launcher jar in, `JavaServer::
//! transport_config` returns `None`, the same "disabled/unavailable" shape
//! every other adapter's missing-tool case produces. `java` is a direct
//! child process (no wrapper hop -- see `config.rs`'s `JavaLspConfig` doc
//! comment for why), so `ProcessCheck::Children` applies, not `Descendants`.

use super::missing::{MissingServerSpec, ProcessCheck};
use anyhow::Result;

const CALCULATOR_JAVA: &str = "package acceptance;\n\npublic class Calculator {\n    public int add(int a, int b) {\n        return a + b;\n    }\n}\n";

const LIB_RS: &str = r#"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}
"#;

pub async fn run() -> Result<()> {
    super::missing::run(MissingServerSpec {
        language_key: "java",
        config_section: "java",
        display_name: "Java",
        missing_path: "C:/definitely-missing-jdtls-install",
        extra_config_lines: vec![],
        source_files: vec![
            (".project", super::java_lsp::DOT_PROJECT),
            (".classpath", super::java_lsp::DOT_CLASSPATH),
            ("src/Calculator.java", CALCULATOR_JAVA),
            ("src/lib.rs", LIB_RS),
        ],
        search_query: "Calculator add",
        search_languages: &["java"],
        definition_file: "src/Calculator.java",
        definition_line: 4,
        definition_character: 15,
        exact_provider_terms: &["jdtls", "java language server"],
        near_word: "java",
        extra_terms: &["jdtls"],
        process_check: ProcessCheck::Children("java"),
    })
    .await
}
