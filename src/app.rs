//! Главный цикл: терминал пользователя ↔ сессии Claude.
//!
//! Всё, что печатаешь, уходит в выбранного Claude. `Ctrl-\` открывает меню,
//! остальное — кнопками и мышью.

use std::io::{self, BufWriter, Stdout, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseButton, MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::Rect;
use signal_hook::consts::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;

use crate::menu::{self, Action};
use crate::picker::Picker;
use crate::session::{self, Session, SessionId};
use crate::ui::{self, Areas, contains};
use crate::{keys, mouse};

/// Кнопки и колесо мыши в формате SGR. Движение включаем, только если его
/// просит Claude (см. `sync_outer_mouse`).
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1006h";
const MOUSE_OFF: &[u8] = b"\x1b[?1003l\x1b[?1002l\x1b[?1006l\x1b[?1000l";
const MOUSE_BASE_MODE: u16 = 1000;
const WHEEL_LINES: usize = 3;
/// Не чаще 60 кадров в секунду.
const FRAME: Duration = Duration::from_millis(16);
/// Сколько ждать продолжения вывода, чтобы не рисовать кадр Claude наполовину.
const SETTLE: Duration = Duration::from_millis(4);
const FLASH_FOR: Duration = Duration::from_secs(5);

static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

/// Кадр копится в буфере и уходит в терминал одним куском.
type Screen = Terminal<CrosstermBackend<BufWriter<Stdout>>>;
const FRAME_BUFFER: usize = 256 * 1024;
/// Начало и конец кадра (DEC 2026): терминал покажет его целиком, без
/// промежуточных состояний. Курсор на время отрисовки прячем.
const FRAME_START: &[u8] = b"\x1b[?2026h\x1b[?25l";
const FRAME_END: &[u8] = b"\x1b[?2026l";

pub enum Event {
    Term(TermEvent),
    Output(SessionId, Vec<u8>),
    Exited(SessionId),
    /// Терминал пользователя пропал или vv попросили закрыться.
    Hangup,
}

/// Что сейчас поверх Claude. Пока что-то открыто, клавиши идут туда.
pub enum Overlay {
    None,
    /// Меню, в нём выбран пункт с этим номером.
    Menu(usize),
    Picker(Picker),
    Rename(String),
    Confirm(Confirm),
    Help,
}

pub enum Confirm {
    Close,
    Quit,
}

pub struct App {
    pub sessions: Vec<Session>,
    pub selected: usize,
    pub overlay: Overlay,
    pub areas: Areas,
    pub home: PathBuf,
    show_sidebar: bool,
    flash: Option<(String, Instant)>,
    next_id: SessionId,
    tx: Sender<Event>,
    /// Какой режим мыши включён в терминале пользователя: 1000, 1002 или 1003.
    outer_mouse: u16,
    /// Закрытые сессии, которые ещё гасятся в фоне.
    stopping: Vec<JoinHandle<()>>,
    quit: bool,
    /// Что сказать после выхода, если последний Claude закрылся с ошибкой.
    exit_note: Option<String>,
}

pub fn run() -> Result<()> {
    let cwd = std::env::current_dir().context("не могу определить текущую папку")?;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| cwd.clone());
    let (tx, rx) = mpsc::channel();
    spawn_signal_thread(tx.clone())?;

    setup_terminal()?;
    install_panic_hook();
    let mut terminal = Terminal::new(CrosstermBackend::new(BufWriter::with_capacity(FRAME_BUFFER, io::stdout())))?;
    let size = terminal.size()?;
    let mut app = App {
        sessions: Vec::new(),
        selected: 0,
        overlay: Overlay::None,
        areas: ui::layout(Rect::new(0, 0, size.width, size.height), true),
        home,
        show_sidebar: true,
        flash: None,
        next_id: 1,
        tx: tx.clone(),
        outer_mouse: MOUSE_BASE_MODE,
        stopping: Vec::new(),
        quit: false,
        exit_note: None,
    };

    let result = app.open(&cwd).and_then(|()| {
        spawn_input_thread(tx);
        app.run_loop(&mut terminal, &rx)
    });
    // Что бы ни случилось, Claude не должны пережить окно vv.
    app.stop_everything();
    restore_terminal();

    result?;
    if let Some(note) = app.exit_note {
        eprintln!("vv: {note}");
    }
    Ok(())
}

impl App {
    pub fn current(&self) -> &Session {
        &self.sessions[self.selected]
    }

    fn current_mut(&mut self) -> &mut Session {
        &mut self.sessions[self.selected]
    }

    pub fn flash(&self) -> Option<&str> {
        self.flash.as_ref().filter(|(_, at)| at.elapsed() < FLASH_FOR).map(|(text, _)| text.as_str())
    }

    fn set_flash(&mut self, text: impl Into<String>) {
        self.flash = Some((text.into(), Instant::now()));
    }

    fn run_loop(&mut self, terminal: &mut Screen, rx: &Receiver<Event>) -> Result<()> {
        let mut dirty = true;
        while !self.quit {
            let hold = self.current().frame_hold();
            if dirty && hold.is_none() {
                self.draw(terminal)?;
                dirty = false;
            }
            // Если висит сообщение, проснуться, чтобы его убрать.
            let wake = match (dirty, self.flash()) {
                (true, _) => Some(hold.unwrap_or(FRAME)),
                (false, Some(_)) => Some(FLASH_FOR),
                (false, None) => None,
            };
            let event = match wake {
                Some(timeout) => match rx.recv_timeout(timeout) {
                    Ok(event) => event,
                    Err(RecvTimeoutError::Timeout) => {
                        dirty = true;
                        continue;
                    }
                    Err(RecvTimeoutError::Disconnected) => break,
                },
                None => match rx.recv() {
                    Ok(event) => event,
                    Err(_) => break,
                },
            };
            dirty |= self.handle(event, terminal)?;

            // Собираем всё, что пришло следом, в один кадр.
            let started = Instant::now();
            while !self.quit {
                let left = FRAME.saturating_sub(started.elapsed());
                if left.is_zero() {
                    break;
                }
                match rx.recv_timeout(left.min(SETTLE)) {
                    Ok(event) => dirty |= self.handle(event, terminal)?,
                    Err(_) => break,
                }
            }
            if !self.quit {
                self.sync_outer_mouse(terminal.backend_mut())?;
            }
        }
        Ok(())
    }

    fn draw(&self, terminal: &mut Screen) -> Result<()> {
        terminal.backend_mut().write_all(FRAME_START)?;
        terminal.draw(|frame| ui::draw(frame, self))?;
        let out = terminal.backend_mut();
        out.write_all(FRAME_END)?;
        out.flush()?;
        Ok(())
    }

    /// Возвращает `true`, если экран надо перерисовать.
    fn handle(&mut self, event: Event, terminal: &mut Screen) -> Result<bool> {
        match event {
            Event::Output(id, bytes) => {
                let Some(index) = self.index_of(id) else { return Ok(false) };
                let session = &mut self.sessions[index];
                let title_before = session.title().to_string();
                session.process(&bytes)?;
                if session.take_bell() {
                    let out = terminal.backend_mut();
                    out.write_all(b"\x07")?;
                    out.flush()?;
                }
                Ok(index == self.selected || session.title() != title_before)
            }
            Event::Exited(id) => {
                self.on_exited(id);
                Ok(true)
            }
            Event::Hangup => {
                self.quit = true;
                Ok(false)
            }
            Event::Term(TermEvent::Key(key)) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Term(TermEvent::Paste(text)) => {
                match &mut self.overlay {
                    Overlay::Picker(picker) => picker.push(&text),
                    Overlay::Rename(input) => input.push_str(text.lines().next().unwrap_or("")),
                    Overlay::None => {
                        let session = self.current_mut();
                        session.scroll_to_bottom();
                        session.paste(&text)?;
                    }
                    _ => {}
                }
                Ok(true)
            }
            Event::Term(TermEvent::Mouse(mouse)) => self.on_mouse(mouse),
            Event::Term(TermEvent::FocusGained | TermEvent::FocusLost) => {
                let gained = matches!(event, Event::Term(TermEvent::FocusGained));
                self.send_focus(self.selected, gained)?;
                Ok(false)
            }
            Event::Term(TermEvent::Resize(width, height)) => {
                self.relayout(Rect::new(0, 0, width, height))?;
                terminal.autoresize()?;
                Ok(true)
            }
            Event::Term(_) => Ok(false),
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<bool> {
        if !matches!(self.overlay, Overlay::None) {
            return self.on_overlay_key(key);
        }
        if keys::is_prefix(&key) {
            self.overlay = Overlay::Menu(self.selected);
            return Ok(true);
        }
        let session = self.current_mut();
        session.scroll_to_bottom();
        let bytes = keys::encode(&key, session.screen().application_cursor());
        if !bytes.is_empty() {
            session.write(&bytes)?;
        }
        Ok(true)
    }

    fn on_overlay_key(&mut self, key: KeyEvent) -> Result<bool> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match &mut self.overlay {
            Overlay::None => {}
            Overlay::Help => self.overlay = Overlay::None,
            Overlay::Menu(cursor) => {
                let items = menu::items(&self.sessions, self.selected, self.areas.sidebar.is_some());
                match keys::latin(key.code) {
                    _ if keys::is_prefix(&key) => self.overlay = Overlay::None,
                    KeyCode::Esc => self.overlay = Overlay::None,
                    KeyCode::Up => *cursor = (*cursor + items.len() - 1) % items.len(),
                    KeyCode::Down | KeyCode::Tab => *cursor = (*cursor + 1) % items.len(),
                    KeyCode::Enter => {
                        let action = items[*cursor].action;
                        self.perform(action)?;
                    }
                    KeyCode::Char(c) if !ctrl => match menu::by_hotkey(&items, c) {
                        Some(action) => self.perform(action)?,
                        None => return Ok(false),
                    },
                    _ => return Ok(false),
                }
            }
            Overlay::Confirm(_) => match keys::latin(key.code) {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => self.confirm_yes(),
                KeyCode::Char('n' | 'N') | KeyCode::Esc => self.overlay = Overlay::None,
                _ => return Ok(false),
            },
            Overlay::Rename(input) => match key.code {
                KeyCode::Esc => self.overlay = Overlay::None,
                KeyCode::Enter => self.confirm_yes(),
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char('u') if ctrl => input.clear(),
                KeyCode::Char('w') if ctrl => {
                    let cut = input.trim_end().rfind(' ').map_or(0, |i| i + 1);
                    input.truncate(cut);
                }
                KeyCode::Char(c) if !ctrl => input.push(c),
                _ => return Ok(false),
            },
            Overlay::Picker(picker) => match (key.code, ctrl) {
                (KeyCode::Esc, _) => self.overlay = Overlay::None,
                (KeyCode::Enter, _) => self.open_picked(),
                (KeyCode::Up, _) | (KeyCode::Char('p' | 'k'), true) => picker.move_by(-1),
                (KeyCode::Down | KeyCode::Tab, _) | (KeyCode::Char('n' | 'j'), true) => picker.move_by(1),
                (KeyCode::Backspace, _) => picker.backspace(),
                (KeyCode::Char('u'), true) => picker.clear(),
                (KeyCode::Char('w'), true) => picker.delete_word(),
                (KeyCode::Char(c), false) => picker.push(c.encode_utf8(&mut [0; 4])),
                _ => return Ok(false),
            },
        }
        Ok(true)
    }

    /// То, что можно сделать из меню и кнопками.
    fn perform(&mut self, action: Action) -> Result<()> {
        self.overlay = Overlay::None;
        match action {
            Action::Select(index) => self.select(index)?,
            Action::New => self.overlay = Overlay::Picker(Picker::new(&self.home)),
            Action::Rename => self.overlay = Overlay::Rename(self.current().name.clone()),
            Action::Close => self.overlay = Overlay::Confirm(Confirm::Close),
            Action::Quit => self.overlay = Overlay::Confirm(Confirm::Quit),
            Action::Help => self.overlay = Overlay::Help,
            Action::ToggleSidebar => {
                self.show_sidebar = !self.show_sidebar;
                self.relayout(self.areas.full)?;
            }
        }
        Ok(())
    }

    /// «Да» в диалоге: сохранить имя, закрыть сессию или выйти.
    fn confirm_yes(&mut self) {
        match std::mem::replace(&mut self.overlay, Overlay::None) {
            Overlay::Rename(input) => {
                let name = input.trim();
                if !name.is_empty() && name != self.current().name {
                    let unique = self.unique_name(name);
                    self.current_mut().name = unique;
                }
            }
            Overlay::Confirm(Confirm::Close) => self.close_current(),
            Overlay::Confirm(Confirm::Quit) => self.quit = true,
            other => self.overlay = other,
        }
    }

    fn open_picked(&mut self) {
        let Overlay::Picker(picker) = &self.overlay else { return };
        let path = picker.selected_path();
        self.overlay = Overlay::None;
        if let Some(path) = path
            && let Err(err) = self.open(&path)
        {
            self.set_flash(format!("не открылось: {err:#}"));
        }
    }

    fn on_mouse(&mut self, event: MouseEvent) -> Result<bool> {
        let (column, row) = (event.column, event.row);
        let clicked = matches!(event.kind, MouseEventKind::Down(MouseButton::Left));
        let wheel = match event.kind {
            MouseEventKind::ScrollUp => -1,
            MouseEventKind::ScrollDown => 1,
            _ => 0,
        };
        let full = self.areas.full;

        match &mut self.overlay {
            Overlay::None => {}
            Overlay::Help => {
                if clicked {
                    self.overlay = Overlay::None;
                }
                return Ok(clicked);
            }
            Overlay::Menu(cursor) => {
                let items = menu::items(&self.sessions, self.selected, self.areas.sidebar.is_some());
                if wheel != 0 {
                    *cursor = (*cursor as isize + wheel).rem_euclid(items.len() as isize) as usize;
                    return Ok(true);
                }
                if !clicked {
                    return Ok(false);
                }
                let hit = ui::menu_rows(full, &items).iter().position(|r| contains(*r, column, row));
                match hit {
                    Some(i) => self.perform(items[i].action)?,
                    None if !ui::menu_contains(full, &items, column, row) => self.overlay = Overlay::None,
                    None => return Ok(false),
                }
                return Ok(true);
            }
            Overlay::Picker(picker) => {
                if wheel != 0 {
                    picker.move_by(wheel);
                    return Ok(true);
                }
                if !clicked {
                    return Ok(false);
                }
                let list = ui::picker_list(full);
                if contains(list, column, row) {
                    let index = ui::picker_offset(list, picker.selected) + (row - list.y) as usize;
                    if index < picker.len() {
                        picker.selected = index;
                        self.open_picked();
                    }
                } else if !ui::picker_contains(full, column, row) {
                    self.overlay = Overlay::None;
                }
                return Ok(true);
            }
            Overlay::Rename(_) | Overlay::Confirm(_) => {
                if !clicked {
                    return Ok(false);
                }
                let [yes, no] = ui::dialog_buttons(full, ui::dialog_labels(&self.overlay));
                if contains(yes, column, row) {
                    self.confirm_yes();
                } else if contains(no, column, row) {
                    self.overlay = Overlay::None;
                }
                return Ok(true);
            }
        }

        if clicked {
            if contains(ui::menu_button(self.areas.top), column, row) {
                self.overlay = Overlay::Menu(self.selected);
                return Ok(true);
            }
            if let Some(ui::Button { action, .. }) =
                ui::bottom_buttons(&self.areas).into_iter().find(|b| contains(b.rect, column, row))
            {
                self.perform(action)?;
                return Ok(true);
            }
            if let Some(sidebar) = self.areas.sidebar {
                let (cards, new_button) = ui::sidebar_parts(sidebar);
                if contains(new_button, column, row) {
                    self.perform(Action::New)?;
                    return Ok(true);
                }
                let offset = ui::cards_offset(cards, self.selected);
                if let Some(index) = ui::card_at(cards, offset, column, row).filter(|&i| i < self.sessions.len()) {
                    self.select(index)?;
                    return Ok(true);
                }
            }
        }

        let area = self.areas.agent;
        if contains(area, column, row) {
            let screen = self.current().screen();
            let encoded = mouse::encode(
                &event,
                column - area.x,
                row - area.y,
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
            );
            if let Some(bytes) = encoded {
                self.current_mut().write(&bytes)?;
                return Ok(false);
            }
            // Claude мышь не слушает — колесо листает нашу историю.
            match wheel {
                -1 => self.current_mut().scroll_up(WHEEL_LINES),
                1 => self.current_mut().scroll_down(WHEEL_LINES),
                _ => return Ok(false),
            }
            return Ok(true);
        }
        Ok(false)
    }

    fn open(&mut self, dir: &Path) -> Result<()> {
        let folder = dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned());
        let name = self.unique_name(&folder);
        let id = self.next_id;
        self.next_id += 1;
        let tx = self.tx.clone();
        let area = self.areas.agent;
        let session = Session::spawn(id, name, dir, area.height.max(1), area.width.max(1), move |chunk| {
            let _ = tx.send(chunk.map_or(Event::Exited(id), |bytes| Event::Output(id, bytes)));
        })?;
        self.sessions.push(session);
        self.select(self.sessions.len() - 1)
    }

    /// Закрыть выбранную сессию: Claude гасим в фоне, чтобы не ждать.
    fn close_current(&mut self) {
        let mut session = self.sessions.remove(self.selected);
        self.set_flash(format!("«{}» закрыта", session.name));
        self.stopping.push(thread::spawn(move || session::stop_all(std::slice::from_mut(&mut session))));
        self.after_removal();
    }

    /// Claude закрылся сам: `/exit` или упал.
    fn on_exited(&mut self, id: SessionId) {
        let Some(index) = self.index_of(id) else { return };
        let mut session = self.sessions.remove(index);
        let code = session.reap();
        let note = match code {
            0 => format!("«{}»: Claude закрылся", session.name),
            code => format!("«{}»: Claude завершился с кодом {code}", session.name),
        };
        if self.sessions.is_empty() && code != 0 {
            self.exit_note = Some(note.clone());
        }
        self.set_flash(note);
        if index < self.selected {
            self.selected -= 1;
        }
        // Меню и диалоги могли ссылаться на эту сессию.
        self.overlay = Overlay::None;
        self.after_removal();
    }

    fn after_removal(&mut self) {
        if self.sessions.is_empty() {
            self.quit = true;
            return;
        }
        self.selected = self.selected.min(self.sessions.len() - 1);
        let _ = self.send_focus(self.selected, true);
    }

    fn select(&mut self, index: usize) -> Result<()> {
        if index != self.selected && self.selected < self.sessions.len() {
            self.send_focus(self.selected, false)?;
        }
        self.selected = index;
        self.send_focus(index, true)
    }

    fn send_focus(&mut self, index: usize, gained: bool) -> Result<()> {
        if let Some(session) = self.sessions.get_mut(index)
            && session.wants_focus_events()
        {
            session.write(if gained { b"\x1b[I" } else { b"\x1b[O" })?;
        }
        Ok(())
    }

    fn relayout(&mut self, full: Rect) -> Result<()> {
        self.areas = ui::layout(full, self.show_sidebar);
        let agent = self.areas.agent;
        for session in &mut self.sessions {
            session.resize(agent.height.max(1), agent.width.max(1))?;
        }
        Ok(())
    }

    fn index_of(&self, id: SessionId) -> Option<usize> {
        self.sessions.iter().position(|s| s.id == id)
    }

    /// `shop-api`, а если занято — `shop-api-2`, `shop-api-3`…
    fn unique_name(&self, base: &str) -> String {
        let taken = |name: &str| self.sessions.iter().any(|s| s.name == name);
        if !taken(base) {
            return base.to_string();
        }
        (2..).map(|n| format!("{base}-{n}")).find(|name| !taken(name)).unwrap()
    }

    /// Если Claude хочет получать движение мыши, включаем его и у нас.
    fn sync_outer_mouse(&mut self, out: &mut impl Write) -> Result<()> {
        let wanted = match self.current().screen().mouse_protocol_mode() {
            vt100::MouseProtocolMode::ButtonMotion => 1002,
            vt100::MouseProtocolMode::AnyMotion => 1003,
            _ => MOUSE_BASE_MODE,
        };
        if wanted != self.outer_mouse {
            if self.outer_mouse != MOUSE_BASE_MODE {
                write!(out, "\x1b[?{}l", self.outer_mouse)?;
            }
            if wanted != MOUSE_BASE_MODE {
                write!(out, "\x1b[?{wanted}h")?;
            }
            out.flush()?;
            self.outer_mouse = wanted;
        }
        Ok(())
    }

    fn stop_everything(&mut self) {
        session::stop_all(&mut self.sessions);
        for handle in self.stopping.drain(..) {
            let _ = handle.join();
        }
    }
}

fn spawn_input_thread(tx: Sender<Event>) {
    thread::spawn(move || {
        loop {
            match event::read() {
                Ok(event) => {
                    if tx.send(Event::Term(event)).is_err() {
                        break;
                    }
                }
                Err(_) => {
                    let _ = tx.send(Event::Hangup);
                    break;
                }
            }
        }
    });
}

/// Закрыли окно терминала (SIGHUP) или попросили выйти — гасим всех Claude.
fn spawn_signal_thread(tx: Sender<Event>) -> Result<()> {
    let mut signals = Signals::new([SIGHUP, SIGTERM, SIGINT, SIGQUIT])?;
    thread::spawn(move || {
        if signals.forever().next().is_some() {
            let _ = tx.send(Event::Hangup);
        }
    });
    Ok(())
}

fn setup_terminal() -> Result<()> {
    terminal::enable_raw_mode()?;
    // Kitty-протокол различает Shift+Enter и Enter, а Esc не путает с началом
    // последовательности. Включаем, только если терминал его знает.
    let enhanced = matches!(terminal::supports_keyboard_enhancement(), Ok(true));
    KEYBOARD_ENHANCED.store(enhanced, Ordering::Relaxed);

    let mut out = io::stdout();
    execute!(out, EnterAlternateScreen, EnableBracketedPaste, EnableFocusChange)?;
    out.write_all(MOUSE_ON)?;
    if enhanced {
        execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
    }
    out.flush()?;
    Ok(())
}

fn restore_terminal() {
    let mut out = io::stdout();
    if KEYBOARD_ENHANCED.load(Ordering::Relaxed) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = out.write_all(MOUSE_OFF);
    let _ = execute!(out, DisableFocusChange, DisableBracketedPaste, LeaveAlternateScreen, cursor::Show);
    let _ = terminal::disable_raw_mode();
}

fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        session::hangup_all_live();
        restore_terminal();
        default_hook(info);
    }));
}
