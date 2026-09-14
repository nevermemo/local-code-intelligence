param(
    [string]$Config,
    [string]$CSharpLanguageServer = 'csharp-ls'
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$binary = Join-Path $projectRoot 'target\debug\local-code-intelligence.exe'
$outputDir = Join-Path $projectRoot 'test-results'
$fixture = Join-Path ([System.IO.Path]::GetTempPath()) "lci-csharp-lsp-$PID"
$dataDir = Join-Path $fixture 'data'
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
    results = [ordered]@{}
}

function Write-Utf8([string]$Path, [string]$Content) {
    [System.IO.File]::WriteAllText($Path, $Content, $utf8NoBom)
}

try {
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
    Write-Utf8 (Join-Path $fixture 'src\CallSite.cs') @'
namespace Acceptance;

public static class CallSite
{
    public static int Run(ICalculator calculator) => calculator.Add(1, 2);
}
'@
    & $dotnetCommand.Source build (Join-Path $fixture 'CSharpAcceptance.csproj') --no-restore
    if ($LASTEXITCODE -ne 0) { throw 'dotnet build failed' }

    $normalizedData = $dataDir.Replace('\', '/')
    $configPath = Join-Path $fixture 'config.toml'
    $configArgs = "data_dir = '$normalizedData'`n[csharp]`npath = '$($resolved.Source.Replace('\', '/'))'`n"
    Write-Utf8 $configPath $configArgs
    $prefix = @('--config', $configPath)
    function Run-Json([string[]]$Args) {
        $text = & $binary @prefix @Args
        if ($LASTEXITCODE -ne 0) { throw "LCI command failed: $($Args -join ' ')" }
        return ($text | ConvertFrom-Json)
    }

    $index = Run-Json @('index', $fixture)
    $symbols = Run-Json @('symbols', $fixture, 'Calculator')
    $definition = Run-Json @('definition', $fixture, 'src/CallSite.cs', '5', '63')
    $references = Run-Json @('references', $fixture, 'src/Production.cs', '7', '39', '--include-declaration')
    $search = Run-Json @('search', $fixture, 'Calculator', '--top-k', '8')
    $evidence.results.index_files = $index.files
    $evidence.results.workspace_symbols = @($symbols.results).Count
    $evidence.results.definition = $definition.results
    $evidence.results.references = $references.results
    $evidence.results.search = $search.results
    $evidence.status = 'passed'
    $evidence | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $evidencePath -Encoding utf8
    Write-Host "PASS: C# LSP acceptance. Evidence: $evidencePath"
}
catch {
    $evidence.status = 'failed'
    $evidence.error = $_.Exception.Message
    $evidence | ConvertTo-Json -Depth 12 | Set-Content -LiteralPath $evidencePath -Encoding utf8
    throw
}
finally {
    Get-Process -Name 'csharp-ls' -ErrorAction SilentlyContinue | Stop-Process -Force -ErrorAction SilentlyContinue
    if (Test-Path -LiteralPath $fixture) {
        Remove-Item -LiteralPath $fixture -Recurse -Force -ErrorAction SilentlyContinue
    }
}