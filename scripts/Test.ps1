param(
    [ValidateSet('Unit', 'Chunk', 'Filter', 'Evaluation', 'Indexing', 'Readiness', 'MCP', 'Watching', 'LSP', 'Full')]
    [string]$Suite = 'Unit'
)

$ErrorActionPreference = 'Stop'

function Invoke-Checked {
    param([string]$Program, [string[]]$Arguments)
    Write-Host "> $Program $($Arguments -join ' ')"
    & $Program @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "$Program exited with code $LASTEXITCODE"
    }
}

switch ($Suite) {
    'Unit'       { Invoke-Checked cargo @('test', '--lib') }
    'Chunk'      { Invoke-Checked cargo @('test', '--lib', 'chunk::tests::') }
    'Filter'     { Invoke-Checked cargo @('test', '--lib', 'filter::tests::') }
    'Evaluation' { Invoke-Checked cargo @('test', '--lib', 'evaluate::tests::'); Invoke-Checked cargo @('test', '--test', 'integration', 'evaluation::') }
    'Indexing'   { Invoke-Checked cargo @('test', '--test', 'integration', 'indexing::') }
    'Readiness'  { Invoke-Checked cargo @('test', '--test', 'integration', 'readiness::') }
    'MCP'        { Invoke-Checked cargo @('test', '--test', 'integration', 'mcp::') }
    'Watching'   { Invoke-Checked cargo @('test', '--test', 'integration', 'watching::') }
    'LSP'        { Invoke-Checked cargo @('test', '--lib', 'lsp::tests::'); Invoke-Checked cargo @('test', '--test', 'integration', 'navigation::') }
    'Full' {
        Invoke-Checked cargo @('fmt', '--all', '--', '--check')
        Invoke-Checked cargo @('test', '--workspace')
        Invoke-Checked cargo @('clippy', '--workspace', '--all-targets', '--', '-D', 'warnings')
    }
}
