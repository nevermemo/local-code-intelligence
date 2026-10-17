//! C++ spec for the shared positive-path LSP acceptance engine
//! (`super::lsp_full`), against a real standalone C++ fixture (no build
//! system) and a real installed `clangd`. Shares the `[clangd]` config
//! section with `c_lsp.rs` -- one clangd instance navigates both `c` and
//! `cpp` -- but uses its own `language_key` ("cpp") so its fixture and
//! evidence file stay distinct from C's.

use super::lsp_full::{
    ExtraProbe, IndexedFilesAssertion, LspFullFlowSpec, SearchAssertion, SymbolsStage,
};
use super::recovery::FixtureScaffold;
use anyhow::Result;
use std::path::PathBuf;

const CALCULATOR_CPP: &str = "namespace acceptance {\n// Calculator performs basic arithmetic for acceptance testing.\nclass Calculator {\npublic:\n    // add returns the sum of two integers.\n    int add(int a, int b) { return a + b; }\n};\n}\n";

const CALLSITE_CPP: &str = "#include \"calculator.cpp\"\n\nint run() {\n    acceptance::Calculator c;\n    return c.add(1, 2);\n}\n";

// The other registered C++ extensions, each navigated by a real clangd through
// an extra probe: `.cc` (call site) to `.hxx` (declaration), and `.cxx` (call
// site) to `.hh` (declaration).
const TALLY_HXX: &str = "namespace acceptance {\n// Tally counts acceptance steps.\nclass Tally {\npublic:\n    // bump increments the tally.\n    int bump(int n) { return n + 1; }\n};\n}\n";

const TALLY_USE_CC: &str = "#include \"tally.hxx\"\n\nint run_tally() {\n    acceptance::Tally t;\n    return t.bump(1);\n}\n";

const SHAPE_HH: &str = "// Shape describes a figure with an area.\nclass Shape {\npublic:\n    // area returns the figure's area.\n    virtual double area() const = 0;\n};\n";

const MEASURE_CXX: &str =
    "#include \"shape.hh\"\n\ndouble measure(const Shape &s) {\n    return s.area();\n}\n";

pub async fn run(clangd: Option<PathBuf>) -> Result<()> {
    super::lsp_full::run(
        LspFullFlowSpec {
            language_key: "cpp",
            display_name: "C++",
            which_name: "clangd",
            cli_flag_display: "clangd",
            fallback_env: None,
            version_arg: Some("--version"),
            scaffold: FixtureScaffold::None,
            source_files: vec![
                ("calculator.cpp", CALCULATOR_CPP),
                ("callsite.cpp", CALLSITE_CPP),
                ("tally.hxx", TALLY_HXX),
                ("tally_use.cc", TALLY_USE_CC),
                ("shape.hh", SHAPE_HH),
                ("measure.cxx", MEASURE_CXX),
            ],
            config_section: "clangd",
            lsp_timeout_seconds: 30,
            extra_config_lines: vec![],
            expected_indexed_files: IndexedFilesAssertion::Exact(6),
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
            extra_probes: vec![
                ExtraProbe {
                    call_site_file: "tally_use.cc",
                    call_site_source: TALLY_USE_CC,
                    call_site_line: 5,
                    call_site_needle: "bump",
                    declaration_file: "tally.hxx",
                    declaration_source: TALLY_HXX,
                    declaration_line: 6,
                    declaration_needle: "bump",
                },
                ExtraProbe {
                    call_site_file: "measure.cxx",
                    call_site_source: MEASURE_CXX,
                    call_site_line: 4,
                    call_site_needle: "area",
                    declaration_file: "shape.hh",
                    declaration_source: SHAPE_HH,
                    declaration_line: 5,
                    declaration_needle: "area",
                },
            ],
        },
        clangd,
    )
    .await
}
