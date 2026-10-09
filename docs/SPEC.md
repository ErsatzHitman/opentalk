# OpenTalk – implementation spec (derived from OpenWhispr analysis, 2026-10-09)

Toolchain: rustup stable-x86_64-pc-windows-gnu + WinLibs mingw64 at C:\Users\aksha\tools\mingw64\bin (dlltool).
Build: `PATH=$HOME/.cargo/bin:/c/Users/aksha/tools/mingw64/bin:$PATH cargo build --release`

## Pill (exact OpenWhispr VoicePill)
- Window 208x120, layered, topmost, no-activate, toolwindow. Placed at work area bottom-right, 4px margin.
  Group anchored 12px from window right and bottom (pill right edge 16px from work-area edge).
- Footprints: idle circle 40x40; recording capsule 98x36. Border 1px inside. rounded-full.
- Capsule internals: pr 6px when compact; logo slot 22x22; divider hidden while recording (margins 3px);
  waveform lane 52x24: 11 bars, 2px wide, 3px gap, min 4 max 22, rounded, color foreground.
- Level: rms of last ~43ms; bar = 4 + min(1,(rms*8)^0.75)*18. Sample every 80ms, shift left (newest right),
  each bar eases 75ms ease-out cubic-bezier(0,0,0.2,1). Silent -> 4px. Reset to 4px on start.
- Logo (24 viewBox at 22px): ring circle c(12,12) r10 stroke 2; bars x=8.75 y10..14, x=12 y8..16, x=15.25 y10..14, stroke 2 round caps.
- States/colors (light | dark):
  fg #171717 | #e8e8e8; muted-fg #525252 | #8f8f8f; border #e5e5e5 | #383b3e; border-hover #d4d4d4 | #4a4d53
  surface-1 #fafafa | #202225; surface-3 #f5f5f5 | #2b2e32; popover #ffffff | #292b30
  idle: bg s1, border border-hover (dark: border/50), icon fg. hover: bg s3, border-hover, gradient tl fg/10 * 0.72, shadow 0 2 8 -2 rgba(0,0,0,.1/.12)
  recording: bg s1, border-hover, fg. processing(warmup): border/60, fg/70, logo pulse 2s (opacity 1->.5->1).
  thinking (transcribing): 40x40, border/60, fg, signal glow on.
  dark only shadow: 0 1px 3px rgba(0,0,0,.2).
- Entrance: 0-260ms circle w/ recording colors; 260ms expand to 98x36 over 300ms cubic-bezier(0.2,0,0,1);
  660ms waveform fades in 200ms. Stop: shrink to 40x40 over 300ms, glow fades in 220ms.
- Signal glow (thinking): colors bright #38bdf8 core #2563eb deep #6366f1 head #eaf6ff. halo op .8 (light .7), comet .95 (light .85).
  ring inset -2 thickness 2.5: conic(from angle) core@25%a 0%, deep@55%a 45%, core 75%, bright 94%, head 100%.
  halo: inset -4, radial(closest-side) core@90% 45%, deep@70% 75%, transparent; blur 5px; breathe 1.9s op .55x..1x scale .96..1.05
  comet: inset -4, conic transparent 0..55%, core@70% 82%, bright 100%; blur 4px. Spin 360deg / 1.6s linear, 0deg=top, clockwise.
- Cancel: 28px circle, gap 8 to right of pill, X 13px stroke 2.5 muted-fg (hover fg); shows on hover while recording/processing; emerges 280ms.
- Tooltip: 10px text above pill (8px gap), popover bg, border, rounded-full, px 6 py 4: hotkey label / "Recording..." / "Processing...".
- No success state. Errors: card 442 wide rounded-2xl surface-0, title 16px + desc 14px muted, auto-dismiss 3/4/5s by length.
- Cues: start C5 523.25 -> E5 659.25; stop D5 587.33 -> A4 440. note .09s gap .025s, attack 15ms to gain .2, exp decay.

## OpenTalk additions
- App bubble: circle left of pill (gap 8), diameter == pill height (animated), same chrome as pill, foreground app icon ~60% of diameter.
- Deterministic filler removal (OpenWhispr only had LLM). Optional LLM cleanup w/ OpenWhispr prompt, 4s timeout, validation.
- Ctrl+Win Start-menu suppression (mask key 0xE8), injected-event filtering, Esc cancel.
- Clipboard: full format snapshot/restore, excluded from clipboard history.

## Settings UI (egui, separate process `--settings`)
- Shell: sidebar 192px (Home/History, Dictionary, Settings), top bar 48px, content card radius 12.
- Settings modal 896w x 85vh, sidebar 208 with groups: App(Preferences, Hotkeys), AI Models(Speech-to-Text, Cleanup), System(Privacy & Data, System)
- Tokens: primary #4577e9 | #3e89ff; background #fff | #191b1d; card #fff | #24272a; surface-window #f7f7f7 | #121416;
  surface-2 #fff|#24272a; surface-raised #f0f0f0|#32363b; border-subtle #e5e5e5|#343539; destructive #dc2626|#ee343b; success #16a34a|#45ba50
- SettingsPanel rounded 8 border/70 bg card/50, rows px16 py12, label 12px medium, desc 12px muted/80.
- Toggle 44x24 track, 16px thumb. Inputs h32 rounded 4. Buttons rounded-full.
