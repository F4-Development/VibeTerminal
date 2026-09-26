//! Главный цикл: терминал пользователя ↔ сессия Claude.

use std::io::{self, Stdout, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableFocusChange, EnableBracketedPaste, EnableFocusChange,
    Event as TermEvent, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, KeyboardEnhancementFlags,
    MouseEvent, MouseEventKind, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use ratatui::crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use ratatui::crossterm::{cursor, execute};
use ratatui::layout::Rect;

use crate::session::Session;
use crate::{keys, mouse, ui};

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

static KEYBOARD_ENHANCED: AtomicBool = AtomicBool::new(false);

pub enum Event {
    Term(TermEvent),
    Output(Vec<u8>),
    Exited,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Клавиши уходят в Claude.
    Agent,
    /// Клавиши управляют vv.
    Normal,
}

pub struct App {
    pub session: Session,
    pub mode: Mode,
    pub cwd: String,
    agent_area: Rect,
    /// Какой режим мыши включён в терминале пользователя: 1000, 1002 или 1003.
    outer_mouse: u16,
    quit: bool,
    killed: bool,
}

pub fn run() -> Result<()> {
    let cwd = std::env::current_dir().context("не могу определить текущую папку")?;
    setup_terminal()?;
    install_panic_hook();
    let result = run_app(&cwd);
    restore_terminal();

    if let Some(code) = result? {
        eprintln!("vv: claude завершился с кодом {code}");
    }
    Ok(())
}

/// Возвращает код выхода Claude, если он закрылся сам и с ошибкой.
fn run_app(cwd: &Path) -> Result<Option<u32>> {
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let (tx, rx) = mpsc::channel();

    let size = terminal.size()?;
    let area = ui::agent_area(Rect::new(0, 0, size.width, size.height));
    let output_tx = tx.clone();
    let session = Session::spawn(cwd, area.height, area.width, move |chunk| {
        let _ = output_tx.send(chunk.map_or(Event::Exited, Event::Output));
    })?;
    spawn_input_thread(tx);

    let mut app = App {
        session,
        mode: Mode::Agent,
        cwd: display_path(cwd),
        agent_area: area,
        outer_mouse: MOUSE_BASE_MODE,
        quit: false,
        killed: false,
    };

    let mut dirty = true;
    while !app.quit {
        let hold = app.session.frame_hold();
        if dirty && hold.is_none() {
            terminal.draw(|frame| ui::draw(frame, &app))?;
            dirty = false;
        }
        let event = if dirty {
            // Кадр Claude ещё не дорисован — ждём конец, но не дольше hold.
            match rx.recv_timeout(hold.unwrap_or(FRAME)) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            }
        } else {
            let Ok(event) = rx.recv() else { break };
            event
        };
        dirty |= app.handle(event, &mut terminal)?;

        // Собираем всё, что пришло следом, в один кадр.
        let started = Instant::now();
        while !app.quit {
            let left = FRAME.saturating_sub(started.elapsed());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left.min(SETTLE)) {
                Ok(event) => dirty |= app.handle(event, &mut terminal)?,
                Err(_) => break,
            }
        }
    }

    let code = app.session.wait();
    Ok(code.filter(|&code| code != 0 && !app.killed))
}

impl App {
    /// Возвращает `true`, если экран надо перерисовать.
    fn handle(&mut self, event: Event, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<bool> {
        match event {
            Event::Output(bytes) => {
                self.session.process(&bytes)?;
                let out = terminal.backend_mut();
                if self.session.take_bell() {
                    out.write_all(b"\x07")?;
                }
                self.sync_outer_mouse(out)?;
                Ok(true)
            }
            Event::Exited => {
                self.quit = true;
                Ok(false)
            }
            Event::Term(TermEvent::Key(key)) if key.kind != KeyEventKind::Release => self.on_key(key),
            Event::Term(TermEvent::Paste(text)) => {
                if self.mode == Mode::Agent {
                    self.session.scroll_to_bottom();
                    self.session.paste(&text)?;
                }
                Ok(true)
            }
            Event::Term(TermEvent::Mouse(mouse)) => self.on_mouse(mouse),
            Event::Term(TermEvent::FocusGained | TermEvent::FocusLost) if self.session.wants_focus_events() => {
                let gained = matches!(event, Event::Term(TermEvent::FocusGained));
                self.session.write(if gained { b"\x1b[I" } else { b"\x1b[O" })?;
                Ok(false)
            }
            Event::Term(TermEvent::Resize(width, height)) => {
                self.agent_area = ui::agent_area(Rect::new(0, 0, width, height));
                self.session.resize(self.agent_area.height, self.agent_area.width)?;
                terminal.autoresize()?;
                Ok(true)
            }
            Event::Term(_) => Ok(false),
        }
    }

    fn on_key(&mut self, key: KeyEvent) -> Result<bool> {
        if self.mode == Mode::Agent {
            if keys::is_prefix(&key) {
                self.mode = Mode::Normal;
            } else {
                self.session.scroll_to_bottom();
                let bytes = keys::encode(&key, self.session.screen().application_cursor());
                if !bytes.is_empty() {
                    self.session.write(&bytes)?;
                }
            }
            return Ok(true);
        }

        if keys::is_prefix(&key) {
            self.session.write(&[0x1c])?;
            self.mode = Mode::Agent;
            return Ok(true);
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let page = self.session.screen().size().0 as usize;
        match (keys::latin(key.code), ctrl) {
            (KeyCode::Char('q'), false) => {
                self.killed = true;
                self.session.kill();
                self.quit = true;
            }
            (KeyCode::Enter | KeyCode::Esc | KeyCode::Char('i'), false) => self.mode = Mode::Agent,
            (KeyCode::Char('u'), true) => self.session.scroll_up(page / 2),
            (KeyCode::Char('d'), true) => self.session.scroll_down(page / 2),
            // В полноэкранном режиме историю листает сам Claude.
            (KeyCode::PageUp | KeyCode::PageDown, _) if self.session.screen().alternate_screen() => {
                self.session.write(&keys::encode(&key, false))?;
            }
            (KeyCode::PageUp, _) => self.session.scroll_up(page.saturating_sub(1).max(1)),
            (KeyCode::PageDown, _) => self.session.scroll_down(page.saturating_sub(1).max(1)),
            (KeyCode::Char('G'), false) => self.session.scroll_to_bottom(),
            _ => return Ok(false),
        }
        Ok(true)
    }
}

impl App {
    fn on_mouse(&mut self, event: MouseEvent) -> Result<bool> {
        let screen = self.session.screen();
        let area = self.agent_area;
        let inside = event.column >= area.x
            && event.column < area.x + area.width
            && event.row >= area.y
            && event.row < area.y + area.height;
        if inside {
            let encoded = mouse::encode(
                &event,
                event.column - area.x,
                event.row - area.y,
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
            );
            if let Some(bytes) = encoded {
                self.session.write(&bytes)?;
                return Ok(false);
            }
        }
        // Claude мышь не слушает — колесо листает нашу историю.
        match event.kind {
            MouseEventKind::ScrollUp => self.session.scroll_up(WHEEL_LINES),
            MouseEventKind::ScrollDown => self.session.scroll_down(WHEEL_LINES),
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// Если Claude хочет получать движение мыши, включаем его и у нас.
    fn sync_outer_mouse(&mut self, out: &mut impl Write) -> Result<()> {
        let wanted = match self.session.screen().mouse_protocol_mode() {
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
}

fn spawn_input_thread(tx: Sender<Event>) {
    thread::spawn(move || {
        while let Ok(event) = event::read() {
            if tx.send(Event::Term(event)).is_err() {
                break;
            }
        }
    });
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
        restore_terminal();
        default_hook(info);
    }));
}

/// `/Users/me/Projects/x` → `~/Projects/x`.
fn display_path(path: &Path) -> String {
    let full = path.display().to_string();
    match std::env::var("HOME") {
        Ok(home) if !home.is_empty() && full.starts_with(&home) => format!("~{}", &full[home.len()..]),
        _ => full,
    }
}
