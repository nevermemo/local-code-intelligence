//! C spec for the shared persistent-reuse/recovery acceptance engine
//! (`super::recovery`). See `recovery.rs` for the behavior every language
//! family shares. `clangd` is a native executable spawned directly as LCI's
//! child (like csharp-ls/gopls, not through an npm/node wrapper), so
//! `ProcessCheck::SingleChild` applies. Shares the `[clangd]` config section
//! with `cpp_recovery.rs`.

use super::recovery::{FixtureScaffold, NavigationProbe, ProcessCheck, RecoverySpec};
use anyhow::Result;
use std::path::PathBuf;

const CALCULATOR_C: &str = "// Calculator performs basic arithmetic for acceptance testing.\ntypedef struct Calculator {\n    int placeholder;\n} Calculator;\n\n// add returns the sum of two integers.\nint add(Calculator c, int a, int b) {\n    return a + b;\n}\n";

const CALLSITE_C: &str = "#include \"calculator.c\"\n\nint run(void) {\n    Calculator c;\n    return add(c, 1, 2);\n}\n";

// The recovery probe navigates from a `.c` call site into a `.h` header.
const COUNTER_H: &str = "// Counter tracks how many items an acceptance run has seen.\ntypedef struct Counter {\n    int value;\n} Counter;\n\n// counter_next returns the value that follows current.\nint counter_next(int current);\n";

const USE_COUNTER_C: &str = "#include \"counter.h\"\n\nint use_counter(int current) {\n    return counter_next(current);\n}\n";

pub async fn run(clangd: Option<PathBuf>) -> Result<()> {
    super::recovery::run(
        RecoverySpec {
            language_key: "c",
            display_name: "C",
            which_name: "clangd",
            cli_flag_display: "clangd",
            fallback_env: None,
            requires_tool: None,
            scaffold: FixtureScaffold::None,
            source_files: vec![
                ("calculator.c", CALCULATOR_C),
                ("callsite.c", CALLSITE_C),
                ("counter.h", COUNTER_H),
                ("use_counter.c", USE_COUNTER_C),
            ],
            config_section: "clangd",
            extra_config_lines: vec![],
            lsp_timeout_seconds: 30,
            attempts: 30,
            probe: NavigationProbe::Definition {
                relative_file_path: "use_counter.c",
                line: 4,
                character: 11,
                expect_substring: "counter.h",
            },
            process_check: ProcessCheck::SingleChild("clangd"),
            process_label: "clangd",
        },
        clangd,
    )
    .await
}
