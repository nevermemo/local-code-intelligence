$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$binary = Join-Path $projectRoot 'target\debug\local-code-intelligence.exe'
$outputDir = Join-Path $projectRoot 'test-results'
$fixture = Join-Path $env:TEMP 'lci-multilingual-fixture'
$dataDir = Join-Path $env:TEMP 'lci-multilingual-data'
$configPath = Join-Path $outputDir 'multilingual-config.toml'
$failureConfigPath = Join-Path $outputDir 'multilingual-failure-config.toml'
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

function Write-Utf8([string]$Path, [string]$Content) {
    [System.IO.File]::WriteAllText($Path, $Content, $utf8NoBom)
}

if (-not (Test-Path -LiteralPath $binary)) {
    throw "Debug binary missing: $binary"
}
foreach ($path in @($fixture, $dataDir)) {
    if (Test-Path -LiteralPath $path) {
        Remove-Item -LiteralPath $path -Recurse -Force
    }
}
New-Item -ItemType Directory -Force -Path (Join-Path $fixture 'src') | Out-Null
New-Item -ItemType Directory -Force -Path (Join-Path $fixture 'tests') | Out-Null
New-Item -ItemType Directory -Force -Path $outputDir | Out-Null

Write-Utf8 (Join-Path $fixture 'Cargo.toml') @'
[package]
name = "multilingual-acceptance-fixture"
version = "0.1.0"
edition = "2024"
'@
Write-Utf8 (Join-Path $fixture '.gitignore') "/target/`n"
Write-Utf8 (Join-Path $fixture 'src\lib.rs') @'
pub fn rust_fixture_anchor() -> &'static str {
    "rust"
}
'@
Write-Utf8 (Join-Path $fixture 'src\telemetry.ts') @'
// Production event normalization used by the live telemetry path.
export function buildProductionTelemetryPipeline(events: string[]): string[] {
  return events.map(event => event.trim()).filter(Boolean);
}
'@
Write-Utf8 (Join-Path $fixture 'src\panel.tsx') @'
export const TelemetryPanel = () => <section>Live telemetry</section>;
'@
Write-Utf8 (Join-Path $fixture 'src\audit.js') @'
export function flushAuditBeacon() {
  return "sent";
}
'@
Write-Utf8 (Join-Path $fixture 'src\badge.jsx') @'
export const TelemetryBadge = () => <strong>Ready</strong>;
'@
Write-Utf8 (Join-Path $fixture 'tests\telemetry.test.ts') @'
// Search decoy: buildProductionTelemetryPipeline is mentioned only in a test.
export const productionPipelineDocumentation = "trim and filter events";
'@
Write-Utf8 (Join-Path $fixture 'src\feature_pipeline.py') @'
# Production feature normalization used by the live feature pipeline.


def build_production_feature_pipeline(features):
    normalized = [feature.strip().lower() for feature in features]
    return [feature for feature in normalized if feature]
'@
Write-Utf8 (Join-Path $fixture 'tests\feature_pipeline.test.py') @'
# Search decoy: build_production_feature_pipeline is mentioned only in a test.
production_pipeline_documentation = "strip, lowercase, and drop empty features"
'@
Write-Utf8 (Join-Path $fixture 'src\TelemetryProcessor.cs') @'
namespace Telemetry;

/// <summary>Production event normalization used by the live telemetry path.</summary>
public sealed class TelemetryProcessor
{
    public string NormalizeProductionEvent(string value) => value.Trim().ToLowerInvariant();
}
'@
Write-Utf8 (Join-Path $fixture 'tests\TelemetryProcessorTests.cs') @'
namespace Telemetry.Tests;

// Search decoy: NormalizeProductionEvent is mentioned only in a test.
public sealed class TelemetryProcessorTests { public const string Expected = "trim lowercase"; }
'@

$normalizedDataDir = $dataDir.Replace('\', '/')
Write-Utf8 $configPath @"
embedding_url = "http://localhost:8766/v1"
embedding_model = "qwen3-embedding-4b"
reranker_url = "http://localhost:8767/rerank"
reranker_model = "qwen3-reranker-4b"
data_dir = "$normalizedDataDir"
"@
Write-Utf8 $failureConfigPath @"
embedding_url = "http://127.0.0.1:1/v1"
embedding_model = "qwen3-embedding-4b"
reranker_url = "http://localhost:8767/rerank"
reranker_model = "qwen3-reranker-4b"
data_dir = "$normalizedDataDir"
embedding_timeout_seconds = 2
"@

function Run-Report([string]$Name, [string[]]$Arguments) {
    return Run-ReportWithConfig $Name $configPath $Arguments
}

function Run-ReportWithConfig([string]$Name, [string]$SelectedConfig, [string[]]$Arguments) {
    $text = & $binary --config $SelectedConfig @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Name failed" }
    $text | Set-Content -LiteralPath (Join-Path $outputDir "$Name.json") -Encoding utf8
    return ($text | ConvertFrom-Json)
}

function Assert-SearchResult(
    [string]$Name,
    [string]$Query,
    [string]$ExpectedPath,
    [string]$ExpectedLanguage,
    [string]$ExpectedCode
) {
    $report = Run-Report $Name @('search', $fixture, $Query, '--top-k', '8')
    $match = @($report.results | Where-Object {
        $_.relative_file_path -eq $ExpectedPath -and
        $_.language -eq $ExpectedLanguage -and
        $_.code -match [regex]::Escape($ExpectedCode)
    })
    if ($match.Count -eq 0) {
        throw "$Name did not return $ExpectedPath as $ExpectedLanguage"
    }
    if (-not $report.reranked) {
        throw "$Name did not complete live reranking: $($report.warning)"
    }
    if (-not ($match | Where-Object {
        $_.retrieval_channels -contains 'semantic' -and
        $_.retrieval_channels -contains 'lexical'
    })) {
        throw "$Name did not retrieve $ExpectedPath through both semantic and lexical channels"
    }
    return $report
}

$first = Run-Report 'multilingual-index' @('index', $fixture)
if ($first.files -ne 10) {
    throw "Initial index included $($first.files) files instead of 10"
}
$rustSearch = Assert-SearchResult 'multilingual-search-rust' 'rust_fixture_anchor' 'src/lib.rs' 'rust' 'rust_fixture_anchor'
$productionSearch = Assert-SearchResult 'multilingual-search-typescript' 'buildProductionTelemetryPipeline' 'src/telemetry.ts' 'typescript' 'function buildProductionTelemetryPipeline'
$tsxSearch = Assert-SearchResult 'multilingual-search-tsx' 'TelemetryPanel' 'src/panel.tsx' 'tsx' 'TelemetryPanel'
$javascriptSearch = Assert-SearchResult 'multilingual-search-javascript' 'flushAuditBeacon' 'src/audit.js' 'javascript' 'flushAuditBeacon'
$jsxSearch = Assert-SearchResult 'multilingual-search-jsx' 'TelemetryBadge' 'src/badge.jsx' 'jsx' 'TelemetryBadge'
$pythonSearch = Assert-SearchResult 'multilingual-search-python' 'build_production_feature_pipeline' 'src/feature_pipeline.py' 'python' 'def build_production_feature_pipeline'
$csharpSearch = Assert-SearchResult 'multilingual-search-csharp' 'NormalizeProductionEvent' 'src/TelemetryProcessor.cs' 'csharp' 'NormalizeProductionEvent'
$csharpDecoy = @($csharpSearch.results | Where-Object {
    $_.relative_file_path -eq 'tests/TelemetryProcessorTests.cs' -and $_.source_role -eq 'test'
})
if ($csharpDecoy.Count -eq 0) { throw 'Initial index did not include the C# test decoy with test role' }
if ($csharpSearch.results[0].relative_file_path -ne 'src/TelemetryProcessor.cs') {
    throw 'Production C# implementation did not outrank the test decoy'
}
$pythonDecoy = @($pythonSearch.results | Where-Object {
    $_.relative_file_path -eq 'tests/feature_pipeline.test.py' -and
    $_.language -eq 'python'
})
if ($pythonDecoy.Count -eq 0) {
    throw 'Initial index did not include the Python test decoy as python'
}
if ($pythonSearch.results[0].relative_file_path -ne 'src/feature_pipeline.py' -or
    $pythonSearch.results[0].code -notmatch 'def build_production_feature_pipeline') {
    throw 'Production Python implementation did not outrank the test decoy'
}
$pythonProduction = @($pythonSearch.results | Where-Object {
    $_.relative_file_path -eq 'src/feature_pipeline.py' -and
    $_.code -match 'def build_production_feature_pipeline'
})[0]
if ($pythonProduction.start_line -lt 1 -or $pythonProduction.end_line -lt $pythonProduction.start_line) {
    throw "Python production result has an invalid one-based line range: $($pythonProduction.start_line)-$($pythonProduction.end_line)"
}
if ($null -eq $pythonProduction.semantic_score) {
    throw 'Python production result is missing a semantic score'
}
if ($pythonSearch.reranked -and $null -eq $pythonProduction.reranker_score) {
    throw 'Python production result is missing a reranker score while live reranking is available'
}
if ($productionSearch.results[0].relative_file_path -ne 'src/telemetry.ts' -or
    $productionSearch.results[0].code -notmatch 'function buildProductionTelemetryPipeline') {
    throw 'Production TypeScript implementation did not outrank the test decoy'
}

$unchanged = Run-Report 'multilingual-reindex' @('index', $fixture)
if ($unchanged.parsed_files -ne 0 -or $unchanged.embedded_chunks -ne 0) {
    throw 'Unchanged multilingual reindex reparsed files or recomputed embeddings'
}

[System.IO.File]::AppendAllText(
    (Join-Path $fixture 'src\telemetry.ts'),
    "`nexport const telemetryRevision = 2;`n",
    $utf8NoBom
)
$changed = Run-Report 'multilingual-typescript-update' @('index', $fixture)
if ($changed.parsed_files -ne 1) {
    throw "TypeScript update parsed $($changed.parsed_files) files instead of exactly one"
}
if ($changed.reused_chunks -eq 0) {
    throw 'TypeScript update did not reuse unchanged chunks from other languages'
}

[System.IO.File]::AppendAllText(
    (Join-Path $fixture 'src\feature_pipeline.py'),
    "`nfeature_pipeline_revision = 2`n",
    $utf8NoBom
)
$pythonChanged = Run-Report 'multilingual-python-update' @('index', $fixture)
if ($pythonChanged.parsed_files -ne 1) {
    throw "Python update parsed $($pythonChanged.parsed_files) files instead of exactly one"
}
if ($pythonChanged.reused_chunks -le 0) {
    throw 'Python-only update did not reuse unchanged chunks from other languages'
}

[System.IO.File]::AppendAllText(
    (Join-Path $fixture 'src\TelemetryProcessor.cs'),
    "`npublic static class TelemetryRevision { public const int Value = 2; }`n",
    $utf8NoBom
)
$csharpChanged = Run-Report 'multilingual-csharp-update' @('index', $fixture)
if ($csharpChanged.parsed_files -ne 1) {
    throw "C# update parsed $($csharpChanged.parsed_files) files instead of exactly one"
}
if ($csharpChanged.reused_chunks -le 0) { throw 'C# update did not reuse other-language chunks' }

Remove-Item -LiteralPath (Join-Path $fixture 'tests\TelemetryProcessorTests.cs')
$csharpDeleted = Run-Report 'multilingual-csharp-delete' @('index', $fixture)
if ($csharpDeleted.removed_files -ne 1) { throw 'C# decoy deletion did not remove exactly one file' }
$csharpDeletedSearch = Run-Report 'multilingual-search-after-csharp-delete' @('search', $fixture, 'TelemetryProcessorTests', '--top-k', '8')
if ($csharpDeletedSearch.results | Where-Object { $_.relative_file_path -eq 'tests/TelemetryProcessorTests.cs' }) {
    throw 'Deleted C# decoy chunks remain searchable'
}

Remove-Item -LiteralPath (Join-Path $fixture 'src\audit.js')
$deleted = Run-Report 'multilingual-javascript-delete' @('index', $fixture)
if ($deleted.removed_files -ne 1) {
    throw "JavaScript deletion removed $($deleted.removed_files) cached files instead of one"
}
$deletedSearch = Run-Report 'multilingual-search-after-delete' @('search', $fixture, 'flushAuditBeacon', '--top-k', '8')
if ($deletedSearch.results | Where-Object { $_.relative_file_path -eq 'src/audit.js' }) {
    throw 'Deleted JavaScript chunks remain searchable'
}

Remove-Item -LiteralPath (Join-Path $fixture 'tests\feature_pipeline.test.py')
$pythonDeleted = Run-Report 'multilingual-python-delete' @('index', $fixture)
if ($pythonDeleted.removed_files -ne 1) {
    throw "Python decoy deletion removed $($pythonDeleted.removed_files) cached files instead of one"
}
$pythonDeletedSearch = Run-Report 'multilingual-search-after-python-delete' @('search', $fixture, 'build_production_feature_pipeline', '--top-k', '8')
if ($pythonDeletedSearch.results | Where-Object { $_.relative_file_path -eq 'tests/feature_pipeline.test.py' }) {
    throw 'Deleted Python decoy chunks remain searchable'
}

$beforeFailure = Run-Report 'multilingual-status-before-failure' @('status', $fixture)
Start-Sleep -Milliseconds 1100
[System.IO.File]::AppendAllText(
    (Join-Path $fixture 'src\telemetry.ts'),
    "`nexport const failedUpdateMarker = 'not committed';`n",
    $utf8NoBom
)
[System.IO.File]::AppendAllText(
    (Join-Path $fixture 'src\feature_pipeline.py'),
    "`nfailedUpdateMarker = 'not committed'`n",
    $utf8NoBom
)
[System.IO.File]::AppendAllText(
    (Join-Path $fixture 'src\TelemetryProcessor.cs'),
    "`npublic static class FailedCSharpUpdateMarker { }`n",
    $utf8NoBom
)
$previousErrorActionPreference = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
$failureOutput = & $binary --config $failureConfigPath index $fixture 2>&1 | Out-String
$failureExitCode = $LASTEXITCODE
$ErrorActionPreference = $previousErrorActionPreference
@{
    exit_code = $failureExitCode
    output = $failureOutput.Trim()
} | ConvertTo-Json | Set-Content -LiteralPath (Join-Path $outputDir 'multilingual-failed-index.json') -Encoding utf8
if ($failureExitCode -eq 0) {
    throw 'Index unexpectedly succeeded with an unreachable embedding endpoint'
}

$status = Run-Report 'multilingual-status-after-failure' @('status', $fixture)
if (-not $status.stale) {
    throw 'Failed source update was not reported as stale'
}
if ($status.chunks -ne $beforeFailure.chunks -or
    $status.indexed_at_unix_seconds -ne $beforeFailure.indexed_at_unix_seconds) {
    throw 'Failed update replaced the previous persisted snapshot'
}
Write-Host "PASS: $($first.files) files; all seven language IDs; zero-work repeat; one-file TS, Python, and C# updates; JS, Python, and C# decoy deletions; failed-update retention. Reports: $outputDir"
