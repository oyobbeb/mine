//! Colors. Two layers live here:
//!
//! - The token layer ([`Theme`]): the dark and light glass palettes as data, a model of GPUI's blend ([`composite`]), and the
//!   contrast rule the palettes are tested against. Views read the current [`Theme`] with [`current`].
//! - The terminal colors. Values are the `0xrrggbb` passed to GPUI `rgb()`.

use gpui::{App, Global, Hsla, WindowBackgroundAppearance, rgb};
use gpui_component::{Theme as UiTheme, ThemeMode};

use crate::appearance::Mode;

/// Default terminal text and background. On connect, the app sends these two colors to the daemon with `ToDaemon::Colors`, which
/// answers OSC 10/11 with them. The background is the pane's own color, the `terminal` token, so what an app learns from OSC 11, the
/// input method's composing fill and inverse video match the pane (Ruling G-R4).
pub const TERM_FG: u32 = 0xd0d3da;
pub const TERM_BG: u32 = 0x0b0c10;
pub const TERM_CURSOR: u32 = 0xd0d3da;
/// The background of selected terminal text and of the find box's matches. The text keeps its own color.
pub const TERM_SELECTION: u32 = 0x2f323a;

/// ANSI 0–15.
pub const ANSI: [u32; 16] = [
    0x1e2128, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xd0d3da, //
    0x5c6370, 0xef8a92, 0xb5d99c, 0xf0d197, 0x8cc5f5, 0xd898e6, 0x7fcbd5, 0xffffff,
];

/// Color number `i` of the 256-color palette. 0–15 are [`ANSI`], 16–231 are the xterm 6×6×6 cube, and 232–255 are the gray ramp.
pub fn indexed(i: u8) -> u32 {
    const LEVELS: [u32; 6] = [0, 95, 135, 175, 215, 255];
    match i {
        0..=15 => ANSI[usize::from(i)],
        16..=231 => {
            let n = u32::from(i - 16);
            let (r, g, b) = (
                LEVELS[(n / 36) as usize],
                LEVELS[(n / 6 % 6) as usize],
                LEVELS[(n % 6) as usize],
            );
            r << 16 | g << 8 | b
        }
        232..=255 => {
            let v = 8 + 10 * u32::from(i - 232);
            v << 16 | v << 8 | v
        }
    }
}

/// A `#rrggbb` string.
pub fn hex(color: u32) -> String {
    format!("#{color:06x}")
}

/// A color with its own alpha. `rgb` is `0xrrggbb`; `alpha` is 0.0 (clear) to 1.0 (solid).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tint {
    pub rgb: u32,
    pub alpha: f32,
}

impl Tint {
    pub const fn solid(rgb: u32) -> Tint {
        Tint { rgb, alpha: 1.0 }
    }

    pub fn hsla(self) -> Hsla {
        Hsla {
            a: self.alpha,
            ..Hsla::from(rgb(self.rgb))
        }
    }
}

const fn tint(rgb: u32, alpha: f32) -> Tint {
    Tint { rgb, alpha }
}

/// Every color the views draw with, for one look: dark or light, glass or opaque.
///
/// Surfaces are drawn bottom to top, and each one only adds to what is below it: the window tint over the blurred wallpaper, then a
/// `panel` (the task list, the tab bar, the center pane), then a `panel_strong` (the selected row, the active tab), a `row_pending`, a
/// [`Theme::hover`], or the launcher's `sheet`; the `terminal` fill replaces `panel` in the center pane. Nothing translucent goes on top
/// of those (their stacks are at the 0.92 alpha cap), so a control that has a fill lights its edge on hover instead. No text sits on
/// the bare window tint: the title row holds only the tab capsule. See [`composite`] for how GPUI blends such a stack.
/// Text and status colors are solid: a translucent glyph would let the wallpaper through it.
#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub dark: bool,
    /// Translucent surfaces over a blurred window. `false` is the opaque variant.
    pub glass: bool,
    /// Painted once over the whole window, by gpui-component's `Root`, so views must not paint it again.
    pub window_tint: Tint,
    pub panel: Tint,
    /// Selected row, active tab, the "New task" capsule.
    pub panel_strong: Tint,
    /// The launcher and dialogs.
    pub sheet: Tint,
    /// The 1 px line around a floating surface.
    pub edge: Tint,
    /// The brighter 1 px line along a floating surface's top edge.
    pub highlight: Tint,
    /// The drop shadow of a floating surface.
    pub shadow: Tint,
    /// The terminal pane. Dark in both modes, and solid: it covers the wallpaper so session output reads as it always has (Ruling
    /// G-R2), and it is [`TERM_BG`].
    pub terminal: Tint,
    pub text: Tint,
    pub text_secondary: Tint,
    /// Hints and other non-essential text.
    pub text_tertiary: Tint,
    /// The fill of the one action that matters (Allow), and focus.
    pub accent: Tint,
    /// The accent as text or as a symbol on glass.
    pub accent_text: Tint,
    /// The label on an `accent` fill.
    pub on_accent: Tint,
    /// Status symbols: Needs approval ◆, Working ◐, Done ✓, Idle ○.
    pub approval: Tint,
    pub working: Tint,
    pub done: Tint,
    pub idle: Tint,
    /// The fill and the edge of a row that waits for approval.
    pub row_pending: Tint,
    pub row_pending_edge: Tint,
}

impl Theme {
    /// Dark glass. Over a white wallpaper the leftover (1 − Σα) of the backdrop is what the light text has to beat, so a panel stack
    /// sums to 0.84. Half of that is the panels' own slate fill, not the window tint: the gaps between the panels show half the
    /// blurred wallpaper, and the panels read as darker glass over it (over white, gap 141 and panel 67; over black, 13 and 26).
    pub fn dark_glass() -> Theme {
        Theme {
            dark: true,
            glass: true,
            window_tint: tint(0x1a1d27, 0.50),
            panel: tint(0x343844, 0.34),
            panel_strong: tint(0xffffff, 0.08),
            sheet: tint(0xffffff, 0.08),
            edge: tint(0xffffff, 0.14),
            highlight: tint(0xffffff, 0.36),
            shadow: tint(0x000000, 0.28),
            terminal: Tint::solid(TERM_BG),
            text: Tint::solid(0xf5f6f8),
            text_secondary: Tint::solid(0xc2c5cc),
            text_tertiary: Tint::solid(0xa6aab3),
            accent: Tint::solid(0xe5c07b),
            accent_text: Tint::solid(0xe5c07b),
            on_accent: Tint::solid(0x1b1d23),
            approval: Tint::solid(0xe5c07b),
            working: Tint::solid(0x56b6c2),
            done: Tint::solid(0x98c379),
            idle: Tint::solid(0xa6aab3),
            row_pending: tint(0xe5c07b, 0.08),
            row_pending_edge: tint(0xe5c07b, 0.45),
        }
    }

    /// Light glass. Over a black wallpaper white layers only reach 1 − Π(1 − α) of white, so the window tint is heavy and the text
    /// colors are darker than a flat light theme would use.
    pub fn light_glass() -> Theme {
        Theme {
            dark: false,
            glass: true,
            window_tint: tint(0xffffff, 0.70),
            panel: tint(0xffffff, 0.12),
            panel_strong: tint(0xffffff, 0.10),
            sheet: tint(0xffffff, 0.10),
            edge: tint(0x000000, 0.06),
            highlight: tint(0xffffff, 0.95),
            shadow: tint(0x3c465a, 0.16),
            terminal: Tint::solid(TERM_BG),
            text: Tint::solid(0x1d1d1f),
            text_secondary: Tint::solid(0x3f3f43),
            text_tertiary: Tint::solid(0x5a5a5f),
            accent: Tint::solid(0xe5c07b),
            accent_text: Tint::solid(0x6b4700),
            on_accent: Tint::solid(0x1b1d23),
            approval: Tint::solid(0x6b4700),
            working: Tint::solid(0x0b5c66),
            done: Tint::solid(0x1f5a14),
            idle: Tint::solid(0x5a5a5f),
            row_pending: tint(0xe5c07b, 0.10),
            row_pending_edge: tint(0x9a6a12, 0.55),
        }
    }

    /// The same palette with every surface turned solid. Each surface token becomes what its stack looks like over a neutral backdrop
    /// (a near-black, so the panels stand off the window as they do in glass, or a light gray), so it keeps the glass look without
    /// needing a wallpaper. `shadow` stays translucent: a shadow needs its alpha to fade out.
    pub fn opaque(&self) -> Theme {
        let base = if self.dark { 0x0e1016 } else { 0xe8e8ed };
        let solid = |stack: &[Tint]| Tint::solid(composite(stack, base));
        let (w, p) = (self.window_tint, self.panel);
        Theme {
            glass: false,
            window_tint: solid(&[w]),
            panel: solid(&[w, p]),
            panel_strong: solid(&[w, p, self.panel_strong]),
            sheet: solid(&[w, p, self.sheet]),
            terminal: solid(&[w, self.terminal]),
            edge: solid(&[w, p, self.edge]),
            highlight: solid(&[w, p, self.highlight]),
            row_pending: solid(&[w, p, self.row_pending]),
            row_pending_edge: solid(&[w, p, self.row_pending_edge]),
            ..self.clone()
        }
    }

    /// A control without a fill, under the pointer: raised one step, to half a `panel_strong`.
    pub fn hover(&self) -> Tint {
        Tint {
            alpha: self.panel_strong.alpha / 2.,
            ..self.panel_strong
        }
    }

    /// The window's own background: blurred wallpaper for glass, plain for the opaque variants.
    pub fn window_background(&self) -> WindowBackgroundAppearance {
        if self.glass {
            WindowBackgroundAppearance::Blurred
        } else {
            WindowBackgroundAppearance::Opaque
        }
    }
}

/// The theme for a mode. `system_dark` is the window's appearance; `reduce_transparency` is the macOS accessibility setting. The
/// caller reads those (see `appearance::reduce_transparency`), so this stays pure.
pub fn resolve(mode: Mode, system_dark: bool, glass: bool, reduce_transparency: bool) -> Theme {
    let dark = match mode {
        Mode::System => system_dark,
        Mode::Dark => true,
        Mode::Light => false,
    };
    let theme = if dark {
        Theme::dark_glass()
    } else {
        Theme::light_glass()
    };
    if glass && !reduce_transparency {
        theme
    } else {
        theme.opaque()
    }
}

fn channels(rgb: u32) -> [f64; 3] {
    [16, 8, 0].map(|shift| f64::from((rgb >> shift) & 0xff) / 255.0)
}

/// What a stack of translucent layers, painted bottom to top over `backdrop`, looks like on screen, as GPUI 0.2.2's Metal renderer
/// blends it (`gpui/src/platform/mac/metal_renderer.rs`, `build_pipeline_state`, used for quads and text):
///
/// - RGB uses `SourceAlpha`, `OneMinusSourceAlpha`: the ordinary "over", into a layer cleared to transparent black.
/// - Alpha uses `One`, `One`: coverage **adds** (and the 8-bit target clamps it at 1).
///
/// macOS then treats the window's layer as premultiplied, `result = color + (1 − coverage) · backdrop`. So the layers keep their ideal
/// "over" color, but the wallpaper only shows at `max(0, 1 − Σα)` (not `Π(1 − α)`), and when `Σα > 1` the light that "over" would have
/// kept from the lower layers is gone (black), not wallpaper. A stack whose alphas sum to 0.92 leaves 8%.
pub fn composite(stack: &[Tint], backdrop: u32) -> u32 {
    let (mut color, mut coverage) = ([0.0_f64; 3], 0.0_f64);
    for layer in stack {
        let a = f64::from(layer.alpha);
        for (v, c) in color.iter_mut().zip(channels(layer.rgb)) {
            *v = c * a + *v * (1.0 - a);
        }
        coverage = (coverage + a).min(1.0);
    }
    let back = channels(backdrop);
    let byte =
        |i: usize| ((color[i] + (1.0 - coverage) * back[i]).clamp(0.0, 1.0) * 255.0).round() as u32;
    byte(0) << 16 | byte(1) << 8 | byte(2)
}

/// The WCAG 2.x contrast ratio of two `0xrrggbb` colors, 1.0 to 21.0.
pub fn contrast(fg: u32, bg: u32) -> f64 {
    let luminance = |rgb: u32| {
        let [r, g, b] = channels(rgb).map(|v| {
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        });
        0.2126 * r + 0.7152 * g + 0.0722 * b
    };
    let (a, b) = (luminance(fg), luminance(bg));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// The letter color on a profile chip: dark or white, whichever contrasts more with the chip's own color.
pub fn chip_text(chip_rgb: u32) -> u32 {
    const DARK: u32 = 0x1b1d23;
    if contrast(DARK, chip_rgb) >= contrast(0xffffff, chip_rgb) {
        DARK
    } else {
        0xffffff
    }
}

/// The theme the views draw with, as a GPUI global.
pub struct CurrentTheme(pub Theme);

impl Global for CurrentTheme {}

/// The theme the views draw with. Every view reads it while drawing, so installing another one and refreshing repaints them all.
pub fn current(cx: &App) -> &Theme {
    &cx.global::<CurrentTheme>().0
}

/// Makes `theme` the current theme and mirrors it into gpui-component's (its mode, and the colors of the widgets mine still uses).
///
/// `Root` paints `background`, and the title bar paints `title_bar`: `background` is `window_tint`, painted once for the whole window
/// so the blur shows under it, and the title bar is clear.
pub fn install(cx: &mut App, theme: &Theme) {
    cx.set_global(CurrentTheme(theme.clone()));
    UiTheme::change(
        if theme.dark {
            ThemeMode::Dark
        } else {
            ThemeMode::Light
        },
        None,
        cx,
    );
    let clear = Hsla::transparent_black();
    let t = UiTheme::global_mut(cx);
    t.background = theme.window_tint.hsla();
    t.foreground = theme.text.hsla();
    t.border = theme.edge.hsla();
    t.muted_foreground = theme.text_secondary.hsla();
    t.title_bar = clear;
    t.title_bar_border = clear;
    t.sidebar = clear;
    t.sidebar_border = theme.edge.hsla();
    t.sidebar_foreground = theme.text.hsla();
    t.sidebar_accent = theme.panel_strong.hsla();
    t.sidebar_accent_foreground = theme.text.hsla();
    t.scrollbar = clear;
    t.scrollbar_thumb = theme.text_tertiary.hsla();
    t.scrollbar_thumb_hover = theme.text_secondary.hsla();
    // Input box (the launcher's `n`): the border is the edge, the focus border is the secondary text color, and the cursor is the text color.
    t.input = theme.edge.hsla();
    t.ring = theme.text_secondary.hsla();
    t.caret = theme.text.hsla();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(rgb: u32, alpha: f32) -> Tint {
        Tint { rgb, alpha }
    }

    #[test]
    fn palette_matches_xterm() {
        assert_eq!(indexed(1), 0xe06c75);
        assert_eq!(indexed(15), 0xffffff);
        assert_eq!(indexed(16), 0x000000);
        assert_eq!(indexed(21), 0x0000ff);
        assert_eq!(indexed(196), 0xff0000);
        assert_eq!(indexed(231), 0xffffff);
        assert_eq!(indexed(232), 0x080808);
        assert_eq!(indexed(255), 0xeeeeee);
        assert_eq!(hex(TERM_BG), "#0b0c10");
    }

    #[test]
    fn composite_matches_gpui_blend() {
        // A stack whose alphas sum to 1 or more hides the backdrop completely.
        assert_eq!(
            composite(&[t(0x000000, 0.6), t(0x000000, 0.6)], 0xffffff),
            0x000000
        );
        // No layer, or a clear one, is the backdrop.
        assert_eq!(composite(&[], 0xabcdef), 0xabcdef);
        assert_eq!(composite(&[t(0x123456, 0.0)], 0xabcdef), 0xabcdef);
        // One layer is the ordinary "over".
        assert_eq!(composite(&[t(0x000000, 0.5)], 0xffffff), 0x808080);
        // The wallpaper shows through at 1 - Σα, not Π(1 - α): two 0.3 layers leave 0.4, not 0.49.
        assert_eq!(
            composite(&[t(0x000000, 0.3), t(0x000000, 0.3)], 0xffffff),
            0x666666
        );
        // Past full coverage the missing light is black, not wallpaper: 0.6 then 0.6 of white is 0.84, not 1.0.
        assert_eq!(
            composite(&[t(0xffffff, 0.6), t(0xffffff, 0.6)], 0x000000),
            0xd6d6d6
        );
    }

    #[test]
    fn contrast_known_values() {
        assert!((contrast(0x000000, 0xffffff) - 21.0).abs() < 0.01);
        assert!((contrast(0xffffff, 0x000000) - 21.0).abs() < 0.01);
        assert!((contrast(0x777777, 0xffffff) - 4.48).abs() < 0.01);
    }

    const BACKDROPS: [u32; 2] = [0xffffff, 0x000000];

    fn palettes() -> [(&'static str, Theme); 4] {
        [
            ("dark glass", Theme::dark_glass()),
            ("light glass", Theme::light_glass()),
            ("dark opaque", Theme::dark_glass().opaque()),
            ("light opaque", Theme::light_glass().opaque()),
        ]
    }

    /// The surfaces text is drawn on, bottom to top. The terminal pane has its own rule. The bare window tint is not one: the title
    /// row holds only the tab capsule, the message bar and the offline banner are capsules, and the launcher's sheet sits in the
    /// center pane (Ruling G-R3). Everything on the sheet is on `[window_tint, panel, sheet]`, which is at the alpha cap: the rows,
    /// the lit row, the clear key capsules and the hints add no fill. The lit row's key is `on_accent` on the solid `accent`.
    fn surface_stacks(th: &Theme) -> [(&'static str, Vec<Tint>); 5] {
        let w = th.window_tint;
        [
            ("[window_tint, panel]", vec![w, th.panel]),
            // A row, tab or button without a fill of its own, under the pointer.
            ("[window_tint, panel, hover]", vec![w, th.panel, th.hover()]),
            (
                "[window_tint, panel, panel_strong]",
                vec![w, th.panel, th.panel_strong],
            ),
            (
                "[window_tint, panel, row_pending]",
                vec![w, th.panel, th.row_pending],
            ),
            ("[window_tint, panel, sheet]", vec![w, th.panel, th.sheet]),
        ]
    }

    #[test]
    fn contrast_floors_hold_for_every_palette() {
        for (name, th) in palettes() {
            let tokens = [
                ("text", th.text, 4.5),
                ("text_secondary", th.text_secondary, 4.5),
                ("text_tertiary", th.text_tertiary, 3.0),
                ("accent_text", th.accent_text, 3.0),
                ("approval", th.approval, 3.0),
                ("working", th.working, 3.0),
                ("done", th.done, 3.0),
                ("idle", th.idle, 3.0),
            ];
            for (stack_name, stack) in surface_stacks(&th) {
                for backdrop in BACKDROPS {
                    let surface = composite(&stack, backdrop);
                    for (token, tint, floor) in tokens {
                        let mut with_text = stack.clone();
                        with_text.push(tint);
                        let ratio = contrast(composite(&with_text, backdrop), surface);
                        assert!(
                            ratio >= floor,
                            "{name}: {token} on {stack_name} over {backdrop:06x} is {ratio:.2}:1, needs {floor}"
                        );
                    }
                }
            }
            // The label of an accent button.
            assert_eq!(th.accent.alpha, 1.0, "{name}: accent is solid");
            assert_eq!(th.on_accent.alpha, 1.0, "{name}: on_accent is solid");
            let ratio = contrast(th.on_accent.rgb, th.accent.rgb);
            assert!(ratio >= 4.5, "{name}: on_accent on accent is {ratio:.2}:1");
            // The find box over the terminal: the panel's solid form, under the input's text and the match count.
            let find = th.opaque().panel.rgb;
            for (token, tint) in [("text", th.text), ("text_secondary", th.text_secondary)] {
                let ratio = contrast(tint.rgb, find);
                assert!(
                    ratio >= 4.5,
                    "{name}: {token} on the find box is {ratio:.2}:1"
                );
            }
            // The terminal stays dark in both modes, and its text is read on it.
            for backdrop in BACKDROPS {
                let bg = composite(&[th.window_tint, th.terminal], backdrop);
                for (what, fg, floor) in [("TERM_FG", TERM_FG, 7.0), ("ANSI 8", ANSI[8], 3.0)] {
                    let ratio = contrast(fg, bg);
                    assert!(
                        ratio >= floor,
                        "{name}: {what} on [window_tint, terminal] over {backdrop:06x} is {ratio:.2}:1, needs {floor}"
                    );
                }
            }
        }
    }

    /// What apps learn from OSC 11 (the daemon answers with `TERM_BG`), the input method's composing fill and inverse video are the
    /// pane's own color, in every palette and over any wallpaper (Ruling G-R4).
    #[test]
    fn term_bg_is_the_terminal_pane() {
        for (name, th) in palettes() {
            for backdrop in BACKDROPS {
                let pane = composite(&[th.window_tint, th.terminal], backdrop);
                assert_eq!(
                    hex(pane),
                    hex(TERM_BG),
                    "{name}: the terminal pane over {backdrop:06x}"
                );
            }
        }
    }

    fn alpha_sum(stack: &[Tint]) -> f64 {
        stack.iter().map(|t| f64::from(t.alpha)).sum()
    }

    #[test]
    fn stack_alpha_sum_within_limit() {
        for (name, th) in [
            ("dark glass", Theme::dark_glass()),
            ("light glass", Theme::light_glass()),
        ] {
            for (stack_name, stack) in surface_stacks(&th) {
                let sum = alpha_sum(&stack);
                assert!(sum <= 0.92 + 1e-6, "{name}: {stack_name} sums to {sum:.3}");
            }
            // The terminal pane is the exception: it covers the backdrop, so the dim ANSI colors keep their contrast over a white wallpaper.
            let sum = alpha_sum(&[th.window_tint, th.terminal]);
            assert!(sum >= 1.0, "{name}: the terminal stack sums to {sum:.3}");
        }
    }

    #[test]
    fn opaque_variants_are_solid() {
        for (name, th) in [
            ("dark", Theme::dark_glass()),
            ("light", Theme::light_glass()),
        ] {
            let o = th.opaque();
            assert!(!o.glass && o.dark == th.dark, "{name}");
            for (token, tint) in [
                ("window_tint", o.window_tint),
                ("panel", o.panel),
                ("panel_strong", o.panel_strong),
                ("sheet", o.sheet),
                ("edge", o.edge),
                ("highlight", o.highlight),
                ("terminal", o.terminal),
                ("row_pending", o.row_pending),
                ("row_pending_edge", o.row_pending_edge),
                ("text_secondary", o.text_secondary),
                ("text_tertiary", o.text_tertiary),
            ] {
                assert_eq!(tint.alpha, 1.0, "{name}: {token}");
            }
            // A shadow needs its alpha to fade out.
            assert_eq!(o.shadow, th.shadow, "{name}");
        }
    }

    #[test]
    fn chip_text_picks_readable_letter() {
        let better = |chip: u32| {
            let letter = chip_text(chip);
            assert!(letter == 0x1b1d23 || letter == 0xffffff, "{chip:06x}");
            let other = if letter == 0xffffff {
                0x1b1d23
            } else {
                0xffffff
            };
            assert!(
                contrast(letter, chip) >= contrast(other, chip),
                "{chip:06x}: {letter:06x} is not the better letter"
            );
            letter
        };
        // The built-in six.
        for chip in [0xe5c07b, 0xc678dd, 0x56b6c2, 0x98c379, 0x61afef, 0xe06c75] {
            let letter = better(chip);
            assert!(
                contrast(letter, chip) >= 4.5,
                "{chip:06x}: {:.2}:1",
                contrast(letter, chip)
            );
        }
        // Colors a user may pick.
        assert_eq!(better(0xffff00), 0x1b1d23);
        assert_eq!(better(0x000080), 0xffffff);
    }

    #[test]
    fn resolve_table() {
        let th = resolve(Mode::System, true, true, false);
        assert!(th.dark && th.glass);
        assert_eq!(th.window_background(), WindowBackgroundAppearance::Blurred);
        let th = resolve(Mode::System, false, true, false);
        assert!(!th.dark && th.glass);
        for system_dark in [true, false] {
            let th = resolve(Mode::Dark, system_dark, true, false);
            assert!(th.dark && th.glass, "Dark with system_dark={system_dark}");
            let th = resolve(Mode::Light, system_dark, true, false);
            assert!(!th.dark && th.glass, "Light with system_dark={system_dark}");
        }
        // No glass, or Reduce transparency: opaque, and the window is not blurred.
        for (mode, system_dark, glass, reduce) in [
            (Mode::System, true, false, false),
            (Mode::System, false, true, true),
            (Mode::Dark, false, false, true),
            (Mode::Light, true, true, true),
        ] {
            let th = resolve(mode, system_dark, glass, reduce);
            assert!(!th.glass, "{mode:?} {system_dark} {glass} {reduce}");
            assert_eq!(th.window_background(), WindowBackgroundAppearance::Opaque);
            assert_eq!(th, resolve(mode, system_dark, true, false).opaque());
        }
        assert!(resolve(Mode::Dark, false, false, false).dark);
        assert!(!resolve(Mode::Light, true, false, false).dark);
    }
}
