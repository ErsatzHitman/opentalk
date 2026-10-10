# Liquid glass in the Settings UI

User request: "use /liquid-glass for the entire UI". Source of truth: the `liquid-glass` skill
(`~/.claude/skills/liquid-glass`: `SKILL.md`, `assets/liquid-glass.css`, `references/recipes.md`,
`references/refraction.md`). The skill targets CSS; this is a native port to egui 0.36 (glow) in
`src/ui`. The design is ported, not the web mechanics. The recording pill and app bubble
(`src/daemon/pill*`) are a separate task and are not touched.

## How the port maps to the skill

| Skill | OpenTalk |
|---|---|
| `liquid-glass.css` tokens, both themes | `glass.rs`: `LIGHT` / `DARK` (`Tokens`), valued exactly as the CSS; test `tokens_match_the_stylesheet_in_both_themes` |
| `.liquid-glass-backdrop` (page bg + 5 radial glows) | `glass::backdrop_shapes` + `mesh_color(p, theme, window)`: the glows composited with CSS `over` semantics, rem = 16 pt, positions relative to the window, `transparent 60%` falloff; drawn as a 24 x 16 vertex-coloured grid |
| the native `backdrop-filter` | Windows 11 acrylic: `DWMWA_SYSTEMBACKDROP_TYPE = DWMSBT_TRANSIENTWINDOW`, `DWMWA_USE_IMMERSIVE_DARK_MODE` per theme, `DwmExtendFrameIntoClientArea(-1)`, on a transparent eframe viewport (`glass::sys`) |
| `.liquid-glass` | `glass::panel` (tint gradient, 1 px border, three inset rim highlights, outer shadow, refraction band) |
| `.liquid-glass-chip` | `glass::chip` (tint + border + top rim; no shadow, no band) |
| `.liquid-glass-menu` | `glass::menu` (55 % `--glass-menu-surface` wash, border, two shadows, top rim) |
| `.liquid-glass--modal` + `.liquid-glass-scrim` | `glass::modal` + `scrim_color` |
| colored glass (recipes.md) | `glass::colored` (tint override, rim + border kept) |
| `liquid-glass.directive.ts` (SVG displacement) | `glass::displacement` (`GLASS_PRESET`, `dist_px`, edge / rim falloff, corner boost, ripple, `warp = false`, page = window size) evaluated analytically and drawn as a rim band |
| `data-theme` | `Palette::dark`; Theme setting System / Light / Dark keeps working, System follows `AppsUseLightTheme` |

### Backdrop (rule 1), two layers

1. **Windows acrylic.** Applied once the HWND exists (`App::new`). `glass::sys::enable_acrylic`
   needs build >= 22621 (read with `RtlGetVersion`, which the manifest shim cannot fake) and an
   accepted `DwmSetWindowAttribute`. It also needs Windows' "Transparency effects" on
   (`EnableTransparency`), because with it off Windows paints acrylic as a flat colour.
2. **Ambient mesh** painted by us on top: `--glass-page-bg` at 0.80 (light) / 0.82 (dark) alpha, then
   the five glows. The page colour alpha is what keeps text legible over any wallpaper.

**Fallback.** When acrylic is unsupported or refused, when Transparency effects is off, or when the
user turns on **Settings > Preferences > Appearance > Reduce transparency**
(`Config::reduce_transparency`, serde default `false`), the page colour is painted opaque
(`glass::set_opaque(true)`). The window is never left see-through without a backdrop; opaque is
also the default until a backdrop is confirmed.

### Refraction

Because the backdrop behind every panel is our own analytic mesh, a panel can bend it exactly. The
field of `refraction.md` is ported as is (constants, signed rounded-rect distance, edge / rim
falloff, corner boost, ripple; `pageW/pageH` are the window size). A band about 8 pt wide, a strip
following the rounded rect (six rows, roughly 100-150 samples), is redrawn with the backdrop colour
`mesh_color(p + displacement(p))` and faded into the untouched interior (warp is off, so the interior
is bias-free: test `the_field_decays_away_from_the_rim_and_vanishes_in_the_middle`). The band geometry
(positions, displacements, weights) is cached per `(rect size, radius, window size, theme)`;
colours are looked up per draw because they depend on where the panel sits.

Two deliberate differences from CSS, both because egui cannot sample or blur what is behind it:

* **Outer shadows are drawn outside the surface only** (a ring mesh with the blurred alpha). CSS clips
  a `box-shadow` out of the border box; painting one under a translucent panel would grey it.
* **Menus and modals sit on a solid stand-in for the backdrop blur.** `backdrop-filter: blur(22px)`
  turns whatever is behind a menu into colour; we paint the opaque analytic backdrop (and, for a
  modal, the scrim over it) under the wash, so text behind a popup never shows through.

## Tier usage

| Tier | Painter | Used for |
|---|---|---|
| panel | `glass::panel` / `w::panel` | glass top bar, sidebar, every list / settings panel on the page |
| chip | `glass::chip` | segmented control and pill tabs, tags and badges, key caps, outline / flat buttons, text inputs on the bare page, **only when not inside glass** |
| menu | `glass::menu` / `w::menu_surface` | dropdowns and select lists, the Support menu, the History row menu, toasts |
| modal | `glass::modal` / `w::glass_modal` | the Settings modal (896 x 85vh), the Ctrl+K palette, confirm dialogs, the snippet edit dialog; radius `--glass-radius-lg` (20) over `--glass-scrim` |
| colored | `glass::colored` | primary and destructive buttons, the toggle's on state |
| colored, low strength | `glass::tinted_shapes` / `w::notice` | warning banner, budget warning, Test / Run results (status tint over a glass panel; inside the modal only a tone wash) |
| wash | `Palette::wash` / `primary_wash` | hover and selection on anything inside glass |

### What each page uses

| Page | Surfaces |
|---|---|
| top bar, sidebar | `glass::panel`; search pill, nav rows, window buttons are washes |
| History | one `w::panel` per day (rows separated by `w::divider`; hover, failed and discarded rows are washes with the panel's edge rounding); search box is a chip-tier input (`input_chrome` outside glass); the "history is off" banner is a `w::notice`; the empty and no-match states are panels; row menu is `w::menu_surface`; delete / clear are `w::confirm_dialog` (modal tier) |
| Dictionary | pill tabs (chip tier); add field is a chip-tier input; the word list, the bulk-import box (a `w::text_area` inside a panel), the empty hero and the snippet create / list panels are `w::panel`; the budget warning is a `w::notice`; the snippet edit dialog is `w::glass_modal`; snippet trigger tags are washes |
| Settings modal | `w::glass_modal`; sidebar rows, the close button and the version footer are washes / hairlines |
| Preferences, Privacy & Data, System | `w::panel` groups (a faint grouping wash inside the modal) of `w::row`s; toggles are coloured glass when on, a wash when off; selects are hairline inputs with `w::menu_surface` lists; segmented control is a wash |
| Hotkeys | the capture box is `input_chrome` (hairline, wash, primary line and ring while capturing) holding key caps (`w::paint_keycap`); the activation rows are washes in one panel (selected: primary wash); Esc is a key cap |
| Speech-to-Text | pill tabs, API key field (hairline input), the model list is one panel of wash rows with `w::paint_chip` tags (Recommended, Streaming, Active); Test result is a `w::notice` |
| Cleanup | segmented control and toggles in panels, provider tabs, key and model fields, Prompt Studio (`w::text_area` in a panel), result `w::notice` |
| hotkey capture (`hotkey_capture.rs`) | no surface: it is the keyboard hook; the box it feeds is in Hotkeys |

Stock egui widgets (tooltips, any stray `ComboBox` or `Window`) follow through `theme::apply`:
transparent panel / extreme fills, menu-surface window fill, glass border, no egui shadows.

## The four rules, as implemented

1. **A backdrop first.** `App::ui` paints `backdrop_shapes` before anything else. Verify it with all
   glass removed: it is uneven (violet / pink / cyan glows, indigo corners).
2. **The glass is colourless.** One tint (white to 70 % grey, 6 %) in both themes; the colour arrives
   from the mesh. Brand colour appears only through `glass::colored` (tint override; rim, border and
   shadow stay). White labels on the primary button need more cover than the recipe's 0.55 / 0.42, so
   the gradient runs 0.90 -> 0.84 alpha (0.97 -> 0.92 on hover); see "Deviations".
3. **Children paint no surface.** `w::panel`, `w::menu_surface` and `w::glass_modal` mark their
   content as inside glass (`w::in_glass`). Inside, buttons, segmented controls, tabs and chips draw
   a soft wash with no border and no rim; selection is a primary wash; rows are separated by spacing,
   type weight and a faint inset divider (the glass border at 60 %). Text inputs and selects have no
   box: a 1 px bottom hairline, a wash on hover, and on focus the line turns primary with a low-alpha
   primary ring. A `w::panel` (or `w::notice`) inside the Settings modal degrades to a faint grouping
   wash, so there is never glass in glass. On the bare page an input has nothing to sit on, so
   `input_chrome` draws it as a chip-tier box instead (the History search and the Dictionary add field
   were invisible as bare washes). The same input code serves both cases.
4. **Refraction is progressive enhancement.** The rim band is plain vertex colour (no engine gate
   needed); the CSS layer alone still gives a complete frosted panel.

## Performance

No continuous repaint: nothing here calls `request_repaint`; hover and focus use egui's
`animate_bool_with_time`, which repaints only while a transition runs. Per frame a surface costs a
few hundred vertex colours (cached geometry, no allocation of textures). No new crates; the only
`Cargo.toml` surface used is the existing `windows` features. The daemon is untouched.

## Checklist (from the skill, with status)

- [x] The backdrop is visible with all glass removed.
- [x] No child of a glass panel sets a background or border (washes only; see `w::in_glass`).
- [x] Both themes valued for every token and both eyeballed (`docs/screenshots/glass/`).
- [x] Text stays legible over the busiest glow: body text sits on the 0.80 / 0.82 page layer; menus
      and modals use the wash tiers over a solid stand-in.
- [x] No acrylic (old Windows, Transparency effects off, Reduce transparency): an opaque page, same
      glass on top.
- [x] Chips and repeated elements use the CSS-only tier.
- [x] Nothing hardcodes a colour where a token exists (washes and rims come from `Tokens`). The
      remaining literals are the toggle thumb and the avatar / window-close hover, which have no token.
- [x] No page paints an opaque surface of its own: `offscreen::pages_inside_glass_paint_no_opaque_surfaces`
      scans the real shapes of History, Dictionary (empty, words, bulk, snippets, edit dialog) and every
      Settings section, in both themes.
- [x] Every page eyeballed in both themes (`docs/screenshots/glass/offscreen/`, 25 scenarios x 2).

## Adding a surface

* A grouped list: `w::panel(ui, &p, |ui| { w::row(..); w::divider(ui, &p); .. })`.
* A dropdown / popover: `Popup::..frame(Frame::NONE).show(|ui| w::menu_surface(ui, &p, |ui| ..))`.
* A dialog: `w::glass_modal(ctx, &p, Id::new("x"), margin, |ui| ..)` (`should_close()` on the result).
* Never paint `Frame::fill` / `rect_filled(p.surfaceN)` behind children of glass; use `p.wash(level)`.

## Deviations and known gaps

* Primary / destructive buttons use higher alpha than the recipe, for label contrast.
* `Palette` keeps the OpenWhispr text colours (`#171717` / `#e8e8e8`); `--glass-page-fg` is present in
  `Tokens` but body text was not changed to it, to keep the existing pages' contrast.
* Notices (`w::notice`, `glass::tinted_shapes`) are the skill's coloured glass at low strength
  (10 % light / 13 % dark tone over the glass tint, border at 30 %), not the 0.55 / 0.42 button
  gradient, so body text stays on glass.
* The stand-in behind a modal or menu is the opaque analytic backdrop, sampled on rings
  (`fill_rings`, one ring per ~128 points) rather than blurred: the glows show through the Settings
  modal, but a modal is a frosted card, not live glass over the desktop. Light-theme modals read
  greyer than the rest because the scrim (`rgba(20,22,30,.35)`) dims everything behind them, as in
  the stylesheet.
* Everything the page paints behind the window (acrylic) is not visible in the headless shots; see
  Verification.
* Pages are fully migrated: no page frames its own surface any more. The theme helpers that used to
  return a surface fill were removed with the last caller.

## Verification

* **Live window** (`docs/screenshots/glass/live-*.png`): the real `opentalk.exe --settings <page>`,
  captured with `CopyFromScreen` over a deliberately busy, colourful, text-covered wallpaper window
  placed behind it, so the acrylic blur is visible. Shell and Settings > Preferences in light and
  dark, plus the dark shell with Reduce transparency on (opaque page). Taken before the page
  migration; the pages below were checked headlessly only and still need a pass on a real desktop
  (acrylic, DPI scaling, Segoe UI and Fluent icons).
* **Headless frame renderer** (`src/ui/offscreen.rs`, `docs/screenshots/glass/offscreen/*.png`): the
  real `App::draw` run through egui, tessellated and rasterised on the CPU. It does not emulate
  acrylic (a synthetic wallpaper is composited under the translucent window). Scenarios, each in
  light and dark: `history`, `history-empty`, `history-disabled` (banner), `history-row-menu`,
  `history-hover-search`, `dictionary-empty`, `dictionary-words`, `dictionary-words-edit`,
  `dictionary-bulk`, `dictionary-snippets`, `dictionary-snippet-edit`, `settings-preferences`,
  `settings-preferences-scrolled`, `settings-hotkeys`, `settings-stt`, `settings-stt-custom`,
  `settings-stt-result`, `settings-stt-error` (the nested notice), `settings-cleanup`, `settings-cleanup-ai`, `settings-privacy`, `settings-system`, `palette`,
  `support-menu`, `confirm-clear`. Regenerate with
  `OPENTALK_SHOT_DIR=docs/screenshots/glass/offscreen cargo test --bin opentalk offscreen_shots`
  (`OPENTALK_SHOT_ONLY=<substring>` renders a subset). Without the variable the test does nothing.
  Under Wine `C:\Windows\Fonts` has no Segoe UI, so `fonts::install_from` falls back to Liberation
  Sans / DejaVu from the Linux host; the icon font has no free stand-in, so icon glyphs appear as
  `?` in the shots (on Windows they are Segoe Fluent Icons). Text metrics differ slightly from Segoe.
  Always-on tests: `offscreen_shell_has_an_uneven_translucent_backdrop`,
  `pages_inside_glass_paint_no_opaque_surfaces` (rule 3 on the real pages) and its negative control.
* **Measured, release build, Settings window idle** (10 s after a 6 s settle, before the page
  migration): working set 13.3 MB (History) / 15.9 MB (Settings modal open), CPU 0 to 0.16 % (one
  15.6 ms scheduler tick in 10 s), no repaint while idle. The migration adds only washes, hairlines
  and a few ring meshes per frame (no textures, no allocation per frame beyond the shape lists), and
  no `request_repaint` calls, so these numbers are expected to hold; re-measure on Windows.
  Executable (release, cross-built): 7,450,624 bytes, under the 8,000,000 budget (the previous
  glass-foundation measurement was 7,324,672; the page migration adds about 126 KB of code).

## Windows notes

* `winit` hides an undecorated window's frame by zeroing the non-client area but leaves `WS_SYSMENU`
  set. With the frame extended into the client area (needed for the acrylic), DWM then draws its own
  caption buttons through our translucent pixels (faint duplicate min / max / close above the bar).
  `glass::sys::hide_caption_buttons` clears `WS_SYSMENU`; the app re-checks it every frame, since
  winit re-applies the style on some state changes. Alt+F4 still closes the window.
* Without `DwmExtendFrameIntoClientArea(-1)` the system backdrop does not show; with winit's
  blur-behind alone the window shows an unblurred, dark blend of whatever is behind it.
