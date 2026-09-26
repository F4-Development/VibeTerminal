//! Раскладка экрана: строка сверху, терминал Claude, строка статуса.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::{App, Mode};
use crate::view;

fn split(full: Rect) -> [Rect; 3] {
    Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(full)
}

/// Место под терминал Claude при данном размере окна.
pub fn agent_area(full: Rect) -> Rect {
    split(full)[1]
}

pub fn draw(frame: &mut Frame, app: &App) {
    let [top, agent, bottom] = split(frame.area());
    let screen = app.session.screen();

    draw_top(frame, top, app);
    view::render(screen, agent, frame.buffer_mut());
    draw_status(frame, bottom, app);

    let at_bottom = app.session.scrollback() == 0;
    if app.mode == Mode::Agent && at_bottom && !screen.hide_cursor() {
        let (row, col) = screen.cursor_position();
        if row < agent.height && col < agent.width {
            frame.set_cursor_position((agent.x + col, agent.y + row));
        }
    }
}

fn draw_top(frame: &mut Frame, area: Rect, app: &App) {
    let left = Line::from(vec![
        Span::styled(" vibe vim ", Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)),
        Span::raw("  "),
        Span::styled(app.cwd.as_str(), Style::new().add_modifier(Modifier::DIM)),
    ]);
    frame.render_widget(Paragraph::new(left), area);

    let title = app.session.title().trim();
    if !title.is_empty() {
        let right = Line::from(Span::styled(format!("{title} "), Style::new().add_modifier(Modifier::DIM)));
        frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
    }
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let (badge, badge_color, hints) = match app.mode {
        Mode::Agent => (" AGENT ", Color::Green, "Ctrl-\\ меню "),
        Mode::Normal => (
            " NORMAL ",
            Color::Blue,
            "q выйти · Enter назад · PgUp/PgDn листать · Ctrl-\\ отправить Ctrl-\\ в Claude ",
        ),
    };

    let mut spans = vec![Span::styled(badge, Style::new().fg(Color::Black).bg(badge_color).bold())];
    let scrolled = app.session.scrollback();
    if scrolled > 0 {
        let back = if app.mode == Mode::Agent { "любая клавиша" } else { "G" };
        spans.push(Span::styled(
            format!("  ↑ история, {scrolled} строк вверх · {back} — вниз"),
            Style::new().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(hints, Style::new().add_modifier(Modifier::DIM))))
            .alignment(Alignment::Right),
        area,
    );
}
