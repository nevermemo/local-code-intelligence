//! C# spec for the shared missing-server acceptance engine (`super::missing`).
//! See `missing.rs` for the behavior every language family shares; ported
//! originally from `scripts/CSharpLspMissingServer.ps1`.

use super::missing::{MissingServerSpec, ProcessCheck};
use anyhow::Result;

const PRODUCTION_CS: &str = r#"namespace Acceptance;

public interface ICalculator { int Add(int left, int right); }

public sealed class Calculator : ICalculator
{
    public int Add(int left, int right) => left + right;
}
"#;

const LIB_RS: &str = r#"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}
"#;

pub async fn run() -> Result<()> {
    super::missing::run(MissingServerSpec {
        language_key: "csharp",
        display_name: "C#",
        missing_path: "definitely-missing-csharp-ls",
        extra_config_lines: vec!["args = ['--solution', 'CSharpAcceptance.sln']".to_string()],
        source_files: vec![("src/Production.cs", PRODUCTION_CS), ("src/lib.rs", LIB_RS)],
        search_query: "Calculator Add",
        search_languages: &["csharp"],
        definition_file: "src/Production.cs",
        definition_line: 7,
        definition_character: 15,
        exact_provider_terms: &["csharp-ls", "csharp language server"],
        near_word: "csharp",
        extra_terms: &[],
        process_check: ProcessCheck::Children("csharp-ls"),
    })
    .await
}
