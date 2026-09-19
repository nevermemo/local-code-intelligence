//! C spec for the shared positive-path LSP acceptance engine
//! (`super::lsp_full`), against a real standalone C fixture (no build
//! system) and a real installed `clangd`. Shares the `[clangd]` config
//! section with `cpp_lsp.rs` -- one clangd instance navigates both `c` and
//! `cpp` -- but uses its own `language_key` ("c") so its fixture and
//! evidence file stay distinct from C++'s.

use super::lsp_full::{IndexedFilesAssertion, LspFullFlowSpec, SearchAssertion, SymbolsStage};
use super::recovery::FixtureScaffold;
use anyhow::Result;
use std::path::PathBuf;

const CALCULATOR_C: &str = "// Calculator performs basic arithmetic for acceptance testing.\ntypedef struct Calculator {\n    int placeholder;\n} Calculator;\n\n// add returns the sum of two integers.\nint add(Calculator c, int a, int b) {\n    return a + b;\n}\n";

const CALLSITE_C: &str = "#include \"calculator.c\"\n\nint run(void) {\n    Calculator c;\n    return add(c, 1, 2);\n}\n";

pub async fn run(clangd: Option<PathBuf>) -> Result<()> {
    super::lsp_full::run(
        LspFullFlowSpec {
            language_key: "c",
            display_name: "C",
            which_name: "clangd",
            cli_flag_display: "clangd",
            windows_fallback: None,
            version_arg: Some("--version"),
            scaffold: FixtureScaffold::None,
            source_files: vec![("calculator.c", CALCULATOR_C), ("callsite.c", CALLSITE_C)],
            config_section: "clangd",
            lsp_timeout_seconds: 30,
            extra_config_lines: vec![],
            expected_indexed_files: IndexedFilesAssertion::Exact(2),
            // Informational, not Asserted: confirmed live (raw JSON-RPC
            // probe against the real installed clangd) that workspace/
            // symbol only returns results for a file clangd has been told
            // is open via textDocument/didOpen -- and `search_symbols`
            // never opens a file before querying (only find_definition/
            // find_references do, via before_position_request). Without a
            // compile_commands.json driving background indexing, clangd
            // has no other way to discover this fixture's files. Same
            // documented limitation and same choice as Python/TypeScript
            // (see src/lsp/typescript.rs's module doc).
            symbols_stage: SymbolsStage::Informational {
                query: "Calculator",
            },
            call_site_file: "callsite.c",
            call_site_source: CALLSITE_C,
            call_site_line: 5,
            call_site_needle: "add",
            declaration_file: "calculator.c",
            declaration_source: CALCULATOR_C,
            declaration_line: 7,
            declaration_needle: "add",
            search_query: "Calculator adds two integers together",
            search_assertion: SearchAssertion::ContainsFile,
            language_identifier: "c",
            provider_name: "clangd",
            generated_dirs: &[],
            cross_file_references: false,
        },
        clangd,
    )
    .await
}
