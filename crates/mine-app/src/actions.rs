//! App shortcuts. The table follows the plan's Global Constraints. The terminal pane binds ⌘C·⌘V (Tasks 5·6) and
//! ⌘F (Task 6). When the terminal pane has focus, every key without ⌘ goes to the session.

use gpui::{Action, KeyBinding, NoAction, actions};

actions!(
    mine,
    [
        Quit,
        NewTask,
        CloseSession,
        PrevSession,
        NextSession,
        PrevProject,
        NextProject,
        ToggleSidebar,
        Paste
    ]
);

/// ⌘1–⌘9: select a tab. The value is the 0-based tab slot.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = mine, no_json)]
pub struct SelectTab(pub usize);

/// The key context of the root view (`Workspace`) where the shortcuts apply.
pub const CONTEXT: &str = "Workspace";

/// The shortcuts the app binds. Only ⌘Q is heard everywhere; the rest are heard when focus is inside the root view.
pub fn bindings() -> Vec<KeyBinding> {
    let ws = Some(CONTEXT);
    let term = Some(crate::terminal::CONTEXT);
    let mut keys = vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-n", NewTask, ws),
        KeyBinding::new("cmd-w", CloseSession, ws),
        KeyBinding::new("cmd-[", PrevSession, ws),
        KeyBinding::new("cmd-]", NextSession, ws),
        // ⌘⇧[·⌘⇧]: macOS reports the character changed by ⇧ as the key and drops the ⇧ (`{`·`}` on a US layout).
        KeyBinding::new("cmd-{", PrevProject, ws),
        KeyBinding::new("cmd-}", NextProject, ws),
        KeyBinding::new("cmd-b", ToggleSidebar, ws),
        KeyBinding::new("cmd-v", Paste, term),
        // gpui-component's Root binds Tab and Shift+Tab to move focus, and bindings run before the pane's key-down
        // listener. NoAction in the deeper Terminal context masks them, so the session gets both keys.
        KeyBinding::new("tab", NoAction, term),
        KeyBinding::new("shift-tab", NoAction, term),
    ];
    keys.extend((1..=9).map(|n| KeyBinding::new(&format!("cmd-{n}"), SelectTab(n - 1), ws)));
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{KeyContext, Keymap, Keystroke, Modifiers};

    // Stands in for gpui-component's Root `Tab`/`TabPrev` (private), bound the same way: `tab`/`shift-tab` in "Root".
    actions!(test_root, [RootTab]);

    #[test]
    fn terminal_masks_root_tab_so_the_session_gets_it() {
        // The same order as main.rs: gpui-component's bindings first, then the app's.
        let mut keymap = Keymap::default();
        keymap.add_bindings([
            KeyBinding::new("tab", RootTab, Some("Root")),
            KeyBinding::new("shift-tab", RootTab, Some("Root")),
        ]);
        keymap.add_bindings(bindings());
        let stack = |names: &[&str]| -> Vec<KeyContext> {
            names
                .iter()
                .map(|n| KeyContext::parse(n).unwrap())
                .collect()
        };
        let pane = stack(&["Root", "Workspace", crate::terminal::CONTEXT]);
        let outside = stack(&["Root", "Workspace"]);
        for key in ["tab", "shift-tab"] {
            let input = [Keystroke::parse(key).unwrap()];
            // No binding and nothing pending: gpui's dispatch_key_event then runs the key-down listeners, so the
            // pane's listener sends the key to the session.
            let (hits, pending) = keymap.bindings_for_input(&input, &pane);
            assert!(hits.is_empty() && !pending, "{key} in the pane: {hits:?}");
            // Outside the pane (launcher, list) Root still moves focus with it.
            let (hits, _) = keymap.bindings_for_input(&input, &outside);
            assert!(hits[0].action().partial_eq(&RootTab), "{key} outside");
        }
    }

    fn table() -> Vec<(String, String)> {
        bindings()
            .iter()
            .map(|b| {
                let keys: Vec<_> = b.keystrokes().iter().map(|k| k.unparse()).collect();
                (keys.join(" "), b.action().name().to_string())
            })
            .collect()
    }

    #[test]
    fn shortcuts_follow_the_table() {
        let got = table();
        for (keys, action) in [
            ("cmd-q", "mine::Quit"),
            ("cmd-n", "mine::NewTask"),
            ("cmd-w", "mine::CloseSession"),
            ("cmd-[", "mine::PrevSession"),
            ("cmd-]", "mine::NextSession"),
            ("cmd-{", "mine::PrevProject"),
            ("cmd-}", "mine::NextProject"),
            ("cmd-b", "mine::ToggleSidebar"),
            ("cmd-v", "mine::Paste"),
        ] {
            assert!(
                got.contains(&(keys.to_string(), action.to_string())),
                "{keys} → {action}: {got:?}"
            );
        }
        // ⌘1–⌘9 select a 0-based tab slot.
        let all = bindings();
        for n in 1..=9 {
            let keys = format!("cmd-{n}");
            let b = all
                .iter()
                .find(|b| b.keystrokes()[0].unparse() == keys)
                .unwrap_or_else(|| panic!("{keys}"));
            assert!(b.action().partial_eq(&SelectTab(n - 1)), "{keys}");
        }
        // 9 shortcuts, ⌘1–⌘9, and the two Tab masks.
        assert_eq!(all.len(), 9 + 9 + 2);
    }

    #[test]
    fn cmd_shift_bracket_arrives_as_brace_and_steps_projects() {
        // macOS reports ⌘⇧[ as `{` without ⇧ (GPUI `parse_keystroke`). That key selects the previous project.
        let typed = |key: &str| Keystroke {
            modifiers: Modifiers {
                platform: true,
                ..Default::default()
            },
            key: key.into(),
            key_char: None,
        };
        let action = |key: &str| {
            bindings()
                .into_iter()
                .find(|b| b.match_keystrokes(&[typed(key)]) == Some(false))
                .map(|b| b.action().boxed_clone())
                .unwrap_or_else(|| panic!("{key}"))
        };
        assert!(action("{").partial_eq(&PrevProject));
        assert!(action("}").partial_eq(&NextProject));
        assert!(action("[").partial_eq(&PrevSession));
    }
}
