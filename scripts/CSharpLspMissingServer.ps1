<#
.SYNOPSIS
    Missing-server and provider-isolation acceptance for the optional C# language
    server, using a disposable configuration with a definitely-missing C# executable.

.DESCRIPTION
    Starts an acceptance-owned `local-code-intelligence.exe serve` (port 8768,
    owned for the duration of this script) with `[csharp].path` pointing at a
    binary that does not exist, plus a real `rust-analyzer` entry so Rust LSP
    behavior can be checked for isolation. Verifies:
      - /ready remains ready.
      - search_code over a C# query still returns results (semantic/lexical) and
        reports a warning identifying the optional C# provider.
      - find_definition/find_references on a C# file return a clear tooling error.
      - service_status reports C# tooling as optional/degraded.
      - A non-C# filtered search (language=rust) does not start csharp-ls and does
        not mention csharp in its warnings, and Rust LSP results are not suppressed
        by the failed C# provider.
#>
param(
    [switch]$KeepFixtureOnFailure
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$binary = Join-Path $projectRoot 'target\debug\local-code-intelligence.exe'
$outputDir = Join-Path $projectRoot 'test-results'
$evidencePath = Join-Path $outputDir 'csharp-lsp-missing-server.json'
$fixture = Join-Path ([System.IO.Path]::GetTempPath()) "lci-csharp-missing-$PID"
$dataDir = Join-Path ([System.IO.Path]::GetTempPath()) "lci-csharp-missing-data-$PID"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$base = 'http://127.0.0.1:8768'

New-Item -ItemType Directory -Force -Path $outputDir | Out-Null

$evidence = [ordered]@{
    status = 'running'
    stage = 'start'
    failed_stage = $null
    lci_pid = $null
    ready_while_csharp_missing = $null
    search_code_returned_results = $null
    search_code_warning_mentions_csharp = $null
    definition_is_tooling_error = $null
    service_status_csharp_optional_degraded = $null
    rust_filtered_search_started_csharp = $null
    rust_filtered_search_mentions_csharp = $null
    rust_filtered_search_returned_rust_results = $null
    error = $null
    cleanup = [ordered]@{}
}

function Write-Utf8([string]$Path, [string]$Content) {
    [System.IO.File]::WriteAllText($Path, $Content, $utf8NoBom)
}

function Save-Evidence {
    $json = $evidence | ConvertTo-Json -Depth 12
    for ($attempt = 1; $attempt -le 5; $attempt++) {
        try { Set-Content -LiteralPath $evidencePath -Value $json -Encoding utf8; return }
        catch { if ($attempt -eq 5) { throw }; Start-Sleep -Milliseconds 100 }
    }
}

function Set-Stage([string]$Name) {
    $evidence.stage = $Name
    Save-Evidence
}

function Invoke-McpToolCall {
    param([string]$SessionId, [int]$Id, [string]$Name, [hashtable]$Arguments)
    $payload = @{ jsonrpc = '2.0'; id = $Id; method = 'tools/call'; params = @{ name = $Name; arguments = $Arguments } }
    $response = Invoke-WebRequest -Uri "$base/mcp" -Method Post -ContentType 'application/json' `
        -Headers @{ 'Accept' = 'application/json, text/event-stream'; 'mcp-session-id' = $SessionId } `
        -Body ($payload | ConvertTo-Json -Depth 12) -UseBasicParsing
    return $response.Content
}

try {
    Set-Stage 'create-fixture'
    New-Item -ItemType Directory -Force -Path (Join-Path $fixture 'src') | Out-Null
    Write-Utf8 (Join-Path $fixture 'src\Production.cs') @'
namespace Acceptance;

public interface ICalculator { int Add(int left, int right); }

public sealed class Calculator : ICalculator
{
    public int Add(int left, int right) => left + right;
}
'@
    Write-Utf8 (Join-Path $fixture 'src\lib.rs') @'
pub fn add(left: i32, right: i32) -> i32 {
    left + right
}
'@

    Set-Stage 'construct-config'
    $rustAnalyzer = (Get-Command rust-analyzer -ErrorAction SilentlyContinue)
    $normalizedData = $dataDir.Replace('\', '/')
    $configPath = Join-Path $fixture 'config.toml'
    $lines = @(
        "data_dir = '$normalizedData'"
        "lsp_timeout_seconds = 10"
    )
    if ($rustAnalyzer) {
        $lines += "rust_analyzer_path = '$($rustAnalyzer.Source.Replace('\', '/'))'"
    }
    $lines += "[csharp]"
    $lines += "path = 'definitely-missing-csharp-ls'"
    $lines += "args = ['--solution', 'CSharpAcceptance.sln']"
    Write-Utf8 $configPath (($lines -join "`n") + "`n")

    Set-Stage 'start-serve'
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $binary
    $psi.Arguments = "--config `"$configPath`" serve"
    $psi.WorkingDirectory = $fixture
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $lci = [System.Diagnostics.Process]::new()
    $lci.StartInfo = $psi
    $lci.Start() | Out-Null
    $evidence.lci_pid = $lci.Id
    Save-Evidence

    Set-Stage 'wait-ready'
    $ready = $false
    for ($i = 0; $i -lt 30; $i++) {
        try {
            $readyResponse = Invoke-RestMethod -Uri "$base/ready" -Method Get -TimeoutSec 2
            if ($readyResponse.status -eq 'ready' -or $readyResponse.ready -eq $true) { $ready = $true; break }
        } catch {}
        Start-Sleep -Milliseconds 500
    }
    $evidence.ready_while_csharp_missing = $ready
    Save-Evidence
    if (-not $ready) { throw '/ready did not report ready while csharp tooling was missing (required deps should still be healthy)' }

    Set-Stage 'mcp-initialize'
    $initResponse = Invoke-WebRequest -Uri "$base/mcp" -Method Post -ContentType 'application/json' `
        -Headers @{ 'Accept' = 'application/json, text/event-stream' } `
        -Body (@{ jsonrpc = '2.0'; id = 1; method = 'initialize'; params = @{ protocolVersion = '2025-03-26'; capabilities = @{}; clientInfo = @{ name = 'csharp-missing-acceptance'; version = '1' } } } | ConvertTo-Json -Depth 12) `
        -UseBasicParsing
    $sessionId = $initResponse.Headers['mcp-session-id']
    if ($sessionId -is [System.Array]) { $sessionId = $sessionId[0] }
    Invoke-WebRequest -Uri "$base/mcp" -Method Post -ContentType 'application/json' `
        -Headers @{ 'Accept' = 'application/json, text/event-stream'; 'mcp-session-id' = $sessionId } `
        -Body (@{ jsonrpc = '2.0'; method = 'notifications/initialized' } | ConvertTo-Json -Depth 12) `
        -UseBasicParsing | Out-Null

    Set-Stage 'index-workspace'
    $indexBody = Invoke-McpToolCall -SessionId $sessionId -Id 2 -Name 'index_workspace' -Arguments @{ workspace_path = $fixture }
    if ($indexBody -match '"isError":true') { throw "index_workspace returned an error: $indexBody" }

    Set-Stage 'search-code-csharp'
    $searchBody = Invoke-McpToolCall -SessionId $sessionId -Id 3 -Name 'search_code' -Arguments @{ workspace_path = $fixture; query = 'Calculator Add' }
    $evidence.search_code_returned_results = ($searchBody -match 'Calculator')
    $evidence.search_code_warning_mentions_csharp = ($searchBody -match '(?i)csharp-ls|csharp language server|csharp.{0,20}(provider|tooling|warning)')
    Save-Evidence
    if (-not $evidence.search_code_returned_results) { throw "search_code did not return semantic/lexical C# results while csharp-ls was missing: $searchBody" }
    if (-not $evidence.search_code_warning_mentions_csharp) { throw "search_code did not report the unavailable optional C# provider: $searchBody" }

    Set-Stage 'definition-csharp'
    $definitionBody = Invoke-McpToolCall -SessionId $sessionId -Id 4 -Name 'find_definition' -Arguments @{ workspace_path = $fixture; relative_file_path = 'src/Production.cs'; line = 7; character = 15 }
    $evidence.definition_is_tooling_error = ($definitionBody -match '"isError":true')
    Save-Evidence
    if (-not $evidence.definition_is_tooling_error) { throw "find_definition did not return a clear tooling error while csharp-ls was missing: $definitionBody" }

    Set-Stage 'service-status'
    $statusBody = Invoke-McpToolCall -SessionId $sessionId -Id 5 -Name 'service_status' -Arguments @{}
    $evidence.service_status_csharp_optional_degraded = ($statusBody -match '(?i)csharp') -and ($statusBody -match '(?i)optional|degraded|unavailable|missing')
    Save-Evidence
    if (-not $evidence.service_status_csharp_optional_degraded) { throw "service_status did not report C# tooling as optional/degraded: $statusBody" }

    Set-Stage 'rust-filtered-search'
    $beforeChildren = @(Get-CimInstance Win32_Process -Filter "Name = 'csharp-ls.exe'" | Where-Object { $_.ParentProcessId -eq $evidence.lci_pid })
    $rustSearchBody = Invoke-McpToolCall -SessionId $sessionId -Id 6 -Name 'search_code' -Arguments @{ workspace_path = $fixture; query = 'add'; languages = @('rust') }
    Start-Sleep -Milliseconds 500
    $afterChildren = @(Get-CimInstance Win32_Process -Filter "Name = 'csharp-ls.exe'" | Where-Object { $_.ParentProcessId -eq $evidence.lci_pid })
    $evidence.rust_filtered_search_started_csharp = ($afterChildren.Count -gt $beforeChildren.Count)
    # Match the specific provider/tooling identifier, not a bare "csharp" substring:
    # the fixture's own temp directory name (lci-csharp-missing-<pid>) is echoed back
    # in file paths and would otherwise cause a false positive on a plain "csharp" match.
    $evidence.rust_filtered_search_mentions_csharp = ($rustSearchBody -match '(?i)csharp-ls|csharp language server|csharp.{0,20}(provider|tooling|warning)')
    $evidence.rust_filtered_search_returned_rust_results = ($rustSearchBody -match '(?i)"language":"rust"')
    Save-Evidence
    if ($evidence.rust_filtered_search_started_csharp) { throw 'a non-C# filtered search unexpectedly started csharp-ls' }
    if ($evidence.rust_filtered_search_mentions_csharp) { throw 'a non-C# filtered search produced an irrelevant csharp warning' }
    if (-not $evidence.rust_filtered_search_returned_rust_results) { throw "the Rust-filtered search did not return Rust results: $rustSearchBody" }

    $evidence.status = 'passed'
    Save-Evidence
    Write-Host "PASS: C# missing-server and provider-isolation acceptance. Evidence: $evidencePath"
}
catch {
    $evidence.status = 'failed'
    $evidence.failed_stage = $evidence.stage
    $evidence.error = $_.Exception.Message
    Save-Evidence
    Write-Host "FAIL at stage '$($evidence.stage)': $($_.Exception.Message). Evidence: $evidencePath"
    exit 1
}
finally {
    $evidence.stage = 'cleanup'
    if ($lci -and -not $lci.HasExited) {
        try { $lci.Kill($true) } catch {}
        try { $lci.WaitForExit(5000) | Out-Null } catch {}
    }
    $evidence.cleanup.lci_removed = ($null -eq $lci) -or $lci.HasExited
    if ((-not $KeepFixtureOnFailure -or $evidence.status -eq 'passed') -and (Test-Path -LiteralPath $fixture)) {
        Remove-Item -LiteralPath $fixture -Recurse -Force -ErrorAction SilentlyContinue
        $evidence.cleanup.fixture_removed = -not (Test-Path -LiteralPath $fixture)
    }
    if (Test-Path -LiteralPath $dataDir) {
        Remove-Item -LiteralPath $dataDir -Recurse -Force -ErrorAction SilentlyContinue
    }
    $evidence.cleanup.data_removed = -not (Test-Path -LiteralPath $dataDir)
    Save-Evidence
}
