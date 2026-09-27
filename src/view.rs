//! Отрисовка экрана эмулятора в буфер ratatui.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

/// `truecolor: false` — терминал знает только 256 цветов, RGB переводим в ближайший.
pub fn render(screen: &vt100::Screen, area: Rect, buf: &mut Buffer, truecolor: bool) {
    let (rows, cols) = screen.size();
    for row in 0..area.height {
        for col in 0..area.width {
            let target = &mut buf[(area.x + col, area.y + row)];
            let cell = (row < rows && col < cols).then(|| screen.cell(row, col)).flatten();
            let Some(cell) = cell else {
                target.reset();
                continue;
            };
            if cell.is_wide_continuation() {
                // Правую половину широкого символа рисует терминал сам.
                target.reset();
                continue;
            }
            target.set_symbol(symbol(cell));
            target.set_style(style(cell, truecolor));
        }
    }
}

/// Ссылки OSC 8 с экрана Claude. ratatui их не передаёт, поэтому после
/// кадра ячейки со ссылкой печатаем ещё раз — тем же текстом и цветом, но
/// внутри OSC 8: терминал делает их ссылками (⌘-наведение, ⌘-клик). Ячейки,
/// закрытые окнами vv, не трогаем. `tag` — чтобы одинаковые номера ссылок
/// разных сессий не слились в одну. Курсор и цвет возвращаются как были.
pub fn links(screen: &vt100::Screen, area: Rect, drawn: &Buffer, truecolor: bool, tag: &str) -> Vec<u8> {
    let (rows, cols) = screen.size();
    let mut out = Vec::new();
    for row in 0..area.height.min(rows) {
        let mut open = 0;
        for col in 0..area.width.min(cols) {
            let Some(cell) = screen.cell(row, col) else { break };
            if cell.is_wide_continuation() {
                continue;
            }
            let (x, y) = (area.x + col, area.y + row);
            let target = &drawn[(x, y)];
            let expected = style(cell, truecolor);
            let visible = target.symbol() == symbol(cell)
                && target.fg == expected.fg.unwrap_or_default()
                && target.bg == expected.bg.unwrap_or_default()
                && target.modifier == expected.add_modifier;
            let link = if visible { cell.link() } else { 0 };
            let uri = screen.link_uri(link);
            if link != open {
                if open != 0 {
                    out.extend_from_slice(b"\x1b]8;;\x1b\\");
                }
                open = 0;
                if let Some(uri) = uri {
                    out.extend_from_slice(format!("\x1b[{};{}H\x1b]8;id={tag}{link};{uri}\x1b\\", y + 1, x + 1).as_bytes());
                    open = link;
                }
            }
            if open != 0 {
                out.extend_from_slice(sgr(target.fg, target.bg, target.modifier).as_bytes());
                out.extend_from_slice(target.symbol().as_bytes());
            }
        }
        if open != 0 {
            out.extend_from_slice(b"\x1b]8;;\x1b\\");
        }
    }
    if !out.is_empty() {
        out.splice(0..0, *b"\x1b7");
        out.extend_from_slice(b"\x1b[0m\x1b8");
    }
    out
}

fn symbol(cell: &vt100::Cell) -> &str {
    if cell.has_contents() { cell.contents() } else { " " }
}

/// SGR для ячейки ratatui — чтобы повторная печать выглядела как кадр.
fn sgr(fg: Color, bg: Color, modifier: Modifier) -> String {
    let mut codes = vec!["0".to_string()];
    let flags = [
        (Modifier::BOLD, "1"),
        (Modifier::DIM, "2"),
        (Modifier::ITALIC, "3"),
        (Modifier::UNDERLINED, "4"),
        (Modifier::SLOW_BLINK, "5"),
        (Modifier::REVERSED, "7"),
        (Modifier::HIDDEN, "8"),
        (Modifier::CROSSED_OUT, "9"),
    ];
    codes.extend(flags.iter().filter(|(flag, _)| modifier.contains(*flag)).map(|(_, code)| code.to_string()));
    for (color, base) in [(fg, 38), (bg, 48)] {
        match color {
            Color::Reset => {}
            Color::Rgb(r, g, b) => codes.push(format!("{base};2;{r};{g};{b}")),
            Color::Indexed(i) => codes.push(format!("{base};5;{i}")),
            named => codes.push(format!("{base};5;{}", ansi_index(named))),
        }
    }
    format!("\x1b[{}m", codes.join(";"))
}

fn ansi_index(color: Color) -> u8 {
    use Color::*;
    match color {
        Black => 0,
        Red => 1,
        Green => 2,
        Yellow => 3,
        Blue => 4,
        Magenta => 5,
        Cyan => 6,
        Gray => 7,
        DarkGray => 8,
        LightRed => 9,
        LightGreen => 10,
        LightYellow => 11,
        LightBlue => 12,
        LightMagenta => 13,
        LightCyan => 14,
        _ => 15,
    }
}

fn style(cell: &vt100::Cell, truecolor: bool) -> Style {
    let mut modifier = Modifier::empty();
    if cell.bold() {
        modifier |= Modifier::BOLD;
    }
    if cell.dim() {
        modifier |= Modifier::DIM;
    }
    if cell.italic() {
        modifier |= Modifier::ITALIC;
    }
    if cell.underline() {
        modifier |= Modifier::UNDERLINED;
    }
    if cell.inverse() {
        modifier |= Modifier::REVERSED;
    }
    Style::default()
        .fg(color(cell.fgcolor(), truecolor))
        .bg(color(cell.bgcolor(), truecolor))
        .add_modifier(modifier)
}

fn color(color: vt100::Color, truecolor: bool) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) if truecolor => Color::Rgb(r, g, b),
        vt100::Color::Rgb(r, g, b) => Color::Indexed(rgb_to_256(r, g, b)),
    }
}

/// Цвет RGB для своего интерфейса: без 24-битного цвета — ближайший из 256.
pub fn rgb(r: u8, g: u8, b: u8, truecolor: bool) -> Color {
    if truecolor { Color::Rgb(r, g, b) } else { Color::Indexed(rgb_to_256(r, g, b)) }
}

/// Ближайший цвет из стандартной палитры xterm: куб 6×6×6 или 24 оттенка серого.
fn rgb_to_256(r: u8, g: u8, b: u8) -> u8 {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    let nearest = |v: u8| (0..6).min_by_key(|&i| (LEVELS[i] as i32 - v as i32).abs()).unwrap();
    let (ri, gi, bi) = (nearest(r), nearest(g), nearest(b));
    let cube = (16 + 36 * ri + 6 * gi + bi) as u8;
    let cube_rgb = (LEVELS[ri], LEVELS[gi], LEVELS[bi]);

    let average = (r as u32 + g as u32 + b as u32) / 3;
    let gray_index = ((average.saturating_sub(8) + 5) / 10).min(23) as u8;
    let gray_level = 8 + 10 * gray_index;
    let gray = 232 + gray_index;

    let distance = |(cr, cg, cb): (u8, u8, u8)| {
        let d = |a: u8, b: u8| (a as i32 - b as i32).pow(2);
        d(cr, r) + d(cg, g) + d(cb, b)
    };
    if distance((gray_level, gray_level, gray_level)) < distance(cube_rgb) { gray } else { cube }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_text_with_colors_and_attributes() {
        let mut parser = vt100::Parser::new(2, 6, 0);
        parser.process(b"\x1b[1;31mhi\x1b[0m \x1b[38;2;1;2;3mok");
        let area = Rect::new(0, 0, 6, 2);
        let mut buf = Buffer::empty(area);
        render(parser.screen(), area, &mut buf, true);

        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(0, 0)].fg, Color::Indexed(1));
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(3, 0)].symbol(), "o");
        assert_eq!(buf[(3, 0)].fg, Color::Rgb(1, 2, 3));
        assert_eq!(buf[(5, 1)].symbol(), " ");
    }

    #[test]
    fn rgb_becomes_palette_color_without_truecolor() {
        let mut parser = vt100::Parser::new(1, 2, 0);
        parser.process(b"\x1b[38;2;255;255;255;48;2;215;119;87mx");
        let area = Rect::new(0, 0, 2, 1);
        let mut buf = Buffer::empty(area);
        render(parser.screen(), area, &mut buf, false);
        assert_eq!(buf[(0, 0)].fg, Color::Indexed(231));
        assert_eq!(buf[(0, 0)].bg, Color::Indexed(173));
    }

    const OPEN: &str = "\x1b]8;;https://example.com/a;b\x1b\\";
    const CLOSE: &str = "\x1b]8;;\x1b\\";

    #[test]
    fn screen_remembers_hyperlinks() {
        let mut parser = vt100::Parser::new(2, 20, 0);
        // Сброс цвета внутри ссылки её не обрывает; `;` в адресе — часть адреса.
        parser.process(format!("{OPEN}\x1b[94mdo\x1b[0mc{CLOSE} x").as_bytes());
        let screen = parser.screen();
        let link = screen.cell(0, 0).unwrap().link();
        assert_ne!(link, 0);
        assert_eq!(screen.link_uri(link), Some("https://example.com/a;b"));
        assert_eq!(screen.cell(0, 2).unwrap().link(), link);
        assert_eq!(screen.cell(0, 3).unwrap().link(), 0);
        assert_eq!(screen.cell(0, 4).unwrap().link(), 0);
        // Тот же адрес — тот же номер; стирание строки ссылку не разносит.
        parser.process(format!("\r\n{OPEN}y\x1b[K{CLOSE}").as_bytes());
        let screen = parser.screen();
        assert_eq!(screen.cell(1, 0).unwrap().link(), link);
        assert_eq!(screen.cell(1, 5).unwrap().link(), 0);
    }

    #[test]
    fn hyperlinks_are_printed_over_the_frame() {
        let mut parser = vt100::Parser::new(1, 8, 0);
        parser.process(format!("> {OPEN}\x1b[94mdocs{CLOSE}!").as_bytes());
        let area = Rect::new(3, 2, 8, 1);
        let mut buf = Buffer::empty(Rect::new(0, 0, 12, 4));
        render(parser.screen(), area, &mut buf, true);
        let out = String::from_utf8(links(parser.screen(), area, &buf, true, "vv7-")).unwrap();
        assert_eq!(
            out,
            "\x1b7\x1b[3;6H\x1b]8;id=vv7-1;https://example.com/a;b\x1b\\\
             \x1b[0;38;5;12md\x1b[0;38;5;12mo\x1b[0;38;5;12mc\x1b[0;38;5;12ms\
             \x1b]8;;\x1b\\\x1b[0m\x1b8"
        );
        // Ссылку закрыло окно vv — печатаем только то, что видно.
        buf[(6, 2)].set_symbol("░");
        let out = String::from_utf8(links(parser.screen(), area, &buf, true, "vv7-")).unwrap();
        assert!(out.contains("\x1b[3;8H\x1b]8;id=vv7-1;"), "{out:?}");
        assert!(!out.contains('░') && !out.contains("mo"), "{out:?}");
        // Нет ссылок — ничего не пишем.
        let mut plain = vt100::Parser::new(1, 8, 0);
        plain.process(b"text");
        render(plain.screen(), area, &mut buf, true);
        assert!(links(plain.screen(), area, &buf, true, "vv7-").is_empty());
    }

    #[test]
    fn rgb_to_256_picks_nearest() {
        assert_eq!(rgb_to_256(0, 0, 0), 16);
        assert_eq!(rgb_to_256(255, 0, 0), 196);
        assert_eq!(rgb_to_256(128, 128, 128), 244);
    }

    #[test]
    fn renders_cyrillic_and_wide_chars() {
        let mut parser = vt100::Parser::new(1, 6, 0);
        parser.process("я🙂b".as_bytes());
        let area = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(area);
        render(parser.screen(), area, &mut buf, true);

        assert_eq!(buf[(0, 0)].symbol(), "я");
        assert_eq!(buf[(1, 0)].symbol(), "🙂");
        assert_eq!(buf[(3, 0)].symbol(), "b");
    }
}
