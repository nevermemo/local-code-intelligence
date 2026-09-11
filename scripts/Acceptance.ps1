param(
    [string]$Workspace = 'C:\Users\micro\Desktop\gpu-dialect-v0',
    [string]$Config
)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$binary = Join-Path $projectRoot 'target\debug\local-code-intelligence.exe'
$outputDir = Join-Path $projectRoot 'test-results'
New-Item -ItemType Directory -Force -Path $outputDir | Out-Null
$prefix = @()
if ($Config) { $prefix = @('--config', $Config) }
function Run-Report([string]$Name, [string[]]$Arguments) {
    $text = & $binary @prefix @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Name failed" }
    $text | Set-Content -LiteralPath (Join-Path $outputDir "$Name.json") -Encoding utf8
    return ($text | ConvertFrom-Json)
}
$indexed = Run-Report 'gust-index' @('index', $Workspace)
$query = 'lower syn AST expressions into generated Slang compute shader code'
$search = Run-Report 'gust-search' @('search', $Workspace, $query)
$repeat = Run-Report 'gust-reindex' @('index', $Workspace)
if ($repeat.embedded_chunks -ne 0) { throw 'Unchanged reindex recomputed embeddings' }
if (-not $search.reranked) { throw "Live reranking did not succeed: $($search.warning)" }
$implementation = @($search.results | Where-Object { $_.relative_file_path -match '^crates/gust-macros/src/slang/' })
if ($implementation.Count -le ($search.results.Count / 2)) { throw 'Translator implementation did not dominate top results' }
if (-not ($implementation | Where-Object { $_.code -match 'fn emit_expression\(' })) { throw 'Expression translator definition missing from top results' }
$search.results | Select-Object relative_file_path, start_line, end_line, semantic_score, reranker_score | Format-Table
$search.timings | Format-List
Write-Host "PASS: $($indexed.chunks) chunks; $($implementation.Count) translator results; zero new embeddings on repeat. Reports: $outputDir"
