# End-to-end dictation test: real daemon, real hotkey pipeline entry (IPC start / stop), real paste into Notepad.
#
#   powershell -File tests\e2e.ps1 [-Exe path] [-Stt mock|mock-stream|openrouter] [-Llm] [-Model id]
#
# - isolated OPENTALK_HOME, OPENTALK_INJECT_WAV=tests\fixtures\tts_fillers.wav replaces the microphone
# - -Stt mock        local OpenAI-shaped STT server returning the raw filler sentence (deterministic)
# - -Stt mock-stream the same sentence over the Deepgram live protocol from the same local server
#                    (streaming session fed while recording; asserts the dictation was streamed)
# - -Stt openrouter  live OpenRouter, ":free" models only (key from -KeyFile, passed via env var only)
# - -Llm             also enable AI cleanup through an OpenRouter ":free" text model
# Opens Notepad, triggers dictation twice (start/stop) and asserts that Notepad receives the cleaned
# text (fillers removed) and that history holds the entry. Cleans up everything it opened.
# The whole clipboard (every format, or an "empty" flag) is saved first and put back exactly at the end;
# the result JSON carries clipboard_restored (true/false) and clipboard_snapshot. A failed restore never
# fails the run. On a locked desktop clipboard access is denied: the snapshot is then "denied" and
# clipboard_restored is false (nothing could be read, so nothing is claimed).
# The OpenRouter key comes from -KeyFile, else OPENTALK_SECRET_OPENROUTER (never printed).
param(
    [string]$Exe = (Join-Path $PSScriptRoot '..\target\release\opentalk.exe'),
    [ValidateSet('mock', 'mock-stream', 'openrouter')][string]$Stt = 'mock',
    [switch]$Llm,
    [string]$Model = 'nvidia/nemotron-3-nano-omni-30b-a3b-reasoning:free',
    [string]$LlmModel = 'nvidia/nemotron-3.5-lightning:free',
    [string]$KeyFile = '',
    # auto: Notepad when none is running (never touch a Notepad that already holds your text), else a scratch form
    [ValidateSet('auto', 'notepad', 'form')][string]$Target = 'auto',
    # paste: type into a real window via SendInput (needs an unlocked interactive desktop).
    # clipboard: auto_paste off, assert on the clipboard text (works on a locked desktop).
    # auto: paste when the desktop is unlocked, otherwise clipboard.
    [ValidateSet('auto', 'paste', 'clipboard')][string]$Mode = 'auto',
    [int]$TimeoutSec = 60
)
. (Join-Path $PSScriptRoot '..\bench\common.ps1')
if (-not $KeyFile) { $KeyFile = $script:KeyFileDefault }
$Exe = (Resolve-Path $Exe).Path
$wav = (Resolve-Path (Join-Path $PSScriptRoot 'fixtures\tts_fillers.wav')).Path
$wavMs = Get-WavMs $wav

$home_ = New-TempHome 'opentalk-e2e'
$daemon = $null; $mock = $null; $notepad = $null; $hwnd = [IntPtr]::Zero; $targetPid = 0
$envv = @{ OPENTALK_HOME = $home_; OPENTALK_INJECT_WAV = $wav }
$cfg = @{ audio_cues = $false; auto_paste = $true; keep_in_clipboard = $false; history_enabled = $true; show_app_bubble = $true; stt = @{ provider = 'openai' } }
$result = [ordered]@{ stt = $Stt; llm = [bool]$Llm; pass = $false }
# Nothing sent by SendInput reaches an app while the session is locked. The Win10/11 lock screen
# (LockApp) runs on the normal desktop, so OpenInputDesktop alone still succeeds there: also read the
# session's lock flag (WTSSessionInfoEx). OpenInputDesktop fails on the secure desktop (LogonUI, UAC).
# The LogonUI process alone is no proof: it can linger after an unlock.
if (-not ('OtDesk' -as [type])) {
    Add-Type -TypeDefinition @'
using System;
using System.Runtime.InteropServices;
public static class OtDesk {
    [DllImport("user32.dll", SetLastError = true)] static extern IntPtr OpenInputDesktop(uint flags, bool inherit, uint access);
    [DllImport("user32.dll")] static extern bool CloseDesktop(IntPtr desk);
    [DllImport("wtsapi32.dll")] static extern bool WTSQuerySessionInformationW(IntPtr server, uint session, int cls, out IntPtr buf, out uint len);
    [DllImport("wtsapi32.dll")] static extern void WTSFreeMemory(IntPtr p);
    static bool SessionLocked() {
        IntPtr buf; uint len;
        if (!WTSQuerySessionInformationW(IntPtr.Zero, 0xFFFFFFFF, 25 /* WTSSessionInfoEx */, out buf, out len)) return false;
        try { return len >= 32 && Marshal.ReadInt32(buf, 0) == 1 && Marshal.ReadInt32(buf, 16) == 0; /* WTS_SESSIONSTATE_LOCK */ }
        finally { WTSFreeMemory(buf); }
    }
    public static bool InputDesktopAvailable() {
        if (SessionLocked()) return false;
        IntPtr d = OpenInputDesktop(0, false, 0x0100); // DESKTOP_SWITCHDESKTOP
        if (d == IntPtr.Zero) return false;
        CloseDesktop(d);
        return true;
    }
}
'@
}
$locked = -not [OtDesk]::InputDesktopAvailable()
if ($Mode -eq 'auto') { $Mode = if ($locked) { 'clipboard' } else { 'paste' } }
if ($Mode -eq 'paste' -and $locked) {
    Write-Host 'E2E SKIPPED: paste mode needs an unlocked interactive desktop (session locked or OpenInputDesktop failed); use -Mode clipboard'
    Remove-Item -Recurse -Force $home_ -ErrorAction SilentlyContinue
    exit 0
}
$result.mode = $Mode
$result.desktop_locked = $locked
$result.paste_verified = ($Mode -eq 'paste')
if ($Mode -eq 'clipboard') { $cfg.auto_paste = $false; $cfg.keep_in_clipboard = $true }
$clipSnap = Save-Clipboard   # whole clipboard; $null only if the helper cannot run
$result.clipboard_snapshot = if ($null -eq $clipSnap) { 'unavailable' } elseif ($clipSnap.Denied) { 'denied' } elseif ($clipSnap.Empty) { 'empty' } else { "$($clipSnap.Formats.Count) formats" }
$result.clipboard_restored = $false

try {
    if ($Stt -eq 'mock') {
        $mock = Start-Mock $Exe 40
        $envv.OPENTALK_STT_BASE_URL = $mock.Url
        $envv.OPENTALK_SECRET_OPENAI = 'e2e-dummy-key-123456'
    } elseif ($Stt -eq 'mock-stream') {
        $mock = Start-Mock $Exe 40
        $envv.OPENTALK_STT_BASE_URL = $mock.Url
        $envv.OPENTALK_STT_WS_BASE_URL = $mock.Url -replace '^http://', 'ws://'
        $envv.OPENTALK_SECRET_DEEPGRAM = 'e2e-dummy-key-123456'
        $cfg.stt = @{ provider = 'deepgram'; prefer_streaming = $true; models = @{ deepgram = 'nova-3' } }
    } else {
        $key = Read-OpenRouterKey $KeyFile
        if (-not $key) { throw 'no OpenRouter key (set OPENTALK_SECRET_OPENROUTER or pass -KeyFile)' }
        if (-not $Model.EndsWith(':free')) { throw 'live tests may only use ":free" models' }
        $envv.OPENTALK_SECRET_OPENROUTER = $key
        $cfg.stt = @{ provider = 'openrouter'; models = @{ openrouter = $Model }; timeout_ms = 45000 }
    }
    if ($Llm) {
        $key = Read-OpenRouterKey $KeyFile
        if (-not $key) { throw 'no OpenRouter key (set OPENTALK_SECRET_OPENROUTER or pass -KeyFile)' }
        if (-not $LlmModel.EndsWith(':free')) { throw 'live tests may only use ":free" models' }
        $envv.OPENTALK_SECRET_OPENROUTER = $key
        $cfg.ai_cleanup = @{ enabled = $true; provider = 'openrouter'; models = @{ openrouter = $LlmModel }; timeout_ms = 15000 }
    }
    Write-Config $home_ $cfg

    $daemon = Start-Daemon $Exe $envv
    Write-Host "daemon up (pid $($daemon.Id)), wav $wavMs ms, home $home_"

    if ($Mode -eq 'paste') {
        # The paste target: Notepad when nothing else is using it, otherwise a scratch text form.
        Add-Win32
        Add-Type -AssemblyName System.Windows.Forms
        $running = @(Get-Process -Name notepad -ErrorAction SilentlyContinue)
        if ($Target -eq 'notepad' -and $running.Count -gt 0) { throw 'Notepad is already running; refusing to touch it (use -Target form)' }
        $useNotepad = ($Target -eq 'notepad') -or ($Target -eq 'auto' -and $running.Count -eq 0)
        $result.target = if ($useNotepad) { 'notepad' } else { 'form' }
        $sw = [Diagnostics.Stopwatch]::StartNew()
        if ($useNotepad) {
            $notepad = Start-Process notepad -PassThru
            while ($hwnd -eq [IntPtr]::Zero -and $sw.Elapsed.TotalSeconds -lt 15) {
                Start-Sleep -Milliseconds 200
                $hwnd = [OtWin32]::FindByClass('Notepad')
            }
        } else {
            $title = 'OpenTalk E2E target ' + [Guid]::NewGuid().ToString('N').Substring(0, 6)
            $ps = "Add-Type -AssemblyName System.Windows.Forms; `$f = New-Object Windows.Forms.Form; `$f.Text = '$title'; `$f.Width = 700; `$f.Height = 300; `$f.StartPosition = 'CenterScreen'; `$t = New-Object Windows.Forms.TextBox; `$t.Multiline = `$true; `$t.Dock = 'Fill'; `$f.Controls.Add(`$t); [Windows.Forms.Application]::Run(`$f)"
            $notepad = Start-Process powershell -ArgumentList @('-NoProfile', '-STA', '-WindowStyle', 'Hidden', '-Command', ('"' + $ps + '"')) -PassThru
            while ($hwnd -eq [IntPtr]::Zero -and $sw.Elapsed.TotalSeconds -lt 20) {
                Start-Sleep -Milliseconds 200
                $hwnd = [OtWin32]::FindByTitle($title)
            }
        }
        if ($hwnd -eq [IntPtr]::Zero) { throw 'target window not found' }
        if ($useNotepad) { [uint32]$wpid = 0; [OtWin32]::GetWindowThreadProcessId($hwnd, [ref]$wpid) | Out-Null; $targetPid = [int]$wpid }
        if (-not (Set-Foreground $hwnd)) { throw 'could not bring the target to the foreground' }
        Start-Sleep -Milliseconds 400
    }

    # Dictate: start, let the injected WAV play, stop. Explicit start / stop (never a toggle) so a stray
    # extra message cannot flip the parity; the bench hotkey (common.ps1) is one nobody presses.
    $t0 = [Diagnostics.Stopwatch]::StartNew()
    if ($Mode -eq 'paste' -and [OtWin32]::GetForegroundWindow() -ne $hwnd) { throw 'the target lost the foreground before dictation (someone is using the desktop?)' }
    if ((Invoke-Ot $Exe @('--bench', 'start') $envv 15).Code -ne 0) { throw 'start failed' }
    $idleSw = [Diagnostics.Stopwatch]::StartNew(); $phase = ''
    while ($phase -ne 'recording' -and $idleSw.Elapsed.TotalSeconds -lt 3) {
        $pr = Invoke-Ot $Exe @('--bench', 'phase') $envv 15
        $phase = if ($pr.Code -eq 0 -and $pr.Out -match '"phase"\s*:\s*"(\w+)"') { $Matches[1] } else { '' }
        if ($phase -ne 'recording') { Start-Sleep -Milliseconds 50 }
    }
    if ($phase -ne 'recording') { throw "start did not begin a recording (phase '$phase')" }
    if ($Mode -eq 'paste') {
        # The paste goes to the window that had focus at the start: never let it be someone else's.
        $expectExe = if ($result.target -eq 'notepad') { 'notepad.exe' } else { 'powershell.exe' }
        $started = $null; $sw = [Diagnostics.Stopwatch]::StartNew()
        while (-not $started -and $sw.Elapsed.TotalSeconds -lt 5) {
            Start-Sleep -Milliseconds 100
            $started = Get-Content (Join-Path $home_ 'logs\daemon.log') -ErrorAction SilentlyContinue | Where-Object { $_ -match 'recording #\d+ started' } | Select-Object -Last 1
        }
        if ("$started" -notmatch "target=$([regex]::Escape($expectExe))") {
            Invoke-Ot $Exe @('--bench', 'cancel') $envv 15 | Out-Null
            throw "dictation captured the wrong paste target ($started); cancelled"
        }
    }
    Start-Sleep -Milliseconds ($wavMs + 700)
    $pr = Invoke-Ot $Exe @('--bench', 'phase') $envv 15
    if ($pr.Out -notmatch '"phase"\s*:\s*"recording"') { throw "the recording ended before the stop (daemon phase: $($pr.Out))" }
    if ((Invoke-Ot $Exe @('--bench', 'stop') $envv 15).Code -ne 0) { throw 'stop failed' }
    $hist = Wait-History $home_ 1 $TimeoutSec
    $result.dictation_ms = [int]$t0.Elapsed.TotalMilliseconds
    if ($hist.Count -lt 1) { throw "no history entry within $TimeoutSec s" }
    $entry = $hist[$hist.Count - 1]
    $result.history_status = $entry.status
    $result.raw = $entry.raw
    $result.history_text = $entry.text
    $result.latency_ms = $entry.latency_ms
    $result.ai_cleaned = [bool]$entry.ai_cleaned
    if ($entry.status -ne 'ok') { throw "history status '$($entry.status)': $($entry.error)" }

    Start-Sleep -Milliseconds 700   # let SendInput + the target app settle
    if ($Mode -eq 'paste') { $text = Get-UiaText $hwnd }
    else {
        # A locked desktop denies clipboard access to us as well: fall back to the history text.
        try { $text = Get-Clipboard -Raw; $result.clipboard_checked = $true }
        catch { $text = $entry.text; $result.clipboard_checked = $false }
    }
    if ($null -eq $text) { throw 'could not read the dictated text (UI Automation / clipboard)' }
    $text = $text.Trim()
    $result.target_text = $text

    $errs = @()
    if ($text -notmatch 'ship the new release on Friday') { $errs += 'expected sentence missing from the target' }
    if ($text -match '(?i)\b(um|uh)\b') { $errs += 'hesitation filler left in the target' }
    if ($text -match '(?i)\byou know\b') { $errs += '"you know" left in the target' }
    if ($text -match '(?i)\bthe the\b') { $errs += 'stutter "the the" left in the target' }
    if ($text.TrimEnd() -ne ($entry.text.TrimEnd())) { $errs += 'target text differs from the history text' }
    if ($Stt -ne 'openrouter' -and ($entry.raw -notmatch '^Um,')) { $errs += 'history raw text is not the recognizer output' }
    $latLine = Get-Content (Join-Path $home_ 'logs\daemon.log') -ErrorAction SilentlyContinue | Where-Object { $_ -match 'latency seq=' } | Select-Object -Last 1
    $result.latency_line = "$latLine" -replace '^.*latency ', ''
    if ($Stt -eq 'mock-stream' -and ("$latLine" -notmatch 'streamed=1')) { $errs += 'the dictation was not streamed' }
    if ($errs.Count -gt 0) { throw ($errs -join '; ') }
    $result.pass = $true
}
catch {
    $result.error = $_.Exception.Message + ' @ line ' + $_.InvocationInfo.ScriptLineNumber
}
finally {
    # Leave Notepad empty (it restores unsaved tabs), then close everything we opened.
    if ($hwnd -ne [IntPtr]::Zero -and $result.target -eq 'notepad') {
        # Notepad restores unsaved tabs: leave it empty.
        try { if (Set-Foreground $hwnd) { [Windows.Forms.SendKeys]::SendWait('^a{DEL}') } } catch {}
    }
    if ($hwnd -ne [IntPtr]::Zero) {
        [OtWin32]::PostMessage($hwnd, 0x0010, [IntPtr]::Zero, [IntPtr]::Zero) | Out-Null   # WM_CLOSE
        Start-Sleep -Milliseconds 500
    }
    if ($notepad -and -not $notepad.HasExited) { try { $notepad.Kill() } catch {} }
    # Notepad is a packaged app: the process we started may be a stub, so end the one that owned the
    # window (it is ours: -Target notepad/auto only opens one when none was running).
    if ($targetPid -gt 0) {
        for ($i = 0; $i -lt 10 -and (Get-Process -Id $targetPid -ErrorAction SilentlyContinue); $i++) { Start-Sleep -Milliseconds 100 }
        try { Stop-Process -Id $targetPid -Force -ErrorAction SilentlyContinue } catch {}
    }
    Stop-Daemon $Exe $daemon $envv
    if ($mock) { Stop-Quietly $mock.Proc }
    # Put the clipboard back last: the daemon may still be restoring it itself while it shuts down.
    $clip = Restore-Clipboard $clipSnap
    $result.clipboard_restored = [bool]$clip.restored
    $result.clipboard_detail = $clip.detail
    if (-not $clip.restored) { Write-Host "clipboard not restored: $($clip.detail)" }
    Remove-Item -Recurse -Force $home_ -ErrorAction SilentlyContinue
}

$result | ConvertTo-Json -Depth 4
if ($result.pass) { Write-Host 'E2E PASS'; exit 0 } else { Write-Host "E2E FAIL: $($result.error)"; exit 1 }
