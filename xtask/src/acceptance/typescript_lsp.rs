//! TypeScript/JavaScript spec for the shared positive-path LSP acceptance
//! engine (`super::lsp_full`). Ported in spirit from `csharp_lsp/full_flow.rs`,
//! adapted for the real `typescript-language-server` adapter.
//!
//! `workspace/symbol` (the `symbols` CLI command) is deliberately NOT part of
//! the pass/fail contract here: this application never writes a
//! `tsconfig.json`-driven "open every file" step into a user's repository,
//! and a fresh, one-shot CLI invocation's typescript-language-server process
//! has no file open yet when `symbols` asks it for `workspace/symbol` — a
//! documented, accepted limitation (see `src/lsp/typescript.rs`), not a bug.
//! `find_definition`/`find_references` open their target file first and are
//! the reliable, verified-working proof this acceptance is built around.

use super::lsp_full::{IndexedFilesAssertion, LspFullFlowSpec, SearchAssertion, SymbolsStage};
use super::recovery::FixtureScaffold;
use anyhow::Result;
use std::path::PathBuf;

const TSCONFIG: &str = r#"{
  "compilerOptions": {
    "target": "es2020",
    "module": "commonjs"
  }
}
"#;

const CALCULATOR_SOURCE: &str = "// Production billing arithmetic implementation.\nexport class Calculator {\n  add(a: number, b: number): number {\n    return a + b;\n  }\n}\n";

const CALLSITE_SOURCE: &str = "import { Calculator } from './Calculator';\n\nexport function run(value: Calculator): number {\n  return value.add(1, 2);\n}\n";

const TEST_SOURCE: &str = "import { Calculator } from '../src/Calculator';\n\n// Test-only Calculator usage and documentation example.\nexport function example(): boolean {\n  return new Calculator().add(1, 2) === 3;\n}\n";

pub async fn run(typescript_language_server: Option<PathBuf>) -> Result<()> {
    super::lsp_full::run(
        LspFullFlowSpec {
            language_key: "typescript",
            display_name: "TypeScript",
            which_name: "typescript-language-server",
            cli_flag_display: "typescript-language-server",
            windows_fallback: None,
            version_arg: Some("--version"),
            // Written before `npm install`/`index` so both npm and the
            // indexer skip the tens of thousands of files a real
            // `typescript` install places under node_modules/ -- without
            // this the indexer overwhelms the local embedding service
            // trying to embed all of it.
            scaffold: FixtureScaffold::Npm {
                install: &["typescript@5.7.3"],
            },
            source_files: vec![
                (".gitignore", "node_modules/\n"),
                ("tsconfig.json", TSCONFIG),
                ("src/Calculator.ts", CALCULATOR_SOURCE),
                ("src/CallSite.ts", CALLSITE_SOURCE),
                ("tests/Calculator.test.ts", TEST_SOURCE),
            ],
            config_section: "typescript",
            // Generous: on top of real spawn/init time, the adapter's
            // before_position_request gives tsserver a bounded ~3s head
            // start per opened file to resolve imports asynchronously.
            lsp_timeout_seconds: 30,
            extra_config_lines: vec![],
            expected_indexed_files: IndexedFilesAssertion::Exact(3),
            symbols_stage: SymbolsStage::Informational {
                query: "Calculator",
            },
            call_site_file: "src/CallSite.ts",
            call_site_source: CALLSITE_SOURCE,
            call_site_line: 4,
            call_site_needle: "add",
            declaration_file: "src/Calculator.ts",
            declaration_source: CALCULATOR_SOURCE,
            declaration_line: 3,
            declaration_needle: "add",
            search_query: "production billing arithmetic Calculator add implementation",
            search_assertion: SearchAssertion::RankedWithScores {
                test_file: "tests/Calculator.test.ts",
                assert_lsp_channel: false,
            },
            language_identifier: "typescript",
            provider_name: "typescript-language-server",
            generated_dirs: &["node_modules", "dist", "build"],
        },
        typescript_language_server,
    )
    .await
}
