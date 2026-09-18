//! C# spec for the shared positive-path LSP acceptance engine
//! (`super::super::lsp_full`). Ported from the non-`-ProbeOnly` branch of
//! `scripts/Acceptance-CSharp-Lsp.ps1`.

use crate::acceptance::lsp_full::{
    IndexedFilesAssertion, LspFullFlowSpec, SearchAssertion, SymbolsStage,
};
use crate::acceptance::recovery::FixtureScaffold;
use anyhow::Result;
use std::path::{Path, PathBuf};

const CSPROJ: &str = r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Library</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
  </PropertyGroup>
</Project>
"#;

const PRODUCTION_SOURCE: &str = "namespace Acceptance;\n\npublic interface ICalculator { int Add(int left, int right); }\n\npublic sealed class Calculator : ICalculator\n{\n    // Production billing arithmetic implementation.\n    public int Add(int left, int right) => left + right;\n    public int Use() => Add(2, 3);\n}\n";

const CALLSITE_SOURCE: &str = "namespace Acceptance;\n\npublic static class CallSite\n{\n    public static int Run(ICalculator calculator) => calculator.Add(1, 2);\n}\n";

const TESTS_SOURCE: &str = "namespace Acceptance.Tests;\n\n// Test-only Calculator usage and documentation example.\npublic static class CalculatorTests\n{\n    public static bool Example() => new Acceptance.Calculator().Add(1, 2) == 3;\n}\n";

pub async fn run(csharp_ls: &Path) -> Result<()> {
    crate::acceptance::lsp_full::run(
        LspFullFlowSpec {
            language_key: "csharp",
            display_name: "C#",
            which_name: "csharp-ls",
            cli_flag_display: "csharp-ls",
            windows_fallback: None,
            version_arg: Some("--version"),
            scaffold: FixtureScaffold::DotNetSolution {
                csproj_filename: "CSharpAcceptance.csproj",
                csproj: CSPROJ,
                solution_name: "CSharpAcceptance",
            },
            source_files: vec![
                (".gitignore", "bin/\nobj/\n"),
                ("src/Production.cs", PRODUCTION_SOURCE),
                ("src/CallSite.cs", CALLSITE_SOURCE),
                ("tests/CalculatorTests.cs", TESTS_SOURCE),
            ],
            config_section: "csharp",
            lsp_timeout_seconds: 60,
            extra_config_lines: vec!["args = ['--solution', 'CSharpAcceptance.sln']".to_string()],
            expected_indexed_files: IndexedFilesAssertion::Exact(3),
            symbols_stage: SymbolsStage::Asserted {
                query: "Calculator",
                expect_name: "Calculator",
            },
            call_site_file: "src/CallSite.cs",
            call_site_source: CALLSITE_SOURCE,
            call_site_line: 5,
            call_site_needle: "Add",
            declaration_file: "src/Production.cs",
            declaration_source: PRODUCTION_SOURCE,
            declaration_line: 8,
            declaration_needle: "Add",
            search_query: "production billing arithmetic Calculator Add implementation",
            search_assertion: SearchAssertion::RankedWithScores {
                test_file: "tests/CalculatorTests.cs",
                assert_lsp_channel: true,
            },
            language_identifier: "csharp",
            provider_name: "csharp-ls",
            generated_dirs: &["bin", "obj"],
        },
        Some(PathBuf::from(csharp_ls)),
    )
    .await
}
