//! Одна сессия Claude Code: процесс в PTY и эмулятор его экрана.

use std::collections::VecDeque;
use std::fs::File;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use portable_pty::{Child, ChildKiller, CommandBuilder, ExitStatus, MasterPty, PtySize, native_pty_system};

use crate::ci::CiState;
use crate::git::RepoStatus;
use crate::hooks::{self, Decision, HookEvent, PermissionRequest, StatusLine};
use crate::reload::HandedSession;
use crate::status::{State, Status};
use crate::worktree;

const SCROLLBACK_LINES: usize = 10_000;
/// Дольше этого не ждём конца кадра, даже если программа его не закрыла.
const SYNC_UPDATE_MAX: Duration = Duration::from_millis(100);
/// Сколько даём Claude на аккуратный выход, прежде чем убить.
const STOP_GRACE: Duration = Duration::from_millis(1500);
/// По этим надписям узнаём родной диалог Claude: разрешение, вопрос, план.
const PROMPT_MARKS: [&str; 3] = ["Esc to cancel", "Do you want to", "Would you like to proceed"];
/// Подготовка копии проекта перед Claude: команда из `VV_SETUP`, потом
/// то, что передано аргументами. Не вышло — Claude всё равно запустится,
/// а ошибка останется на экране выше.
const SETUP_SCRIPT: &str = r#"printf '\033[2m  Подготовка копии: %s\033[0m\n\n' "$VV_SETUP"
sh -c "$VV_SETUP"
code=$?
[ $code -ne 0 ] && printf '\n\033[31m  Подготовка не удалась (код %s). Запускаю Claude.\033[0m\n' "$code"
printf '\n'
exec "$@""#;
const PASTE_START: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// Метки чужой сессии Claude, если vv запущен изнутри Claude Code. С ними
/// наш Claude считает себя дочерним и, например, не сохраняет историю.
/// `CLAUDE_CODE_SSE_PORT` не трогаем — это связь с IDE.
pub(crate) const PARENT_CLAUDE_VARS: &[&str] = &[
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

/// Живые процессы Claude — чтобы погасить их, даже если vv упал.
static LIVE_PIDS: Mutex<Vec<i32>> = Mutex::new(Vec::new());

pub type SessionId = u64;

/// Что запускается в сессии.
pub enum Program {
    /// Claude с этими флагами.
    Claude(Vec<String>),
    /// Сначала команда подготовки (`npm install` в новой копии проекта) —
    /// на виду, в этом же окне, — потом Claude с этими флагами.
    ClaudeAfter(String, Vec<String>),
    /// Просто команда — например, вход в glab.
    Command(String),
}

/// Как запускать Claude в этом окне vv.
pub struct Launch {
    pub truecolor: bool,
    /// Терминал понимает ссылки OSC 8 (см. `Caps::hyperlinks`).
    pub hyperlinks: bool,
    /// Сокет окна vv, куда стучатся хуки.
    pub socket: PathBuf,
    pub vv_exe: PathBuf,
}

pub struct Session {
    pub id: SessionId,
    pub name: String,
    pub cwd: PathBuf,
    /// Сессия в отдельной копии этого проекта (git worktree).
    pub copy_of: Option<PathBuf>,
    /// Чья сессия в списке: папка проекта, у копии — проекта, с которого
    /// она сделана. Копии стоят деревом под своим проектом.
    pub project: PathBuf,
    /// Запросы разрешения, которые ждут ответа. Показываем последний.
    pub permissions: VecDeque<PermissionRequest>,
    /// Git в папке сессии; `None` — не репозиторий или ещё не узнали.
    pub git: Option<RepoStatus>,
    /// Что git делает сейчас в фоне («отправляю»).
    pub git_busy: Option<&'static str>,
    pub git_refreshing: bool,
    pub last_fetch: Option<Instant>,
    /// CI ветки; `None` — ещё не узнали.
    pub ci: Option<CiState>,
    pub ci_refreshing: bool,
    /// Когда проверить CI снова; `None` — при первой возможности.
    pub ci_due: Option<Instant>,
    /// Ветку только что отправили: ждём пайплайн на этот коммит (и с каких пор).
    pub ci_expect: Option<(String, Instant)>,
    /// Сессия-команда (вход, установка), а не Claude.
    pub is_command: bool,
    /// Работает, готово, прервали — по событиям Claude.
    pub status: Status,
    /// Заполнение контекста и лимиты из строки состояния Claude.
    pub status_line: Option<StatusLine>,
    /// Диалог Claude и его файл — по ним сессия продолжится после перезапуска.
    pub claude_id: Option<String>,
    pub transcript: Option<PathBuf>,
    parser: vt100::Parser<Term>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    pid: i32,
    exit_code: Option<u32>,
    sync_since: Option<Instant>,
    prompt: PromptWatch,
}

/// Следим за диалогом Claude, пока ждём ответа на запрос. Отвечают в нём
/// нажатиями в сессии, и событие от Claude приходит, только когда команда
/// закончится, — а она может идти минутами.
#[derive(Default)]
struct PromptWatch {
    /// Диалог был на экране.
    seen: bool,
    /// После этого в сессию что-то нажали.
    touched: bool,
}

impl Session {
    /// Запускает `claude` в `cwd`. Вывод процесса приходит в `on_output`
    /// из отдельного потока; `None` — процесс закрыл терминал.
    pub fn spawn(
        id: SessionId,
        name: String,
        cwd: &Path,
        (rows, cols): (u16, u16),
        launch: &Launch,
        program: &Program,
        on_output: impl Fn(Option<Vec<u8>>) + Send + 'static,
    ) -> Result<Self> {
        let pty = native_pty_system().openpty(pty_size(rows, cols))?;

        // VV_CLAUDE — подменить команду (для отладки), например `VV_CLAUDE='seq 100; cat'`.
        let shell = |script: &str| {
            let mut cmd = CommandBuilder::new("sh");
            cmd.args(["-c", script]);
            cmd
        };
        let debug = std::env::var("VV_CLAUDE").ok();
        let mut cmd = match (program, debug.as_ref()) {
            (Program::Command(script), _) | (Program::Claude(_) | Program::ClaudeAfter(..), Some(script)) => shell(script),
            (Program::Claude(claude_args) | Program::ClaudeAfter(_, claude_args), None) => {
                let mut cmd = CommandBuilder::new("claude");
                cmd.args(["--settings", &hooks::settings_json(&launch.vv_exe)]);
                // Модель, режим разрешений и флаги из настроек.
                cmd.args(claude_args);
                // VV_CLAUDE_ARGS — дополнительные флаги claude (для отладки).
                if let Ok(extra) = std::env::var("VV_CLAUDE_ARGS") {
                    cmd.args(extra.split_whitespace());
                }
                cmd
            }
        };
        if let Program::ClaudeAfter(setup, _) = program {
            let mut prepare = shell(SETUP_SCRIPT);
            prepare.arg("vv");
            prepare.args(cmd.get_argv());
            prepare.env("VV_SETUP", setup);
            cmd = prepare;
        }
        cmd.cwd(cwd);
        cmd.env("TERM", "xterm-256color");
        // Говорим Claude правду о цветах: без 24-битного цвета он сам выберет
        // палитру из 256, которую понимает терминал.
        if launch.truecolor {
            cmd.env("COLORTERM", "truecolor");
        } else {
            cmd.env_remove("COLORTERM");
        }
        // Ссылки: Claude узнаёт терминалы по TERM_PROGRAM, а тут он видит
        // vv. Понимает терминал ссылки — пусть выводит их ссылками (vv
        // передаст), нет — адресом текстом, чтобы адрес не потерялся.
        cmd.env("FORCE_HYPERLINK", if launch.hyperlinks { "1" } else { "0" });
        cmd.env("TERM_PROGRAM", "vibeterminal");
        cmd.env("TERM_PROGRAM_VERSION", env!("CARGO_PKG_VERSION"));
        cmd.env(hooks::ENV_SESSION, id.to_string());
        cmd.env(hooks::ENV_SOCKET, &launch.socket);
        for var in PARENT_CLAUDE_VARS {
            cmd.env_remove(var);
        }

        let child = pty
            .slave
            .spawn_command(cmd)
            .context("не получилось запустить claude — он установлен и есть в PATH?")?;
        drop(pty.slave);
        let pid = child.process_id().context("у claude нет pid")? as i32;
        LIVE_PIDS.lock().unwrap().push(pid);

        let reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;
        read_output(reader, on_output);
        let is_command = matches!(program, Program::Command(_));
        Ok(Self::assemble(id, name, cwd, (rows, cols), pty.master, writer, child, pid, is_command))
    }

    /// Новая версия vv после перезагрузки принимает сессию старой: терминал
    /// уже открыт, Claude работает. Экран и режимы — как были.
    pub fn adopt(
        handed: &HandedSession,
        (rows, cols): (u16, u16),
        on_output: impl Fn(Option<Vec<u8>>) + Send + 'static,
    ) -> Result<Self> {
        // SAFETY: fd открыт старой версией и передан нам через `exec`;
        // проверяем, что он живой, прежде чем владеть им.
        if unsafe { libc::fcntl(handed.fd, libc::F_GETFD) } == -1 {
            anyhow::bail!("терминал сессии «{}» не дошёл", handed.name);
        }
        let fd = unsafe { OwnedFd::from_raw_fd(handed.fd) };
        // SAFETY: fcntl на своём fd.
        unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) };
        let master = AdoptedMaster { fd };
        let reader = master.try_clone_reader()?;
        let writer = master.take_writer()?;
        read_output(reader, on_output);
        LIVE_PIDS.lock().unwrap().push(handed.pid);
        let child = Box::new(AdoptedChild { pid: handed.pid });
        let path = Path::new(&handed.cwd);
        let mut session = Self::assemble(
            handed.id,
            handed.name.clone(),
            path,
            (rows, cols),
            Box::new(master),
            writer,
            child,
            handed.pid,
            handed.is_command,
        );
        session.parser.process(handed.screen.as_bytes());
        session.status = Status::restored(State::from_name(&handed.state), handed.detail.clone(), handed.since_secs);
        session.claude_id.clone_from(&handed.claude_id);
        session.transcript.clone_from(&handed.transcript);
        Ok(session)
    }

    /// Отдать сессию новой версии vv: терминал остаётся открытым через
    /// `exec`, экран и режимы — в виде байтов для vt100.
    pub fn handoff(&self) -> Option<HandedSession> {
        let fd = self.master.as_raw_fd()?;
        // Терминал должен пережить `exec`.
        // SAFETY: fcntl на своём fd.
        unsafe { libc::fcntl(fd, libc::F_SETFD, 0) };
        let screen = self.parser.screen();
        let mut state = Vec::new();
        if screen.alternate_screen() {
            state.extend_from_slice(b"\x1b[?1049h");
        }
        state.extend(screen.state_formatted());
        let term = self.parser.callbacks();
        if term.focus_events {
            state.extend_from_slice(b"\x1b[?1004h");
        }
        if let Some(style) = term.cursor_style {
            state.extend(format!("\x1b[{style} q").into_bytes());
        }
        if !term.title.is_empty() {
            state.extend(format!("\x1b]2;{}\x07", term.title.replace(['\x07', '\x1b'], "")).into_bytes());
        }
        Some(HandedSession {
            id: self.id,
            name: self.name.clone(),
            cwd: self.cwd.clone(),
            fd,
            pid: self.pid,
            is_command: self.is_command,
            claude_id: self.claude_id.clone(),
            transcript: self.transcript.clone(),
            state: self.status.state.name().to_string(),
            detail: self.status.detail.clone(),
            since_secs: self.status.since.elapsed().as_secs(),
            screen: String::from_utf8_lossy(&state).into_owned(),
        })
    }

    /// Попросить программу перерисоваться целиком: размер туда и обратно.
    pub fn redraw(&mut self) {
        let (rows, cols) = self.parser.screen().size();
        let _ = self.master.resize(pty_size(rows, cols.saturating_sub(1).max(1)));
        let _ = self.master.resize(pty_size(rows, cols));
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble(
        id: SessionId,
        name: String,
        cwd: &Path,
        (rows, cols): (u16, u16),
        master: Box<dyn MasterPty + Send>,
        writer: Box<dyn Write + Send>,
        child: Box<dyn Child + Send + Sync>,
        pid: i32,
        is_command: bool,
    ) -> Self {
        let copy_of = worktree::main_repo(cwd);
        Self {
            id,
            name,
            cwd: cwd.to_path_buf(),
            copy_of: copy_of.clone(),
            project: worktree::canonical(copy_of.as_deref().unwrap_or(cwd)),
            permissions: VecDeque::new(),
            git: None,
            git_busy: None,
            git_refreshing: false,
            last_fetch: None,
            ci: None,
            ci_refreshing: false,
            ci_due: None,
            ci_expect: None,
            is_command,
            status: Status::default(),
            status_line: None,
            claude_id: None,
            transcript: None,
            parser: vt100::Parser::new_with_callbacks(rows, cols, SCROLLBACK_LINES, Term::default()),
            master,
            writer,
            child,
            pid,
            exit_code: None,
            sync_since: None,
            prompt: PromptWatch::default(),
        }
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
            self.send(&replies)?;
        }
        Ok(())
    }

    /// Программа прислала начало кадра, но не конец (DEC 2026): сколько ещё
    /// подождать с отрисовкой, чтобы не показать кадр наполовину.
    pub fn frame_hold(&self) -> Option<Duration> {
        let left = SYNC_UPDATE_MAX.checked_sub(self.sync_since?.elapsed())?;
        (!left.is_zero()).then_some(left)
    }

    /// Claude сообщил свой диалог (он меняется после `/clear`). `true` — новый.
    pub fn note_dialog(&mut self, id: &str, transcript: &str) -> bool {
        if id.is_empty() || self.claude_id.as_deref() == Some(id) {
            return false;
        }
        self.claude_id = Some(id.to_string());
        self.transcript = (!transcript.is_empty()).then(|| PathBuf::from(transcript));
        true
    }

    /// На карточке идут секунды — перерисовывать почаще.
    pub fn ticking(&self) -> bool {
        let since = match self.permissions.front() {
            Some(request) => request.at,
            None if self.status.state != State::Idle => self.status.since,
            None => return false,
        };
        since.elapsed() < Duration::from_secs(60)
    }

    /// Запрос, который сейчас показываем: самый свежий.
    pub fn pending_permission(&self) -> Option<&PermissionRequest> {
        self.permissions.back()
    }

    /// Убирает запросы, на которые уже ответили в окне Claude: инструмент
    /// выполнился или получил отказ, либо Claude закончил или получил новое
    /// сообщение. Возвращает `true`, если что-то убрали.
    pub fn resolve_permissions(&mut self, event: &HookEvent) -> bool {
        let before = self.permissions.len();
        let answered: Vec<PermissionRequest> = match event.name.as_str() {
            "Stop" | "StopFailure" | "UserPromptSubmit" => self.permissions.drain(..).collect(),
            "PostToolUse" | "PostToolUseFailure" | "PermissionDenied" => {
                // В запросе разрешения Claude не присылает номер вызова —
                // узнаём его по инструменту и тому, что он получил.
                let same = |r: &PermissionRequest| match (&r.tool_use_id, &event.tool_use_id) {
                    (Some(a), Some(b)) => a == b,
                    _ => r.tool == event.tool && r.input == event.input,
                };
                let (done, waiting): (Vec<_>, Vec<_>) =
                    std::mem::take(&mut self.permissions).into_iter().partition(|r| same(r));
                self.permissions = waiting.into();
                done
            }
            _ => Vec::new(),
        };
        // Хуку отвечаем пусто: Claude уже решил сам, пусть хук просто закроется.
        for request in answered {
            request.answer(Decision::AsUsual);
        }
        self.permissions.len() != before
    }

    /// Ответили прямо в диалоге Claude: он был на экране, в сессию нажали,
    /// и он пропал. Снимаем ждущие запросы сразу. `true` — что-то сняли.
    pub fn resolve_answered_prompt(&mut self) -> bool {
        if self.permissions.is_empty() {
            self.prompt = PromptWatch::default();
            return false;
        }
        match self.prompt_visible() {
            Some(true) => self.prompt.seen = true,
            Some(false) if self.prompt.seen && self.prompt.touched => {
                self.prompt = PromptWatch::default();
                for request in self.permissions.drain(..) {
                    request.answer(Decision::AsUsual);
                }
                return true;
            }
            _ => {}
        }
        false
    }

    /// Виден ли диалог Claude. `None` — судить рано: кадр дорисован не до
    /// конца или смотрим историю.
    fn prompt_visible(&self) -> Option<bool> {
        let screen = self.parser.screen();
        if self.parser.callbacks().sync_update || screen.scrollback() > 0 {
            return None;
        }
        let contents = screen.contents();
        Some(PROMPT_MARKS.iter().any(|mark| contents.contains(mark)))
    }

    /// Форма курсора, которую попросил Claude (DECSCUSR), если просил.
    pub fn cursor_style(&self) -> Option<u16> {
        self.parser.callbacks().cursor_style
    }

    pub fn wants_focus_events(&self) -> bool {
        self.parser.callbacks().focus_events
    }

    pub fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.parser.callbacks_mut().bell)
    }

    /// То, что нажал или вставил человек.
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        // Esc или Ctrl-C — Claude, скорее всего, остановили.
        if bytes == b"\x1b" || bytes == b"\x03" {
            self.status.interrupt();
        }
        if !self.permissions.is_empty() {
            if self.prompt_visible() == Some(true) {
                self.prompt.seen = true;
            }
            self.prompt.touched |= self.prompt.seen;
        }
        self.send(bytes)
    }

    /// Служебное: ответы терминала, фокус окна.
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
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

    /// Claude закрыл терминал: дожидаемся процесса и отдаём код выхода.
    pub fn reap(&mut self) -> u32 {
        stop_all(std::slice::from_mut(self));
        self.exit_code.unwrap_or(0)
    }

    /// Сигнал всей группе процессов Claude: ему и тому, что он запустил.
    fn signal(&self, signal: i32) {
        unsafe {
            libc::kill(-self.pid, signal);
            libc::kill(self.pid, signal);
        }
    }

    fn try_reap(&mut self) -> bool {
        if self.exit_code.is_none()
            && let Ok(Some(status)) = self.child.try_wait() {
                self.set_exited(status.exit_code());
            }
        self.exit_code.is_some()
    }

    fn set_exited(&mut self, code: u32) {
        self.exit_code = Some(code);
        LIVE_PIDS.lock().unwrap().retain(|&pid| pid != self.pid);
    }
}

/// Останавливает Claude так же, как закрытие окна терминала: SIGHUP, а кто
/// не вышел за `STOP_GRACE` — SIGKILL. Возвращается, когда все мертвы.
pub fn stop_all(sessions: &mut [Session]) {
    for session in sessions.iter_mut() {
        if !session.try_reap() {
            session.signal(libc::SIGHUP);
        }
    }
    let deadline = Instant::now() + STOP_GRACE;
    while Instant::now() < deadline && !sessions.iter_mut().all(Session::try_reap) {
        thread::sleep(Duration::from_millis(20));
    }
    for session in sessions.iter_mut().filter(|s| s.exit_code.is_none()) {
        session.signal(libc::SIGKILL);
        let code = session.child.wait().map(|s| s.exit_code()).unwrap_or(1);
        session.set_exited(code);
    }
}

/// Для аварийного выхода, когда до сессий уже не добраться.
pub fn hangup_all_live() {
    if let Ok(pids) = LIVE_PIDS.try_lock() {
        for &pid in pids.iter() {
            unsafe {
                libc::kill(-pid, libc::SIGHUP);
                libc::kill(pid, libc::SIGHUP);
            }
        }
    }
}

fn pty_size(rows: u16, cols: u16) -> PtySize {
    PtySize { rows, cols, pixel_width: 0, pixel_height: 0 }
}

/// Вывод программы — в `on_output` из отдельного потока; `None` — терминал закрыт.
fn read_output(mut reader: Box<dyn Read + Send>, on_output: impl Fn(Option<Vec<u8>>) + Send + 'static) {
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
}

/// Терминал, открытый прежней версией vv и принятый после перезагрузки.
struct AdoptedMaster {
    fd: OwnedFd,
}

impl MasterPty for AdoptedMaster {
    fn resize(&self, size: PtySize) -> Result<(), anyhow::Error> {
        let winsize = libc::winsize { ws_row: size.rows, ws_col: size.cols, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: ioctl с правильной структурой на своём fd.
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCSWINSZ, &winsize) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(())
    }

    fn get_size(&self) -> Result<PtySize, anyhow::Error> {
        let mut winsize = libc::winsize { ws_row: 0, ws_col: 0, ws_xpixel: 0, ws_ypixel: 0 };
        // SAFETY: ioctl с правильной структурой на своём fd.
        if unsafe { libc::ioctl(self.fd.as_raw_fd(), libc::TIOCGWINSZ, &mut winsize) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(pty_size(winsize.ws_row, winsize.ws_col))
    }

    fn try_clone_reader(&self) -> Result<Box<dyn Read + Send>, anyhow::Error> {
        Ok(Box::new(File::from(self.fd.try_clone()?)))
    }

    fn take_writer(&self) -> Result<Box<dyn Write + Send>, anyhow::Error> {
        Ok(Box::new(File::from(self.fd.try_clone()?)))
    }

    fn process_group_leader(&self) -> Option<libc::pid_t> {
        // SAFETY: tcgetpgrp на своём fd.
        let pgrp = unsafe { libc::tcgetpgrp(self.fd.as_raw_fd()) };
        (pgrp > 0).then_some(pgrp)
    }

    fn as_raw_fd(&self) -> Option<RawFd> {
        Some(self.fd.as_raw_fd())
    }

    fn tty_name(&self) -> Option<PathBuf> {
        None
    }
}

/// Процесс Claude, принятый после перезагрузки. После `exec` он остаётся
/// нашим дочерним — ждём его обычным `waitpid`.
#[derive(Debug)]
struct AdoptedChild {
    pid: i32,
}

impl AdoptedChild {
    fn wait_with(&mut self, flags: i32) -> std::io::Result<Option<ExitStatus>> {
        let mut status = 0;
        // SAFETY: waitpid на свой дочерний процесс.
        let done = unsafe { libc::waitpid(self.pid, &mut status, flags) };
        match done {
            0 => Ok(None),
            -1 => {
                let err = std::io::Error::last_os_error();
                // Уже подобран — считаем вышедшим.
                if err.raw_os_error() == Some(libc::ECHILD) { Ok(Some(ExitStatus::with_exit_code(0))) } else { Err(err) }
            }
            _ if libc::WIFEXITED(status) => Ok(Some(ExitStatus::with_exit_code(libc::WEXITSTATUS(status) as u32))),
            _ => Ok(Some(ExitStatus::with_exit_code(1))),
        }
    }
}

impl ChildKiller for AdoptedChild {
    fn kill(&mut self) -> std::io::Result<()> {
        // SAFETY: сигнал своему дочернему процессу.
        if unsafe { libc::kill(self.pid, libc::SIGHUP) } == -1 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }

    fn clone_killer(&self) -> Box<dyn ChildKiller + Send + Sync> {
        Box::new(AdoptedChild { pid: self.pid })
    }
}

impl Child for AdoptedChild {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        self.wait_with(libc::WNOHANG)
    }

    fn wait(&mut self) -> std::io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.wait_with(0)? {
                return Ok(status);
            }
        }
    }

    fn process_id(&self) -> Option<u32> {
        Some(self.pid as u32)
    }
}

/// Реакция эмулятора на то, что vt100 сам не обрабатывает.
#[derive(Default)]
struct Term {
    replies: Vec<u8>,
    title: String,
    bell: bool,
    focus_events: bool,
    sync_update: bool,
    cursor_style: Option<u16>,
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
            (Some(b' '), 'q', style) => self.cursor_style = style.filter(|&s| s != 0),
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
    fn remembers_cursor_style() {
        let mut p = parser();
        p.process(b"\x1b[2 q");
        assert_eq!(p.callbacks().cursor_style, Some(2));
        p.process(b"\x1b[0 q");
        assert_eq!(p.callbacks().cursor_style, None);
    }

    #[test]
    fn remembers_window_title() {
        let mut p = parser();
        p.process(b"\x1b]0;my task\x07");
        assert_eq!(p.callbacks().title, "my task");
    }
}
