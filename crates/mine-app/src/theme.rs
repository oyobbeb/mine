//! The mockup colors the user approved, and the terminal colors. Values are the `0xrrggbb` passed to GPUI `rgb()`.

use gpui::{App, Hsla, rgb};
use gpui_component::{Theme, ThemeMode};

/// Window background.
pub const WINDOW_BG: u32 = 0x1e2128;
/// Left list background.
pub const LIST_BG: u32 = 0x23252b;
/// Title bar and strips.
pub const BAR_BG: u32 = 0x1b1d23;
/// Selected row, and a clickable row while the mouse is over it.
pub const SELECTED_BG: u32 = 0x2f323a;
pub const BORDER: u32 = 0x2c2f36;
pub const TEXT: u32 = 0xd0d3da;
pub const TITLE: u32 = 0xe6e6e6;
/// Dim text (group names, second line).
pub const MUTED: u32 = 0x8a8f99;
/// Dimmer text (counts, shortcuts, hints).
pub const FAINT: u32 = 0x6b707a;
/// The waiting text of a session that needs approval.
pub const WAITING: u32 = 0xc9a85c;
/// The Needs approval symbol ◆.
pub const APPROVAL: u32 = 0xe5c07b;
/// The Working symbol ◐ and the Done symbol ✓. Same colors as the TUI list.
pub const WORKING: u32 = 0x56b6c2;
pub const DONE: u32 = 0x98c379;
/// Chip text color.
pub const CHIP_FG: u32 = 0x1e2128;

/// Default terminal text and background. On connect, the app sends these two colors to the daemon with `ToDaemon::Colors`.
pub const TERM_FG: u32 = 0xd0d3da;
pub const TERM_BG: u32 = 0x1e2128;
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

fn hsla(color: u32) -> Hsla {
    rgb(color).into()
}

/// Sets the `gpui-component` theme to the dark variant and applies the mockup colors. The colors are the same even if the system appearance is light.
pub fn install(cx: &mut App) {
    Theme::change(ThemeMode::Dark, None, cx);
    let t = Theme::global_mut(cx);
    t.background = hsla(WINDOW_BG);
    t.foreground = hsla(TEXT);
    t.border = hsla(BORDER);
    t.muted_foreground = hsla(MUTED);
    t.title_bar = hsla(BAR_BG);
    t.title_bar_border = hsla(BORDER);
    t.sidebar = hsla(LIST_BG);
    t.sidebar_border = hsla(BORDER);
    t.sidebar_foreground = hsla(TEXT);
    t.sidebar_accent = hsla(SELECTED_BG);
    t.sidebar_accent_foreground = hsla(TITLE);
    t.scrollbar = hsla(LIST_BG);
    t.scrollbar_thumb = hsla(SELECTED_BG);
    t.scrollbar_thumb_hover = hsla(BORDER);
    // Input box (the launcher's `n`): the border is the border color, the focus border is the dim text color, and the cursor is the text color.
    t.input = hsla(BORDER);
    t.ring = hsla(MUTED);
    t.caret = hsla(TEXT);
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(hex(TERM_BG), "#1e2128");
    }
}
