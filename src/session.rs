//! Одна сессия Claude Code: процесс в PTY и эмулятор его экрана.

use std::io::{ErrorKind, Read, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};

const SCROLLBACK_LINES: usize = 10_000;
/// Дольше этого не ждём конца кадра, даже если программа его не закрыла.
const SYNC_UPDATE_MAX: Duration = Duration::from_millis(100);
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Метки чужой сессии Claude, если vv запущен изнутри Claude Code. С ними
/// наш Claude считает себя дочерним и, например, не сохраняет историю.
/// `CLAUDE_CODE_SSE_PORT` не трогаем — это связь с IDE.
const PARENT_CLAUDE_VARS: &[&str] = &[
    "CLAUDECODE",
    "CLAUDE_PID",
    "CLAUDE_EFFORT",
    "CLAUDE_CODE_ENTRYPOINT",
    "CLAUDE_CODE_EXECPATH",
    "CLAUDE_CODE_SESSION_ID",
    "CLAUDE_CODE_CHILD_SESSION",
    "CLAUDE_CODE_SESSION_ATTENDED",
    "CLAUDE_CODE_MESSAGING_SOCKET",
    "CLAUDE_CODE_MESSAGING_TOKEN",
];

pub struct Session {
    parser: vt100::Parser<Term>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    sync_since: Option<Instant>,
}

impl Session {
    /// Запускает `claude` в `cwd`. Вывод процесса приходит в `on_output`
    /// из отдельного потока; `None` — процесс закрыл терминал.
    pub fn spawn(
        cwd: &Path,
        rows: u16,
        cols: u16,
        on_output: impl Fn(Option<Vec<u8>>) + Send + 'static,
    ) -> Result<Self> {
        let pty = native_pty_system().openpty(pty_size(rows, cols))?;

        // VV_CLAUDE — подменить команду (для отладки), например `VV_CLAUDE='seq 100; cat'`.
        let mut cmd = match std::env::var("VV_CLAUDE") {
            Ok(script) => {
                let mut cmd = CommandBuilder::new("sh");
                cmd.args(["-c", &script]);
                cmd
            }
            Err(_) => CommandBuilder::new("claude"),
        };
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "vibevim");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        for var in PARENT_CLAUDE_VARS {
            cmd.env_remove(var);
        }

        let child = pty
            .slave
            .spawn_command(cmd)
            .context("не получилось запустить claude — он установлен и есть в PATH?")?;
        drop(pty.slave);

        let mut reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;
        thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => on_output(Some(buf[..n].to_vec())),
                    Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
            on_output(None);
        });

        Ok(Self {
            parser: vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK_LINES, Term::default()),
            master: pty.master,
            writer,
            child,
            sync_since: None,
        })
    }

    pub fn screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    pub fn title(&self) -> &str {
        &self.parser.callbacks().title
    }

    /// Скармливает вывод Claude эмулятору и отвечает на запросы терминала.
    pub fn process(&mut self, bytes: &[u8]) -> Result<()> {
        self.parser.process(bytes);
        self.sync_since = match (self.sync_since, self.parser.callbacks().sync_update) {
            (None, true) => Some(Instant::now()),
            (since, true) => since,
            (_, false) => None,
        };
        let replies = std::mem::take(&mut self.parser.callbacks_mut().replies);
        if !replies.is_empty() {
            self.write(&replies)?;
        }
        Ok(())
    }

    /// Программа прислала начало кадра, но не конец (DEC 2026): сколько ещё
    /// подождать с отрисовкой, чтобы не показать кадр наполовину.
    pub fn frame_hold(&self) -> Option<Duration> {
        let left = SYNC_UPDATE_MAX.checked_sub(self.sync_since?.elapsed())?;
        (!left.is_zero()).then_some(left)
    }

    pub fn wants_focus_events(&self) -> bool {
        self.parser.callbacks().focus_events
    }

    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.parser.callbacks_mut().bell)
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    pub fn paste(&mut self, text: &str) -> Result<()> {
        let text = text.replace("\r\n", "\r").replace('\n', "\r");
        let text = text.replace("\x1b[201~", "");
        let bracketed = self.parser.screen().bracketed_paste();
        let mut data = Vec::with_capacity(text.len() + PASTE_START.len() + PASTE_END.len());
        if bracketed {
            data.extend_from_slice(PASTE_START);
        }
        data.extend_from_slice(text.as_bytes());
        if bracketed {
            data.extend_from_slice(PASTE_END);
        }
        self.write(&data)
    }

    pub fn resize(&mut self, rows: u16, cols: u16) -> Result<()> {
        if self.parser.screen().size() == (rows, cols) {
            return Ok(());
        }
        self.parser.screen_mut().set_size(rows, cols);
        self.master.resize(pty_size(rows, cols))?;
        Ok(())
    }

    pub fn scrollback(&self) -> usize {
        self.parser.screen().scrollback()
    }

    pub fn scroll_up(&mut self, lines: usize) {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(screen.scrollback() + lines);
    }

    pub fn scroll_down(&mut self, lines: usize) {
        let screen = self.parser.screen_mut();
        screen.set_scrollback(screen.scrollback().saturating_sub(lines));
    }

    pub fn scroll_to_bottom(&mut self) {
        self.parser.screen_mut().set_scrollback(0);
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    pub fn wait(&mut self) -> Option<u32> {
        self.child.wait().ok().map(|status| status.exit_code())
    }
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

/// Реакция эмулятора на то, что vt100 сам не обрабатывает.
#[derive(Default)]
struct Term {
    replies: Vec<u8>,
    title: String,
    bell: bool,
    focus_events: bool,
    sync_update: bool,
}

impl vt100::Callbacks for Term {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bell = true;
    }

    fn set_window_title(&mut self, _: &mut vt100::Screen, title: &[u8]) {
        self.title = String::from_utf8_lossy(title).into_owned();
    }

    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied();
        match (i1, c, first) {
            // Статус устройства: «всё в порядке».
            (None, 'n', Some(5)) => self.replies.extend_from_slice(b"\x1b[0n"),
            // Где курсор.
            (None, 'n', Some(6)) => {
                let (row, col) = screen.cursor_position();
                self.replies.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
            }
            // Кто ты: VT220 с цветами. Без ответа программы ждут таймаут.
            (None, 'c', _) => self.replies.extend_from_slice(b"\x1b[?62;22c"),
            (Some(b'>'), 'c', _) => self.replies.extend_from_slice(b"\x1b[>1;10;0c"),
            // Режимы, которых vt100 не знает: фокус окна и кадры целиком.
            (Some(b'?'), 'h' | 'l', _) => {
                for param in params {
                    match param.first() {
                        Some(1004) => self.focus_events = c == 'h',
                        Some(2026) => self.sync_update = c == 'h',
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parser() -> vt100::Parser<Term> {
        vt100::Parser::new_with_callbacks(5, 20, 0, Term::default())
    }

    #[test]
    fn answers_cursor_position_request() {
        let mut p = parser();
        p.process(b"ab\x1b[6n");
        assert_eq!(p.callbacks().replies, b"\x1b[1;3R");
    }

    #[test]
    fn answers_device_attributes() {
        let mut p = parser();
        p.process(b"\x1b[c\x1b[>c");
        assert_eq!(p.callbacks().replies, b"\x1b[?62;22c\x1b[>1;10;0c");
    }

    #[test]
    fn tracks_focus_and_synchronized_output_modes() {
        let mut p = parser();
        p.process(b"\x1b[?1004h\x1b[?2026h");
        assert!(p.callbacks().focus_events && p.callbacks().sync_update);
        p.process(b"\x1b[?2026l");
        assert!(p.callbacks().focus_events && !p.callbacks().sync_update);
    }

    #[test]
    fn remembers_window_title() {
        let mut p = parser();
        p.process(b"\x1b]0;my task\x07");
        assert_eq!(p.callbacks().title, "my task");
    }
}
