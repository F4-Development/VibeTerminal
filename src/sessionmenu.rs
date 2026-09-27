//! Меню сессии по правому клику (или Ctrl+клику) в списке сессий: что
//! сделать с этой сессией, не открывая её, — новая рабочая копия, имя,
//! папка, закрыть. Здесь пункты, геометрия для кликов и отрисовка; сами
//! действия выполняет app.

use std::path::{Path, PathBuf};

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::picker::folder_name;
use crate::session::{Session, SessionId};
use crate::ui::{contains, frame_block, primary, width};

/// По чему кликнули.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum About {
    Session(SessionId),
    /// Строка проекта над его копиями — сам он не открыт.
    Project(PathBuf),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    NewCopy,
    Git,
    /// Копия → к сессии её проекта, а нет такой — открыть.
    ToProject,
    /// Открыть сессию в папке проекта.
    OpenProject,
    Rename,
    Finder,
    CopyPath,
    Close,
}

struct Item {
    icon: &'static str,
    label: String,
    act: Act,
    /// Перед пунктом — пустая строка.
    gap: bool,
}

pub struct SessionMenu {
    pub about: About,
    title: String,
    items: Vec<Item>,
    pub cursor: usize,
    /// Где кликнули: меню открывается там.
    at: (u16, u16),
}

impl SessionMenu {
    pub fn for_session(session: &Session, at: (u16, u16)) -> Self {
        // Без запуска git: что знаем о папке — ветка в шапке, копия, `.git`.
        let git = !session.is_command
            && (session.git.is_some() || session.copy_of.is_some() || session.cwd.join(".git").exists());
        let mut items = Vec::new();
        let mut push = |icon, label: &str, act| items.push(Item { icon, label: label.to_string(), act, gap: false });
        if git {
            push("+", "Новая рабочая копия…", Act::NewCopy);
            push("⎇", "Ветки и git…", Act::Git);
        }
        if let Some(project) = &session.copy_of {
            push("↑", &format!("К проекту {}", folder_name(project)), Act::ToProject);
        }
        push("✎", "Переименовать…", Act::Rename);
        push("↗", "Открыть в Finder", Act::Finder);
        push("⎘", "Скопировать путь", Act::CopyPath);
        items.push(Item { icon: "✕", label: "Закрыть сессию".into(), act: Act::Close, gap: true });
        Self { about: About::Session(session.id), title: format!(" {} ", session.name), items, cursor: 0, at }
    }

    pub fn for_project(project: &Path, at: (u16, u16)) -> Self {
        let items = [
            ("→", "Открыть проект", Act::OpenProject),
            ("+", "Новая рабочая копия…", Act::NewCopy),
            ("↗", "Открыть в Finder", Act::Finder),
            ("⎘", "Скопировать путь", Act::CopyPath),
        ]
        .into_iter()
        .map(|(icon, label, act)| Item { icon, label: label.into(), act, gap: false })
        .collect();
        Self { about: About::Project(project.to_path_buf()), title: format!(" {} ", folder_name(project)), items, cursor: 0, at }
    }

    pub fn chosen(&self) -> Act {
        self.items[self.cursor].act
    }

    pub fn move_by(&mut self, delta: isize) {
        let len = self.items.len() as isize;
        self.cursor = (self.cursor as isize + delta).rem_euclid(len) as usize;
    }

    /// Там, где кликнули, а не влезает — сдвинуто внутрь экрана.
    pub fn rect(&self, full: Rect) -> Rect {
        let longest = self.items.iter().map(|i| width(&i.label)).chain([width(&self.title)]).max().unwrap_or(0);
        let w = (longest + 8).min(full.width);
        let gaps = self.items.iter().filter(|i| i.gap).count() as u16;
        let h = (self.items.len() as u16 + gaps + 2).min(full.height);
        let x = self.at.0.min(full.right().saturating_sub(w));
        let y = self.at.1.min(full.bottom().saturating_sub(h));
        Rect::new(x, y, w, h)
    }

    /// Строки пунктов.
    fn rows(&self, full: Rect) -> Vec<Rect> {
        let inner = frame_block("", true).inner(self.rect(full));
        let mut y = inner.y;
        let mut rows = Vec::new();
        for item in &self.items {
            y += item.gap as u16;
            rows.push(Rect::new(inner.x, y.min(inner.bottom().saturating_sub(1)), inner.width, 1));
            y += 1;
        }
        rows
    }

    /// Пункт под мышью.
    pub fn item_at(&self, full: Rect, column: u16, row: u16) -> Option<usize> {
        self.rows(full).iter().position(|r| contains(*r, column, row))
    }
}

pub fn draw(frame: &mut Frame, menu: &SessionMenu, full: Rect) {
    let area = menu.rect(full);
    frame.render_widget(Clear, area);
    frame.render_widget(frame_block(&menu.title, true), area);
    for (i, (item, row)) in menu.items.iter().zip(menu.rows(full)).enumerate() {
        let style = if i == menu.cursor { primary() } else { Style::new() };
        let line = Line::from(vec![Span::styled(format!(" {} ", item.icon), style), Span::styled(format!(" {}", item.label), style)]);
        frame.render_widget(Paragraph::new(line).style(style), row);
    }
}
