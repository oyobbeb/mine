//! 세션이 켠 키 인코딩 모드. 바깥 터미널에 같은 모드를 걸 때 쓴다.

use alacritty_terminal::term::TermMode;

use crate::screen::Screen;

/// 바깥 터미널(Ghostty)에 건 키 인코딩 모드. 붙은 세션이 켠 모드를 따라 걸어, 키가 세션이 바란 인코딩으로 온다.
/// 그래야 Claude Code의 Shift+Enter가 CR이 아닌 인코딩된 키로 간다.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct KeyModes {
    /// DECCKM(`CSI ? 1 h`): 화살표를 `ESC O A`로 보낸다.
    pub app_cursor: bool,
    /// DECKPAM(`ESC =`): 숫자판을 응용 모드로 보낸다.
    pub app_keypad: bool,
    /// xterm modifyOtherKeys 단계(0~2).
    pub modify_other_keys: u8,
    /// kitty 키보드 플래그.
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
