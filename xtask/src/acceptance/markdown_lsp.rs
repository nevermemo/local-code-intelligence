//! Markdown spec for the shared positive-path LSP acceptance engine
//! (`super::lsp_full`), against two real documents and a real installed
//! marksman. The fixture is prose only (`[index] prose = true`): a link in
//! `docs/index.md` points at a heading of `docs/guide.md`, which is what
//! marksman resolves (definition at the link, references at the heading,
//! `workspace/symbol` over the headings). marksman takes the `server`
//! subcommand, which the adapter adds; `--version` is a plain probe.

use super::lsp_full::{IndexedFilesAssertion, LspFullFlowSpec, SearchAssertion, SymbolsStage};
use super::recovery::FixtureScaffold;
use anyhow::Result;
use std::path::PathBuf;

const GUIDE_MD: &str = "# Calculator guide\n\n## Adding numbers\n\nThe add function returns the sum of two integers.\n";

const INDEX_MD: &str =
    "# Index\n\nSee [adding numbers](guide.md#adding-numbers) for how the calculator adds.\n";

pub async fn run(marksman: Option<PathBuf>) -> Result<()> {
    super::lsp_full::run(
        LspFullFlowSpec {
            language_key: "markdown",
            display_name: "Markdown",
            which_name: "marksman",
            cli_flag_display: "marksman",
            fallback_env: Some("LCI_ACCEPTANCE_MARKSMAN"),
            version_arg: Some("--version"),
            scaffold: FixtureScaffold::None,
            source_files: vec![
                // marksman reads a workspace only below a `.git` directory or a
                // `.marksman.toml`; without one it answers for open files alone.
                (".marksman.toml", ""),
                ("docs/guide.md", GUIDE_MD),
                ("docs/index.md", INDEX_MD),
            ],
            config_section: "markdown",
            lsp_timeout_seconds: 30,
            extra_config_lines: vec!["[index]".to_string(), "prose = true".to_string()],
            expected_indexed_files: IndexedFilesAssertion::Exact(2),
            symbols_stage: SymbolsStage::Asserted {
                query: "Adding",
                expect_name: "H2: Adding numbers",
            },
            call_site_file: "docs/index.md",
            call_site_source: INDEX_MD,
            call_site_line: 3,
            call_site_needle: "guide.md",
            declaration_file: "docs/guide.md",
            declaration_source: GUIDE_MD,
            declaration_line: 3,
            declaration_needle: "Adding",
            search_query: "adding numbers calculator",
            search_assertion: SearchAssertion::ContainsFile,
            language_identifier: "markdown",
            provider_name: "marksman",
            generated_dirs: &[],
            cross_file_references: true,
            extra_probes: vec![],
        },
        marksman,
    )
    .await
}
