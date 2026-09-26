//! Меню по `Ctrl-\`: сессии и все действия с подписанными клавишами.

use crate::session::Session;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Action {
    Select(usize),
    New,
    Rename,
    Close,
    ToggleSidebar,
    Help,
    Quit,
}

pub struct MenuItem {
    pub icon: &'static str,
    pub label: String,
    pub hotkey: Option<char>,
    pub action: Action,
    /// Перед пунктом — пустая строка (начало нового раздела).
    pub gap_before: bool,
}

pub fn items(sessions: &[Session], selected: usize, sidebar_shown: bool) -> Vec<MenuItem> {
    let mut items: Vec<MenuItem> = sessions
        .iter()
        .enumerate()
        .map(|(i, session)| MenuItem {
            icon: if i == selected { "❯" } else { " " },
            label: session.name.clone(),
            hotkey: char::from_digit(i as u32 + 1, 10).filter(|_| i < 9),
            action: Action::Select(i),
            gap_before: false,
        })
        .collect();

    let sidebar_label = if sidebar_shown { "Скрыть список сессий" } else { "Показать список сессий" };
    let actions = [
        ("+", "Новая сессия", 'n', Action::New),
        ("✎", "Переименовать сессию", 'r', Action::Rename),
        ("✕", "Закрыть сессию", 'x', Action::Close),
        ("◧", sidebar_label, 'z', Action::ToggleSidebar),
        ("?", "Как пользоваться", '?', Action::Help),
        ("↪", "Выйти из VibeTerminal", 'q', Action::Quit),
    ];
    let has_session = !sessions.is_empty();
    let actions = actions.into_iter().filter(|(_, _, _, action)| has_session || !matches!(action, Action::Rename | Action::Close));
    for (i, (icon, label, key, action)) in actions.enumerate() {
        items.push(MenuItem { icon, label: label.to_string(), hotkey: Some(key), action, gap_before: i == 0 });
    }
    items
}

pub fn by_hotkey(items: &[MenuItem], key: char) -> Option<Action> {
    let key = key.to_ascii_lowercase();
    items.iter().find(|item| item.hotkey == Some(key)).map(|item| item.action)
}
