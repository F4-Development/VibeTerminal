//! Главный цикл: терминал пользователя ↔ сессии Claude.
//!
//! Всё, что печатаешь, уходит в выбранного Claude. `Ctrl-\` открывает меню,
//! остальное — кнопками и мышью.

use std::collections::BTreeMap;
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
use crate::picker::{self, Picker};
use crate::reload;
use crate::saved::{self, SavedSession, SavedWindow};
use crate::session::{self, Launch, Program, Session, SessionId};
use crate::sessionmenu::{About, Act, SessionMenu};
use crate::notify::{self, Notifier};
use crate::status::{self, State};
use crate::usage::{self, Limit};
use crate::voice;
use crate::worktree;
use crate::settings::Settings;
use crate::caps::Caps;
use crate::ci::{self, CiState, Job};
use crate::git::{self, GitOp, OpResult, RepoStatus};
use crate::gitui::{self, GitOverlay};
use crate::hooks::{self, Decision, HookEvent, HookServer, PermissionRequest, StatusLine};
use crate::hotkeys::{self, Hotkey};
use crate::ui::{self, Areas, Target, contains};
use crate::keys::{self, Edit};
use crate::{mouse, view};

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
    /// Проверили новую версию vv: запускается или нет.
    ReloadChecked(PathBuf, bool),
    /// Всплывающее окно VibeTerminal спрашивает, что показать по запросу.
    AskQuery(SessionId, u64, Sender<String>),
    /// …и присылает ответ; `true` в канал — ответ принят.
    AskAnswer(SessionId, u64, serde_json::Value, Sender<bool>),
    /// Копия проекта на этой ветке готова (или нет) — открыть в ней сессию.
    CopyReady(String, Result<worktree::Prepared, String>),
    /// Копию закрытой сессии удалили (или нет).
    CopyRemoved(String, Result<(), String>),
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
    /// Меню сессии по правому клику в списке.
    SessionMenu(SessionMenu),
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
    /// Горячие клавиши из настроек — для подсказок в меню; обновляются при
    /// каждом нажатии.
    pub keys: BTreeMap<String, String>,
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
    /// Следим за файлом vv — обновился, перезапускаемся с теми же сессиями.
    reload: Option<reload::Watch>,
    /// Новая версия проверена и ждёт, когда закроют окна поверх.
    reload_ready: Option<PathBuf>,
    quit: bool,
}

pub fn run() -> Result<()> {
    // Перезапуск после обновления vv: сессии передала прежняя версия.
    let handoff = reload::take();
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
        hyperlinks: caps.hyperlinks,
        socket: hooks.path.clone(),
        vv_exe: std::env::current_exe().context("не знаю, где лежит vv")?,
    };

    setup_terminal()?;
    install_panic_hook();
    let mut terminal = Terminal::new(CrosstermBackend::new(BufWriter::with_capacity(FRAME_BUFFER, io::stdout())))?;
    let size = terminal.size()?;
    let settings = Settings::load(&home);
    let (usage_shown, keys) = (settings.usage_shown, settings.keys);
    let mut app = App {
        keys,
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
        reload: reload::Watch::new(),
        reload_ready: None,
        stopping: Vec::new(),
        quit: false,
    };

    // Из Dock приложение стартует в домашней папке: возвращаем сессии,
    // что были открыты до перезапуска, а нет таких — выбор проекта.
    // Из терминала в папке проекта — сразу Claude там.
    let from_dock = cwd == app.home || cwd == Path::new("/");
    let adopted = handoff.is_some_and(|handoff| app.adopt(handoff));
    // Сохранение, где нечего вернуть (папки уже нет), не мешает следующему.
    let home = app.home.clone();
    let restored = adopted || (from_dock && std::iter::from_fn(|| saved::claim(&home)).any(|window| app.restore(window)));
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
            // Файл vv обновился — проверить новую версию в фоне.
            if let Some(path) = self.reload.as_mut().and_then(reload::Watch::poll) {
                let tx = self.tx.clone();
                thread::spawn(move || {
                    let works = reload::works(&path);
                    let _ = tx.send(Event::ReloadChecked(path, works));
                });
            }
            // Проверена — перезапуститься, когда поверх ничего не открыто.
            if self.reload_ready.is_some()
                && matches!(self.overlay, Overlay::None)
                && matches!(self.voice, VoiceState::Idle)
                && let Some(path) = self.reload_ready.take()
            {
                self.hot_reload(&path, terminal)?;
                dirty = true;
            }
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
        let frame = terminal.draw(|frame| ui::draw(frame, this))?;
        let links = match self.current() {
            Some(session) if self.caps.hyperlinks => {
                let tag = format!("vv{}-", session.id);
                view::links(session.screen(), self.areas.agent, frame.buffer, self.caps.truecolor, &tag)
            }
            _ => Vec::new(),
        };
        let out = terminal.backend_mut();
        out.write_all(&links)?;
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
            Event::AskQuery(id, request, reply) => {
                let found = self.index_of(id).and_then(|index| {
                    let session = &self.sessions[index];
                    let pending = session.permissions.iter().find(|r| r.id == request)?;
                    Some(crate::ask::describe(pending, &session.name, &session.cwd, &self.home))
                });
                // Уже ответили (в терминале или тут) — окну пора закрыться.
                let text = found.map_or_else(|| serde_json::json!({ "gone": true }).to_string(), |v| v.to_string());
                let _ = reply.send(text);
                Ok(false)
            }
            Event::AskAnswer(id, request, answer, reply) => {
                let Some(index) = self.index_of(id) else {
                    let _ = reply.send(false);
                    return Ok(false);
                };
                let session = &mut self.sessions[index];
                let Some(position) = session.permissions.iter().position(|r| r.id == request) else {
                    let _ = reply.send(false);
                    return Ok(false);
                };
                let Some(decision) = crate::ask::decision(&session.permissions[position], &answer) else {
                    let _ = reply.send(false);
                    return Ok(false);
                };
                if let Some(pending) = session.permissions.remove(position) {
                    pending.answer_with(decision);
                }
                let _ = reply.send(true);
                Ok(true)
            }
            Event::ReloadChecked(path, works) => {
                if works {
                    self.reload_ready = Some(path);
                } else {
                    self.set_flash("Новая сборка vv не запускается — работаю на прежней");
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
            Event::CopyReady(branch, result) => {
                match result {
                    Ok(copy) => {
                        let args = Settings::load(&self.home).claude_args();
                        let program = match copy.setup.is_empty() {
                            true => Program::Claude(args),
                            false => Program::ClaudeAfter(copy.setup, args),
                        };
                        // Коротко: не влезет в нижнюю строку — не покажется. Ветка и так в шапке.
                        let note = match copy.carried.as_slice() {
                            [] => "Копия готова".to_string(),
                            [one] => format!("Копия готова, перенёс {one}"),
                            [first, rest @ ..] => format!("Копия готова, перенёс {first} и ещё {}", rest.len()),
                        };
                        // Копия делалась в фоне. Открыто окно — оно про другую
                        // сессию (закрыть, удалить копию…): выбор не трогаем.
                        let busy = !matches!(self.overlay, Overlay::None);
                        let result = if busy {
                            self.add_session(branch.clone(), &copy.dir, program)
                        } else {
                            self.spawn_session(branch.clone(), &copy.dir, program)
                        };
                        match result {
                            Ok(_) if busy => self.set_flash(format!("Копия «{branch}» готова — она в списке")),
                            Ok(_) => self.set_flash(note),
                            Err(err) => self.set_flash(format!("не открылось: {err:#}")),
                        }
                    }
                    Err(text) if matches!(self.overlay, Overlay::None) => {
                        self.overlay = Overlay::Git(gitui::copy_failed(&text));
                    }
                    Err(text) => self.set_flash(format!("Копия не получилась: {text}")),
                }
                Ok(true)
            }
            Event::CopyRemoved(name, result) => {
                match result {
                    Ok(()) => self.set_flash(format!("Копия «{name}» удалена")),
                    Err(text) if matches!(self.overlay, Overlay::None) => {
                        self.overlay = Overlay::Git(gitui::remove_failed(&text));
                    }
                    Err(_) => self.set_flash(format!("Копия «{name}» не удалилась")),
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
        let settings = Settings::load(&self.home);
        self.keys = settings.keys.clone();
        // Сочетание из настроек: VibeTerminal присылает служебную клавишу,
        // в другом терминале ловим сами.
        let from_app = hotkeys::service(&key);
        let hotkey = from_app.or_else(|| hotkeys::from_key(&key, &settings.keys));
        // Отпускание клавиши знает только VibeTerminal — без него запись
        // всегда по нажатию.
        let hold = settings.voice_mode == "hold" && from_app.is_some();
        // Идёт запись: Enter — распознать, Esc — отменить, остальное не в Claude.
        if matches!(self.voice, VoiceState::Recording { .. }) {
            match (key.code, hotkey) {
                (KeyCode::Enter, _) => self.finish_voice(),
                (KeyCode::Esc, _) => self.cancel_voice(),
                // «Нажать и говорить»: второе нажатие — готово.
                (_, Some(Hotkey::VoicePress)) if !hold => self.finish_voice(),
                // «Удерживать клавишу»: отпустил — готово.
                (_, Some(Hotkey::VoiceRelease)) if hold => self.finish_voice(),
                _ => {}
            }
            return Ok(true);
        }
        if matches!(self.voice, VoiceState::Transcribing { .. }) && key.code == KeyCode::Esc {
            self.cancel_voice();
            return Ok(true);
        }
        if let Some(hotkey) = hotkey {
            return self.on_hotkey(hotkey);
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

    /// Горячая клавиша действия — работает и поверх открытых окон vv.
    fn on_hotkey(&mut self, hotkey: Hotkey) -> Result<bool> {
        let has_session = !self.sessions.is_empty();
        let action = match hotkey {
            Hotkey::VoiceRelease => return Ok(false),
            Hotkey::VoicePress => {
                if !matches!(self.overlay, Overlay::None) {
                    return Ok(false);
                }
                self.start_voice();
                return Ok(true);
            }
            Hotkey::Menu => {
                self.overlay = match self.overlay {
                    Overlay::Menu(_) => Overlay::None,
                    _ => Overlay::Menu(self.selected),
                };
                return Ok(true);
            }
            Hotkey::Next | Hotkey::Previous => {
                let len = self.sessions.len();
                if len == 0 {
                    return Ok(false);
                }
                let step = if hotkey == Hotkey::Next { 1 } else { len - 1 };
                Action::Select((self.selected + step) % len)
            }
            Hotkey::Session(index) if index < self.sessions.len() => Action::Select(index),
            Hotkey::Session(_) => return Ok(false),
            Hotkey::New => Action::New,
            Hotkey::NextWaiting => Action::NextWaiting,
            Hotkey::Usage => Action::Usage,
            Hotkey::Git | Hotkey::Rename | Hotkey::Close if !has_session => return Ok(false),
            Hotkey::Git => Action::Git,
            Hotkey::Rename => Action::Rename,
            Hotkey::Close => Action::Close,
        };
        self.perform(action)?;
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
            Overlay::SessionMenu(menu) => match key.code {
                KeyCode::Esc => self.overlay = Overlay::None,
                KeyCode::Up => menu.move_by(-1),
                KeyCode::Down | KeyCode::Tab => menu.move_by(1),
                KeyCode::Enter => self.session_act(),
                _ => return Ok(false),
            },
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
                _ => match keys::edit(&key) {
                    Some(edit) => edit.apply(input),
                    None => return Ok(false),
                },
            },
            Overlay::Picker(picker) => match (key.code, ctrl) {
                (KeyCode::Esc, _) => self.overlay = Overlay::None,
                (KeyCode::Enter, _) if key.modifiers.contains(KeyModifiers::ALT) => self.copy_picked(),
                (KeyCode::Enter, _) => self.open_picked(),
                (KeyCode::Up, _) | (KeyCode::Char('p' | 'k'), true) => picker.move_by(-1),
                (KeyCode::Down | KeyCode::Tab, _) | (KeyCode::Char('n' | 'j'), true) => picker.move_by(1),
                _ => match keys::edit(&key) {
                    Some(Edit::Insert(c)) => picker.push(c.encode_utf8(&mut [0; 4])),
                    Some(Edit::Backspace) => picker.backspace(),
                    // Слово в пути — до `/`.
                    Some(Edit::DeleteWord) => picker.delete_word(),
                    Some(Edit::Clear) => picker.clear(),
                    None => return Ok(false),
                },
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
            Action::NewCopy => {
                if let Some(dir) = self.current().map(|s| s.cwd.clone()) {
                    self.open_copy(&dir, "");
                }
            }
            Action::Rename => {
                if let Some(session) = self.current() {
                    self.overlay = Overlay::Rename(session.name.clone());
                }
            }
            Action::Close if !self.sessions.is_empty() => self.ask_close(),
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

    /// Спросить, точно ли закрыть. Сессию в копии проекта — и что делать с копией.
    fn ask_close(&mut self) {
        let Some(session) = self.current() else { return };
        self.overlay = match &session.copy_of {
            Some(project) if !session.is_command => {
                // Узнанный недавно статус. Устарел — не беда: без «всё равно»
                // git не удалит копию, где есть изменения.
                let status = session.git.clone().or_else(|| git::status(&session.cwd)).unwrap_or_default();
                let project = picker::folder_name(project);
                let ours = worktree::is_ours(&self.home, &session.cwd);
                let foreign = (!ours).then(|| picker::display_path(&session.cwd, &self.home));
                let (name, branch) = (&session.name, status.head_label());
                Overlay::Git(gitui::close_copy(name, &project, &branch, status.changed, foreign.as_deref()))
            }
            _ => Overlay::Confirm(Confirm::Close),
        };
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
        let Some(path) = path else { return };
        // Там уже работает Claude — второй будет ему мешать.
        let busy = self.sessions.iter().any(|s| !s.is_command && worktree::same_dir(&s.cwd, &path));
        if busy && path.join(".git").exists() {
            self.overlay = Overlay::Git(gitui::folder_busy(&path, &picker::folder_name(&path)));
            return;
        }
        self.open_folder(&path);
    }

    /// ⌥Enter или кнопка в выборе папки: задача в отдельной копии проекта.
    fn copy_picked(&mut self) {
        let Overlay::Picker(picker) = &self.overlay else { return };
        if let Some(path) = picker.selected_path() {
            self.open_copy(&path, "");
        }
    }

    fn open_folder(&mut self, dir: &Path) {
        if let Err(err) = self.open(dir) {
            self.set_flash(format!("не открылось: {err:#}"));
        }
    }

    /// Окно «Задача в отдельной копии» для проекта папки.
    fn open_copy(&mut self, dir: &Path, branch: &str) {
        match gitui::open_copy(dir, branch, &self.home) {
            Some(copy) => self.overlay = Overlay::Git(copy),
            None => {
                self.overlay = Overlay::None;
                self.set_flash("Здесь нет git-репозитория — отдельную копию сделать нельзя");
            }
        }
    }

    /// К сессии в этой папке, а нет такой — открыть её.
    fn go_to(&mut self, dir: &Path) {
        match self.sessions.iter().position(|s| !s.is_command && worktree::same_dir(&s.cwd, dir)) {
            Some(index) => {
                let _ = self.select(index);
            }
            None => self.open_folder(dir),
        }
    }

    /// Выбранный пункт меню сессии.
    fn session_act(&mut self) {
        let Overlay::SessionMenu(menu) = std::mem::replace(&mut self.overlay, Overlay::None) else { return };
        let act = menu.chosen();
        // Сессия могла закрыться, пока меню было открыто.
        let (index, dir) = match &menu.about {
            About::Session(id) => match self.index_of(*id) {
                Some(index) => (Some(index), self.sessions[index].cwd.clone()),
                None => return,
            },
            About::Project(project) => (None, project.clone()),
        };
        match act {
            Act::NewCopy => self.open_copy(&dir, ""),
            Act::ToProject => {
                if let Some(project) = index.and_then(|i| self.sessions[i].copy_of.clone()) {
                    self.go_to(&project);
                }
            }
            Act::OpenProject => self.go_to(&dir),
            Act::Finder => {
                thread::spawn(move || Command::new("open").arg(&dir).stdout(Stdio::null()).stderr(Stdio::null()).status());
            }
            Act::CopyPath => {
                let copied = Command::new("pbcopy").stdin(Stdio::piped()).spawn().and_then(|mut child| {
                    if let Some(mut stdin) = child.stdin.take() {
                        stdin.write_all(dir.as_os_str().as_encoded_bytes())?;
                    }
                    child.wait()
                });
                let ok = copied.is_ok_and(|status| status.success());
                self.set_flash(if ok { "Путь скопирован" } else { "Путь не скопировался" });
            }
            // Остальное — с открытой сессией: переименовать, git, закрыть.
            Act::Git | Act::Rename | Act::Close => {
                let Some(index) = index else { return };
                let _ = self.select(index);
                match act {
                    Act::Git => self.open_git_menu(),
                    Act::Rename => self.overlay = Overlay::Rename(self.sessions[index].name.clone()),
                    _ => self.ask_close(),
                }
            }
        }
    }

    /// Сделать копию в фоне — у большого проекта это секунды — и открыть в ней сессию.
    fn create_copy(&mut self, from: PathBuf, branch: String, setup: String) {
        self.overlay = Overlay::None;
        self.set_flash(format!("Готовлю копию {branch}…"));
        let (home, tx) = (self.home.clone(), self.tx.clone());
        thread::spawn(move || {
            let result = worktree::prepare(&home, &from, &branch, &setup);
            let _ = tx.send(Event::CopyReady(branch, result));
        });
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

        // Правый клик (или Ctrl+клик, как принято в macOS) по сессии в
        // списке — её меню. Поверх диалогов не открываем.
        let context = matches!(event.kind, MouseEventKind::Down(MouseButton::Right))
            || (matches!(event.kind, MouseEventKind::Down(MouseButton::Left)) && event.modifiers.contains(KeyModifiers::CONTROL));
        if context && matches!(self.overlay, Overlay::None | Overlay::SessionMenu(_)) {
            let menu = match ui::slot_at(self, column, row) {
                Some(ui::Slot::Card(index, _)) => Some(SessionMenu::for_session(&self.sessions[index], (column, row))),
                Some(ui::Slot::Project(first)) => {
                    self.sessions[first].copy_of.as_deref().map(|project| SessionMenu::for_project(project, (column, row)))
                }
                None => None,
            };
            if let Some(menu) = menu {
                self.overlay = Overlay::SessionMenu(menu);
                self.hover = None;
                return Ok(true);
            }
        }

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
                (Overlay::SessionMenu(menu), Some(Target::SessionMenuItem(i))) if menu.cursor != i => {
                    menu.cursor = i;
                    dirty = true;
                }
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
                    Overlay::SessionMenu(menu) => !contains(menu.rect(full), column, row),
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
            Target::Credit => {
                thread::spawn(|| Command::new("open").arg(ui::F4_SITE).stdout(Stdio::null()).stderr(Stdio::null()).status());
            }
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
            Target::PickerCopy => self.copy_picked(),
            Target::SessionMenuItem(index) => {
                if let Overlay::SessionMenu(menu) = &mut self.overlay {
                    menu.cursor = index;
                }
                self.session_act();
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
        let mut args = Settings::load(&self.home).claude_args();
        // Копия — это задача: вернулся к ней — продолжить тот же разговор.
        // Если там уже работает Claude, второй в тот же разговор не лезет.
        let busy = self.sessions.iter().any(|s| !s.is_command && worktree::same_dir(&s.cwd, dir));
        if !busy && worktree::is_ours(&self.home, dir) && picker::has_dialogs(&self.home, dir) {
            args.push("--continue".to_string());
        }
        self.spawn_session(folder, dir, Program::Claude(args)).map(drop)
    }

    /// Перезапуститься новой версией vv тем же процессом, отдав ей сессии:
    /// терминалы с Claude остаются открытыми, Claude работает дальше.
    fn hot_reload(&mut self, path: &Path, terminal: &mut Screen) -> Result<()> {
        let handoff = reload::Handoff {
            sessions: self.sessions.iter().filter_map(Session::handoff).collect(),
            selected: self.selected,
            show_sidebar: self.show_sidebar,
        };
        let file = reload::write(&self.home, &handoff)?;
        self.save_state();
        voice::unload();
        terminal.backend_mut().flush()?;
        // Экран не трогаем — новая версия сразу нарисует своё поверх.
        // Обычный режим терминала возвращаем, чтобы новая версия запомнила
        // его как исходный.
        if KEYBOARD_ENHANCED.load(Ordering::Relaxed) {
            let _ = execute!(io::stdout(), PopKeyboardEnhancementFlags);
        }
        let _ = terminal::disable_raw_mode();
        let err = reload::exec(path, &file);
        // Не вышло — работаем дальше прежней версией.
        let _ = std::fs::remove_file(&file);
        setup_terminal()?;
        self.set_flash(format!("Новая версия vv не запустилась: {err}"));
        Ok(())
    }

    /// Принять сессии от прежней версии vv после перезагрузки. `false` —
    /// принять нечего.
    fn adopt(&mut self, handoff: reload::Handoff) -> bool {
        let agent = self.areas.agent;
        let size = (agent.height.max(1), agent.width.max(1));
        for handed in &handoff.sessions {
            let (id, tx) = (handed.id, self.tx.clone());
            let adopted = Session::adopt(handed, size, move |chunk| {
                let _ = tx.send(chunk.map_or(Event::Exited(id), |bytes| Event::Output(id, bytes)));
            });
            match adopted {
                Ok(session) => self.sessions.push(session),
                Err(err) => self.set_flash(err.to_string()),
            }
            self.next_id = self.next_id.max(handed.id + 1);
        }
        if self.sessions.is_empty() {
            return false;
        }
        self.show_sidebar = handoff.show_sidebar;
        let _ = self.relayout(self.areas.full);
        let selected = self.sessions[handoff.selected.min(self.sessions.len() - 1)].id;
        self.arrange();
        let _ = self.select(self.index_of(selected).unwrap_or(0));
        // Экран уже как был, но пусть Claude перерисуется — на случай, если
        // что-то вывел, пока vv перезапускался.
        for session in &mut self.sessions {
            session.redraw();
        }
        self.set_flash("vv обновился — сессии на месте");
        true
    }

    /// Вернуть сессии, открытые до перезапуска: диалоги Claude продолжаются
    /// с той же историей. `false` — ни одну открыть не вышло.
    fn restore(&mut self, window: SavedWindow) -> bool {
        let args = Settings::load(&self.home).claude_args();
        // Сессии встают деревом, а каких-то папок уже нет, — номера сдвигаются.
        // Свою сессию и выбранную ищем по её номеру в vv, а не по месту в списке.
        let mut selected = None;
        for (i, saved) in window.sessions.iter().enumerate().filter(|(_, s)| s.cwd.is_dir()) {
            let mut args = args.clone();
            if let Some(id) = saved.resumable() {
                args.extend(["--resume".to_string(), id.to_string()]);
            }
            if let Ok(id) = self.spawn_session(saved.name.clone(), &saved.cwd, Program::Claude(args))
                && let Some(index) = self.index_of(id)
            {
                let session = &mut self.sessions[index];
                session.claude_id.clone_from(&saved.claude_id);
                session.transcript.clone_from(&saved.transcript);
                if i == window.selected {
                    selected = Some(id);
                }
            }
        }
        if self.sessions.is_empty() {
            return false;
        }
        let _ = self.select(selected.and_then(|id| self.index_of(id)).unwrap_or(0));
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

    fn spawn_session(&mut self, name: String, dir: &Path, program: Program) -> Result<SessionId> {
        let id = self.add_session(name, dir, program)?;
        let index = self.index_of(id).unwrap_or(self.sessions.len() - 1);
        self.select(index)?;
        Ok(id)
    }

    /// Запустить сессию и поставить в список, не открывая её.
    fn add_session(&mut self, name: String, dir: &Path, program: Program) -> Result<SessionId> {
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
        self.arrange();
        Ok(id)
    }

    /// Сессии деревом: копии — сразу под своим проектом. Номера в меню и
    /// ⌘1–9 идут в том же порядке, что и в списке.
    fn arrange(&mut self) {
        let order = ui::tree_order(&ui::tree_keys(&self.sessions));
        if order.iter().enumerate().all(|(i, &j)| i == j) {
            return;
        }
        let selected = self.current().map(|s| s.id);
        let mut taken: Vec<Option<Session>> = std::mem::take(&mut self.sessions).into_iter().map(Some).collect();
        self.sessions = order.into_iter().filter_map(|i| taken[i].take()).collect();
        if let Some(index) = selected.and_then(|id| self.index_of(id)) {
            self.selected = index;
        }
    }

    /// Закрыть выбранную сессию: Claude гасим в фоне, чтобы не ждать.
    fn close_current(&mut self) {
        self.close_selected(None);
    }

    /// Закрыть выбранную сессию; `Some(force)` — и удалить её копию
    /// проекта, когда Claude остановится.
    fn close_selected(&mut self, remove: Option<bool>) {
        if self.selected >= self.sessions.len() {
            return;
        }
        let mut session = self.sessions.remove(self.selected);
        let copy = remove.zip(session.copy_of.clone()).map(|(force, project)| (project, session.cwd.clone(), force));
        let name = session.name.clone();
        let note = if copy.is_some() { format!("«{name}» закрыта, удаляю копию…") } else { format!("«{name}» закрыта") };
        self.set_flash(note);
        let tx = self.tx.clone();
        self.stopping.push(thread::spawn(move || {
            session::stop_all(std::slice::from_mut(&mut session));
            if let Some((project, dir, force)) = copy {
                let _ = tx.send(Event::CopyRemoved(name, worktree::remove(&project, &dir, force)));
            }
        }));
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
            // Ты в другом приложении, Claude ждёт ответа — окно поверх всех
            // программ, где можно сразу ответить. Нет такого окна — баннер.
            let request = self.sessions.get(index).and_then(|s| s.pending_permission()).map(|r| r.id);
            let popup = kind == notify::Kind::Waiting && !self.focused && settings.notify_popup;
            let asked = match request {
                Some(request) if popup => self.notifier.ask(&settings, id, request, title.clone(), &body),
                _ => false,
            };
            if !asked {
                self.notifier.send(&settings, id, title, &body, self.focused);
            }
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
            Ok(recording) => {
                if settings.voice_sounds {
                    notify::cue(notify::Cue::Begin);
                }
                self.voice = VoiceState::Recording { session: id, recording, hint };
            }
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
        if settings.voice_sounds {
            notify::cue(notify::Cue::Confirm);
        }
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
        let was = std::mem::replace(&mut self.voice, VoiceState::Idle);
        if !matches!(was, VoiceState::Idle) && Settings::load(&self.home).voice_sounds {
            notify::cue(notify::Cue::Cancel);
        }
        if let VoiceState::Recording { recording, .. } = was {
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
            gitui::Command::OpenCopy { from, branch } => {
                self.open_copy(&from, &branch);
                true
            }
            gitui::Command::NewCopy { from, branch, setup } => {
                self.create_copy(from, branch, setup);
                true
            }
            gitui::Command::OpenFolder(dir) => {
                self.overlay = Overlay::None;
                self.open_folder(&dir);
                true
            }
            gitui::Command::GoTo(dir) => {
                self.overlay = Overlay::None;
                self.go_to(&dir);
                true
            }
            gitui::Command::CloseSession(remove) => {
                self.overlay = Overlay::None;
                self.close_selected(remove);
                true
            }
            gitui::Command::Ask(next) => {
                self.overlay = Overlay::Git(GitOverlay::Choice(*next));
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
