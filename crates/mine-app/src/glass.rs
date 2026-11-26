//! The shapes of the floating glass surfaces, so views compose them instead of repeating style chains. Each helper returns a styled
//! `Div` with its fill, edge, highlight and shadow from a [`Theme`]; the caller adds the size, the layout and the children.
//!
//! A floating surface (`panel`, `sheet`, `capsule`) carries two children of its own, painted before the caller's: the `highlight` line
//! along its top, and its shadow. Its shadow is painted outside it, so don't set `overflow_hidden` on the surface itself (that would
//! clip the shadow away); put it on a child.

use gpui::{
    BorderStyle, Corners, Div, Hsla, IntoElement, ParentElement, Pixels, Styled, canvas, div, px,
    quad,
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

/// How far each kind of shadow spreads (its Gaussian's sigma, in px; it fades out by 3 × that). A panel's is gone before it crosses
/// the 12 px gap to the next panel; a sheet's is deeper.
const PANEL_BLUR: f32 = 5.;
const SHEET_BLUR: f32 = 10.;
const CAPSULE_BLUR: f32 = 2.;

/// The task list and the terminal pane: radius 20, `panel` fill, edge and highlight, and a shadow.
pub fn panel(theme: &Theme) -> Div {
    floating(theme, theme.panel, PANEL_RADIUS, PANEL_BLUR)
}

/// The launcher and dialogs: radius 28, `sheet` fill, and a deeper shadow.
// ponytail: the launcher restyle (Task 4) is its first user.
#[allow(dead_code)]
pub fn sheet(theme: &Theme) -> Div {
    floating(theme, theme.sheet, SHEET_RADIUS, SHEET_BLUR)
}

/// A capsule (fully rounded): the tab bar, buttons. `strong` is the active or primary one (`panel_strong`); it then stands out by its
/// fill, and, in a group of plain items, by its edge, highlight and shadow too.
pub fn capsule(theme: &Theme, strong: bool) -> Div {
    let fill = if strong {
        theme.panel_strong
    } else {
        theme.panel
    };
    floating(theme, fill, FULL_RADIUS, CAPSULE_BLUR)
}

/// A list row: radius 16. Selected, it gets `panel_strong` plus the edge outline and the highlight line, because in light glass
/// `panel_strong` is barely brighter than the panel under it. Unselected, the outline is clear, so selecting doesn't move it.
// ponytail: the task list and launcher restyles (Tasks 3, 4) are its first users.
#[allow(dead_code)]
pub fn row(theme: &Theme, selected: bool) -> Div {
    let row = div().relative().rounded(px(ROW_RADIUS)).border_1();
    if selected {
        row.bg(theme.panel_strong.hsla())
            .border_color(theme.edge.hsla())
            .child(highlight(theme, ROW_RADIUS))
    } else {
        row.border_color(Hsla::transparent_black())
    }
}

fn floating(theme: &Theme, fill: Tint, radius: f32, blur: f32) -> Div {
    div()
        .relative()
        .rounded(px(radius))
        .bg(fill.hsla())
        .border_1()
        .border_color(theme.edge.hsla())
        .child(highlight(theme, radius))
        .child(shadow(theme.shadow, radius, blur))
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

    /// The rings sit edge to edge from 1 px outside the surface (none inside it, none twice) and fade from half the shadow alpha.
    #[test]
    fn shadow_rings_fade_out_outside_the_edge() {
        let rings: Vec<_> = rings(0.28, PANEL_BLUR).collect();
        assert_eq!(rings.len(), 15);
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
