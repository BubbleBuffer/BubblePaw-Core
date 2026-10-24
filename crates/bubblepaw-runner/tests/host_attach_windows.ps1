# Run after: cargo build -p bubblepaw-runner --bins
# Requires PowerShell 7. All processes and temporary files belong to this smoke run.
param([string]$BinaryDirectory)
$ErrorActionPreference = 'Stop'
if (-not $BinaryDirectory) {
    $manifest = Join-Path $PSScriptRoot '../Cargo.toml'
    $metadataText = & cargo metadata --manifest-path $manifest --format-version 1 --no-deps --locked --offline
    if ($LASTEXITCODE -ne 0) { throw 'Could not resolve Cargo target directory' }
    $cargoMetadata = $metadataText | ConvertFrom-Json
    $BinaryDirectory = Join-Path $cargoMetadata.target_directory 'debug'
}
$binaryRoot = (Resolve-Path -LiteralPath $BinaryDirectory).Path
$binary = Join-Path $binaryRoot 'bubblepaw.exe'
$fake = Join-Path $binaryRoot 'bubblepaw-fake-extension.exe'
$smokeRoot = Join-Path ([IO.Path]::GetTempPath()) ('bubblepaw-attach-' + [guid]::NewGuid())
$processes = [Collections.Generic.List[object]]::new()
New-Item -ItemType Directory -Path $smokeRoot | Out-Null
$workspaceA = New-Item -ItemType Directory -Path (Join-Path $smokeRoot 'a')
$workspaceB = New-Item -ItemType Directory -Path (Join-Path $smokeRoot 'b')
$data = Join-Path $smokeRoot 'data'
$configFile = Join-Path $smokeRoot 'empty.toml'
Set-Content -LiteralPath $configFile -Value ''
$extensionFile = Join-Path $smokeRoot 'extensions.json'
@(@{id='fake'; command=$fake; args=@(); cwd=$smokeRoot}) | ConvertTo-Json -AsArray -Depth 4 | Set-Content -LiteralPath $extensionFile

function Start-Bubble([string]$workspace, [string[]]$CommandArgs) {
    $start = [Diagnostics.ProcessStartInfo]::new($binary)
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardOutput = $true
    $start.RedirectStandardError = $true
    foreach ($key in @($start.Environment.Keys)) {
        if ($key.StartsWith('BUBBLEPAW_') -or $key.StartsWith('BP_FAKE_')) {
            $start.Environment.Remove($key) | Out-Null
        }
    }
    $start.Environment['HOME'] = $smokeRoot
    $start.Environment['USERPROFILE'] = $smokeRoot
    $start.Environment['CODEX_HOME'] = (Join-Path $smokeRoot 'codex')
    foreach ($argument in (@('--config',$configFile,'--provider','demo','--model','fake/code-model','--approve-all','--json','--workspace',$workspace,'--data-dir',$data,'--extensions',$extensionFile) + $CommandArgs)) {
        $start.ArgumentList.Add($argument)
    }
    $process = [Diagnostics.Process]::Start($start)
    $record = @{ Process=$process; Out=$process.StandardOutput.ReadToEndAsync(); Err=$process.StandardError.ReadToEndAsync() }
    $processes.Add($record)
    return $record
}
function Finish-Bubble($record) {
    if (-not $record.Process.WaitForExit(45000)) { throw 'Timed out waiting for smoke process' }
    $output = $record.Out.GetAwaiter().GetResult()
    $errorText = $record.Err.GetAwaiter().GetResult()
    if ($record.Process.ExitCode -ne 0) { throw "bubblepaw failed: $errorText" }
    return $output
}
try {
    $owner = Start-Bubble $workspaceA.FullName @('run','sleep 6000')
    $discovery = Join-Path $data 'host.json'
    $deadline = [DateTime]::UtcNow.AddSeconds(15)
    while (-not (Test-Path -LiteralPath $discovery)) {
        if ($owner.Process.HasExited -or [DateTime]::UtcNow -gt $deadline) { throw 'Host did not publish host.json' }
        Start-Sleep -Milliseconds 25
    }
    $metadata = Get-Content -Raw -LiteralPath $discovery | ConvertFrom-Json
    if ($metadata.pid -ne $owner.Process.Id -or -not $metadata.endpoint.StartsWith('\\.\pipe\')) { throw 'Unexpected host descriptor' }
    $client = Start-Bubble $workspaceB.FullName @('run','sleep 10000')
    Start-Sleep -Seconds 7
    if ($owner.Process.HasExited -or $client.Process.HasExited) { throw 'Host did not wait for attached work' }
    if ((Get-Content -Raw -LiteralPath $discovery | ConvertFrom-Json).pid -ne $metadata.pid) { throw 'Host changed during attachment' }
    $clientOutput = Finish-Bubble $client
    $ownerOutput = Finish-Bubble $owner
    if ($clientOutput -notmatch '"type":"result"' -or $ownerOutput -notmatch '"type":"result"') { throw 'Missing result records' }
    if (Test-Path -LiteralPath $discovery) { throw 'host.json remains after exit' }
    Write-Host 'PASS: named-pipe attach, two workspaces, shared host lifetime, metadata cleanup'
} finally {
    foreach ($record in $processes) {
        if (-not $record.Process.HasExited) { $record.Process.Kill($true); $record.Process.WaitForExit() }
        $record.Process.Dispose()
    }
    # Only this freshly generated, resolved directory may be removed.
    $resolved = [IO.Path]::GetFullPath($smokeRoot)
    $tempRoot = [IO.Path]::GetFullPath([IO.Path]::GetTempPath())
    if ($resolved.StartsWith($tempRoot, [StringComparison]::OrdinalIgnoreCase) -and [IO.Path]::GetFileName($resolved).StartsWith('bubblepaw-attach-')) {
        Remove-Item -LiteralPath $resolved -Recurse -Force
    }
}
