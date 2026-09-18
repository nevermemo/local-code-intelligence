//! Go spec for the shared persistent-reuse/recovery acceptance engine
//! (`super::recovery`). See `recovery.rs` for the behavior every language
//! family shares. `gopls` is a native executable spawned directly as LCI's
//! child (like csharp-ls, not through an npm/node wrapper), so
//! `ProcessCheck::SingleChild` applies, same as C#.

use super::recovery::{FixtureScaffold, NavigationProbe, ProcessCheck, RecoverySpec};
use anyhow::Result;
use std::path::PathBuf;

const CALCULATOR_GO: &str = "package acceptance\n\n// Calculator performs basic arithmetic for acceptance testing.\ntype Calculator struct{}\n\n// Add returns the sum of two integers.\nfunc (c Calculator) Add(a, b int) int {\n\treturn a + b\n}\n";

const CALLSITE_GO: &str = "package acceptance\n\n// Run exercises Calculator.Add from a separate file.\nfunc Run(c Calculator) int {\n\treturn c.Add(1, 2)\n}\n";

pub async fn run(gopls: Option<PathBuf>) -> Result<()> {
    super::recovery::run(
        RecoverySpec {
            language_key: "go",
            display_name: "Go",
            which_name: "gopls",
            cli_flag_display: "gopls",
            windows_fallback: None,
            requires_tool: None,
            scaffold: FixtureScaffold::None,
            source_files: vec![
                ("go.mod", "module acceptance\n\ngo 1.21\n"),
                ("calculator.go", CALCULATOR_GO),
                ("callsite.go", CALLSITE_GO),
            ],
            config_section: "go",
            extra_config_lines: vec![],
            lsp_timeout_seconds: 30,
            attempts: 30,
            probe: NavigationProbe::Definition {
                relative_file_path: "callsite.go",
                line: 5,
                character: 10,
                expect_substring: "calculator.go",
            },
            process_check: ProcessCheck::SingleChild("gopls"),
            process_label: "gopls",
        },
        gopls,
    )
    .await
}
