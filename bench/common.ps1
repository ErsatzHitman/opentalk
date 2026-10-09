# Shared helpers for bench/bench.ps1 and tests/e2e.ps1 (dot-source this file).
# Windows PowerShell 5.1 compatible. Everything runs against an isolated OPENTALK_HOME.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$script:KeyFileDefault = 'C:/Users/aksha/AppData/Local/Temp/claude/D--openwhispr/8fc84a6d-11b7-44ef-a857-0498a3e693e9/scratchpad/openrouter.key'

function New-TempHome([string]$prefix = 'opentalk') {
    $dir = Join-Path ([IO.Path]::GetTempPath()) ("{0}-{1}" -f $prefix, [Guid]::NewGuid().ToString('N').Substring(0, 8))
    New-Item -ItemType Directory -Path $dir | Out-Null
    return $dir
}

# Write config.json (UTF-8 without BOM) from a hashtable.
function Write-Config([string]$home_, [hashtable]$cfg) {
    $json = $cfg | ConvertTo-Json -Depth 8
    [IO.File]::WriteAllText((Join-Path $home_ 'config.json'), $json, (New-Object Text.UTF8Encoding $false))
}

# Run opentalk.exe with arguments and return stdout (GUI-subsystem exe: PowerShell's & does not wait for it).
function Invoke-Ot([string]$exe, [string[]]$arguments, [hashtable]$environment = @{}, [int]$timeoutSec = 120) {
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = ($arguments | ForEach-Object { if ($_ -match '[\s"]') { '"' + ($_ -replace '"', '\"') + '"' } else { $_ } }) -join ' '
    $psi.UseShellExecute = $false
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.CreateNoWindow = $true
    foreach ($k in $environment.Keys) { $psi.EnvironmentVariables[$k] = [string]$environment[$k] }
    $p = [Diagnostics.Process]::Start($psi)
    $errTask = $p.StandardError.ReadToEndAsync()
    $out = $p.StandardOutput.ReadToEnd()
    if (-not $p.WaitForExit($timeoutSec * 1000)) { $p.Kill(); throw "opentalk $($arguments -join ' ') timed out" }
    return [pscustomobject]@{ Out = $out.Trim(); Err = $errTask.Result.Trim(); Code = $p.ExitCode }
}

# Start a long-running opentalk process (daemon or mock) with a custom environment.
function Start-Ot([string]$exe, [string[]]$arguments, [hashtable]$environment = @{}, [switch]$redirectOut) {
    $psi = New-Object Diagnostics.ProcessStartInfo
    $psi.FileName = $exe
    $psi.Arguments = $arguments -join ' '
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    if ($redirectOut) { $psi.RedirectStandardOutput = $true }
    foreach ($k in $environment.Keys) { $psi.EnvironmentVariables[$k] = [string]$environment[$k] }
    return [Diagnostics.Process]::Start($psi)
}

# Local OpenAI-shaped STT mock (opentalk --bench mock-stt). Returns @{ Proc; Url }.
function Start-Mock([string]$exe, [int]$delayMs = 0) {
    $p = Start-Ot $exe @('--bench', 'mock-stt', '--port', '0', '--delay-ms', $delayMs) @{} -redirectOut
    $line = $p.StandardOutput.ReadLine()
    if ($line -notmatch 'listening (http://\S+)') { try { $p.Kill() } catch {}; throw "mock STT did not start: '$line'" }
    return [pscustomobject]@{ Proc = $p; Url = $Matches[1] }
}

# Refuse to run next to another OpenTalk daemon (single-instance lock; we must not touch the user's own).
function Assert-NoDaemon([string]$exe) {
    if ((Invoke-Ot $exe @('--bench', 'wait-ready', '0.3') @{} 10).Code -eq 0) {
        throw 'an OpenTalk daemon is already running; quit it first (the benchmarks and e2e tests need the single-instance lock)'
    }
}

# Start the daemon and wait until its window exists. Returns the process.
function Start-Daemon([string]$exe, [hashtable]$environment) {
    Assert-NoDaemon $exe
    $p = Start-Ot $exe @('--hidden') $environment
    $r = Invoke-Ot $exe @('--bench', 'wait-ready', '15') $environment 30
    if ($r.Code -ne 0) { try { $p.Kill() } catch {}; throw "daemon did not become ready: $($r.Err)" }
    if ($p.HasExited) { throw 'daemon exited right after start' }
    return $p
}

function Stop-Daemon([string]$exe, $proc, [hashtable]$environment) {
    if ($null -eq $proc) { return }
    try { Invoke-Ot $exe @('--bench', 'quit') $environment 15 | Out-Null } catch {}
    if (-not $proc.WaitForExit(5000)) { try { $proc.Kill() } catch {} }
}

function Stop-Quietly($proc) {
    if ($null -ne $proc) { try { if (-not $proc.HasExited) { $proc.Kill() } } catch {} }
}

# Parse history.jsonl (oldest first).
function Read-History([string]$home_) {
    $f = Join-Path $home_ 'history.jsonl'
    if (-not (Test-Path $f)) { return ,@() }
    $rows = @(Get-Content -Raw -Encoding UTF8 $f -ErrorAction SilentlyContinue |
        ForEach-Object { $_ -split "`n" } | Where-Object { $_.Trim() } | ForEach-Object { try { $_ | ConvertFrom-Json } catch {} })
    return ,$rows
}

function Wait-History([string]$home_, [int]$count, [int]$timeoutSec) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $timeoutSec) {
        $h = Read-History $home_
        if ($h.Count -ge $count) { return ,$h }
        Start-Sleep -Milliseconds 100
    }
    return ,@(Read-History $home_)
}

function Read-OpenRouterKey([string]$keyFile) {
    if (-not (Test-Path $keyFile)) { return $null }
    return (Get-Content -Raw $keyFile).Trim()
}

# WAV duration in ms (PCM16 only; assumes a canonical header).
function Get-WavMs([string]$path) {
    $b = [IO.File]::ReadAllBytes($path)
    $rate = [BitConverter]::ToInt32($b, 24)
    $ch = [BitConverter]::ToInt16($b, 22)
    $len = $b.Length - 44
    return [int](($len / (2 * $ch)) * 1000 / $rate)
}

# ---- process sampling ------------------------------------------------------------------

# Sample a process for $seconds. CPU% is relative to ALL cores (task-manager style) and to one core.
function Measure-Process($proc, [double]$seconds, [int]$intervalMs = 250) {
    $cores = [Environment]::ProcessorCount
    $p = Get-Process -Id $proc.Id
    $p.Refresh()
    $cpu0 = $p.TotalProcessorTime.TotalSeconds
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $ws = New-Object Collections.Generic.List[double]
    $pb = New-Object Collections.Generic.List[double]
    $handles = 0; $threads = 0
    while ($sw.Elapsed.TotalSeconds -lt $seconds) {
        Start-Sleep -Milliseconds $intervalMs
        $p.Refresh()
        $ws.Add($p.WorkingSet64 / 1MB); $pb.Add($p.PrivateMemorySize64 / 1MB)
        $handles = $p.HandleCount; $threads = $p.Threads.Count
    }
    $p.Refresh()
    $wall = $sw.Elapsed.TotalSeconds
    $cpu = $p.TotalProcessorTime.TotalSeconds - $cpu0
    return [pscustomobject]@{
        Seconds      = [math]::Round($wall, 1)
        CpuSeconds   = [math]::Round($cpu, 3)
        CpuPctAll    = [math]::Round($cpu / ($wall * $cores) * 100, 3)
        CpuPctCore   = [math]::Round($cpu / $wall * 100, 3)
        WsMeanMB     = [math]::Round(($ws | Measure-Object -Average).Average, 1)
        WsPeakMB     = [math]::Round(($ws | Measure-Object -Maximum).Maximum, 1)
        PrivMeanMB   = [math]::Round(($pb | Measure-Object -Average).Average, 1)
        PrivPeakMB   = [math]::Round(($pb | Measure-Object -Maximum).Maximum, 1)
        Handles      = $handles
        Threads      = $threads
        Cores        = $cores
    }
}

# Per-thread CPU time (ms) of a process, keyed by thread id, with the thread descriptions Rust sets.
function Get-ThreadTimes([int]$procId) {
    if (-not ('OtThreads' -as [type])) {
        Add-Type -TypeDefinition @'
using System; using System.Runtime.InteropServices;
public static class OtThreads {
    [DllImport("kernel32.dll")] static extern IntPtr OpenThread(uint access, bool inherit, uint id);
    [DllImport("kernel32.dll")] static extern int GetThreadDescription(IntPtr h, out IntPtr d);
    [DllImport("kernel32.dll")] static extern bool CloseHandle(IntPtr h);
    [DllImport("kernel32.dll")] static extern IntPtr LocalFree(IntPtr h);
    public static string Name(uint id) {
        IntPtr h = OpenThread(0x0800, false, id); if (h == IntPtr.Zero) return "";
        IntPtr d; string s = "";
        if (GetThreadDescription(h, out d) >= 0 && d != IntPtr.Zero) { s = Marshal.PtrToStringUni(d); LocalFree(d); }
        CloseHandle(h); return s;
    }
}
'@
    }
    $out = @{}
    foreach ($t in (Get-Process -Id $procId).Threads) {
        try { $out[$t.Id] = [pscustomobject]@{ Name = [OtThreads]::Name([uint32]$t.Id); Ms = $t.TotalProcessorTime.TotalMilliseconds } } catch {}
    }
    return $out
}

# Busiest threads between two Get-ThreadTimes snapshots: "name pct%" (% of one core).
function Get-TopThreads($before, $after, [double]$seconds, [int]$top = 3) {
    $rows = foreach ($id in $after.Keys) {
        $b = if ($before.ContainsKey($id)) { $before[$id].Ms } else { 0 }
        $n = if ($after[$id].Name) { $after[$id].Name } else { "tid$id" }
        [pscustomobject]@{ Name = $n; Pct = [math]::Round(($after[$id].Ms - $b) / ($seconds * 1000) * 100, 2) }
    }
    return @($rows | Sort-Object Pct -Descending | Select-Object -First $top | ForEach-Object { "$($_.Name) $($_.Pct)%" })
}

# ---- desktop helpers (Notepad e2e) -------------------------------------------------------

function Add-Win32 {
    if ('OtWin32' -as [type]) { return }
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class OtWin32 {
    [DllImport("user32.dll")] public static extern bool SetForegroundWindow(IntPtr h);
    [DllImport("user32.dll")] public static extern IntPtr GetForegroundWindow();
    [DllImport("user32.dll")] public static extern bool ShowWindow(IntPtr h, int cmd);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindow(string cls, string title);
    public static IntPtr FindByTitle(string t) { return FindWindow(null, t); }
    public static IntPtr FindByClass(string c) { return FindWindow(c, null); }
    [DllImport("user32.dll")] public static extern void keybd_event(byte vk, byte scan, uint flags, UIntPtr extra);
    [DllImport("user32.dll")] public static extern void SwitchToThisWindow(IntPtr h, bool alt);
    [DllImport("user32.dll")] public static extern uint GetWindowThreadProcessId(IntPtr h, out uint pid);
    [DllImport("user32.dll")] public static extern bool AttachThreadInput(uint a, uint b, bool attach);
    [DllImport("kernel32.dll")] public static extern uint GetCurrentThreadId();
    [DllImport("user32.dll")] public static extern bool BringWindowToTop(IntPtr h);
    public static bool Force(IntPtr h) {
        IntPtr fg = GetForegroundWindow();
        uint pid;
        uint fgThread = fg == IntPtr.Zero ? 0 : GetWindowThreadProcessId(fg, out pid);
        uint me = GetCurrentThreadId();
        bool attached = fgThread != 0 && fgThread != me && AttachThreadInput(me, fgThread, true);
        BringWindowToTop(h);
        bool ok = SetForegroundWindow(h);
        if (attached) AttachThreadInput(me, fgThread, false);
        if (!ok) SwitchToThisWindow(h, true);
        return GetForegroundWindow() == h;
    }
    [DllImport("user32.dll")] public static extern bool SetCursorPos(int x, int y);
    [DllImport("user32.dll")] public static extern bool GetCursorPos(out POINT p);
    [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
    [DllImport("user32.dll")] public static extern void mouse_event(uint flags, uint dx, uint dy, uint data, UIntPtr extra);
    [StructLayout(LayoutKind.Sequential)] public struct POINT { public int X, Y; }
    [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
    // Last resort: a real click on the window's centre activates it even when SetForegroundWindow is refused.
    public static bool ClickCenter(IntPtr h) {
        RECT r; if (!GetWindowRect(h, out r)) return false;
        POINT old; GetCursorPos(out old);
        SetCursorPos((r.L + r.R) / 2, (r.T + r.B) / 2);
        mouse_event(0x0002, 0, 0, 0, UIntPtr.Zero); mouse_event(0x0004, 0, 0, 0, UIntPtr.Zero);
        System.Threading.Thread.Sleep(150);
        SetCursorPos(old.X, old.Y);
        return GetForegroundWindow() == h;
    }
    [DllImport("user32.dll")] public static extern bool PostMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr FindWindowEx(IntPtr parent, IntPtr after, string cls, string title);
    [DllImport("user32.dll", CharSet = CharSet.Unicode)] public static extern IntPtr SendMessage(IntPtr h, uint msg, IntPtr w, System.Text.StringBuilder l);
    [DllImport("user32.dll")] public static extern IntPtr SendMessage(IntPtr h, uint msg, IntPtr w, IntPtr l);
    // Text of the first child window (a WinForms / Win32 edit control) via WM_GETTEXT.
    public static string ChildText(IntPtr h) {
        IntPtr c = FindWindowEx(h, IntPtr.Zero, null, null);
        if (c == IntPtr.Zero) return null;
        int n = (int)SendMessage(c, 0x000E, IntPtr.Zero, IntPtr.Zero);   // WM_GETTEXTLENGTH
        var sb = new System.Text.StringBuilder(n + 1);
        SendMessage(c, 0x000D, (IntPtr)(n + 1), sb);                       // WM_GETTEXT
        return sb.ToString();
    }
}
'@
}

# Bring a window to the foreground even though we are a background process (Alt-key trick).
function Set-Foreground([IntPtr]$hwnd) {
    Add-Win32
    [OtWin32]::ShowWindow($hwnd, 9) | Out-Null
    if ([OtWin32]::Force($hwnd)) { return $true }
    if ([OtWin32]::ClickCenter($hwnd)) { return $true }
    [OtWin32]::keybd_event(0x12, 0, 0, [UIntPtr]::Zero)   # Alt down
    [OtWin32]::SetForegroundWindow($hwnd) | Out-Null
    [OtWin32]::keybd_event(0x12, 0, 2, [UIntPtr]::Zero)   # Alt up
    for ($i = 0; $i -lt 20; $i++) {
        if ([OtWin32]::GetForegroundWindow() -eq $hwnd) { return $true }
        Start-Sleep -Milliseconds 50
    }
    return ([OtWin32]::GetForegroundWindow() -eq $hwnd)
}

function Get-UiaText([IntPtr]$hwnd) {
    Add-Type -AssemblyName UIAutomationClient, UIAutomationTypes
    $root = [Windows.Automation.AutomationElement]::FromHandle($hwnd)
    foreach ($type in @([Windows.Automation.ControlType]::Document, [Windows.Automation.ControlType]::Edit)) {
        $cond = New-Object Windows.Automation.PropertyCondition([Windows.Automation.AutomationElement]::ControlTypeProperty, $type)
        $el = $root.FindFirst([Windows.Automation.TreeScope]::Descendants, $cond)
        if ($null -eq $el) { continue }
        $pat = $null
        if ($el.TryGetCurrentPattern([Windows.Automation.TextPattern]::Pattern, [ref]$pat)) { return $pat.DocumentRange.GetText(-1) }
        if ($el.TryGetCurrentPattern([Windows.Automation.ValuePattern]::Pattern, [ref]$pat)) { return $pat.Current.Value }
    }
    # WinForms edits often expose no UIA text patterns: read the child edit control directly.
    Add-Win32
    return [OtWin32]::ChildText($hwnd)
}
