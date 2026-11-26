# mine desktop app: Liquid Glass look, dark and light

- Date: 2026-11-26
- Status: approved by the user 2026-11-26 (they approved this design and asked to proceed)
- Builds on: [desktop design](2026-11-23-mine-desktop-design.md), [profiles design](2026-11-24-mine-profiles-design.md)
- Mockups the user approved in direction: https://claude.ai/artifact/Kdy15eThT5NU7pTFQKw59L (dark glass, light glass, launcher sheet). The user chose to ship **both** dark and light glass.

## 1. Intent

The user wants the desktop app to feel like a native macOS Tahoe app in Apple's Liquid Glass language:

- The window is translucent glass over the wallpaper.
- Controls and panels float inside the window as rounded glass layers with a light edge and a soft shadow.
- Actions are capsules, and the one action that matters (Allow) is tinted glass.

The app today is opaque and flat, with one fixed dark palette. The look must work in both dark and light appearance and follow the system setting the way native apps do.

This is a visual pass. It changes no behavior.

## 2. Goals, non-goals, anti-goals

**Goals**

1. A theme layer with two glass palettes, dark and light, that every view draws from. No view uses a literal color.
2. The window background is real macOS blur (GPUI `WindowBackgroundAppearance::Blurred`) under a translucent tint.
3. Panels float as glass layers: the task list, the tab bar, the terminal pane, the launcher sheet, the find box, and the message and offline bar.
   - Each is rounded, with a translucent fill, a light edge, and a soft shadow, inset from the window edges.
4. Capsule shapes for buttons, tabs, the project filter, key hints, and chips. Allow is accent-tinted glass; Deny is clear glass.
5. The appearance follows the system (light ↔ dark) live, without a restart, through `observe_window_appearance`.
   - A config override in `<state>/config.toml` lets the user pin the mode or turn glass off.
6. macOS **Reduce transparency** is respected: when it's on, every surface becomes opaque, with the same layout and palette.
7. Text stays readable on any wallpaper. This is enforced by a contrast test over worst-case backdrops (section 7).
8. The terminal stays a dark, solid pane in both modes and at every glass level (G-R14); it keeps the glass radius, edge, highlight, and shadow. Its ANSI palette is unchanged, so session output looks exactly as it does today.

**Non-goals**

- The TUI. Terminals can't do glass, and the TUI keeps its look.
- In-window backdrop blur or refraction: blurring what is under a panel inside the window. GPUI 0.2.2 can't do it. Panels approximate glass with translucent fills, edge lines, and shadows, and only the window itself blurs.
- Window corner radius and traffic lights. macOS draws them.
- Liquid Glass morphing animations. A subtle hover and press state is enough, with no new motion system.
- A GUI theme editor. `config.toml` is enough.
- Any GPUI or gpui-component version change.

**Anti-goals**

- Text that falls below the contrast floor on a bright or dark wallpaper.
- A terminal that's harder to read than today's.
- A mode switch that needs a restart, or that leaves one surface in the old palette.
- Ignoring Reduce transparency.
- Behavior changes: keys, clicks, layout logic, focus.

## 3. Approaches considered

| | Approach | Trade-off |
|---|---|---|
| A (chosen) | One `Theme` value (a GPUI global) with `dark_glass()`, `light_glass()`, and opaque variants. Every view reads it; mine's palette is mirrored into gpui-component's theme. | One place to tune. Switching modes swaps one value and refreshes the window. Testable as data (contrast test). Costs a refactor of every literal color. |
| B | Only retheme gpui-component (its `ThemeMode::Light/Dark`) | mine draws most of its UI as plain GPUI `div`s with its own constants, so they wouldn't change. Not enough. |
| C | Per-view `if dark {…} else {…}` | Scatters the palette, can't be contrast-tested as a whole, and drifts quickly. |

## 4. Look

The spec fixes the shapes and the rules. Exact alpha values are tuned by the builder, under the contrast test.

### 4.1 Geometry

- **Window:** blurred background plus a full-window tint. The title bar stays transparent, with traffic lights as today.
- **Inset:** floating panels sit 12 px from the window edges and 12 px from each other. The tab bar sits in the title-bar row, centered.
- **Radii:**
  - Panels (task list, terminal pane): 20 px.
  - Launcher sheet and dialog: 28 px.
  - List rows: 16 px.
  - Capsules: full (height ÷ 2).
  - Chips: circles.
- **Edge:** a 1 px border in the palette's `edge` color (light on dark, a faint dark line on light), plus a brighter `highlight` 1 px top edge where GPUI allows it. GPUI has no inset shadow, so the top edge is a separate element or a gradient border; the builder's choice.
- **Shadow:** one soft drop shadow per floating panel (`BoxShadow`, palette `shadow`). Sheets get a deeper one.

### 4.2 Surfaces (theme tokens)

Each palette defines these tokens as RGBA. Names are fixed; values are the builder's, under section 7.

| Token | Dark glass (direction) | Light glass (direction) |
|---|---|---|
| `window_tint` | charcoal `#1c1e26` at ~0.75 | white at ~0.7 |
| `panel` | white at ~0.06–0.10 over the tint | white at ~0.55–0.7 |
| `panel_strong` (selected row, active tab, New task) | white at ~0.14–0.18 | white, opaque |
| `sheet` (launcher, dialog) | white at ~0.10–0.14 plus a deep shadow | white at ~0.8 |
| `edge`, `highlight` | white ~0.08 / ~0.22 | black ~0.05 / white ~0.95 |
| `shadow` | black ~0.28 | slate `#3c465a` ~0.16 |
| `terminal` | solid `#0b0c10` (TERM_BG, G-R4) | the same; the terminal stays dark in light mode |
| `text`, `text_secondary`, `text_tertiary` | `#e8eaef`, white ~0.62, white ~0.45 | `#1d1d1f`, `#48484a`, `#6e6e73` |
| `accent` (Allow, focus, ◆) | `#e5c07b` | `#e5c07b` fill with dark text; `#9a6a12` for ◆ and accent text |
| status ◆ ◐ ✓ ○ | current colors | darker variants that pass on light |

Chips keep each profile's color (profiles spec 5). The letter is dark or white, whichever has the higher contrast on that chip color, so user-chosen colors stay readable.

**How GPUI blends (from its Metal renderer).** Translucent layers add their coverage (One/One blending), so the wallpaper shows through at about `max(0, 1 − Σα)` of a stack, not `Π(1 − α)`. Rules:

- Along any stack (window tint → panel → row), the alphas must sum to ≤ 0.92 so a little wallpaper still shows. Light glass mostly needs lower values than the table's directions, e.g. a tint of ~0.35 under panels of ~0.5.
- The contrast test (section 7) composites with this real blend, not ideal "over".
- The first task takes one window-id capture early to confirm the blend before tuning.

### 4.3 Components

- **Task list:**
  - A floating panel.
  - Group headers are small uppercase secondary text with letter spacing.
  - Rows are 16 px rounded. The selected row uses `panel_strong`.
  - A row waiting for approval gets an accent-tinted fill (accent at low alpha) and an accent edge, with **Allow** (accent capsule, dark text, a soft accent glow) and **Deny** (clear capsule).
  - "＋ New task ⌘N" is a `panel_strong` capsule. The project filter is a capsule. "＋ Add project" is a quiet text capsule.
- **Tabs:**
  - One glass capsule bar in the title row.
  - Each tab is a capsule, and the active one is `panel_strong` with the highlight edge.
  - `+` is a round capsule.
  - Allow/Deny on a pending tab follow the task-list style, in compact form.
- **Terminal pane:**
  - A floating panel filled with `terminal`.
  - Default-background cells don't paint over it, so the pane's rounded glass shows. Cells with an explicit background paint as today.
  - Selection, find highlights, the cursor, and the IME underline are unchanged.
- **Launcher sheet:**
  - A `sheet` centered over the terminal pane.
  - Profile rows are 16 px rounded, and the hovered or Enter row uses `panel_strong`.
  - Key hints are small capsules: the Enter row's key is accent-filled, the others clear (G-R15: a key fill on a hovered row would exceed the 0.92 cap).
  - Not-logged-in rows use tertiary text.
- **Close-confirm dialog:** stays the native macOS alert (`window.prompt`). It keeps its modality, Return = Cancel (D-R9), Esc, and ⌘W. macOS Tahoe already draws it in its own glass, following the system appearance.
- **Find box:** a small glass capsule at the top right of the terminal pane. The match count is secondary text.
- **Launcher sheet placement:** the launcher keeps today's behavior and replaces the terminal content while open. The sheet floats over the empty glass terminal pane, never over a live session. This is a pure restyle.
- **Message and offline bar:** keep their current places and space. The offline banner stays at the top of the task list. The message bar still takes its row, so the terminal resizes as today. Each is restyled as a capsule within its slot and never overlays the terminal.
- **Hover and press:** hover raises a surface one step (e.g. `panel` → `panel_strong` at half strength); press dims it slightly. No animation beyond GPUI's instant state change.

## 5. Modes and settings

```toml
# <state>/config.toml
[appearance]
mode = "system"   # "system" (default) | "dark" | "light"
glass = "clear"   # "clear" (default) | "balanced" | "solid"
```

- **Glass levels** (the user's call, 2026-11-26, after comparing a build with the mockup):
  - **`clear`** (default): the mockup's translucency. Show-through is measured as `1 − Σα` under GPUI's blend, the share of the blurred backdrop that reaches the eye. Panels show ≈ 41 % (dark) and ≈ 29 % (light) of it. Those are the mockup's browser values. The terminal stays solid at every level, unlike the mockup's ≈ 9 %: more show-through can't hold ANSI 8 at 3:1 over white (Ruling G-R14). Secondary and tertiary text are drawn one weight heavier (Medium), and brighter in dark or darker in light, to hold up. Text is weaker than in `balanced` over a very bright backdrop in dark mode, or a very dark one in light mode; section 7 states the floors.
  - **`balanced`**: the tuning Tasks 1–4 shipped. It's readable over pure white at the full floors.
  - **`solid`**: the opaque variant (below).
  - Old configs keep working: `glass = true` → `clear`, `glass = false` → `solid`.

- **`mode = "system"`:** the app reads `window.appearance()` at start and on `observe_window_appearance`. `Dark` and `VibrantDark` map to dark; `Light` and `VibrantLight` map to light.
- **Switching:**
  - Swaps the `Theme` global, re-applies gpui-component's theme (`Theme::change(ThemeMode, …)` plus mine's overrides), and refreshes the window.
  - No surface may keep the old palette; this is tested.
- **Opaque:** each translucent token is composited over its backdrop color into an opaque one, and the window background becomes `Opaque`. This happens when `glass = "solid"` (or `false`), or when macOS **Reduce transparency** is on, whatever the level.
  - The app reads Reduce transparency at start and on window activation. The builder picks the mechanism, for example `defaults read com.apple.universalaccess reduceTransparency`, or a macOS API through a crate already in the dependency tree. A new dependency needs its reason in the commit body.
- **Bad config:** each bad value in `[appearance]` falls back to its default, with one warning per bad value in the app's log. A warning names the place (line or key) and the reason, never the value. It never blocks the app.
- **Who reads it:** `[appearance]` is a client-side setting. The app reads `<state>/config.toml` itself, at start and on window activation, so edits apply the next time the window is focused.
  - The daemon's profile parser must ignore the `[appearance]` table silently, so it never warns about it or treats the config as broken.
- **Pinned mode and native chrome:** `mode = "dark"` or `"light"` restyles mine's own surfaces only. GPUI 0.2.2 can't set the NSApp/NSWindow appearance, and mine adds no objc code for it, so native chrome follows the system: the close alert, the Add-project folder panel, and menus.

## 6. Code shape

- `crates/mine-app/src/theme.rs` becomes the token layer:
  - `pub struct Theme { … tokens … }`
  - `Theme::dark_glass()`, `light_glass()`, and `opaque(&self) -> Theme`
  - `pub fn resolve(mode, appearance, glass, reduce_transparency) -> Theme`
  - A GPUI `Global` holding the current `Theme`.
  - `install(cx)` applies it to gpui-component.
- The terminal colors (`TERM_*`, `ANSI`, `indexed`) stay as they are.
- Views (`sidebar.rs`, `tabs.rs`, `launcher.rs`, `workspace.rs`, `terminal/*`, the dialog and find box) read `cx.global::<Theme>()` and never `rgb(<literal>)`.
  - A test greps the view sources for `rgb(0x` and `rgba(0x` literals outside `theme.rs`; the brand palette lives in `theme.rs` only.
- Shared shape helpers live in one small module, e.g. `crates/mine-app/src/glass.rs`: `panel(…)`, `sheet(…)`, `capsule(…)`, with radius, edge, and shadow from the theme. Views compose them instead of repeating style chains.

## 7. Readability (the contrast rule)

For each palette, and for each text token over each surface it appears on, a unit test computes the WCAG contrast ratio. It composites the surface stack (window tint → panel → row fill) over two worst-case backdrops, pure white `#ffffff` and pure black `#000000`, to stand in for any wallpaper the blur might show.

- **Floors:**
  - `text`: ≥ 4.5:1.
  - `text_secondary`: ≥ 4.5:1 (it's small).
  - `text_tertiary`: ≥ 3:1 (used only for non-essential hints).
  - Status symbols and accent text: ≥ 3:1.
  - Chip letters on chip colors: ≥ 4.5:1.
- **Terminal:**
  - `#d0d3da` on `terminal` over both backdrops: ≥ 7:1.
  - The worst ANSI foreground used by agents (`#5c6370`): ≥ 3:1.
- The opaque variants must pass the same floors.
- **`clear` floors:**
  - Over `#000000`: the full floors above.
  - Over `#ffffff`: the user traded some readability for translucency. `text` ≥ 3.5, `text_secondary` ≥ 3, `text_tertiary`, status symbols, and accent text ≥ 2.
  - Chip letters (chips are opaque) and the terminal floors hold over both backdrops.
- The builder tunes the alphas until every case passes. The test is the authority for values; the mockup is the authority for feel.

## 8. Testing and review focus

- **Contrast tests** (section 7) for dark, light, and both opaque variants.
- **`resolve` table:**
  - Each `mode` × appearance (four variants) × `glass` × reduce-transparency combination gives the expected palette and window background.
  - A broken `[appearance]` section gives the defaults, with one warning per bad value.
- **Mode switch:** a model-level test that switching the appearance swaps the global and that every view reads from it. The grep test covers literals.
- **No behavior change:** the existing app, TUI, core, and integration suites stay green, with no test changes except color expectations.
- **Visual check:** one window-id capture (D-R4) of each mode, with a temp `MINE_STATE_DIR` and the same demo data as the README shot.
  - No key automation; the user is using this Mac.
  - The mode is forced through `[appearance] mode`, not by changing the user's system setting.
- **README:** replace `docs/images/mine-app.png` with the dark-glass capture (neutral demo data only), and mention `[appearance]` in both READMEs.

## 9. Calls made after the builder check (the controller's, open to the user's correction)

1. The close-confirm dialog stays the native alert (modality and D-R9 for free; macOS draws it as glass).
2. The launcher sheet floats over an empty terminal pane, as today. No live session sits under it.
3. The message and offline bar keep their slots, restyled as capsules, with no overlay.
4. Light glass is tuned for GPUI's additive blend: the stack alpha sum is ≤ 0.92, and the contrast test models the real blend. An early capture confirms it.
5. A pinned mode restyles mine's surfaces only; native chrome follows the system.
6. The app reads `[appearance]` itself, at start and on activation; the daemon ignores that table.
7. The chip letter color is chosen by contrast.
8. gpui-component's `Root` and `TitleBar` paint opaque theme colors today. The theme sets them transparent or to glass tokens, so the window blur shows.
9. Glass levels (section 5) were the user's call after comparing a build with the mockup: `clear` by default, `balanced` and `solid` in config.

## 10. Left to the builder

- Exact alpha, shadow, and highlight values, under section 7.
- How the top highlight edge is drawn in GPUI.
- The Reduce-transparency detection mechanism.
- How gpui-component's own widgets (the find box's input) pick up the tokens.
