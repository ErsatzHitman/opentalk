<#
  Measure one renderer probe exe: private bytes, working set, private working set, idle CPU, frame stats, loaded GPU/hook DLLs.

  .\measure.ps1 -Exe target\release\probe_glow.exe [-IdleSec 20] [-Runs 3] [-ExtraEnv PROBE_POWER=low]

  The probe renders continuously for ~4 s, writes one report line to $env:PROBE_OUT, then stops repainting. Memory is
  sampled every 250 ms during the idle window that follows; idle CPU is Process.TotalProcessorTime over that window,
  as a share of all logical processors (Task Manager style) and of one core. Prints one markdown row per run.
#>
param(
    [Parameter(Mandatory)][string]$Exe,
    [int]$IdleSec = 20,
    [int]$Runs = 3,
    [string[]]$ExtraEnv = @()
)
$ErrorActionPreference = 'Stop'
$cores = [Environment]::ProcessorCount
$Exe = (Resolve-Path $Exe).Path

# Private working set (Task Manager's "Memory"). The 'Process V2' PDH counters fail on this machine (800007d2),
# so read the same value from the WMI performance class.
function Get-PrivateWsMB([int]$procId) {
    try {
        $r = Get-CimInstance Win32_PerfFormattedData_PerfProc_Process -Filter "IDProcess=$procId" -ErrorAction Stop | Select-Object -First 1
        if ($r) { return [math]::Round($r.WorkingSetPrivate / 1MB, 1) }
    } catch {}
    return $null
}

for ($run = 1; $run -le $Runs; $run++) {
    $out = Join-Path $env:TEMP ("probe_" + [IO.Path]::GetFileNameWithoutExtension($Exe) + "_$run.txt")
    Remove-Item $out -ErrorAction SilentlyContinue
    $env:PROBE_OUT = $out
    foreach ($kv in $ExtraEnv) { $k, $v = $kv -split '=', 2; Set-Item "env:$k" $v }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p = Start-Process -FilePath $Exe -PassThru -WindowStyle Normal
    while (-not (Test-Path $out) -and $sw.Elapsed.TotalSeconds -lt 60 -and -not $p.HasExited) { Start-Sleep -Milliseconds 100 }
    if (-not (Test-Path $out)) {
        Write-Host "run ${run}: no report (exited=$($p.HasExited))" -ForegroundColor Yellow
        if (-not $p.HasExited) { $p.Kill() }
        continue
    }
    $report = (Get-Content $out -Raw).Trim()
    Start-Sleep -Seconds 2    # let the last frames and any deferred trims settle
    $p.Refresh()
    $cpu0 = $p.TotalProcessorTime.TotalSeconds
    $t = [Diagnostics.Stopwatch]::StartNew()
    $ws = New-Object Collections.Generic.List[double]
    $pb = New-Object Collections.Generic.List[double]
    $pwsMid = $null
    while ($t.Elapsed.TotalSeconds -lt $IdleSec) {
        Start-Sleep -Milliseconds 250
        $p.Refresh()
        $ws.Add($p.WorkingSet64 / 1MB); $pb.Add($p.PrivateMemorySize64 / 1MB)
        if ($null -eq $pwsMid -and $t.Elapsed.TotalSeconds -gt $IdleSec / 2) { $pwsMid = Get-PrivateWsMB $p.Id }
    }
    $p.Refresh()
    $wall = $t.Elapsed.TotalSeconds
    $cpu = $p.TotalProcessorTime.TotalSeconds - $cpu0
    $threads = $p.Threads.Count; $handles = $p.HandleCount
    $mods = @(); try { $mods = $p.Modules | ForEach-Object { $_.ModuleName } } catch {}
    $rtss = [bool]($mods -match '^RTSSHooks')
    $gpuDlls = ($mods | Where-Object { $_ -match '^(opengl32|d3d12|d3d11|dxgi|vulkan-1|nvoglv64|nvwgf2umx|nvd3dumx|igd\w*|ig\w*icd64|RTSS\w*|MSIAfter\w*|OverlayHook\w*|gdi32full|dcomp|d3dcompiler\w*|dxcompiler|dxil)\.dll$' } | Sort-Object -Unique) -join ','
    $hasWin = $p.MainWindowHandle -ne 0
    $mean = { param($l) [math]::Round(($l | Measure-Object -Average).Average, 1) }
    $max = { param($l) [math]::Round(($l | Measure-Object -Maximum).Maximum, 1) }
    $pwsEnd = Get-PrivateWsMB $p.Id
    [pscustomobject]@{
        Exe = [IO.Path]::GetFileName($Exe); Run = $run; Window = $hasWin
        PrivateMB_mean = (& $mean $pb); PrivateMB_peak = (& $max $pb)
        WsMB_mean = (& $mean $ws); WsMB_peak = (& $max $ws); PrivateWsMB = $pwsEnd
        IdleCpuPctAll = [math]::Round($cpu / ($wall * $cores) * 100, 3); IdleCpuPctCore = [math]::Round($cpu / $wall * 100, 3)
        Threads = $threads; Handles = $handles; RTSSHooks = $rtss; GpuModules = $gpuDlls
    } | Format-List | Out-String | Write-Host
    Write-Host "report: $report`n"
    $p.Kill(); $p.WaitForExit()
    Start-Sleep -Seconds 1
}
