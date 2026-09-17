param(
    [string]$CSharpLanguageServer = 'csharp-ls',
    [switch]$StopAfterIndex,
    [switch]$KeepFixtureOnFailure,
    [switch]$ProbeOnly
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$binary = Join-Path $projectRoot 'target\debug\local-code-intelligence.exe'
$outputDir = Join-Path $projectRoot 'test-results'
$fixture = Join-Path ([System.IO.Path]::GetTempPath()) "lci-csharp-lsp-$PID"
$dataDir = Join-Path ([System.IO.Path]::GetTempPath()) "lci-csharp-lsp-data-$PID"
$evidencePath = Join-Path $outputDir 'csharp-lsp-acceptance.json'
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

New-Item -ItemType Directory -Force -Path $outputDir | Out-Null
$resolved = Get-Command $CSharpLanguageServer -ErrorAction SilentlyContinue
$evidence = [ordered]@{
    server = $CSharpLanguageServer
    executable = if ($resolved) { $resolved.Source } else { $null }
    version = $null
    dotnet = $null
    status = $null
    stage = 'resolve-prerequisites'
    failed_stage = $null
    command = $null
    exit_code = $null
    elapsed_ms = $null
    error = $null
    preexisting_process_ids = [ordered]@{}
    cleanup = [ordered]@{}
    results = [ordered]@{}
}

function Write-Utf8([string]$Path, [string]$Content) {
    [System.IO.File]::WriteAllText($Path, $Content, $utf8NoBom)
}

# Computes the zero-based UTF-16 character offset of $Needle on the given
# one-based source line, so definition/references requests target the actual
# call site instead of a hand-guessed column.
function Get-CallSitePosition {
    param([string]$Content, [int]$Line, [string]$Needle)
    $lines = $Content -split "`r`n|`n"
    $lineText = $lines[$Line - 1]
    $index = $lineText.IndexOf($Needle)
    if ($index -lt 0) { throw "needle '$Needle' not found on line $Line" }
    return [ordered]@{ line = $Line; character = $index }
}

function Save-Evidence {
    $evidence | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $evidencePath -Encoding utf8
}

function Assert-Acceptance([bool]$Condition, [string]$Message) {
    if (-not $Condition) { throw $Message }
}

function Set-Stage([string]$Name, [string[]]$CommandArguments = @()) {
    $evidence.stage = $Name
    $evidence.command = @($CommandArguments)
    $evidence.exit_code = $null
    $evidence.elapsed_ms = $null
    Save-Evidence
}

function Invoke-DotNet([string]$Stage, [string[]]$CommandArguments) {
    Set-Stage $Stage $CommandArguments
    $started = [System.Diagnostics.Stopwatch]::StartNew()
    $output = & $dotnetCommand.Source @CommandArguments 2>&1 | Out-String
    $evidence.exit_code = $LASTEXITCODE
    $evidence.elapsed_ms = $started.ElapsedMilliseconds
    if ($LASTEXITCODE -ne 0) {
        $evidence.error = $output.Trim().Substring(0, [Math]::Min(2000, $output.Trim().Length))
        Save-Evidence
        throw "$Stage failed: $($CommandArguments -join ' ')"
    }
    Save-Evidence
    return $output
}

try {
    # Optional standalone protocol probe (independent of the LCI binary). When
    # -ProbeOnly is set, run the probe and stop; its bounded JSON evidence is
    # written to test-results/csharp-lsp-standalone-probe.json.
    if ($ProbeOnly) {
        $probeScript = Join-Path $PSScriptRoot 'CSharpLspProbe.ps1'
        & $probeScript
        $probeExit = $LASTEXITCODE
        $probeEvidence = Join-Path $outputDir 'csharp-lsp-standalone-probe.json'
        $probeResult = if (Test-Path -LiteralPath $probeEvidence) {
            try { Get-Content -Raw -LiteralPath $probeEvidence | ConvertFrom-Json } catch { $null }
        } else { $null }
        $evidence.status = if ($probeExit -eq 0) { 'passed-probe-only' } else { 'failed-probe-only' }
        $evidence.results.standalone_probe = $probeResult
        $evidence | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $evidencePath -Encoding utf8
        if ($probeExit -eq 0) {
            Write-Host "PASS: standalone C# LSP probe. Evidence: $probeEvidence"
        } else {
            Write-Host "FAIL: standalone C# LSP probe (exit $probeExit). Evidence: $probeEvidence"
        }
        exit $probeExit
    }
    $evidence.preexisting_process_ids.csharp_ls = @(
        Get-Process -Name 'csharp-ls' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id
    )
    $evidence.preexisting_process_ids.local_code_intelligence = @(
        Get-Process -Name 'local-code-intelligence' -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Id
    )
    Set-Stage 'resolve-prerequisites'
    if (-not $resolved) {
        $evidence.status = 'prerequisite-unavailable'
        $evidence | ConvertTo-Json -Depth 8 | Set-Content -LiteralPath $evidencePath -Encoding utf8
        Write-Host "PREREQUISITE_UNAVAILABLE: csharp-ls was not found. Evidence: $evidencePath"
        exit 2
    }
    $evidence.version = (& $resolved.Source --version 2>&1 | Out-String).Trim()
    $dotnetCommand = Get-Command dotnet -ErrorAction SilentlyContinue
    if (-not $dotnetCommand) { throw 'dotnet SDK is required for the real C# fixture' }
    $evidence.dotnet = (& $dotnetCommand.Source --version 2>&1 | Out-String).Trim()
    if (-not (Test-Path -LiteralPath $binary)) { throw "Debug binary missing: $binary" }

    Set-Stage 'create-fixture'
    New-Item -ItemType Directory -Force -Path (Join-Path $fixture 'src') | Out-Null
    New-Item -ItemType Directory -Force -Path (Join-Path $fixture 'tests') | Out-Null
    Write-Utf8 (Join-Path $fixture '.gitignore') "bin/`nobj/`n"
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
    $productionSource = @'
namespace Acceptance;

public interface ICalculator { int Add(int left, int right); }

public sealed class Calculator : ICalculator
{
    // Production billing arithmetic implementation.
    public int Add(int left, int right) => left + right;
    public int Use() => Add(2, 3);
}
'@
    Write-Utf8 (Join-Path $fixture 'src\Production.cs') $productionSource
    $callSiteSource = @'
namespace Acceptance;

public static class CallSite
{
    public static int Run(ICalculator calculator) => calculator.Add(1, 2);
}
'@
    Write-Utf8 (Join-Path $fixture 'src\CallSite.cs') $callSiteSource
    Write-Utf8 (Join-Path $fixture 'tests\CalculatorTests.cs') @'
namespace Acceptance.Tests;

// Test-only Calculator usage and documentation example.
public static class CalculatorTests
{
    public static bool Example() => new Acceptance.Calculator().Add(1, 2) == 3;
}
'@
    $projectPath = Join-Path $fixture 'CSharpAcceptance.csproj'
    $solutionPath = Join-Path $fixture 'CSharpAcceptance.sln'
    Invoke-DotNet 'create-fixture' @('new', 'sln', '--format', 'sln', '--name', 'CSharpAcceptance', '--output', $fixture)
    Invoke-DotNet 'create-fixture' @('solution', $solutionPath, 'add', $projectPath)
    Invoke-DotNet 'restore-fixture' @('restore', $solutionPath)
    Invoke-DotNet 'build-fixture' @('build', $solutionPath, '--no-restore')

    Set-Stage 'construct-config'
    $normalizedData = $dataDir.Replace('\', '/')
    $configPath = Join-Path $fixture 'config.toml'
    $configArgs = "data_dir = '$normalizedData'`nlsp_timeout_seconds = 60`n[csharp]`npath = '$($resolved.Source.Replace('\', '/'))'`nargs = ['--solution', 'CSharpAcceptance.sln']`n"
    Write-Utf8 $configPath $configArgs
    $prefix = @('--config', $configPath)
    function Run-Json {
        param(
            [Parameter(Mandatory)]
            [string]$Stage,
            [Parameter(Mandatory)]
            [string[]]$CommandArguments
        )
        Set-Stage $Stage $CommandArguments
        $started = [System.Diagnostics.Stopwatch]::StartNew()
        $stderrPath = Join-Path $fixture "$Stage.stderr.txt"
        $previousErrorPreference = $ErrorActionPreference
        try {
            # Windows PowerShell surfaces redirected native stderr as error
            # records. Keep it non-terminating here so the harness can inspect
            # the real process exit code and bounded stderr itself.
            $ErrorActionPreference = 'Continue'
            $text = & $binary @prefix @CommandArguments 2> $stderrPath
            $nativeExitCode = $LASTEXITCODE
        } finally {
            $ErrorActionPreference = $previousErrorPreference
        }
        $evidence.exit_code = $nativeExitCode
        $evidence.elapsed_ms = $started.ElapsedMilliseconds
        $stderr = if (Test-Path $stderrPath) { Get-Content -Raw -LiteralPath $stderrPath } else { '' }
        if ($nativeExitCode -ne 0) {
            $bounded = (($stderr + "`n" + (($text | Out-String).Trim())).Trim())
            $evidence.error = $bounded.Substring(0, [Math]::Min(2000, $bounded.Length))
            Save-Evidence
            throw "LCI command failed: $($CommandArguments -join ' ')"
        }
        Save-Evidence
        return ($text | ConvertFrom-Json)
    }

    $index = Run-Json -Stage 'index' -CommandArguments @('index', $fixture)
    $evidence.results.index_files = $index.files
    $evidence.results.index_file_count = [int]$index.files
    Assert-Acceptance ($evidence.results.index_file_count -eq 3) "expected exactly 3 indexed C# files, got $($evidence.results.index_file_count)"
    Save-Evidence
    if ($StopAfterIndex) {
        $evidence.status = 'passed-index-only'
        Save-Evidence
        Write-Host "PASS: C# indexing acceptance. Evidence: $evidencePath"
        return
    }
    $symbols = Run-Json -Stage 'workspace-symbol' -CommandArguments @('symbols', $fixture, 'Calculator')
    $definitionPos = Get-CallSitePosition -Content $callSiteSource -Line 5 -Needle 'Add'
    $definition = Run-Json -Stage 'definition' -CommandArguments @('definition', $fixture, 'src/CallSite.cs', "$($definitionPos.line)", "$($definitionPos.character)")
    $referencesPos = Get-CallSitePosition -Content $productionSource -Line 8 -Needle 'Add'
    $references = Run-Json -Stage 'references' -CommandArguments @('references', $fixture, 'src/Production.cs', "$($referencesPos.line)", "$($referencesPos.character)", '--include-declaration')
    $search = Run-Json -Stage 'search' -CommandArguments @('search', $fixture, 'production billing arithmetic Calculator Add implementation', '--top-k', '8')
    $evidence.results.workspace_symbols = @($symbols.results).Count
    $evidence.results.definition_position = $definitionPos
    $evidence.results.definition = $definition.results
    $evidence.results.references_position = $referencesPos
    $evidence.results.references = $references.results
    $evidence.results.search = $search.results

    Set-Stage 'assert-results'
    $symbolResults = @($symbols.results)
    $definitionResults = @($definition.results)
    $referenceResults = @($references.results)
    $searchResults = @($search.results)
    Assert-Acceptance ($symbolResults.Count -gt 0) 'workspace symbols returned no results'
    Assert-Acceptance (@($symbolResults | Where-Object { $_.name -eq 'Calculator' }).Count -gt 0) 'workspace symbols did not contain Calculator'
    Assert-Acceptance ($definitionResults.Count -gt 0) 'definition returned no locations'
    Assert-Acceptance (@($definitionResults | Where-Object { $_.relative_file_path -eq 'src/Production.cs' }).Count -gt 0) 'definition did not resolve to src/Production.cs'
    Assert-Acceptance (@($referenceResults | Where-Object { $_.relative_file_path -eq 'src/Production.cs' }).Count -gt 0) 'references did not include src/Production.cs'
    Assert-Acceptance (@($referenceResults | Where-Object { $_.relative_file_path -eq 'src/CallSite.cs' }).Count -gt 0) 'references did not include src/CallSite.cs'
    foreach ($location in @($definitionResults + $referenceResults)) {
        Assert-Acceptance ($location.language -eq 'csharp') "location has unexpected language: $($location.language)"
        Assert-Acceptance ($location.provider -eq 'csharp-ls') "location has unexpected provider: $($location.provider)"
        Assert-Acceptance ([int]$location.start_line -ge 1) "location line is not one-based: $($location.start_line)"
        $sourcePath = Join-Path $fixture ($location.relative_file_path -replace '/', '\')
        Assert-Acceptance (Test-Path -LiteralPath $sourcePath) "location points outside fixture sources: $($location.relative_file_path)"
        $sourceLineCount = (Get-Content -LiteralPath $sourcePath).Count
        Assert-Acceptance ([int]$location.end_line -le $sourceLineCount) "location exceeds source line count: $($location.relative_file_path):$($location.end_line)"
    }
    foreach ($result in @($definitionResults + $referenceResults + $searchResults)) {
        Assert-Acceptance ($result.relative_file_path -notmatch '(^|/)(bin|obj)/') "generated build output appeared in results: $($result.relative_file_path)"
    }
    $productionRank = -1
    $testRank = -1
    for ($rank = 0; $rank -lt $searchResults.Count; $rank++) {
        if ($searchResults[$rank].relative_file_path -eq 'src/Production.cs' -and $productionRank -lt 0) { $productionRank = $rank }
        if ($searchResults[$rank].relative_file_path -eq 'tests/CalculatorTests.cs' -and $testRank -lt 0) { $testRank = $rank }
    }
    Assert-Acceptance ($productionRank -ge 0) 'production Calculator implementation was absent from search results'
    Assert-Acceptance ($testRank -lt 0 -or $productionRank -lt $testRank) 'test/example result outranked the production Calculator implementation'
    $productionHit = @($searchResults | Where-Object { $_.relative_file_path -eq 'src/Production.cs' })[0]
    Assert-Acceptance (@($productionHit.retrieval_channels) -contains 'lsp') 'production result did not include the LSP retrieval channel'
    Assert-Acceptance ($null -ne $productionHit.semantic_score) 'production result did not contain a semantic score'
    Assert-Acceptance ($null -ne $productionHit.reranker_score) 'production result did not contain a reranker score'
    $evidence.status = 'passed'
    $evidence | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $evidencePath -Encoding utf8
    Write-Host "PASS: C# LSP acceptance. Evidence: $evidencePath"
}
catch {
    $evidence.status = 'failed'
    $evidence.failed_stage = $evidence.stage
    $evidence.error = $_.Exception.Message
    Save-Evidence
    throw
}
finally {
    $evidence.stage = 'cleanup'
    $evidence.cleanup.fixture_removed = $false
    $successful = $evidence.status -in @('passed', 'passed-index-only', 'passed-probe-only')
    if (($successful -or -not $KeepFixtureOnFailure) -and (Test-Path -LiteralPath $fixture)) {
        Remove-Item -LiteralPath $fixture -Recurse -Force -ErrorAction SilentlyContinue
        $evidence.cleanup.fixture_removed = -not (Test-Path -LiteralPath $fixture)
    } elseif (Test-Path -LiteralPath $fixture) {
        $evidence.cleanup.fixture_removed = $false
        $evidence.cleanup.fixture_path = $fixture
    }
    if (Test-Path -LiteralPath $dataDir) {
        Remove-Item -LiteralPath $dataDir -Recurse -Force -ErrorAction SilentlyContinue
    }
    $evidence.cleanup.data_removed = -not (Test-Path -LiteralPath $dataDir)
    $evidence.cleanup.owned_process_ids = @()
    Save-Evidence
}
