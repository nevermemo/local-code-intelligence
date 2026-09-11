$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$toolRoot = Join-Path $projectRoot '.tools'
$protocPath = Join-Path $toolRoot 'protoc\bin\protoc.exe'
if (-not (Test-Path -LiteralPath $protocPath)) {
    New-Item -ItemType Directory -Force -Path $toolRoot | Out-Null
    $archive = Join-Path $toolRoot 'protoc.zip'
    Invoke-WebRequest 'https://github.com/protocolbuffers/protobuf/releases/download/v33.0/protoc-33.0-win64.zip' -OutFile $archive
    $expected = '3742CD49C8B6BD78B6760540367EB0FF62FA70A1032E15DAFE131BFAF296986A'
    if ((Get-FileHash -LiteralPath $archive -Algorithm SHA256).Hash -ne $expected) { throw 'Protobuf archive checksum mismatch' }
    Expand-Archive -LiteralPath $archive -DestinationPath (Join-Path $toolRoot 'protoc') -Force
}
& $protocPath --version
if ($LASTEXITCODE -ne 0) { throw 'Protobuf compiler verification failed' }
