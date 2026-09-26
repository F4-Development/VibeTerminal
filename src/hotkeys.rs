//! Горячие клавиши действий vv. Сочетания выбирают в настройках VibeTerminal
//! (вкладка «Клавиши»), в vv.json (`keys`) — только изменённые, пусто —
//! выключено.
//!
//! В VibeTerminal сочетания ловит само приложение — так доходят и ⌘, которые
//! терминал программам не передаёт, — и присылает vv служебную клавишу:
//! F13–F17, потом они же с Shift, Ctrl, Option, Ctrl+Shift. Только F13–F17:
//! коды F18–F20 crossterm путает с F13–F14. Номер служебной клавиши — место
//! действия в `SLOTS`, тот же порядок в VibeHotkeys.swift. В другом
//! терминале vv сам ловит сочетания с Ctrl и Option.

use std::collections::BTreeMap;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::keys;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hotkey {
    /// Клавишу голосового ввода нажали…
    VoicePress,
    /// …и отпустили — для записи, пока держишь.
    VoiceRelease,
    Menu,
    New,
    Next,
    Previous,
    NextWaiting,
    Git,
    Usage,
    Rename,
    Close,
    /// Сессия по номеру в списке, с нуля.
    Session(usize),
}

use Hotkey::*;

/// Служебные клавиши по порядку. Менять только вместе с VibeHotkeys.swift.
const SLOTS: [Hotkey; 20] = [
    VoicePress,
    VoiceRelease,
    Menu,
    New,
    Next,
    Previous,
    NextWaiting,
    Git,
    Usage,
    Rename,
    Close,
    Session(0),
    Session(1),
    Session(2),
    Session(3),
    Session(4),
    Session(5),
    Session(6),
    Session(7),
    Session(8),
];

/// Действия с настраиваемым сочетанием: имя в `keys` и сочетание по
/// умолчанию. Сессии открываются по номеру: `sessions` — модификаторы для
/// цифр 1–9.
pub const BINDINGS: [(&str, Hotkey, &str); 10] = [
    ("voice", VoicePress, "cmd+shift+space"),
    ("menu", Menu, "ctrl+\\"),
    ("new", New, "cmd+t"),
    ("next", Next, "cmd+shift+]"),
    ("previous", Previous, "cmd+shift+["),
    ("waiting", NextWaiting, "cmd+j"),
    ("git", Git, "cmd+b"),
    ("usage", Usage, "cmd+l"),
    ("rename", Rename, "cmd+r"),
    ("close", Close, "cmd+w"),
];
pub const SESSIONS_DEFAULT: &str = "cmd";

/// Служебная клавиша от VibeTerminal → действие.
pub fn service(key: &KeyEvent) -> Option<Hotkey> {
    let KeyCode::F(n @ 13..=17) = key.code else { return None };
    let ctrl_shift = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    let row = match key.modifiers {
        KeyModifiers::NONE => 0,
        KeyModifiers::SHIFT => 1,
        KeyModifiers::CONTROL => 2,
        KeyModifiers::ALT => 3,
        mods if mods == ctrl_shift => 4,
        _ => return None,
    };
    SLOTS.get(row * 5 + usize::from(n - 13)).copied()
}

/// Сочетание действия: из настроек или по умолчанию; пусто — выключено.
pub fn binding<'a>(keys: &'a BTreeMap<String, String>, name: &str) -> &'a str {
    keys.get(name).map(String::as_str).unwrap_or_else(|| {
        if name == "sessions" {
            return SESSIONS_DEFAULT;
        }
        BINDINGS.iter().find(|(n, _, _)| *n == name).map_or("", |(_, _, default)| default)
    })
}

/// Сочетание для подсказки в меню: `⌘T`, `⌃\`, `⇧⌘Space`, `⌘1`.
pub fn label(keys: &BTreeMap<String, String>, hotkey: Hotkey) -> Option<String> {
    let binding = match hotkey {
        Session(index) if index < 9 => {
            let mods = binding(keys, "sessions");
            if mods.is_empty() {
                return None;
            }
            format!("{mods}+{}", index + 1)
        }
        _ => {
            let (name, _, _) = BINDINGS.iter().find(|(_, h, _)| *h == hotkey)?;
            binding(keys, name).to_string()
        }
    };
    let (mods, key) = split(&binding)?;
    let mut text: String = [("ctrl", "⌃"), ("alt", "⌥"), ("shift", "⇧"), ("cmd", "⌘")]
        .iter()
        .filter(|(m, _)| mods.contains(m))
        .map(|(_, glyph)| *glyph)
        .collect();
    match key {
        "space" => text.push_str("Space"),
        key => text.push_str(&key.to_uppercase()),
    }
    Some(text)
}

/// Не в VibeTerminal: сочетание из настроек, которое терминал передаёт
/// программе (с Ctrl или Option, без ⌘).
pub fn from_key(key: &KeyEvent, keys: &BTreeMap<String, String>) -> Option<Hotkey> {
    for (name, hotkey, _) in BINDINGS {
        if matches(binding(keys, name), key) {
            return Some(hotkey);
        }
    }
    let mods = binding(keys, "sessions");
    (1..=9).find(|n| matches(&format!("{mods}+{n}"), key)).map(|n| Session(n - 1))
}

fn matches(binding: &str, key: &KeyEvent) -> bool {
    let Some((mods, name)) = split(binding) else { return false };
    let ctrl = mods.contains(&"ctrl");
    let alt = mods.contains(&"alt");
    if mods.contains(&"cmd") || !(ctrl || alt) {
        return false;
    }
    if key.modifiers.contains(KeyModifiers::CONTROL) != ctrl || key.modifiers.contains(KeyModifiers::ALT) != alt {
        return false;
    }
    let wanted = match name {
        "space" => ' ',
        name => match name.chars().collect::<Vec<_>>()[..] {
            [c] => c,
            _ => return false,
        },
    };
    // Ctrl-\ терминал присылает как Ctrl-4; русская раскладка — латиница.
    match keys::latin(key.code) {
        KeyCode::Char('4') if ctrl && wanted == '\\' => true,
        KeyCode::Char(c) => c.to_ascii_lowercase() == wanted,
        _ => false,
    }
}

/// `cmd+shift+]` → (["cmd", "shift"], "]"). Клавиша `+` пишется как `cmd++`.
fn split(binding: &str) -> Option<(Vec<&str>, &str)> {
    let (mods, key) = match binding.strip_suffix("++") {
        Some(mods) => (mods, "+"),
        None => binding.rsplit_once('+')?,
    };
    (!key.is_empty()).then(|| (mods.split('+').collect(), key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn service_keys_follow_slots() {
        let f = |n, mods| service(&key(KeyCode::F(n), mods));
        assert_eq!(f(13, KeyModifiers::NONE), Some(VoicePress));
        assert_eq!(f(14, KeyModifiers::NONE), Some(VoiceRelease));
        assert_eq!(f(15, KeyModifiers::NONE), Some(Menu));
        assert_eq!(f(13, KeyModifiers::SHIFT), Some(Previous));
        assert_eq!(f(17, KeyModifiers::SHIFT), Some(Rename));
        assert_eq!(f(13, KeyModifiers::CONTROL), Some(Close));
        assert_eq!(f(14, KeyModifiers::CONTROL), Some(Session(0)));
        assert_eq!(f(17, KeyModifiers::ALT), Some(Session(8)));
        // Ctrl+Shift — запас на новые действия.
        assert_eq!(f(13, KeyModifiers::CONTROL | KeyModifiers::SHIFT), None);
        assert_eq!(f(12, KeyModifiers::NONE), None);
        assert_eq!(f(18, KeyModifiers::NONE), None);
        assert_eq!(f(13, KeyModifiers::SUPER), None);
    }

    #[test]
    fn labels_and_overrides() {
        let mut keys = BTreeMap::new();
        assert_eq!(label(&keys, New).as_deref(), Some("⌘T"));
        assert_eq!(label(&keys, VoicePress).as_deref(), Some("⇧⌘Space"));
        assert_eq!(label(&keys, Menu).as_deref(), Some("⌃\\"));
        keys.insert("new".to_string(), "ctrl+alt+n".to_string());
        keys.insert("close".to_string(), String::new());
        assert_eq!(label(&keys, New).as_deref(), Some("⌃⌥N"));
        assert_eq!(label(&keys, Close), None);
        assert_eq!(split("cmd++"), Some((vec!["cmd"], "+")));
        assert_eq!(label(&keys, Session(0)).as_deref(), Some("⌘1"));
        keys.insert("sessions".to_string(), String::new());
        assert_eq!(label(&keys, Session(0)), None);
    }

    #[test]
    fn catches_ctrl_and_option_in_other_terminals() {
        let mut keys = BTreeMap::new();
        // Ctrl-\ приходит как Ctrl-4.
        assert_eq!(from_key(&key(KeyCode::Char('4'), KeyModifiers::CONTROL), &keys), Some(Menu));
        // ⌘ до программы не доходит — без VibeTerminal не ловим.
        assert_eq!(from_key(&key(KeyCode::Char('t'), KeyModifiers::NONE), &keys), None);
        keys.insert("git".to_string(), "alt+g".to_string());
        keys.insert("sessions".to_string(), "alt".to_string());
        assert_eq!(from_key(&key(KeyCode::Char('g'), KeyModifiers::ALT), &keys), Some(Git));
        // Русская раскладка.
        assert_eq!(from_key(&key(KeyCode::Char('п'), KeyModifiers::ALT), &keys), Some(Git));
        assert_eq!(from_key(&key(KeyCode::Char('3'), KeyModifiers::ALT), &keys), Some(Session(2)));
        assert_eq!(from_key(&key(KeyCode::Char('g'), KeyModifiers::CONTROL), &keys), None);
    }
}
