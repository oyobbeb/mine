//! Key-encoding modes a session has turned on. Used to apply the same modes to the outer terminal.

use alacritty_terminal::term::TermMode;

use crate::screen::Screen;

/// Key-encoding modes applied to the outer terminal (Ghostty). They follow the modes the attached session turned on, so keys arrive in the encoding the session asked for.
/// That way Claude Code's Shift+Enter is sent as an encoded key instead of CR.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KeyModes {
    /// DECCKM (`CSI ? 1 h`): send arrow keys as `ESC O A`.
    pub app_cursor: bool,
    /// DECKPAM (`ESC =`): send the numeric keypad in application mode.
    pub app_keypad: bool,
    /// xterm modifyOtherKeys level (0-2).
    pub modify_other_keys: u8,
    /// kitty keyboard flags.
    pub kitty: u8,
}

impl KeyModes {
    pub fn of(screen: &Screen) -> KeyModes {
        let mode = screen.mode();
        KeyModes {
            app_cursor: mode.contains(TermMode::APP_CURSOR),
            app_keypad: mode.contains(TermMode::APP_KEYPAD),
            modify_other_keys: screen.modify_other_keys(),
            kitty: screen.kitty_flags(),
        }
    }
}
