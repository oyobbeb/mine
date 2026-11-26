//! The `[appearance]` table of `<state>/config.toml`: which palette the app draws and whether surfaces are glass.
//!
//! This is a client-side setting. The app reads the file itself; the daemon's profile parser ignores the table.
// ponytail: only the tests use most of this until the window wiring lands, so the dead-code lint is off for now.
#![allow(dead_code)]

use std::fs;
use std::io;
use std::process::Command;

use mine_core::paths::State;

/// `mode` in `[appearance]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Mode {
    /// Follow the window appearance (light or dark).
    #[default]
    System,
    Dark,
    Light,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Appearance {
    pub mode: Mode,
    /// `false` draws every surface opaque, with the same palette.
    pub glass: bool,
}

impl Default for Appearance {
    fn default() -> Self {
        Appearance {
            mode: Mode::System,
            glass: true,
        }
    }
}

/// Reads `[appearance]` from the text of `config.toml` (`None` for no file). Always returns a usable value: a bad key falls back to its
/// default and adds a warning. Warnings give the place and the reason, never a value from the file.
pub fn parse(text: Option<&str>) -> (Appearance, Vec<String>) {
    let mut appearance = Appearance::default();
    let mut warnings = Vec::new();
    let table = match text.map(str::parse::<toml::Table>) {
        None => return (appearance, warnings),
        Some(Ok(table)) => table,
        Some(Err(e)) => {
            let line = text.zip(e.span()).map_or(0, |(text, span)| {
                1 + text.as_bytes()[..span.start.min(text.len())]
                    .iter()
                    .filter(|&&b| b == b'\n')
                    .count()
            });
            let why =
                format!("config.toml line {line}: not valid TOML; using the default appearance");
            return (appearance, vec![why]);
        }
    };
    match table.get("appearance") {
        None => {}
        Some(toml::Value::Table(section)) => {
            for (key, value) in section {
                match (key.as_str(), value) {
                    ("mode", toml::Value::String(word)) if word == "system" => {}
                    ("mode", toml::Value::String(word)) if word == "dark" => {
                        appearance.mode = Mode::Dark;
                    }
                    ("mode", toml::Value::String(word)) if word == "light" => {
                        appearance.mode = Mode::Light;
                    }
                    ("mode", _) => warnings.push(
                        "[appearance] mode must be \"system\", \"dark\" or \"light\"; using system"
                            .into(),
                    ),
                    ("glass", toml::Value::Boolean(glass)) => appearance.glass = *glass,
                    ("glass", _) => {
                        warnings
                            .push("[appearance] glass must be true or false; using true".into());
                    }
                    _ => warnings.push(format!("[appearance] unknown key {key:?}, ignored")),
                }
            }
        }
        Some(_) => warnings.push("appearance must be a table ([appearance])".into()),
    }
    (appearance, warnings)
}

/// Reads `<state>/config.toml`. A missing file is the normal case and gives no warning.
pub fn load(state: &State) -> (Appearance, Vec<String>) {
    match fs::read_to_string(state.root.join("config.toml")) {
        Ok(text) => parse(Some(&text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => parse(None),
        Err(e) => (
            Appearance::default(),
            vec![format!(
                "config.toml can't be read ({}); using the default appearance",
                e.kind()
            )],
        ),
    }
}

/// Whether macOS **Reduce transparency** is on. `false` when it can't be read, so a failure never makes the app opaque.
pub fn reduce_transparency() -> bool {
    Command::new("defaults")
        .args(["read", "com.apple.universalaccess", "reduceTransparency"])
        .output()
        .is_ok_and(|out| out.status.success() && is_on(&out.stdout))
}

/// The output of `defaults read com.apple.universalaccess reduceTransparency` that means on: `1`.
fn is_on(stdout: &[u8]) -> bool {
    stdout.trim_ascii() == b"1"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_empty_appearance_gives_defaults_without_warning() {
        for text in [None, Some(""), Some("[profiles.a]\nkind = \"claude\"\n")] {
            let (a, warnings) = parse(text);
            assert_eq!(a, Appearance::default(), "{text:?}");
            assert!(warnings.is_empty(), "{text:?}: {warnings:?}");
        }
        assert_eq!(
            Appearance::default(),
            Appearance {
                mode: Mode::System,
                glass: true
            }
        );
    }

    #[test]
    fn valid_appearance_is_read() {
        let (a, warnings) = parse(Some("[appearance]\nmode = \"light\"\nglass = false\n"));
        assert_eq!(
            a,
            Appearance {
                mode: Mode::Light,
                glass: false
            }
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        for (word, mode) in [
            ("system", Mode::System),
            ("dark", Mode::Dark),
            ("light", Mode::Light),
        ] {
            let (a, warnings) = parse(Some(&format!("[appearance]\nmode = \"{word}\"\n")));
            assert_eq!((a.mode, a.glass), (mode, true));
            assert!(warnings.is_empty(), "{warnings:?}");
        }
    }

    #[test]
    fn bad_appearance_falls_back() {
        for text in [
            "[appearance]\nmode = \"blue\"\n",
            "[appearance]\nglass = \"yes\"\n",
            "[appearance]\nmode = 3\n",
            "appearance = 5\n",
            // A syntax error: the TOML as a whole can't be read.
            "[appearance]\nmode = \"dark\nglass = false\n",
        ] {
            let (a, warnings) = parse(Some(text));
            assert_eq!(a, Appearance::default(), "{text:?}");
            assert!(!warnings.is_empty(), "{text:?}");
            // A warning is the place and the reason: it never quotes a value.
            for w in &warnings {
                assert!(!w.contains("blue") && !w.contains("yes"), "{text:?}: {w}");
            }
        }
        // One bad key doesn't throw away a good one.
        let (a, warnings) = parse(Some("[appearance]\nmode = \"dark\"\nglass = \"yes\"\n"));
        assert_eq!(
            a,
            Appearance {
                mode: Mode::Dark,
                glass: true
            }
        );
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn load_reads_the_state_config() {
        let root = std::env::temp_dir().join(format!("mine-appearance-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let state = State { root: root.clone() };
        // No file is the normal case.
        let (a, warnings) = load(&state);
        assert_eq!(a, Appearance::default());
        assert!(warnings.is_empty(), "{warnings:?}");
        fs::write(root.join("config.toml"), "[appearance]\nmode = \"dark\"\n").unwrap();
        assert_eq!(load(&state).0.mode, Mode::Dark);
        // A config that can't be read is a warning, not a silent fallback.
        fs::remove_file(root.join("config.toml")).unwrap();
        fs::create_dir(root.join("config.toml")).unwrap();
        let (a, warnings) = load(&state);
        assert_eq!(a, Appearance::default());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn only_a_one_means_reduce_transparency_is_on() {
        assert!(is_on(b"1\n"));
        assert!(is_on(b"1"));
        for off in [&b"0\n"[..], b"", b"2\n", b"true\n", b"10\n"] {
            assert!(!is_on(off), "{off:?}");
        }
    }
}
