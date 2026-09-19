//! Go spec for the shared positive-path LSP acceptance engine
//! (`super::lsp_full`), against a real `go.mod`-rooted module and a real
//! installed `gopls`.

use super::lsp_full::{IndexedFilesAssertion, LspFullFlowSpec, SearchAssertion, SymbolsStage};
use super::recovery::FixtureScaffold;
use anyhow::Result;
use std::path::PathBuf;

const GO_MOD: &str = "module acceptance\n\ngo 1.21\n";

const CALCULATOR_GO: &str = "package acceptance\n\n// Calculator performs basic arithmetic for acceptance testing.\ntype Calculator struct{}\n\n// Add returns the sum of two integers.\nfunc (c Calculator) Add(a, b int) int {\n\treturn a + b\n}\n";

const CALLSITE_GO: &str = "package acceptance\n\n// Run exercises Calculator.Add from a separate file.\nfunc Run(c Calculator) int {\n\treturn c.Add(1, 2)\n}\n";

pub async fn run(gopls: Option<PathBuf>) -> Result<()> {
    super::lsp_full::run(
        LspFullFlowSpec {
            language_key: "go",
            display_name: "Go",
            which_name: "gopls",
            cli_flag_display: "gopls",
            fallback_env: None,
            version_arg: Some("version"),
            scaffold: FixtureScaffold::None,
            source_files: vec![
                ("go.mod", GO_MOD),
                ("calculator.go", CALCULATOR_GO),
                ("callsite.go", CALLSITE_GO),
            ],
            config_section: "go",
            lsp_timeout_seconds: 30,
            extra_config_lines: vec![],
            expected_indexed_files: IndexedFilesAssertion::Exact(2),
            symbols_stage: SymbolsStage::Asserted {
                query: "Calculator",
                expect_name: "Calculator",
            },
            call_site_file: "callsite.go",
            call_site_source: CALLSITE_GO,
            call_site_line: 5,
            call_site_needle: "Add",
            declaration_file: "calculator.go",
            declaration_source: CALCULATOR_GO,
            declaration_line: 7,
            declaration_needle: "Add",
            search_query: "Calculator adds two integers together",
            search_assertion: SearchAssertion::ContainsFile,
            language_identifier: "go",
            provider_name: "gopls",
            generated_dirs: &[],
            cross_file_references: true,
        },
        gopls,
    )
    .await
}
