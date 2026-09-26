//! Окна git: меню веток (как в PhpStorm), действия с веткой, ввод имени,
//! диалоги выбора и окно коммита. Здесь их состояние, клавиши, геометрия
//! для кликов и отрисовка; vv исполняет то, что они вернут (`Command`).

use std::path::PathBuf;

use ratatui::Frame;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};

use crate::git::{self, Branch, ChangedFile, Failure, GitOp, OpError, Operation, RepoStatus};
use crate::keys;
use crate::picker::score;
use crate::ui::{accent, bold, centered, contains, dim, frame_block, primary, width};

const MENU_WIDTH: u16 = 62;
const DIALOG_WIDTH: u16 = 66;
const COMMIT_SIZE: (u16, u16) = (80, 26);

pub enum GitOverlay {
    Menu(GitMenu),
    Branch(BranchMenu),
    Input(GitInput),
    Choice(GitChoice),
    Commit(CommitDialog),
}

/// Что сделать vv в ответ на клавишу или клик.
pub enum Command {
    None,
    Redraw,
    Close,
    Run(GitOp),
    /// Отправить текст Claude в открытую сессию.
    AskClaude(String),
}

/// Кликабельные места в окнах git.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum GitTarget {
    Row(usize),
    Button(usize),
    File(usize),
    Message,
}

// ── Меню веток ────────────────────────────────────────────────────────────

pub struct GitMenu {
    cwd: PathBuf,
    status: RepoStatus,
    branches: Vec<Branch>,
    query: String,
    cursor: usize,
    entries: Vec<Entry>,
    /// Под каким местом шапки открыто меню.
    anchor: Rect,
}

struct Entry {
    kind: EntryKind,
    icon: &'static str,
    label: String,
    hint: String,
}

#[derive(Clone, Copy)]
enum EntryKind {
    Header,
    Action(MenuAction),
    Branch(usize),
}

#[derive(Clone, Copy)]
enum MenuAction {
    Fetch,
    Pull,
    Push,
    Commit,
    NewBranch,
    Resolve,
    Continue(Operation),
    Abort(Operation),
}

impl GitMenu {
    pub fn new(cwd: PathBuf, status: RepoStatus, anchor: Rect) -> Self {
        let branches = git::branches(&cwd);
        let mut menu = Self { cwd, status, branches, query: String::new(), cursor: 0, entries: Vec::new(), anchor };
        menu.rebuild();
        menu
    }

    fn rebuild(&mut self) {
        let status = &self.status;
        let mut entries = Vec::new();
        let header = |entries: &mut Vec<Entry>, text: &str| {
            entries.push(Entry { kind: EntryKind::Header, icon: "", label: text.to_string(), hint: String::new() });
        };
        let action = |icon, label: &str, hint: String, action| Entry {
            kind: EntryKind::Action(action),
            icon,
            label: label.to_string(),
            hint,
        };

        if self.query.is_empty() {
            if let Some(operation) = status.operation {
                header(&mut entries, &format!("Незаконченное {}", operation.label()));
                if status.conflicts > 0 {
                    let hint = format!("конфликтов: {}", status.conflicts);
                    entries.push(action("✦", "Пусть Claude разрешит конфликты", hint, MenuAction::Resolve));
                } else {
                    entries.push(action("✓", "Завершить", String::new(), MenuAction::Continue(operation)));
                }
                entries.push(action("✕", "Отменить", String::new(), MenuAction::Abort(operation)));
            }
            let behind = if status.behind > 0 { format!("↓{}", status.behind) } else { String::new() };
            let ahead = match (status.ahead, &status.upstream) {
                (_, None) if status.branch.is_some() => "новая ветка".to_string(),
                (0, _) => String::new(),
                (n, _) => format!("↑{n}"),
            };
            entries.push(action("↓", "Получить изменения (fetch)", String::new(), MenuAction::Fetch));
            entries.push(action("⇣", "Обновить ветку (pull)", behind, MenuAction::Pull));
            entries.push(action("⇡", "Отправить (push)", ahead, MenuAction::Push));
            if status.changed > 0 && status.operation.is_none() {
                let hint = format!("изменений: {}", status.changed);
                entries.push(action("✓", "Закоммитить…", hint, MenuAction::Commit));
            }
            entries.push(action("+", "Новая ветка…", String::new(), MenuAction::NewBranch));
        }

        let query = self.query.trim().to_lowercase();
        let latin: String = query.chars().map(|c| keys::ru_to_en(c).unwrap_or(c)).collect();
        let matches = |name: &str| {
            let name = name.to_lowercase();
            query.is_empty() || score(&query, &name).is_some() || score(&latin, &name).is_some()
        };
        for (remote, title) in [(false, "Локальные"), (true, "Удалённые")] {
            let found: Vec<usize> =
                (0..self.branches.len()).filter(|&i| self.branches[i].remote == remote && matches(&self.branches[i].name)).collect();
            if found.is_empty() {
                continue;
            }
            header(&mut entries, title);
            for i in found {
                let b = &self.branches[i];
                let mut hint = String::new();
                if b.ahead > 0 {
                    hint.push_str(&format!("↑{} ", b.ahead));
                }
                if b.behind > 0 {
                    hint.push_str(&format!("↓{}", b.behind));
                }
                let icon = if b.current { "❯" } else { " " };
                entries.push(Entry { kind: EntryKind::Branch(i), icon, label: b.name.clone(), hint: hint.trim().into() });
            }
        }
        self.entries = entries;
        self.cursor = 0;
        self.move_by(0);
    }

    fn selectable(&self, i: usize) -> bool {
        self.entries.get(i).is_some_and(|e| !matches!(e.kind, EntryKind::Header))
    }

    /// Сдвинуть выбор, перескакивая заголовки.
    fn move_by(&mut self, delta: isize) {
        let len = self.entries.len() as isize;
        if len == 0 {
            return;
        }
        let step = if delta < 0 { -1 } else { 1 };
        let mut i = (self.cursor as isize + delta).rem_euclid(len);
        for _ in 0..len {
            if self.selectable(i as usize) {
                self.cursor = i as usize;
                return;
            }
            i = (i + step).rem_euclid(len);
        }
    }

    fn activate(&self) -> Activation {
        let Some(entry) = self.entries.get(self.cursor) else { return Activation::Command(Command::None) };
        match entry.kind {
            EntryKind::Header => Activation::Command(Command::None),
            EntryKind::Branch(i) => Activation::Branch(self.branches[i].clone()),
            EntryKind::Action(action) => match action {
                MenuAction::Fetch => Activation::Command(Command::Run(GitOp::Fetch { quiet: false })),
                MenuAction::Pull => Activation::Command(Command::Run(GitOp::Pull)),
                MenuAction::Push => Activation::Command(Command::Run(GitOp::Push)),
                MenuAction::Commit => Activation::Commit,
                MenuAction::NewBranch => Activation::NewBranch,
                MenuAction::Resolve => Activation::Command(Command::AskClaude(resolve_prompt(self.status.operation))),
                MenuAction::Continue(op) => Activation::Command(Command::Run(GitOp::Continue(op))),
                MenuAction::Abort(op) => Activation::Abort(op),
            },
        }
    }

    fn rect(&self, full: Rect) -> Rect {
        let height = (self.entries.len() as u16 + 4).min(full.height.saturating_sub(self.anchor.y + 2)).max(6);
        let w = MENU_WIDTH.min(full.width.saturating_sub(2));
        let x = self.anchor.x.saturating_sub(1).min(full.right().saturating_sub(w + 1));
        Rect::new(x, self.anchor.y + 1, w, height)
    }

    /// Строки списка и какой пункт в каждой.
    fn rows(&self, full: Rect) -> Vec<(Rect, usize)> {
        let inner = frame_block("", true).inner(self.rect(full));
        let list = Rect::new(inner.x, inner.y + 2, inner.width, inner.height.saturating_sub(2));
        let visible = list.height as usize;
        let offset = self.cursor.saturating_sub(visible.saturating_sub(1));
        (offset..self.entries.len())
            .take(visible)
            .enumerate()
            .map(|(row, i)| (Rect::new(list.x, list.y + row as u16, list.width, 1), i))
            .collect()
    }
}

enum Activation {
    Command(Command),
    Branch(Branch),
    Commit,
    NewBranch,
    Abort(Operation),
}

fn resolve_prompt(operation: Option<Operation>) -> String {
    let (what, command) = match operation {
        Some(Operation::Rebase) => ("перебазирование", "git rebase --continue"),
        Some(Operation::CherryPick) => ("cherry-pick", "git cherry-pick --continue"),
        Some(Operation::Revert) => ("revert", "git revert --continue"),
        _ => ("слияние", "git commit --no-edit"),
    };
    format!(
        "В репозитории незаконченное {what} с конфликтами. Разреши конфликты во всех файлах, \
         проверь, что проект собирается, добавь файлы (git add) и заверши: {command}. Не пушь."
    )
}

// ── Действия с веткой ─────────────────────────────────────────────────────

pub struct BranchMenu {
    parent: Box<GitMenu>,
    branch: Branch,
    items: Vec<(String, BranchAction)>,
    cursor: usize,
}

#[derive(Clone, Copy)]
enum BranchAction {
    Switch,
    NewFrom,
    Merge,
    Rebase,
    Rename,
    Delete,
}

impl BranchMenu {
    fn new(parent: GitMenu, branch: Branch) -> Self {
        let current = parent.status.head_label();
        let name = &branch.name;
        let mut items = Vec::new();
        if branch.current {
            items.push(("Новая ветка от неё…".to_string(), BranchAction::NewFrom));
            items.push(("Переименовать…".to_string(), BranchAction::Rename));
        } else {
            let switch = if branch.remote { "Переключиться (создать локальную)" } else { "Переключиться" };
            items.push((switch.to_string(), BranchAction::Switch));
            items.push(("Новая ветка от неё…".to_string(), BranchAction::NewFrom));
            items.push((format!("Слить «{name}» в «{current}» (merge)"), BranchAction::Merge));
            items.push((format!("Перебазировать «{current}» на «{name}» (rebase)"), BranchAction::Rebase));
            if !branch.remote {
                items.push(("Переименовать…".to_string(), BranchAction::Rename));
                items.push(("Удалить…".to_string(), BranchAction::Delete));
            }
        }
        Self { parent: Box::new(parent), branch, items, cursor: 0 }
    }

    fn rect(&self, full: Rect) -> Rect {
        let base = self.parent.rect(full);
        Rect::new(base.x, base.y, base.width, (self.items.len() as u16 + 2).min(base.height.max(4)))
    }

    fn rows(&self, full: Rect) -> Vec<(Rect, usize)> {
        let inner = frame_block("", true).inner(self.rect(full));
        (0..self.items.len())
            .take(inner.height as usize)
            .map(|i| (Rect::new(inner.x, inner.y + i as u16, inner.width, 1), i))
            .collect()
    }
}

// ── Ввод имени ветки ──────────────────────────────────────────────────────

pub struct GitInput {
    title: String,
    value: String,
    purpose: InputPurpose,
}

enum InputPurpose {
    NewBranch { base: Option<String> },
    Rename { old: String },
}

impl GitInput {
    fn submit(&self) -> Command {
        // В имени ветки не бывает пробелов.
        let name = self.value.trim().replace(' ', "-");
        if name.is_empty() {
            return Command::None;
        }
        Command::Run(match &self.purpose {
            InputPurpose::NewBranch { base } => GitOp::NewBranch { name, base: base.clone() },
            InputPurpose::Rename { old } => GitOp::Rename { old: old.clone(), new: name },
        })
    }
}

const INPUT_BUTTONS: [&str; 2] = [" Готово ", " Отмена "];

// ── Диалог выбора ─────────────────────────────────────────────────────────

pub struct GitChoice {
    title: String,
    lines: Vec<String>,
    buttons: Vec<(String, ChoiceAction)>,
    cursor: usize,
}

pub enum ChoiceAction {
    Run(GitOp),
    AskClaude(String),
    Close,
}

impl GitChoice {
    fn new(title: &str, text: &str, buttons: Vec<(&str, ChoiceAction)>) -> Self {
        let lines = wrap(text, (DIALOG_WIDTH - 4) as usize);
        let buttons = buttons.into_iter().map(|(label, action)| (format!(" {label} "), action)).collect();
        Self { title: format!(" {title} "), lines, buttons, cursor: 0 }
    }

    fn choose(&mut self, index: usize) -> Command {
        if index >= self.buttons.len() {
            return Command::None;
        }
        match std::mem::replace(&mut self.buttons[index].1, ChoiceAction::Close) {
            ChoiceAction::Run(op) => Command::Run(op),
            ChoiceAction::AskClaude(text) => Command::AskClaude(text),
            ChoiceAction::Close => Command::Close,
        }
    }

    fn rect(&self, full: Rect) -> Rect {
        centered(full, DIALOG_WIDTH, self.lines.len() as u16 + 5)
    }

    fn buttons(&self, full: Rect) -> Vec<Rect> {
        let inner = frame_block("", true).inner(self.rect(full));
        let y = inner.bottom().saturating_sub(1);
        let mut x = inner.x + 1;
        self.buttons
            .iter()
            .map(|(label, _)| {
                let rect = Rect::new(x, y, width(label), 1);
                x += width(label) + 2;
                rect
            })
            .collect()
    }
}

/// Не вышло — что предложить. `None` — ничего не показывать.
pub fn failure_dialog(op: &GitOp, error: &OpError) -> Option<GitChoice> {
    let details = first_lines(&error.text, 6);
    let dialog = match (&error.failure, op) {
        (_, GitOp::Fetch { quiet: true }) => return None,
        (Failure::DirtyCheckout, GitOp::Switch { branch, remote, .. }) => GitChoice::new(
            "Есть незакоммиченные изменения",
            &format!("Они мешают переключиться на {branch}. Можно забрать их с собой в новую ветку."),
            vec![
                ("Забрать с собой", ChoiceAction::Run(GitOp::Switch { branch: branch.clone(), remote: *remote, carry: true })),
                ("Отмена", ChoiceAction::Close),
            ],
        ),
        (Failure::Diverged, _) => GitChoice::new(
            "Ветка разошлась с сервером",
            "И у тебя, и на сервере есть новые коммиты. Как объединить?",
            vec![
                ("Слить (merge)", ChoiceAction::Run(GitOp::PullMerge)),
                ("Перебазировать (rebase)", ChoiceAction::Run(GitOp::PullRebase)),
                ("Отмена", ChoiceAction::Close),
            ],
        ),
        (Failure::PushRejected, _) => GitChoice::new(
            "Сервер опередил",
            "На сервере есть коммиты, которых у тебя нет. Сначала обнови ветку. \
             Принудительная отправка перезапишет чужие коммиты на сервере.",
            vec![
                ("Обновить ветку (pull)", ChoiceAction::Run(GitOp::Pull)),
                ("Отправить принудительно", ChoiceAction::Run(GitOp::PushForce)),
                ("Отмена", ChoiceAction::Close),
            ],
        ),
        (Failure::NotMerged, GitOp::Delete { name, .. }) => GitChoice::new(
            "Ветка не слита",
            &format!("В {name} есть коммиты, которых нет в других ветках. Удалить всё равно — они пропадут."),
            vec![
                ("Удалить всё равно", ChoiceAction::Run(GitOp::Delete { name: name.clone(), force: true })),
                ("Отмена", ChoiceAction::Close),
            ],
        ),
        (Failure::Conflicts, _) => GitChoice::new(
            "Конфликты",
            "Изменения пересеклись в одних и тех же местах. Claude может разобраться сам.",
            vec![
                ("Пусть Claude разрешит", ChoiceAction::AskClaude(resolve_prompt(conflict_operation(op)))),
                ("Позже", ChoiceAction::Close),
            ],
        ),
        (Failure::Auth, _) => GitChoice::new(
            "Нет доступа к серверу",
            &format!("Проверь SSH-ключ или вход на сервер.\n\n{details}"),
            vec![("ОК", ChoiceAction::Close)],
        ),
        _ => GitChoice::new("Не получилось", &details, vec![("ОК", ChoiceAction::Close)]),
    };
    Some(dialog)
}

fn conflict_operation(op: &GitOp) -> Option<Operation> {
    match op {
        GitOp::Rebase(_) | GitOp::PullRebase => Some(Operation::Rebase),
        _ => Some(Operation::Merge),
    }
}

fn first_lines(text: &str, count: usize) -> String {
    text.lines().filter(|l| !l.trim().is_empty()).take(count).collect::<Vec<_>>().join("\n")
}

fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for paragraph in text.split('\n') {
        let mut line = String::new();
        for word in paragraph.split_whitespace() {
            if !line.is_empty() && line.chars().count() + 1 + word.chars().count() > width {
                lines.push(std::mem::take(&mut line));
            }
            if !line.is_empty() {
                line.push(' ');
            }
            line.push_str(word);
        }
        lines.push(line);
    }
    lines
}

// ── Коммит ────────────────────────────────────────────────────────────────

pub struct CommitDialog {
    branch: String,
    files: Vec<(ChangedFile, bool)>,
    message: String,
    focus: CommitFocus,
    cursor: usize,
}

#[derive(PartialEq, Eq)]
enum CommitFocus {
    Files,
    Message,
}

const COMMIT_BUTTONS: [&str; 4] = [" Закоммитить ", " Закоммитить и отправить ", " Пусть Claude закоммитит ", " Отмена "];

impl CommitDialog {
    fn new(cwd: PathBuf, branch: String) -> Self {
        let files = git::changed_files(&cwd).into_iter().map(|f| (f, true)).collect();
        Self { branch, files, message: String::new(), focus: CommitFocus::Message, cursor: 0 }
    }

    fn selected(&self) -> Vec<String> {
        self.files.iter().filter(|(_, on)| *on).map(|(f, _)| f.path.clone()).collect()
    }

    fn button(&mut self, index: usize) -> Command {
        let files = self.selected();
        match index {
            0 | 1 if files.is_empty() => Command::None,
            0 | 1 if self.message.trim().is_empty() => {
                self.focus = CommitFocus::Message;
                Command::Redraw
            }
            0 | 1 => Command::Run(GitOp::Commit { message: self.message.trim().to_string(), files, push: index == 1 }),
            2 => Command::AskClaude(format!(
                "Закоммить изменения в этих файлах: {}. Напиши понятное короткое сообщение коммита на русском. Не пушь.",
                files.join(", ")
            )),
            _ => Command::Close,
        }
    }

    fn rect(&self, full: Rect) -> Rect {
        centered(full, COMMIT_SIZE.0, COMMIT_SIZE.1.min(self.files.len() as u16 + 10))
    }

    /// Список файлов, поле сообщения и кнопки.
    fn layout(&self, full: Rect) -> (Rect, Rect, Vec<Rect>) {
        let inner = frame_block("", true).inner(self.rect(full));
        let buttons_y = inner.bottom().saturating_sub(1);
        let message = Rect::new(inner.x + 1, buttons_y.saturating_sub(2), inner.width.saturating_sub(2), 1);
        let files = Rect::new(inner.x, inner.y + 1, inner.width, message.y.saturating_sub(inner.y + 3));
        let mut x = inner.x + 1;
        let buttons = COMMIT_BUTTONS
            .iter()
            .map(|label| {
                let rect = Rect::new(x, buttons_y, width(label), 1);
                x += width(label) + 1;
                rect
            })
            .collect();
        (files, message, buttons)
    }

    fn file_rows(&self, full: Rect) -> Vec<(Rect, usize)> {
        let (list, _, _) = self.layout(full);
        let visible = list.height as usize;
        let offset = self.cursor.saturating_sub(visible.saturating_sub(1));
        (offset..self.files.len())
            .take(visible)
            .enumerate()
            .map(|(row, i)| (Rect::new(list.x, list.y + row as u16, list.width, 1), i))
            .collect()
    }
}

// ── Открыть окна ──────────────────────────────────────────────────────────

pub fn open_menu(cwd: PathBuf, status: RepoStatus, anchor: Rect) -> GitOverlay {
    GitOverlay::Menu(GitMenu::new(cwd, status, anchor))
}

pub fn open_commit(cwd: PathBuf, status: &RepoStatus) -> GitOverlay {
    GitOverlay::Commit(CommitDialog::new(cwd, status.head_label()))
}

// ── Клавиши ───────────────────────────────────────────────────────────────

pub fn on_key(overlay: &mut GitOverlay, key: KeyEvent) -> Command {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    match overlay {
        GitOverlay::Menu(menu) => match key.code {
            KeyCode::Esc => Command::Close,
            KeyCode::Up => {
                menu.move_by(-1);
                Command::Redraw
            }
            KeyCode::Down | KeyCode::Tab => {
                menu.move_by(1);
                Command::Redraw
            }
            KeyCode::Enter | KeyCode::Right => activate_menu(overlay),
            KeyCode::Backspace => {
                menu.query.pop();
                menu.rebuild();
                Command::Redraw
            }
            KeyCode::Char('u') if ctrl => {
                menu.query.clear();
                menu.rebuild();
                Command::Redraw
            }
            KeyCode::Char(c) if !ctrl => {
                menu.query.push(c);
                menu.rebuild();
                Command::Redraw
            }
            _ => Command::None,
        },
        GitOverlay::Branch(branch) => match key.code {
            KeyCode::Esc | KeyCode::Left => {
                back_to_menu(overlay);
                Command::Redraw
            }
            KeyCode::Up => {
                branch.cursor = (branch.cursor + branch.items.len() - 1) % branch.items.len();
                Command::Redraw
            }
            KeyCode::Down | KeyCode::Tab => {
                branch.cursor = (branch.cursor + 1) % branch.items.len();
                Command::Redraw
            }
            KeyCode::Enter | KeyCode::Right => activate_branch(overlay),
            _ => Command::None,
        },
        GitOverlay::Input(input) => match key.code {
            KeyCode::Esc => Command::Close,
            KeyCode::Enter => input.submit(),
            KeyCode::Backspace => {
                input.value.pop();
                Command::Redraw
            }
            KeyCode::Char('u') if ctrl => {
                input.value.clear();
                Command::Redraw
            }
            KeyCode::Char(c) if !ctrl => {
                input.value.push(c);
                Command::Redraw
            }
            _ => Command::None,
        },
        GitOverlay::Choice(choice) => match key.code {
            KeyCode::Esc => Command::Close,
            KeyCode::Left => {
                choice.cursor = choice.cursor.saturating_sub(1);
                Command::Redraw
            }
            KeyCode::Right | KeyCode::Tab => {
                choice.cursor = (choice.cursor + 1).min(choice.buttons.len() - 1);
                Command::Redraw
            }
            KeyCode::Enter => {
                let index = choice.cursor;
                choice.choose(index)
            }
            _ => Command::None,
        },
        GitOverlay::Commit(commit) => match (key.code, &commit.focus) {
            (KeyCode::Esc, _) => Command::Close,
            (KeyCode::Tab | KeyCode::BackTab, _) => {
                commit.focus =
                    if commit.focus == CommitFocus::Files { CommitFocus::Message } else { CommitFocus::Files };
                Command::Redraw
            }
            (KeyCode::Enter, CommitFocus::Message) => commit.button(0),
            (KeyCode::Up, CommitFocus::Files) => {
                commit.cursor = commit.cursor.saturating_sub(1);
                Command::Redraw
            }
            (KeyCode::Down, CommitFocus::Files) => {
                commit.cursor = (commit.cursor + 1).min(commit.files.len().saturating_sub(1));
                Command::Redraw
            }
            (KeyCode::Char(' ') | KeyCode::Enter, CommitFocus::Files) => {
                if let Some((_, on)) = commit.files.get_mut(commit.cursor) {
                    *on = !*on;
                }
                Command::Redraw
            }
            (KeyCode::Backspace, CommitFocus::Message) => {
                commit.message.pop();
                Command::Redraw
            }
            (KeyCode::Char('u'), CommitFocus::Message) if ctrl => {
                commit.message.clear();
                Command::Redraw
            }
            (KeyCode::Char(c), CommitFocus::Message) if !ctrl => {
                commit.message.push(c);
                Command::Redraw
            }
            _ => Command::None,
        },
    }
}

/// Вставка текста: в поиск, имя ветки или сообщение коммита.
pub fn on_paste(overlay: &mut GitOverlay, text: &str) {
    let line = text.lines().next().unwrap_or_default();
    match overlay {
        GitOverlay::Menu(menu) => {
            menu.query.push_str(line);
            menu.rebuild();
        }
        GitOverlay::Input(input) => input.value.push_str(line),
        GitOverlay::Commit(commit) => commit.message.push_str(line),
        _ => {}
    }
}

fn activate_menu(overlay: &mut GitOverlay) -> Command {
    let GitOverlay::Menu(menu) = overlay else { return Command::None };
    match menu.activate() {
        Activation::Command(command) => command,
        Activation::Branch(branch) => {
            let GitOverlay::Menu(menu) = std::mem::replace(overlay, GitOverlay::Choice(empty_choice())) else {
                unreachable!()
            };
            *overlay = GitOverlay::Branch(BranchMenu::new(menu, branch));
            Command::Redraw
        }
        Activation::Commit => {
            *overlay = open_commit(menu.cwd.clone(), &menu.status);
            Command::Redraw
        }
        Activation::NewBranch => {
            *overlay = GitOverlay::Input(GitInput {
                title: " Новая ветка ".into(),
                value: String::new(),
                purpose: InputPurpose::NewBranch { base: None },
            });
            Command::Redraw
        }
        Activation::Abort(operation) => {
            *overlay = GitOverlay::Choice(GitChoice::new(
                &format!("Отменить {}?", operation.label()),
                "Всё, что успело слиться, откатится к состоянию до начала.",
                vec![("Отменить", ChoiceAction::Run(GitOp::Abort(operation))), ("Нет", ChoiceAction::Close)],
            ));
            Command::Redraw
        }
    }
}

fn empty_choice() -> GitChoice {
    GitChoice { title: String::new(), lines: Vec::new(), buttons: Vec::new(), cursor: 0 }
}

fn back_to_menu(overlay: &mut GitOverlay) {
    if let GitOverlay::Branch(_) = overlay
        && let GitOverlay::Branch(branch) = std::mem::replace(overlay, GitOverlay::Choice(empty_choice()))
    {
        *overlay = GitOverlay::Menu(*branch.parent);
    }
}

fn activate_branch(overlay: &mut GitOverlay) -> Command {
    let GitOverlay::Branch(menu) = overlay else { return Command::None };
    let branch = menu.branch.clone();
    let name = branch.name.clone();
    match menu.items[menu.cursor].1 {
        BranchAction::Switch => Command::Run(GitOp::Switch { branch: name, remote: branch.remote, carry: false }),
        BranchAction::Merge => Command::Run(GitOp::Merge(name)),
        BranchAction::Rebase => Command::Run(GitOp::Rebase(name)),
        BranchAction::NewFrom => {
            *overlay = GitOverlay::Input(GitInput {
                title: format!(" Новая ветка от {name} "),
                value: String::new(),
                purpose: InputPurpose::NewBranch { base: Some(name) },
            });
            Command::Redraw
        }
        BranchAction::Rename => {
            *overlay = GitOverlay::Input(GitInput {
                title: format!(" Переименовать {name} "),
                value: name.clone(),
                purpose: InputPurpose::Rename { old: name },
            });
            Command::Redraw
        }
        BranchAction::Delete => {
            *overlay = GitOverlay::Choice(GitChoice::new(
                &format!("Удалить ветку {name}?"),
                "Ветка удалится только у тебя, на сервере останется.",
                vec![
                    ("Удалить", ChoiceAction::Run(GitOp::Delete { name, force: false })),
                    ("Отмена", ChoiceAction::Close),
                ],
            ));
            Command::Redraw
        }
    }
}

// ── Мышь ──────────────────────────────────────────────────────────────────

/// Что под мышью в открытом окне git.
pub fn target_at(overlay: &GitOverlay, full: Rect, column: u16, row: u16) -> Option<GitTarget> {
    let hit = |rows: Vec<(Rect, usize)>| rows.into_iter().find(|(r, _)| contains(*r, column, row)).map(|(_, i)| i);
    match overlay {
        GitOverlay::Menu(menu) => hit(menu.rows(full)).filter(|&i| menu.selectable(i)).map(GitTarget::Row),
        GitOverlay::Branch(branch) => hit(branch.rows(full)).map(GitTarget::Row),
        GitOverlay::Input(input) => input_buttons(input, full)
            .iter()
            .position(|r| contains(*r, column, row))
            .map(GitTarget::Button),
        GitOverlay::Choice(choice) => {
            choice.buttons(full).iter().position(|r| contains(*r, column, row)).map(GitTarget::Button)
        }
        GitOverlay::Commit(commit) => {
            let (_, message, buttons) = commit.layout(full);
            if let Some(i) = buttons.iter().position(|r| contains(*r, column, row)) {
                return Some(GitTarget::Button(i));
            }
            if contains(message, column, row) {
                return Some(GitTarget::Message);
            }
            hit(commit.file_rows(full)).map(GitTarget::File)
        }
    }
}

/// Наведение: в меню пункт под мышью становится выбранным.
pub fn hover(overlay: &mut GitOverlay, target: GitTarget) -> bool {
    match (overlay, target) {
        (GitOverlay::Menu(menu), GitTarget::Row(i)) if menu.cursor != i => {
            menu.cursor = i;
            true
        }
        (GitOverlay::Branch(branch), GitTarget::Row(i)) if branch.cursor != i => {
            branch.cursor = i;
            true
        }
        (GitOverlay::Choice(choice), GitTarget::Button(i)) if choice.cursor != i => {
            choice.cursor = i;
            true
        }
        _ => false,
    }
}

pub fn click(overlay: &mut GitOverlay, target: GitTarget) -> Command {
    match (&mut *overlay, target) {
        (GitOverlay::Menu(menu), GitTarget::Row(i)) => {
            menu.cursor = i;
            activate_menu(overlay)
        }
        (GitOverlay::Branch(branch), GitTarget::Row(i)) => {
            branch.cursor = i;
            activate_branch(overlay)
        }
        (GitOverlay::Input(input), GitTarget::Button(0)) => input.submit(),
        (GitOverlay::Input(_), GitTarget::Button(_)) => Command::Close,
        (GitOverlay::Choice(choice), GitTarget::Button(i)) => choice.choose(i),
        (GitOverlay::Commit(commit), GitTarget::Button(i)) => commit.button(i),
        (GitOverlay::Commit(commit), GitTarget::Message) => {
            commit.focus = CommitFocus::Message;
            Command::Redraw
        }
        (GitOverlay::Commit(commit), GitTarget::File(i)) => {
            commit.focus = CommitFocus::Files;
            commit.cursor = i;
            if let Some((_, on)) = commit.files.get_mut(i) {
                *on = !*on;
            }
            Command::Redraw
        }
        _ => Command::None,
    }
}

/// Клик мимо: меню закрывается, диалоги — нет.
pub fn closes_on_outside_click(overlay: &GitOverlay, full: Rect, column: u16, row: u16) -> bool {
    match overlay {
        GitOverlay::Menu(menu) => !contains(menu.rect(full), column, row),
        GitOverlay::Branch(branch) => !contains(branch.rect(full), column, row),
        _ => false,
    }
}

fn input_rect(full: Rect) -> Rect {
    centered(full, DIALOG_WIDTH, 7)
}

fn input_buttons(_input: &GitInput, full: Rect) -> Vec<Rect> {
    let inner = frame_block("", true).inner(input_rect(full));
    let y = inner.bottom().saturating_sub(1);
    let first = Rect::new(inner.x + 1, y, width(INPUT_BUTTONS[0]), 1);
    let second = Rect::new(first.right() + 2, y, width(INPUT_BUTTONS[1]), 1);
    vec![first, second]
}

// ── Отрисовка ─────────────────────────────────────────────────────────────

pub fn draw(frame: &mut Frame, overlay: &GitOverlay, full: Rect) {
    match overlay {
        GitOverlay::Menu(menu) => draw_menu(frame, menu, full),
        GitOverlay::Branch(branch) => draw_branch(frame, branch, full),
        GitOverlay::Input(input) => draw_input(frame, input, full),
        GitOverlay::Choice(choice) => draw_choice(frame, choice, full),
        GitOverlay::Commit(commit) => draw_commit(frame, commit, full),
    }
}

fn draw_menu(frame: &mut Frame, menu: &GitMenu, full: Rect) {
    let area = menu.rect(full);
    frame.render_widget(Clear, area);
    let block = frame_block(" Ветки ", true)
        .title_bottom(Line::from(" ↑↓ и Enter — выбрать · Esc — закрыть ").centered());
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let search = Rect::new(inner.x, inner.y, inner.width, 1);
    let prompt = Span::styled(" Поиск: ", accent());
    let query = if menu.query.is_empty() {
        Span::styled("начни печатать название ветки", dim())
    } else {
        Span::raw(menu.query.as_str())
    };
    let cursor_x = search.x + prompt.width() as u16 + width(&menu.query);
    frame.render_widget(Paragraph::new(Line::from(vec![prompt, query])), search);
    frame.set_cursor_position((cursor_x.min(search.right().saturating_sub(1)), search.y));
    let rule = Span::styled("─".repeat(inner.width as usize), Style::new().fg(Color::DarkGray));
    frame.render_widget(Paragraph::new(rule), Rect::new(inner.x, inner.y + 1, inner.width, 1));

    for (rect, i) in menu.rows(full) {
        let entry = &menu.entries[i];
        if let EntryKind::Header = entry.kind {
            frame.render_widget(Paragraph::new(Span::styled(format!(" {}", entry.label), dim())), rect);
            continue;
        }
        let selected = i == menu.cursor;
        let style = if selected { primary() } else { Style::new() };
        let icon_style = match (selected, entry.kind) {
            (true, _) => style,
            (false, EntryKind::Branch(b)) if menu.branches[b].current => accent(),
            _ => Style::new().fg(Color::Indexed(173)),
        };
        let line = Line::from(vec![Span::styled(format!(" {} ", entry.icon), icon_style), Span::styled(entry.label.as_str(), style)]);
        frame.render_widget(Paragraph::new(line).style(style), rect);
        if !entry.hint.is_empty() {
            let hint = Span::styled(format!("{} ", entry.hint), if selected { style } else { dim() });
            frame.render_widget(Paragraph::new(hint).alignment(Alignment::Right), rect);
        }
    }
}

fn draw_branch(frame: &mut Frame, menu: &BranchMenu, full: Rect) {
    draw_menu(frame, &menu.parent, full);
    let area = menu.rect(full);
    frame.render_widget(Clear, area);
    let title = format!(" {} ", menu.branch.name);
    let block = frame_block(&title, true)
        .title_bottom(Line::from(" Enter — выбрать · Esc — назад ").centered());
    frame.render_widget(block, area);
    for (rect, i) in menu.rows(full) {
        let style = if i == menu.cursor { primary() } else { Style::new() };
        frame.render_widget(Paragraph::new(Span::raw(format!("  {}", menu.items[i].0))).style(style), rect);
    }
}

fn draw_input(frame: &mut Frame, input: &GitInput, full: Rect) {
    let area = input_rect(full);
    frame.render_widget(Clear, area);
    let block = frame_block(&input.title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), 1);
    frame.render_widget(Paragraph::new("Название ветки:"), text);
    let field = Rect::new(text.x, text.y + 1, text.width, 1);
    frame.render_widget(
        Paragraph::new(Span::styled(format!("{} ", input.value), Style::new().add_modifier(Modifier::UNDERLINED))),
        field,
    );
    frame.set_cursor_position(((field.x + width(&input.value)).min(field.right().saturating_sub(1)), field.y));
    let buttons = input_buttons(input, full);
    frame.render_widget(Paragraph::new(Span::styled(INPUT_BUTTONS[0], primary())), buttons[0]);
    frame.render_widget(Paragraph::new(Span::styled(INPUT_BUTTONS[1], bold())), buttons[1]);
}

fn draw_choice(frame: &mut Frame, choice: &GitChoice, full: Rect) {
    let area = choice.rect(full);
    frame.render_widget(Clear, area);
    let block = frame_block(&choice.title, true);
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let text = Rect::new(inner.x + 1, inner.y, inner.width.saturating_sub(2), inner.height.saturating_sub(2));
    frame.render_widget(Paragraph::new(choice.lines.iter().map(|l| Line::raw(l.as_str())).collect::<Vec<_>>()), text);
    for (i, rect) in choice.buttons(full).into_iter().enumerate() {
        let style = if i == choice.cursor { primary() } else { bold() };
        frame.render_widget(Paragraph::new(Span::styled(choice.buttons[i].0.as_str(), style)), rect);
    }
}

fn draw_commit(frame: &mut Frame, commit: &CommitDialog, full: Rect) {
    let area = commit.rect(full);
    frame.render_widget(Clear, area);
    let selected = commit.files.iter().filter(|(_, on)| *on).count();
    let title = format!(" Коммит в {} ", commit.branch);
    let block = frame_block(&title, true)
        .title_bottom(Line::from(" Tab — файлы/сообщение · Пробел — отметить · Esc — закрыть ").centered());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let header = format!(" Файлы: отмечено {selected} из {}", commit.files.len());
    frame.render_widget(Paragraph::new(Span::styled(header, dim())), Rect::new(inner.x, inner.y, inner.width, 1));

    let files_focused = commit.focus == CommitFocus::Files;
    for (rect, i) in commit.file_rows(full) {
        let (file, on) = &commit.files[i];
        let check = if *on { "[x]" } else { "[ ]" };
        let kind_style = match file.kind {
            'A' | '?' => Style::new().fg(Color::Green),
            'D' => Style::new().fg(Color::Red),
            _ => Style::new().fg(Color::Yellow),
        };
        let style = if files_focused && i == commit.cursor { primary() } else { Style::new() };
        let line = Line::from(vec![
            Span::styled(format!(" {check} "), style),
            Span::styled(format!("{} ", file.kind), if style == primary() { style } else { kind_style }),
            Span::styled(file.path.as_str(), style),
        ]);
        frame.render_widget(Paragraph::new(line).style(style), rect);
    }

    let (_, message, buttons) = commit.layout(full);
    let label = Rect::new(message.x, message.y.saturating_sub(1), message.width, 1);
    frame.render_widget(Paragraph::new(Span::styled("Сообщение коммита:", dim())), label);
    let text = if commit.message.is_empty() && !files_focused {
        Span::styled("что сделано — например, «Добавил вход по почте»", dim())
    } else {
        Span::styled(format!("{} ", commit.message), Style::new().add_modifier(Modifier::UNDERLINED))
    };
    frame.render_widget(Paragraph::new(text), message);
    if !files_focused {
        frame.set_cursor_position(((message.x + width(&commit.message)).min(message.right().saturating_sub(1)), message.y));
    }
    for (i, rect) in buttons.into_iter().enumerate() {
        let style = if i == 0 { primary() } else { bold() };
        frame.render_widget(Paragraph::new(Span::styled(COMMIT_BUTTONS[i], style)), rect);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn menu_with(status: RepoStatus, branches: Vec<Branch>) -> GitMenu {
        let mut menu =
            GitMenu { cwd: PathBuf::new(), status, branches, query: String::new(), cursor: 0, entries: Vec::new(), anchor: Rect::default() };
        menu.rebuild();
        menu
    }

    fn branch(name: &str, current: bool, remote: bool) -> Branch {
        Branch { name: name.into(), remote, current, ahead: 0, behind: 0 }
    }

    #[test]
    fn menu_lists_actions_then_branches() {
        let status = RepoStatus { branch: Some("main".into()), upstream: Some("origin/main".into()), changed: 2, ..Default::default() };
        let menu = menu_with(status, vec![branch("main", true, false), branch("feat", false, false), branch("origin/dev", false, true)]);
        let labels: Vec<&str> = menu.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            labels,
            ["Получить изменения (fetch)", "Обновить ветку (pull)", "Отправить (push)", "Закоммитить…", "Новая ветка…",
             "Локальные", "main", "feat", "Удалённые", "origin/dev"]
        );
    }

    #[test]
    fn search_filters_branches_and_skips_headers() {
        let status = RepoStatus { branch: Some("main".into()), ..Default::default() };
        let mut menu = menu_with(status, vec![branch("main", true, false), branch("feat/auth", false, false)]);
        menu.query = "фгер".into(); // «auth» в русской раскладке
        menu.rebuild();
        let labels: Vec<&str> = menu.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(labels, ["Локальные", "feat/auth"]);
        assert_eq!(menu.cursor, 1, "курсор на ветке, а не на заголовке");
    }

    #[test]
    fn conflicts_offer_claude_first() {
        let status = RepoStatus { branch: Some("main".into()), conflicts: 2, operation: Some(Operation::Merge), ..Default::default() };
        let menu = menu_with(status, vec![]);
        assert_eq!(menu.entries[1].label, "Пусть Claude разрешит конфликты");
    }

    #[test]
    fn diverged_pull_offers_merge_or_rebase() {
        let error = OpError { failure: Failure::Diverged, text: String::new() };
        let dialog = failure_dialog(&GitOp::Pull, &error).unwrap();
        let labels: Vec<&str> = dialog.buttons.iter().map(|(l, _)| l.trim()).collect();
        assert_eq!(labels, ["Слить (merge)", "Перебазировать (rebase)", "Отмена"]);
        assert!(failure_dialog(&GitOp::Fetch { quiet: true }, &error).is_none());
    }
}
