//! Turns a key press into the bytes a terminal sends, in the encoding the session asked for (`KeyModes`).

use mine_core::keys::KeyModes;

/// A key press as GPUI reports it. `key` is the key name (`enter`, `up`, `a`), `key_char` the character it types.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct KeyIn {
    pub key: String,
    pub key_char: Option<String>,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub cmd: bool,
}

/// The bytes for `k` under the session's modes `m`. ⌘ combinations (app shortcuts) and keys that type a character
/// give None: characters arrive through the input method (`ime`).
///
/// ponytail: of the kitty flags only 1 (disambiguate) is honored; 2, 4, 8 and 16 are ignored in D1. Widen this when an
/// app requests them.
/// ponytail: Option types characters as macOS does by default (they take the character path). Option as Meta is a
/// later setting.
pub fn encode_key(k: &KeyIn, m: &KeyModes) -> Option<Vec<u8>> {
    if k.cmd {
        return None;
    }
    // The xterm modifier parameter: 1 + Shift 1 + Alt 2 + Ctrl 4.
    let mods = 1 + u8::from(k.shift) + 2 * u8::from(k.alt) + 4 * u8::from(k.ctrl);
    let kitty = m.kitty & 1 != 0;
    // ponytail: modifyOtherKeys 1 is treated as off.
    let mok = m.modify_other_keys >= 2;
    let coded = |code: u32, legacy: &[u8]| coded(code, legacy, mods, k.alt, kitty, mok);
    let s = match k.key.as_str() {
        "up" => letter(b'A', mods, m.app_cursor),
        "down" => letter(b'B', mods, m.app_cursor),
        "right" => letter(b'C', mods, m.app_cursor),
        "left" => letter(b'D', mods, m.app_cursor),
        "home" => letter(b'H', mods, m.app_cursor),
        "end" => letter(b'F', mods, m.app_cursor),
        "f1" => letter(b'P', mods, true),
        "f2" => letter(b'Q', mods, true),
        "f3" => letter(b'R', mods, true),
        "f4" => letter(b'S', mods, true),
        "insert" => tilde(2, mods),
        "delete" => tilde(3, mods),
        "pageup" => tilde(5, mods),
        "pagedown" => tilde(6, mods),
        "f5" => tilde(15, mods),
        "f6" => tilde(17, mods),
        "f7" => tilde(18, mods),
        "f8" => tilde(19, mods),
        "f9" => tilde(20, mods),
        "f10" => tilde(21, mods),
        "f11" => tilde(23, mods),
        "f12" => tilde(24, mods),
        "enter" => coded(13, b"\r"),
        // Shift+Tab is the one backtab every app reads, in every mode.
        "tab" if mods == 2 => b"\x1b[Z".to_vec(),
        "tab" => coded(9, b"\t"),
        "backspace" => coded(127, b"\x7f"),
        "escape" => coded(27, b"\x1b"),
        // Without Ctrl, a key types a character (Shift and Option included), which takes the input method's path.
        _ if !k.ctrl => return None,
        "space" => coded(32, b"\0"),
        key => {
            let mut chars = key.chars();
            let (Some(c), None) = (chars.next(), chars.next()) else {
                return None;
            };
            match ctrl_byte(c) {
                Some(b) => coded(u32::from(c), &[b]),
                None if kitty || mok => coded(u32::from(c), b""),
                None => return None,
            }
        }
    };
    Some(s)
}

/// A key that has a code point: Enter, Tab, Backspace, Esc, or a character with Ctrl. `legacy` is what it sends with no
/// key mode on. kitty sends Esc encoded even unmodified, Enter, Tab and Backspace only with modifiers.
fn coded(code: u32, legacy: &[u8], mods: u8, alt: bool, kitty: bool, mok: bool) -> Vec<u8> {
    match mods {
        1 if kitty && code == 27 => b"\x1b[27u".to_vec(),
        2.. if kitty => format!("\x1b[{code};{mods}u").into_bytes(),
        2.. if mok => format!("\x1b[27;{mods};{code}~").into_bytes(),
        _ if alt => [b"\x1b", legacy].concat(),
        _ => legacy.to_vec(),
    }
}

/// `ESC [ c`, or `ESC O c` in application mode, or `ESC [ 1 ; mods c` with modifiers.
fn letter(c: u8, mods: u8, app: bool) -> Vec<u8> {
    let c = char::from(c);
    match mods {
        1 if app => format!("\x1bO{c}"),
        1 => format!("\x1b[{c}"),
        _ => format!("\x1b[1;{mods}{c}"),
    }
    .into_bytes()
}

/// `ESC [ n ~`, or `ESC [ n ; mods ~` with modifiers.
fn tilde(n: u8, mods: u8) -> Vec<u8> {
    match mods {
        1 => format!("\x1b[{n}~"),
        _ => format!("\x1b[{n};{mods}~"),
    }
    .into_bytes()
}

/// The control byte Ctrl+`c` sends without a key mode.
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' | '@' | '['..='_' => Some(c as u8 & 0x1f),
        '/' => Some(0x1f),
        '?' => Some(0x7f),
        _ => None,
    }
}

/// Pasted text as sent to the session. Bracketed, it is wrapped in `ESC [200~` … `ESC [201~` with every Esc dropped, so
/// pasted text can't end the paste early and run the rest as keys. Unbracketed, line breaks go as Enter (CR).
pub fn paste_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    if bracketed {
        format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', "")).into_bytes()
    } else {
        text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A key from `ctrl+shift+a` style text. A single key without Ctrl or ⌘ also types itself.
    fn key(spec: &str) -> KeyIn {
        let mut parts: Vec<&str> = spec.split('+').collect();
        let name = parts.pop().unwrap().to_string();
        let mut k = KeyIn {
            ctrl: parts.contains(&"ctrl"),
            alt: parts.contains(&"alt"),
            shift: parts.contains(&"shift"),
            cmd: parts.contains(&"cmd"),
            ..KeyIn::default()
        };
        if name.chars().count() == 1 && !k.ctrl && !k.cmd {
            k.key_char = Some(name.clone());
        }
        k.key = name;
        k
    }

    const DEFAULT: KeyModes = KeyModes {
        app_cursor: false,
        app_keypad: false,
        modify_other_keys: 0,
        kitty: 0,
    };
    const KITTY1: KeyModes = KeyModes {
        kitty: 1,
        ..DEFAULT
    };
    const MOK2: KeyModes = KeyModes {
        modify_other_keys: 2,
        ..DEFAULT
    };
    const DECCKM: KeyModes = KeyModes {
        app_cursor: true,
        ..DEFAULT
    };

    fn enc(spec: &str, m: KeyModes) -> Option<Vec<u8>> {
        encode_key(&key(spec), &m)
    }

    fn bytes(s: &str) -> Option<Vec<u8>> {
        Some(s.as_bytes().to_vec())
    }

    #[test]
    fn encode_key_enter_and_escape_follow_the_mode() {
        assert_eq!(enc("enter", DEFAULT), bytes("\r"));
        assert_eq!(enc("shift+enter", DEFAULT), bytes("\r"));
        assert_eq!(enc("shift+enter", KITTY1), bytes("\x1b[13;2u"));
        assert_eq!(enc("shift+enter", MOK2), bytes("\x1b[27;2;13~"));
        assert_eq!(enc("escape", DEFAULT), bytes("\x1b"));
        assert_eq!(enc("escape", KITTY1), bytes("\x1b[27u"));
    }

    #[test]
    fn encode_key_ctrl_letters() {
        assert_eq!(enc("ctrl+c", DEFAULT), bytes("\x03"));
        assert_eq!(enc("ctrl+a", KITTY1), bytes("\x1b[97;5u"));
    }

    #[test]
    fn encode_key_cursor_and_editing_keys() {
        assert_eq!(enc("up", DEFAULT), bytes("\x1b[A"));
        assert_eq!(enc("up", DECCKM), bytes("\x1bOA"));
        assert_eq!(enc("shift+up", DEFAULT), bytes("\x1b[1;2A"));
        assert_eq!(enc("backspace", DEFAULT), bytes("\x7f"));
        assert_eq!(enc("tab", DEFAULT), bytes("\t"));
        assert_eq!(enc("shift+tab", DEFAULT), bytes("\x1b[Z"));
        assert_eq!(enc("home", DEFAULT), bytes("\x1b[H"));
        assert_eq!(enc("end", DEFAULT), bytes("\x1b[F"));
        assert_eq!(enc("pageup", DEFAULT), bytes("\x1b[5~"));
        assert_eq!(enc("pagedown", DEFAULT), bytes("\x1b[6~"));
        assert_eq!(enc("delete", DEFAULT), bytes("\x1b[3~"));
        assert_eq!(enc("f1", DEFAULT), bytes("\x1bOP"));
        assert_eq!(enc("f5", DEFAULT), bytes("\x1b[15~"));
    }

    #[test]
    fn encode_key_leaves_cmd_and_characters_to_others() {
        for m in [DEFAULT, KITTY1, MOK2, DECCKM] {
            assert_eq!(enc("cmd+c", m), None);
        }
        assert_eq!(enc("a", DEFAULT), None);
    }

    #[test]
    fn paste_bytes_wraps_when_bracketed() {
        assert_eq!(paste_bytes("ls\n", true), b"\x1b[200~ls\n\x1b[201~");
        // A pasted end marker can't end the paste early and run the rest as typed keys.
        assert_eq!(
            paste_bytes("a\x1b[201~rm\r", true),
            b"\x1b[200~a[201~rm\r\x1b[201~"
        );
        // Unbracketed, line breaks go as Enter (CR), like a typed key.
        assert_eq!(paste_bytes("a\r\nb\nc", false), b"a\rb\rc");
    }
}
