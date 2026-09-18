//! Java spec for the shared persistent-reuse/recovery acceptance engine
//! (`super::recovery`). See `recovery.rs` for the behavior every language
//! family shares. `java` is a native executable spawned directly as LCI's
//! child (like csharp-ls/gopls, not through a wrapper -- see `config.rs`'s
//! `JavaLspConfig` doc comment for why), so `ProcessCheck::SingleChild`
//! applies. `NavigationProbe::Definition` is used rather than `Symbols`:
//! jdtls's cold-start indexing is slow enough that `search_symbols`
//! immediately after a fresh restart is a less reliable recovery signal
//! than definition resolution, which `JavaServer::before_position_request`
//! primes with a `textDocument/didOpen` first (same rationale as Python's
//! recovery spec).

use super::recovery::{FixtureScaffold, NavigationProbe, ProcessCheck, RecoverySpec};
use anyhow::Result;
use std::path::PathBuf;

const WINDOWS_JDTLS_FALLBACK: &str = r"C:\Users\micro\tools\jdtls";

const CALCULATOR_JAVA: &str = "package acceptance;\n\npublic class Calculator {\n    public int add(int a, int b) {\n        return a + b;\n    }\n}\n";

const CALLSITE_JAVA: &str = "package acceptance;\n\npublic class CallSite {\n    public int run(Calculator calculator) {\n        return calculator.add(1, 2);\n    }\n}\n";

pub async fn run(java_dir: Option<PathBuf>) -> Result<()> {
    super::recovery::run(
        RecoverySpec {
            language_key: "java",
            display_name: "Java",
            which_name: "jdtls",
            cli_flag_display: "java",
            windows_fallback: Some(WINDOWS_JDTLS_FALLBACK),
            requires_tool: None,
            scaffold: FixtureScaffold::None,
            source_files: vec![
                (".project", super::java_lsp::DOT_PROJECT),
                (".classpath", super::java_lsp::DOT_CLASSPATH),
                ("src/Calculator.java", CALCULATOR_JAVA),
                ("src/CallSite.java", CALLSITE_JAVA),
            ],
            config_section: "java",
            extra_config_lines: vec![],
            lsp_timeout_seconds: 90,
            attempts: 30,
            probe: NavigationProbe::Definition {
                relative_file_path: "src/CallSite.java",
                line: 5,
                character: 26,
                expect_substring: "Calculator.java",
            },
            process_check: ProcessCheck::SingleChild("java"),
            process_label: "java",
        },
        java_dir,
    )
    .await
}
