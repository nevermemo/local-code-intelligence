//! C++ spec for the shared persistent-reuse/recovery acceptance engine
//! (`super::recovery`). See `recovery.rs` for the behavior every language
//! family shares. `clangd` is a native executable spawned directly as LCI's
//! child, so `ProcessCheck::SingleChild` applies. Shares the `[clangd]`
//! config section with `c_recovery.rs`.

use super::recovery::{FixtureScaffold, NavigationProbe, ProcessCheck, RecoverySpec};
use anyhow::Result;
use std::path::PathBuf;

const CALCULATOR_CPP: &str = "namespace acceptance {\n// Calculator performs basic arithmetic for acceptance testing.\nclass Calculator {\npublic:\n    // add returns the sum of two integers.\n    int add(int a, int b) { return a + b; }\n};\n}\n";

const CALLSITE_CPP: &str = "#include \"calculator.cpp\"\n\nint run() {\n    acceptance::Calculator c;\n    return c.add(1, 2);\n}\n";

pub async fn run(clangd: Option<PathBuf>) -> Result<()> {
    super::recovery::run(
        RecoverySpec {
            language_key: "cpp",
            display_name: "C++",
            which_name: "clangd",
            cli_flag_display: "clangd",
            windows_fallback: None,
            requires_tool: None,
            scaffold: FixtureScaffold::None,
            source_files: vec![
                ("calculator.cpp", CALCULATOR_CPP),
                ("callsite.cpp", CALLSITE_CPP),
            ],
            config_section: "clangd",
            extra_config_lines: vec![],
            lsp_timeout_seconds: 30,
            attempts: 30,
            probe: NavigationProbe::Definition {
                relative_file_path: "callsite.cpp",
                line: 5,
                character: 13,
                expect_substring: "calculator.cpp",
            },
            process_check: ProcessCheck::SingleChild("clangd"),
            process_label: "clangd",
        },
        clangd,
    )
    .await
}
