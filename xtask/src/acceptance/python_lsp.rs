//! Python spec for the shared positive-path LSP acceptance engine
//! (`super::lsp_full`): definition/references resolution (the reliable,
//! verified-working navigation path) plus semantic search, against a real
//! Python package fixture.
//!
//! `search_symbols` (workspace/symbol) is not run at all here: against a
//! freshly-spawned pyright that has never had any file opened, it was
//! observed to return an empty result rather than a hard error, unlike
//! `find_definition`/`find_references` which reliably resolve real
//! cross-file references once the target file has been opened (handled by
//! `PythonServer::before_position_request` in `src/lsp/python.rs`).

use super::lsp_full::{IndexedFilesAssertion, LspFullFlowSpec, SearchAssertion, SymbolsStage};
use super::recovery::FixtureScaffold;
use anyhow::Result;
use std::path::PathBuf;

const INIT_PY: &str = "";

const CALCULATOR_PY: &str = r#"class Calculator:
    def add(self, a, b):
        return a + b
"#;

const CALL_SITE_PY: &str = "from .calculator import Calculator\n\n\ndef run(value: Calculator) -> int:\n    return value.add(1, 2)\n";

pub async fn run(pyright: Option<PathBuf>) -> Result<()> {
    super::lsp_full::run(
        LspFullFlowSpec {
            language_key: "python",
            display_name: "Python",
            which_name: "pyright-langserver",
            cli_flag_display: "pyright",
            windows_fallback: None,
            // Unlike csharp-ls/typescript-language-server,
            // pyright-langserver has no standalone `--version`/`--help`
            // output: any argument other than a transport flag
            // (`--stdio`/`--node-ipc`/`--socket`) makes it print a
            // connection error and exit immediately.
            version_arg: None,
            scaffold: FixtureScaffold::None,
            // A proper Python package (`__init__.py` present) is required
            // for the relative import in call_site.py to resolve at all;
            // cross-file member resolution additionally needs the explicit
            // `value: Calculator` annotation below (pyright cannot infer
            // types across a function boundary without one).
            source_files: vec![
                ("src/__init__.py", INIT_PY),
                ("src/calculator.py", CALCULATOR_PY),
                ("src/call_site.py", CALL_SITE_PY),
            ],
            config_section: "python",
            lsp_timeout_seconds: 30,
            extra_config_lines: vec![],
            expected_indexed_files: IndexedFilesAssertion::AtLeast(2),
            symbols_stage: SymbolsStage::Skipped,
            call_site_file: "src/call_site.py",
            call_site_source: CALL_SITE_PY,
            call_site_line: 5,
            call_site_needle: "add",
            declaration_file: "src/calculator.py",
            declaration_source: CALCULATOR_PY,
            declaration_line: 2,
            declaration_needle: "add",
            search_query: "calculator add two numbers",
            search_assertion: SearchAssertion::ContainsFile,
            language_identifier: "python",
            provider_name: "pyright",
            generated_dirs: &["bin", "obj"],
            cross_file_references: true,
        },
        pyright,
    )
    .await
}
