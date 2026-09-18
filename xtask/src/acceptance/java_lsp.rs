//! Java spec for the shared positive-path LSP acceptance engine
//! (`super::lsp_full`), against a real Eclipse `.project`/`.classpath` and a
//! real installed jdtls.
//!
//! Unlike every other language family, `[java].path` in the written config
//! is a jdtls *installation directory*, not an executable -- `JavaServer`
//! (`lci-core/src/lsp/java.rs`) spawns `java` directly, finding the launcher
//! jar and platform config dir itself. There is no PATH-discoverable jdtls
//! command to resolve via `which_name`, so this always resolves through
//! `windows_fallback` (a hardcoded install location, the same precedent
//! `csharp_recovery.rs` uses for `WINDOWS_CSHARP_LS_FALLBACK`) unless a real
//! `--java <dir>` CLI override is given. `version_arg: None`, matching
//! pyright's precedent: `resolve_server` now resolves a directory here, and
//! a directory cannot be executed for a `--version` probe.

use crate::acceptance::lsp_full::{
    IndexedFilesAssertion, LspFullFlowSpec, SearchAssertion, SymbolsStage,
};
use crate::acceptance::recovery::FixtureScaffold;
use anyhow::Result;
use std::path::PathBuf;

const WINDOWS_JDTLS_FALLBACK: &str = r"C:\Users\micro\tools\jdtls";

pub(crate) const DOT_PROJECT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<projectDescription>
	<name>acceptance</name>
	<comment></comment>
	<projects>
	</projects>
	<buildSpec>
		<buildCommand>
			<name>org.eclipse.jdt.core.javabuilder</name>
			<arguments>
			</arguments>
		</buildCommand>
	</buildSpec>
	<natures>
		<nature>org.eclipse.jdt.core.javanature</nature>
	</natures>
</projectDescription>
"#;

pub(crate) const DOT_CLASSPATH: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<classpath>
	<classpathentry kind="src" path="src"/>
	<classpathentry kind="con" path="org.eclipse.jdt.launching.JRE_CONTAINER"/>
	<classpathentry kind="output" path="bin"/>
</classpath>
"#;

const CALCULATOR_JAVA: &str = "package acceptance;\n\n// Production billing arithmetic implementation.\npublic class Calculator {\n    public int add(int a, int b) {\n        return a + b;\n    }\n}\n";

const CALLSITE_JAVA: &str = "package acceptance;\n\npublic class CallSite {\n    public int run(Calculator calculator) {\n        return calculator.add(1, 2);\n    }\n}\n";

const TESTS_JAVA: &str = "package acceptance;\n\n// Test-only Calculator usage and documentation example.\npublic class CalculatorTests {\n    public static boolean example() {\n        return new Calculator().add(1, 2) == 3;\n    }\n}\n";

pub async fn run(java_dir: Option<PathBuf>) -> Result<()> {
    super::lsp_full::run(
        LspFullFlowSpec {
            language_key: "java",
            display_name: "Java",
            which_name: "jdtls",
            cli_flag_display: "java",
            windows_fallback: Some(WINDOWS_JDTLS_FALLBACK),
            version_arg: None,
            scaffold: FixtureScaffold::None,
            source_files: vec![
                (".project", DOT_PROJECT),
                (".classpath", DOT_CLASSPATH),
                ("src/Calculator.java", CALCULATOR_JAVA),
                ("src/CallSite.java", CALLSITE_JAVA),
                ("tests/CalculatorTests.java", TESTS_JAVA),
            ],
            config_section: "java",
            lsp_timeout_seconds: 90,
            extra_config_lines: vec![],
            expected_indexed_files: IndexedFilesAssertion::Exact(3),
            symbols_stage: SymbolsStage::Asserted {
                query: "Calculator",
                expect_name: "Calculator",
            },
            call_site_file: "src/CallSite.java",
            call_site_source: CALLSITE_JAVA,
            call_site_line: 5,
            call_site_needle: "add",
            declaration_file: "src/Calculator.java",
            declaration_source: CALCULATOR_JAVA,
            declaration_line: 5,
            declaration_needle: "add",
            search_query: "production billing arithmetic Calculator add implementation",
            // Informational only, like TypeScript: `search`'s own "lsp"
            // channel is powered internally by lowercase, "#"-suffixed
            // symbol queries derived from the search text (e.g.
            // "calculator#") -- confirmed live that jdtls's workspace/symbol
            // does not match that derived form the way it matches a literal
            // query (the dedicated symbols_stage above, using the literal
            // "Calculator", passes reliably). Not a regression when absent
            // here; ranking and scores are still asserted.
            search_assertion: SearchAssertion::RankedWithScores {
                test_file: "tests/CalculatorTests.java",
                assert_lsp_channel: false,
            },
            language_identifier: "java",
            provider_name: "jdtls",
            generated_dirs: &["bin"],
        },
        java_dir,
    )
    .await
}
