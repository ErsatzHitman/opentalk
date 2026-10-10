# OpenTalk benchmark (docs/PLAN.md section 11). Writes docs\BENCHMARK.md (or -OutFile) and bench\out\results.json.
#
#   powershell -File bench\bench.ps1 [-Exe path] [-Quick] [-SkipLive] [-SkipMic] [-OutFile path] [-CheckSizeOnly]
#
# -OutFile       report path (default docs\BENCHMARK.md). Blocks between `<!-- keep:begin NAME -->` and
#                `<!-- keep:end NAME -->` lines in the existing file are re-emitted byte for byte (see
#                Write-ReportKeepingBlocks in common.ps1); everything else is regenerated.
# -CheckSizeOnly only check the release exe against the 8,000,000 byte budget (for CI): exit 1 when over.
#                A normal run also exits 1 after writing the report when the exe is over budget.
# The OpenRouter key comes from -KeyFile / OPENTALK_OPENROUTER_KEY_FILE, else OPENTALK_SECRET_OPENROUTER.
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
    [string]$LiveModels = 'nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free,thinkingmachines/inkling-small:free,thinkingmachines/inkling:free',
    [switch]$Quick,
    [switch]$SkipLive,
    [switch]$SkipMic,
    [switch]$SkipE2E,
    [string]$OutFile = (Join-Path $PSScriptRoot '..\docs\BENCHMARK.md'),
    [switch]$CheckSizeOnly
)
. (Join-Path $PSScriptRoot 'common.ps1')
if (-not $KeyFile) { $KeyFile = $script:KeyFileDefault }
if ($Quick) { $IdleSec = 10; $Dictations = 3; $RecordSec = 8; $SettingsSec = 8; $LiveDictations = 1 }
$Exe = (Resolve-Path $Exe).Path

# Release executable budget (PLAN section 1): decimal bytes, so the MB figures (bytes / 10^6) match what the budget says.
$SizeBudgetBytes = 8000000
function Format-Bytes([long]$n) { return $n.ToString('N0', [Globalization.CultureInfo]::InvariantCulture) }
function Format-ExeMB([long]$n) { return [math]::Round($n / 1e6, 2).ToString('0.00', [Globalization.CultureInfo]::InvariantCulture) }
# Prints the size verdict; returns $true when within budget.
function Test-ExeSize([string]$exePath) {
    $n = (Get-Item $exePath).Length
    if ($n -le $SizeBudgetBytes) {
        Write-Host ("exe size {0} bytes ({1} MB (10^6 bytes)) is within the {2} byte budget" -f (Format-Bytes $n), (Format-ExeMB $n), (Format-Bytes $SizeBudgetBytes))
        return $true
    }
    Write-Host ("FAIL: exe size {0} bytes ({1} MB (10^6 bytes)) exceeds the {2} byte budget by {3} bytes" -f (Format-Bytes $n), (Format-ExeMB $n), (Format-Bytes $SizeBudgetBytes), (Format-Bytes ($n - $SizeBudgetBytes))) -ForegroundColor Red
    return $false
}
if ($CheckSizeOnly) { if (Test-ExeSize $Exe) { exit 0 } else { exit 1 } }

$wav = (Resolve-Path (Join-Path $PSScriptRoot '..\tests\fixtures\tts_fillers.wav')).Path
$wavMs = Get-WavMs $wav
$outDir = Join-Path $PSScriptRoot 'out'
New-Item -ItemType Directory -Force -Path $outDir | Out-Null
$R = @{}
$mock = $null   # set by the idle section; the trap below reads it under strict mode
# Optional scenarios leave their key null (the report reads them under strict mode).
foreach ($k in 'recording_mic', 'settings', 'settings_open_ms', 'daemon_with_settings', 'daemon_after_settings') { $R[$k] = $null }

function Section([string]$t) { Write-Host ("`n== {0}" -f $t) -ForegroundColor Cyan }

# CPU of the thread(s) named $name between two Get-ThreadTimes snapshots, as % of one core.
function Get-ThreadPct($before, $after, [double]$seconds, [string]$name) {
    $ms = 0.0
    foreach ($id in $after.Keys) {
        if ($after[$id].Name -ne $name) { continue }
        $b = if ($before.ContainsKey($id)) { $before[$id].Ms } else { 0 }
        $ms += $after[$id].Ms - $b
    }
    return [math]::Round($ms / ($seconds * 1000) * 100, 2)
}

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

# Private working set (what Task Manager shows as "Memory"): PROCESS_MEMORY_COUNTERS_EX2.PrivateWorkingSetSize via
# K32GetProcessMemoryInfo (the 'Process V2' perf counter throws invalid-sample errors on some machines). Falls back
# to the WMI perf class (WorkingSetPrivate). Returns MB, or $null when neither works.
function Get-PrivateWsBytes([int]$procId) {
    if (-not ('OtMem' -as [type])) {
        Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public static class OtMem {
    [StructLayout(LayoutKind.Sequential)]
    struct PMC_EX2 {
        public uint cb; public uint PageFaultCount;
        public UIntPtr PeakWorkingSetSize, WorkingSetSize, QuotaPeakPagedPoolUsage, QuotaPagedPoolUsage,
            QuotaPeakNonPagedPoolUsage, QuotaNonPagedPoolUsage, PagefileUsage, PeakPagefileUsage, PrivateUsage;
        public UIntPtr PrivateWorkingSetSize;
        public ulong SharedCommitUsage;
    }
    [DllImport("kernel32.dll", SetLastError = true)] static extern IntPtr OpenProcess(uint access, bool inherit, uint pid);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll", EntryPoint = "K32GetProcessMemoryInfo", SetLastError = true)]
    static extern bool GetProcessMemoryInfo(IntPtr h, out PMC_EX2 c, uint cb);
    // -1 when the process cannot be opened or the OS does not know the EX2 layout.
    public static long PrivateWorkingSet(uint pid) {
        IntPtr h = OpenProcess(0x1000 | 0x0010, false, pid);   // PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ
        if (h == IntPtr.Zero) return -1;
        try {
            PMC_EX2 c;
            if (!GetProcessMemoryInfo(h, out c, (uint)Marshal.SizeOf(typeof(PMC_EX2)))) return -1;
            return (long)c.PrivateWorkingSetSize.ToUInt64();
        } finally { CloseHandle(h); }
    }
}
'@
    }
    try { $b = [OtMem]::PrivateWorkingSet([uint32]$procId); if ($b -gt 0) { return [long]$b } } catch {}
    try {
        $o = Get-CimInstance -ClassName Win32_PerfFormattedData_PerfProc_Process -Filter "IDProcess=$procId" -ErrorAction Stop | Select-Object -First 1
        if ($o -and $o.WorkingSetPrivate) { return [long]$o.WorkingSetPrivate }
    } catch {}
    return $null
}

function Get-PrivateWsMB([int]$procId) {
    $b = Get-PrivateWsBytes $procId
    if ($null -eq $b) { return $null }
    return [math]::Round($b / 1MB, 1)
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
    exe_bytes = $exeInfo.Length; exe_mb = [math]::Round($exeInfo.Length / 1e6, 2); size_budget_bytes = $SizeBudgetBytes; exe_built = $exeInfo.LastWriteTime.ToString('s')
    wav_ms = $wavMs; date = (Get-Date).ToString('s')
    desktop_locked = [bool](Get-Process -Name LogonUI -ErrorAction SilentlyContinue | Where-Object { $_.SessionId -eq (Get-Process -Id $PID).SessionId })
}
Write-Host ("exe {0} bytes ({1} MB (10^6 bytes)), {2} cores, {3}" -f (Format-Bytes $R.env.exe_bytes), (Format-ExeMB $R.env.exe_bytes), $R.env.logical_cores, $cpuName)

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
# An error that ends the script must not leave the mock STT running (each daemon has its own finally).
trap { if ($mock) { Stop-Quietly $mock.Proc }; break }
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
    $thr0 = Get-ThreadTimes $daemon.Id
    $R['recording_injected'] = Measure-Process $daemon $RecordSec
    $thr1 = Get-ThreadTimes $daemon.Id
    $R.recording_injected | Add-Member top_threads (Get-TopThreads $thr0 $thr1 $RecordSec)
    $R.recording_injected | Add-Member main_thread_pct (Get-ThreadPct $thr0 $thr1 $RecordSec 'main')
    $R.recording_injected | Add-Member private_ws_mb (Get-PrivateWsMB $daemon.Id)
    Invoke-Ot $Exe @('--bench', 'toggle') $e 15 | Out-Null
    Wait-History $h ($before + 1) 30 | Out-Null
    Write-Host ("recording (injected): {0}% of all cores ({1}% of one core), WS peak {2} MB, private peak {3} MB; busiest threads: {4}" -f $R.recording_injected.CpuPctAll, $R.recording_injected.CpuPctCore, $R.recording_injected.WsPeakMB, $R.recording_injected.PrivPeakMB, ($R.recording_injected.top_threads -join ', '))

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
    # App bubble off here: this run isolates the cost of capture + pill (the bubble's icon lookup is
    # measured by the injected run above).
    Write-Config $h @{ audio_cues = $false; auto_paste = $false; show_app_bubble = $false; start_minimized = $true; stt = @{ provider = 'openai' } }
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
            $thr0 = Get-ThreadTimes $daemon.Id
            $R['recording_mic'] = Measure-Process $daemon $RecordSec
            $thr1 = Get-ThreadTimes $daemon.Id
            $R.recording_mic | Add-Member private_ws_mb (Get-PrivateWsMB $daemon.Id)
            $R.recording_mic | Add-Member top_threads (Get-TopThreads $thr0 $thr1 $RecordSec)
            # the main thread owns the message loop and the pill; this is the number the pill work is judged by
            $R.recording_mic | Add-Member main_thread_pct (Get-ThreadPct $thr0 $thr1 $RecordSec 'main')
            Write-Host ("recording (mic): {0}% of all cores ({1}% of one core), main thread {4}% of one core, WS peak {2} MB, private peak {3} MB; busiest threads: {5}" -f $R.recording_mic.CpuPctAll, $R.recording_mic.CpuPctCore, $R.recording_mic.WsPeakMB, $R.recording_mic.PrivPeakMB, $R.recording_mic.main_thread_pct, ($R.recording_mic.top_threads -join ', '))
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
            $row = [ordered]@{ stt = $mode; latency = $null; pass = [bool]($json -and $json.pass); clipboard_restored = $(if ($json -and $json.PSObject.Properties['clipboard_restored']) { $json.clipboard_restored } else { $null }); error = if ($json -and $json.PSObject.Properties['error']) { $json.error } elseif ($json) { $null } else { 'no JSON from tests\e2e.ps1' } }
            if ($json -and $json.PSObject.Properties['latency_line'] -and $json.latency_line) { $row.latency = ConvertFrom-LatencyLine $json.latency_line }
            $R['e2e'] += [pscustomobject]$row
            Write-Host ("e2e {0}: pass={1} {2}" -f $mode, $row.pass, $(if ($row.latency) { "paste {0} ms, total {1} ms" -f $row.latency.paste, $row.latency.total } else { $row.error }))
        }
    }
}

# ---- live OpenRouter (free models only) ---------------------------------------------------
# Rotates over the free audio-input models (free tiers rate-limit, gate or drop audio); each round
# uses the next model whose last attempt did not fail. The key comes from -KeyFile /
# OPENTALK_OPENROUTER_KEY_FILE, else OPENTALK_SECRET_OPENROUTER; it only ever travels in the
# daemon's environment and is never printed or written.
$R['live'] = $null
$R['live_status'] = if ($SkipLive) { 'not run (-SkipLive)' } else { 'not run' }
if (-not $SkipLive) {
    $models = @($LiveModels -split ',' | ForEach-Object { $_.Trim() } | Where-Object { $_.EndsWith(':free') })
    $key = Read-OpenRouterKey $KeyFile
    Section "live OpenRouter ($($models -join ', ')), $LiveDictations dictations"
    if (-not $key) {
        $R['live_status'] = 'skipped: no key'
        Write-Host 'skipped: no key (set OPENTALK_SECRET_OPENROUTER, or pass -KeyFile)' -ForegroundColor Yellow
    } elseif ($models.Count -eq 0) {
        $R['live_status'] = 'skipped: no :free audio model'
        Write-Host 'skipped: no :free audio model in -LiveModels' -ForegroundColor Yellow
    } else {
        $R['live_status'] = 'ran'
        $live = [ordered]@{ models = $models; rounds = 0; ok = 0; verified = 0; per_model = [ordered]@{} }
        $round = 0
        foreach ($m in $models) {
            if ($round -ge $LiveDictations) { break }
            $h = New-TempHome 'ot-bench-live'
            $e = New-Env $h @{ OPENTALK_INJECT_WAV = $wav; OPENTALK_SECRET_OPENROUTER = $key }
            Write-Config $h @{ audio_cues = $false; auto_paste = $false; keep_in_clipboard = $true; start_minimized = $true; stt = @{ provider = 'openrouter'; models = @{ openrouter = $m }; timeout_ms = 90000 } }
            $daemon = Start-Daemon $Exe $e
            try {
                $n = 1
                $dr = Invoke-Ot $Exe @('--bench', 'dictate', $n, $wav, '150') $e 900
                $round += $n
                $hist = Read-History $h
                $dictateStatus = @($dr.Out -split "`n" | Where-Object { $_.Trim().StartsWith('{') } | ForEach-Object { try { ($_ | ConvertFrom-Json).status } catch {} })
                $ok = @($hist | Where-Object { $_.status -eq 'ok' })
                # A dictation only counts as verified when the transcript has the words in the fixture (tts_fillers.wav).
                $verified = @($ok | Where-Object { $_.text -match '(?i)friday|release' })
                $live.rounds += $hist.Count; $live.ok += $ok.Count; $live.verified += $verified.Count
                $live.per_model[$m] = [ordered]@{
                    ok = $ok.Count; verified = $verified.Count; total = $hist.Count
                    errors = @(@($hist | Where-Object { $_.status -ne 'ok' } | ForEach-Object { if ($_.error) { $_.error } else { "status $($_.status)" } }) + $(if ($hist.Count -eq 0) { "no history entry within 150 s (dictation $($dictateStatus -join '/'); the free tier answered slowly or kept rate limiting)" }) | Select-Object -Unique)
                    texts = @($ok | ForEach-Object { $_.text })
                }
                if ($ok.Count) { $live.latency = (Invoke-Ot $Exe @('--bench', 'latency') $e 30).Out | ConvertFrom-Json }
                $verdict = if ($verified.Count) { 'VERIFIED' } else { 'UNVERIFIED' }
                Write-Host ("live {0}: {4} - {1}/{2} ok, {5} with the spoken words {3}" -f $m, $ok.Count, $hist.Count, (($live.per_model[$m].errors) -join ' | '), $verdict, $verified.Count)
            }
            finally {
                Stop-Daemon $Exe $daemon $e
                Remove-Item -Recurse -Force $h -ErrorAction SilentlyContinue
            }
        }
        $R['live'] = $live
    }
}
$key = $null

$R | ConvertTo-Json -Depth 8 | Set-Content -Encoding UTF8 (Join-Path $outDir 'results.json')

# ---- report -------------------------------------------------------------------------------
function Fmt($v, [string]$unit = '') { if ($null -eq $v) { 'n/a' } else { "$v$unit" } }
function Verdict([bool]$ok) { if ($ok) { 'meets' } else { 'misses' } }
$lat = $R.latency
$sizeOk = ($R.env.exe_bytes -le $SizeBudgetBytes)
$exeSizeText = "$(Format-ExeMB $R.env.exe_bytes) MB (10^6 bytes), $(Format-Bytes $R.env.exe_bytes) bytes"
# Live section text (the status says why it did not run; an all-UNVERIFIED run says so and points to the evidence section).
$liveMd = switch -Wildcard ($R.live_status) {
    'ran' {
        $t = "$($R.live.verified) of $($R.live.rounds) live dictations VERIFIED (transcript contains the spoken words); $($R.live.ok) returned any text."
        if ($R.live.verified -eq 0) {
            $t += " **UNVERIFIED:** no free model produced a transcript of the audio, so this run says nothing about live STT correctness. The free NVIDIA endpoint drops the audio (``prompt_tokens=28`` with or without the WAV) and the inkling models answer 403; see the section 'Live STT on the free tier (evidence)'."
        }
        $t + "`n`n| Model | OK | Errors reported (redacted by OpenTalk) |`n|---|---|---|`n" + (($R.live.per_model.Keys | ForEach-Object { $pm = $R.live.per_model[$_]; "| ``$_`` | $($pm.ok) / $($pm.total) | $((@($pm.errors) | ForEach-Object { $_ -replace '\|', '/' }) -join '; ') |" }) -join "`n")
    }
    'skipped: no key' { '**Skipped: no key.** Set `OPENTALK_SECRET_OPENROUTER` (or pass `-KeyFile`) to run it.' }
    'skipped: no :free audio model' { '**Skipped: no :free audio model** in `-LiveModels`.' }
    default { "Not run ($($R.live_status))." }
}
$clipNote = if ($R.env.desktop_locked) {
    '- Clipboard and paste timings in this run are not representative: the workstation was locked (LogonUI active), where Windows refuses clipboard access, so `copy_to_clipboard` exhausts its retries (~95 ms) and real-paste e2e runs are skipped. Re-run on an unlocked desktop.'
} else {
    '- Clipboard and paste timings are only meaningful on an unlocked desktop (Windows refuses clipboard access while the workstation is locked); this run found the desktop unlocked.'
}
$rec = if ($R.recording_mic) { $R.recording_mic } else { $R.recording_injected }
$recLabel = if ($R.recording_mic) { 'real microphone' } else { 'injected WAV' }
$md = @"
# OpenTalk benchmark

Generated by ``bench/bench.ps1`` on $($R.env.date) - $($R.env.cpu), $($R.env.logical_cores) logical cores, $($R.env.os).
Release build ($($R.env.exe_built)), single executable $exeSizeText (budget $(Format-Bytes $SizeBudgetBytes) bytes). Reference: **Spokenly ~3% CPU, ~110 MB RAM** (PLAN section 1).

CPU % is process CPU time / (wall time x $($R.env.logical_cores) logical cores), i.e. what Task Manager's CPU column shows;
the one-core figure (100% = one fully busy core) is given in brackets.

## Summary

| Metric | OpenTalk | Target (PLAN 1) | Spokenly | Result |
|---|---|---|---|---|
| Idle CPU (daemon, $($R.idle.Seconds) s) | $($R.idle.CpuPctAll)% ($($R.idle.CpuPctCore)% of one core) | ~0.0% | ~3% | $(Verdict ($R.idle.CpuPctCore -lt 0.5)) |
| Recording CPU ($recLabel, $RecordSec s) | $($rec.CpuPctAll)% ($($rec.CpuPctCore)% of one core) | <= 1.5% | ~3% | $(Verdict ($rec.CpuPctAll -le 1.5)) |
| Recording CPU, projected on a 4-core laptop (one-core % / 4) | $([math]::Round($rec.CpuPctCore / 4, 2))% | <= 1.5% | ~3% | $(Verdict ($rec.CpuPctCore / 4 -le 1.5)) |
| Busiest daemon threads while recording (injected, % of one core) | $(if ($R.recording_injected.top_threads) { $R.recording_injected.top_threads -join ', ' } else { 'n/a' }) | - | - | - |
$(if ($R.recording_mic -and $null -ne $R.recording_mic.main_thread_pct) { "| Main thread while recording (real mic, % of one core; message loop + pill) | $($R.recording_mic.main_thread_pct)% (projected on 4 cores: $([math]::Round($R.recording_mic.main_thread_pct / 4, 2))%) | <= 6% (<= 1.5% on 4 cores) | - | $(Verdict ($R.recording_mic.main_thread_pct -le 6)) |" })
$(if ($R.recording_mic -and $R.recording_mic.top_threads) { "| Busiest daemon threads while recording (real mic, % of one core) | $($R.recording_mic.top_threads -join ', ') | - | - | - |" })
| Daemon working set, idle (mean / peak) | $($R.idle.WsMeanMB) / $($R.idle.WsPeakMB) MB | <= 25 MB | ~110 MB | $(Verdict ($R.idle.WsPeakMB -le 25)) |
| Daemon private bytes, idle (mean / peak) | $($R.idle.PrivMeanMB) / $($R.idle.PrivPeakMB) MB | <= 15 MB | n/a | $(Verdict ($R.idle.PrivPeakMB -le 15)) |
| Daemon private working set, idle (Task Manager "Memory") | $(Fmt $R.idle.private_ws_mb ' MB') | n/a | ~110 MB | - |
| Daemon working set during $Dictations dictations (peak) | $($R.dictations.WsPeakMB) MB | <= 25 MB | ~110 MB | $(Verdict ($R.dictations.WsPeakMB -le 25)) |
| Daemon working set while recording, app bubble on (peak / private WS) | $($R.recording_injected.WsPeakMB) / $(Fmt $R.recording_injected.private_ws_mb ' MB') | <= 25 MB | ~110 MB | $(Verdict ($R.recording_injected.WsPeakMB -le 25)) |
$(if ($R.recording_mic) { "| Daemon working set while recording, real mic, app bubble off (peak / private WS) | $($R.recording_mic.WsPeakMB) / $(Fmt $R.recording_mic.private_ws_mb ' MB') | <= 25 MB | ~110 MB | $(Verdict ($R.recording_mic.WsPeakMB -le 25)) |" })
| Settings window working set (mean / peak) | $(if ($R.settings) { "$($R.settings.WsMeanMB) / $($R.settings.WsPeakMB) MB" } else { 'n/a' }) | <= 80 MB, only while open | n/a | $(if ($R.settings) { Verdict ($R.settings.WsPeakMB -le 80) } else { '-' }) |
| Settings window CPU while idle | $(if ($R.settings) { "$($R.settings.CpuPctAll)%" } else { 'n/a' }) | ~0 | n/a | - |
| Settings window private bytes (commit, only while open; mean / peak / private WS) | $(if ($R.settings) { "$($R.settings.PrivMeanMB) / $($R.settings.PrivPeakMB) / $(Fmt $R.settings.private_ws_mb ' MB')" } else { 'n/a' }) | <= 80 MB as working set | n/a | $(if ($R.settings) { 'documented deviation: GL driver commit, see the Settings private bytes note' } else { '-' }) |
| Executable size | $exeSizeText | <= $(Format-Bytes $SizeBudgetBytes) bytes | n/a | $(Verdict $sizeOk) |
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
$(if (Test-Path (Join-Path $outDir 'size.json')) { $sz = Get-Content -Raw (Join-Path $outDir 'size.json') | ConvertFrom-Json
"## Binary size

Release executable, measured by rebuilding the same commit with parts of the cleanup LLM code stubbed out (``bench/out/size.json``):

| Build | Bytes | MB (10^6 bytes) | Delta |
|---|---|---|---|
" + (($sz.builds | ForEach-Object { "| $($_.name) | $($_.bytes) | $(Format-ExeMB $_.bytes) | $(if ($_.delta) { '+' + [math]::Round($_.delta / 1000) + ' kB (10^3 bytes)' } else { '-' }) |" }) -join "`n") + "

$($sz.note)
" })
$(if (Test-Path (Join-Path $outDir 'extra.md')) { (Get-Content -Raw -Encoding UTF8 (Join-Path $outDir 'extra.md')).TrimEnd() + "`n" })
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

$liveMd

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
- Per-thread CPU uses ``ProcessThread.TotalProcessorTime`` and the thread names Rust sets (``SetThreadDescription``).

## Notes

- **opengl32.dll is delay-loaded** (``build.rs``). Only the Settings window draws with OpenGL, but a load-time import put
  ``opengl32.dll`` into the daemon too, and overlay injectors that hook GL processes (RivaTuner/MSI Afterburner, which runs on
  this machine) then treated the daemon as a 3D app: its private commit jumped from ~2 MB to ~58 MB at the first STA
  ``CoInitializeEx``. With the delay-load import the daemon never loads the DLL; Settings loads it on its first WGL call.
  The Settings process's large private-bytes figure is that same injector inside a real GL process; its working set is small.
- **Settings private bytes**: the PLAN section 1 Settings figure (80 MB) is read as working set, not as ``PrivateMemorySize64``.
  The Settings process commits ~265 MB, almost all of it the OpenGL driver's command and shader pools reserved at context
  creation (plus any overlay hook such as ``RTSSHooks64.dll``); it is mostly untouched, so it never enters the working set.
  ``ui/app.rs::native_options`` already asks for no depth, stencil or MSAA (eframe's defaults), and vsync or
  ``hardware_acceleration`` do not change it; software GL is 1.1, which ``egui_glow`` rejects. Only a non-GL renderer would
  avoid it. Documented deviation; the daemon never loads ``opengl32.dll`` and does not pay it.
- **Recording CPU is the pill, kept to the lane**: once the entrance has settled the pill caches its static layer (shadow,
  body, logo) and each frame redraws only the 52x24 waveform lane, uploads only that rectangle and presents it with
  ``UpdateLayeredWindowIndirect`` (``prcDirty``). A frame whose bars all moved by <= 1/255 px is skipped, which is every
  frame while the input is silent. The thinking glow keeps its 60 fps but also caches the chrome. WASAPI capture and the
  VAD/level meter stay well under 1% of one core. The row "Main thread while recording" is the figure the pill is judged by
  (<= 6% of one core is <= 1.5% on a 4-core laptop).
- **Working set and the app bubble**: the active-app icon of a classic Win32 executable comes from ``PrivateExtractIconsW``
  (user32 only), then ``SHGetFileInfoW`` (in its own COM apartment). ``IShellItemImageFactory`` is the last resort, used
  only for packaged (UWP / MSIX) apps or when both lighter paths return nothing, because it loads ~35 shell DLLs (XAML,
  StateRepository, thumbcache, ...) into the daemon for the rest of its life (~+17 MB working set of shared image pages,
  ~2-4 MB private). Since commits d53ba4d and 7fbcd92 those shell DLLs stay out of the daemon for Win32 apps. The
  bubble-on working set on an unlocked desktop with a real foreground app has not been re-measured since the change and is
  still to be done: the "app bubble on" rows of this report use whichever window was in front of the benchmark, and the
  earlier unlocked-desktop section predates it.
$clipNote
"@
$kept = @(Write-ReportKeepingBlocks $OutFile $md)
Write-Host ("`nwrote {0} and bench\out\results.json{1}" -f $OutFile, $(if ($kept.Count) { " (kept hand-written blocks: $($kept -join ', '))" } else { '' }))
if (-not (Test-ExeSize $Exe)) { exit 1 }
