//! Раскладка экрана: строка сверху, список сессий, терминал Claude, строка
//! статуса и всплывающие окна поверх.

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use crate::app::{App, Confirm, Mode, Overlay};
use crate::picker::{Picker, display_path};
use crate::view;

pub const SIDEBAR_WIDTH: u16 = 26;
/// Каждая сессия в списке занимает две строки: имя и тема диалога.
pub const SIDEBAR_ROWS_PER_SESSION: u16 = 2;
const MIN_AGENT_WIDTH: u16 = 30;

#[derive(Clone, Copy, Default)]
pub struct Areas {
    pub top: Rect,
    pub sidebar: Option<Rect>,
    pub agent: Rect,
    pub status: Rect,
}

pub fn layout(full: Rect, show_sidebar: bool) -> Areas {
    let [top, middle, status] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(full);
    if show_sidebar && middle.width >= SIDEBAR_WIDTH + 1 + MIN_AGENT_WIDTH {
        let [sidebar, _separator, agent] = Layout::horizontal([
            Constraint::Length(SIDEBAR_WIDTH),
            Constraint::Length(1),
            Constraint::Min(1),
        ])
        .areas(middle);
        Areas { top, sidebar: Some(sidebar), agent, status }
    } else {
        Areas { top, sidebar: None, agent: middle, status }
    }
}

/// Первая видимая сессия в списке, чтобы выбранная не уехала за край.
pub fn sidebar_offset(area: Rect, selected: usize) -> usize {
    let visible = (area.height / SIDEBAR_ROWS_PER_SESSION).max(1) as usize;
    selected.saturating_sub(visible - 1)
}

pub fn draw(frame: &mut Frame, app: &App) {
    let areas = app.areas;
    let session = app.current();
    let screen = session.screen();

    draw_top(frame, areas.top, app);
    if let Some(sidebar) = areas.sidebar {
        draw_sidebar(frame, sidebar, app);
    }
    view::render(screen, areas.agent, frame.buffer_mut());
    draw_status(frame, areas.status, app);

    match &app.overlay {
        Overlay::Picker(picker) => draw_picker(frame, picker),
        Overlay::Help => draw_help(frame),
        Overlay::None if app.mode == Mode::Agent && session.scrollback() == 0 && !screen.hide_cursor() => {
            let (row, col) = screen.cursor_position();
            let agent = areas.agent;
            if row < agent.height && col < agent.width {
                frame.set_cursor_position((agent.x + col, agent.y + row));
            }
        }
        _ => {}
    }
}

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn mode_color(mode: Mode) -> Color {
    match mode {
        Mode::Agent => Color::Green,
        Mode::Normal => Color::Blue,
    }
}

fn draw_top(frame: &mut Frame, area: Rect, app: &App) {
    let session = app.current();
    let left = Line::from(vec![
        Span::styled(" vibe vim ", Style::new().add_modifier(Modifier::BOLD | Modifier::REVERSED)),
        Span::raw("  "),
        Span::styled(display_path(&session.cwd, &app.home), dim()),
    ]);
    frame.render_widget(Paragraph::new(left), area);

    let title = session.title().trim();
    if !title.is_empty() && app.areas.sidebar.is_none() {
        let right = Line::from(Span::styled(format!("{title} "), dim()));
        frame.render_widget(Paragraph::new(right).alignment(Alignment::Right), area);
    }
}

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    let offset = sidebar_offset(area, app.selected);
    let mut lines = Vec::new();
    for (i, session) in app.sessions.iter().enumerate().skip(offset) {
        let selected = i == app.selected;
        let marker = if selected {
            Span::styled("▌", Style::new().fg(mode_color(app.mode)))
        } else {
            Span::raw(" ")
        };
        let name_style = if selected { Style::new().add_modifier(Modifier::BOLD) } else { Style::new() };
        lines.push(Line::from(vec![
            marker.clone(),
            Span::styled(format!("{} ", i + 1), dim()),
            Span::styled(session.name.clone(), name_style),
        ]));
        let title = session.title().trim();
        let detail = if title.is_empty() { display_path(&session.cwd, &app.home) } else { title.to_string() };
        lines.push(Line::from(vec![marker, Span::styled(format!("  {detail}"), dim())]));
    }
    frame.render_widget(Paragraph::new(lines), area);

    // Разделитель между списком и Claude.
    let separator = Rect::new(area.right(), area.y, 1, area.height);
    let bar: Vec<Line> = (0..area.height).map(|_| Line::from(Span::styled("│", dim()))).collect();
    frame.render_widget(Paragraph::new(bar), separator);
}

fn draw_status(frame: &mut Frame, area: Rect, app: &App) {
    let badge = match app.mode {
        Mode::Agent => " AGENT ",
        Mode::Normal => " NORMAL ",
    };
    let mut spans = vec![
        Span::styled(badge, Style::new().fg(Color::Black).bg(mode_color(app.mode)).add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {} ", app.current().name), Style::new().add_modifier(Modifier::BOLD)),
    ];

    match &app.overlay {
        Overlay::Rename(input) => {
            spans.push(Span::raw(" Имя сессии: "));
            spans.push(Span::styled(input.as_str(), Style::new().add_modifier(Modifier::UNDERLINED)));
            let cursor_x = area.x + Line::from(spans.clone()).width() as u16;
            frame.set_cursor_position((cursor_x.min(area.right().saturating_sub(1)), area.y));
            spans.push(Span::styled("  Enter — сохранить · Esc — отмена", dim()));
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }
        Overlay::Confirm(confirm) => {
            let question = match confirm {
                Confirm::Close => format!(" Закрыть «{}»? Claude в ней остановится.", app.current().name),
                Confirm::Quit => format!(" Выйти из vv? Остановятся все Claude: {}.", app.sessions.len()),
            };
            spans.push(Span::styled(question, Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD)));
            spans.push(Span::styled("  y/Enter — да · n/Esc — нет", dim()));
            frame.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        }
        _ => {}
    }

    if let Some(flash) = app.flash() {
        spans.push(Span::styled(format!(" {flash}"), Style::new().fg(Color::Yellow)));
    } else if app.current().scrollback() > 0 {
        let back = if app.mode == Mode::Agent { "любая клавиша" } else { "G" };
        spans.push(Span::styled(
            format!(" ↑ история, {} строк вверх · {back} — вниз", app.current().scrollback()),
            Style::new().fg(Color::Yellow),
        ));
    }
    let left = Line::from(spans);
    let free = (area.width as usize).saturating_sub(left.width() + 2);
    frame.render_widget(Paragraph::new(left), area);

    // Подсказки справа — полные, короткие или никаких, если не влезают.
    let variants: &[&str] = match app.mode {
        Mode::Agent => &["Ctrl-\\ меню "],
        Mode::Normal => &["n новая · j/k выбор · Enter в Claude · X закрыть · ? клавиши · q выход ", "? клавиши "],
    };
    if let Some(hints) = variants.iter().find(|h| h.chars().count() <= free) {
        frame.render_widget(Paragraph::new(Line::from(Span::styled(*hints, dim()))).alignment(Alignment::Right), area);
    }
}

fn centered(frame: &Frame, width: u16, height: u16) -> Rect {
    let area = frame.area();
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(area.x + (area.width - width) / 2, area.y + (area.height - height) / 2, width, height)
}

fn draw_picker(frame: &mut Frame, picker: &Picker) {
    let area = centered(frame, 90, 24);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Новая сессия: выбери папку ")
        .title_bottom(Line::from(" Enter — открыть · Esc — отмена · ↑↓ — выбор · можно вписать путь ").centered());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.height < 2 {
        return;
    }

    let input = Line::from(vec![Span::styled("› ", Style::new().fg(Color::Green)), Span::raw(picker.query.as_str())]);
    frame.render_widget(Paragraph::new(input), Rect::new(inner.x, inner.y, inner.width, 1));
    let cursor_x = inner.x + 2 + Span::raw(picker.query.as_str()).width() as u16;
    frame.set_cursor_position((cursor_x.min(inner.right().saturating_sub(1)), inner.y));

    let list = Rect::new(inner.x, inner.y + 1, inner.width, inner.height - 1);
    if picker.len() == 0 {
        frame.render_widget(Paragraph::new(Span::styled("  ничего не нашлось", dim())), list);
        return;
    }
    let rows = list.height as usize;
    let offset = picker.selected.saturating_sub(rows.saturating_sub(1));
    for (row, i) in (offset..picker.len()).take(rows).enumerate() {
        let Some(item) = picker.item(i) else { break };
        let line_area = Rect::new(list.x, list.y + row as u16, list.width, 1);
        let style =
            if i == picker.selected { Style::new().add_modifier(Modifier::REVERSED) } else { Style::new() };
        frame.render_widget(Paragraph::new(Span::raw(format!("  {}", item.display))).style(style), line_area);
        if !item.hint.is_empty() {
            let hint = Span::styled(format!("{} ", item.hint), if i == picker.selected { style } else { dim() });
            frame.render_widget(Paragraph::new(hint).alignment(Alignment::Right), line_area);
        }
    }
}

fn draw_help(frame: &mut Frame) {
    const KEYS: &[(&str, &str)] = &[
        ("Ctrl-\\", "из Claude в меню (NORMAL)"),
        ("Enter / i / Esc", "обратно в Claude"),
        ("Ctrl-\\ Ctrl-\\", "отправить сам Ctrl-\\ в Claude"),
        ("", ""),
        ("n", "новая сессия"),
        ("j / k, ↓ / ↑", "выбрать сессию"),
        ("1 … 9", "сессия по номеру"),
        ("R", "переименовать"),
        ("X", "закрыть сессию"),
        ("z", "спрятать / показать список"),
        ("PgUp / PgDn", "листать историю"),
        ("q", "выйти из vv (все Claude остановятся)"),
        ("", ""),
        ("клик по сессии", "открыть её"),
    ];
    let area = centered(frame, 60, KEYS.len() as u16 + 4);
    frame.render_widget(Clear, area);
    let block = Block::bordered()
        .title(" Клавиши ")
        .title_bottom(Line::from(" любая клавиша — закрыть ").centered());
    let lines: Vec<Line> = KEYS
        .iter()
        .map(|(key, what)| {
            Line::from(vec![
                Span::styled(format!(" {key:<18}"), Style::new().add_modifier(Modifier::BOLD)),
                Span::raw(*what),
            ])
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).block(block), area);
}
