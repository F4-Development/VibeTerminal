//! Уведомления на Mac: баннер и звук, когда сессия ждёт тебя, закончила
//! или упала, а ты смотришь не на неё.
//!
//! Баннер в VibeTerminal показывает само приложение: vv пишет в терминал
//! OSC 777 с меткой сессии, VibeTerminal убирает метку, ставит нашу иконку,
//! а по клику выводит окно вперёд и через сокет vv просит открыть сессию.
//! В другом терминале — обычное уведомление через `osascript`. Звук vv
//! играет сам, чтобы его можно было выбрать в настройках.

use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::session::SessionId;
use crate::settings::{self, Settings};

/// VibeTerminal (как и Ghostty) показывает не больше одного баннера в секунду
/// и молча выкидывает лишние — отправляем с запасом.
const BANNER_GAP: Duration = Duration::from_millis(1200);
/// Несколько событий подряд — один звук.
const SOUND_GAP: Duration = Duration::from_secs(1);
const SOUNDS_DIR: &str = "/System/Library/Sounds";
const BODY_MAX: usize = 180;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    /// Разрешение, вопрос или план.
    Waiting,
    Done,
    Failed,
}

impl Kind {
    pub fn enabled(self, settings: &Settings) -> bool {
        match self {
            Kind::Waiting => settings.notify_waiting,
            Kind::Done => settings.notify_done,
            Kind::Failed => settings.notify_failed,
        }
    }
}

struct Banner {
    session: SessionId,
    title: String,
    body: String,
    /// Не баннер, а всплывающее окно для ответа на этот запрос.
    ask: Option<u64>,
}

pub struct Notifier {
    queue: VecDeque<Banner>,
    last_banner: Option<Instant>,
    last_sound: Option<Instant>,
    /// Сокет этого окна vv: по нему VibeTerminal сообщит о клике.
    socket: PathBuf,
    in_vibeterminal: bool,
    /// Эта версия VibeTerminal умеет окно для ответа (ставит `VIBETERMINAL_ASK`).
    can_ask: bool,
}

impl Notifier {
    pub fn new(socket: PathBuf) -> Self {
        let in_vibeterminal = settings::in_vibeterminal();
        let can_ask = in_vibeterminal && std::env::var_os("VIBETERMINAL_ASK").is_some();
        Self { queue: VecDeque::new(), last_banner: None, last_sound: None, socket, in_vibeterminal, can_ask }
    }

    /// Звук сразу, баннер — в очередь, если окно не в фокусе.
    pub fn send(&mut self, settings: &Settings, session: SessionId, title: String, body: &str, focused: bool) {
        if self.last_sound.is_none_or(|at| at.elapsed() >= SOUND_GAP) && play(&settings.notify_sound) {
            self.last_sound = Some(Instant::now());
        }
        if settings.notify_banner && !focused {
            // Новое уведомление сессии заменяет ещё не показанное.
            self.queue.retain(|banner| banner.session != session);
            self.queue.push_back(Banner { session, title, body: short(body), ask: None });
        }
    }

    /// Всплывающее окно VibeTerminal для ответа. Только в VibeTerminal —
    /// `false`, если здесь его нет (тогда нужен обычный баннер).
    pub fn ask(&mut self, settings: &Settings, session: SessionId, request: u64, title: String, body: &str) -> bool {
        if !self.can_ask {
            return false;
        }
        if self.last_sound.is_none_or(|at| at.elapsed() >= SOUND_GAP) && play(&settings.notify_sound) {
            self.last_sound = Some(Instant::now());
        }
        self.queue.retain(|banner| banner.session != session);
        self.queue.push_back(Banner { session, title, body: short(body), ask: Some(request) });
        true
    }

    /// Окно снова в фокусе — неотправленные баннеры уже не нужны.
    pub fn clear(&mut self) {
        self.queue.clear();
    }

    /// Отправляет следующий баннер, если пора. Возвращает, через сколько
    /// нужно позвать снова, если в очереди ещё что-то есть.
    pub fn flush(&mut self, out: &mut impl Write) -> std::io::Result<Option<Duration>> {
        if self.queue.is_empty() {
            return Ok(None);
        }
        if let Some(wait) = self.last_banner.and_then(|at| BANNER_GAP.checked_sub(at.elapsed())) {
            return Ok(Some(wait));
        }
        let Some(banner) = self.queue.pop_front() else { return Ok(None) };
        if self.in_vibeterminal {
            out.write_all(osc_777(&banner, &self.socket).as_bytes())?;
            out.flush()?;
        } else {
            osascript(&banner);
        }
        self.last_banner = Some(Instant::now());
        Ok((!self.queue.is_empty()).then_some(BANNER_GAP))
    }
}

/// `OSC 777 ; notify ; ⟦vv:сессия:сокет⟧заголовок ; текст`. Метку понимает
/// только VibeTerminal — в другие терминалы так не пишем.
fn osc_777(banner: &Banner, socket: &Path) -> String {
    // `;` делит поля, управляющие символы оборвали бы последовательность.
    let title = clean(&banner.title).replace(';', ",");
    let body = clean(&banner.body);
    let mark = match banner.ask {
        Some(request) => crate::ask::marker(banner.session, request, socket),
        None => format!("⟦vv:{}:{}⟧", banner.session, socket.display()),
    };
    format!("\x1b]777;notify;{mark}{title};{body}\x1b\\")
}

fn osascript(banner: &Banner) {
    let quote = |text: &str| clean(text).replace('\\', "\\\\").replace('"', "\\\"");
    let script = format!("display notification \"{}\" with title \"{}\"", quote(&banner.body), quote(&banner.title));
    spawn(Command::new("osascript").args(["-e", &script]));
}

/// Играет системный звук. `false` — звук выключен или такого нет.
pub fn play(name: &str) -> bool {
    let Some(path) = sound_path(name) else { return false };
    spawn(Command::new("afplay").arg(path));
    true
}

/// Звуки диктовки macOS — те же, что при нажатии клавиши микрофона.
const DICTATION_SOUNDS: &str = "/System/Library/PrivateFrameworks/AssistantServices.framework/Versions/A/Resources";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cue {
    /// Запись пошла.
    Begin,
    /// Запись закончена — распознаём.
    Confirm,
    /// Запись отменили.
    Cancel,
}

/// Сыграть звук диктовки. Нет файла (другая версия macOS) — тихо.
pub fn cue(cue: Cue) {
    let file = match cue {
        Cue::Begin => "dt-begin.caf",
        Cue::Confirm => "dt-confirm.caf",
        Cue::Cancel => "dt-cancel.caf",
    };
    let path = Path::new(DICTATION_SOUNDS).join(file);
    if path.exists() {
        spawn(Command::new("afplay").arg(path));
    }
}

fn sound_path(name: &str) -> Option<PathBuf> {
    let name = name.trim();
    if name.is_empty() || name.contains(['/', '.']) {
        return None;
    }
    let path = Path::new(SOUNDS_DIR).join(format!("{name}.aiff"));
    path.exists().then_some(path)
}

/// Запустить и дождаться в фоне, чтобы не копились зомби.
fn spawn(command: &mut Command) {
    let child = command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn();
    if let Ok(mut child) = child {
        thread::spawn(move || {
            let _ = child.wait();
        });
    }
}

fn clean(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect::<String>().trim().to_string()
}

fn short(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default();
    match line.char_indices().nth(BODY_MAX) {
        Some((cut, _)) => format!("{}…", &line[..cut]),
        None => line.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marks_banner_for_vibeterminal() {
        let banner = Banner { session: 3, title: "api; закончил".into(), body: "Готово\x07 всё".into(), ask: None };
        let osc = osc_777(&banner, Path::new("/tmp/run/1.sock"));
        assert_eq!(osc, "\x1b]777;notify;⟦vv:3:/tmp/run/1.sock⟧api, закончил;Готово  всё\x1b\\");
    }

    #[test]
    fn spaces_banners_and_replaces_per_session() {
        let mut notifier = Notifier::new("/tmp/s.sock".into());
        notifier.in_vibeterminal = true;
        let settings = Settings { notify_sound: String::new(), ..Settings::default() };
        notifier.send(&settings, 1, "a ждёт".into(), "первое", false);
        notifier.send(&settings, 1, "a ждёт".into(), "второе", false);
        notifier.send(&settings, 2, "b закончил".into(), "ответ", false);
        // Смотришь в окно — баннера нет.
        notifier.send(&settings, 3, "c".into(), "x", true);
        let mut out = Vec::new();
        assert_eq!(notifier.flush(&mut out).unwrap(), Some(BANNER_GAP));
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("второе") && !text.contains("первое"));
        let mut out = Vec::new();
        assert!(notifier.flush(&mut out).unwrap().is_some_and(|wait| wait <= BANNER_GAP));
        assert!(out.is_empty(), "второй баннер не раньше чем через {BANNER_GAP:?}");
    }

    #[test]
    fn sounds_only_from_system_folder() {
        assert!(sound_path("").is_none());
        assert!(sound_path("../../etc/passwd").is_none());
        assert!(sound_path("Нет такого").is_none());
        if Path::new(SOUNDS_DIR).join("Glass.aiff").exists() {
            assert!(sound_path("Glass").is_some());
        }
    }

    #[test]
    fn shortens_body() {
        assert_eq!(short("\n  первая строка\nвторая"), "первая строка");
        assert_eq!(short(&"я".repeat(200)).chars().count(), BODY_MAX + 1);
    }
}
