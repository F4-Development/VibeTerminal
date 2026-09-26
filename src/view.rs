//! Отрисовка экрана эмулятора в буфер ratatui.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};

pub fn render(screen: &vt100::Screen, area: Rect, buf: &mut Buffer) {
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
            target.set_symbol(if cell.has_contents() { cell.contents() } else { " " });
            target.set_style(style(cell));
        }
    }
}

fn style(cell: &vt100::Cell) -> Style {
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
        .fg(color(cell.fgcolor()))
        .bg(color(cell.bgcolor()))
        .add_modifier(modifier)
}

fn color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(i) => Color::Indexed(i),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
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
        render(parser.screen(), area, &mut buf);

        assert_eq!(buf[(0, 0)].symbol(), "h");
        assert_eq!(buf[(0, 0)].fg, Color::Indexed(1));
        assert!(buf[(0, 0)].modifier.contains(Modifier::BOLD));
        assert_eq!(buf[(3, 0)].symbol(), "o");
        assert_eq!(buf[(3, 0)].fg, Color::Rgb(1, 2, 3));
        assert_eq!(buf[(5, 1)].symbol(), " ");
    }

    #[test]
    fn renders_cyrillic_and_wide_chars() {
        let mut parser = vt100::Parser::new(1, 6, 0);
        parser.process("я🙂b".as_bytes());
        let area = Rect::new(0, 0, 6, 1);
        let mut buf = Buffer::empty(area);
        render(parser.screen(), area, &mut buf);

        assert_eq!(buf[(0, 0)].symbol(), "я");
        assert_eq!(buf[(1, 0)].symbol(), "🙂");
        assert_eq!(buf[(3, 0)].symbol(), "b");
    }
}
