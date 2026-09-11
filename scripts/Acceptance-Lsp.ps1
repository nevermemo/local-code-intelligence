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
$symbols = Run-Report 'gust-lsp-symbols' @('symbols', $Workspace, 'emit_expression#')
$definition = Run-Report 'gust-lsp-definition' @('definition', $Workspace, 'crates/gust-macros/src/slang/mod.rs', '266', '24')
$references = Run-Report 'gust-lsp-references' @('references', $Workspace, 'crates/gust-macros/src/slang/mod.rs', '581', '8', '--include-declaration')
if (-not ($symbols.results | Where-Object { $_.name -eq 'emit_expression' -and $_.start_line -eq 581 })) {
    throw 'workspace symbol search did not resolve emit_expression at line 581'
}
if (-not ($definition.results | Where-Object { $_.start_line -eq 581 })) {
    throw 'definition lookup did not resolve the call at line 266 to line 581'
}
if ($references.results.Count -lt 2) {
    throw 'reference lookup returned too few locations'
}
Write-Host "PASS: symbol, definition, and $($references.results.Count) reference locations. Reports: $outputDir"
