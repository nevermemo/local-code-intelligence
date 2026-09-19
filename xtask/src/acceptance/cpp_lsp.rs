//! C++ spec for the shared positive-path LSP acceptance engine
//! (`super::lsp_full`), against a real standalone C++ fixture (no build
//! system) and a real installed `clangd`. Shares the `[clangd]` config
//! section with `c_lsp.rs` -- one clangd instance navigates both `c` and
//! `cpp` -- but uses its own `language_key` ("cpp") so its fixture and
//! evidence file stay distinct from C's.

use super::lsp_full::{IndexedFilesAssertion, LspFullFlowSpec, SearchAssertion, SymbolsStage};
use super::recovery::FixtureScaffold;
use anyhow::Result;
use std::path::PathBuf;

const CALCULATOR_CPP: &str = "namespace acceptance {\n// Calculator performs basic arithmetic for acceptance testing.\nclass Calculator {\npublic:\n    // add returns the sum of two integers.\n    int add(int a, int b) { return a + b; }\n};\n}\n";

const CALLSITE_CPP: &str = "#include \"calculator.cpp\"\n\nint run() {\n    acceptance::Calculator c;\n    return c.add(1, 2);\n}\n";

pub async fn run(clangd: Option<PathBuf>) -> Result<()> {
    super::lsp_full::run(
        LspFullFlowSpec {
            language_key: "cpp",
            display_name: "C++",
            which_name: "clangd",
            cli_flag_display: "clangd",
            windows_fallback: None,
            version_arg: Some("--version"),
            scaffold: FixtureScaffold::None,
            source_files: vec![
                ("calculator.cpp", CALCULATOR_CPP),
                ("callsite.cpp", CALLSITE_CPP),
            ],
            config_section: "clangd",
            lsp_timeout_seconds: 30,
            extra_config_lines: vec![],
            expected_indexed_files: IndexedFilesAssertion::Exact(2),
            // Informational, not Asserted -- see c_lsp.rs for why (confirmed
            // live: clangd's workspace/symbol only sees files it's been
            // told are open, and search_symbols never opens one first).
            symbols_stage: SymbolsStage::Informational {
                query: "Calculator",
            },
            call_site_file: "callsite.cpp",
            call_site_source: CALLSITE_CPP,
            call_site_line: 5,
            call_site_needle: "add",
            declaration_file: "calculator.cpp",
            declaration_source: CALCULATOR_CPP,
            declaration_line: 6,
            declaration_needle: "add",
            search_query: "Calculator adds two integers together",
            search_assertion: SearchAssertion::ContainsFile,
            language_identifier: "cpp",
            provider_name: "clangd",
            generated_dirs: &[],
            cross_file_references: false,
        },
        clangd,
    )
    .await
}
