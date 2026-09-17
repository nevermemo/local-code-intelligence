<#
.SYNOPSIS
    Acceptance test for persistent LCI `serve` process reuse and forced-death
    recovery of the C# language server child process, over the real MCP HTTP
    endpoint (port 8768, owned for the duration of this script only).

.DESCRIPTION
    Builds its own small real C# fixture (dotnet sln/csproj/restore/build),
    starts `local-code-intelligence.exe serve` as an acceptance-owned process,
    performs the MCP JSON-RPC-over-HTTP handshake (initialize / initialized /
    tools/call), and exercises `search_symbols` twice to prove the same
    csharp-ls child process is reused across requests. It then force-kills
    that child, issues a third request, and verifies LCI recovers with a new
    child csharp-ls process (no repeated restart loop, exactly one
    replacement). Only processes owned by this script are ever terminated.
#>
param(
    [string]$CSharpLanguageServer = 'C:\Users\micro\.dotnet\tools\csharp-ls.exe',
    [switch]$KeepFixtureOnFailure
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$binary = Join-Path $projectRoot 'target\debug\local-code-intelligence.exe'
$outputDir = Join-Path $projectRoot 'test-results'
$evidencePath = Join-Path $outputDir 'csharp-lsp-recovery.json'
$fixture = Join-Path ([System.IO.Path]::GetTempPath()) "lci-csharp-recovery-$PID"
$dataDir = Join-Path ([System.IO.Path]::GetTempPath()) "lci-csharp-recovery-data-$PID"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)
$base = 'http://127.0.0.1:8768'

New-Item -ItemType Directory -Force -Path $outputDir | Out-Null

$evidence = [ordered]@{
    status = 'running'
    stage = 'start'
    failed_stage = $null
    base_url = $base
    lci_pid = $null
    first_child_pid = $null
    reused_child_pid = $null
    reuse_confirmed = $false
    killed_child_pid = $null
    recovery_child_pid = $null
    recovery_confirmed = $false
    replacement_process_count = $null
    workspace_symbol_results = @()
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

function Invoke-DotNet([string[]]$CommandArguments) {
    $output = & $dotnetCommand.Source @CommandArguments 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) {
        throw "dotnet failed (exit $LASTEXITCODE): $($CommandArguments -join ' ')`n$($output.Trim())"
    }
}

# Returns the csharp-ls.exe child process(es) whose ParentProcessId is $ParentPid.
function Get-CSharpLsChildren {
    param([int]$ParentPid)
    Get-CimInstance Win32_Process -Filter "Name = 'csharp-ls.exe'" |
        Where-Object { $_.ParentProcessId -eq $ParentPid } |
        Select-Object -ExpandProperty ProcessId
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
    Set-Stage 'resolve-prerequisites'
    if (-not (Test-Path -LiteralPath $CSharpLanguageServer)) {
        $evidence.status = 'prerequisite-unavailable'
        Save-Evidence
        Write-Host "PREREQUISITE_UNAVAILABLE: csharp-ls not found at $CSharpLanguageServer. Evidence: $evidencePath"
        exit 2
    }
    $dotnetCommand = Get-Command dotnet -ErrorAction SilentlyContinue
    if (-not $dotnetCommand) { throw 'dotnet SDK is required for the real C# fixture' }
    if (-not (Test-Path -LiteralPath $binary)) { throw "Debug binary missing: $binary" }

    Set-Stage 'create-fixture'
    New-Item -ItemType Directory -Force -Path (Join-Path $fixture 'src') | Out-Null
    Write-Utf8 (Join-Path $fixture 'CSharpAcceptance.csproj') @'
<Project Sdk="Microsoft.NET.Sdk">
  <PropertyGroup>
    <OutputType>Library</OutputType>
    <TargetFramework>net8.0</TargetFramework>
    <ImplicitUsings>enable</ImplicitUsings>
    <Nullable>enable</Nullable>
  </PropertyGroup>
</Project>
'@
    Write-Utf8 (Join-Path $fixture 'src\Production.cs') @'
namespace Acceptance;

public interface ICalculator { int Add(int left, int right); }

public sealed class Calculator : ICalculator
{
    public int Add(int left, int right) => left + right;
    public int Use() => Add(2, 3);
}
'@
    $projectPath = Join-Path $fixture 'CSharpAcceptance.csproj'
    $solutionPath = Join-Path $fixture 'CSharpAcceptance.sln'
    Invoke-DotNet @('new', 'sln', '--format', 'sln', '--name', 'CSharpAcceptance', '--output', $fixture)
    Invoke-DotNet @('solution', $solutionPath, 'add', $projectPath)
    Invoke-DotNet @('restore', $solutionPath)
    Invoke-DotNet @('build', $solutionPath, '--no-restore')

    Set-Stage 'construct-config'
    $normalizedData = $dataDir.Replace('\', '/')
    $configPath = Join-Path $fixture 'config.toml'
    $configArgs = "data_dir = '$normalizedData'`nlsp_timeout_seconds = 60`n[csharp]`npath = '$($CSharpLanguageServer.Replace('\', '/'))'`nargs = ['--solution', 'CSharpAcceptance.sln']`n"
    Write-Utf8 $configPath $configArgs

    Set-Stage 'start-serve'
    $stdoutPath = Join-Path $fixture 'serve.stdout.log'
    $stderrPath = Join-Path $fixture 'serve.stderr.log'
    $lci = Start-Process -FilePath $binary `
        -ArgumentList @('--config', $configPath, 'serve') `
        -WorkingDirectory $fixture -WindowStyle Hidden -PassThru `
        -RedirectStandardOutput $stdoutPath -RedirectStandardError $stderrPath
    $evidence.lci_pid = $lci.Id
    Save-Evidence

    # Bounded wait for /health to become ready.
    Set-Stage 'wait-health'
    $healthy = $false
    for ($i = 0; $i -lt 30; $i++) {
        try {
            $health = Invoke-RestMethod -Uri "$base/health" -Method Get -TimeoutSec 2
            if ($health.status -eq 'ok') { $healthy = $true; break }
        } catch {}
        Start-Sleep -Milliseconds 500
    }
    if (-not $healthy) { throw '/health did not report ok within the bounded wait' }

    Set-Stage 'mcp-initialize'
    $initResponse = Invoke-WebRequest -Uri "$base/mcp" -Method Post -ContentType 'application/json' `
        -Headers @{ 'Accept' = 'application/json, text/event-stream' } `
        -Body (@{ jsonrpc = '2.0'; id = 1; method = 'initialize'; params = @{ protocolVersion = '2025-03-26'; capabilities = @{}; clientInfo = @{ name = 'csharp-recovery-acceptance'; version = '1' } } } | ConvertTo-Json -Depth 12) `
        -UseBasicParsing
    $sessionId = $initResponse.Headers['mcp-session-id']
    if ($sessionId -is [System.Array]) { $sessionId = $sessionId[0] }
    if (-not $sessionId) { throw 'no mcp-session-id header returned from initialize' }
    Invoke-WebRequest -Uri "$base/mcp" -Method Post -ContentType 'application/json' `
        -Headers @{ 'Accept' = 'application/json, text/event-stream'; 'mcp-session-id' = $sessionId } `
        -Body (@{ jsonrpc = '2.0'; method = 'notifications/initialized' } | ConvertTo-Json -Depth 12) `
        -UseBasicParsing | Out-Null

    Set-Stage 'index-workspace'
    $indexBody = Invoke-McpToolCall -SessionId $sessionId -Id 2 -Name 'index_workspace' -Arguments @{ workspace_path = $fixture }
    if ($indexBody -match '"isError":true') { throw "index_workspace returned an error: $indexBody" }

    Set-Stage 'first-navigation'
    $args1 = @{ workspace_path = $fixture; query = 'Calculator' }
    $body1 = Invoke-McpToolCall -SessionId $sessionId -Id 3 -Name 'search_symbols' -Arguments $args1
    if ($body1 -match '"isError":true') { throw "search_symbols returned an error: $body1" }
    $evidence.workspace_symbol_results = @($body1 | Select-String -Pattern 'Calculator' -AllMatches).Matches.Count
    if ($evidence.workspace_symbol_results -lt 1) { throw "first search_symbols response did not contain Calculator: $body1" }

    # Bound the wait for the LCI-owned csharp-ls child to appear.
    $firstChild = $null
    for ($i = 0; $i -lt 20; $i++) {
        $children = @(Get-CSharpLsChildren -ParentPid $evidence.lci_pid)
        if ($children.Count -gt 0) { $firstChild = $children[0]; break }
        Start-Sleep -Milliseconds 500
    }
    if (-not $firstChild) { throw 'no csharp-ls child process appeared under the owned LCI process' }
    $evidence.first_child_pid = $firstChild
    Save-Evidence

    Set-Stage 'second-navigation-reuse'
    $body2 = Invoke-McpToolCall -SessionId $sessionId -Id 4 -Name 'search_symbols' -Arguments $args1
    if ($body2 -match '"isError":true') { throw "search_symbols (reuse) returned an error: $body2" }
    if ($body2 -notmatch 'Calculator') { throw "search_symbols (reuse) did not contain Calculator: $body2" }
    $reusedChildren = @(Get-CSharpLsChildren -ParentPid $evidence.lci_pid)
    $evidence.reused_child_pid = if ($reusedChildren.Count -gt 0) { $reusedChildren[0] } else { $null }
    $evidence.reuse_confirmed = ($reusedChildren.Count -eq 1) -and ($reusedChildren[0] -eq $firstChild)
    Save-Evidence
    if (-not $evidence.reuse_confirmed) { throw "process was not reused: first=$firstChild reused=$($reusedChildren -join ',')" }

    Set-Stage 'forced-death-recovery'
    $evidence.killed_child_pid = $firstChild
    Stop-Process -Id $firstChild -Force -ErrorAction SilentlyContinue
    Save-Evidence

    $body3 = Invoke-McpToolCall -SessionId $sessionId -Id 5 -Name 'search_symbols' -Arguments $args1
    if ($body3 -match '"isError":true') { throw "search_symbols (recovery) returned an error: $body3" }
    if ($body3 -notmatch 'Calculator') { throw "search_symbols (recovery) did not contain Calculator: $body3" }
    $recoveryChildren = $null
    for ($i = 0; $i -lt 20; $i++) {
        $recoveryChildren = @(Get-CSharpLsChildren -ParentPid $evidence.lci_pid)
        if ($recoveryChildren.Count -gt 0 -and $recoveryChildren[0] -ne $firstChild) { break }
        Start-Sleep -Milliseconds 500
    }
    $evidence.recovery_child_pid = if ($recoveryChildren.Count -gt 0) { $recoveryChildren[0] } else { $null }
    $evidence.replacement_process_count = $recoveryChildren.Count
    $evidence.recovery_confirmed = ($recoveryChildren.Count -eq 1) -and ($recoveryChildren[0] -ne $firstChild)
    Save-Evidence
    if (-not $evidence.recovery_confirmed) { throw "recovery not confirmed: killed=$firstChild candidates=$($recoveryChildren -join ',')" }

    $evidence.status = 'passed'
    Save-Evidence
}
catch {
    $evidence.status = 'failed'
    $evidence.failed_stage = $evidence.stage
    $evidence.error = $_.Exception.Message
    Save-Evidence
}
finally {
    $evidence.stage = 'cleanup'
    # Stop only the LCI process we started and its direct language-server
    # children. Windows PowerShell does not expose Process.Kill(bool), so
    # invoking that overload can silently leave the acceptance server alive.
    if ($lci -and -not $lci.HasExited) {
        try {
            @(Get-CimInstance Win32_Process -Filter "ParentProcessId = $($lci.Id)" -ErrorAction SilentlyContinue) |
                ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
        } catch {}
        try { Stop-Process -Id $lci.Id -Force -ErrorAction SilentlyContinue } catch {}
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
    if (-not $evidence.cleanup.lci_removed -or -not $evidence.cleanup.fixture_removed -or -not $evidence.cleanup.data_removed) {
        $evidence.status = 'failed'
        $evidence.failed_stage = 'cleanup'
        $evidence.error = "cleanup incomplete: lci_removed=$($evidence.cleanup.lci_removed), fixture_removed=$($evidence.cleanup.fixture_removed), data_removed=$($evidence.cleanup.data_removed)"
    }
    Save-Evidence
}

if ($evidence.status -eq 'passed') {
    Write-Host "PASS: C# LSP persistent-reuse and recovery acceptance. Evidence: $evidencePath"
    exit 0
}
Write-Host "FAIL at stage '$($evidence.failed_stage)': $($evidence.error). Evidence: $evidencePath"
exit 1
