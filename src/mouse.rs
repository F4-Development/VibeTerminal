//! Мышь для программы внутри: если Claude сам слушает мышь (полноэкранный
//! режим), отдаём ему события в том формате, который он попросил.

use ratatui::crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// `col`, `row` — координаты внутри экрана Claude, с нуля.
pub fn encode(
    event: &MouseEvent,
    col: u16,
    row: u16,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    use MouseEventKind::*;
    let (button, release, motion) = match event.kind {
        Down(b) => (button_code(b), false, false),
        Up(b) => (button_code(b), true, false),
        Drag(b) => (button_code(b), false, true),
        Moved => (3, false, true),
        ScrollUp => (64, false, false),
        ScrollDown => (65, false, false),
        ScrollLeft => (66, false, false),
        ScrollRight => (67, false, false),
    };
    let wanted = match mode {
        MouseProtocolMode::None => false,
        MouseProtocolMode::Press => !release && !motion,
        MouseProtocolMode::PressRelease => !motion,
        MouseProtocolMode::ButtonMotion => !matches!(event.kind, Moved),
        MouseProtocolMode::AnyMotion => true,
    };
    if !wanted {
        return None;
    }

    let mut modifiers = 0;
    if event.modifiers.contains(KeyModifiers::SHIFT) {
        modifiers += 4;
    }
    if event.modifiers.contains(KeyModifiers::ALT) {
        modifiers += 8;
    }
    if event.modifiers.contains(KeyModifiers::CONTROL) {
        modifiers += 16;
    }
    let motion = if motion { 32 } else { 0 };
    let (x, y) = (u32::from(col) + 1, u32::from(row) + 1);

    match encoding {
        MouseProtocolEncoding::Sgr => {
            let code = button + motion + modifiers;
            let end = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{code};{x};{y}{end}").into_bytes())
        }
        // Старые форматы: отпускание без номера кнопки, всё смещено на 32.
        MouseProtocolEncoding::Default | MouseProtocolEncoding::Utf8 => {
            let code = if release { 3 } else { button } + motion + modifiers;
            let mut out = b"\x1b[M".to_vec();
            out.push(32 + code as u8);
            for value in [x, y] {
                let value = 32 + value;
                if encoding == MouseProtocolEncoding::Utf8 {
                    out.extend_from_slice(char::from_u32(value)?.encode_utf8(&mut [0; 4]).as_bytes());
                } else {
                    out.push(u8::try_from(value).ok()?);
                }
            }
            Some(out)
        }
    }
}

fn button_code(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(kind: MouseEventKind) -> MouseEvent {
        MouseEvent { kind, column: 0, row: 0, modifiers: KeyModifiers::NONE }
    }

    #[test]
    fn sgr_wheel_and_clicks() {
        let sgr = MouseProtocolEncoding::Sgr;
        let mode = MouseProtocolMode::AnyMotion;
        assert_eq!(encode(&event(MouseEventKind::ScrollUp), 9, 4, mode, sgr).unwrap(), b"\x1b[<64;10;5M");
        assert_eq!(
            encode(&event(MouseEventKind::Down(MouseButton::Left)), 0, 0, mode, sgr).unwrap(),
            b"\x1b[<0;1;1M"
        );
        assert_eq!(
            encode(&event(MouseEventKind::Up(MouseButton::Left)), 0, 0, mode, sgr).unwrap(),
            b"\x1b[<0;1;1m"
        );
        assert_eq!(encode(&event(MouseEventKind::Moved), 1, 1, mode, sgr).unwrap(), b"\x1b[<35;2;2M");
    }

    #[test]
    fn respects_requested_mode() {
        let sgr = MouseProtocolEncoding::Sgr;
        assert!(encode(&event(MouseEventKind::ScrollUp), 0, 0, MouseProtocolMode::None, sgr).is_none());
        assert!(encode(&event(MouseEventKind::Moved), 0, 0, MouseProtocolMode::PressRelease, sgr).is_none());
        let up = event(MouseEventKind::Up(MouseButton::Left));
        assert!(encode(&up, 0, 0, MouseProtocolMode::Press, sgr).is_none());
    }

    #[test]
    fn legacy_encoding() {
        let enc = encode(
            &event(MouseEventKind::Down(MouseButton::Right)),
            0,
            0,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Default,
        );
        assert_eq!(enc.unwrap(), b"\x1b[M\x22\x21\x21");
    }
}
