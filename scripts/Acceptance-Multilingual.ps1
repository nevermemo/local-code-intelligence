$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$binary = Join-Path $projectRoot 'target\debug\local-code-intelligence.exe'
$outputDir = Join-Path $projectRoot 'test-results'
$fixture = Join-Path $outputDir 'multilingual-fixture'
$dataDir = Join-Path $outputDir 'multilingual-data'
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

$normalizedDataDir = $dataDir.Replace('\', '/')
Write-Utf8 $configPath @"
embedding_url = "http://localhost:8766/v1"
embedding_model = "qwen3-embedding-8b"
reranker_url = "http://localhost:8767/rerank"
reranker_model = "qwen3-reranker-4b"
data_dir = "$normalizedDataDir"
"@
Write-Utf8 $failureConfigPath @"
embedding_url = "http://127.0.0.1:1/v1"
embedding_model = "qwen3-embedding-8b"
reranker_url = "http://localhost:8767/rerank"
reranker_model = "qwen3-reranker-4b"
data_dir = "$normalizedDataDir"
embedding_timeout_seconds = 2
"@

function Run-Report([string]$Name, [string[]]$Arguments) {
    $text = & $binary --config $configPath @Arguments
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
$rustSearch = Assert-SearchResult 'multilingual-search-rust' 'rust_fixture_anchor' 'src/lib.rs' 'rust' 'rust_fixture_anchor'
$productionSearch = Assert-SearchResult 'multilingual-search-typescript' 'buildProductionTelemetryPipeline' 'src/telemetry.ts' 'typescript' 'function buildProductionTelemetryPipeline'
$tsxSearch = Assert-SearchResult 'multilingual-search-tsx' 'TelemetryPanel' 'src/panel.tsx' 'tsx' 'TelemetryPanel'
$javascriptSearch = Assert-SearchResult 'multilingual-search-javascript' 'flushAuditBeacon' 'src/audit.js' 'javascript' 'flushAuditBeacon'
$jsxSearch = Assert-SearchResult 'multilingual-search-jsx' 'TelemetryBadge' 'src/badge.jsx' 'jsx' 'TelemetryBadge'
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

Remove-Item -LiteralPath (Join-Path $fixture 'src\audit.js')
$deleted = Run-Report 'multilingual-javascript-delete' @('index', $fixture)
if ($deleted.removed_files -ne 1) {
    throw "JavaScript deletion removed $($deleted.removed_files) cached files instead of one"
}
$deletedSearch = Run-Report 'multilingual-search-after-delete' @('search', $fixture, 'flushAuditBeacon', '--top-k', '8')
if ($deletedSearch.results | Where-Object { $_.relative_file_path -eq 'src/audit.js' }) {
    throw 'Deleted JavaScript chunks remain searchable'
}

[System.IO.File]::AppendAllText(
    (Join-Path $fixture 'src\telemetry.ts'),
    "`nexport const failedUpdateMarker = 'not committed';`n",
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

$retained = Run-Report 'multilingual-search-retained-snapshot' @('search', $fixture, 'buildProductionTelemetryPipeline', '--top-k', '8')
$retainedProduction = @($retained.results | Where-Object {
    $_.relative_file_path -eq 'src/telemetry.ts' -and
    $_.code -match 'function buildProductionTelemetryPipeline'
})
if ($retainedProduction.Count -eq 0) {
    throw 'Previous TypeScript snapshot was not searchable after the failed update'
}
if ($retainedProduction | Where-Object { $_.code -match 'failedUpdateMarker' }) {
    throw 'Failed update leaked into the persisted snapshot'
}
$status = Run-Report 'multilingual-status-after-failure' @('status', $fixture)
if (-not $status.stale) {
    throw 'Failed source update was not reported as stale'
}

Write-Host "PASS: $($first.files) files; all five language IDs; zero-work repeat; one-file TS update; JS deletion; failed-update retention. Reports: $outputDir"