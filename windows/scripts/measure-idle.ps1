<#
.SYNOPSIS
  Measures Coucou's idle CPU, memory and wakeups on Windows.

.DESCRIPTION
  Launches a fresh, isolated copy of a release build (its own APPDATA and
  LOCALAPPDATA, so it never reads your settings, writes your log or collides with
  a Coucou that is already running), lets it settle, then samples the whole process
  tree -- coucou.exe and every WebView2 process it spawned -- for a fixed time.

  Scenarios (the island's own state machine, see src/island/fsm.ts):
    startup  from launch: profile creation, greeting animation, first compact state
    compact  Mochi visible and idle in the compact (petit) state, no hover
    hidden   the island has hidden itself and the window is the tiny wake strip
    home     the expanded panel, left alone (opened as the tray's Open does)
    session  a Claude Code session in progress: hook events sent over the real relay
             pipe (a prompt, then a tool call every few seconds), so Mochi is in its
             working state and the island stays revealed

  CPU is cumulative processor time of the tree divided by wall time, from the OS
  performance counters (they can read the sandboxed WebView2 renderer and GPU
  processes, which Get-Process cannot). It is shown as a percentage of one logical
  core and of the whole machine. Memory is the sum of working set and private bytes
  over the tree. Wakeups are context switches per second, summed over the tree's
  threads. Counters update coarsely, so a window of a minute or more is what means
  something; the per-sample buckets are only for seeing the shape.

  A run is only reported as valid when the measurement was not disturbed:
    * the process count is what one instance has (about nine),
    * the island's window had the size the scenario expects (the 240x6 wake strip
      when hidden, the 720x320 panel otherwise), and
    * the mouse was never over the island's zone (it wakes a hidden island and
      keeps an open one open).
  The machine should otherwise be left alone while this runs.

.EXAMPLE
  .\scripts\measure-idle.ps1 -Scenario hidden -Runs 3
#>
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\coucou.exe'),
    [ValidateSet('startup', 'compact', 'hidden', 'home', 'session')][string]$Scenario = 'hidden',
    [int]$Seconds = 60,
    [int]$Runs = 3,
    [int]$SampleEvery = 5,
    [string]$OutDir = (Join-Path $PSScriptRoot '..\target\idle-measurements')
)

$ErrorActionPreference = 'Stop'
$Exe = (Resolve-Path $Exe).Path
$exeName = [IO.Path]::GetFileNameWithoutExtension($Exe)
$cores = [Environment]::ProcessorCount
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Add-Type -AssemblyName System.Windows.Forms
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @"
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Runtime.InteropServices;
public static class CoucouWin {
    public delegate bool EnumProc(IntPtr h, IntPtr l);
    [DllImport("user32.dll")] static extern bool EnumWindows(EnumProc p, IntPtr l);
    [DllImport("user32.dll")] static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] static extern bool IsWindowVisible(IntPtr h);
    [DllImport("user32.dll")] static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] static extern bool PrintWindow(IntPtr h, IntPtr hdc, uint flags);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    // How much is drawn in the island window below its top 44 px (the compact island
    // lives in the top strip; the expanded panel fills the area below). Counts
    // non-black pixels on a coarse grid in an image of that window alone, taken with
    // PrintWindow, so nothing from the desktop behind it is ever captured. -1 when
    // there is no such window.
    public static long Painted(HashSet<int> pids) {
        long painted = -1;
        EnumWindows((h, l) => {
            uint pid; GetWindowThreadProcessId(h, out pid);
            RECT r;
            if (pids.Contains((int)pid) && IsWindowVisible(h) && GetWindowRect(h, out r)) {
                int w = r.Right - r.Left, ht = r.Bottom - r.Top;
                if (w >= 100 && ht >= 100) {
                    using (var bmp = new Bitmap(w, ht)) {
                        using (var g = Graphics.FromImage(bmp)) {
                            IntPtr hdc = g.GetHdc();
                            PrintWindow(h, hdc, 2);
                            g.ReleaseHdc(hdc);
                        }
                        painted = 0;
                        for (int y = 44; y < ht; y += 2)
                            for (int x = 0; x < w; x += 2) {
                                Color c = bmp.GetPixel(x, y);
                                if (c.R > 12 || c.G > 12 || c.B > 12) painted++;
                            }
                    }
                    return false;
                }
            }
            return true;
        }, IntPtr.Zero);
        return painted;
    }
    // Size of every visible, reasonably wide top-level window owned by `pids`.
    public static List<string> Windows(HashSet<int> pids) {
        var found = new List<string>();
        EnumWindows((h, l) => {
            uint pid; GetWindowThreadProcessId(h, out pid);
            RECT r;
            if (pids.Contains((int)pid) && IsWindowVisible(h) && GetWindowRect(h, out r)) {
                int w = r.Right - r.Left, ht = r.Bottom - r.Top;
                if (w >= 100) found.Add(w + "x" + ht);
            }
            return true;
        }, IntPtr.Zero);
        return found;
    }
}
"@

# How long to wait after launch before the measurement window opens. The compact
# state lasts 60 s after the greeting before the island hides itself, so `hidden`
# waits it out; `compact` measures inside it.
$settle = @{ startup = 0; compact = 15; hidden = 90; home = 20; session = 15 }[$Scenario]

function Get-Tree([int]$rootPid) {
    $all = Get-CimInstance Win32_Process | Select-Object ProcessId, ParentProcessId
    $ids = New-Object System.Collections.Generic.HashSet[int]
    [void]$ids.Add($rootPid)
    $grew = $true
    while ($grew) {
        $grew = $false
        foreach ($p in $all) {
            if ($ids.Contains([int]$p.ParentProcessId) -and $ids.Add([int]$p.ProcessId)) { $grew = $true }
        }
    }
    return ,$ids   # the comma stops PowerShell unrolling the set into a scalar or array
}


# One hook event, sent the way coucou-hook sends it: a JSON line on the relay pipe
# `\.\pipe\coucou-<sid>`. Fire-and-forget events need no answer.
function Send-Hook([hashtable]$payload) {
    $sid = [Security.Principal.WindowsIdentity]::GetCurrent().User.Value
    $pipe = New-Object System.IO.Pipes.NamedPipeClientStream('.', "coucou-$sid", [System.IO.Pipes.PipeDirection]::InOut)
    try {
        $pipe.Connect(2000)
        $bytes = [Text.Encoding]::UTF8.GetBytes(($payload | ConvertTo-Json -Compress -Depth 4) + "`n")
        $pipe.Write($bytes, 0, $bytes.Length)
        $pipe.Flush()
    }
    finally { $pipe.Dispose() }
}

function Send-ToolCall {
    Send-Hook @{ hook_event_name = 'PreToolUse'; session_id = 'measure'; cwd = 'C:\work\demo'; tool_name = 'Bash'; tool_input = @{ command = 'echo measuring' } }
}
# The island's zone on the primary screen, in pixels: the 720x320 panel at the top
# centre plus the 14 px entry margin the hit test uses.
$screen = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
$zoneLeft = $screen.X + ($screen.Width / 2) - 374
$zoneRight = $screen.X + ($screen.Width / 2) + 374

function Test-CursorNearIsland {
    $pt = New-Object CoucouWin+POINT
    [void][CoucouWin]::GetCursorPos([ref]$pt)
    return ($pt.X -ge $zoneLeft -and $pt.X -le $zoneRight -and $pt.Y -le 340)
}

function Get-Snapshot([int]$rootPid) {
    # The OS performance counters, through CIM (see the description).
    $set = Get-Tree $rootPid
    $cpu = @{}
    $ws = 0L; $priv = 0L; $threads = 0; $handles = 0
    Get-CimInstance Win32_PerfRawData_PerfProc_Process |
        Where-Object { $set.Contains([int]$_.IDProcess) } |
        ForEach-Object {
            # PercentProcessorTime is cumulative processor time in 100 ns units.
            $cpu[[int]$_.IDProcess] = [double]$_.PercentProcessorTime / 1e7
            $ws += [int64]$_.WorkingSet; $priv += [int64]$_.PrivateBytes
            $threads += [int]$_.ThreadCount; $handles += [int]$_.HandleCount
        }
    # Context switches only exist per thread; the counter is cumulative.
    $cs = 0L
    Get-CimInstance Win32_PerfRawData_PerfProc_Thread |
        Where-Object { $set.Contains([int]$_.IDProcess) } |
        ForEach-Object { $cs += [int64]$_.ContextSwitchesPersec }
    [pscustomobject]@{
        Time = [Diagnostics.Stopwatch]::GetTimestamp()
        Cpu = $cpu; WorkingSet = $ws; Private = $priv
        Threads = $threads; Handles = $handles; Procs = $cpu.Count; Switches = $cs
        Windows = @([CoucouWin]::Windows($set))
        CursorNear = (Test-CursorNearIsland)
    }
}

# Ends everything a run started: the tree under the launched process, any process
# with the run's profile folder on its command line (WebView2 children can outlive
# their parent), and a second launch if there was one.
function Stop-Run([int[]]$rootPids, [string]$profile) {
    foreach ($root in $rootPids) {
        foreach ($id in (Get-Tree $root)) { Stop-Process -Id $id -Force -ErrorAction SilentlyContinue }
    }
    Get-CimInstance Win32_Process |
        Where-Object { $_.CommandLine -and $_.CommandLine.Contains($profile) } |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Start-Sleep -Seconds 2
}

$results = @()
for ($run = 1; $run -le $Runs; $run++) {
    $left = @(Get-Process -Name $exeName -ErrorAction SilentlyContinue)
    if ($left.Count -gt 0) {
        throw "A '$exeName' process is already running (pid $($left[0].Id)). Close it first: it would take over the launch."
    }

    $profile = Join-Path $env:TEMP ("coucou-measure-{0}-{1}" -f $PID, $run)
    New-Item -ItemType Directory -Force -Path "$profile\roaming", "$profile\local" | Out-Null
    # `home` is left alone for the whole window: the auto-close timer is at its maximum.
    if ($Scenario -eq 'home') {
        New-Item -ItemType Directory -Force -Path "$profile\roaming\Coucou" | Out-Null
        '{ "version": 1, "autoCloseInterval": 120 }' | Set-Content "$profile\roaming\Coucou\settings.json"
    }

    $env:APPDATA = "$profile\roaming"; $env:LOCALAPPDATA = "$profile\local"
    $proc = Start-Process -FilePath $Exe -PassThru
    $second = $null
    $launched = [Diagnostics.Stopwatch]::StartNew()
    try {
        $snaps = @(Get-Snapshot $proc.Id)
        if ($Scenario -ne 'startup') {
            while ($launched.Elapsed.TotalSeconds -lt $settle) { Start-Sleep -Milliseconds 500 }
            if ($Scenario -eq 'home') {
                # A second launch is what the single-instance hook turns into "open";
                # the second process hands over and exits by itself.
                $second = Start-Process -FilePath $Exe -PassThru
                Start-Sleep -Seconds 6
            }
            if ($Scenario -eq 'session') {
                Send-Hook @{ hook_event_name = 'SessionStart'; session_id = 'measure'; cwd = 'C:\work\demo' }
                Send-Hook @{ hook_event_name = 'UserPromptSubmit'; session_id = 'measure'; cwd = 'C:\work\demo'; prompt = 'measure idle cost' }
                Send-ToolCall
                Start-Sleep -Seconds 3
            }
            $snaps = @(Get-Snapshot $proc.Id)
        }
        $paintedStart = [CoucouWin]::Painted((Get-Tree $proc.Id))

        $windowStart = [Diagnostics.Stopwatch]::StartNew()
        while ($windowStart.Elapsed.TotalSeconds -lt $Seconds) {
            # A session keeps working: a tool call every sampling interval or so.
            if ($Scenario -eq 'session') { Send-ToolCall }
            Start-Sleep -Seconds $SampleEvery
            $snaps += Get-Snapshot $proc.Id
        }
        $paintedEnd = [CoucouWin]::Painted((Get-Tree $proc.Id))

        # Per-interval CPU: a process present at both ends is counted by its delta, a
        # process that appeared inside the interval by its whole time.
        $cpuTotal = 0.0; $wall = 0.0; $buckets = @()
        for ($i = 1; $i -lt $snaps.Count; $i++) {
            $a = $snaps[$i - 1]; $b = $snaps[$i]
            $dt = ($b.Time - $a.Time) / [Diagnostics.Stopwatch]::Frequency
            $d = 0.0
            # [double]0, not 0: Max(0, 0.36) picks the integer overload and returns 0.
            foreach ($id in $b.Cpu.Keys) {
                if ($a.Cpu.ContainsKey($id)) { $d += [Math]::Max([double]0, $b.Cpu[$id] - $a.Cpu[$id]) } else { $d += $b.Cpu[$id] }
            }
            $cpuTotal += $d; $wall += $dt
            $buckets += [Math]::Round(100 * $d / $dt, 1)
        }
        $s0 = $snaps[0]; $sn = $snaps[-1]

        $sizes = @($snaps | ForEach-Object { $_.Windows } | Sort-Object -Unique)
        $cursorHits = @($snaps | Where-Object { $_.CursorNear }).Count
        $procsOk = ($sn.Procs -ge 5 -and $sn.Procs -le 14)
        # Hidden: the 240x6 logical strip (a few pixels tall). Otherwise: the panel.
        $tall = @($sizes | Where-Object { [int]($_ -split 'x')[1] -ge 100 }).Count -gt 0
        $short = @($sizes | Where-Object { [int]($_ -split 'x')[1] -lt 40 }).Count -gt 0
        # What the island looked like, from its own window: compact paints almost
        # nothing below its top strip, the expanded panel paints thousands of points.
        $stateOk = switch ($Scenario) {
            'hidden'  { $short -and -not $tall }
            'startup' { $true }
            'compact' { $tall -and $paintedStart -ge 0 -and $paintedStart -lt 300 -and $paintedEnd -ge 0 -and $paintedEnd -lt 300 }
            'home'    { $tall -and $paintedStart -gt 3000 -and $paintedEnd -gt 3000 }
            'session' { $tall }
        }
        $valid = $procsOk -and $stateOk -and ($cursorHits -eq 0)

        $r = [pscustomobject]@{
            Scenario = $Scenario; Run = $run; Valid = $valid; Seconds = [Math]::Round($wall, 1)
            CpuPctOfOneCore = [Math]::Round(100 * $cpuTotal / $wall, 2)
            CpuPctOfMachine = [Math]::Round(100 * $cpuTotal / $wall / $cores, 2)
            WorkingSetMB = [Math]::Round($sn.WorkingSet / 1MB, 1)
            PrivateMB = [Math]::Round($sn.Private / 1MB, 1)
            WorkingSetStartMB = [Math]::Round($s0.WorkingSet / 1MB, 1)
            Procs = $sn.Procs; Threads = $sn.Threads; Handles = $sn.Handles
            SwitchesPerSec = [Math]::Round(($sn.Switches - $s0.Switches) / $wall, 0)
            WindowSizes = ($sizes -join ' ')
            CursorNearIslandSamples = $cursorHits
            PaintedStartEnd = "$paintedStart $paintedEnd"
            CpuBuckets = ($buckets -join ' ')
        }
        $results += $r
        $r | Format-List | Out-String | Write-Host
    }
    finally {
        $stop = @($proc.Id); if ($second) { $stop += $second.Id }
        Stop-Run $stop $profile
        Remove-Item -Recurse -Force $profile -ErrorAction SilentlyContinue
    }
}

$csv = Join-Path $OutDir ("{0}-{1}.csv" -f $Scenario, (Get-Date -Format 'yyyyMMdd-HHmmss'))
$results | Export-Csv -NoTypeInformation $csv
$valid = @($results | Where-Object { $_.Valid })
Write-Host ("{0}: {1} of {2} run(s) valid, {3} s each, {4} logical cores" -f $Scenario, $valid.Count, $results.Count, $Seconds, $cores)
if ($valid.Count -gt 0) {
    $avg = ($valid | Measure-Object CpuPctOfOneCore -Average).Average
    $sd = if ($valid.Count -gt 1) {
        [Math]::Sqrt((($valid | ForEach-Object { [Math]::Pow($_.CpuPctOfOneCore - $avg, 2) }) | Measure-Object -Sum).Sum / ($valid.Count - 1))
    } else { 0 }
    $mem = ($valid | Measure-Object WorkingSetMB -Average).Average
    $sw = ($valid | Measure-Object SwitchesPerSec -Average).Average
    Write-Host ("  valid runs: CPU {0:N2}% of one core (sd {1:N2}), working set {2:N0} MB, {3:N0} context switches/s" -f $avg, $sd, $mem, $sw)
}
Write-Host "csv: $csv"
