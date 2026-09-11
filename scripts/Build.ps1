param([switch]$Test)
$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
Push-Location $projectRoot
try {
    $protocPath = Join-Path $projectRoot '.tools\protoc\bin\protoc.exe'
    if (Test-Path -LiteralPath $protocPath) { $env:PROTOC = $protocPath }
    elseif (-not (Get-Command protoc -ErrorAction SilentlyContinue)) {
        throw 'Protobuf compiler missing. Run scripts/Setup-BuildTools.ps1 first.'
    }
    cargo build --locked -j 8
    if ($LASTEXITCODE -ne 0) { throw 'Build failed' }
    if ($Test) {
        cargo test --locked -j 8
        if ($LASTEXITCODE -ne 0) { throw 'Tests failed' }
    }
} finally { Pop-Location }
