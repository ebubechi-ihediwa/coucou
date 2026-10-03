<#
.SYNOPSIS
  Checks that Coucou's island page is quiet when it should be: launches an isolated
  copy of the release build with DevTools on a local port, waits for the island to
  reach the state, and runs check-idle-render.mjs against it. Exit code 0 = quiet.

.DESCRIPTION
  This is the live counterpart of the `island::tests` unit tests, which only read
  the page's sources. It needs a built app (`npm run tauri build -- --no-bundle`),
  a display, and Node 22 or later (for its WebSocket client).

.EXAMPLE
  .\scripts\check-idle-render.ps1 -State hidden
#>
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\coucou.exe'),
    [ValidateSet('hidden', 'compact', 'home')][string]$State = 'hidden',
    [int]$Port = 9333
)

$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path $Exe).Path
$exeName = [IO.Path]::GetFileNameWithoutExtension($Exe)
if (@(Get-Process -Name $exeName -ErrorAction SilentlyContinue).Count -gt 0) {
    throw "A '$exeName' process is already running; close it first (it would take over the launch)."
}

function Get-Tree([int]$rootPid) {
    $all = Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId
    $ids = New-Object System.Collections.Generic.HashSet[int]
    [void]$ids.Add($rootPid)
    $grew = $true
    while ($grew) {
        $grew = $false
        foreach ($p in $all) { if ($ids.Contains([int]$p.ParentProcessId) -and $ids.Add([int]$p.ProcessId)) { $grew = $true } }
    }
    return , $ids
}

# Hidden is reached 60 s after the greeting; the compact and home states are earlier.
$settle = @{ hidden = 90; compact = 20; home = 20 }[$State]

$profile = Join-Path $env:TEMP ("coucou-idle-check-{0}" -f $PID)
New-Item -ItemType Directory -Force -Path "$profile\roaming", "$profile\local" | Out-Null
$env:APPDATA = "$profile\roaming"; $env:LOCALAPPDATA = "$profile\local"
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=$Port --remote-allow-origins=*"

$app = Start-Process -FilePath $Exe -PassThru
try {
    Start-Sleep -Seconds $settle
    if ($State -eq 'home') { Start-Process -FilePath $Exe | Out-Null; Start-Sleep -Seconds 6 }
    node (Join-Path $PSScriptRoot 'check-idle-render.mjs') $Port 6 $State
    $code = $LASTEXITCODE
}
finally {
    foreach ($id in (Get-Tree $app.Id)) { Stop-Process -Id $id -Force -ErrorAction SilentlyContinue }
    Get-CimInstance Win32_Process | Where-Object { $_.CommandLine -and $_.CommandLine.Contains($profile) } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Seconds 2
    Remove-Item -Recurse -Force $profile -ErrorAction SilentlyContinue
}
exit $code
