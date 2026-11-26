//! Views draw with the theme's tokens, never a color literal: the palettes live in `theme.rs` only (spec 6). A literal is `rgb(0x`,
//! `rgba(0x`, or `hsla(` followed by a digit (Ruling G-R1), so `Tint::hsla()` and `rgb(theme::X)` pass. Test modules aren't exempt.

use std::fs;
use std::path::{Path, PathBuf};

fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

fn literal(line: &str) -> bool {
    line.contains("rgb(0x")
        || line.contains("rgba(0x")
        || line
            .match_indices("hsla(")
            .any(|(at, m)| line[at + m.len()..].starts_with(|c: char| c.is_ascii_digit()))
}

#[test]
fn no_literal_colors_outside_theme() {
    assert!(
        literal("rgb(0x1e2128)")
            && literal("rgba(0x00000080)")
            && literal("hsla(0.6, 0.1, 0.2, 1.)")
    );
    assert!(
        !literal("t.text.hsla()")
            && !literal("rgb(theme::chip_text(color))")
            && !literal("rgb(color)")
    );
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&src, &mut files);
    assert!(files.len() > 10, "scanned only {files:?}");
    let theme = src.join("theme.rs");
    let found: Vec<String> = files
        .iter()
        .filter(|path| **path != theme)
        .flat_map(|path| {
            let text = fs::read_to_string(path).unwrap();
            text.lines()
                .enumerate()
                .filter(|(_, line)| literal(line))
                .map(|(i, line)| format!("{}:{}: {}", path.display(), i + 1, line.trim()))
                .collect::<Vec<_>>()
        })
        .collect();
    assert!(
        found.is_empty(),
        "color literals outside theme.rs:\n{}",
        found.join("\n")
    );
}
