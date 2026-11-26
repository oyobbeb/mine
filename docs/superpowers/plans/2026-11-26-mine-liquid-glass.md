# mine Liquid Glass (dark and light) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give mine-app Apple's Liquid Glass look in dark and light. The look follows the system appearance live, has an `[appearance]` override, falls back to opaque for Reduce transparency, and keeps text readable on any wallpaper. Behavior doesn't change.

**Architecture:**
- **Theme module.** A pure `theme` module defines the token palettes (dark glass, light glass, opaque variants), `resolve`, a GPUI-blend model, and a WCAG contrast test. A small `appearance` module reads `[appearance]` and Reduce transparency.
- **Wiring.** The app keeps the resolved `Theme` in a GPUI global and swaps it on `observe_window_appearance` and on window activation.
- **Views.** Every view draws from the theme through a few `glass` helpers (panel, sheet, capsule, row).

**Tech Stack:** Rust 2024; GPUI 0.2.2 (`WindowBackgroundAppearance::Blurred`, `observe_window_appearance`, `BoxShadow`, `linear_gradient`); gpui-component 0.4.1 theme.

**Spec:** `docs/superpowers/specs/2026-11-26-mine-liquid-glass-design.md` (sections cited below). Mockups: https://claude.ai/artifact/Kdy15eThT5NU7pTFQKw59L

## Global Constraints

- **Working location:** worktree `/Users/me/.gitbutler-worktrees/mine/feat-desktop`, branch `feat/liquid-glass`. Base: `92effed`.
- **Commits:** GitButler from the worktree only: `but commit -F - <paths>` without `-b`. No git write commands.
  - Conventional Commits in English, with a body stating what, why, and verification.
  - Final line `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`, or the model that did the work.
- **Language:** everything is English. Copy follows `.superpowers/sdd/GLOSSARY.md`. `README.ko.md` mirrors `README.md`.
- **Dependencies:**
  - GPUI pins stay `gpui =0.2.2` (runtime_shaders) and `gpui-component =0.4.1`.
  - No new dependency. Reduce transparency is read with `std::process::Command` (`defaults`).
- **Behavior:** no change. Keys, clicks, focus, layout logic, and the close alert (native `window.prompt`, D-R9) stay as they are.
- **Terminal:** the ANSI palette and `TERM_*` foreground, cursor, and selection colors are unchanged. The terminal pane stays dark in both modes.
- **Geometry (spec 4.1):**
  - Panels are inset 12 px from window edges and from each other.
  - Radii: panel 20 px, sheet 28 px, row 16 px, capsule full (height ÷ 2), chip circle.
  - Each floating surface gets a 1 px `edge`, a `highlight` top edge, and one `shadow`.
- **Token names (spec 4.2):** `window_tint`, `panel`, `panel_strong`, `sheet`, `edge`, `highlight`, `shadow`, `terminal`, `text`, `text_secondary`, `text_tertiary`, `accent`, `accent_text`, `on_accent`, `approval`, `working`, `done`, `idle`, `row_pending`, `row_pending_edge`.
- **Blend (spec 4.2):** translucent layers add coverage as in GPUI 0.2.2's Metal blend state. The alphas along any stack sum to ≤ 0.92.
- **Contrast floors (spec 7), over `#ffffff` and `#000000` backdrops with the real blend:**
  - `text` ≥ 4.5
  - `text_secondary` ≥ 4.5
  - `text_tertiary` ≥ 3
  - status symbols and accent text ≥ 3
  - chip letter ≥ 4.5 for the built-in palette
  - terminal `#d0d3da` on `terminal` ≥ 7
  - `#5c6370` on `terminal` ≥ 3
  - The opaque variants must pass too.
- **Config (spec 5):** `[appearance] mode = "system"|"dark"|"light"` (default `system`), `glass = true|false` (default `true`).
  - The app reads it itself, at start and on window activation.
  - The daemon's profile parser ignores the `appearance` table silently.
- **Safety:**
  - Never touch `~/.mine` or the user's running `~/.cargo/bin/mine daemon`; use temp `MINE_STATE_DIR`s.
  - GUI checks use D-R4 window-id capture only (the large on-screen mine-app window) and never key automation; the user is using this Mac.
  - Force the mode with `[appearance] mode`, never by changing the system setting. Clean up every process you start.
- **Check command:** `cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`. The flake `session_size_follows_last_input_client` may be rerun once.

## Review Focus

1. A very bright or very dark wallpaper must not make any text unreadable. Covered by Task 1 `contrast_floors_hold_for_every_palette`, which runs over white and black backdrops with the real blend.
2. A chip color the user picked, such as `#ffff00` or `#000080`, must still have a readable letter. Covered by Task 1 `chip_text_picks_readable_letter`.
3. A broken `[appearance]` (`mode = "blue"`, `glass = "yes"`) must fall back to the defaults with one warning. The daemon must never treat it as a broken profile config. Covered by Task 1 `bad_appearance_falls_back` and `appearance_table_is_ignored_by_profiles`.
4. Turning Reduce transparency on while the app runs must make it opaque at the next window activation. Covered by Task 2 `activation_rereads_reduce_transparency_and_config`.
5. Making the terminal pane glass must not make a full-screen app's own background colors (vim, htop) disappear. Covered by Task 4 `default_bg_cells_skip_paint_explicit_bg_cells_paint`.

---

### Task 1: Theme tokens, appearance config, contrast rule

**Files:**
- Modify: `crates/mine-app/src/theme.rs`. It becomes the token layer. Keep `TERM_*`, `ANSI`, `indexed`, `hex`, and the existing palette test.
- Create: `crates/mine-app/src/appearance.rs`
- Modify: `crates/mine-core/src/profiles.rs`. The `[appearance]` table is ignored silently.

**Interfaces:**
- **Produces:**
  ```rust
  // theme.rs
  #[derive(Clone, Copy, Debug, PartialEq)] pub struct Tint { pub rgb: u32, pub alpha: f32 }
  impl Tint { pub const fn solid(rgb: u32) -> Tint; pub fn hsla(self) -> gpui::Hsla; }
  #[derive(Clone, Debug, PartialEq)] pub struct Theme { pub dark: bool, pub glass: bool, /* every token name from Global Constraints, as Tint */ }
  impl Theme { pub fn dark_glass() -> Theme; pub fn light_glass() -> Theme; pub fn opaque(&self) -> Theme;
               pub fn window_background(&self) -> gpui::WindowBackgroundAppearance; } // Blurred when glass, else Opaque
  pub fn resolve(mode: appearance::Mode, system_dark: bool, glass: bool, reduce_transparency: bool) -> Theme;
  pub fn composite(stack: &[Tint], backdrop: u32) -> u32;   // GPUI 0.2.2 Metal blend, bottom-to-top
  pub fn contrast(fg: u32, bg: u32) -> f64;                 // WCAG 2.x ratio
  pub fn chip_text(chip_rgb: u32) -> u32;                   // 0x1b1d23 or 0xffffff, whichever contrasts more
  pub struct CurrentTheme(pub Theme); impl gpui::Global for CurrentTheme {}
  pub fn install(cx: &mut gpui::App, theme: &Theme);       // sets CurrentTheme + gpui-component theme mode/colors
  // appearance.rs
  #[derive(Clone, Copy, Debug, PartialEq, Eq, Default)] pub enum Mode { #[default] System, Dark, Light }
  #[derive(Clone, Copy, Debug, PartialEq, Eq)] pub struct Appearance { pub mode: Mode, pub glass: bool } // Default: System, true
  pub fn parse(text: Option<&str>) -> (Appearance, Vec<String>);   // warnings: line + reason only
  pub fn load(state: &mine_core::paths::State) -> (Appearance, Vec<String>);
  pub fn reduce_transparency() -> bool;   // `defaults read com.apple.universalaccess reduceTransparency` == "1"; false on any error
  ```
  - `install` must make gpui-component's `Root` and `TitleBar` backgrounds transparent (or `window_tint`) so the window blur shows (spec 9.8).
  - `opaque()` composites each translucent token over its backdrop into a solid `Tint`, and sets `glass = false`.

- [ ] **Step 1: Write the failing tests.**
  - `composite_matches_gpui_blend`: read GPUI 0.2.2's blend state (`platform/mac/metal_renderer.rs`, the pipeline `set_*_blend_factor` calls around lines 1200–1215) and implement exactly that. One known case must hold: a stack whose alphas sum to ≥ 1 hides the backdrop completely. Assert `composite(&[t(0x000000, 0.6), t(0x000000, 0.6)], 0xffffff) == 0x000000`, adjusted to the true formula if it differs, and say so in the report. Also assert a single `Tint { alpha: 0.0 }` returns the backdrop.
  - `contrast_known_values`: `contrast(0x000000, 0xffffff)` ≈ 21.0, `contrast(0x777777, 0xffffff)` ≈ 4.48 (±0.01).
  - `contrast_floors_hold_for_every_palette`: run for `dark_glass()`, `light_glass()`, and both `opaque()`. Composite each text or status token over its surface stacks (`[window_tint]`, `[window_tint, panel]`, `[window_tint, panel, panel_strong]`, `[window_tint, panel, row_pending]`, `[window_tint, sheet]`, `[window_tint, terminal]`) on both backdrops, then assert the Global Constraints floors. The failure message names the palette, token, stack, backdrop, and ratio.
  - `stack_alpha_sum_within_limit`: every listed stack's alpha sum is ≤ 0.92 in the glass palettes.
  - `chip_text_picks_readable_letter`: for `#e5c07b`, `#c678dd`, `#56b6c2`, `#98c379`, `#61afef`, `#e06c75`, `#ffff00`, and `#000080`, the chosen letter has contrast ≥ 4.5 for the built-in six, and is the better of the two choices for all.
  - `resolve_table`:
    - `(System, true, true, false)` → `dark && glass`.
    - `(System, false, true, false)` → `!dark && glass`.
    - `(Dark, false, …)` → dark.
    - `(Light, true, …)` → light.
    - `glass = false` or `reduce_transparency = true` → `!glass`, and `window_background()` is `Opaque`.
  - `bad_appearance_falls_back`: `mode = "blue"`, `glass = "yes"`, and a TOML syntax error each give `Appearance::default()` plus warnings. Warnings never quote values. A missing `[appearance]` gives the defaults with no warning.
  - mine-core `appearance_table_is_ignored_by_profiles`: a config with `[appearance] mode = "dark"` plus valid `[profiles]` gives no warning, and `broken` stays `None`.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-app -- theme appearance` and `cargo test -p mine-core -- appearance_table` → FAIL.
- [ ] **Step 3: Implement.**
  - Start the palettes from the spec 4.2 directions and the mockup values, then tune alphas until the contrast and stack tests pass.
  - `appearance::load` reads `<state>/config.toml` the way `mine_core::profiles` locates it.
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS. Existing views keep compiling: keep the old constants as thin aliases until Task 2, or migrate their uses, whichever is smaller.
- [ ] **Step 5: Commit.** `feat(app): add dark and light glass theme tokens with a tested contrast rule`

### Task 2: Glass window, floating layout, live appearance

**Files:**
- Create: `crates/mine-app/src/glass.rs`
- Modify: `crates/mine-app/src/main.rs` (window options, theme install, observers), `crates/mine-app/src/workspace.rs` (layout with insets and floating panels, the activation hook)

**Interfaces:**
- **Consumes:** Task 1 (`Theme`, `resolve`, `CurrentTheme`, `install`, `appearance::{load, reduce_transparency}`)
- **Produces:**
  ```rust
  // glass.rs — each returns a styled gpui Div; the caller adds children
  pub fn panel(theme: &Theme) -> Div;     // radius 20, panel fill, edge + highlight, shadow
  pub fn sheet(theme: &Theme) -> Div;     // radius 28, sheet fill, deeper shadow
  pub fn capsule(theme: &Theme, strong: bool) -> Div; // rounded_full, panel or panel_strong
  pub fn row(theme: &Theme, selected: bool) -> Div;   // radius 16
  pub const INSET: f32 = 12.0;
  // workspace / main
  pub fn appearance_theme(system_dark: bool, state: &State) -> Theme; // load + reduce_transparency + resolve (detection injectable for tests)
  ```
  - **Window:** `WindowOptions.window_background` comes from `theme.window_background()`. On `observe_window_appearance` and on window activation, the app recomputes `Theme`, calls `install`, calls `window.set_background_appearance(...)`, and refreshes.
  - **Layout:**
    - The task list and the terminal pane are `panel`s inset `INSET` from the window edges and from each other.
    - The tab bar is a `capsule` group centered in the title-bar row.
    - The traffic lights stay where they are.
  - Views not yet restyled in this task read colors from `cx.global::<CurrentTheme>()`.

- [ ] **Step 1: Write the failing tests** (model and logic level):
  - `appearance_theme_follows_system`: with config `mode = "system"` and detection stubbed off, `system_dark = false` → light glass, and `true` → dark glass.
  - `activation_rereads_reduce_transparency_and_config`: with injected detection, glass first; set reduce = true, then activate → opaque. Change config to `mode = "light"`, then activate → light.
  - `glass_helpers_use_theme_tokens`: `panel(&dark)` and `panel(&light)` produce different background fills, and `sheet` has a larger radius than `panel`. Inspect the `Div`'s style through GPUI's test support if needed; otherwise assert on a small style-descriptor struct the helpers build first.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-app -- appearance_theme activation_rereads glass_helpers` → FAIL.
- [ ] **Step 3: Implement.** Then confirm the blend with one early capture (D-R4) in each mode. Use a temp `MINE_STATE_DIR`, `[appearance] mode = "dark"` then `"light"`, and the README demo data (fake `claude`/`codex` on PATH as in `docs/images` history). If GPUI's real blend differs from Task 1's model, fix `composite` and the alphas (the Task 1 tests must still pass), and say so.
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS. Kill everything you started.
- [ ] **Step 5: Commit.** `feat(app): float the task list, tabs, and terminal as glass over a blurred window that follows the system appearance`

### Task 3: Task list and tabs restyle

**Files:**
- Modify: `crates/mine-app/src/sidebar.rs`, `crates/mine-app/src/tabs.rs`, plus chip drawing wherever it lives

**Interfaces:**
- **Consumes:** Task 1 tokens and `chip_text`; Task 2 `glass::{capsule, row}`.

The spec 4.3 rules for these components:
- **Group headers:** small uppercase `text_secondary` with letter spacing.
- **Rows:** a `row`. The selected row uses `panel_strong`.
- **A pending row:** `row_pending` fill with a `row_pending_edge`. **Allow** is an `accent` capsule with `on_accent` text and a soft accent shadow; **Deny** is a clear capsule. The element ids stay per (session, request, action), as Task 10 of D1.5 made them.
- **"＋ New task ⌘N":** a `panel_strong` capsule. The project filter is a capsule, and "＋ Add project" is a quiet text capsule.
- **Tabs:** capsules in the Task 2 capsule bar. The active tab is `panel_strong` with the highlight edge. `+` is round. A pending tab shows compact Allow/Deny capsules.
- **Chips:** circles in the profile color, with the letter in `chip_text(color)`.
- **Hover and press:** hover raises one step; press dims slightly.

- [ ] **Step 1: Write the failing tests.** `chip_letter_uses_chip_text` is a unit test on the chip builder or its color function: a navy chip gets a white letter, and a gold chip gets a dark letter. Existing sidebar and tab tests (Allow/Deny ids, click wiring, pending rows) must still pass unchanged.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-app -- chip_letter` → FAIL.
- [ ] **Step 3: Implement** with the `glass` helpers. Replace every literal color in these files with theme tokens.
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS. Capture both modes (D-R4) with the demo data. Kill everything you started.
- [ ] **Step 5: Commit.** `feat(app): restyle the task list and tabs as Liquid Glass with capsule controls`

### Task 4: Launcher sheet, terminal pane, find box, bars, docs

**Files:**
- Modify: `crates/mine-app/src/launcher.rs`, `crates/mine-app/src/terminal/{mod.rs,render.rs}`, `crates/mine-app/src/workspace.rs` (message bar, offline banner)
- Modify: `README.md`, `README.ko.md`, `docs/images/mine-app.png`
- Test: a source-scan test in `crates/mine-app` (e.g. `tests/no_literal_colors.rs` or a unit test)

**Interfaces:**
- **Consumes:** Tasks 1–3.

The spec 4.3 rules for these components:
- **Launcher:** a `sheet` centered over the empty terminal pane, as today, never over a live session.
  - Profile rows are `row`s, and the hovered or Enter row uses `panel_strong`.
  - Key hints are small capsules: the Enter row's is `accent` with `on_accent` text, the others `panel_strong`.
  - Not-logged-in rows use `text_tertiary`.
- **Terminal pane:** fill is `terminal`. Cells whose background is the default background are not painted; cells with an explicit background paint exactly as today. Selection, find highlights, the cursor, and the IME underline are unchanged.
- **Find box:** a small capsule at the top right of the pane. The match count is `text_secondary`.
- **Message bar and offline banner:** they keep their current slots (spec 4.3) and are restyled as capsules within them.

- [ ] **Step 1: Write the failing tests.**
  - `default_bg_cells_skip_paint_explicit_bg_cells_paint` in `render.rs`: a row with default-bg cells, one `\e[44m` blue-bg cell, and a selected cell → the background runs it emits contain only the explicit-bg and selected cells.
  - `no_literal_colors_outside_theme`: scan `crates/mine-app/src/**/*.rs` except `theme.rs` for `rgb(0x`, `rgba(0x`, and `hsla(` literals, and assert none remain. Test modules are exempt only if the scan can tell them apart cleanly; otherwise exempt nothing.
- [ ] **Step 2: Confirm the failure.** Run: `cargo test -p mine-app -- default_bg_cells no_literal_colors` → FAIL.
- [ ] **Step 3: Implement.**
  - **README:**
    - Replace `docs/images/mine-app.png` with a dark-glass capture (D-R4) using neutral demo data only: no username or company names.
    - Add `[appearance]` to the Configuration section, and mention that the look follows the system appearance and Reduce transparency.
    - Mirror both changes in `README.ko.md` (polite 해요체).
- [ ] **Step 4: Confirm the tests pass.** Run the check command → PASS. Capture both modes once more for the report. Kill everything you started.
- [ ] **Step 5: Commit.** `feat(app): restyle the launcher, terminal pane, find box, and bars as Liquid Glass`
