//! TypeScript/JavaScript spec for the shared missing-server acceptance
//! engine (`super::missing`). See `missing.rs` for the behavior every
//! language family shares.
//!
//! Unlike csharp-ls (a native executable spawned directly as LCI's child),
//! a definitely-missing typescript-language-server never spawns any process
//! at all, so `ProcessCheck::Descendants("node")` here is really only
//! proving a negative for the Rust-filtered-search isolation check -- but it
//! is the correct check to use rather than `Children`, since a *real*
//! typescript-language-server spawns through an intermediate `cmd.exe` shim
//! on Windows and would never appear as a direct child.

use super::missing::{MissingServerSpec, ProcessCheck};
use anyhow::Result;

const PRODUCTION_TS: &str = r#"export interface ICalculator {
  add(left: number, right: number): number;
}

export class Calculator implements ICalculator {
  add(left: number, right: number): number {
    return left + right;
  }
}
"#;

const LIB_RS: &str = r#"pub fn add(left: i32, right: i32) -> i32 {
    left + right
}
"#;

pub async fn run() -> Result<()> {
    super::missing::run(MissingServerSpec {
        language_key: "typescript",
        config_section: "typescript",
        display_name: "TypeScript",
        missing_path: "definitely-missing-typescript-language-server",
        extra_config_lines: vec![],
        source_files: vec![("src/production.ts", PRODUCTION_TS), ("src/lib.rs", LIB_RS)],
        search_query: "Calculator add",
        search_languages: &["typescript"],
        definition_file: "src/production.ts",
        definition_line: 6,
        definition_character: 2,
        exact_provider_terms: &["typescript-language-server", "typescript language server"],
        near_word: "typescript",
        extra_terms: &[],
        process_check: ProcessCheck::Descendants("node"),
    })
    .await
}
