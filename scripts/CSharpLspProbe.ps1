<#
.SYNOPSIS
    Standalone C# LSP protocol probe for the real csharp-ls.exe binary.

.DESCRIPTION
    A fully independent JSON-RPC client test. It creates its own minimal .NET
    fixture (duplicating the dotnet sln/csproj/restore/build steps so it runs
    without the LCI binary), starts csharp-ls with redirected stdin/stdout/
    stderr, speaks the LSP protocol (Content-Length framing, response
    correlation, conservative server-request handling), and writes bounded JSON
    evidence to test-results/csharp-lsp-standalone-probe.json.

    This script does NOT depend on the LCI application code. It is a pure
    diagnostic tool that can be run directly:
        pwsh -File scripts/CSharpLspProbe.ps1

    Flags used against csharp-ls (per csharp-ls --help / the existing C#
    acceptance script): --solution <relative.sln> --rpclog <path>. The solution
    is referenced relative to the process working directory (the fixture root).
#>
param(
    [string]$CSharpLanguageServer = 'C:\Users\micro\.dotnet\tools\csharp-ls.exe',
    [switch]$KeepFixtureOnFailure
)

$ErrorActionPreference = 'Stop'
$projectRoot = Split-Path $PSScriptRoot -Parent
$outputDir = Join-Path $projectRoot 'test-results'
$evidencePath = Join-Path $outputDir 'csharp-lsp-standalone-probe.json'
$fixture = Join-Path ([System.IO.Path]::GetTempPath()) "lci-csharp-probe-$PID"
$utf8NoBom = New-Object System.Text.UTF8Encoding($false)

New-Item -ItemType Directory -Force -Path $outputDir | Out-Null

# ---------------------------------------------------------------------------
# Shared protocol state (initialized before the protocol functions use them)
# ---------------------------------------------------------------------------
$queueLock = New-Object object
$messageQueue = [System.Collections.Generic.List[string]]::new()
$responses = @{}
$notifications = [System.Collections.Generic.List[string]]::new()
$serverRequests = [System.Collections.Generic.List[string]]::new()
$handledRequests = [System.Collections.Generic.List[string]]::new()
$stopEvent = [System.Threading.ManualResetEvent]::new($false)
$stderrLock = New-Object object
$stderrLines = [System.Collections.Generic.List[string]]::new()
$workspaceFolders = @()
$stdinStream = $null
$process = $null
$ownedPid = $null
$readerPS = $null
$readerRunspace = $null
$stderrPS = $null
$stderrRunspace = $null
$rpcLogPath = $null

$evidence = [ordered]@{
    status = 'running'
    stage = 'start'
    failed_stage = $null
    executable = $CSharpLanguageServer
    version = $null
    arguments = @()
    working_directory = $fixture
    solution_name = 'CSharpAcceptance.sln'
    owned_pid = $null
    initialize_elapsed_ms = $null
    initialize_response_received = $false
    server_capability_keys = @()
    notification_methods_observed = @()
    server_request_methods_observed = @()
    server_requests_handled = @()
    workspace_symbol_attempts = 0
    workspace_symbol_elapsed_ms = $null
    matching_symbol_names = @()
    matching_repo_relative_paths = @()
    shutdown_response_received = $false
    exit_code = $null
    forced_termination_required = $false
    stderr_tail = ''
    rpc_log_path = $null
    rpc_log_tail = ''
    cleanup = [ordered]@{}
}

function Write-Utf8([string]$Path, [string]$Content) {
    [System.IO.File]::WriteAllText($Path, $Content, $utf8NoBom)
}

function Save-Evidence {
    $json = $evidence | ConvertTo-Json -Depth 12
    for ($attempt = 1; $attempt -le 5; $attempt++) {
        try {
            Set-Content -LiteralPath $evidencePath -Value $json -Encoding utf8
            return
        } catch {
            if ($attempt -eq 5) { throw }
            Start-Sleep -Milliseconds 100
        }
    }
}

function Set-Stage([string]$Name) {
    $evidence.stage = $Name
    Save-Evidence
}

function Add-Unique {
    param($list, $value)
    if ($null -eq $value) { return }
    if ($list -notcontains $value) { $list.Add($value) }
}

function Get-BoundedTail {
    param([string]$Text, [int]$Max)
    if ([string]::IsNullOrEmpty($Text)) { return '' }
    if ($Text.Length -le $Max) { return $Text }
    return $Text.Substring($Text.Length - $Max)
}

function Sync-MessageEvidence {
    $evidence.notification_methods_observed = @($notifications)
    $evidence.server_request_methods_observed = @($serverRequests)
    $evidence.server_requests_handled = @($handledRequests)
}

function Invoke-DotNet([string[]]$CommandArguments) {
    $output = & $dotnetCommand.Source @CommandArguments 2>&1 | Out-String
    if ($LASTEXITCODE -ne 0) {
        throw "dotnet failed (exit $LASTEXITCODE): $($CommandArguments -join ' ')`n$($output.Trim())"
    }
}

# Outgoing frame: Content-Length: <utf8 byte count>\r\n\r\n<utf8 body> (no trailing CRLF)
function Send-Message {
    param([string]$json)
    $bytes = [System.Text.Encoding]::UTF8.GetBytes($json)
    $header = "Content-Length: $($bytes.Length)`r`n`r`n"
    $headerBytes = [System.Text.Encoding]::ASCII.GetBytes($header)
    $stdinStream.Write($headerBytes, 0, $headerBytes.Length)
    $stdinStream.Write($bytes, 0, $bytes.Length)
    $stdinStream.Flush()
}

# Conservative, bounded responses to the server-to-client requests we know how to answer.
function Respond-ServerRequest {
    param($msg)
    $method = $msg.method
    $id = $msg.id
    $result = $null
    $handled = $false
    switch -Regex ($method) {
        '^client/(register|unregister)Capability$' { $result = $null; $handled = $true }
        '^workspace/configuration$' {
            $result = @()
            if ($msg.params -and $msg.params.PSObject.Properties.Name -contains 'items') {
                foreach ($it in @($msg.params.items)) { $result += $null }
            }
            $handled = $true
        }
        '^workspace/workspaceFolders$' { $result = $workspaceFolders; $handled = $true }
        '^window/workDoneProgress/create$' { $result = [ordered]@{}; $handled = $true }
        default { $handled = $false }
    }
    if ($handled) {
        $resp = [ordered]@{ jsonrpc = '2.0'; id = $id; result = $result }
        Send-Message ($resp | ConvertTo-Json -Depth 12)
    }
    return $handled
}

# Classify one parsed JSON-RPC message and update shared state.
function Classify-Message {
    param([string]$json)
    if ([string]::IsNullOrWhiteSpace($json)) { return }
    $msg = $null
    try { $msg = $json | ConvertFrom-Json } catch { return }
    $hasMethod = ($msg.PSObject.Properties.Name -contains 'method')
    $hasId = ($msg.PSObject.Properties.Name -contains 'id')
    $hasResult = ($msg.PSObject.Properties.Name -contains 'result')
    $hasError = ($msg.PSObject.Properties.Name -contains 'error')
    if ($hasMethod -and $hasId) {
        Add-Unique $serverRequests $msg.method
        $handled = Respond-ServerRequest $msg
        if ($handled) { Add-Unique $handledRequests $msg.method }
    }
    elseif ($hasMethod) {
        Add-Unique $notifications $msg.method
    }
    elseif ($hasId -and ($hasResult -or $hasError)) {
        $responses["$($msg.id)"] = $msg
    }
}

# Drain queued frames (handling notifications / server requests) until the
# response for $id is present or the bounded timeout elapses.
function Wait-ForResponse {
    param([object]$id, [int]$timeoutMs)
    $key = "$id"
    $deadline = [DateTimeOffset]::Now.AddMilliseconds($timeoutMs)
    while ($true) {
        if ($responses.ContainsKey($key)) { return $true }
        $remaining = ($deadline - [DateTimeOffset]::Now).TotalMilliseconds
        if ($remaining -le 0) { return $false }
        $item = $null
        [System.Threading.Monitor]::Enter($queueLock)
        try {
            if ($messageQueue.Count -gt 0) { $item = $messageQueue[0]; $messageQueue.RemoveAt(0) }
        } finally { [System.Threading.Monitor]::Exit($queueLock) }
        if ($null -ne $item) { Classify-Message $item }
        Start-Sleep -Milliseconds 20
    }
}

# Convert a file URI to a path relative to $root (forward slashes), or $null.
function Get-RelativePath {
    param([string]$uri, [string]$root)
    try { $local = [System.Uri]::new($uri).LocalPath } catch { return $null }
    $rootNorm = $root.TrimEnd('\','/')
    if ($local.StartsWith($rootNorm, [System.StringComparison]::OrdinalIgnoreCase)) {
        $rel = $local.Substring($rootNorm.Length).TrimStart('\','/')
        return ($rel -replace '\\','/')
    }
    return $null
}

try {
    # --- resolve prerequisites ---
    Set-Stage 'resolve-prerequisites'
    if (-not (Test-Path -LiteralPath $CSharpLanguageServer)) {
        $evidence.status = 'prerequisite-unavailable'
        $evidence.failed_stage = 'resolve-prerequisites'
        Save-Evidence
        Write-Host "PREREQUISITE_UNAVAILABLE: csharp-ls not found at $CSharpLanguageServer. Evidence: $evidencePath"
        exit 2
    }
    $evidence.version = (& $CSharpLanguageServer --version 2>&1 | Out-String).Trim()
    $dotnetCommand = Get-Command dotnet -ErrorAction SilentlyContinue
    if (-not $dotnetCommand) { throw 'dotnet SDK is required for the real C# fixture' }

    # --- create fixture (standalone; duplicates the acceptance fixture steps) ---
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
    Write-Utf8 (Join-Path $fixture 'src\CallSite.cs') @'
namespace Acceptance;

public static class CallSite
{
    public static int Run(ICalculator calculator) => calculator.Add(1, 2);
}
'@
    $projectPath = Join-Path $fixture 'CSharpAcceptance.csproj'
    $solutionPath = Join-Path $fixture 'CSharpAcceptance.sln'
    Invoke-DotNet @('new', 'sln', '--format', 'sln', '--name', 'CSharpAcceptance', '--output', $fixture)
    Invoke-DotNet @('solution', $solutionPath, 'add', $projectPath)
    Invoke-DotNet @('restore', $solutionPath)
    Invoke-DotNet @('build', $solutionPath, '--no-restore')
    if (-not (Test-Path -LiteralPath $solutionPath)) { throw "solution file not created: $solutionPath" }

    # --- start csharp-ls ---
    Set-Stage 'start-process'
    $rpcLogPath = Join-Path $fixture 'csharp-ls-rpc.log'
    $evidence.rpc_log_path = $rpcLogPath
    $evidence.arguments = @('--solution', 'CSharpAcceptance.sln', '--rpclog', $rpcLogPath)
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $CSharpLanguageServer
    $psi.Arguments = "--solution CSharpAcceptance.sln --rpclog `"$rpcLogPath`""
    $psi.WorkingDirectory = $fixture
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $process = [System.Diagnostics.Process]::new()
    $process.StartInfo = $psi
    $process.Start() | Out-Null
    $ownedPid = $process.Id
    $evidence.owned_pid = $ownedPid

    $stdinStream = $process.StandardInput.BaseStream
    $stdoutStream = $process.StandardOutput.BaseStream

    # stderr capture (bounded; never treated as protocol data). PowerShell cannot
    # attach a scriptblock to the .NET ErrorDataReceived event via `+=`, so read
    # stderr synchronously on a dedicated background thread instead.
    $stderrReader = $process.StandardError
    $stderrArgs = @{ Reader = $stderrReader; Lines = $stderrLines; Lock = $stderrLock; Stop = $stopEvent }
    $stderrSb = {
        param($state)
        $reader = $state.Reader
        $lines = $state.Lines
        $slock = $state.Lock
        try {
            while ($true) {
                $line = $reader.ReadLine()
                if ($null -eq $line) { break }
                [System.Threading.Monitor]::Enter($slock)
                try {
                    $lines.Add($line)
                    if ($lines.Count -gt 500) { $lines.RemoveAt(0) }
                } finally { [System.Threading.Monitor]::Exit($slock) }
            }
        } catch {
            # swallow; bounded stderr tail is best-effort diagnostic evidence
        }
    }
    # PS scriptblocks require a Runspace; a raw System.Threading.Thread cannot run one.
    $stderrRunspace = [runspacefactory]::CreateRunspace()
    $stderrRunspace.Open()
    $stderrPS = [System.Management.Automation.PowerShell]::Create()
    $stderrPS.Runspace = $stderrRunspace
    [void]$stderrPS.AddScript($stderrSb).AddArgument($stderrArgs)
    $stderrHandle = $stderrPS.BeginInvoke()

    # Background reader thread: continuously parse Content-Length frames from stdout.
    # Reads headers byte-by-byte until the blank CRLF line, parses Content-Length
    # case-insensitively (ignoring any other header such as Content-Type), then reads
    # exactly that many bytes for the body. Malformed/truncated frames break the loop
    # (surfaced later via timeouts + rpc log), never an unhandled crash.
    $readerArgs = @{ Stream = $stdoutStream; Queue = $messageQueue; QueueLock = $queueLock; Stop = $stopEvent }
    $readerSb = {
        param($state)
        $stream = $state.Stream
        $queue = $state.Queue
        $qlock = $state.QueueLock
        $stop = $state.Stop
        try {
            $utf8 = [System.Text.Encoding]::UTF8
            while (-not $stop.WaitOne(0)) {
                $header = New-Object System.Collections.Generic.List[byte]
                $eof = $false
                while ($true) {
                    $b = $stream.ReadByte()
                    if ($b -eq -1) { $eof = $true; break }
                    $header.Add([byte]$b)
                    $n = $header.Count
                    if ($n -ge 4 -and $header[$n-4] -eq 13 -and $header[$n-3] -eq 10 -and $header[$n-2] -eq 13 -and $header[$n-1] -eq 10) { break }
                }
                if ($eof) { break }
                $headerText = [System.Text.Encoding]::ASCII.GetString($header.ToArray())
                $contentLength = -1
                foreach ($line in ($headerText -split "`r`n")) {
                    $t = $line.Trim()
                    if ($t -eq '') { continue }
                    $ci = $t.IndexOf(':')
                    if ($ci -lt 0) { continue }
                    $name = $t.Substring(0, $ci).Trim()
                    $val = $t.Substring($ci + 1).Trim()
                    if ($name -ieq 'Content-Length') { $contentLength = [int]$val }
                }
                if ($contentLength -lt 0) { break }
                $body = New-Object byte[] $contentLength
                $read = 0
                while ($read -lt $contentLength) {
                    $r = $stream.Read($body, $read, $contentLength - $read)
                    if ($r -le 0) { break }
                    $read += $r
                }
                if ($read -lt $contentLength) { break }
                $json = $utf8.GetString($body)
                [System.Threading.Monitor]::Enter($qlock)
                try { $queue.Add($json) } finally { [System.Threading.Monitor]::Exit($qlock) }
            }
        } catch {
            # swallow; the main thread detects EOF via process exit / bounded timeouts
        }
    }
    $readerRunspace = [runspacefactory]::CreateRunspace()
    $readerRunspace.Open()
    $readerPS = [System.Management.Automation.PowerShell]::Create()
    $readerPS.Runspace = $readerRunspace
    [void]$readerPS.AddScript($readerSb).AddArgument($readerArgs)
    $readerHandle = $readerPS.BeginInvoke()

    # --- initialize (id=1) ---
    Set-Stage 'initialize'
    $rootUri = [System.Uri]::new('file:///' + ($fixture -replace '\\', '/')).AbsoluteUri
    $workspaceFolders = @([ordered]@{ uri = $rootUri; name = 'CSharpAcceptance' })
    $initializeParams = [ordered]@{
        processId = $PID
        rootUri = $rootUri
        workspaceFolders = $workspaceFolders
        capabilities = [ordered]@{
            workspace = [ordered]@{
                symbol = [ordered]@{ dynamicRegistration = $false }
                workspaceFolders = $true
            }
            textDocument = [ordered]@{
                synchronization = [ordered]@{ didSave = $true }
                definition = [ordered]@{ dynamicRegistration = $false }
                references = [ordered]@{ dynamicRegistration = $false }
            }
        }
    }
    $initReq = [ordered]@{ jsonrpc = '2.0'; id = 1; method = 'initialize'; params = $initializeParams }
    $initSw = [System.Diagnostics.Stopwatch]::StartNew()
    Send-Message ($initReq | ConvertTo-Json -Depth 12)
    $initGot = Wait-ForResponse 1 30000
    $initSw.Stop()
    $evidence.initialize_elapsed_ms = $initSw.ElapsedMilliseconds
    if (-not $initGot) { throw 'initialize response (id=1) not received within 30s' }
    $initResp = $responses['1']
    $evidence.initialize_response_received = $true
    if ($initResp.result -and $initResp.result.PSObject.Properties.Name -contains 'capabilities') {
        $caps = $initResp.result.capabilities
        $evidence.server_capability_keys = @($caps.PSObject.Properties.Name)
    } else {
        throw 'initialize result is missing the capabilities object'
    }

    # --- initialized (notification, no response expected) ---
    Set-Stage 'initialized'
    $initNotif = [ordered]@{ jsonrpc = '2.0'; method = 'initialized'; params = [ordered]@{} }
    Send-Message ($initNotif | ConvertTo-Json -Depth 12)

    # --- workspace/symbol polling (bounded retry until a Calculator symbol appears) ---
    Set-Stage 'workspace-symbol'
    $nextId = 100
    $attempt = 0
    $maxAttempts = 30
    $lastSymbolResponse = $null
    $symbolSw = [System.Diagnostics.Stopwatch]::StartNew()
    while ($attempt -lt $maxAttempts) {
        $attempt++
        $evidence.workspace_symbol_attempts = $attempt
        $id = $nextId++
        $req = [ordered]@{ jsonrpc = '2.0'; id = $id; method = 'workspace/symbol'; params = [ordered]@{ query = 'Calculator' } }
        Send-Message ($req | ConvertTo-Json -Depth 12)
        $got = Wait-ForResponse $id 5000
        if ($got) {
            $resp = $responses["$id"]
            $lastSymbolResponse = $resp
            $results = @()
            if ($resp.result) { $results = @($resp.result) }
            $match = $results | Where-Object { $_.name -eq 'Calculator' }
            if ($match) { break }
        }
        Start-Sleep -Milliseconds 1000
    }
    $symbolSw.Stop()
    $evidence.workspace_symbol_elapsed_ms = $symbolSw.ElapsedMilliseconds

    # --- validate: at least one Calculator symbol whose uri is inside the fixture ---
    Set-Stage 'validate-symbols'
    $matchingNames = [System.Collections.Generic.List[string]]::new()
    $matchingPaths = [System.Collections.Generic.List[string]]::new()
    if ($lastSymbolResponse -and $lastSymbolResponse.result) {
        foreach ($sym in @($lastSymbolResponse.result)) {
            if ($sym.name -ne 'Calculator') { continue }
            $uri = $null
            $loc = $sym.location
            if ($loc) {
                if ($loc -is [string]) { $uri = $loc }
                elseif ($loc.PSObject.Properties.Name -contains 'uri') { $uri = $loc.uri }
            }
            if ($uri) {
                $rel = Get-RelativePath $uri $fixture
                if ($rel) {
                    $matchingNames.Add($sym.name)
                    $matchingPaths.Add($rel)
                }
            }
        }
    }
    $evidence.matching_symbol_names = @($matchingNames)
    $evidence.matching_repo_relative_paths = @($matchingPaths)
    if ($matchingNames.Count -eq 0) {
        throw "no Calculator symbol inside the fixture after $attempt workspace/symbol attempts"
    }

    # --- shutdown (request) ---
    Set-Stage 'shutdown'
    $shutdownId = $nextId++
    $shutdownReq = [ordered]@{ jsonrpc = '2.0'; id = $shutdownId; method = 'shutdown' }
    Send-Message ($shutdownReq | ConvertTo-Json -Depth 12)
    $shutdownGot = Wait-ForResponse $shutdownId 10000
    $evidence.shutdown_response_received = $shutdownGot
    if (-not $shutdownGot) { throw 'shutdown response was not received within 10s' }

    # --- exit (notification) ---
    Set-Stage 'exit'
    $exitReq = [ordered]@{ jsonrpc = '2.0'; method = 'exit' }
    Send-Message ($exitReq | ConvertTo-Json -Depth 12)

    # --- wait for the owned process to exit naturally (bounded) ---
    Set-Stage 'wait-exit'
    $exited = $process.WaitForExit(5000)
    $evidence.forced_termination_required = $false
    if (-not $exited) {
        try { $process.Kill(); $evidence.forced_termination_required = $true } catch {}
        $process.WaitForExit(3000) | Out-Null
        throw 'csharp-ls did not exit naturally within 5s after the exit notification'
    }
    $process.Refresh()
    try { $evidence.exit_code = $process.ExitCode } catch { $evidence.exit_code = $null }

    # --- confirm the owned PID is gone ---
    Set-Stage 'confirm-exit'
    $pidGone = $true
    try { $null = Get-Process -Id $ownedPid -ErrorAction Stop; $pidGone = $false } catch { $pidGone = $true }
    if (-not $pidGone) { throw "owned csharp-ls PID $ownedPid still running after exit" }

    $evidence.status = 'passed'
    Sync-MessageEvidence
    Save-Evidence
    Write-Host "PASS: standalone C# LSP probe. Evidence: $evidencePath"
}
catch {
    $evidence.status = 'failed'
    $evidence.failed_stage = $evidence.stage
    Sync-MessageEvidence
    Save-Evidence
    Write-Host "FAIL at stage '$($evidence.stage)': $($_.Exception.Message). Evidence: $evidencePath"
    exit 1
}
finally {
    try { $stopEvent.Set() } catch {}
    # Safety net: any failure path before the normal shutdown/exit/wait-exit sequence
    # must not leave the owned csharp-ls process running.
    try {
        if ($process -and -not $process.HasExited) {
            $process.Kill()
            $evidence.forced_termination_required = $true
            $process.WaitForExit(3000) | Out-Null
        }
    } catch {}
    try { if ($readerPS) { $readerPS.Stop(); if ($readerHandle) { $readerPS.EndInvoke($readerHandle) | Out-Null }; $readerPS.Dispose() } } catch {}
    try { if ($readerRunspace) { $readerRunspace.Close(); $readerRunspace.Dispose() } } catch {}
    try { if ($stderrPS) { $stderrPS.Stop(); if ($stderrHandle) { $stderrPS.EndInvoke($stderrHandle) | Out-Null }; $stderrPS.Dispose() } } catch {}
    try { if ($stderrRunspace) { $stderrRunspace.Close(); $stderrRunspace.Dispose() } } catch {}
    [System.Threading.Monitor]::Enter($stderrLock)
    try { $stderrAll = ($stderrLines -join "`n") } finally { [System.Threading.Monitor]::Exit($stderrLock) }
    $evidence.stderr_tail = Get-BoundedTail $stderrAll 2000
    if ($evidence.rpc_log_path -and (Test-Path -LiteralPath $evidence.rpc_log_path)) {
        $rpcAll = Get-Content -Raw -LiteralPath $evidence.rpc_log_path -ErrorAction SilentlyContinue
        $evidence.rpc_log_tail = Get-BoundedTail $rpcAll 4000
    }
    $evidence.cleanup = [ordered]@{
        fixture_removed = $false
        fixture_path = $fixture
        owned_pid = $ownedPid
        pid_confirmed_gone = $false
    }
    # Successful runs always clean up. KeepFixtureOnFailure applies only to failures.
    if ($evidence.status -eq 'passed' -or -not $KeepFixtureOnFailure) {
        Remove-Item -LiteralPath $fixture -Recurse -Force -ErrorAction SilentlyContinue
        $evidence.cleanup.fixture_removed = -not (Test-Path -LiteralPath $fixture)
    }
    if ($ownedPid) {
        try { $null = Get-Process -Id $ownedPid -ErrorAction Stop; $evidence.cleanup.pid_confirmed_gone = $false } catch { $evidence.cleanup.pid_confirmed_gone = $true }
    } else {
        $evidence.cleanup.pid_confirmed_gone = $true
    }
    Save-Evidence
}
