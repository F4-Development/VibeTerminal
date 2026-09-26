//! Статус сессии в списке: работает, ждёт тебя, готово.
//!
//! Собирается из событий Claude (хуков). Хуки асинхронные и приходят не по
//! порядку — у каждого есть время запуска, более старые пропускаем. «Ждёт»
//! не хранится здесь: это просто ждущий запрос в сессии.

use std::path::Path;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::hooks::{self, HookEvent};
use crate::session::Session;

/// После Esc Claude молчит столько — значит, его остановили. Своего
/// события о прерывании у Claude Code нет.
const INTERRUPT_SETTLE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    /// Только запустили, ты посмотрел ответ или прервал.
    Idle,
    Working,
    /// Закончил, а ты ещё не смотрел.
    Done,
    /// Ход оборвался ошибкой: лимит, вход, сервер.
    Failed,
}

pub struct Status {
    pub state: State,
    pub since: Instant,
    /// Текущее действие, первая строка ответа или ошибка.
    pub detail: String,
    last_event: u64,
    /// Когда нажали Esc (и то же время в мс, чтобы сравнивать с событиями).
    interrupted: Option<(Instant, u64)>,
}

impl Default for Status {
    fn default() -> Self {
        Self { state: State::Idle, since: Instant::now(), detail: String::new(), last_event: 0, interrupted: None }
    }
}

impl Status {
    /// Событие от Claude. `true` — статус поменялся.
    pub fn on_event(&mut self, event: &HookEvent, cwd: &Path) -> bool {
        if event.at < self.last_event {
            return false;
        }
        let (state, detail) = match event.name.as_str() {
            "UserPromptSubmit" => (State::Working, String::new()),
            "PreToolUse" => (State::Working, hooks::action(&event.tool, &event.input, cwd)),
            // Инструмент закончил — Claude думает дальше, действие оставляем.
            "PostToolUse" | "PostToolUseFailure" | "PermissionDenied" => {
                let detail = if self.state == State::Working { self.detail.clone() } else { String::new() };
                (State::Working, detail)
            }
            "Stop" => (State::Done, first_line(&event.message)),
            "StopFailure" => (State::Failed, failure(&event.error).to_string()),
            _ => return false,
        };
        self.last_event = event.at;
        if self.interrupted.is_some_and(|(_, at)| event.at > at) {
            self.interrupted = None;
        }
        self.set(state, detail)
    }

    /// Нажали Esc или Ctrl-C: может, остановили Claude.
    pub fn interrupt(&mut self) {
        if self.state == State::Working {
            self.interrupted = Some((Instant::now(), now_ms()));
        }
    }

    /// После Esc Claude замолчал — считаем, что остановили. `true` — поменялся.
    pub fn settle(&mut self) -> bool {
        match self.interrupted {
            Some((at, _)) if at.elapsed() >= INTERRUPT_SETTLE => {
                self.interrupted = None;
                self.state == State::Working && self.set(State::Idle, "прервано".into())
            }
            _ => false,
        }
    }

    /// Ты посмотрел сессию: «готово» и ошибка прочитаны.
    pub fn seen(&mut self) -> bool {
        self.unread() && self.set(State::Idle, String::new())
    }

    pub fn unread(&self) -> bool {
        matches!(self.state, State::Done | State::Failed)
    }

    fn set(&mut self, state: State, detail: String) -> bool {
        let changed = state != self.state || detail != self.detail;
        if state != self.state {
            self.since = Instant::now();
        }
        self.state = state;
        self.detail = detail;
        changed
    }
}

/// Кто ждёт тебя, по очереди: сначала запросы (кто дольше ждёт — первым),
/// потом «готово». Открытую сейчас сессию не считаем.
pub fn queue(sessions: &[Session], selected: usize) -> Vec<usize> {
    let mut asking = Vec::new();
    let mut done = Vec::new();
    for (index, session) in sessions.iter().enumerate().filter(|(i, _)| *i != selected) {
        if let Some(first) = session.permissions.front() {
            asking.push((first.at, index));
        } else if session.status.unread() {
            done.push((session.status.since, index));
        }
    }
    asking.sort();
    done.sort();
    asking.into_iter().chain(done).map(|(_, index)| index).collect()
}

/// «12с», «5м», «1ч 20м».
pub fn elapsed(since: Instant) -> String {
    let secs = since.elapsed().as_secs();
    match secs {
        0..60 => format!("{secs}с"),
        60..3600 => format!("{}м", secs / 60),
        _ => match (secs / 3600, secs % 3600 / 60) {
            (hours, 0) => format!("{hours}ч"),
            (hours, minutes) => format!("{hours}ч {minutes}м"),
        },
    }
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

/// Первая строка ответа без разметки Markdown.
fn first_line(text: &str) -> String {
    let line = text.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or_default();
    line.trim_start_matches(['#', '>', '-', '*', ' ']).replace("**", "").replace('`', "")
}

fn failure(error: &str) -> &'static str {
    match error {
        "rate_limit" => "Упёрся в лимит, подожди",
        "overloaded" | "server_error" => "Сервер Anthropic перегружен",
        "authentication_failed" | "oauth_org_not_allowed" | "verification_required" | "cloud_credential_error" => {
            "Нужно войти заново"
        }
        "billing_error" | "account_on_hold" => "Проблема с оплатой",
        "max_output_tokens" => "Ответ не влез в лимит",
        "model_not_found" => "Нет такой модели",
        _ => "Ошибка API",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(name: &str, at: u64) -> HookEvent {
        HookEvent { name: name.into(), at, ..HookEvent::default() }
    }

    #[test]
    fn follows_a_turn() {
        let cwd = Path::new("/p");
        let mut status = Status::default();
        assert!(status.on_event(&event("UserPromptSubmit", 1), cwd));
        assert_eq!(status.state, State::Working);
        let bash = HookEvent { tool: "Bash".into(), input: json!({"command": "cargo test"}), ..event("PreToolUse", 2) };
        status.on_event(&bash, cwd);
        assert_eq!(status.detail, "$ cargo test");
        status.on_event(&event("PostToolUse", 3), cwd);
        assert_eq!(status.detail, "$ cargo test");
        let stop = HookEvent { message: "\n## **Готово**: тесты зелёные\nДальше…".into(), ..event("Stop", 5) };
        status.on_event(&stop, cwd);
        assert_eq!((status.state, status.detail.as_str()), (State::Done, "Готово: тесты зелёные"));
        // Опоздавшее событие от инструмента не возвращает «работает».
        assert!(!status.on_event(&event("PostToolUse", 4), cwd));
        assert_eq!(status.state, State::Done);
        assert!(status.seen());
        assert_eq!(status.state, State::Idle);
    }

    #[test]
    fn api_errors_are_readable() {
        let mut status = Status::default();
        status.on_event(&HookEvent { error: "rate_limit".into(), ..event("StopFailure", 1) }, Path::new("/"));
        assert_eq!((status.state, status.detail.as_str()), (State::Failed, "Упёрся в лимит, подожди"));
    }

    #[test]
    fn esc_without_new_events_means_interrupted() {
        let cwd = Path::new("/p");
        let mut status = Status::default();
        status.on_event(&event("UserPromptSubmit", now_ms()), cwd);
        status.interrupt();
        // Событие после Esc — Claude работает дальше.
        status.on_event(&event("PreToolUse", now_ms() + 1), cwd);
        status.interrupted = status.interrupted.map(|(_, ms)| (Instant::now() - INTERRUPT_SETTLE, ms));
        assert!(!status.settle());
        assert_eq!(status.state, State::Working);

        status.interrupt();
        status.interrupted = status.interrupted.map(|(_, ms)| (Instant::now() - INTERRUPT_SETTLE, ms));
        assert!(status.settle());
        assert_eq!((status.state, status.detail.as_str()), (State::Idle, "прервано"));
    }

    #[test]
    fn formats_elapsed() {
        let ago = |secs| Instant::now() - Duration::from_secs(secs);
        assert_eq!(elapsed(ago(12)), "12с");
        assert_eq!(elapsed(ago(300)), "5м");
        assert_eq!(elapsed(ago(3600)), "1ч");
        assert_eq!(elapsed(ago(4800)), "1ч 20м");
    }
}
