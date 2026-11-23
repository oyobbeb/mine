//! 앱 단축키. 표는 계획의 Global Constraints를 따른다. ⌘C·⌘V(Task 5·6)와 ⌘F(Task 6)는 터미널 칸이 묶는다.
//! 터미널 칸이 포커스를 가지면 ⌘ 없는 키는 모두 세션으로 간다.

use gpui::{Action, KeyBinding, actions};

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
        ToggleSidebar
    ]
);

/// ⌘1–⌘9: 탭 고르기. 값은 0부터 센 탭 자리다.
#[derive(Clone, Debug, PartialEq, Action)]
#[action(namespace = mine, no_json)]
pub struct SelectTab(pub usize);

/// 단축키가 걸리는 뿌리 뷰(`Workspace`)의 키 문맥.
pub const CONTEXT: &str = "Workspace";

/// 앱이 묶는 단축키. ⌘Q만 어디서나 듣고, 나머지는 포커스가 뿌리 뷰 안에 있을 때 듣는다.
pub fn bindings() -> Vec<KeyBinding> {
    let ws = Some(CONTEXT);
    let mut keys = vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-n", NewTask, ws),
        KeyBinding::new("cmd-w", CloseSession, ws),
        KeyBinding::new("cmd-[", PrevSession, ws),
        KeyBinding::new("cmd-]", NextSession, ws),
        // ⌘⇧[·⌘⇧]: macOS는 ⇧로 바뀐 글자를 키로 알리고 ⇧는 뗀다(US 배열에서 `{`·`}`).
        KeyBinding::new("cmd-{", PrevProject, ws),
        KeyBinding::new("cmd-}", NextProject, ws),
        KeyBinding::new("cmd-b", ToggleSidebar, ws),
    ];
    keys.extend((1..=9).map(|n| KeyBinding::new(&format!("cmd-{n}"), SelectTab(n - 1), ws)));
    keys
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Keystroke, Modifiers};

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
        ] {
            assert!(
                got.contains(&(keys.to_string(), action.to_string())),
                "{keys} → {action}: {got:?}"
            );
        }
        // ⌘1–⌘9는 0부터 센 탭 자리를 고른다.
        let all = bindings();
        for n in 1..=9 {
            let keys = format!("cmd-{n}");
            let b = all
                .iter()
                .find(|b| b.keystrokes()[0].unparse() == keys)
                .unwrap_or_else(|| panic!("{keys}"));
            assert!(b.action().partial_eq(&SelectTab(n - 1)), "{keys}");
        }
        assert_eq!(all.len(), 8 + 9);
    }

    #[test]
    fn cmd_shift_bracket_arrives_as_brace_and_steps_projects() {
        // macOS는 ⌘⇧[를 ⇧ 없이 `{`로 알린다(GPUI `parse_keystroke`). 그 키가 이전 프로젝트를 고른다.
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
