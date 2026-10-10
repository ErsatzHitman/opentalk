# Shared helpers for bench/bench.ps1 and tests/e2e.ps1 (dot-source this file).
# Windows PowerShell 5.1 compatible. Everything runs against an isolated OPENTALK_HOME.
Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# OpenRouter key for live runs: -KeyFile, or the OPENTALK_OPENROUTER_KEY_FILE environment variable;
# without a file, Read-OpenRouterKey falls back to OPENTALK_SECRET_OPENROUTER.
$script:KeyFileDefault = $env:OPENTALK_OPENROUTER_KEY_FILE

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

# OpenRouter key: the key file when one is given and exists, else OPENTALK_SECRET_OPENROUTER. Never print the result.
function Read-OpenRouterKey([string]$keyFile) {
    if ($keyFile -and (Test-Path $keyFile)) {
        $k = (Get-Content -Raw $keyFile)
        if ($k -and $k.Trim()) { return $k.Trim() }
    }
    $envKey = [Environment]::GetEnvironmentVariable('OPENTALK_SECRET_OPENROUTER')
    if ($envKey -and $envKey.Trim()) { return $envKey.Trim() }
    return $null
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

# ---- generated report + hand-written blocks ---------------------------------------------------
# A report file may hold hand-written sections that the generator must not drop. They sit between
# lines `<!-- keep:begin NAME -->` and `<!-- keep:end NAME -->` (each marker alone on its line; the
# block includes its own '## ' heading). Write-ReportKeepingBlocks re-emits every block byte for
# byte, right after the generated '## ' section that preceded it in the old file (headings match on
# their text up to the first '(' or the whole line); a block whose anchor is gone goes at the end.
# A generated section with the same heading as a kept block is replaced by that block.

function Get-HeadingKey([string]$line) {
    $t = ($line -replace '^##\s+', '').Trim()
    $i = $t.IndexOf('(')
    if ($i -gt 0) { $t = $t.Substring(0, $i) }
    return $t.Trim()
}

# Lines of $text with their terminators kept (so joining them gives $text back).
function Split-KeepEol([string]$text) {
    $lines = New-Object Collections.Generic.List[string]
    $pos = 0
    while ($pos -lt $text.Length) {
        $nl = $text.IndexOf("`n", $pos)
        if ($nl -lt 0) { $lines.Add($text.Substring($pos)); break }
        $lines.Add($text.Substring($pos, $nl - $pos + 1))
        $pos = $nl + 1
    }
    return ,$lines
}

# Keep blocks of a report: @{ Name; Text (raw, byte-exact); Anchor (heading key of the generated section before it, or $null); Heading (key of its own '## ' line) }.
function Read-KeepBlocks([string]$text) {
    $blocks = New-Object Collections.Generic.List[object]
    $anchor = $null; $cur = $null
    foreach ($line in (Split-KeepEol $text)) {
        $bare = $line.TrimEnd("`r", "`n")
        if ($null -eq $cur) {
            if ($bare.Trim() -match '^<!-- keep:begin (\S+) -->$') {
                $cur = @{ Name = $Matches[1]; Text = New-Object Text.StringBuilder; Anchor = $anchor; Heading = $null; Unterminated = $false }
                [void]$cur.Text.Append($line)
            } elseif ($bare -match '^## ') { $anchor = Get-HeadingKey $bare }
        } else {
            [void]$cur.Text.Append($line)
            if ($null -eq $cur.Heading -and $bare -match '^## ') { $cur.Heading = Get-HeadingKey $bare }
            if ($bare.Trim() -eq ('<!-- keep:end {0} -->' -f $cur.Name)) {
                $cur.Text = $cur.Text.ToString(); $blocks.Add([pscustomobject]$cur); $cur = $null
            }
        }
    }
    if ($null -ne $cur) {   # unterminated: keep the rest of the file rather than lose it
        Write-Warning ("keep block '{0}' has no end marker; keeping the rest of the file verbatim" -f $cur.Name)
        $cur.Text = $cur.Text.ToString(); $cur.Unterminated = $true; $blocks.Add([pscustomobject]$cur)
    }
    return $blocks.ToArray()
}

# Generated report text + the kept blocks of the old report text -> merged text (line ending of the old file).
function Merge-KeepBlocks([string]$generated, [string]$oldText) {
    $eol = if ($oldText -and $oldText.Contains("`r`n")) { "`r`n" } elseif ($oldText -and $oldText.Contains("`n")) { "`n" } else { "`r`n" }
    $gen = $generated -replace "`r?`n", $eol
    $blocks = @(if ($oldText) { Read-KeepBlocks $oldText })
    if ($blocks.Count -eq 0) { return $gen }
    # Split the generated text into [preamble, section...] at its '## ' headings.
    $sections = New-Object Collections.Generic.List[object]
    $cur = @{ Key = $null; Text = New-Object Text.StringBuilder }
    foreach ($line in (Split-KeepEol $gen)) {
        if ($line -match '^## ') {
            $sections.Add([pscustomobject]$cur)
            $cur = @{ Key = (Get-HeadingKey $line.TrimEnd("`r", "`n")); Text = New-Object Text.StringBuilder }
        }
        [void]$cur.Text.Append($line)
    }
    $sections.Add([pscustomobject]$cur)
    $out = New-Object Text.StringBuilder
    $add = {
        param([string]$chunk)
        if ($out.Length -gt 0) {
            if (-not $out.ToString().EndsWith("`n")) { [void]$out.Append($eol) }
            [void]$out.Append($eol)   # one blank line between chunks
        }
        [void]$out.Append($chunk)
    }
    $placed = @{}
    foreach ($s in $sections) {
        $text = $s.Text.ToString()
        $replaced = ($null -ne $s.Key) -and (@($blocks | Where-Object { $_.Heading -eq $s.Key }).Count -gt 0)
        if (-not $replaced -and $text.Trim()) { & $add ($text.TrimEnd("`r", "`n") + $eol) }
        foreach ($b in $blocks) {
            if (-not $placed.ContainsKey($b.Name) -and $b.Anchor -eq $s.Key) {
                $t = $b.Text; if (-not $t.EndsWith("`n")) { $t += $eol }
                & $add $t; $placed[$b.Name] = $true
            }
        }
    }
    foreach ($b in $blocks) {   # anchor no longer exists: at the end, original order
        if (-not $placed.ContainsKey($b.Name)) {
            $t = $b.Text; if (-not $t.EndsWith("`n")) { $t += $eol }
            & $add $t; $placed[$b.Name] = $true
        }
    }
    return $out.ToString()
}

# Write $generated to $path (UTF-8, BOM kept as the old file had it, BOM for a new file), keeping the old file's keep blocks.
# Returns the names of the blocks that were kept.
function Write-ReportKeepingBlocks([string]$path, [string]$generated) {
    $path = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($path)
    $oldText = $null; $bom = $true
    if (Test-Path -LiteralPath $path) {
        $bytes = [IO.File]::ReadAllBytes($path)
        $bom = ($bytes.Length -ge 3 -and $bytes[0] -eq 0xEF -and $bytes[1] -eq 0xBB -and $bytes[2] -eq 0xBF)
        $skip = if ($bom) { 3 } else { 0 }
        try { $oldText = (New-Object Text.UTF8Encoding($false, $true)).GetString($bytes, $skip, $bytes.Length - $skip) }
        catch { throw "$path is not valid UTF-8; refusing to overwrite it" }
    }
    $old = @(if ($oldText) { Read-KeepBlocks $oldText })
    foreach ($b in $old) {
        if ($b.Unterminated) { throw "keep block '$($b.Name)' in $path has no '<!-- keep:end $($b.Name) -->' line; add it (the file was left untouched)" }
    }
    $merged = Merge-KeepBlocks $generated $oldText
    # Invariant: every old block is present, unchanged.
    $new = @(Read-KeepBlocks $merged)
    foreach ($b in $old) {
        $n = @($new | Where-Object { $_.Name -eq $b.Name })
        if ($n.Count -ne 1 -or $n[0].Text -ne $b.Text) { throw "keep block '$($b.Name)' would not survive the rewrite; $path left untouched" }
    }
    $dir = Split-Path -Parent $path
    if ($dir -and -not (Test-Path -LiteralPath $dir)) { New-Item -ItemType Directory -Force -Path $dir | Out-Null }
    $enc = New-Object Text.UTF8Encoding($false)
    $body = $enc.GetBytes($merged)
    $pre = if ($bom) { [byte[]](0xEF, 0xBB, 0xBF) } else { [byte[]]@() }
    [IO.File]::WriteAllBytes($path, [byte[]]($pre + $body))
    return @($old | ForEach-Object { $_.Name })
}

# ---- whole-clipboard snapshot / restore -------------------------------------------------------
# Every native format with its data (text, HTML, images, file lists, raw streams), taken and put back
# on a dedicated STA thread. Restore verifies each format by content hash and never throws.

function Add-ClipHelper {
    if ('OtClip' -as [type]) { return }
    Add-Type -ReferencedAssemblies System.Windows.Forms, System.Drawing -TypeDefinition @'
using System;
using System.Collections.Generic;
using System.Drawing;
using System.Drawing.Imaging;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
using System.Threading;
using System.Windows.Forms;

public class OtClipSnapshot {
    public bool Denied;            // the clipboard could not be opened (locked desktop, or another app holds it)
    public bool Empty;             // it held nothing
    public string Error = "";
    public List<string> Formats = new List<string>();
    public List<object> Values = new List<object>();
    public List<string> Skipped = new List<string>();   // formats whose data could not be read
}

public static class OtClip {
    class StreamBytes { public byte[] Bytes; }

    static string Hex(byte[] b) {
        using (SHA1 h = SHA1.Create()) { return BitConverter.ToString(h.ComputeHash(b)).Replace("-", ""); }
    }

    static byte[] ReadAll(Stream s) {
        MemoryStream m = s as MemoryStream;
        if (m != null) return m.ToArray();
        if (s.CanSeek) s.Position = 0;
        using (MemoryStream c = new MemoryStream()) { s.CopyTo(c); return c.ToArray(); }
    }

    static byte[] Pixels(Image img) {
        using (Bitmap bmp = new Bitmap(img)) {
            Rectangle r = new Rectangle(0, 0, bmp.Width, bmp.Height);
            BitmapData bd = bmp.LockBits(r, ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
            try {
                byte[] buf = new byte[Math.Abs(bd.Stride) * bd.Height];
                Marshal.Copy(bd.Scan0, buf, 0, buf.Length);
                return buf;
            } finally { bmp.UnlockBits(bd); }
        }
    }

    static object Copy(object o) {
        Stream st = o as Stream;
        if (st != null) { StreamBytes sb = new StreamBytes(); sb.Bytes = ReadAll(st); return sb; }
        byte[] b = o as byte[];
        if (b != null) return b.Clone();
        string[] a = o as string[];
        if (a != null) return a.Clone();
        Image img = o as Image;
        if (img != null) return new Bitmap(img);
        return o;
    }

    static string Fingerprint(object o) {
        if (o == null) return "null";
        StreamBytes sb = o as StreamBytes;
        if (sb != null) return "b:" + Hex(sb.Bytes);
        Stream st = o as Stream;
        if (st != null) return "b:" + Hex(ReadAll(st));
        byte[] b = o as byte[];
        if (b != null) return "b:" + Hex(b);
        string s = o as string;
        if (s != null) return "s:" + Hex(Encoding.UTF8.GetBytes(s));
        string[] a = o as string[];
        if (a != null) return "a:" + Hex(Encoding.UTF8.GetBytes(string.Join("\0", a)));
        Image img = o as Image;
        if (img != null) return "i:" + img.Width + "x" + img.Height + ":" + Hex(Pixels(img));
        return "o:" + o.GetType().FullName;
    }

    static void CopyFrom(IDataObject d, OtClipSnapshot s) {
        string[] fmts = d.GetFormats(false);
        if (fmts == null || fmts.Length == 0) { s.Empty = true; return; }
        foreach (string f in fmts) {
            try {
                object o = d.GetData(f, false);
                if (o == null) { s.Skipped.Add(f); continue; }
                s.Values.Add(Copy(o));
                s.Formats.Add(f);
            } catch (Exception) { s.Skipped.Add(f); }
        }
    }

    static DataObject Build(OtClipSnapshot s) {
        DataObject d = new DataObject();
        for (int i = 0; i < s.Formats.Count; i++) {
            object v = s.Values[i];
            StreamBytes sb = v as StreamBytes;
            if (sb != null) v = new MemoryStream(sb.Bytes);
            else if (v is Image) v = new Bitmap((Image)v);
            else if (v is byte[]) v = ((byte[])v).Clone();
            else if (v is string[]) v = ((string[])v).Clone();
            d.SetData(s.Formats[i], false, v);
        }
        return d;
    }

    // "" when every saved format is present in now with the same content; else what differs.
    static string Compare(OtClipSnapshot s, IDataObject now) {
        List<string> bad = new List<string>();
        for (int i = 0; i < s.Formats.Count; i++) {
            try {
                if (Fingerprint(Copy(now.GetData(s.Formats[i], false))) != Fingerprint(s.Values[i])) bad.Add(s.Formats[i]);
            } catch (Exception) { bad.Add(s.Formats[i]); }
        }
        return bad.Count == 0 ? "" : "differs: " + string.Join(", ", bad.ToArray());
    }

    static OtClipSnapshot SaveCore() {
        OtClipSnapshot s = new OtClipSnapshot();
        IDataObject d = null;
        for (int i = 0; i < 5 && d == null; i++) {
            try { d = Clipboard.GetDataObject(); }
            catch (Exception e) { s.Error = e.GetType().Name + ": " + e.Message; Thread.Sleep(50); }
        }
        if (d == null) { s.Denied = true; if (s.Error.Length == 0) s.Error = "no data object"; return s; }
        s.Error = "";
        CopyFrom(d, s);
        return s;
    }

    static string RestoreCore(OtClipSnapshot s, out bool ok) {
        ok = false;
        if (s.Denied) return "clipboard was not readable when the snapshot was taken (" + s.Error + "), so there is nothing to put back";
        try {
            if (s.Empty) {
                Clipboard.Clear();
                IDataObject e = Clipboard.GetDataObject();
                string[] left = e == null ? null : e.GetFormats(false);
                ok = left == null || left.Length == 0;
                return ok ? "cleared (it was empty)" : "could not clear the clipboard";
            }
            Clipboard.SetDataObject(Build(s), true, 10, 100);
            string diff = Compare(s, Clipboard.GetDataObject());
            string note = s.Skipped.Count > 0 ? "not captured: " + string.Join(", ", s.Skipped.ToArray()) : "";
            ok = diff.Length == 0 && note.Length == 0;
            return ok ? "restored " + s.Formats.Count + " format(s)" : (diff + (diff.Length > 0 && note.Length > 0 ? "; " : "") + note);
        } catch (Exception e) {
            return e.GetType().Name + ": " + e.Message;
        }
    }

    public static OtClipSnapshot Save() {
        OtClipSnapshot snap = null;
        Thread t = new Thread(delegate() { snap = SaveCore(); });
        t.SetApartmentState(ApartmentState.STA); t.Start(); t.Join();
        return snap;
    }

    public static bool Restore(OtClipSnapshot s, out string detail) {
        bool ok = false; string d = "";
        Thread t = new Thread(delegate() { d = RestoreCore(s, out ok); });
        t.SetApartmentState(ApartmentState.STA); t.Start(); t.Join();
        detail = d;
        return ok;
    }

    // Round trip through a private DataObject (no system clipboard): "" when every format survives.
    public static string SelfTest() {
        string result = "";
        Thread t = new Thread(delegate() {
            try {
                DataObject src = new DataObject();
                src.SetData("UnicodeText", false, "caf\u00e9 \u4e2d\u6587 text");
                src.SetData("HTML Format", false, "<b>bold</b>");
                src.SetData("OtTest Raw", false, new MemoryStream(new byte[] { 0, 1, 2, 254, 255 }));
                src.SetData("FileDrop", false, new string[] { "C:\\a.txt", "C:\\b.txt" });
                using (Bitmap bmp = new Bitmap(4, 3)) { bmp.SetPixel(1, 1, Color.FromArgb(200, 10, 20, 30)); src.SetData("Bitmap", false, new Bitmap(bmp)); }
                OtClipSnapshot s = new OtClipSnapshot();
                CopyFrom(src, s);
                if (s.Formats.Count != 5) { result = "snapshot has " + s.Formats.Count + " formats, expected 5 (skipped: " + string.Join(",", s.Skipped.ToArray()) + ")"; return; }
                result = Compare(s, Build(s));
                // The comparison must notice a missing format.
                if (Compare(s, new DataObject()).Length == 0) result = "comparison missed a missing format";
            } catch (Exception e) { result = e.GetType().Name + ": " + e.Message; }
        });
        t.SetApartmentState(ApartmentState.STA); t.Start(); t.Join();
        return result;
    }
}
'@
}

# Snapshot of the whole clipboard ($null if the helper itself cannot run).
function Save-Clipboard {
    try { Add-ClipHelper; return [OtClip]::Save() } catch { return $null }
}

# Put a Save-Clipboard snapshot back. Never throws: { restored (bool); detail }.
function Restore-Clipboard($snapshot) {
    if ($null -eq $snapshot) { return [pscustomobject]@{ restored = $false; detail = 'no snapshot (clipboard helper unavailable)' } }
    try {
        $detail = ''
        $ok = [OtClip]::Restore($snapshot, [ref]$detail)
        return [pscustomobject]@{ restored = [bool]$ok; detail = $detail }
    } catch { return [pscustomobject]@{ restored = $false; detail = $_.Exception.Message } }
}
