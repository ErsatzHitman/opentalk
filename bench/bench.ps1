# OpenTalk benchmark (docs/PLAN.md section 11). Writes docs\BENCHMARK.md and bench\out\results.json.
#
#   powershell -File bench\bench.ps1 [-Exe path] [-Quick] [-SkipLive] [-SkipMic]
#
# Scenarios (each runs a fresh daemon in an isolated OPENTALK_HOME):
#   startup      process start -> daemon window, 5 runs
#   idle         daemon doing nothing for 60 s
#   dictations   10 dictations of an 8.5 s injected recording through a local mock STT (no network:
#                this isolates OpenTalk's own CPU, memory and latency)
#   recording    30 s continuous recording: injected WAV, and the real microphone (cancelled, no upload)
#   settings     Settings window open and idle (child process) + daemon after it closes
#   e2e          one dictation pasted for real (SendInput) into a scratch text box, batch and streaming
#                (tests\e2e.ps1 -Target form; skipped on a locked desktop)
#   live         3 dictations through OpenRouter, ":free" model only (best effort: free tiers rate-limit)
# CPU% = TotalProcessorTime delta / (wall time x logical cores); the one-core figure is also given.
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\opentalk.exe'),
    [string]$KeyFile = '',
    [int]$IdleSec = 60,
    [int]$Dictations = 10,
    [int]$RecordSec = 30,
    [int]$SettingsSec = 20,
    [int]$LiveDictations = 3,
    [string]$LiveModel = 'nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free',
    [switch]$Quick,
    [switch]$SkipLive,
    [switch]$SkipMic,
    [switch]$SkipE2E
)
. (Join-Path $PSScriptRoot 'common.ps1')
if (-not $KeyFile) { $KeyFile = $script:KeyFileDefault }
if ($Quick) { $IdleSec = 10; $Dictations = 3; $RecordSec = 8; $SettingsSec = 8; $LiveDictations = 1 }
$Exe = (Resolve-Path $Exe).Path
$wav = (Resolve-Path (Join-Path $PSScriptRoot '..\tests\fixtures\tts_fillers.wav')).Path
$wavMs = Get-WavMs $wav
$outDir = Join-Path $PSScriptRoot 'out'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$R = @{}

function Section([string]$t) { Write-Host ("`n== {0}" -f $t) -ForegroundColor Cyan }

# Sample $proc until $until (a process) exits, or for $seconds.
function Measure-Until($proc, $until) {
    $cores = [Environment]::ProcessorCount
    $p = Get-Process -Id $proc.Id; $p.Refresh()
    $cpu0 = $p.TotalProcessorTime.TotalSeconds
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $ws = New-Object Collections.Generic.List[double]; $pb = New-Object Collections.Generic.List[double]
    while (-not $until.HasExited) {
        Start-Sleep -Milliseconds 250
        $p.Refresh(); $ws.Add($p.WorkingSet64 / 1MB); $pb.Add($p.PrivateMemorySize64 / 1MB)
    }
    $p.Refresh(); $wall = $sw.Elapsed.TotalSeconds; $cpu = $p.TotalProcessorTime.TotalSeconds - $cpu0
    return [pscustomobject]@{
        Seconds = [math]::Round($wall, 1); CpuSeconds = [math]::Round($cpu, 3)
        CpuPctAll = [math]::Round($cpu / ($wall * $cores) * 100, 3); CpuPctCore = [math]::Round($cpu / $wall * 100, 3)
        WsMeanMB = [math]::Round(($ws | Measure-Object -Average).Average, 1); WsPeakMB = [math]::Round(($ws | Measure-Object -Maximum).Maximum, 1)
        PrivMeanMB = [math]::Round(($pb | Measure-Object -Average).Average, 1); PrivPeakMB = [math]::Round(($pb | Measure-Object -Maximum).Maximum, 1)
        Handles = $p.HandleCount; Threads = $p.Threads.Count; Cores = $cores
    }
}

# Private working set (what Task Manager shows as "Memory"), via the Process V2 counters.
function Get-PrivateWsMB([int]$procId) {
    try {
        $c = Get-Counter -Counter '\Process V2(*)\Working Set - Private' -ErrorAction Stop
        $s = $c.CounterSamples | Where-Object { $_.InstanceName -like "*:$procId" } | Select-Object -First 1
        if ($s) { return [math]::Round($s.CookedValue / 1MB, 1) }
    } catch {}
    return $null
}

function New-Env([string]$home_, [hashtable]$extra = @{}) {
    $e = @{ OPENTALK_HOME = $home_ }
    foreach ($k in $extra.Keys) { $e[$k] = $extra[$k] }
    return $e
}

# Is Windows counting this exe as using the microphone right now? (The taskbar mic indicator and
# Settings > Privacy > Microphone read the same ConsentStore entry: LastUsedTimeStop = 0 while in use.)
function Get-MicInUse([string]$exePath) {
    $key = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\CapabilityAccessManager\ConsentStore\microphone\NonPackaged\' + ($exePath -replace '\\', '#')
    if (-not (Test-Path -LiteralPath $key)) { return [pscustomobject]@{ known = $false; in_use = $false } }
    $v = Get-ItemProperty -LiteralPath $key -ErrorAction SilentlyContinue
    $start = [int64]($v.LastUsedTimeStart); $stop = [int64]($v.LastUsedTimeStop)
    return [pscustomobject]@{ known = $true; in_use = ($start -gt 0 -and $stop -eq 0) }
}

# "k=v k=v" (the daemon's latency line) -> hashtable of numbers/strings.
function ConvertFrom-LatencyLine([string]$line) {
    $h = @{}
    foreach ($kv in ($line -replace '^.*latency ', '') -split '\s+') {
        $k, $v = $kv -split '=', 2
        if (-not $k) { continue }
        $d = 0.0
        $h[$k] = if ([double]::TryParse($v, [Globalization.NumberStyles]::Float, [Globalization.CultureInfo]::InvariantCulture, [ref]$d)) { $d } else { $v }
    }
    return $h
}

function Read-Log([string]$home_) {
    $f = Join-Path $home_ 'logs\daemon.log'
    if (Test-Path $f) { return (Get-Content $f -ErrorAction SilentlyContinue) }
    return @()
}

# ---- environment ------------------------------------------------------------------------
$cpuName = (Get-CimInstance Win32_Processor | Select-Object -First 1).Name.Trim()
$os = (Get-CimInstance Win32_OperatingSystem)
$exeInfo = Get-Item $Exe
$R['env'] = [ordered]@{
    cpu = $cpuName; logical_cores = [Environment]::ProcessorCount; os = "$($os.Caption) $($os.Version)"
    exe_bytes = $exeInfo.Length; exe_mb = [math]::Round($exeInfo.Length / 1MB, 2); exe_built = $exeInfo.LastWriteTime.ToString('s')
    wav_ms = $wavMs; date = (Get-Date).ToString('s')
    desktop_locked = [bool](Get-Process -Name LogonUI -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq (Get-Process -Id $PID).SessionId })
}
Write-Host ("exe {0:N2} MB, {1} cores, {2}" -f $R.env.exe_mb, $R.env.logical_cores, $cpuName)

# ---- startup ----------------------------------------------------------------------------
Section 'startup (5 runs)'
$starts = @(); $readyLog = @()
Assert-NoDaemon $Exe
for ($i = 0; $i -lt 5; $i++) {
    $h = New-TempHome 'ot-bench-start'
    $e = New-Env $h @{ OPENTALK_SECRET_OPENAI = 'bench-dummy-key-123456' }
    Write-Config $h @{ audio_cues = $true }
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $p = Start-Ot $Exe @('--hidden') $e
    $null = Invoke-Ot $Exe @('--bench', 'wait-ready', '15') $e 30
    $starts += $sw.Elapsed.TotalMilliseconds
    Start-Sleep -Milliseconds 800
    $line = Read-Log $h | Where-Object { $_ -match 'ready in (\d+) ms' } | Select-Object -First 1
    if ($line -match 'ready in (\d+) ms') { $readyLog += [double]$Matches[1] }
    if ($i -eq 0) { $R['startup_ws_mb'] = [math]::Round((Get-Process -Id $p.Id).WorkingSet64 / 1MB, 1) }
    Stop-Daemon $Exe $p $e
    Remove-Item -Recurse -Force $h -ErrorAction SilentlyContinue
}
$R['startup'] = [ordered]@{
    process_to_window_ms_median = [math]::Round((($starts | Sort-Object)[2]), 0)
    daemon_ready_log_ms_median  = if ($readyLog.Count) { [math]::Round((($readyLog | Sort-Object)[[int][math]::Floor(($readyLog.Count - 1) / 2)]), 0) } else { $null }
}
Write-Host ("startup: process->window median {0} ms; daemon-reported ready {1} ms" -f $R.startup.process_to_window_ms_median, $R.startup.daemon_ready_log_ms_median)

# ---- idle -------------------------------------------------------------------------------
$mock = Start-Mock $Exe 0
$baseEnv = @{ OPENTALK_INJECT_WAV = $wav; OPENTALK_STT_BASE_URL = $mock.Url; OPENTALK_SECRET_OPENAI = 'bench-dummy-key-123456' }
$h = New-TempHome 'ot-bench'
$e = New-Env $h $baseEnv
Write-Config $h @{ audio_cues = $true; auto_paste = $false; keep_in_clipboard = $true; show_app_bubble = $true; stt = @{ provider = 'openai' } }
$daemon = Start-Daemon $Exe $e
try {
    Section "idle ($IdleSec s)"
    Start-Sleep -Seconds 3
    $R['idle'] = Measure-Process $daemon $IdleSec
    $R.idle | Add-Member private_ws_mb (Get-PrivateWsMB $daemon.Id)
    $R['mic_idle'] = Get-MicInUse $Exe
    Write-Host ("idle: {0}% of all cores ({1}% of one core), WS {2} MB, private {3} MB, {4} threads, {5} handles" -f $R.idle.CpuPctAll, $R.idle.CpuPctCore, $R.idle.WsMeanMB, $R.idle.PrivMeanMB, $R.idle.Threads, $R.idle.Handles)

    # ---- dictations (mock STT) --------------------------------------------------------
    Section "$Dictations dictations (mock STT, injected $wavMs ms recording)"
    $d = Start-Ot $Exe @('--bench', 'dictate', $Dictations, ('"{0}"' -f $wav)) $e -redirectOut
    $R['dictations'] = Measure-Until $daemon $d
    $out = $d.StandardOutput.ReadToEnd()
    $rounds = @($out -split "`n" | Where-Object { $_.Trim().StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
    $R.dictations | Add-Member rounds_ok (@($rounds | Where-Object { $_.status -eq 'ok' }).Count)
    $R.dictations | Add-Member rounds_total $rounds.Count
    $lat = (Invoke-Ot $Exe @('--bench', 'latency') $e 30).Out | ConvertFrom-Json
    $R['latency'] = $lat
    Write-Host ("dictations: {0}/{1} ok; CPU {2}% of all cores over {3}s; WS peak {4} MB, private peak {5} MB" -f $R.dictations.rounds_ok, $R.dictations.rounds_total, $R.dictations.CpuPctAll, $R.dictations.Seconds, $R.dictations.WsPeakMB, $R.dictations.PrivPeakMB)
    Write-Host ("latency (median ms): pill {0}, mic {1}, prep {2}, stt {3}, cleanup {4}, clipboard {5}, total {6}, local before paste {7}" -f $lat.hotkey_pill.median, $lat.hotkey_audio.median, $lat.prep.median, $lat.stt.median, $lat.cleanup.median, $lat.paste.median, $lat.total.median, $lat.local_before_paste.median)
    Start-Sleep -Seconds 3
    $R['after_dictations'] = Measure-Process $daemon 5

    # ---- 30 s recording, injected -----------------------------------------------------
    Section "recording $RecordSec s (injected WAV, then silence)"
    $before = (Read-History $h).Count
    Invoke-Ot $Exe @('--bench', 'toggle') $e 15 | Out-Null
    $R['recording_injected'] = Measure-Process $daemon $RecordSec
    Invoke-Ot $Exe @('--bench', 'toggle') $e 15 | Out-Null
    Wait-History $h ($before + 1) 30 | Out-Null
    Write-Host ("recording (injected): {0}% of all cores ({1}% of one core), WS peak {2} MB, private peak {3} MB" -f $R.recording_injected.CpuPctAll, $R.recording_injected.CpuPctCore, $R.recording_injected.WsPeakMB, $R.recording_injected.PrivPeakMB)

    # ---- settings window --------------------------------------------------------------
    Section "settings window open ($SettingsSec s)"
    Start-Sleep -Seconds 2
    Invoke-Ot $Exe @('--bench', 'settings') $e 15 | Out-Null
    $child = $null
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while (-not $child -and $sw.Elapsed.TotalSeconds -lt 15) {
        Start-Sleep -Milliseconds 300
        $c = Get-CimInstance Win32_Process -Filter "ParentProcessId=$($daemon.Id) AND Name='opentalk.exe'" | Select-Object -First 1
        if ($c) { $child = Get-Process -Id $c.ProcessId -ErrorAction SilentlyContinue }
    }
    if ($child) {
        $R['settings_open_ms'] = [int]$sw.Elapsed.TotalMilliseconds
        Start-Sleep -Seconds 4   # let the first frames settle
        $R['settings'] = Measure-Process $child $SettingsSec
        $R.settings | Add-Member private_ws_mb (Get-PrivateWsMB $child.Id)
        $R['daemon_with_settings'] = Measure-Process $daemon 3
        Write-Host ("settings: {0}% of all cores, WS {1} MB (peak {2}), private {3} MB, {4} threads" -f $R.settings.CpuPctAll, $R.settings.WsMeanMB, $R.settings.WsPeakMB, $R.settings.PrivMeanMB, $R.settings.Threads)
        $child.CloseMainWindow() | Out-Null
        if (-not $child.WaitForExit(4000)) { $child.Kill() }
        Start-Sleep -Seconds 2
        $R['daemon_after_settings'] = Measure-Process $daemon 3
    } else { Write-Host 'settings window did not start' -ForegroundColor Yellow; $R['settings'] = $null }
}
finally {
    Stop-Daemon $Exe $daemon $e
    Remove-Item -Recurse -Force $h -ErrorAction SilentlyContinue
}

# ---- real microphone ----------------------------------------------------------------------
if (-not $SkipMic) {
    Section "recording $RecordSec s (real microphone, cancelled, no upload)"
    $h = New-TempHome 'ot-bench-mic'
    $e = New-Env $h @{ OPENTALK_STT_BASE_URL = $mock.Url; OPENTALK_SECRET_OPENAI = 'bench-dummy-key-123456' }
    Write-Config $h @{ audio_cues = $false; auto_paste = $false; stt = @{ provider = 'openai' } }
    $daemon = Start-Daemon $Exe $e
    try {
        Start-Sleep -Seconds 2
        Invoke-Ot $Exe @('--bench', 'toggle') $e 15 | Out-Null
        Start-Sleep -Milliseconds 1500
        $log = (Read-Log $h) -join "`n"
        if ($log -match 'cannot start the microphone') {
            Write-Host 'no usable microphone on this machine; skipped' -ForegroundColor Yellow
            $R['recording_mic'] = $null
        } else {
            $R['recording_mic'] = Measure-Process $daemon $RecordSec
            Write-Host ("recording (mic): {0}% of all cores ({1}% of one core), WS peak {2} MB, private peak {3} MB" -f $R.recording_mic.CpuPctAll, $R.recording_mic.CpuPctCore, $R.recording_mic.WsPeakMB, $R.recording_mic.PrivPeakMB)
            Invoke-Ot $Exe @('--bench', 'cancel') $e 15 | Out-Null
        }
    }
    finally {
        Stop-Daemon $Exe $daemon $e
        Remove-Item -Recurse -Force $h -ErrorAction SilentlyContinue
    }
}
Stop-Quietly $mock.Proc

# ---- real paste (e2e) ---------------------------------------------------------------------
$R['e2e'] = @()
if (-not $SkipE2E) {
    if ($R.env.desktop_locked) { Write-Host 'e2e paste skipped: desktop is locked' -ForegroundColor Yellow }
    else {
        foreach ($mode in @('mock', 'mock-stream')) {
            Section "real paste into a text box (e2e, $mode)"
            $script = Join-Path $PSScriptRoot '..\tests\e2e.ps1'
            $raw = & powershell -NoProfile -ExecutionPolicy Bypass -File $script -Exe $Exe -Stt $mode -Target form -Mode paste 2>&1 | Out-String
            $json = $null
            if ($raw -match '(?s)(\{.*\})') { try { $json = $Matches[1] | ConvertFrom-Json } catch {} }
            $row = [ordered]@{ stt = $mode; pass = [bool]($json -and $json.pass); error = if ($json) { $json.error } else { 'no JSON from tests\e2e.ps1' } }
            if ($json -and $json.latency_line) { $row.latency = ConvertFrom-LatencyLine $json.latency_line }
            $R['e2e'] += [pscustomobject]$row
            Write-Host ("e2e {0}: pass={1} {2}" -f $mode, $row.pass, $(if ($row.latency) { "paste {0} ms, total {1} ms" -f $row.latency.paste, $row.latency.total } else { $row.error }))
        }
    }
}

# ---- live OpenRouter (free models only) ---------------------------------------------------
$R['live'] = $null
if (-not $SkipLive) {
    Section "live OpenRouter ($LiveModel), $LiveDictations dictations"
    $key = Read-OpenRouterKey $KeyFile
    if (-not $key -or -not $LiveModel.EndsWith(':free')) {
        Write-Host 'skipped (no key, or model is not :free)' -ForegroundColor Yellow
    } else {
        $h = New-TempHome 'ot-bench-live'
        $e = New-Env $h @{ OPENTALK_INJECT_WAV = $wav; OPENTALK_SECRET_OPENROUTER = $key }
        Write-Config $h @{ audio_cues = $false; auto_paste = $false; keep_in_clipboard = $true; stt = @{ provider = 'openrouter'; models = @{ openrouter = $LiveModel }; timeout_ms = 45000 } }
        $daemon = Start-Daemon $Exe $e
        try {
            $d = Invoke-Ot $Exe @('--bench', 'dictate', $LiveDictations, $wav) $e 600
            $rounds = @($d.Out -split "`n" | Where-Object { $_.Trim().StartsWith('{') } | ForEach-Object { $_ | ConvertFrom-Json })
            $hist = Read-History $h
            $live = [ordered]@{ model = $LiveModel; rounds = $rounds.Count; ok = @($hist | Where-Object { $_.status -eq 'ok' }).Count
                errors = @($hist | Where-Object { $_.status -eq 'failed' } | ForEach-Object { $_.error } | Select-Object -Unique) }
            $live.latency = (Invoke-Ot $Exe @('--bench', 'latency') $e 30).Out | ConvertFrom-Json
            $R['live'] = $live
            Write-Host ("live: {0}/{1} ok; errors: {2}" -f $live.ok, $live.rounds, ($live.errors -join ' | '))
        }
        finally {
            Stop-Daemon $Exe $daemon $e
            Remove-Item -Recurse -Force $h -ErrorAction SilentlyContinue
        }
    }
}

$R | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 (Join-Path $outDir 'results.json')

# ---- report -------------------------------------------------------------------------------
function Fmt($v, [string]$unit = '') { if ($null -eq $v) { 'n/a' } else { "$v$unit" } }
function Verdict([bool]$ok) { if ($ok) { 'meets' } else { 'misses' } }
$lat = $R.latency
$rec = if ($R.recording_mic) { $R.recording_mic } else { $R.recording_injected }
$recLabel = if ($R.recording_mic) { 'real microphone' } else { 'injected WAV' }
$md = @"
# OpenTalk benchmark

Generated by ``bench/bench.ps1`` on $($R.env.date) - $($R.env.cpu), $($R.env.logical_cores) logical cores, $($R.env.os).
Release build ($($R.env.exe_built)), single executable $($R.env.exe_mb) MB. Reference: **Spokenly ~3% CPU, ~110 MB RAM** (PLAN section 1).

CPU % is process CPU time / (wall time x $($R.env.logical_cores) logical cores), i.e. what Task Manager's CPU column shows;
the one-core figure (100% = one fully busy core) is given in brackets.

## Summary

| Metric | OpenTalk | Target (PLAN 1) | Spokenly | Result |
|---|---|---|---|---|
| Idle CPU (daemon, $($R.idle.Seconds) s) | $($R.idle.CpuPctAll)% ($($R.idle.CpuPctCore)% of one core) | ~0.0% | ~3% | $(Verdict ($R.idle.CpuPctCore -lt 0.5)) |
| Recording CPU ($recLabel, $RecordSec s) | $($rec.CpuPctAll)% ($($rec.CpuPctCore)% of one core) | <= 1.5% | ~3% | $(Verdict ($rec.CpuPctAll -le 1.5)) |
| Daemon working set, idle (mean / peak) | $($R.idle.WsMeanMB) / $($R.idle.WsPeakMB) MB | <= 25 MB | ~110 MB | $(Verdict ($R.idle.WsPeakMB -le 25)) |
| Daemon private bytes, idle (mean / peak) | $($R.idle.PrivMeanMB) / $($R.idle.PrivPeakMB) MB | <= 15 MB | n/a | $(Verdict ($R.idle.PrivPeakMB -le 15)) |
| Daemon private working set, idle (Task Manager "Memory") | $(Fmt $R.idle.private_ws_mb ' MB') | n/a | ~110 MB | - |
| Daemon working set during $Dictations dictations (peak) | $($R.dictations.WsPeakMB) MB | <= 25 MB | ~110 MB | $(Verdict ($R.dictations.WsPeakMB -le 25)) |
| Daemon working set while recording (peak) | $($rec.WsPeakMB) MB | <= 25 MB | ~110 MB | $(Verdict ($rec.WsPeakMB -le 25)) |
| Settings window working set (mean / peak) | $(if ($R.settings) { "$($R.settings.WsMeanMB) / $($R.settings.WsPeakMB) MB" } else { 'n/a' }) | <= 80 MB, only while open | n/a | $(if ($R.settings) { Verdict ($R.settings.WsPeakMB -le 80) } else { '-' }) |
| Settings window CPU while idle | $(if ($R.settings) { "$($R.settings.CpuPctAll)%" } else { 'n/a' }) | ~0 | n/a | - |
| Executable size | $($R.env.exe_mb) MB | <= 8 MB | n/a | $(Verdict ($R.env.exe_mb -le 8)) |
| Startup (process start -> daemon window, median of 5) | $($R.startup.process_to_window_ms_median) ms (daemon reports ready at $(Fmt $R.startup.daemon_ready_log_ms_median ' ms')) | - | - | - |
| Daemon threads / handles (idle) | $($R.idle.Threads) / $($R.idle.Handles) | - | - | - |
| Microphone in use while idle (prepared stream) | $(if ($R.mic_idle.known) { if ($R.mic_idle.in_use) { 'yes' } else { 'no' } } else { 'not recorded by Windows yet' }) | no (mic indicator off) | - | $(if ($R.mic_idle.known) { Verdict (-not $R.mic_idle.in_use) } else { '-' }) |

## Latency ($($lat.count) dictations, local mock STT with no network delay, median / p95 in ms)

| Stage | Median | p95 | Target |
|---|---|---|---|
| Hotkey -> pill repainted | $($lat.hotkey_pill.median) | $($lat.hotkey_pill.p95) | <= 16 |
| Hotkey -> first microphone packet | $($lat.hotkey_audio.median) | $($lat.hotkey_audio.p95) | <= 50 |
| Stop -> request sent (stop recorder, VAD, trim) | $($lat.prep.median) | $($lat.prep.p95) | - |
| STT request + response (mock server, loopback) | $($lat.stt.median) | $($lat.stt.p95) | network RTT |
| Cleanup (deterministic + snippets) | $($lat.cleanup.median) | $($lat.cleanup.p95) | < 1 |
| Clipboard write (``auto_paste`` off in this run; see note) | $($lat.paste.median) | $($lat.paste.p95) | - |
| Stop -> text delivered | $($lat.total.median) | $($lat.total.p95) | - |
| **Local overhead before paste (prep + cleanup)** | **$($lat.local_before_paste.median)** | **$($lat.local_before_paste.p95)** | **<= 40** |
| Local overhead including the clipboard write (total - STT) | $($lat.local_overhead.median) | $($lat.local_overhead.p95) | - |

Upload per dictation: $(Fmt $lat.upload_bytes.median ' bytes') (16 kHz mono PCM WAV after silence trim).
The paste stage here only writes the clipboard (the benchmark runs with ``auto_paste = false``); a real paste adds the
10 ms clipboard settle plus ``SendInput``. $(if ($R.env.desktop_locked) { 'The desktop was **locked** during this run (LogonUI active), where Windows denies clipboard access, so the clipboard write retries and fails; its time is not representative. Re-run on an unlocked desktop for the real figure.' })

$(if (@($R.e2e).Count) {
"## Real paste (tests/e2e.ps1, scratch text box, local mock STT)

The same pipeline with ``auto_paste = true``: clipboard snapshot, focus check, ``SendInput`` Ctrl+V into a WinForms text box,
then the text is read back with UI Automation.

| STT transport | Passed | Stop -> request (ms) | STT (ms) | Cleanup (ms) | Paste (ms) | Stop -> pasted (ms) | Local overhead (ms) |
|---|---|---|---|---|---|---|---|
" + ((@($R.e2e) | ForEach-Object { $l = $_.latency; if ($l) { "| $($_.stt) | $($_.pass) | $($l.prep) | $($l.stt) | $($l.cleanup) | $($l.paste) | $($l.total) | $([math]::Round($l.total - $l.stt, 1)) |" } else { "| $($_.stt) | $($_.pass) | - | - | - | - | - | $($_.error) |" } }) -join "`n") + "

The mock answers 40 ms after the request (or after ``Finalize`` when streaming), so local overhead = total - STT.
"
})
## Scenario details

| Scenario | Duration | CPU (all cores) | CPU (one core) | WS mean / peak MB | Private mean / peak MB | Threads | Handles |
|---|---|---|---|---|---|---|---|
| Idle | $($R.idle.Seconds) s | $($R.idle.CpuPctAll)% | $($R.idle.CpuPctCore)% | $($R.idle.WsMeanMB) / $($R.idle.WsPeakMB) | $($R.idle.PrivMeanMB) / $($R.idle.PrivPeakMB) | $($R.idle.Threads) | $($R.idle.Handles) |
| $Dictations dictations (mock STT) | $($R.dictations.Seconds) s | $($R.dictations.CpuPctAll)% | $($R.dictations.CpuPctCore)% | $($R.dictations.WsMeanMB) / $($R.dictations.WsPeakMB) | $($R.dictations.PrivMeanMB) / $($R.dictations.PrivPeakMB) | $($R.dictations.Threads) | $($R.dictations.Handles) |
| Idle again after the dictations | $($R.after_dictations.Seconds) s | $($R.after_dictations.CpuPctAll)% | $($R.after_dictations.CpuPctCore)% | $($R.after_dictations.WsMeanMB) / $($R.after_dictations.WsPeakMB) | $($R.after_dictations.PrivMeanMB) / $($R.after_dictations.PrivPeakMB) | $($R.after_dictations.Threads) | $($R.after_dictations.Handles) |
| Recording (injected WAV) | $($R.recording_injected.Seconds) s | $($R.recording_injected.CpuPctAll)% | $($R.recording_injected.CpuPctCore)% | $($R.recording_injected.WsMeanMB) / $($R.recording_injected.WsPeakMB) | $($R.recording_injected.PrivMeanMB) / $($R.recording_injected.PrivPeakMB) | $($R.recording_injected.Threads) | $($R.recording_injected.Handles) |
$(if ($R.recording_mic) { "| Recording (real microphone) | $($R.recording_mic.Seconds) s | $($R.recording_mic.CpuPctAll)% | $($R.recording_mic.CpuPctCore)% | $($R.recording_mic.WsMeanMB) / $($R.recording_mic.WsPeakMB) | $($R.recording_mic.PrivMeanMB) / $($R.recording_mic.PrivPeakMB) | $($R.recording_mic.Threads) | $($R.recording_mic.Handles) |" })
$(if ($R.settings) { "| Settings window open (separate process) | $($R.settings.Seconds) s | $($R.settings.CpuPctAll)% | $($R.settings.CpuPctCore)% | $($R.settings.WsMeanMB) / $($R.settings.WsPeakMB) | $($R.settings.PrivMeanMB) / $($R.settings.PrivPeakMB) | $($R.settings.Threads) | $($R.settings.Handles) |" })
$(if ($R.daemon_with_settings) { "| Daemon while Settings is open | $($R.daemon_with_settings.Seconds) s | $($R.daemon_with_settings.CpuPctAll)% | $($R.daemon_with_settings.CpuPctCore)% | $($R.daemon_with_settings.WsMeanMB) / $($R.daemon_with_settings.WsPeakMB) | $($R.daemon_with_settings.PrivMeanMB) / $($R.daemon_with_settings.PrivPeakMB) | $($R.daemon_with_settings.Threads) | $($R.daemon_with_settings.Handles) |" })
$(if ($R.daemon_after_settings) { "| Daemon after Settings closed | $($R.daemon_after_settings.Seconds) s | $($R.daemon_after_settings.CpuPctAll)% | $($R.daemon_after_settings.CpuPctCore)% | $($R.daemon_after_settings.WsMeanMB) / $($R.daemon_after_settings.WsPeakMB) | $($R.daemon_after_settings.PrivMeanMB) / $($R.daemon_after_settings.PrivPeakMB) | $($R.daemon_after_settings.Threads) | $($R.daemon_after_settings.Handles) |" })

$(if ($R.settings) { "Settings window: opened in $($R.settings_open_ms) ms; private working set $(Fmt $R.settings.private_ws_mb ' MB'). It is a separate process that exits when closed, so the daemon never carries the GL/egui stack." })

## Live check (OpenRouter, ``:free`` models only)

$(if ($R.live) { "Model ``$($R.live.model)``: $($R.live.ok) of $($R.live.rounds) dictations succeeded.`n`n" + $(if ($R.live.errors.Count) { "Errors reported by the provider:`n`n" + (($R.live.errors | ForEach-Object { "- $_" }) -join "`n") } else { "No errors." }) } else { 'Not run.' })

Free OpenRouter models are rate limited and sometimes not available to non-agent clients, so this section is a smoke test of the real
network path, not a latency reference; the table above is measured against a local mock to isolate OpenTalk's own cost.

## Method

- Release build, GNU toolchain, ``opt-level = 3``, fat LTO, stripped. One executable; Settings is the same exe started as ``--settings``.
- Each scenario starts a fresh daemon with its own ``OPENTALK_HOME``. Dictations are driven through the same message the hotkey
  produces (``--bench toggle`` / ``--bench dictate``); the microphone is replaced by ``OPENTALK_INJECT_WAV`` (real-time playback of
  ``tests/fixtures/tts_fillers.wav``, $($R.env.wav_ms) ms) except where stated.
- STT goes to ``opentalk --bench mock-stt``, an in-repo OpenAI-shaped server on 127.0.0.1 that answers instantly, so network
  speed does not enter the numbers.
- Samples are taken every 250 ms with ``Process.TotalProcessorTime``, ``WorkingSet64`` and ``PrivateMemorySize64``.
- Latencies come from the daemon's own QPC-stamped log lines (``latency seq=...``), read back with ``--bench latency``.
"@
$md | Set-Content -Encoding UTF8 (Join-Path $PSScriptRoot '..\docs\BENCHMARK.md')
Write-Host "`nwrote docs\BENCHMARK.md and bench\out\results.json"
