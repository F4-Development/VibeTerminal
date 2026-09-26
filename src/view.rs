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
            target.set_symbol(if cell.has_contents() { cell.contents() } else { " " });
            target.set_style(style(cell, truecolor));
        }
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
