//! The shapes of the floating glass surfaces, so views compose them instead of repeating style chains. Each helper returns a styled
//! `Div` with its fill, edge, highlight and shadow from a [`Theme`]; the caller adds the size, the layout and the children.
//!
//! A floating surface (`panel`, `sheet`, `capsule`) carries two children of its own, painted before the caller's: the `highlight` line
//! along its top, and its shadow. Its shadow is painted outside it, so don't set `overflow_hidden` on the surface itself (that would
//! clip the shadow away); put it on a child.

use gpui::{
    BorderStyle, Corners, Div, Hsla, InteractiveElement, IntoElement, ParentElement, Pixels,
    Stateful, StatefulInteractiveElement, Styled, canvas, div, px, quad,
};

use crate::theme::{Theme, Tint};

/// The gap between floating panels, and between a panel and the window edge.
pub const INSET: f32 = 12.0;

/// The width of a surface's edge. GPUI lays a border out inside the surface, where the mockup's inset-shadow edge takes no room.
const EDGE: f32 = 1.;

/// A padding from the mockup, which measures from a surface's outer edge, as the padding to set on a `panel`, `sheet`, `capsule`
/// or `row`: inside its 1 px edge. Use it for every padding on these surfaces, so content lands on the mockup's pixel.
pub const fn pad(mockup: f32) -> Pixels {
    px(mockup - EDGE)
}

const PANEL_RADIUS: f32 = 20.;
const SHEET_RADIUS: f32 = 28.;
const ROW_RADIUS: f32 = 16.;
/// What `rounded_full` uses. GPUI clamps a radius to half the shorter side, so it becomes height ÷ 2.
const FULL_RADIUS: f32 = 9999.;

/// How far each kind of shadow spreads (its Gaussian's sigma, in px; it fades out by 3 × that). A panel's is gone 7 px out: the
/// panels sit 7 px below the tab bar (a 38 px bar centered in the 52 px title row), and a wider one would paint over the bar's
/// bottom edge. A sheet's is deeper.
const PANEL_BLUR: f32 = 2.3;
const SHEET_BLUR: f32 = 10.;
const CAPSULE_BLUR: f32 = 2.;

/// The task list and the terminal pane: radius 20, `panel` fill, edge and highlight, and a shadow.
pub fn panel(theme: &Theme) -> Div {
    floating(theme, theme.panel, PANEL_RADIUS, PANEL_BLUR, theme.shadow)
}

/// The launcher and dialogs: radius 28, `sheet` fill, and a deeper shadow.
pub fn sheet(theme: &Theme) -> Div {
    floating(theme, theme.sheet, SHEET_RADIUS, SHEET_BLUR, theme.shadow)
}

/// A capsule (fully rounded): the tab bar, buttons. `strong` is the active or primary one (`panel_strong`); it then stands out by its
/// fill, and, in a group of plain items, by its edge, highlight and shadow too.
pub fn capsule(theme: &Theme, strong: bool) -> Div {
    let fill = if strong {
        theme.panel_strong
    } else {
        theme.panel
    };
    floating(theme, fill, FULL_RADIUS, CAPSULE_BLUR, theme.shadow)
}

/// A list row: radius 16. Selected, it gets `panel_strong` plus the edge outline and the highlight line, because in light glass
/// `panel_strong` is barely brighter than the panel under it. Unselected, the outline is clear, so selecting doesn't move it.
pub fn row(theme: &Theme, selected: bool) -> Div {
    if selected {
        lit(theme, Some(theme.panel_strong), theme.edge, ROW_RADIUS)
    } else {
        plain(ROW_RADIUS)
    }
}

/// A row with no fill of its own: a box in a panel (the offline banner), or the base of a [`sheet_row`].
/// Lit, it shows by the edge outline and the highlight line; unlit, the outline is clear.
pub fn clear_row(theme: &Theme, on: bool) -> Div {
    if on {
        lit(theme, None, theme.edge, ROW_RADIUS)
    } else {
        plain(ROW_RADIUS)
    }
}

/// A launcher row on a [`sheet`]: a [`clear_row`] that, lit, also takes the mockup's fill where the theme has room for it
/// ([`Theme::sheet_row_fill`]).
pub fn sheet_row(theme: &Theme, lit: bool) -> Div {
    let row = clear_row(theme, lit);
    match theme.sheet_row_fill().filter(|_| lit) {
        Some(fill) => row.bg(fill.hsla()),
        None => row,
    }
}

/// A row that waits for approval: the `row_pending` fill with a `row_pending_edge` outline, and the highlight line.
pub fn pending_row(theme: &Theme) -> Div {
    lit(
        theme,
        Some(theme.row_pending),
        theme.row_pending_edge,
        ROW_RADIUS,
    )
}

/// A capsule in a group of them (a tab, the round "+"): the active one is a strong [`capsule`]; the others are clear, with a clear
/// outline so that becoming active doesn't move them.
pub fn pill(theme: &Theme, active: bool) -> Div {
    if active {
        capsule(theme, true)
    } else {
        plain(FULL_RADIUS)
    }
}

/// A clear capsule: no fill, only the edge and the highlight line (Deny, the project filter).
pub fn clear_capsule(theme: &Theme) -> Div {
    lit(theme, None, theme.edge, FULL_RADIUS)
}

/// The one action that matters (Allow): an `accent` capsule with a soft glow in the accent's translucent edge color.
pub fn accent_capsule(theme: &Theme) -> Div {
    floating(
        theme,
        theme.accent,
        FULL_RADIUS,
        CAPSULE_BLUR,
        theme.row_pending_edge,
    )
}

/// How much a pressed control keeps of its look: press dims it slightly.
const PRESSED: f32 = 0.85;

/// Hover and press for a control without a fill of its own: hover raises it one step, to [`Theme::hover`]; press dims it.
pub fn hover_fill(control: Stateful<Div>, theme: &Theme) -> Stateful<Div> {
    let fill = theme.hover().hsla();
    control
        .cursor_pointer()
        .hover(move |s| s.bg(fill))
        .active(|s| s.opacity(PRESSED))
}

/// Hover and press for a control that has a fill already (selected, strong, pending or accent). In `balanced` its
/// stack is at the 0.92 alpha cap already, so hover lights its edge up to `rim` instead of adding fill. Press dims it.
pub fn hover_edge(control: Stateful<Div>, rim: Tint) -> Stateful<Div> {
    let rim = rim.hsla();
    control
        .cursor_pointer()
        .hover(move |s| s.border_color(rim))
        .active(|s| s.opacity(PRESSED))
}

/// Hover and press for a [`sheet_row`]: hover lights its edge (to `highlight` on the lit row) and gives it the lit row's fill where
/// the theme has room for one. Press dims it.
pub fn hover_sheet_row(control: Stateful<Div>, theme: &Theme, lit: bool) -> Stateful<Div> {
    let rim = if lit { theme.highlight } else { theme.edge }.hsla();
    let fill = theme.sheet_row_fill().map(Tint::hsla);
    control
        .cursor_pointer()
        .hover(move |s| {
            let s = s.border_color(rim);
            match fill {
                Some(fill) => s.bg(fill),
                None => s,
            }
        })
        .active(|s| s.opacity(PRESSED))
}

/// A surface with no fill and a clear 1 px outline, so it lines up with its lit or strong form.
fn plain(radius: f32) -> Div {
    div()
        .relative()
        .rounded(px(radius))
        .border_1()
        .border_color(Hsla::transparent_black())
}

/// A surface lit from above, with no shadow: an optional fill, a 1 px `edge` outline and the highlight line.
fn lit(theme: &Theme, fill: Option<Tint>, edge: Tint, radius: f32) -> Div {
    let surface = div()
        .relative()
        .rounded(px(radius))
        .border_1()
        .border_color(edge.hsla())
        .child(highlight(theme, radius));
    match fill {
        Some(fill) => surface.bg(fill.hsla()),
        None => surface,
    }
}

fn floating(theme: &Theme, fill: Tint, radius: f32, blur: f32, shadow_color: Tint) -> Div {
    div()
        .relative()
        .rounded(px(radius))
        .bg(fill.hsla())
        .border_1()
        .border_color(theme.edge.hsla())
        .child(highlight(theme, radius))
        .child(shadow(shadow_color, radius, blur))
}

/// The brighter 1 px line along the top, just inside the edge. GPUI has no inset shadow; a top-only border on a box of the same shape
/// follows the rounded corners and thins out along them.
fn highlight(theme: &Theme, radius: f32) -> Div {
    div()
        .absolute()
        .inset_0()
        .rounded(px(radius - 1.))
        .border_t_1()
        .border_color(theme.highlight.hsla())
}

/// The surface's soft shadow: concentric 1 px rings outside its edge, each following the rounded shape, never under the surface.
///
/// GPUI's `BoxShadow` fills its whole box, the interior included. Under a translucent fill that adds coverage (see `theme::composite`),
/// so the wallpaper would stop showing through the panel and the stacks the contrast test checks would be wrong. Masking a `BoxShadow`
/// off the interior doesn't work either: GPUI drops a shadow whose unblurred box misses its content mask, which loses every side.
/// The rings don't overlap, so no pixel is shaded twice.
///
/// ponytail: symmetric, with no drop offset. Rings moved down would reach under the fill along the top.
fn shadow(color: Tint, radius: f32, blur: f32) -> impl IntoElement {
    canvas(
        |_, _, _| {},
        move |bounds, (), window, _| {
            // The canvas fills the surface inside its 1 px edge; the rings start outside the edge.
            let b = bounds.dilate(px(EDGE));
            let r = Corners::all(px(radius))
                .clamp_radii_for_quad_size(b.size)
                .top_left;
            for (k, alpha) in rings(color.alpha, blur) {
                let out = px(k);
                let ring = Tint { alpha, ..color }.hsla();
                let clear = Hsla::transparent_black();
                window.paint_quad(quad(
                    b.dilate(out),
                    r + out,
                    clear,
                    px(1.),
                    ring,
                    BorderStyle::Solid,
                ));
            }
        },
    )
    .absolute()
    .inset_0()
}

/// The shadow's rings, from the edge outward: each ring's outer distance in px and its alpha. A Gaussian fall-off from half of
/// `alpha` at the edge (as a blurred box shadow has there) to nothing at 3 × `blur`.
fn rings(alpha: f32, blur: f32) -> impl Iterator<Item = (f32, f32)> {
    (1..=(3. * blur).ceil() as u16).map(move |k| {
        let d = f32::from(k) - 0.5;
        (
            f32::from(k),
            alpha * 0.5 * (-(d * d) / (2. * blur * blur)).exp(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{AbsoluteLength, Fill, Pixels};

    fn fill(mut d: Div) -> Option<Fill> {
        d.style().background.clone()
    }

    fn radius(mut d: Div) -> Pixels {
        match d.style().corner_radii.top_left {
            Some(AbsoluteLength::Pixels(p)) => p,
            other => panic!("radius {other:?}"),
        }
    }

    fn border(mut d: Div) -> Option<Hsla> {
        d.style().border_color
    }

    #[test]
    fn glass_helpers_use_theme_tokens() {
        let (dark, light) = (Theme::dark_glass(), Theme::light_glass());
        let token = |t: Tint| Some(Fill::from(t.hsla()));
        assert_eq!(fill(panel(&dark)), token(dark.panel));
        assert_eq!(fill(panel(&light)), token(light.panel));
        assert_ne!(fill(panel(&dark)), fill(panel(&light)));
        assert_eq!(fill(sheet(&light)), token(light.sheet));
        assert_eq!(fill(capsule(&dark, false)), token(dark.panel));
        assert_eq!(fill(capsule(&dark, true)), token(dark.panel_strong));
        assert_eq!(fill(row(&dark, true)), token(dark.panel_strong));
        assert_eq!(fill(row(&dark, false)), None);
        // A clear row has no fill: it shows by its edge.
        assert_eq!(fill(clear_row(&dark, true)), None);
        // A lit launcher row takes the mockup's fill where the sheet's stack has room for it (`clear`); in `balanced` it doesn't.
        let clear = Theme::dark_clear();
        assert_eq!(fill(sheet_row(&clear, true)), token(clear.panel_strong));
        assert_eq!(border(sheet_row(&clear, true)), Some(clear.edge.hsla()));
        assert_eq!(fill(sheet_row(&clear, false)), None);
        assert_eq!(fill(sheet_row(&dark, true)), None);
        assert_eq!(border(clear_row(&light, true)), Some(light.edge.hsla()));
        assert_eq!(
            border(clear_row(&light, false)),
            Some(Hsla::transparent_black())
        );
        assert_eq!(radius(clear_row(&dark, true)), px(16.));
        // Spec 4.1 radii; a capsule is fully rounded.
        assert_eq!(radius(panel(&dark)), px(20.));
        assert_eq!(radius(sheet(&dark)), px(28.));
        assert_eq!(radius(row(&dark, false)), px(16.));
        assert!(radius(sheet(&dark)) > radius(panel(&dark)));
        assert!(radius(capsule(&dark, false)) >= px(999.));
        // Every floating surface has the 1 px edge. A selected row's cue is more than brightness (light `panel_strong` is barely
        // brighter than `panel`): it gets the edge outline too, and an unselected row a clear one, so selecting doesn't shift it.
        assert_eq!(border(panel(&light)), Some(light.edge.hsla()));
        assert_eq!(border(capsule(&light, true)), Some(light.edge.hsla()));
        assert_eq!(border(row(&light, true)), Some(light.edge.hsla()));
        assert_eq!(border(row(&light, false)), Some(Hsla::transparent_black()));
        // A mockup padding is measured from the outer edge; inside the laid-out 1 px edge it is 1 px less.
        assert_eq!(pad(14.), px(13.));
    }

    /// The rings sit edge to edge from 1 px outside the surface (none inside it, none twice) and fade from half the shadow alpha. A
    /// panel's stop short of the tab bar, which ends (52 − 38) / 2 = 7 px above the panels.
    #[test]
    fn shadow_rings_fade_out_outside_the_edge() {
        let rings: Vec<_> = rings(0.28, PANEL_BLUR).collect();
        let bar = crate::tabs::HEIGHT + 2. * 4.;
        assert_eq!(rings.len(), 7);
        assert!(rings.len() as f32 <= (crate::workspace::TITLE_ROW - bar) / 2.);
        assert_eq!(rings[0].0, 1.);
        assert!((rings[0].1 - 0.14).abs() < 0.005, "{rings:?}");
        assert!(
            rings
                .windows(2)
                .all(|w| w[1].0 == w[0].0 + 1. && w[1].1 < w[0].1),
            "{rings:?}"
        );
        assert!(rings.last().unwrap().1 < 0.01 * 0.28, "{rings:?}");
    }
}
