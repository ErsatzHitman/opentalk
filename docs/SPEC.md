# OpenTalk – implementation spec (derived from OpenWhispr analysis, 2026-10-09)

Toolchain: rustup stable-x86_64-pc-windows-gnu + WinLibs mingw64 at C:\Users\aksha\tools\mingw64\bin (dlltool).
Build: `PATH=$HOME/.cargo/bin:/c/Users/aksha/tools/mingw64/bin:$PATH cargo build --release`

> **Approved by the user (2026-10-10):** the implemented recording pill and the round app-icon bubble beside it look right. Treat the current rendering in `src/daemon/pill*` as the reference: fix bugs or clear SPEC mismatches only, and do not restyle, resize or re-time it.
>
> **Liquid glass (user decision 2026-10-10, "Glass pill + bubble too"):** the one thing that changes is the **surface material** of the pill, the cancel bud, the app-icon bubble, the tooltip and the error card, which becomes liquid glass (section "Pill surface material" below). Everything else stays exactly as approved: window size, capsule/circle geometry, positions, states, every animation and timing, the waveform bars (size, colour, behaviour), the logo, and the bubble's size and placement.

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


## Pill surface material (liquid glass)
Implements the `liquid-glass` skill for a layered window (no DWM backdrop is available): `src/daemon/pill/glass.rs` (pure) and `capture.rs` (Win32).
- Surfaces: pill capsule/circle, the fused cancel bud, the app-icon bubble (a glass circle, same recipe, same height as the pill), the tooltip (menu tier) and the error card (modal tier). The logo, waveform bars, cancel glyph, divider, hover gradient and text are drawn on top, unchanged.
- Backdrop: the screen rect under the window plus a refraction margin (`0.12 * monitor size`, capped at 96 logical px, clipped to the monitor) is copied with `BitBlt` from the screen DC, box-averaged into 3 px cells, frosted (3-pass box blur ~ gaussian sigma 10 logical px, `saturate(125%)`) and sampled bilinearly (edges clamp).
- Refraction: the skill's field verbatim (`GLASS_PRESET`: edge 0.015 / rim 0.028 / base 0.05, distances 0.5 / 1.7 / 0.2, corner boost 0.06, ripple 0.26, **warp off**), signed rounded-rect `distPx`, texcoord-space normal, `pageW x pageH` = the pill's monitor in logical px. Each glass pixel samples the frosted backdrop at its displaced position. The displacement map of an exact rounded box is baked once per (size, radius, monitor size, scale, sub-pixel phase) and reused.
- Material on top (tokens of `liquid-glass.css`): frost layer (the modal tier, stronger for the 36 px pill: white 56 % in light mode, near-black 69 % in dark mode; the pill follows the app theme), tint gradient white -> 70 % grey at 6 % (11 % while hovered), three inset rim highlights (top 1 px at 0.55 / 0.50, hairline 0.12 / 0.10, bottom 0.06), 1 px border (`rgba(30,33,45,.12)` / `rgba(255,255,255,.08)`; x0.6 in the processing / thinking / unavailable states, as the old `border/60`), drop shadow (the skill's `0 10px 30px` scaled to the 12 px the window leaves around the pill: `0 3px 9px` light, `0 4px 10px` dark; lifted while hovered).
- Legibility (unit-tested): foreground against the glass is >= 4.5:1 over the waveform lane and the logo for pure white, pure black and a busy stripe/noise backdrop, in both themes.
- Cost (PLAN section 1 stays binding): nothing is captured while the window is hidden. While shown, a USER timer on the pill window recaptures every 125 ms when something is happening (recording, thinking, hover, drag, error) and every 500 ms for the resting circle; also on show, on a size / dock / scale change, and (rate-limited to one capture per 100 ms) on a state change. A capture whose cell hash is unchanged recomposes nothing. Steady recording frames still redraw only the waveform lane: the glass lives in the cached base layer, which is rebuilt only when the backdrop picture changes.
- Capture exclusion: the window calls `SetWindowDisplayAffinity(WDA_EXCLUDEFROMCAPTURE)` so the screen copy never contains the pill. **Side effect: the pill (and bubble, tooltip, error card) is invisible to screenshots, screen recordings and screen sharing.** The flag needs Windows 10 2004; where it is refused, or when a capture fails (secure desktop, locked session), nothing is captured and the glass is frosted over a flat neutral backdrop (light `#d9dce3`, dark `#30333a`) - never garbage.
- Goldens: `--render-pill <state> <t_ms> out.png [--dark] --backdrop white|black|gradient|busy-text|path.png` renders over a fixture (`tests/fixtures/pill_backdrops/`, the bottom-right 640x360 of a 1920x1080 screen with a 40 px taskbar, window docked as the daemon would); without `--backdrop` the neutral backdrop is used, so every render is deterministic.
- Superseded tokens: `surface-0/1/3`, `popover`, `border` and `border-hover` no longer paint any fill or border (the glass does); `fg` and `muted-fg` still colour the logo, bars, cancel glyph, text and the Retry button.

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
