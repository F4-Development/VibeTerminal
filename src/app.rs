//! Главный цикл: терминал пользователя ↔ сессии Claude.
//!
//! Всё, что печатаешь, уходит в выбранного Claude. `Ctrl-\` открывает меню,
//! остальное — кнопками и мышью.

use std::io::{self, BufWriter, Stdout, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
use crate::saved::{self, SavedSession, SavedWindow};
use crate::session::{self, Launch, Program, Session, SessionId};
use crate::notify::{self, Notifier};
use crate::status::{self, State};
use crate::usage::{self, Limit};
use crate::voice;
use crate::settings::Settings;
use crate::caps::Caps;
use crate::ci::{self, CiState, Job};
use crate::git::{self, GitOp, OpResult, RepoStatus};
use crate::gitui::{self, GitOverlay};
use crate::hooks::{self, Decision, HookEvent, HookServer, PermissionRequest, StatusLine};
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
/// Как часто обновлять ветку и изменения в шапке.
const GIT_TICK: Duration = Duration::from_secs(3);
/// Пока на карточках идут секунды — перерисовывать так часто.
const STATUS_TICK: Duration = Duration::from_secs(1);
/// Fetch при открытии сессии — не чаще этого.
const FETCH_EVERY: Duration = Duration::from_secs(120);
/// После отправки ветки ждём пайплайн на новый коммит не дольше этого…
const CI_EXPECT_FOR: Duration = Duration::from_secs(120);
/// …и проверяем так часто.
const CI_EXPECT_POLL: Duration = Duration::from_secs(5);
/// Лимиты Claude узнаём раз в столько…
const USAGE_EVERY: Duration = Duration::from_secs(300);
/// …после ответа Claude — если прошло хотя бы столько…
const USAGE_AFTER_TURN: Duration = Duration::from_secs(60);
/// …а при открытии окна лимитов — если старше этого.
const USAGE_ON_OPEN: Duration = Duration::from_secs(20);
/// Пока идёт запись, эквалайзер перерисовывается так часто.
const VOICE_FRAME: Duration = Duration::from_millis(40);
/// Модель голоса не нужна столько — выгружаем, она занимает сотни мегабайт.
const VOICE_UNLOAD: Duration = Duration::from_secs(600);

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
    /// Узнали состояние git в папке сессии.
    GitStatus(SessionId, Option<RepoStatus>),
    /// Закончилась команда git.
    GitDone(SessionId, GitOp, OpResult),
    /// Узнали CI ветки.
    CiUpdate(SessionId, String, CiState),
    /// Задачи пайплайна для окна CI.
    CiJobs(SessionId, String, Result<Vec<Job>, String>),
    /// Логи упавших задач собраны — задание для Claude или ошибка.
    CiFixReady(SessionId, Result<String, String>),
    CiRetried(SessionId, Result<(), String>),
    /// Хук запроса закрылся сам — Claude больше не ждёт.
    PermissionGone(SessionId, u64),
    /// Claude сообщил, что сделал: выполнил инструмент, закончил ответ…
    Hook(SessionId, HookEvent),
    /// Кликнули по уведомлению этой сессии.
    OpenSession(SessionId),
    /// Узнали лимиты Claude.
    Usage(Result<Vec<Limit>, String>),
    /// Claude обновил строку состояния: контекст и лимиты.
    StatusLine(SessionId, StatusLine),
    /// Голос распознан (или нет).
    VoiceText(SessionId, Result<String, String>),
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
    /// Меню веток, диалоги git, коммит.
    Git(GitOverlay),
    /// Лимиты Claude, выбран лимит с этим номером.
    Usage(usize),
}

/// Голосовой ввод в сессию.
pub enum VoiceState {
    Idle,
    /// `hint` — что нажать, чтобы закончить: зависит от настроек.
    Recording { session: SessionId, recording: voice::Recording, hint: &'static str },
    Transcribing { session: SessionId, since: Instant },
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
    /// Окно VibeTerminal сейчас в фокусе: открытую сессию ты видишь.
    focused: bool,
    last_git_tick: Instant,
    notifier: Notifier,
    /// Лимиты Claude: последние, что удалось узнать.
    pub usage: Vec<Limit>,
    /// Почему не удалось узнать в последний раз.
    pub usage_error: Option<String>,
    pub usage_at: Option<Instant>,
    pub usage_loading: bool,
    /// Какие лимиты показывать внизу (из настроек).
    pub usage_shown: Vec<String>,
    pub voice: VoiceState,
    /// Когда последний раз распознавали — чтобы выгрузить модель без дела.
    voice_used: Option<Instant>,
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
    let usage_shown = Settings::load(&home).usage_shown;
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
        notifier: Notifier::new(launch.socket.clone()),
        launch,
        hover: None,
        pointer: "",
        cursor_shape: 0,
        window_title: String::new(),
        focused: true,
        last_git_tick: Instant::now(),
        usage: Vec::new(),
        usage_error: None,
        usage_at: None,
        usage_loading: false,
        usage_shown,
        voice: VoiceState::Idle,
        voice_used: None,
        stopping: Vec::new(),
        quit: false,
    };

    // Из Dock приложение стартует в домашней папке: возвращаем сессии,
    // что были открыты до перезапуска, а нет таких — выбор проекта.
    // Из терминала в папке проекта — сразу Claude там.
    let from_dock = cwd == app.home || cwd == Path::new("/");
    let restored = from_dock && saved::claim(&app.home).is_some_and(|window| app.restore(window));
    let started = if restored {
        Ok(())
    } else if from_dock {
        app.overlay = Overlay::Picker(Picker::new(&app.home));
        Ok(())
    } else {
        app.open(&cwd)
    };
    let result = started.and_then(|()| {
        spawn_input_thread(tx);
        app.run_loop(&mut terminal, &rx)
    });
    // Запомнить, что было открыто, — после перезапуска сессии вернутся.
    app.save_state();
    // Что бы ни случилось, Claude не должны пережить окно vv.
    app.stop_everything();
    // Модель голоса — до выхода, иначе Metal уронит процесс на выходе.
    voice::unload();
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
            // После Esc Claude замолчал — сессия больше не «работает».
            for session in &mut self.sessions {
                dirty |= session.status.settle();
            }
            // Git и CI открытой сессии — по часам, даже когда Claude без
            // остановки что-то выводит и таймаута ожидания не бывает.
            if self.last_git_tick.elapsed() >= GIT_TICK {
                self.last_git_tick = Instant::now();
                self.refresh_git(self.selected);
                self.refresh_ci_if_due(self.selected);
            }
            self.refresh_ci_view();
            self.refresh_usage(USAGE_EVERY);
            let banner_due = self.notifier.flush(terminal.backend_mut())?;
            dirty |= !matches!(self.voice, VoiceState::Idle);
            if self.voice_used.is_some_and(|at| at.elapsed() >= VOICE_UNLOAD) && matches!(self.voice, VoiceState::Idle) {
                self.voice_used = None;
                thread::spawn(voice::unload);
            }
            let hold = self.current().and_then(Session::frame_hold);
            if dirty && hold.is_none() {
                self.draw(terminal)?;
                dirty = false;
            }
            // Если висит сообщение, проснуться, чтобы его убрать.
            // Просыпаемся дорисовать кадр, убрать сообщение или обновить git.
            let tick = if self.sessions.iter().any(Session::ticking) { STATUS_TICK } else { GIT_TICK };
            let wake = match (dirty, self.flash()) {
                (true, _) => hold.unwrap_or(FRAME),
                (false, Some(_)) => FLASH_FOR.min(tick),
                (false, None) => tick,
            };
            let wake = banner_due.map_or(wake, |due| due.min(wake));
            // Эквалайзер и «Распознаю…» живые.
            let wake = if matches!(self.voice, VoiceState::Idle) { wake } else { wake.min(VOICE_FRAME) };
            let event = match rx.recv_timeout(wake) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => {
                    dirty = true;
                    continue;
                }
                Err(RecvTimeoutError::Disconnected) => break,
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
                let answered = session.resolve_answered_prompt();
                if session.take_bell() {
                    let out = terminal.backend_mut();
                    out.write_all(b"\x07")?;
                    out.flush()?;
                }
                Ok(index == self.selected || answered || session.title() != title_before)
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
                let asks = if request.is_question() { "ждёт ответа" } else { "просит разрешение" };
                session.permissions.push_back(request);
                if index != self.selected {
                    let note = format!("«{}» {asks}", session.name);
                    self.set_flash(note);
                }
                self.notify(index, notify::Kind::Waiting);
                Ok(true)
            }
            Event::PermissionGone(id, request) => {
                let Some(index) = self.index_of(id) else { return Ok(false) };
                self.sessions[index].permissions.retain(|r| r.id != request);
                Ok(true)
            }
            Event::Hook(id, event) => {
                let Some(index) = self.index_of(id) else { return Ok(false) };
                // Claude что-то поменял — пересчитать изменения в шапке.
                if event.name != "PreToolUse" {
                    self.refresh_git(index);
                }
                // Claude ответил — лимиты сдвинулись.
                if event.name == "Stop" {
                    self.refresh_usage(USAGE_AFTER_TURN);
                }
                if self.sessions[index].note_dialog(&event.claude_id, &event.transcript) {
                    self.save_state();
                }
                let session = &mut self.sessions[index];
                let before = session.status.state;
                let worked = session.status.since.elapsed();
                let resolved = session.resolve_permissions(&event);
                let changed = session.status.on_event(&event, &session.cwd);
                let after = session.status.state;
                if after != before {
                    let long_enough = worked.as_secs() >= Settings::load(&self.home).notify_done_after;
                    match after {
                        State::Done if before == State::Working && long_enough => self.notify(index, notify::Kind::Done),
                        State::Failed => self.notify(index, notify::Kind::Failed),
                        _ => {}
                    }
                }
                self.mark_seen();
                Ok(resolved || changed)
            }
            Event::StatusLine(id, line) => {
                let Some(index) = self.index_of(id) else { return Ok(false) };
                if self.sessions[index].note_dialog(&line.claude_id, &line.transcript) {
                    self.save_state();
                }
                let (five_hour, seven_day) = (line.five_hour, line.seven_day);
                let changed = self.sessions[index].status_line.as_ref() != Some(&line);
                self.sessions[index].status_line = Some(line);
                let mut limits = false;
                if let Some(percent) = five_hour {
                    limits |= usage::set_live(&mut self.usage, "session", percent);
                }
                if let Some(percent) = seven_day {
                    limits |= usage::set_live(&mut self.usage, "week", percent);
                }
                Ok(limits || (changed && index == self.selected))
            }
            Event::VoiceText(id, result) => {
                if !matches!(self.voice, VoiceState::Transcribing { session, .. } if session == id) {
                    return Ok(false);
                }
                self.voice = VoiceState::Idle;
                self.voice_used = Some(Instant::now());
                let send = Settings::load(&self.home).voice_after != "insert";
                match (result, self.index_of(id)) {
                    (Ok(text), Some(index)) => {
                        let session = &mut self.sessions[index];
                        session.scroll_to_bottom();
                        session.paste(&text)?;
                        if send {
                            session.write(b"\r")?;
                        }
                    }
                    (Ok(_), None) => {}
                    (Err(text), _) => self.set_flash(format!("Голос: {text}")),
                }
                Ok(true)
            }
            Event::Usage(result) => {
                self.usage_loading = false;
                self.usage_at = Some(Instant::now());
                match result {
                    Ok(limits) => {
                        self.usage = limits;
                        self.usage_error = None;
                    }
                    // Прошлые цифры не выкидываем — показываем, почему не обновились.
                    Err(text) => self.usage_error = Some(text),
                }
                Ok(true)
            }
            Event::OpenSession(id) => {
                let Some(index) = self.index_of(id) else { return Ok(false) };
                self.overlay = Overlay::None;
                self.select(index)?;
                Ok(true)
            }
            Event::GitStatus(id, status) => {
                let Some(index) = self.index_of(id) else { return Ok(false) };
                let session = &mut self.sessions[index];
                session.git_refreshing = false;
                let changed = session.git != status;
                // Другая ветка — её CI узнаём сразу.
                let branch = |s: &Option<RepoStatus>| s.as_ref().and_then(|g| g.branch.clone());
                if branch(&session.git) != branch(&status) {
                    session.ci = None;
                    session.ci_due = None;
                    session.ci_expect = None;
                } else if let (Some(old), Some(new)) = (&session.git, &status)
                    && new.upstream_head.is_some()
                    && old.upstream_head != new.upstream_head
                {
                    // Ветка на сервере сдвинулась — неважно, кто отправил:
                    // ты, Claude или другой терминал. CI сейчас запустится.
                    let sha = new.upstream_head.clone().unwrap_or_default();
                    session.ci_expect = Some((sha, Instant::now()));
                    session.ci_due = None;
                }
                session.git = status;
                self.refresh_ci_if_due(index);
                Ok(changed && index == self.selected)
            }
            Event::GitDone(id, op, result) => {
                self.on_git_done(id, op, result);
                Ok(true)
            }
            Event::CiUpdate(id, branch, state) => {
                let Some(index) = self.index_of(id) else { return Ok(false) };
                let session = &mut self.sessions[index];
                session.ci_refreshing = false;
                // Пока узнавали, ветку могли сменить — такой ответ не нужен.
                if session.git.as_ref().and_then(|g| g.branch.as_deref()) != Some(branch.as_str()) {
                    return Ok(false);
                }
                // Ждали пайплайн на отправленный коммит: появился, не дождались
                // за 2 минуты или CI вообще недоступен — больше не ждём.
                if let Some((sha, since)) = &session.ci_expect {
                    let started = matches!(&state, CiState::Pipeline(_, p) if p.sha == *sha);
                    let reachable = matches!(state, CiState::Pipeline(..) | CiState::NoPipeline(_));
                    if started || !reachable || since.elapsed() >= CI_EXPECT_FOR {
                        session.ci_expect = None;
                    }
                }
                let wait = match &state {
                    _ if session.ci_expect.is_some() => CI_EXPECT_POLL,
                    CiState::Pipeline(_, p) if p.status.active() => Duration::from_secs(15),
                    CiState::Pipeline(..) | CiState::NoPipeline(_) => Duration::from_secs(120),
                    CiState::Unsupported => Duration::from_secs(3600),
                    _ => Duration::from_secs(300),
                };
                session.ci_due = Some(Instant::now() + wait);
                let changed = session.ci.as_ref() != Some(&state);
                // Окно CI открыто — в нём тот же статус и, если был push, новый пайплайн.
                if index == self.selected
                    && let CiState::Pipeline(_, pipeline) = &state
                    && let Overlay::Git(GitOverlay::Ci(view)) = &mut self.overlay
                {
                    view.update_pipeline(pipeline);
                }
                session.ci = Some(state);
                Ok(changed && index == self.selected)
            }
            Event::CiJobs(id, pipeline, jobs) => {
                if let Overlay::Git(GitOverlay::Ci(view)) = &mut self.overlay
                    && self.sessions.get(self.selected).is_some_and(|s| s.id == id)
                    && view.pipeline.id == pipeline
                {
                    // Задачи закончились — узнать итог пайплайна, не дожидаясь очереди.
                    if view.jobs_loaded(jobs)
                        && let Some(session) = self.sessions.get_mut(self.selected)
                    {
                        session.ci_due = None;
                    }
                    return Ok(true);
                }
                Ok(false)
            }
            Event::CiFixReady(id, result) => {
                match result {
                    Ok(prompt) => {
                        if let Some(index) = self.index_of(id) {
                            let session = &mut self.sessions[index];
                            let sent = session.paste(&prompt).and_then(|()| session.write(b"\r"));
                            if sent.is_ok() {
                                self.set_flash("Логи CI отправлены Claude");
                            }
                        }
                    }
                    Err(text) => self.set_flash(format!("CI: {text}")),
                }
                Ok(true)
            }
            Event::CiRetried(id, result) => {
                match result {
                    Ok(()) => {
                        self.set_flash("Упавшие задачи перезапущены");
                        if let Some(index) = self.index_of(id) {
                            self.sessions[index].ci_due = Some(Instant::now() + Duration::from_secs(5));
                        }
                    }
                    Err(text) => self.set_flash(format!("CI: {text}")),
                }
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
                    Overlay::Git(git) => gitui::on_paste(git, &text),
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
                self.focused = gained;
                if gained {
                    self.notifier.clear();
                }
                self.send_focus(self.selected, gained)?;
                Ok(self.mark_seen())
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
        // ⌘⇧Space VibeTerminal отдаёт как F13 (нажали) и F14 (отпустили).
        let hold = Settings::load(&self.home).voice_mode == "hold";
        // Идёт запись: Enter — распознать, Esc — отменить, остальное не в Claude.
        if matches!(self.voice, VoiceState::Recording { .. }) {
            match key.code {
                KeyCode::Enter => self.finish_voice(),
                KeyCode::Esc => self.cancel_voice(),
                // «Нажать и говорить»: второе нажатие — готово.
                KeyCode::F(13) if !hold => self.finish_voice(),
                // «Удерживать клавишу»: отпустил — готово.
                KeyCode::F(14) if hold => self.finish_voice(),
                _ => {}
            }
            return Ok(true);
        }
        if matches!(self.voice, VoiceState::Transcribing { .. }) && key.code == KeyCode::Esc {
            self.cancel_voice();
            return Ok(true);
        }
        match key.code {
            KeyCode::F(13) if matches!(self.overlay, Overlay::None) => {
                self.start_voice();
                return Ok(true);
            }
            KeyCode::F(14) => return Ok(false),
            _ => {}
        }
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
        let usage_rows = ui::usage_rows(self).len().max(1);
        match &mut self.overlay {
            Overlay::None => {}
            Overlay::Help => self.overlay = Overlay::None,
            Overlay::Usage(cursor) => {
                let len = usage_rows;
                match keys::latin(key.code) {
                    KeyCode::Esc => self.overlay = Overlay::None,
                    _ if keys::is_prefix(&key) => self.overlay = Overlay::None,
                    KeyCode::Up => *cursor = (*cursor + len - 1) % len,
                    KeyCode::Down | KeyCode::Tab => *cursor = (*cursor + 1) % len,
                    KeyCode::Char(' ') | KeyCode::Enter => {
                        let index = *cursor;
                        self.toggle_usage(index);
                    }
                    KeyCode::Char('r' | 'R') => self.refresh_usage(Duration::ZERO),
                    _ => return Ok(false),
                }
            }
            Overlay::Git(git) => {
                let command = gitui::on_key(git, key);
                return Ok(self.apply_git(command));
            }
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
            Action::Usage => {
                self.overlay = Overlay::Usage(0);
                self.refresh_usage(USAGE_ON_OPEN);
            }
            Action::Voice => match self.voice {
                VoiceState::Recording { .. } => self.finish_voice(),
                _ => self.start_voice(),
            },
            Action::NextWaiting => {
                if let Some(&index) = status::queue(&self.sessions, self.selected).first() {
                    self.select(index)?;
                }
            }
            Action::Git => self.open_git_menu(),
            Action::Settings => {
                if let Err(err) = Settings::open(&self.home) {
                    self.set_flash(format!("настройки не открылись: {err}"));
                }
            }
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
                    self.save_state();
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
                (Overlay::Git(git), Some(Target::Git(t))) => dirty |= gitui::hover(git, t),
                (Overlay::Usage(cursor), Some(Target::UsageRow(i))) if *cursor != i => {
                    *cursor = i;
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
                    Overlay::Git(git) => gitui::closes_on_outside_click(git, full, column, row),
                    Overlay::Usage(_) => !ui::usage_contains(self, column, row),
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
            Target::Branch => self.open_git_menu(),
            Target::Ci => self.open_ci(),
            Target::Usage => self.perform(Action::Usage)?,
            Target::Mic => self.perform(Action::Voice)?,
            Target::UsageRow(index) => {
                self.overlay = Overlay::Usage(index);
                self.toggle_usage(index);
            }
            Target::UsageRefresh => self.refresh_usage(Duration::ZERO),
            Target::Git(t) => {
                if let Overlay::Git(git) = &mut self.overlay {
                    let command = gitui::click(git, t);
                    self.apply_git(command);
                }
            }
            Target::NewSession => self.perform(Action::New)?,
            Target::Card(index) => self.select(index)?,
            Target::NextWaiting => self.perform(Action::NextWaiting)?,
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

    /// Новая сессия Claude в папке.
    fn open(&mut self, dir: &Path) -> Result<()> {
        let folder = dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned());
        let args = Settings::load(&self.home).claude_args();
        self.spawn_session(folder, dir, Program::Claude(args))
    }

    /// Вернуть сессии, открытые до перезапуска: диалоги Claude продолжаются
    /// с той же историей. `false` — ни одну открыть не вышло.
    fn restore(&mut self, window: SavedWindow) -> bool {
        let args = Settings::load(&self.home).claude_args();
        for saved in window.sessions.iter().filter(|s| s.cwd.is_dir()) {
            let mut args = args.clone();
            if let Some(id) = saved.resumable() {
                args.extend(["--resume".to_string(), id.to_string()]);
            }
            if self.spawn_session(saved.name.clone(), &saved.cwd, Program::Claude(args)).is_ok()
                && let Some(session) = self.sessions.last_mut()
            {
                session.claude_id.clone_from(&saved.claude_id);
                session.transcript.clone_from(&saved.transcript);
            }
        }
        if self.sessions.is_empty() {
            return false;
        }
        let selected = window.selected.min(self.sessions.len() - 1);
        let _ = self.select(selected);
        let note = match self.sessions.len() {
            1 => "Сессия вернулась после перезапуска".to_string(),
            n => format!("Вернул сессии после перезапуска: {n}"),
        };
        self.set_flash(note);
        self.save_state();
        true
    }

    /// Запомнить открытые сессии этого окна. Сессии-команды (вход, установка)
    /// не запоминаем.
    fn save_state(&self) {
        let claude: Vec<(usize, &Session)> = self.sessions.iter().enumerate().filter(|(_, s)| !s.is_command).collect();
        let selected = claude.iter().position(|(i, _)| *i == self.selected).unwrap_or(0);
        let window = SavedWindow {
            sessions: claude
                .iter()
                .map(|(_, s)| SavedSession {
                    name: s.name.clone(),
                    cwd: s.cwd.clone(),
                    claude_id: s.claude_id.clone(),
                    transcript: s.transcript.clone(),
                })
                .collect(),
            selected,
        };
        let _ = saved::save(&self.home, &window);
    }

    fn spawn_session(&mut self, name: String, dir: &Path, program: Program) -> Result<()> {
        let name = self.unique_name(&name);
        let id = self.next_id;
        self.next_id += 1;
        let tx = self.tx.clone();
        let area = self.areas.agent;
        let (rows, cols) = (area.height.max(1), area.width.max(1));
        let session = Session::spawn(id, name, dir, (rows, cols), &self.launch, &program, move |chunk| {
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
        let note = match (session.is_command, code) {
            (true, 0) => format!("«{}»: готово", session.name),
            (true, code) => format!("«{}»: завершилось с кодом {code}", session.name),
            (false, 0) => format!("«{}»: Claude закрылся", session.name),
            (false, code) => format!("«{}»: Claude завершился с кодом {code}", session.name),
        };
        // Вход или установка закончились — CI всех сессий узнаём заново.
        if session.is_command {
            for other in &mut self.sessions {
                other.ci_due = None;
            }
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
        // Закрытую сессию после перезапуска не возвращаем.
        self.save_state();
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
        self.save_state();
        self.mark_seen();
        self.refresh_git(index);
        self.fetch_if_stale(index);
        self.send_focus(index, true)
    }

    /// Сессия ждёт тебя, закончила или упала — позвать, если ты смотришь
    /// не на неё. Окно в фокусе, но открыта другая сессия — звук и строка
    /// внизу; окно не в фокусе — ещё и баннер.
    fn notify(&mut self, index: usize, kind: notify::Kind) {
        let settings = Settings::load(&self.home);
        let Some(session) = self.sessions.get(index) else { return };
        let looking = self.focused && index == self.selected;
        let (title, body) = match kind {
            notify::Kind::Waiting => {
                let Some(request) = session.pending_permission() else { return };
                let (what, detail) = hooks::describe(&request.tool, &request.input, &session.cwd);
                if request.is_question() {
                    (format!("{} {what}", session.name), detail)
                } else {
                    (format!("{} просит разрешение", session.name), format!("{what}: {detail}"))
                }
            }
            notify::Kind::Done => {
                let body = if session.status.detail.is_empty() { "Claude закончил работу" } else { &session.status.detail };
                (format!("{} закончил", session.name), body.to_string())
            }
            notify::Kind::Failed => (format!("{}: ошибка", session.name), session.status.detail.clone()),
        };
        let id = session.id;
        if index != self.selected && kind != notify::Kind::Waiting {
            self.set_flash(format!("«{title}»"));
        }
        if !looking && kind.enabled(&settings) {
            self.notifier.send(&settings, id, title, &body, self.focused);
        }
    }

    /// Открытую сессию ты видишь — её «готово» прочитано.
    fn mark_seen(&mut self) -> bool {
        let focused = self.focused;
        self.current_mut().is_some_and(|session| focused && session.status.seen())
    }

    /// Узнать ветку и изменения в фоне; ответ придёт событием.
    fn refresh_git(&mut self, index: usize) {
        let Some(session) = self.sessions.get_mut(index) else { return };
        if session.git_refreshing {
            return;
        }
        session.git_refreshing = true;
        let (id, cwd, tx) = (session.id, session.cwd.clone(), self.tx.clone());
        thread::spawn(move || {
            let _ = tx.send(Event::GitStatus(id, git::status(&cwd)));
        });
    }

    /// Fetch при открытии сессии и переключении на неё, но не чаще FETCH_EVERY.
    fn fetch_if_stale(&mut self, index: usize) {
        let Some(session) = self.sessions.get(index) else { return };
        if session.git_busy.is_none() && session.last_fetch.is_none_or(|at| at.elapsed() > FETCH_EVERY) {
            self.run_git(index, GitOp::Fetch { quiet: true });
        }
    }

    /// Команда git в фоне; пока идёт, в шапке «⟳ отправляю…».
    fn run_git(&mut self, index: usize, op: GitOp) {
        let Some(session) = self.sessions.get_mut(index) else { return };
        if session.git_busy.is_some() {
            if !matches!(op, GitOp::Fetch { quiet: true }) {
                self.set_flash("git ещё занят предыдущей командой");
            }
            return;
        }
        session.git_busy = Some(op.busy_label());
        if matches!(op, GitOp::Fetch { .. }) {
            session.last_fetch = Some(Instant::now());
        }
        let (id, cwd, tx) = (session.id, session.cwd.clone(), self.tx.clone());
        thread::spawn(move || {
            let result = git::execute(&cwd, &op);
            let _ = tx.send(Event::GitDone(id, op, result));
        });
    }

    fn on_git_done(&mut self, id: SessionId, op: GitOp, result: OpResult) {
        let Some(index) = self.index_of(id) else { return };
        self.sessions[index].git_busy = None;
        self.refresh_git(index);
        let quiet = matches!(op, GitOp::Fetch { quiet: true });
        // После отправки CI запускается — посмотрим на него скоро.
        if result.is_ok() && matches!(op, GitOp::Push | GitOp::PushForce | GitOp::Commit { push: true, .. }) {
            self.sessions[index].ci_due = Some(Instant::now() + Duration::from_secs(8));
        }
        match result {
            Ok(message) if !quiet => self.set_flash(message),
            Ok(_) => {}
            Err(error) => {
                // Не перебиваем то, что открыто сейчас, и не показываем чужие ошибки поверх другой сессии.
                let dialog = gitui::failure_dialog(&op, &error);
                match dialog {
                    Some(dialog) if index == self.selected && matches!(self.overlay, Overlay::None) => {
                        self.overlay = Overlay::Git(GitOverlay::Choice(dialog));
                    }
                    Some(_) => self.set_flash(format!("«{}»: git не справился", self.sessions[index].name)),
                    None => {}
                }
            }
        }
    }

    /// Проверить CI ветки в фоне, если пора.
    fn refresh_ci_if_due(&mut self, index: usize) {
        let Some(session) = self.sessions.get_mut(index) else { return };
        let Some(branch) = session.git.as_ref().and_then(|g| g.branch.clone()) else { return };
        if session.ci_refreshing || session.is_command || session.ci_due.is_some_and(|due| Instant::now() < due) {
            return;
        }
        session.ci_refreshing = true;
        let (id, cwd, tx) = (session.id, session.cwd.clone(), self.tx.clone());
        thread::spawn(move || {
            let state = ci::state(&cwd, &branch);
            let _ = tx.send(Event::CiUpdate(id, branch, state));
        });
    }

    /// Клик по значку CI в шапке.
    fn open_ci(&mut self) {
        let Some(session) = self.current() else { return };
        let Some(state) = session.ci.clone() else { return };
        let Some(overlay) = gitui::open_ci(&state) else { return };
        self.overlay = Overlay::Git(overlay);
        self.refresh_ci_view();
    }

    /// Начать запись в открытую сессию. Модели нет — открыть настройки голоса.
    fn start_voice(&mut self) {
        let Some(session) = self.current().filter(|s| !s.is_command) else { return };
        if !matches!(self.voice, VoiceState::Idle) {
            return;
        }
        let id = session.id;
        let settings = Settings::load(&self.home);
        let Some(model) = voice::model_path(&self.home, &settings.voice_model) else {
            self.open_voice_settings();
            return;
        };
        // Модель грузится, пока ты говоришь.
        thread::spawn(move || voice::preload(&model));
        let hint = match (settings.voice_mode.as_str(), settings.voice_after.as_str()) {
            ("hold", _) => "Отпусти ⌘⇧Space — готово · Esc — отмена ",
            (_, "insert") => "Enter — вставить в поле · Esc — отмена ",
            _ => "Enter — отправить · Esc — отмена ",
        };
        match voice::Recording::start(&settings.voice_device) {
            Ok(recording) => self.voice = VoiceState::Recording { session: id, recording, hint },
            Err(text) => self.set_flash(format!("Голос: {text}")),
        }
    }

    /// Enter: остановить запись и распознать в фоне.
    fn finish_voice(&mut self) {
        let VoiceState::Recording { session, recording, .. } = std::mem::replace(&mut self.voice, VoiceState::Idle) else {
            return;
        };
        let settings = Settings::load(&self.home);
        let Some(model) = voice::model_path(&self.home, &settings.voice_model) else {
            recording.cancel();
            self.open_voice_settings();
            return;
        };
        self.voice = VoiceState::Transcribing { session, since: Instant::now() };
        let tx = self.tx.clone();
        thread::spawn(move || {
            let result = recording
                .finish()
                .and_then(|captured| voice::transcribe(captured, &model, &settings.voice_language, &settings.voice_words));
            let _ = tx.send(Event::VoiceText(session, result));
        });
    }

    fn cancel_voice(&mut self) {
        if let VoiceState::Recording { recording, .. } = std::mem::replace(&mut self.voice, VoiceState::Idle) {
            recording.cancel();
        }
    }

    /// Модель не выбрана: вкладка «Голос» в настройках VibeTerminal.
    fn open_voice_settings(&mut self) {
        let opened = Command::new("open")
            .arg("vibeterminal://settings/voice")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
        self.set_flash(if opened {
            "Выбери модель голоса в настройках — вкладка «Голос»"
        } else {
            "Выбери модель голоса: voice_model в vv.json"
        });
    }

    /// Узнать лимиты Claude в фоне, если последние старше `older_than`.
    fn refresh_usage(&mut self, older_than: Duration) {
        if self.usage_loading || self.usage_at.is_some_and(|at| at.elapsed() < older_than) {
            return;
        }
        self.usage_loading = true;
        let tx = self.tx.clone();
        thread::spawn(move || {
            let _ = tx.send(Event::Usage(usage::fetch()));
        });
    }

    /// Галочка у лимита: показывать его внизу или нет. Запоминается в настройках.
    fn toggle_usage(&mut self, index: usize) {
        let Some(key) = ui::usage_rows(self).into_iter().nth(index).map(|row| row.key) else { return };
        match self.usage_shown.iter().position(|shown| *shown == key) {
            Some(position) => {
                self.usage_shown.remove(position);
            }
            None => self.usage_shown.push(key),
        }
        let mut settings = Settings::load(&self.home);
        settings.usage_shown = self.usage_shown.clone();
        if let Err(err) = settings.save(&self.home) {
            self.set_flash(format!("не сохранилось: {err}"));
        }
    }

    /// Окно CI открыто: задачи загружаются сразу и обновляются сами, пока
    /// пайплайн идёт.
    fn refresh_ci_view(&mut self) {
        let Overlay::Git(GitOverlay::Ci(view)) = &mut self.overlay else { return };
        let Some(session) = self.sessions.get(self.selected) else { return };
        if !view.wants_jobs() {
            return;
        }
        view.loading = true;
        let (id, cwd, tx) = (session.id, session.cwd.clone(), self.tx.clone());
        let (provider, pipeline) = (view.provider.clone(), view.pipeline.clone());
        thread::spawn(move || {
            let jobs = ci::jobs(&cwd, &provider, &pipeline);
            let _ = tx.send(Event::CiJobs(id, pipeline.id, jobs));
        });
    }

    fn open_git_menu(&mut self) {
        let Some(session) = self.current() else { return };
        let Some(status) = session.git.clone() else {
            self.set_flash("В этой папке нет git-репозитория");
            return;
        };
        let anchor = ui::branch_rect(self).unwrap_or(self.areas.agent_frame);
        self.overlay = Overlay::Git(gitui::open_menu(session.cwd.clone(), status, anchor));
    }

    /// Ответ окон git: закрыть, запустить команду или поручить Claude.
    fn apply_git(&mut self, command: gitui::Command) -> bool {
        match command {
            gitui::Command::None => false,
            gitui::Command::Redraw => true,
            gitui::Command::Close => {
                self.overlay = Overlay::None;
                true
            }
            gitui::Command::Run(op) => {
                self.overlay = Overlay::None;
                self.run_git(self.selected, op);
                true
            }
            gitui::Command::OpenUrl(url) => {
                let _ = std::process::Command::new("open").arg(url).spawn();
                true
            }
            gitui::Command::CiFix => {
                let Overlay::Git(GitOverlay::Ci(view)) = &self.overlay else { return false };
                let Some(Ok(jobs)) = &view.jobs else {
                    self.set_flash("Подожди — задачи CI ещё загружаются");
                    return true;
                };
                let (provider, pipeline, jobs) = (view.provider.clone(), view.pipeline.clone(), jobs.clone());
                self.overlay = Overlay::None;
                let Some(session) = self.current() else { return true };
                let (id, cwd, tx) = (session.id, session.cwd.clone(), self.tx.clone());
                self.set_flash("Скачиваю логи CI…");
                thread::spawn(move || {
                    let result = ci::failed_log(&cwd, &provider, &pipeline, &jobs).map(|path| ci::fix_prompt(&pipeline, &path));
                    let _ = tx.send(Event::CiFixReady(id, result));
                });
                true
            }
            gitui::Command::CiRetry => {
                let Overlay::Git(GitOverlay::Ci(view)) = &self.overlay else { return false };
                let (provider, pipeline) = (view.provider.clone(), view.pipeline.clone());
                self.overlay = Overlay::None;
                let Some(session) = self.current() else { return true };
                let (id, cwd, tx) = (session.id, session.cwd.clone(), self.tx.clone());
                thread::spawn(move || {
                    let _ = tx.send(Event::CiRetried(id, ci::retry(&cwd, &provider, &pipeline)));
                });
                true
            }
            gitui::Command::Terminal { name, command } => {
                self.overlay = Overlay::None;
                let dir = self.current().map_or(self.home.clone(), |s| s.cwd.clone());
                if let Err(err) = self.spawn_session(name, &dir, Program::Command(command)) {
                    self.set_flash(format!("не запустилось: {err:#}"));
                }
                true
            }
            gitui::Command::AskClaude(text) => {
                self.overlay = Overlay::None;
                if let Some(session) = self.current_mut() {
                    let sent = session.paste(&text).and_then(|()| session.write(b"\r"));
                    if sent.is_ok() {
                        self.set_flash("Задача отправлена Claude");
                    }
                }
                true
            }
        }
    }

    fn send_focus(&mut self, index: usize, gained: bool) -> Result<()> {
        if let Some(session) = self.sessions.get_mut(index)
            && session.wants_focus_events()
        {
            session.send(if gained { b"\x1b[I" } else { b"\x1b[O" })?;
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
