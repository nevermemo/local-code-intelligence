//! Ported from `scripts/Acceptance-Multilingual.ps1`.

use super::{Evidence, Fixture, run_lci, test_results_dir};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::Path;
use std::time::Duration;

const RUST_LIB: &str = r#"pub fn rust_fixture_anchor() -> &'static str {
    "rust"
}
"#;

const TELEMETRY_TS_V1: &str = r#"// Production event normalization used by the live telemetry path.
export function buildProductionTelemetryPipeline(events: string[]): string[] {
  return events.map(event => event.trim()).filter(Boolean);
}
"#;

const TELEMETRY_PANEL_TSX: &str = r#"export const TelemetryPanel = () => <section>Live telemetry</section>;
"#;

const AUDIT_JS: &str = r#"export function flushAuditBeacon() {
  return "sent";
}
"#;

const BADGE_JSX: &str = r#"export const TelemetryBadge = () => <strong>Ready</strong>;
"#;

const TELEMETRY_TEST_TS: &str = r#"// Search decoy: buildProductionTelemetryPipeline is mentioned only in a test.
export const productionPipelineDocumentation = "trim and filter events";
"#;

const FEATURE_PIPELINE_PY_V1: &str = r#"# Production feature normalization used by the live feature pipeline.


def build_production_feature_pipeline(features):
    normalized = [feature.strip().lower() for feature in features]
    return [feature for feature in normalized if feature]
"#;

const FEATURE_PIPELINE_TEST_PY: &str = r#"# Search decoy: build_production_feature_pipeline is mentioned only in a test.
production_pipeline_documentation = "strip, lowercase, and drop empty features"
"#;

const TELEMETRY_PROCESSOR_CS_V1: &str = r#"namespace Telemetry;

/// <summary>Production event normalization used by the live telemetry path.</summary>
public sealed class TelemetryProcessor
{
    public string NormalizeProductionEvent(string value) => value.Trim().ToLowerInvariant();
}
"#;

const TELEMETRY_PROCESSOR_TESTS_CS: &str = r#"namespace Telemetry.Tests;

// Search decoy: NormalizeProductionEvent is mentioned only in a test.
public sealed class TelemetryProcessorTests { public const string Expected = "trim lowercase"; }
"#;

const PIPELINE_GO_V1: &str = r#"package pipeline

import "strings"

// BuildProductionEventPipeline trims and filters production events.
func BuildProductionEventPipeline(events []string) []string {
	result := make([]string, 0, len(events))
	for _, event := range events {
		if trimmed := strings.TrimSpace(event); trimmed != "" {
			result = append(result, trimmed)
		}
	}
	return result
}
"#;

const PIPELINE_TEST_GO: &str = r#"package pipeline

// Search decoy: BuildProductionEventPipeline is mentioned only in a test.
const ExpectedPipelineDescription = "trim and filter events"
"#;

const ORDER_VALIDATOR_JAVA_V1: &str = r#"package orders;

import java.util.List;

/** Validates a production order before it is queued for fulfillment. */
public final class OrderValidator {
    public static boolean isValidProductionOrder(String orderId, List<String> items) {
        return orderId != null && !orderId.isEmpty() && !items.isEmpty();
    }
}
"#;

const ORDER_VALIDATOR_TEST_JAVA: &str = r#"package orders;

// Search decoy: isValidProductionOrder is mentioned only in a test.
public final class OrderValidatorTest {
    public static final String EXPECTED_ORDER_VALIDATION_DESCRIPTION =
        "reject orders with no id or no items";
}
"#;

const FIXTURE_CARGO_TOML: &str = r#"[package]
name = "multilingual-acceptance-fixture"
version = "0.1.0"
edition = "2024"
"#;

const FIXTURE_GITIGNORE: &str = "/target/\n";

pub async fn run() -> Result<()> {
    let mut evidence = Evidence::new("acceptance-multilingual")?;
    let fixture = Fixture::create("multilingual")?;

    let outcome = execute(&fixture, &mut evidence).await;

    fixture.cleanup();
    if !fixture.removed() {
        let message =
            "fixture cleanup did not fully remove the multilingual fixture/data directories";
        let _ = evidence.fail(message);
        return match outcome {
            Ok(()) => Err(anyhow::anyhow!(message)),
            Err(error) => Err(error.context(message)),
        };
    }

    match outcome {
        Ok(()) => {
            evidence.pass()?;
            Ok(())
        }
        Err(error) => {
            evidence.fail(&error)?;
            Err(error)
        }
    }
}

async fn execute(fixture: &Fixture, evidence: &mut Evidence) -> Result<()> {
    evidence.stage("write-fixture")?;
    fixture.write("Cargo.toml", FIXTURE_CARGO_TOML)?;
    fixture.write(".gitignore", FIXTURE_GITIGNORE)?;
    fixture.write("src/lib.rs", RUST_LIB)?;
    fixture.write("src/telemetry.ts", TELEMETRY_TS_V1)?;
    fixture.write("src/panel.tsx", TELEMETRY_PANEL_TSX)?;
    fixture.write("src/audit.js", AUDIT_JS)?;
    fixture.write("src/badge.jsx", BADGE_JSX)?;
    fixture.write("tests/telemetry.test.ts", TELEMETRY_TEST_TS)?;
    fixture.write("src/feature_pipeline.py", FEATURE_PIPELINE_PY_V1)?;
    fixture.write("tests/feature_pipeline.test.py", FEATURE_PIPELINE_TEST_PY)?;
    fixture.write("src/TelemetryProcessor.cs", TELEMETRY_PROCESSOR_CS_V1)?;
    fixture.write(
        "tests/TelemetryProcessorTests.cs",
        TELEMETRY_PROCESSOR_TESTS_CS,
    )?;
    fixture.write("src/pipeline.go", PIPELINE_GO_V1)?;
    fixture.write("tests/pipeline_test.go", PIPELINE_TEST_GO)?;
    fixture.write("src/OrderValidator.java", ORDER_VALIDATOR_JAVA_V1)?;
    fixture.write("tests/OrderValidatorTest.java", ORDER_VALIDATOR_TEST_JAVA)?;

    evidence.stage("write-config")?;
    let config_path = fixture.write_config(&[
        "embedding_url = 'http://localhost:8766/v1'".to_string(),
        "embedding_model = 'qwen3-embedding-4b'".to_string(),
        "reranker_url = 'http://localhost:8767/rerank'".to_string(),
        "reranker_model = 'qwen3-reranker-4b'".to_string(),
    ])?;
    let normalized_data_dir = fixture.data_dir.to_string_lossy().replace('\\', "/");
    let failure_config_path = fixture.write(
        "failure-config.toml",
        &format!(
            "data_dir = '{normalized_data_dir}'\n\
             embedding_url = 'http://127.0.0.1:1/v1'\n\
             embedding_model = 'qwen3-embedding-4b'\n\
             reranker_url = 'http://localhost:8767/rerank'\n\
             reranker_model = 'qwen3-reranker-4b'\n\
             embedding_timeout_seconds = 2\n"
        ),
    )?;

    let workspace = fixture.dir.to_string_lossy().to_string();

    evidence.stage("initial-index")?;
    let first = run_report(&config_path, "multilingual-index", &["index", &workspace]).await?;
    let files = first["files"]
        .as_u64()
        .context("index report missing files count")?;
    if files != 14 {
        bail!("Initial index included {files} files instead of 14");
    }
    evidence.set("initial_files", files)?;

    evidence.stage("per-language-search")?;
    let _rust_search = assert_search(
        &config_path,
        &workspace,
        "multilingual-search-rust",
        "rust_fixture_anchor",
        "src/lib.rs",
        "rust",
        "rust_fixture_anchor",
    )
    .await?;

    let production_search = assert_search(
        &config_path,
        &workspace,
        "multilingual-search-typescript",
        "buildProductionTelemetryPipeline",
        "src/telemetry.ts",
        "typescript",
        "function buildProductionTelemetryPipeline",
    )
    .await?;

    let _tsx_search = assert_search(
        &config_path,
        &workspace,
        "multilingual-search-tsx",
        "TelemetryPanel",
        "src/panel.tsx",
        "tsx",
        "TelemetryPanel",
    )
    .await?;

    let _javascript_search = assert_search(
        &config_path,
        &workspace,
        "multilingual-search-javascript",
        "flushAuditBeacon",
        "src/audit.js",
        "javascript",
        "flushAuditBeacon",
    )
    .await?;

    let _jsx_search = assert_search(
        &config_path,
        &workspace,
        "multilingual-search-jsx",
        "TelemetryBadge",
        "src/badge.jsx",
        "jsx",
        "TelemetryBadge",
    )
    .await?;

    let python_search = assert_search(
        &config_path,
        &workspace,
        "multilingual-search-python",
        "build_production_feature_pipeline",
        "src/feature_pipeline.py",
        "python",
        "def build_production_feature_pipeline",
    )
    .await?;

    let csharp_search = assert_search(
        &config_path,
        &workspace,
        "multilingual-search-csharp",
        "NormalizeProductionEvent",
        "src/TelemetryProcessor.cs",
        "csharp",
        "NormalizeProductionEvent",
    )
    .await?;

    let go_search = assert_search(
        &config_path,
        &workspace,
        "multilingual-search-go",
        "BuildProductionEventPipeline",
        "src/pipeline.go",
        "go",
        "func BuildProductionEventPipeline",
    )
    .await?;

    let java_search = assert_search(
        &config_path,
        &workspace,
        "multilingual-search-java",
        "isValidProductionOrder",
        "src/OrderValidator.java",
        "java",
        "static boolean isValidProductionOrder",
    )
    .await?;

    evidence.stage("ranking-and-decoys")?;
    let csharp_results = results_array(&csharp_search)?;
    let csharp_decoy_present = csharp_results.iter().any(|hit| {
        hit["relative_file_path"].as_str() == Some("tests/TelemetryProcessorTests.cs")
            && hit["source_role"].as_str() == Some("test")
    });
    if !csharp_decoy_present {
        bail!("Initial index did not include the C# test decoy with test role");
    }
    if csharp_results
        .first()
        .and_then(|hit| hit["relative_file_path"].as_str())
        != Some("src/TelemetryProcessor.cs")
    {
        bail!("Production C# implementation did not outrank the test decoy");
    }

    let go_results = results_array(&go_search)?;
    let go_decoy_present = go_results.iter().any(|hit| {
        hit["relative_file_path"].as_str() == Some("tests/pipeline_test.go")
            && hit["source_role"].as_str() == Some("test")
    });
    if !go_decoy_present {
        bail!("Initial index did not include the Go test decoy with test role");
    }
    if go_results
        .first()
        .and_then(|hit| hit["relative_file_path"].as_str())
        != Some("src/pipeline.go")
    {
        bail!("Production Go implementation did not outrank the test decoy");
    }

    let java_results = results_array(&java_search)?;
    let java_decoy_present = java_results.iter().any(|hit| {
        hit["relative_file_path"].as_str() == Some("tests/OrderValidatorTest.java")
            && hit["source_role"].as_str() == Some("test")
    });
    if !java_decoy_present {
        bail!("Initial index did not include the Java test decoy with test role");
    }
    if java_results
        .first()
        .and_then(|hit| hit["relative_file_path"].as_str())
        != Some("src/OrderValidator.java")
    {
        bail!("Production Java implementation did not outrank the test decoy");
    }

    let python_results = results_array(&python_search)?;
    let python_decoy_present = python_results.iter().any(|hit| {
        hit["relative_file_path"].as_str() == Some("tests/feature_pipeline.test.py")
            && hit["language"].as_str() == Some("python")
    });
    if !python_decoy_present {
        bail!("Initial index did not include the Python test decoy as python");
    }
    let python_first = python_results
        .first()
        .context("python search returned no results")?;
    if python_first["relative_file_path"].as_str() != Some("src/feature_pipeline.py")
        || !python_first["code"]
            .as_str()
            .unwrap_or_default()
            .contains("def build_production_feature_pipeline")
    {
        bail!("Production Python implementation did not outrank the test decoy");
    }
    let python_production = python_results
        .iter()
        .find(|hit| {
            hit["relative_file_path"].as_str() == Some("src/feature_pipeline.py")
                && hit["code"]
                    .as_str()
                    .unwrap_or_default()
                    .contains("def build_production_feature_pipeline")
        })
        .context("python search missing the production implementation hit")?;
    let start_line = python_production["start_line"]
        .as_i64()
        .context("python production hit missing start_line")?;
    let end_line = python_production["end_line"]
        .as_i64()
        .context("python production hit missing end_line")?;
    if start_line < 1 || end_line < start_line {
        bail!(
            "Python production result has an invalid one-based line range: {start_line}-{end_line}"
        );
    }
    if python_production["semantic_score"].is_null() {
        bail!("Python production result is missing a semantic score");
    }
    if python_search["reranked"].as_bool() == Some(true)
        && python_production["reranker_score"].is_null()
    {
        bail!(
            "Python production result is missing a reranker score while live reranking is available"
        );
    }

    let production_results = results_array(&production_search)?;
    let production_first = production_results
        .first()
        .context("typescript search returned no results")?;
    if production_first["relative_file_path"].as_str() != Some("src/telemetry.ts")
        || !production_first["code"]
            .as_str()
            .unwrap_or_default()
            .contains("function buildProductionTelemetryPipeline")
    {
        bail!("Production TypeScript implementation did not outrank the test decoy");
    }

    evidence.stage("zero-work-reindex")?;
    let unchanged =
        run_report(&config_path, "multilingual-reindex", &["index", &workspace]).await?;
    if unchanged["parsed_files"].as_u64() != Some(0)
        || unchanged["embedded_chunks"].as_u64() != Some(0)
    {
        bail!("Unchanged multilingual reindex reparsed files or recomputed embeddings");
    }

    evidence.stage("typescript-update")?;
    let telemetry_ts_v2 = format!("{TELEMETRY_TS_V1}\nexport const telemetryRevision = 2;\n");
    fixture.write("src/telemetry.ts", &telemetry_ts_v2)?;
    let changed = run_report(
        &config_path,
        "multilingual-typescript-update",
        &["index", &workspace],
    )
    .await?;
    if changed["parsed_files"].as_u64() != Some(1) {
        let parsed = changed["parsed_files"].as_u64().unwrap_or_default();
        bail!("TypeScript update parsed {parsed} files instead of exactly one");
    }
    if changed["reused_chunks"].as_u64().unwrap_or(0) == 0 {
        bail!("TypeScript update did not reuse unchanged chunks from other languages");
    }

    evidence.stage("python-update")?;
    let feature_pipeline_py_v2 =
        format!("{FEATURE_PIPELINE_PY_V1}\nfeature_pipeline_revision = 2\n");
    fixture.write("src/feature_pipeline.py", &feature_pipeline_py_v2)?;
    let python_changed = run_report(
        &config_path,
        "multilingual-python-update",
        &["index", &workspace],
    )
    .await?;
    if python_changed["parsed_files"].as_u64() != Some(1) {
        let parsed = python_changed["parsed_files"].as_u64().unwrap_or_default();
        bail!("Python update parsed {parsed} files instead of exactly one");
    }
    if python_changed["reused_chunks"].as_u64().unwrap_or(0) == 0 {
        bail!("Python-only update did not reuse unchanged chunks from other languages");
    }

    evidence.stage("csharp-update")?;
    let telemetry_processor_cs_v2 = format!(
        "{TELEMETRY_PROCESSOR_CS_V1}\npublic static class TelemetryRevision {{ public const int Value = 2; }}\n"
    );
    fixture.write("src/TelemetryProcessor.cs", &telemetry_processor_cs_v2)?;
    let csharp_changed = run_report(
        &config_path,
        "multilingual-csharp-update",
        &["index", &workspace],
    )
    .await?;
    if csharp_changed["parsed_files"].as_u64() != Some(1) {
        let parsed = csharp_changed["parsed_files"].as_u64().unwrap_or_default();
        bail!("C# update parsed {parsed} files instead of exactly one");
    }
    if csharp_changed["reused_chunks"].as_u64().unwrap_or(0) == 0 {
        bail!("C# update did not reuse other-language chunks");
    }

    evidence.stage("go-update")?;
    let pipeline_go_v2 = format!("{PIPELINE_GO_V1}\nfunc PipelineRevision() int {{ return 2 }}\n");
    fixture.write("src/pipeline.go", &pipeline_go_v2)?;
    let go_changed = run_report(
        &config_path,
        "multilingual-go-update",
        &["index", &workspace],
    )
    .await?;
    if go_changed["parsed_files"].as_u64() != Some(1) {
        let parsed = go_changed["parsed_files"].as_u64().unwrap_or_default();
        bail!("Go update parsed {parsed} files instead of exactly one");
    }
    if go_changed["reused_chunks"].as_u64().unwrap_or(0) == 0 {
        bail!("Go update did not reuse other-language chunks");
    }

    evidence.stage("java-update")?;
    let order_validator_java_v2 = format!(
        "{ORDER_VALIDATOR_JAVA_V1}\nfinal class OrderValidatorRevision {{ static final int VALUE = 2; }}\n"
    );
    fixture.write("src/OrderValidator.java", &order_validator_java_v2)?;
    let java_changed = run_report(
        &config_path,
        "multilingual-java-update",
        &["index", &workspace],
    )
    .await?;
    if java_changed["parsed_files"].as_u64() != Some(1) {
        let parsed = java_changed["parsed_files"].as_u64().unwrap_or_default();
        bail!("Java update parsed {parsed} files instead of exactly one");
    }
    if java_changed["reused_chunks"].as_u64().unwrap_or(0) == 0 {
        bail!("Java update did not reuse other-language chunks");
    }

    evidence.stage("csharp-decoy-delete")?;
    std::fs::remove_file(fixture.dir.join("tests/TelemetryProcessorTests.cs"))
        .context("remove C# decoy test file")?;
    let csharp_deleted = run_report(
        &config_path,
        "multilingual-csharp-delete",
        &["index", &workspace],
    )
    .await?;
    if csharp_deleted["removed_files"].as_u64() != Some(1) {
        bail!("C# decoy deletion did not remove exactly one file");
    }
    let csharp_deleted_search = run_report(
        &config_path,
        "multilingual-search-after-csharp-delete",
        &[
            "search",
            &workspace,
            "TelemetryProcessorTests",
            "--top-k",
            "8",
        ],
    )
    .await?;
    if results_array(&csharp_deleted_search)?
        .iter()
        .any(|hit| hit["relative_file_path"].as_str() == Some("tests/TelemetryProcessorTests.cs"))
    {
        bail!("Deleted C# decoy chunks remain searchable");
    }

    evidence.stage("javascript-delete")?;
    std::fs::remove_file(fixture.dir.join("src/audit.js"))
        .context("remove JavaScript decoy file")?;
    let deleted = run_report(
        &config_path,
        "multilingual-javascript-delete",
        &["index", &workspace],
    )
    .await?;
    if deleted["removed_files"].as_u64() != Some(1) {
        let removed = deleted["removed_files"].as_u64().unwrap_or_default();
        bail!("JavaScript deletion removed {removed} cached files instead of one");
    }
    let deleted_search = run_report(
        &config_path,
        "multilingual-search-after-delete",
        &["search", &workspace, "flushAuditBeacon", "--top-k", "8"],
    )
    .await?;
    if results_array(&deleted_search)?
        .iter()
        .any(|hit| hit["relative_file_path"].as_str() == Some("src/audit.js"))
    {
        bail!("Deleted JavaScript chunks remain searchable");
    }

    evidence.stage("python-decoy-delete")?;
    std::fs::remove_file(fixture.dir.join("tests/feature_pipeline.test.py"))
        .context("remove Python decoy test file")?;
    let python_deleted = run_report(
        &config_path,
        "multilingual-python-delete",
        &["index", &workspace],
    )
    .await?;
    if python_deleted["removed_files"].as_u64() != Some(1) {
        let removed = python_deleted["removed_files"].as_u64().unwrap_or_default();
        bail!("Python decoy deletion removed {removed} cached files instead of one");
    }
    let python_deleted_search = run_report(
        &config_path,
        "multilingual-search-after-python-delete",
        &[
            "search",
            &workspace,
            "build_production_feature_pipeline",
            "--top-k",
            "8",
        ],
    )
    .await?;
    if results_array(&python_deleted_search)?
        .iter()
        .any(|hit| hit["relative_file_path"].as_str() == Some("tests/feature_pipeline.test.py"))
    {
        bail!("Deleted Python decoy chunks remain searchable");
    }

    evidence.stage("go-decoy-delete")?;
    std::fs::remove_file(fixture.dir.join("tests/pipeline_test.go"))
        .context("remove Go decoy test file")?;
    let go_deleted = run_report(
        &config_path,
        "multilingual-go-delete",
        &["index", &workspace],
    )
    .await?;
    if go_deleted["removed_files"].as_u64() != Some(1) {
        let removed = go_deleted["removed_files"].as_u64().unwrap_or_default();
        bail!("Go decoy deletion removed {removed} cached files instead of one");
    }
    let go_deleted_search = run_report(
        &config_path,
        "multilingual-search-after-go-delete",
        &[
            "search",
            &workspace,
            "ExpectedPipelineDescription",
            "--top-k",
            "8",
        ],
    )
    .await?;
    if results_array(&go_deleted_search)?
        .iter()
        .any(|hit| hit["relative_file_path"].as_str() == Some("tests/pipeline_test.go"))
    {
        bail!("Deleted Go decoy chunks remain searchable");
    }

    evidence.stage("java-decoy-delete")?;
    std::fs::remove_file(fixture.dir.join("tests/OrderValidatorTest.java"))
        .context("remove Java decoy test file")?;
    let java_deleted = run_report(
        &config_path,
        "multilingual-java-delete",
        &["index", &workspace],
    )
    .await?;
    if java_deleted["removed_files"].as_u64() != Some(1) {
        let removed = java_deleted["removed_files"].as_u64().unwrap_or_default();
        bail!("Java decoy deletion removed {removed} cached files instead of one");
    }
    let java_deleted_search = run_report(
        &config_path,
        "multilingual-search-after-java-delete",
        &[
            "search",
            &workspace,
            "EXPECTED_ORDER_VALIDATION_DESCRIPTION",
            "--top-k",
            "8",
        ],
    )
    .await?;
    if results_array(&java_deleted_search)?
        .iter()
        .any(|hit| hit["relative_file_path"].as_str() == Some("tests/OrderValidatorTest.java"))
    {
        bail!("Deleted Java decoy chunks remain searchable");
    }

    evidence.stage("failed-update-retention")?;
    let before_failure = run_report(
        &config_path,
        "multilingual-status-before-failure",
        &["status", &workspace],
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(1100)).await;

    let telemetry_ts_v3 =
        format!("{telemetry_ts_v2}\nexport const failedUpdateMarker = 'not committed';\n");
    fixture.write("src/telemetry.ts", &telemetry_ts_v3)?;
    let feature_pipeline_py_v3 =
        format!("{feature_pipeline_py_v2}\nfailedUpdateMarker = 'not committed'\n");
    fixture.write("src/feature_pipeline.py", &feature_pipeline_py_v3)?;
    let telemetry_processor_cs_v3 = format!(
        "{telemetry_processor_cs_v2}\npublic static class FailedCSharpUpdateMarker {{ }}\n"
    );
    fixture.write("src/TelemetryProcessor.cs", &telemetry_processor_cs_v3)?;
    let pipeline_go_v3 = format!("{pipeline_go_v2}\n// failedUpdateMarker: not committed\n");
    fixture.write("src/pipeline.go", &pipeline_go_v3)?;
    let order_validator_java_v3 =
        format!("{order_validator_java_v2}\n// failedUpdateMarker: not committed\n");
    fixture.write("src/OrderValidator.java", &order_validator_java_v3)?;

    let failure_output = run_lci(&failure_config_path, &["index", &workspace]).await?;
    let failure_evidence = serde_json::json!({
        "success": failure_output.success,
        "output": failure_output.combined().trim(),
    });
    std::fs::write(
        test_results_dir()?.join("multilingual-failed-index.json"),
        serde_json::to_string_pretty(&failure_evidence)?,
    )?;
    if failure_output.success {
        bail!("Index unexpectedly succeeded with an unreachable embedding endpoint");
    }

    let status_after = run_report(
        &config_path,
        "multilingual-status-after-failure",
        &["status", &workspace],
    )
    .await?;
    if status_after["stale"].as_bool() != Some(true) {
        bail!("Failed source update was not reported as stale");
    }
    if status_after["chunks"] != before_failure["chunks"]
        || status_after["indexed_at_unix_seconds"] != before_failure["indexed_at_unix_seconds"]
    {
        bail!("Failed update replaced the previous persisted snapshot");
    }

    evidence.set("files", files)?;
    Ok(())
}

/// Extracts the `results` array from an `index`/`search` JSON report.
fn results_array(report: &Value) -> Result<&Vec<Value>> {
    report["results"]
        .as_array()
        .context("search report is missing a results array")
}

/// Runs a `search` and asserts it returned the expected hit through live
/// reranking with both semantic and lexical retrieval channels present,
/// mirroring the original script's `Assert-SearchResult` function.
async fn assert_search(
    config_path: &Path,
    workspace: &str,
    name: &str,
    query: &str,
    expected_path: &str,
    expected_language: &str,
    expected_code: &str,
) -> Result<Value> {
    let report = run_report(
        config_path,
        name,
        &["search", workspace, query, "--top-k", "8"],
    )
    .await?;
    let results = results_array(&report)?;
    let matches: Vec<&Value> = results
        .iter()
        .filter(|hit| {
            hit["relative_file_path"].as_str() == Some(expected_path)
                && hit["language"].as_str() == Some(expected_language)
                && hit["code"]
                    .as_str()
                    .map(|code| code.contains(expected_code))
                    .unwrap_or(false)
        })
        .collect();
    if matches.is_empty() {
        bail!("{name} did not return {expected_path} as {expected_language}");
    }
    if report["reranked"].as_bool() != Some(true) {
        let warning = report["warning"].as_str().unwrap_or("");
        bail!("{name} did not complete live reranking: {warning}");
    }
    let has_both_channels = matches.iter().any(|hit| {
        hit["retrieval_channels"]
            .as_array()
            .map(|channels| {
                let has_semantic = channels.iter().any(|c| c.as_str() == Some("semantic"));
                let has_lexical = channels.iter().any(|c| c.as_str() == Some("lexical"));
                has_semantic && has_lexical
            })
            .unwrap_or(false)
    });
    if !has_both_channels {
        bail!("{name} did not retrieve {expected_path} through both semantic and lexical channels");
    }
    Ok(report)
}

/// Runs the compiled CLI with `config_path` and `args` via the shared
/// `run_lci` helper, saves the raw stdout to `test-results/<name>.json`
/// (matching the .ps1 scripts' `Run-Report` convention), and parses it as
/// JSON on success. Bails with the combined stdout+stderr on a nonzero exit.
async fn run_report(config_path: &Path, name: &str, args: &[&str]) -> Result<Value> {
    let output = run_lci(config_path, args).await?;
    std::fs::write(
        test_results_dir()?.join(format!("{name}.json")),
        &output.stdout,
    )
    .with_context(|| format!("write {name} evidence"))?;
    if !output.success {
        bail!("{name} failed: {}", output.combined());
    }
    output
        .json()
        .with_context(|| format!("{name} did not produce valid JSON: {}", output.combined()))
}
