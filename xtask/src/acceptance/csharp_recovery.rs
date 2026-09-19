//! C# spec for the shared persistent-reuse/recovery acceptance engine
//! (`super::recovery`). See `recovery.rs` for the behavior every language
//! family shares; ported originally from `scripts/CSharpLspRecovery.ps1`.

use super::recovery::{FixtureScaffold, NavigationProbe, ProcessCheck, RecoverySpec};
use anyhow::Result;
use std::path::PathBuf;

const CSPROJ: &str = r#"<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Library</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
  </PropertyGroup>
</Project>
"#;

const PRODUCTION_CS: &str = r#"namespace Acceptance;

public interface ICalculator { int Add(int left, int right); }

public sealed class Calculator : ICalculator
{
    public int Add(int left, int right) => left + right;
    public int Use() => Add(2, 3);
}
"#;

pub async fn run(csharp_ls: Option<PathBuf>) -> Result<()> {
    super::recovery::run(
        RecoverySpec {
            language_key: "csharp",
            display_name: "C#",
            which_name: "csharp-ls",
            cli_flag_display: "csharp-ls",
            fallback_env: Some(super::CSHARP_LS_FALLBACK_ENV),
            requires_tool: Some("dotnet"),
            scaffold: FixtureScaffold::DotNetSolution {
                csproj_filename: "CSharpAcceptance.csproj",
                csproj: CSPROJ,
                solution_name: "CSharpAcceptance",
            },
            source_files: vec![("src/Production.cs", PRODUCTION_CS)],
            config_section: "csharp",
            extra_config_lines: vec!["args = ['--solution', 'CSharpAcceptance.sln']".to_string()],
            lsp_timeout_seconds: 60,
            attempts: 20,
            probe: NavigationProbe::Symbols {
                query: "Calculator",
                expect_substring: "calculator",
            },
            process_check: ProcessCheck::SingleChild("csharp-ls"),
            process_label: "csharp-ls",
        },
        csharp_ls,
    )
    .await
}
