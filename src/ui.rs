//! Экран: шапка, список сессий, окно Claude, кнопки внизу и всплывающие
//! окна. Здесь же геометрия кнопок — по ней app понимает, куда кликнули.

use std::path::Path;

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Clear, Paragraph};

use crate::app::{App, Confirm, Overlay};
use crate::hooks::{self, Decision};
use crate::menu::{self, Action, MenuItem};
use crate::picker::{Picker, display_path};
use crate::session::Session;
use crate::view;

/// Цвет Claude — работает и в тёмной, и в светлой теме терминала.
const ACCENT: Color = Color::Indexed(173);
const BORDER: Color = Color::DarkGray;
const SIDEBAR_WIDTH: u16 = 28;
const MIN_AGENT_WIDTH: u16 = 40;
/// Карточка сессии: имя, тема диалога, пустая строка.
const CARD_ROWS: u16 = 3;
const MENU_WIDTH: u16 = 46;
const DIALOG_SIZE: (u16, u16) = (58, 7);
const PICKER_SIZE: (u16, u16) = (90, 24);

const MENU_BUTTON: &str = "☰ Меню  Ctrl-\\ ";
const NEW_BUTTON: &str = "+ Новая сессия";
/// Кнопки внизу: значок цветом Claude и подпись.
const BOTTOM_BUTTONS: [(&str, &str, Action); 5] = [
    ("+", "Новая сессия", Action::New),
    ("✎", "Имя", Action::Rename),
    ("✕", "Закрыть", Action::Close),
    ("?", "Помощь", Action::Help),
    ("↪", "Выход", Action::Quit),
];
const BUTTON_GAP: u16 = 4;

pub struct Button {
    pub rect: Rect,
    pub icon: &'static str,
    pub label: &'static str,
    pub action: Action,
}

#[derive(Clone, Copy, Default)]
pub struct Areas {
    pub full: Rect,
    pub top: Rect,
    /// Панель со списком сессий, вместе с рамкой.
    pub sidebar: Option<Rect>,
    /// Окно Claude вместе с рамкой.
    pub agent_frame: Rect,
    /// Сам терминал Claude внутри рамки.
    pub agent: Rect,
    pub bottom: Rect,
}

pub fn layout(full: Rect, show_sidebar: bool) -> Areas {
    let [top, middle, bottom] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(3), Constraint::Length(1)]).areas(full);
    let (sidebar, agent_frame) = if show_sidebar && middle.width >= SIDEBAR_WIDTH + MIN_AGENT_WIDTH {
        let [sidebar, agent] =
            Layout::horizontal([Constraint::Length(SIDEBAR_WIDTH), Constraint::Min(1)]).areas(middle);
        (Some(sidebar), agent)
    } else {
        (None, middle)
    };
    Areas { full, top, sidebar, agent_frame, agent: inner(agent_frame), bottom }
}

fn inner(area: Rect) -> Rect {
    Block::bordered().inner(area)
}

pub fn contains(area: Rect, column: u16, row: u16) -> bool {
    column >= area.x && column < area.right() && row >= area.y && row < area.bottom()
}

fn width(text: &str) -> u16 {
    Span::raw(text).width() as u16
}

fn centered(full: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(full.width.saturating_sub(2));
    let height = height.min(full.height.saturating_sub(2));
    Rect::new(full.x + (full.width - width) / 2, full.y + (full.height - height) / 2, width, height)
}

// ── Геометрия кликабельного ───────────────────────────────────────────────

/// Всё, на что можно нажать мышью.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    MenuButton,
    NewSession,
    Card(usize),
    Bottom(Action),
    MenuItem(usize),
    PickerItem(usize),
    DialogYes,
    DialogNo,
    /// Ответ на запрос разрешения сессии с этим номером.
    Permit(usize, Decision),
}

/// Что под мышью. Пока открыто окно, кликается только оно.
pub fn target_at(app: &App, column: u16, row: u16) -> Option<Target> {
    let full = app.areas.full;
    match &app.overlay {
        Overlay::Help => None,
        Overlay::Menu(_) => {
            let items = menu::items(&app.sessions, app.selected, app.areas.sidebar.is_some());
            menu_rows(full, &items).iter().position(|r| contains(*r, column, row)).map(Target::MenuItem)
        }
        Overlay::Picker(picker) => {
            let list = picker_list(full);
            let index = picker_offset(list, picker.selected) + row.checked_sub(list.y)? as usize;
            (contains(list, column, row) && index < picker.len()).then_some(Target::PickerItem(index))
        }
        Overlay::Rename(_) | Overlay::Confirm(_) => {
            let [yes, no] = dialog_buttons(full, dialog_labels(&app.overlay));
            if contains(yes, column, row) {
                Some(Target::DialogYes)
            } else {
                contains(no, column, row).then_some(Target::DialogNo)
            }
        }
        Overlay::None => {
            if let Some(bar) = permit_bar(app) {
                if let Some((_, _, decision)) = bar.buttons.iter().find(|(r, _, _)| contains(*r, column, row)) {
                    return Some(Target::Permit(app.selected, *decision));
                }
            } else if let Some(button) = bottom_buttons(&app.areas).into_iter().find(|b| contains(b.rect, column, row)) {
                return Some(Target::Bottom(button.action));
            }
            if contains(menu_button(app.areas.top), column, row) {
                return Some(Target::MenuButton);
            }
            let (cards, new_button) = sidebar_parts(app.areas.sidebar?);
            if contains(new_button, column, row) {
                return Some(Target::NewSession);
            }
            let offset = cards_offset(cards, app.selected);
            let index = card_at(cards, offset, column, row).filter(|&i| i < app.sessions.len())?;
            if !app.sessions[index].permissions.is_empty() {
                let [yes, no] = card_buttons(cards, offset, index);
                if contains(yes, column, row) {
                    return Some(Target::Permit(index, Decision::Allow));
                }
                if contains(no, column, row) {
                    return Some(Target::Permit(index, Decision::Deny));
                }
            }
            Some(Target::Card(index))
        }
    }
}

const PERMIT_BUTTONS: [(&str, Decision); 2] = [(" ✓ Разрешить ", Decision::Allow), (" ✕ Отклонить ", Decision::Deny)];
const CARD_YES: &str = "✓ Да";
const CARD_NO: &str = "✕ Нет";

/// Нижняя строка, пока Claude в открытой сессии ждёт разрешения: что он
/// просит и кнопки. Окно Claude не закрываем — там его родной диалог.
pub struct PermitBar {
    pub text: String,
    pub buttons: Vec<(Rect, &'static str, Decision)>,
}

pub fn permit_bar(app: &App) -> Option<PermitBar> {
    let session = app.current();
    let request = session.pending_permission()?;
    let (what, detail) = hooks::describe(&request.tool, &request.input, &session.cwd);
    let detail = detail.lines().next().unwrap_or_default();
    let bottom = app.areas.bottom;
    let mut x = bottom.right();
    let mut buttons = Vec::new();
    for (label, decision) in PERMIT_BUTTONS.iter().rev() {
        let w = width(label);
        x = x.saturating_sub(w + 1);
        buttons.push((Rect::new(x, bottom.y, w, 1), *label, *decision));
    }
    buttons.reverse();
    Some(PermitBar { text: format!(" ! Claude просит: {what}: {detail}"), buttons })
}

/// Кнопки «Да / Нет» в третьей строке карточки ждущей сессии.
pub fn card_buttons(cards: Rect, offset: usize, index: usize) -> [Rect; 2] {
    let y = cards.y + (index - offset) as u16 * CARD_ROWS + 2;
    let yes = Rect::new(cards.x + 2, y, width(CARD_YES), 1);
    let no = Rect::new(yes.right() + 3, y, width(CARD_NO), 1);
    [yes, no]
}

pub fn menu_button(top: Rect) -> Rect {
    let w = width(MENU_BUTTON).min(top.width);
    Rect::new(top.right() - w, top.y, w, 1)
}

/// Карточки сессий и кнопка «+ Новая сессия» внутри панели.
pub fn sidebar_parts(sidebar: Rect) -> (Rect, Rect) {
    let inner = inner(sidebar);
    let w = width(NEW_BUTTON).min(inner.width);
    let button = Rect::new(inner.x + (inner.width - w) / 2, inner.bottom().saturating_sub(1), w, inner.height.min(1));
    let cards = Rect::new(inner.x, inner.y, inner.width, inner.height.saturating_sub(2));
    (cards, button)
}

/// Первая видимая карточка, чтобы выбранная не уехала за край.
pub fn cards_offset(cards: Rect, selected: usize) -> usize {
    let visible = (cards.height / CARD_ROWS).max(1) as usize;
    selected.saturating_sub(visible - 1)
}

pub fn card_at(cards: Rect, offset: usize, column: u16, row: u16) -> Option<usize> {
    contains(cards, column, row).then(|| offset + ((row - cards.y) / CARD_ROWS) as usize)
}

/// Кнопки внизу. «Новая сессия» здесь, только когда спрятан список, где она уже есть.
pub fn bottom_buttons(areas: &Areas) -> Vec<Button> {
    let bottom = areas.bottom;
    let mut x = bottom.x + 1;
    let mut out = Vec::new();
    let skip_new = areas.sidebar.is_some() as usize;
    for (icon, label, action) in BOTTOM_BUTTONS.into_iter().skip(skip_new) {
        let w = width(icon) + 1 + width(label);
        if x + w > bottom.right() {
            break;
        }
        out.push(Button { rect: Rect::new(x, bottom.y, w, 1), icon, label, action });
        x += w + BUTTON_GAP;
    }
    out
}

fn menu_rect(full: Rect, items: &[MenuItem]) -> Rect {
    let gaps = items.iter().filter(|i| i.gap_before).count() as u16;
    // Рамка, заголовок «Сессии», пункты и пустые строки между разделами.
    centered(full, MENU_WIDTH, 2 + 1 + items.len() as u16 + gaps)
}

pub fn menu_rows(full: Rect, items: &[MenuItem]) -> Vec<Rect> {
    let area = inner(menu_rect(full, items));
    let mut y = area.y + 1;
    let mut rows = Vec::new();
    for item in items {
        if item.gap_before {
            y += 1;
        }
        if y >= area.bottom() {
            break;
        }
        rows.push(Rect::new(area.x, y, area.width, 1));
        y += 1;
    }
    rows
}

pub fn menu_contains(full: Rect, items: &[MenuItem], column: u16, row: u16) -> bool {
    contains(menu_rect(full, items), column, row)
}

fn dialog_rect(full: Rect) -> Rect {
    centered(full, DIALOG_SIZE.0, DIALOG_SIZE.1)
}

/// Подписи кнопок диалога: первая — «да», вторая — «отмена».
pub fn dialog_labels(overlay: &Overlay) -> [&'static str; 2] {
    match overlay {
        Overlay::Rename(_) => [" Сохранить ", " Отмена "],
        Overlay::Confirm(Confirm::Quit) => [" Да, выйти ", " Отмена "],
        _ => [" Да, закрыть ", " Отмена "],
    }
}

pub fn dialog_buttons(full: Rect, labels: [&str; 2]) -> [Rect; 2] {
    let area = inner(dialog_rect(full));
    let (w0, w1) = (width(labels[0]), width(labels[1]));
    let x0 = area.x + area.width.saturating_sub(w0 + 3 + w1) / 2;
    let y = area.bottom().saturating_sub(1);
    [Rect::new(x0, y, w0, 1), Rect::new(x0 + w0 + 3, y, w1, 1)]
}

fn picker_rect(full: Rect) -> Rect {
    centered(full, PICKER_SIZE.0, PICKER_SIZE.1)
}

/// Строки со списком папок: под строкой поиска и разделителем.
pub fn picker_list(full: Rect) -> Rect {
    let area = inner(picker_rect(full));
    Rect::new(area.x, area.y + 2, area.width, area.height.saturating_sub(2))
}

pub fn picker_offset(list: Rect, selected: usize) -> usize {
    selected.saturating_sub((list.height as usize).saturating_sub(1))
}

pub fn picker_contains(full: Rect, column: u16, row: u16) -> bool {
    contains(picker_rect(full), column, row)
}

// ── Отрисовка ─────────────────────────────────────────────────────────────

fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}

fn primary() -> Style {
    Style::new().fg(Color::Indexed(16)).bg(ACCENT).add_modifier(Modifier::BOLD)
}

fn hovered(app: &App, target: Target) -> bool {
    app.hover == Some(target)
}

fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}

fn accent() -> Style {
    Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
}

fn frame_block(title: &str, focused: bool) -> Block<'_> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(if focused { ACCENT } else { BORDER }))
        .title(Span::styled(title, Style::new().add_modifier(Modifier::BOLD)))
}

/// Вторая строка карточки: тема диалога, а пока её нет — папка.
fn session_detail(session: &Session, home: &Path) -> String {
    let title = session.title().trim();
    let text = title.trim_start_matches(|c: char| !c.is_alphanumeric()).trim();
    if text.is_empty() || text == "Claude Code" {
        display_path(&session.cwd, home)
    } else {
        title.to_string()
    }
}

pub fn draw(frame: &mut Frame, app: &App) {
    draw_top(frame, app);
    if let Some(sidebar) = app.areas.sidebar {
        draw_sidebar(frame, sidebar, app);
    }
    draw_agent(frame, app);
    draw_bottom(frame, app);

    let full = app.areas.full;
    match &app.overlay {
        Overlay::None => {
            let session = app.current();
            let screen = session.screen();
            let agent = app.areas.agent;
            let (row, col) = screen.cursor_position();
            if session.scrollback() == 0 && !screen.hide_cursor() && row < agent.height && col < agent.width {
                frame.set_cursor_position((agent.x + col, agent.y + row));
            }
        }
        Overlay::Menu(cursor) => draw_menu(frame, app, *cursor),
        Overlay::Picker(picker) => draw_picker(frame, full, picker),
        Overlay::Rename(input) => {
            let lines = vec![Line::raw("Новое имя сессии:"), Line::raw("")];
            let text = draw_dialog(frame, app, " Переименовать ", lines);
            let field = Rect::new(text.x, text.y + 1, text.width, 1);
            frame.render_widget(
                Paragraph::new(Span::styled(format!("{input} "), Style::new().add_modifier(Modifier::UNDERLINED))),
                field,
            );
            let x = field.x + width(input);
            frame.set_cursor_position((x.min(field.right().saturating_sub(1)), field.y));
        }
        Overlay::Confirm(confirm) => {
            let (title, text) = match confirm {
                Confirm::Close => (" Закрыть сессию? ", format!("Claude в «{}» остановится.", app.current().name)),
                Confirm::Quit => (
                    " Выйти из Vibe Vim? ",
                    match app.sessions.len() {
                        1 => "Claude остановится.".to_string(),
                        n => format!("Остановятся все Claude: {n}."),
                    },
                ),
            };
            draw_dialog(frame, app, title, vec![Line::raw(text)]);
        }
        Overlay::Help => draw_help(frame, full),
    }
}

fn draw_top(frame: &mut Frame, app: &App) {
    let area = app.areas.top;
    let logo = Span::styled(" ✻ Vibe Vim ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));
    frame.render_widget(Paragraph::new(logo), area);
    let open = matches!(app.overlay, Overlay::Menu(_));
    let button = if open || hovered(app, Target::MenuButton) {
        Line::from(Span::styled(MENU_BUTTON, primary()))
    } else {
        Line::from(vec![Span::styled("☰", accent()), Span::raw(" Меню  "), Span::styled("Ctrl-\\ ", dim())])
    };
    frame.render_widget(Paragraph::new(button), menu_button(area));
}

fn draw_sidebar(frame: &mut Frame, area: Rect, app: &App) {
    frame.render_widget(frame_block(" Сессии ", false), area);
    let (cards, button) = sidebar_parts(area);
    let offset = cards_offset(cards, app.selected);
    for (i, session) in app.sessions.iter().enumerate().skip(offset) {
        let y = cards.y + (i - offset) as u16 * CARD_ROWS;
        if y + 1 >= cards.bottom() {
            break;
        }
        let name = if i != app.selected && hovered(app, Target::Card(i)) {
            Line::from(vec![
                Span::styled("› ", Style::new().fg(ACCENT)),
                Span::styled(session.name.as_str(), Style::new().fg(ACCENT)),
            ])
        } else if i == app.selected {
            Line::from(vec![
                Span::styled("❯ ", Style::new().fg(ACCENT)),
                Span::styled(session.name.as_str(), Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)),
            ])
        } else {
            Line::from(vec![Span::raw("  "), Span::raw(session.name.as_str())])
        };
        if session.permissions.is_empty() {
            let detail = Line::from(Span::styled(format!("  {}", session_detail(session, &app.home)), dim()));
            frame.render_widget(Paragraph::new(vec![name, detail]), Rect::new(cards.x, y, cards.width, 2));
            continue;
        }
        let waiting = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let detail = Line::from(Span::styled("  ! ждёт разрешения", waiting));
        frame.render_widget(Paragraph::new(vec![name, detail]), Rect::new(cards.x, y, cards.width, 2));
        let [yes, no] = card_buttons(cards, offset, i);
        if yes.bottom() <= cards.bottom() {
            let yes_style = if hovered(app, Target::Permit(i, Decision::Allow)) { primary() } else { accent() };
            let no_style = if hovered(app, Target::Permit(i, Decision::Deny)) { primary() } else { bold() };
            frame.render_widget(Paragraph::new(Span::styled(CARD_YES, yes_style)), yes);
            frame.render_widget(Paragraph::new(Span::styled(CARD_NO, no_style)), no);
        }
    }
    let style = if hovered(app, Target::NewSession) { primary() } else { accent() };
    frame.render_widget(Paragraph::new(Span::styled(NEW_BUTTON, style)), button);
}

fn draw_agent(frame: &mut Frame, app: &App) {
    let session = app.current();
    let path = display_path(&session.cwd, &app.home);
    let title = Line::from(vec![
        Span::styled(format!(" {} ", session.name), Style::new().add_modifier(Modifier::BOLD)),
        Span::styled(format!("· {path} "), dim()),
    ]);
    let mut block = frame_block("", matches!(app.overlay, Overlay::None)).title(title);
    let detail = session_detail(session, &app.home);
    if app.areas.sidebar.is_none() && detail != path {
        block = block.title(Line::from(Span::styled(format!(" {detail} "), dim())).right_aligned());
    }
    frame.render_widget(block, app.areas.agent_frame);
    view::render(session.screen(), app.areas.agent, frame.buffer_mut(), app.caps.truecolor);
}

fn draw_bottom(frame: &mut Frame, app: &App) {
    let area = app.areas.bottom;
    if let Some(bar) = permit_bar(app) {
        let text_width = bar.buttons.first().map_or(area.width, |(r, _, _)| r.x.saturating_sub(area.x + 1));
        let waiting = Style::new().fg(Color::Yellow).add_modifier(Modifier::BOLD);
        let text = Rect::new(area.x, area.y, text_width, 1);
        frame.render_widget(Paragraph::new(Span::styled(bar.text.as_str(), waiting)), text);
        for (rect, label, decision) in &bar.buttons {
            let hover = hovered(app, Target::Permit(app.selected, *decision));
            let style = if hover || *decision == Decision::Allow { primary() } else { bold() };
            frame.render_widget(Paragraph::new(Span::styled(*label, style)), *rect);
        }
        return;
    }
    let buttons = bottom_buttons(&app.areas);
    for button in &buttons {
        let line = if hovered(app, Target::Bottom(button.action)) {
            Line::from(Span::styled(format!("{} {}", button.icon, button.label), primary()))
        } else {
            Line::from(vec![Span::styled(button.icon, accent()), Span::raw(format!(" {}", button.label))])
        };
        frame.render_widget(Paragraph::new(line), button.rect);
    }

    let note = app.flash().map(str::to_string).or_else(|| {
        let scrolled = app.current().scrollback();
        (scrolled > 0).then(|| format!("↑ история, {scrolled} строк вверх · любая клавиша — вниз"))
    });
    if let Some(note) = note {
        let used = buttons.last().map_or(area.x, |b| b.rect.right());
        let free = area.right().saturating_sub(used + 2);
        if width(&note) <= free {
            let line = Line::from(Span::styled(format!("{note} "), Style::new().fg(Color::Yellow)));
            frame.render_widget(Paragraph::new(line).alignment(Alignment::Right), area);
        }
    }
}

fn draw_menu(frame: &mut Frame, app: &App, cursor: usize) {
    let full = app.areas.full;
    let items = menu::items(&app.sessions, app.selected, app.areas.sidebar.is_some());
    let area = menu_rect(full, &items);
    frame.render_widget(Clear, area);
    let block =
        frame_block(" Меню ", true).title_bottom(Line::from(" ↑↓ и Enter — выбрать · Esc — закрыть ").centered());
    let content = block.inner(area);
    frame.render_widget(block, area);
    let header = Rect::new(content.x, content.y, content.width, 1);
    frame.render_widget(Paragraph::new(Span::styled(" Сессии", dim())), header);

    for (i, (item, row)) in items.iter().zip(menu_rows(full, &items)).enumerate() {
        let style = if i == cursor { primary() } else { Style::new() };
        let is_session = matches!(item.action, Action::Select(_));
        let icon_style = if i != cursor && is_session { style.fg(ACCENT) } else { style };
        let line = Line::from(vec![
            Span::styled(format!(" {} ", item.icon), icon_style),
            Span::styled(format!(" {}", item.label), style),
        ]);
        frame.render_widget(Paragraph::new(line).style(style), row);
        if let Some(key) = item.hotkey {
            let hint = Span::styled(format!("{} ", key.to_uppercase()), if i == cursor { style } else { dim() });
            frame.render_widget(Paragraph::new(hint).alignment(Alignment::Right), row);
        }
    }
}

/// Рисует диалог и возвращает область для текста (под поле ввода).
fn draw_dialog(frame: &mut Frame, app: &App, title: &str, lines: Vec<Line>) -> Rect {
    let full = app.areas.full;
    let labels = dialog_labels(&app.overlay);
    let area = dialog_rect(full);
    frame.render_widget(Clear, area);
    let block = frame_block(title, true);
    let content = block.inner(area);
    frame.render_widget(block, area);
    let text = Rect::new(content.x + 1, content.y, content.width.saturating_sub(2), content.height.saturating_sub(1));
    frame.render_widget(Paragraph::new(lines), text);
    let [yes, no] = dialog_buttons(full, labels);
    frame.render_widget(Paragraph::new(Span::styled(labels[0], primary())), yes);
    let no_style = if hovered(app, Target::DialogNo) { primary() } else { Style::new().add_modifier(Modifier::BOLD) };
    frame.render_widget(Paragraph::new(Span::styled(labels[1], no_style)), no);
    text
}

fn draw_picker(frame: &mut Frame, full: Rect, picker: &Picker) {
    let area = picker_rect(full);
    frame.render_widget(Clear, area);
    let block = frame_block(" Новая сессия: в какой папке? ", true)
        .title_bottom(Line::from(" Enter — открыть · Esc — отмена · ↑↓ — выбор ").centered());
    let content = block.inner(area);
    frame.render_widget(block, area);
    if content.height < 3 {
        return;
    }

    let search = Rect::new(content.x, content.y, content.width, 1);
    let prompt = Span::styled(" Поиск: ", Style::new().fg(ACCENT).add_modifier(Modifier::BOLD));
    let query = if picker.query.is_empty() {
        Span::styled("начни печатать название проекта или путь", dim())
    } else {
        Span::raw(picker.query.as_str())
    };
    let cursor_x = search.x + prompt.width() as u16 + width(&picker.query);
    frame.render_widget(Paragraph::new(Line::from(vec![prompt, query])), search);
    frame.set_cursor_position((cursor_x.min(search.right().saturating_sub(1)), search.y));
    let rule = Span::styled("─".repeat(content.width as usize), Style::new().fg(BORDER));
    frame.render_widget(Paragraph::new(rule), Rect::new(content.x, content.y + 1, content.width, 1));

    let list = picker_list(full);
    if picker.len() == 0 {
        frame.render_widget(Paragraph::new(Span::styled("  Ничего не нашлось", dim())), list);
        return;
    }
    let offset = picker_offset(list, picker.selected);
    for (row, i) in (offset..picker.len()).take(list.height as usize).enumerate() {
        let Some(item) = picker.item(i) else { break };
        let line_area = Rect::new(list.x, list.y + row as u16, list.width, 1);
        let selected = i == picker.selected;
        let style = if selected { primary() } else { Style::new() };
        frame.render_widget(Paragraph::new(Span::raw(format!("  {}", item.display))).style(style), line_area);
        if !item.hint.is_empty() {
            let hint = Span::styled(format!("{} ", item.hint), if selected { style } else { dim() });
            frame.render_widget(Paragraph::new(hint).alignment(Alignment::Right), line_area);
        }
    }
}

fn draw_help(frame: &mut Frame, full: Rect) {
    let bold = Style::new().add_modifier(Modifier::BOLD);
    let lines = vec![
        Line::raw("Всё, что ты печатаешь, уходит в Claude — как обычно."),
        Line::raw(""),
        Line::from(vec![Span::styled("Ctrl-\\ ", bold), Span::raw(" — меню: переключить сессию, новая, закрыть…")]),
        Line::raw("          В меню: ↑↓ и Enter, цифры 1–9 — сессии,"),
        Line::raw("          буквы — действия (подписаны справа)."),
        Line::raw(""),
        Line::from(vec![Span::styled("Мышь    ", bold), Span::raw(" — клик по сессии слева открывает её,")]),
        Line::raw("          все кнопки кликаются, колесо листает историю."),
        Line::raw(""),
        Line::from(vec![Span::styled("Разрешения", bold), Span::raw(" — когда Claude спрашивает, можно ли")]),
        Line::raw("          что-то сделать, внизу появятся кнопки «Разрешить»"),
        Line::raw("          и «Отклонить». Можно ответить и в окне Claude."),
        Line::raw(""),
        Line::raw("Выделить текст мышью — с зажатым Option или Shift."),
        Line::raw("Закрыл окно терминала — все Claude останавливаются."),
    ];
    let area = centered(full, 64, lines.len() as u16 + 4);
    frame.render_widget(Clear, area);
    let block =
        frame_block(" Как пользоваться ", true).title_bottom(Line::from(" любая клавиша — закрыть ").centered());
    let content = block.inner(area);
    frame.render_widget(block, area);
    let text = Rect::new(content.x + 1, content.y + 1, content.width.saturating_sub(2), content.height.saturating_sub(1));
    frame.render_widget(Paragraph::new(lines), text);
}
