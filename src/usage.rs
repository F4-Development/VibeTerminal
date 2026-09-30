//! Лимиты подписки Claude — те же, что `/usage` в самом Claude.
//!
//! Спрашиваем у `claude`: `claude -p /usage` печатает их сам, со своим
//! входом. Модель не зовётся, диалог в историю не пишется, настройки и хуки
//! пользователя не грузятся. Токен vv не трогает.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::session::PARENT_CLAUDE_VARS;

const FETCH_TIMEOUT: Duration = Duration::from_secs(40);
const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
const MONTHS_RU: [&str; 12] = ["янв", "фев", "мар", "апр", "мая", "июн", "июл", "авг", "сен", "окт", "ноя", "дек"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Limit {
    /// `session`, `week`, `week:Fable` — по нему запоминаем, что показывать.
    pub key: String,
    /// «Сессия (5 ч)», «Неделя, все модели», «Неделя, Fable».
    pub label: String,
    /// Коротко для строки внизу: «5 ч», «неделя», «Fable».
    pub short: String,
    pub percent: u8,
    /// Когда сбросится, секунды Unix.
    pub resets_at: Option<i64>,
}

/// Свежий процент из строки состояния Claude — между запросами `/usage`.
/// `true` — что-то поменялось.
pub fn set_live(limits: &mut Vec<Limit>, key: &str, percent: f64) -> bool {
    let percent = percent.round().clamp(0.0, 100.0) as u8;
    if let Some(limit) = limits.iter_mut().find(|limit| limit.key == key) {
        let changed = limit.percent != percent;
        limit.percent = percent;
        return changed;
    }
    let name = if key == "session" { "Current session" } else { "Current week (all models)" };
    let (key, label, short) = names(name);
    let at = if key == "session" { 0 } else { limits.len().min(1) };
    limits.insert(at, Limit { key, label, short, percent, resets_at: None });
    true
}

/// Спросить у `claude`. Долго (несколько секунд) — звать из фонового потока.
pub fn fetch() -> Result<Vec<Limit>, String> {
    let dir = std::env::temp_dir().join("vibeterminal-usage");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let mut command = Command::new("claude");
    command
        .args(["-p", "/usage", "--no-session-persistence", "--setting-sources", "project"])
        .current_dir(&dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for var in PARENT_CLAUDE_VARS {
        command.env_remove(var);
    }
    // SAFETY: setsid безопасен между fork и exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().map_err(|_| "claude не найден".to_string())?;
    let started = Instant::now();
    while child.try_wait().map_err(|e| e.to_string())?.is_none() {
        if started.elapsed() > FETCH_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err("claude не ответил".into());
        }
        thread::sleep(Duration::from_millis(100));
    }
    let output = child.wait_with_output().map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&output.stdout);
    let limits = parse(&text, now());
    if limits.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let first = text.lines().chain(stderr.lines()).find(|l| !l.trim().is_empty());
        return Err(match first {
            Some(line) if line.contains("API") || line.contains("cost") => "у этого входа нет лимитов подписки".into(),
            Some(line) => line.trim().to_string(),
            None => "claude не показал лимиты".into(),
        });
    }
    Ok(limits)
}

/// `Current session: 38% used · resets Sep 26 at 5:10pm (America/…)`.
fn parse(text: &str, now: i64) -> Vec<Limit> {
    text.lines().filter_map(|line| parse_line(line.trim(), now)).collect()
}

fn parse_line(line: &str, now: i64) -> Option<Limit> {
    let (name, rest) = line.split_once(": ")?;
    let (percent, rest) = rest.split_once("% used")?;
    let percent = percent.trim().parse::<f64>().ok()?.round().clamp(0.0, 100.0) as u8;
    let resets_at = rest.split_once("resets ").and_then(|(_, when)| {
        let when = when.split(" (").next().unwrap_or(when);
        parse_reset(when.trim(), now)
    });
    let (key, label, short) = names(name.trim());
    Some(Limit { key, label, short, percent, resets_at })
}

fn names(name: &str) -> (String, String, String) {
    if name == "Current session" {
        return ("session".into(), "Сессия (5 ч)".into(), "5 ч".into());
    }
    if let Some(scope) = name.strip_prefix("Current week (").and_then(|s| s.strip_suffix(')')) {
        if scope == "all models" {
            return ("week".into(), "Неделя, все модели".into(), "неделя".into());
        }
        let model = scope.strip_suffix(" only").unwrap_or(scope);
        let label = if scope.ends_with(" only") { format!("Неделя, только {model}") } else { format!("Неделя, {model}") };
        return (format!("week:{model}"), label, model.to_string());
    }
    (name.to_lowercase(), name.to_string(), name.to_string())
}

/// `Sep 26 at 5:10pm`, `Sep 28 at 2pm`, `5:10pm` — местное время.
fn parse_reset(text: &str, now: i64) -> Option<i64> {
    let mut words = text.split_whitespace().peekable();
    let today = local(now)?;
    let (mut month, mut day) = (today.tm_mon, today.tm_mday);
    if let Some(index) = words.peek().and_then(|w| MONTHS.iter().position(|m| m == w)) {
        words.next();
        month = index as i32;
        day = words.next()?.trim_end_matches(',').parse().ok()?;
        if words.peek() == Some(&"at") {
            words.next();
        }
    }
    let (hour, minute) = match words.next() {
        Some(time) => parse_clock(time)?,
        None => (0, 0),
    };
    let mut tm = today;
    tm.tm_mon = month;
    tm.tm_mday = day;
    tm.tm_hour = hour;
    tm.tm_min = minute;
    tm.tm_sec = 0;
    tm.tm_isdst = -1;
    let mut at = make_time(tm)?;
    // Без даты — ближайшее такое время; дата в прошлом — это уже следующий год.
    if at < now - 60 {
        if words_had_date(text) {
            tm.tm_year += 1;
        } else {
            tm.tm_mday += 1;
        }
        at = make_time(tm)?;
    }
    Some(at)
}

fn words_had_date(text: &str) -> bool {
    text.split_whitespace().next().is_some_and(|w| MONTHS.contains(&w))
}

/// `5:10pm` → (17, 10), `2pm` → (14, 0), `14:00` → (14, 0).
fn parse_clock(text: &str) -> Option<(i32, i32)> {
    let lower = text.to_lowercase();
    let (clock, pm) = match (lower.strip_suffix("pm"), lower.strip_suffix("am")) {
        (Some(clock), _) => (clock.to_string(), Some(true)),
        (_, Some(clock)) => (clock.to_string(), Some(false)),
        _ => (lower.clone(), None),
    };
    let (hour, minute) = clock.split_once(':').unwrap_or((&clock, "0"));
    let mut hour: i32 = hour.parse().ok()?;
    let minute: i32 = minute.parse().ok()?;
    match pm {
        Some(true) if hour < 12 => hour += 12,
        Some(false) if hour == 12 => hour = 0,
        _ => {}
    }
    ((0..24).contains(&hour) && (0..60).contains(&minute)).then_some((hour, minute))
}

/// «сброс сегодня в 17:10 · через 2 ч 25 мин», «сброс 28 сен в 14:00 · через 2 дн».
pub fn reset_text(at: i64) -> String {
    let now = now();
    let (Some(when), Some(today)) = (local(at), local(now)) else { return String::new() };
    let clock = format!("{}:{:02}", when.tm_hour, when.tm_min);
    let same_day = |a: &libc::tm, b: &libc::tm| a.tm_year == b.tm_year && a.tm_yday == b.tm_yday;
    let tomorrow = local(now + 86_400);
    let day = if same_day(&when, &today) {
        format!("сегодня в {clock}")
    } else if tomorrow.is_some_and(|t| same_day(&when, &t)) {
        format!("завтра в {clock}")
    } else {
        format!("{} {} в {clock}", when.tm_mday, MONTHS_RU[when.tm_mon.clamp(0, 11) as usize])
    };
    let left = (at - now).max(0);
    let (days, hours, minutes) = (left / 86_400, left % 86_400 / 3600, left % 3600 / 60);
    let rest = match (days, hours, minutes) {
        (0, 0, m) => format!("{m} мин"),
        (0, h, m) => format!("{h} ч {m} мин"),
        (d, h, _) => format!("{d} дн {h} ч"),
    };
    format!("сброс {day} · через {rest}")
}

pub fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

pub fn local(at: i64) -> Option<libc::tm> {
    let time = at as libc::time_t;
    // SAFETY: localtime_r пишет только в переданную структуру.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        (!libc::localtime_r(&time, &mut tm).is_null()).then_some(tm)
    }
}

pub fn make_time(mut tm: libc::tm) -> Option<i64> {
    // SAFETY: mktime читает и нормализует только переданную структуру.
    let at = unsafe { libc::mktime(&mut tm) };
    (at != -1).then_some(at as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "You are currently using your subscription to power your Claude Code usage

Current session: 38% used · resets Sep 26 at 5:10pm (America/Buenos_Aires)
Current week (all models): 66% used · resets Sep 28 at 2pm (America/Buenos_Aires)
Current week (Sonnet only): 3% used · resets Sep 28 at 2pm (America/Buenos_Aires)
Current week (Fable): 35% used · resets Sep 28 at 2pm (America/Buenos_Aires)

What's contributing to your limits usage?
Last 24h · 3563 requests · 26 sessions
  99% of your usage came from subagent-heavy sessions";

    #[test]
    fn parses_usage_like_claude_prints_it() {
        let now = make_time(libc::tm { tm_year: 126, tm_mon: 8, tm_mday: 26, tm_hour: 14, tm_isdst: -1, ..zeroed() }).unwrap();
        let limits = parse(SAMPLE, now);
        let keys: Vec<_> = limits.iter().map(|l| (l.key.as_str(), l.short.as_str(), l.percent)).collect();
        assert_eq!(
            keys,
            [("session", "5 ч", 38), ("week", "неделя", 66), ("week:Sonnet", "Sonnet", 3), ("week:Fable", "Fable", 35)]
        );
        assert_eq!(limits[2].label, "Неделя, только Sonnet");
        let reset = local(limits[0].resets_at.unwrap()).unwrap();
        assert_eq!((reset.tm_mon, reset.tm_mday, reset.tm_hour, reset.tm_min), (8, 26, 17, 10));
        let reset = local(limits[1].resets_at.unwrap()).unwrap();
        assert_eq!((reset.tm_mday, reset.tm_hour, reset.tm_min), (28, 14, 0));
    }

    #[test]
    fn live_percent_updates_or_adds() {
        let mut limits = Vec::new();
        assert!(set_live(&mut limits, "week", 66.4));
        assert!(set_live(&mut limits, "session", 38.0));
        assert_eq!(limits.iter().map(|l| (l.key.as_str(), l.percent)).collect::<Vec<_>>(), [("session", 38), ("week", 66)]);
        assert!(!set_live(&mut limits, "session", 38.2));
        assert!(set_live(&mut limits, "session", 41.0));
    }

    #[test]
    fn reads_clock_times() {
        assert_eq!(parse_clock("5:10pm"), Some((17, 10)));
        assert_eq!(parse_clock("2pm"), Some((14, 0)));
        assert_eq!(parse_clock("12am"), Some((0, 0)));
        assert_eq!(parse_clock("12:30pm"), Some((12, 30)));
        assert_eq!(parse_clock("14:00"), Some((14, 0)));
        assert_eq!(parse_clock("25pm"), None);
    }

    #[test]
    fn describes_reset_in_russian() {
        let text = reset_text(now() + 2 * 3600 + 25 * 60 + 30);
        assert!(text.starts_with("сброс ") && text.ends_with("через 2 ч 25 мин"), "{text}");
        assert!(reset_text(now() + 3 * 86_400).contains("через 3 дн"));
    }

    fn zeroed() -> libc::tm {
        // SAFETY: tm — простая структура из чисел и указателя, нули допустимы.
        unsafe { std::mem::zeroed() }
    }
}
