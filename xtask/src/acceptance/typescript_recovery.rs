//! TypeScript/JavaScript spec for the shared persistent-reuse/recovery
//! acceptance engine (`super::recovery`). See `recovery.rs` for the behavior
//! every language family shares.
//!
//! `find_definition` (not `search_symbols`) is the probe: workspace/symbol
//! search is a documented limitation for TypeScript since LCI never writes
//! a `tsconfig.json`/project file into a user's repository, so a freshly
//! spawned server has no project context until a file is opened.
//!
//! On Windows, a real `typescript-language-server` invoked via its npm
//! `.cmd` shim spawns as
//! `local-code-intelligence.exe -> cmd.exe -> node.exe [-> node.exe]`: the
//! real server process (and tsserver's own child) are grandchildren or
//! deeper, not a direct child, so this uses `ProcessCheck::DescendantSet`
//! (recursive, any depth) rather than `SingleChild`.

use super::recovery::{FixtureScaffold, NavigationProbe, ProcessCheck, RecoverySpec};
use anyhow::Result;
use std::path::PathBuf;

const TSCONFIG: &str = r#"{
  "compilerOptions": {
    "target": "es2020",
    "module": "commonjs"
  }
}
"#;

const CALCULATOR_SOURCE: &str = "export class Calculator {\n  add(a: number, b: number): number {\n    return a + b;\n  }\n\n  use(): number {\n    return this.add(2, 3);\n  }\n}\n";

const CALLSITE_SOURCE: &str = "import { Calculator } from './Calculator';\n\nexport function run(value: Calculator): number {\n  return value.add(1, 2);\n}\n";

pub async fn run(typescript_language_server: Option<PathBuf>) -> Result<()> {
    super::recovery::run(
        RecoverySpec {
            language_key: "typescript",
            display_name: "TypeScript",
            which_name: "typescript-language-server",
            cli_flag_display: "typescript-language-server",
            windows_fallback: None,
            requires_tool: Some("npm"),
            // Written before `npm install`/indexing so both npm and the
            // indexer skip the tens of thousands of files a real
            // `typescript` install places under node_modules/.
            scaffold: FixtureScaffold::Npm {
                install: &["typescript@5.7.3"],
            },
            source_files: vec![
                (".gitignore", "node_modules/\n"),
                ("tsconfig.json", TSCONFIG),
                ("src/Calculator.ts", CALCULATOR_SOURCE),
                ("src/CallSite.ts", CALLSITE_SOURCE),
            ],
            config_section: "typescript",
            extra_config_lines: vec![],
            lsp_timeout_seconds: 60,
            attempts: 40,
            probe: NavigationProbe::Definition {
                relative_file_path: "src/CallSite.ts",
                line: 4,
                character: 15,
                expect_substring: "calculator.ts",
            },
            process_check: ProcessCheck::DescendantSet("node"),
            process_label: "node descendant",
        },
        typescript_language_server,
    )
    .await
}
