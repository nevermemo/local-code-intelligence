//! Python spec for the shared persistent-reuse/recovery acceptance engine
//! (`super::recovery`). See `recovery.rs` for the behavior every language
//! family shares; ported from the C# family's `csharp_recovery.rs`.
//!
//! `find_definition`/`find_references` are used here rather than
//! `search_symbols`: against a freshly-spawned pyright that has never had a
//! file opened, `search_symbols` (workspace/symbol) was observed to return
//! an empty result rather than exercising real navigation, whereas
//! `find_definition` reliably drives a real cross-file resolution once
//! `PythonServer::before_position_request` (src/lsp/python.rs) opens the
//! target file.
//!
//! Unlike csharp-ls (a native executable spawned directly as LCI's child),
//! `pyright-langserver` is an npm `.cmd` shim that, on Windows, spawns as
//! `local-code-intelligence.exe -> cmd.exe -> node.exe -> node.exe`, so the
//! real server process is a grandchild or deeper --
//! `ProcessCheck::DescendantSet` (recursive, any depth) is used instead of
//! `SingleChild` for every "did a child process spawn" check, and the
//! reuse/recovery checks compare *sets* of PIDs rather than a single PID
//! since more than one `node` descendant can be present at once. No
//! `requires_tool` prerequisite is needed: unlike TypeScript, this fixture
//! is plain `.py` source with no package-manager scaffolding step.

use super::recovery::{FixtureScaffold, NavigationProbe, ProcessCheck, RecoverySpec};
use anyhow::Result;
use std::path::PathBuf;

const CALCULATOR_PY: &str = r#"class Calculator:
    def add(self, a, b):
        return a + b
"#;

const CALL_SITE_PY: &str = "from .calculator import Calculator\n\n\ndef run(value: Calculator) -> int:\n    return value.add(1, 2)\n";

pub async fn run(pyright: Option<PathBuf>) -> Result<()> {
    super::recovery::run(
        RecoverySpec {
            language_key: "python",
            display_name: "Python",
            which_name: "pyright-langserver",
            cli_flag_display: "pyright",
            fallback_env: None,
            requires_tool: None,
            scaffold: FixtureScaffold::None,
            source_files: vec![
                ("src/__init__.py", ""),
                ("src/calculator.py", CALCULATOR_PY),
                ("src/call_site.py", CALL_SITE_PY),
            ],
            config_section: "python",
            extra_config_lines: vec![],
            lsp_timeout_seconds: 30,
            attempts: 30,
            probe: NavigationProbe::Definition {
                relative_file_path: "src/call_site.py",
                line: 5,
                character: 17,
                expect_substring: "calculator.py",
            },
            process_check: ProcessCheck::DescendantSet("node"),
            process_label: "node descendant",
        },
        pyright,
    )
    .await
}
