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
use crate::session::{self, Launch, Session, SessionId};
use crate::caps::Caps;
use crate::hooks::{Decision, HookEvent, HookServer, PermissionRequest};
use crate::ui::{self, Areas, Target, contains};
use crate::{keys, mouse};

/// Кнопки, колесо и движение мыши в формате SGR. Движение нужно, чтобы
/// подсвечивать то, что под мышью; Claude получает его, только если просил.
const MOUSE_ON: &[u8] = b"\x1b[?1000h\x1b[?1003h\x1b[?1006h";
const MOUSE_OFF: &[u8] = b"\x1b[?1003l\x1b[?1002l\x1b[?1006l\x1b[?1000l";
/// Тонкая мигающая черта — привычный текстовый курсор, если Claude не попросил другой.
const CURSOR_BAR: u16 = 5;
const WHEEL_LINES: usize = 3;
/// Не чаще 60 кадров в секунду.
const FRAME: Duration = Duration::from_millis(16);
/// Сколько ждать продолжения вывода, чтобы не рисовать кадр Claude наполовину.
const SETTLE: Duration = Duration::from_millis(4);
const FLASH_FOR: Duration = Duration::from_secs(5);

static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);
static POINTER_SHAPES: AtomicBool = AtomicBool::new(false);

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
    /// Claude в сессии просит разрешение и ждёт ответа.
    Permission(SessionId, PermissionRequest),
    /// Хук запроса закрылся сам — Claude больше не ждёт.
    PermissionGone(SessionId, u64),
    /// Claude сообщил, что сделал: выполнил инструмент, закончил ответ…
    Hook(SessionId, HookEvent),
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
    pub caps: Caps,
    launch: Launch,
    /// Что сейчас под мышью — подсвечиваем.
    pub hover: Option<Target>,
    /// Какая форма указателя мыши выставлена в терминале.
    pointer: &'static str,
    /// Какая форма текстового курсора выставлена в терминале.
    cursor_shape: u16,
    /// Заголовок окна терминала: имя открытой сессии.
    window_title: String,
    /// Закрытые сессии, которые ещё гасятся в фоне.
    stopping: Vec<JoinHandle<()>>,
    quit: bool,
}

pub fn run() -> Result<()> {
    let cwd = std::env::current_dir().context("не могу определить текущую папку")?;
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| cwd.clone());
    let (tx, rx) = mpsc::channel();
    spawn_signal_thread(tx.clone())?;
    let caps = Caps::detect();
    POINTER_SHAPES.store(caps.pointer_shape, Ordering::Relaxed);
    // Сокет живёт, пока живёт окно, и убирается при выходе.
    let hooks = HookServer::start(&home.join(".vibeterminal/run"), tx.clone())?;
    let launch = Launch {
        truecolor: caps.truecolor,
        socket: hooks.path.clone(),
        vv_exe: std::env::current_exe().context("не знаю, где лежит vv")?,
    };

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
        caps,
        launch,
        hover: None,
        pointer: "",
        cursor_shape: 0,
        window_title: String::new(),
        stopping: Vec::new(),
        quit: false,
    };

    // Из Dock приложение стартует в домашней папке — там Claude не нужен,
    // встречаем выбором проекта. Из терминала в папке проекта — сразу Claude.
    let started = if cwd == app.home || cwd == Path::new("/") {
        app.overlay = Overlay::Picker(Picker::new(&app.home));
        Ok(())
    } else {
        app.open(&cwd)
    };
    let result = started.and_then(|()| {
        spawn_input_thread(tx);
        app.run_loop(&mut terminal, &rx)
    });
    // Что бы ни случилось, Claude не должны пережить окно vv.
    app.stop_everything();
    restore_terminal();

    result
}

impl App {
    /// Открытая сессия. Нет ни одной — на экране выбор проекта.
    pub fn current(&self) -> Option<&Session> {
        self.sessions.get(self.selected)
    }

    fn current_mut(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.selected)
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
            let hold = self.current().and_then(Session::frame_hold);
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
        }
        Ok(())
    }

    fn draw(&mut self, terminal: &mut Screen) -> Result<()> {
        terminal.backend_mut().write_all(FRAME_START)?;
        let this = &*self;
        terminal.draw(|frame| ui::draw(frame, this))?;
        let out = terminal.backend_mut();
        let title = match self.current() {
            Some(session) => format!("{} — VibeTerminal", session.name),
            None => "VibeTerminal".to_string(),
        };
        if title != self.window_title {
            write!(out, "\x1b]0;{title}\x07")?;
            self.window_title = title;
        }
        let shape = self.current().and_then(Session::cursor_style).unwrap_or(CURSOR_BAR);
        if shape != self.cursor_shape {
            write!(out, "\x1b[{shape} q")?;
            self.cursor_shape = shape;
        }
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
            Event::Permission(id, request) => {
                let Some(index) = self.index_of(id) else {
                    request.answer(Decision::AsUsual);
                    return Ok(false);
                };
                let session = &mut self.sessions[index];
                session.permissions.push_back(request);
                if index != self.selected {
                    let note = format!("«{}» просит разрешение", session.name);
                    self.set_flash(note);
                }
                let out = terminal.backend_mut();
                out.write_all(b"\x07")?;
                out.flush()?;
                Ok(true)
            }
            Event::PermissionGone(id, request) => {
                let Some(index) = self.index_of(id) else { return Ok(false) };
                self.sessions[index].permissions.retain(|r| r.id != request);
                Ok(true)
            }
            Event::Hook(id, event) => {
                let Some(index) = self.index_of(id) else { return Ok(false) };
                Ok(self.sessions[index].resolve_permissions(&event))
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
                        if let Some(session) = self.current_mut() {
                            session.scroll_to_bottom();
                            session.paste(&text)?;
                        }
                    }
                    _ => {}
                }
                Ok(true)
            }
            Event::Term(TermEvent::Mouse(mouse)) => self.on_mouse(mouse, terminal.backend_mut()),
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
        let Some(session) = self.current_mut() else { return Ok(false) };
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
            Action::Rename => {
                if let Some(session) = self.current() {
                    self.overlay = Overlay::Rename(session.name.clone());
                }
            }
            Action::Close if !self.sessions.is_empty() => self.overlay = Overlay::Confirm(Confirm::Close),
            Action::Close => {}
            // Нечего останавливать — выходим без вопроса.
            Action::Quit if self.sessions.is_empty() => self.quit = true,
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
                let current = self.current().map(|s| s.name.clone());
                if !name.is_empty() && current.is_some_and(|c| c != name) {
                    let unique = self.unique_name(name);
                    if let Some(session) = self.current_mut() {
                        session.name = unique;
                    }
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

    fn on_mouse(&mut self, event: MouseEvent, out: &mut impl Write) -> Result<bool> {
        let (column, row) = (event.column, event.row);
        let target = ui::target_at(self, column, row);
        let mut dirty = target != self.hover;
        self.hover = target;
        let in_agent = matches!(self.overlay, Overlay::None) && contains(self.areas.agent, column, row);
        let pointer = match (target, in_agent) {
            (Some(_), _) => "pointer",
            (None, true) => "text",
            (None, false) => "default",
        };
        self.set_pointer(pointer, out)?;

        match event.kind {
            // Меню и список папок выделяют пункт под мышью.
            MouseEventKind::Moved | MouseEventKind::Drag(_) => match (&mut self.overlay, target) {
                (Overlay::Menu(cursor), Some(Target::MenuItem(i))) if *cursor != i => {
                    *cursor = i;
                    dirty = true;
                }
                (Overlay::Picker(picker), Some(Target::PickerItem(i))) if picker.selected != i => {
                    picker.selected = i;
                    dirty = true;
                }
                _ => {}
            },
            MouseEventKind::Down(MouseButton::Left) => {
                if let Some(target) = target {
                    self.click(target)?;
                    self.hover = None;
                    return Ok(true);
                }
                // Клик мимо меню, списка папок или помощи закрывает их.
                let full = self.areas.full;
                let outside = match &self.overlay {
                    Overlay::Help => true,
                    Overlay::Menu(_) => {
                        let items = menu::items(&self.sessions, self.selected, self.areas.sidebar.is_some());
                        !ui::menu_contains(full, &items, column, row)
                    }
                    Overlay::Picker(_) => !ui::picker_contains(full, column, row),
                    _ => false,
                };
                if outside {
                    self.overlay = Overlay::None;
                    return Ok(true);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let delta = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                match &mut self.overlay {
                    Overlay::Menu(cursor) => {
                        let len = menu::items(&self.sessions, self.selected, self.areas.sidebar.is_some()).len();
                        *cursor = (*cursor as isize + delta).rem_euclid(len as isize) as usize;
                        return Ok(true);
                    }
                    Overlay::Picker(picker) => {
                        picker.move_by(delta);
                        return Ok(true);
                    }
                    _ => {}
                }
            }
            _ => {}
        }

        if !in_agent {
            return Ok(dirty);
        }
        let area = self.areas.agent;
        let Some(screen) = self.current().map(Session::screen) else { return Ok(dirty) };
        let encoded =
            mouse::encode(&event, column - area.x, row - area.y, screen.mouse_protocol_mode(), screen.mouse_protocol_encoding());
        if let Some(bytes) = encoded {
            if let Some(session) = self.current_mut() {
                session.write(&bytes)?;
            }
            return Ok(dirty);
        }
        // Claude мышь не слушает — колесо листает нашу историю.
        match event.kind {
            MouseEventKind::ScrollUp => self.current_mut().into_iter().for_each(|s| s.scroll_up(WHEEL_LINES)),
            MouseEventKind::ScrollDown => self.current_mut().into_iter().for_each(|s| s.scroll_down(WHEEL_LINES)),
            _ => return Ok(dirty),
        }
        Ok(true)
    }

    fn click(&mut self, target: Target) -> Result<()> {
        match target {
            Target::MenuButton => self.overlay = Overlay::Menu(self.selected),
            Target::NewSession => self.perform(Action::New)?,
            Target::Card(index) => self.select(index)?,
            Target::Bottom(action) => self.perform(action)?,
            Target::MenuItem(index) => {
                let items = menu::items(&self.sessions, self.selected, self.areas.sidebar.is_some());
                self.perform(items[index].action)?;
            }
            Target::PickerItem(index) => {
                if let Overlay::Picker(picker) = &mut self.overlay {
                    picker.selected = index;
                }
                self.open_picked();
            }
            Target::DialogYes => self.confirm_yes(),
            Target::DialogNo => self.overlay = Overlay::None,
            Target::Permit(index, decision) => self.answer_permission(index, decision),
        }
        Ok(())
    }

    /// «Рука» над кнопками — там, где терминал умеет менять указатель.
    fn set_pointer(&mut self, shape: &'static str, out: &mut impl Write) -> Result<()> {
        if self.caps.pointer_shape && shape != self.pointer {
            write!(out, "\x1b]22;{shape}\x1b\\")?;
            out.flush()?;
            self.pointer = shape;
        }
        Ok(())
    }

    fn answer_permission(&mut self, index: usize, decision: Decision) {
        let Some(session) = self.sessions.get_mut(index) else { return };
        if let Some(request) = session.permissions.pop_back() {
            request.answer(decision);
        }
    }

    fn open(&mut self, dir: &Path) -> Result<()> {
        let folder = dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned());
        let name = self.unique_name(&folder);
        let id = self.next_id;
        self.next_id += 1;
        let tx = self.tx.clone();
        let area = self.areas.agent;
        let (rows, cols) = (area.height.max(1), area.width.max(1));
        let session = Session::spawn(id, name, dir, rows, cols, &self.launch, move |chunk| {
            let _ = tx.send(chunk.map_or(Event::Exited(id), |bytes| Event::Output(id, bytes)));
        })?;
        self.sessions.push(session);
        self.select(self.sessions.len() - 1)
    }

    /// Закрыть выбранную сессию: Claude гасим в фоне, чтобы не ждать.
    fn close_current(&mut self) {
        if self.selected >= self.sessions.len() {
            return;
        }
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
        self.set_flash(note);
        if index < self.selected {
            self.selected -= 1;
        }
        // Меню и диалоги могли ссылаться на эту сессию.
        self.overlay = Overlay::None;
        self.after_removal();
    }

    fn after_removal(&mut self) {
        // Закрылась последняя — снова выбор проекта, окно не закрываем.
        if self.sessions.is_empty() {
            self.selected = 0;
            self.overlay = Overlay::Picker(Picker::new(&self.home));
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
    // Курсор и указатель — как были до vv.
    let _ = out.write_all(b"\x1b[0 q");
    if POINTER_SHAPES.load(Ordering::Relaxed) {
        let _ = out.write_all(b"\x1b]22;default\x1b\\");
    }
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
