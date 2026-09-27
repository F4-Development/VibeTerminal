//! Клавиши: перевод событий crossterm обратно в байты для терминала Claude,
//! правка строки в полях vv и работа хоткеев в русской раскладке.

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

const ESC: u8 = 0x1b;

/// `Ctrl-\` — вход в NORMAL. Без kitty-протокола терминал шлёт байт 0x1c,
/// который crossterm называет `Ctrl-4`. В раскладке «Русская (Mac)» на месте `\` стоит `ё`.
pub fn is_prefix(key: &KeyEvent) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char('\\' | '4' | 'ё' | 'Ё'))
}

/// Буквы русской раскладки → латиница на тех же клавишах, чтобы хоткеи
/// работали без переключения раскладки.
pub fn latin(code: KeyCode) -> KeyCode {
    match code {
        KeyCode::Char(c) => KeyCode::Char(ru_to_en(c).unwrap_or(c)),
        other => other,
    }
}

pub fn ru_to_en(c: char) -> Option<char> {
    const RU: &str = "йцукенгшщзхъфывапролджэячсмитьбюёЙЦУКЕНГШЩЗХЪФЫВАПРОЛДЖЭЯЧСМИТЬБЮЁ";
    const EN: &str = "qwertyuiop[]asdfghjkl;'zxcvbnm,.`QWERTYUIOP{}ASDFGHJKL:\"ZXCVBNM<>~";
    RU.chars().position(|r| r == c).and_then(|i| EN.chars().nth(i))
}

/// Байты, которые настоящий xterm отправил бы программе на это нажатие.
pub fn encode(key: &KeyEvent, application_cursor: bool) -> Vec<u8> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let shift = key.modifiers.contains(KeyModifiers::SHIFT);
    let modifier = 1 + shift as u8 + 2 * alt as u8 + 4 * ctrl as u8;
    let mut out = Vec::new();

    // ⌘ в байтах терминала не бывает: правку текста из macOS переводим в
    // клавиши Emacs, которые понимает Claude, а ⌘ с буквой ничего не
    // печатает. Выделения в поле Claude нет — с ⇧ то же самое.
    if key.modifiers.contains(KeyModifiers::SUPER) {
        match key.code {
            KeyCode::Left => return vec![0x01],
            KeyCode::Right => return vec![0x05],
            KeyCode::Backspace => return vec![0x15],
            KeyCode::Delete => return vec![0x0b],
            KeyCode::Char(_) => return out,
            _ => {}
        }
    }

    match key.code {
        KeyCode::Char(c) => {
            if alt {
                out.push(ESC);
            }
            let control = if ctrl { ctrl_byte(ru_to_en(c).unwrap_or(c)) } else { None };
            match control {
                Some(byte) => out.push(byte),
                None => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            }
        }
        KeyCode::Enter => {
            // Shift/Option+Enter — перенос строки в поле ввода Claude.
            if alt || shift {
                out.push(ESC);
            }
            out.push(b'\r');
        }
        KeyCode::Tab if shift => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Tab => out.push(b'\t'),
        KeyCode::BackTab => out.extend_from_slice(b"\x1b[Z"),
        KeyCode::Backspace => {
            if alt {
                out.push(ESC);
            }
            out.push(if ctrl { 0x08 } else { 0x7f });
        }
        KeyCode::Esc => out.push(ESC),
        KeyCode::Up => cursor_key(&mut out, b'A', modifier, application_cursor),
        KeyCode::Down => cursor_key(&mut out, b'B', modifier, application_cursor),
        KeyCode::Right => cursor_key(&mut out, b'C', modifier, application_cursor),
        KeyCode::Left => cursor_key(&mut out, b'D', modifier, application_cursor),
        KeyCode::Home => cursor_key(&mut out, b'H', modifier, application_cursor),
        KeyCode::End => cursor_key(&mut out, b'F', modifier, application_cursor),
        KeyCode::Insert => tilde_key(&mut out, 2, modifier),
        // ⌥⌦ — стереть слово справа, как в macOS. `Esc [3;3~` Claude
        // стирает до конца строки.
        KeyCode::Delete if alt && !ctrl => out.extend_from_slice(b"\x1bd"),
        KeyCode::Delete => tilde_key(&mut out, 3, modifier),
        KeyCode::PageUp => tilde_key(&mut out, 5, modifier),
        KeyCode::PageDown => tilde_key(&mut out, 6, modifier),
        KeyCode::F(n @ 1..=4) => {
            let letter = b"PQRS"[n as usize - 1];
            if modifier == 1 {
                out.extend_from_slice(&[ESC, b'O', letter]);
            } else {
                out.extend_from_slice(format!("\x1b[1;{modifier}{}", letter as char).as_bytes());
            }
        }
        KeyCode::F(n @ 5..=12) => {
            let code = [15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5];
            tilde_key(&mut out, code, modifier);
        }
        _ => {}
    }
    out
}

/// Правка в полях ввода vv — имя сессии, поиск проекта, git. Как в полях
/// macOS: ⌥⌫ стирает слово, ⌘⌫ — всё; и терминальные ⌃W, ⌃U.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Edit {
    Insert(char),
    Backspace,
    DeleteWord,
    Clear,
}

pub fn edit(key: &KeyEvent) -> Option<Edit> {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let cmd = key.modifiers.contains(KeyModifiers::SUPER);
    match key.code {
        KeyCode::Backspace if cmd => Some(Edit::Clear),
        KeyCode::Backspace if alt || ctrl => Some(Edit::DeleteWord),
        KeyCode::Backspace => Some(Edit::Backspace),
        KeyCode::Char(c) if ctrl => match ru_to_en(c).unwrap_or(c) {
            'u' => Some(Edit::Clear),
            'w' => Some(Edit::DeleteWord),
            _ => None,
        },
        // Символы с Option macOS присылает уже готовыми, без Alt.
        KeyCode::Char(c) if !alt && !cmd => Some(Edit::Insert(c)),
        _ => None,
    }
}

impl Edit {
    pub fn apply(self, text: &mut String) {
        match self {
            Edit::Insert(c) => text.push(c),
            Edit::Backspace => {
                text.pop();
            }
            Edit::DeleteWord => {
                let cut = text.trim_end().rfind(' ').map_or(0, |i| i + 1);
                text.truncate(cut);
            }
            Edit::Clear => text.clear(),
        }
    }
}

fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),
        'A'..='Z' => Some(c as u8 - b'A' + 1),
        '@' | ' ' | '2' => Some(0),
        '[' | '3' => Some(0x1b),
        '\\' | '4' => Some(0x1c),
        ']' | '5' => Some(0x1d),
        '^' | '6' => Some(0x1e),
        '_' | '-' | '7' | '/' => Some(0x1f),
        '8' | '?' => Some(0x7f),
        _ => None,
    }
}

fn cursor_key(out: &mut Vec<u8>, letter: u8, modifier: u8, application_cursor: bool) {
    if modifier > 1 {
        out.extend_from_slice(format!("\x1b[1;{modifier}{}", letter as char).as_bytes());
    } else if application_cursor {
        out.extend_from_slice(&[ESC, b'O', letter]);
    } else {
        out.extend_from_slice(&[ESC, b'[', letter]);
    }
}

fn tilde_key(out: &mut Vec<u8>, code: u8, modifier: u8) {
    if modifier > 1 {
        out.extend_from_slice(format!("\x1b[{code};{modifier}~").as_bytes());
    } else {
        out.extend_from_slice(format!("\x1b[{code}~").as_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    fn enc(code: KeyCode, modifiers: KeyModifiers) -> Vec<u8> {
        encode(&key(code, modifiers), false)
    }

    #[test]
    fn plain_and_unicode_chars() {
        assert_eq!(enc(KeyCode::Char('a'), KeyModifiers::NONE), b"a");
        assert_eq!(enc(KeyCode::Char('ж'), KeyModifiers::NONE), "ж".as_bytes());
    }

    #[test]
    fn control_chars() {
        assert_eq!(enc(KeyCode::Char('c'), KeyModifiers::CONTROL), [0x03]);
        assert_eq!(enc(KeyCode::Char('r'), KeyModifiers::CONTROL), [0x12]);
        // Ctrl+С в русской раскладке — тоже прерывание.
        assert_eq!(enc(KeyCode::Char('с'), KeyModifiers::CONTROL), [0x03]);
    }

    #[test]
    fn alt_prefixes_escape() {
        assert_eq!(enc(KeyCode::Char('b'), KeyModifiers::ALT), b"\x1bb");
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::ALT), b"\x1b\x7f");
    }

    #[test]
    fn enter_variants() {
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::NONE), b"\r");
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::SHIFT), b"\x1b\r");
    }

    #[test]
    fn arrows_respect_cursor_mode_and_modifiers() {
        assert_eq!(enc(KeyCode::Up, KeyModifiers::NONE), b"\x1b[A");
        assert_eq!(encode(&key(KeyCode::Up, KeyModifiers::NONE), true), b"\x1bOA");
        assert_eq!(enc(KeyCode::Left, KeyModifiers::ALT), b"\x1b[1;3D");
        assert_eq!(enc(KeyCode::Right, KeyModifiers::CONTROL), b"\x1b[1;5C");
    }

    #[test]
    fn tab_and_shift_tab() {
        assert_eq!(enc(KeyCode::Tab, KeyModifiers::NONE), b"\t");
        assert_eq!(enc(KeyCode::BackTab, KeyModifiers::SHIFT), b"\x1b[Z");
    }

    #[test]
    fn special_keys() {
        assert_eq!(enc(KeyCode::Delete, KeyModifiers::NONE), b"\x1b[3~");
        assert_eq!(enc(KeyCode::PageUp, KeyModifiers::SHIFT), b"\x1b[5;2~");
        assert_eq!(enc(KeyCode::F(1), KeyModifiers::NONE), b"\x1bOP");
        assert_eq!(enc(KeyCode::F(5), KeyModifiers::NONE), b"\x1b[15~");
    }

    #[test]
    fn macos_text_editing() {
        let alt_shift = KeyModifiers::ALT | KeyModifiers::SHIFT;
        let cmd_shift = KeyModifiers::SUPER | KeyModifiers::SHIFT;
        // ⌥⌫, ⌥⌦ — слово слева и справа.
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::ALT), b"\x1b\x7f");
        assert_eq!(enc(KeyCode::Backspace, alt_shift), b"\x1b\x7f");
        assert_eq!(enc(KeyCode::Delete, KeyModifiers::ALT), b"\x1bd");
        // ⌥←, ⌥⇧← — по словам.
        assert_eq!(enc(KeyCode::Left, KeyModifiers::ALT), b"\x1b[1;3D");
        assert_eq!(enc(KeyCode::Right, alt_shift), b"\x1b[1;4C");
        // ⌘ — начало и конец строки, стереть до них.
        assert_eq!(enc(KeyCode::Left, KeyModifiers::SUPER), [0x01]);
        assert_eq!(enc(KeyCode::Right, cmd_shift), [0x05]);
        assert_eq!(enc(KeyCode::Backspace, KeyModifiers::SUPER), [0x15]);
        assert_eq!(enc(KeyCode::Delete, KeyModifiers::SUPER), [0x0b]);
        // ⌘ с буквой не печатает её.
        assert_eq!(enc(KeyCode::Char('e'), KeyModifiers::SUPER), b"");
        assert_eq!(enc(KeyCode::Char('s'), cmd_shift), b"");
        assert_eq!(enc(KeyCode::Enter, KeyModifiers::SUPER), b"\r");
    }

    #[test]
    fn line_editing_in_vv_fields() {
        let edit_of = |code, modifiers| edit(&key(code, modifiers));
        assert_eq!(edit_of(KeyCode::Char('ж'), KeyModifiers::NONE), Some(Edit::Insert('ж')));
        assert_eq!(edit_of(KeyCode::Char('A'), KeyModifiers::SHIFT), Some(Edit::Insert('A')));
        assert_eq!(edit_of(KeyCode::Backspace, KeyModifiers::NONE), Some(Edit::Backspace));
        assert_eq!(edit_of(KeyCode::Backspace, KeyModifiers::ALT), Some(Edit::DeleteWord));
        assert_eq!(edit_of(KeyCode::Backspace, KeyModifiers::SUPER), Some(Edit::Clear));
        assert_eq!(edit_of(KeyCode::Char('w'), KeyModifiers::CONTROL), Some(Edit::DeleteWord));
        assert_eq!(edit_of(KeyCode::Char('г'), KeyModifiers::CONTROL), Some(Edit::Clear));
        assert_eq!(edit_of(KeyCode::Char('e'), KeyModifiers::SUPER), None);
        assert_eq!(edit_of(KeyCode::Char('b'), KeyModifiers::ALT), None);
        assert_eq!(edit_of(KeyCode::Char('c'), KeyModifiers::CONTROL), None);

        let mut text = "fix the  bug ".to_string();
        Edit::DeleteWord.apply(&mut text);
        assert_eq!(text, "fix the  ");
        Edit::DeleteWord.apply(&mut text);
        assert_eq!(text, "fix ");
        Edit::Backspace.apply(&mut text);
        Edit::Insert('!').apply(&mut text);
        assert_eq!(text, "fix!");
        Edit::DeleteWord.apply(&mut text);
        assert_eq!(text, "");
    }

    #[test]
    fn prefix_in_any_encoding() {
        assert!(is_prefix(&key(KeyCode::Char('4'), KeyModifiers::CONTROL)));
        assert!(is_prefix(&key(KeyCode::Char('\\'), KeyModifiers::CONTROL)));
        assert!(is_prefix(&key(KeyCode::Char('ё'), KeyModifiers::CONTROL)));
        assert!(!is_prefix(&key(KeyCode::Char('\\'), KeyModifiers::NONE)));
    }

    #[test]
    fn russian_layout_maps_to_latin() {
        assert_eq!(latin(KeyCode::Char('й')), KeyCode::Char('q'));
        assert_eq!(latin(KeyCode::Char('ш')), KeyCode::Char('i'));
        assert_eq!(latin(KeyCode::Char('П')), KeyCode::Char('G'));
        assert_eq!(latin(KeyCode::Char('x')), KeyCode::Char('x'));
    }
}
