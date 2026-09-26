//! Связь с Claude Code через его хуки.
//!
//! Каждой сессии vv подключает хук `PermissionRequest` через `claude --settings`.
//! Claude вызывает `vv hook permission`, тот по сокету этого окна vv передаёт
//! запрос и ждёт ответа из vv. Claude при этом показывает и свой диалог и
//! хук не закрывает, если ответили там, — поэтому vv ещё слушает события
//! «инструмент выполнился / отказано / Claude закончил» и по ним убирает
//! запрос. Если окна vv нет, хук молча ничего не отвечает.

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Sender};
use std::thread;
use std::time::Instant;

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::app::Event;
use crate::session::SessionId;
use crate::status;

/// Сколько Claude ждёт ответа на запрос разрешения: сутки — ты мог уйти.
const PERMISSION_TIMEOUT_SECS: u64 = 24 * 60 * 60;
const DENY_MESSAGE: &str = "Пользователь отклонил это действие в VibeTerminal.";

pub const ENV_SOCKET: &str = "VV_SOCK";
pub const ENV_SESSION: &str = "VV_SESSION";

static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);

/// Запрос разрешения от Claude, который ждёт ответа.
pub struct PermissionRequest {
    pub id: u64,
    /// Какой вызов инструмента ждёт разрешения — по нему узнаём, что он уже прошёл.
    pub tool_use_id: Option<String>,
    pub tool: String,
    pub input: Value,
    /// Когда пришёл: кто дольше ждёт — первым в очереди.
    pub at: Instant,
    reply: Sender<Reply>,
}

enum Reply {
    Answer(String),
    /// Хук закрылся сам: ответили в окне Claude или Claude закрыли.
    Gone,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    Allow,
    Deny,
    /// Пусть Claude покажет свой диалог со всеми вариантами.
    AsUsual,
}

/// Инструменты, где Claude спрашивает на своей карточке: выбрать вариант или
/// утвердить план. «Разрешить / Отклонить» тут не подходят — отвечают в окне
/// сессии (Claude сам помечает их как requiresUserInteraction).
const ANSWERED_IN_SESSION: [&str; 2] = ["AskUserQuestion", "ExitPlanMode"];

impl PermissionRequest {
    pub fn answer(self, decision: Decision) {
        let _ = self.reply.send(Reply::Answer(decision_json(decision)));
    }

    /// Вопрос или план, а не просьба что-то разрешить.
    pub fn is_question(&self) -> bool {
        ANSWERED_IN_SESSION.contains(&self.tool.as_str())
    }
}

fn decision_json(decision: Decision) -> String {
    let decision = match decision {
        Decision::Allow => json!({ "behavior": "allow" }),
        Decision::Deny => json!({ "behavior": "deny", "message": DENY_MESSAGE }),
        Decision::AsUsual => return String::new(),
    };
    json!({ "hookSpecificOutput": { "hookEventName": "PermissionRequest", "decision": decision } }).to_string()
}

/// События Claude, о которых vv просто узнаёт: Claude их не ждёт.
const NOTIFY_EVENTS: [&str; 7] = [
    "UserPromptSubmit",
    "PreToolUse",
    "PostToolUse",
    "PostToolUseFailure",
    "PermissionDenied",
    "Stop",
    "StopFailure",
];

/// Что Claude сообщил о своих делах.
#[derive(Default)]
pub struct HookEvent {
    pub name: String,
    pub tool_use_id: Option<String>,
    pub tool: String,
    pub input: Value,
    /// Последний ответ Claude (Stop, StopFailure).
    pub message: String,
    /// Почему ход оборвался (StopFailure): `rate_limit`, `overloaded`…
    pub error: String,
    /// Когда запустился хук, мс от начала эпохи. Хуки асинхронные и
    /// приходят не по порядку — по этому времени старые пропускаем.
    pub at: u64,
}

/// Настройки для `claude --settings`: наши хуки только для этой сессии,
/// твой `~/.claude/settings.json` не трогаем.
pub fn settings_json(vv_exe: &Path) -> String {
    let exe = vv_exe.to_string_lossy();
    let permission = json!({
        "type": "command",
        "command": exe,
        "args": ["hook", "permission"],
        "timeout": PERMISSION_TIMEOUT_SECS,
    });
    let notify = json!({ "type": "command", "command": exe, "args": ["hook", "event"], "async": true, "timeout": 10 });
    let mut hooks = serde_json::Map::new();
    hooks.insert("PermissionRequest".into(), json!([{ "matcher": "*", "hooks": [permission] }]));
    for event in NOTIFY_EVENTS {
        hooks.insert(event.into(), json!([{ "matcher": "*", "hooks": [notify.clone()] }]));
    }
    json!({ "hooks": hooks }).to_string()
}

/// Сокет окна vv. Убирается, когда окно закрывается.
pub struct HookServer {
    pub path: PathBuf,
}

impl HookServer {
    pub fn start(dir: &Path, tx: Sender<Event>) -> Result<Self> {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        let path = dir.join(format!("{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).with_context(|| format!("не открылся сокет {}", path.display()))?;
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let tx = tx.clone();
                thread::spawn(move || serve(stream, tx));
            }
        });
        Ok(Self { path })
    }
}

impl Drop for HookServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Одно обращение `vv hook`: строка JSON туда, ответ обратно. Пока ждём
/// ответа, следим за соединением: Claude закрывает хук, если на запрос
/// ответили прямо в его окне.
fn serve(stream: UnixStream, tx: Sender<Event>) {
    let mut line = String::new();
    if BufReader::new(&stream).read_line(&mut line).is_err() {
        return;
    }
    let Ok(message) = serde_json::from_str::<Value>(&line) else { return };
    let Some(session) = message["session"].as_u64() else { return };
    let payload = &message["payload"];
    let tool_use_id = payload["tool_use_id"].as_str().map(str::to_string);
    match message["event"].as_str() {
        Some("permission") => {}
        // Клик по уведомлению в VibeTerminal.
        Some("open") => {
            let _ = tx.send(Event::OpenSession(session as SessionId));
            return;
        }
        Some("event") => {
            let text = |key: &str| payload[key].as_str().unwrap_or_default().to_string();
            let event = HookEvent {
                name: text("hook_event_name"),
                tool_use_id,
                tool: text("tool_name"),
                input: payload["tool_input"].clone(),
                message: text("last_assistant_message"),
                error: text("error"),
                at: message["at"].as_u64().unwrap_or_else(status::now_ms),
            };
            let _ = tx.send(Event::Hook(session as SessionId, event));
            return;
        }
        _ => return,
    }
    let Ok(watch) = stream.try_clone() else { return };

    let (reply, answer) = mpsc::channel();
    let gone = reply.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 64];
        while matches!((&watch).read(&mut buf), Ok(n) if n > 0) {}
        let _ = gone.send(Reply::Gone);
    });

    let id = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
    let request = PermissionRequest {
        id,
        tool_use_id,
        tool: payload["tool_name"].as_str().unwrap_or("?").to_string(),
        input: payload["tool_input"].clone(),
        at: Instant::now(),
        reply,
    };
    if tx.send(Event::Permission(session as SessionId, request)).is_err() {
        return;
    }
    match answer.recv() {
        Ok(Reply::Answer(response)) => {
            let _ = (&stream).write_all(response.as_bytes());
        }
        _ => {
            let _ = tx.send(Event::PermissionGone(session as SessionId, id));
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
}

/// `vv hook <событие>`: зовёт Claude Code. Никогда не ломает Claude —
/// при любой ошибке молча выходит без ответа.
pub fn run_hook(event: &str) {
    let _ = try_run_hook(event);
}

fn try_run_hook(event: &str) -> Result<()> {
    let socket = std::env::var(ENV_SOCKET)?;
    let session: u64 = std::env::var(ENV_SESSION)?.parse()?;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;
    let payload: Value = serde_json::from_str(&input)?;

    let mut stream = UnixStream::connect(socket)?;
    let message = json!({ "session": session, "event": event, "at": status::now_ms(), "payload": payload });
    // Соединение держим открытым до ответа: если Claude убьёт хук, vv это заметит.
    stream.write_all(format!("{message}\n").as_bytes())?;
    if event != "permission" {
        return Ok(());
    }
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    print!("{response}");
    Ok(())
}

/// Что просит Claude, по-человечески: «Выполнить команду» и сама команда.
pub fn describe(tool: &str, input: &Value, cwd: &Path) -> (String, String) {
    let text = |key: &str| input[key].as_str().unwrap_or_default().to_string();
    let path = |key: &str| relative(&text(key), cwd);
    let (what, detail) = match tool {
        "Bash" => ("Выполнить команду", format!("$ {}", text("command"))),
        "Edit" | "MultiEdit" => ("Изменить файл", path("file_path")),
        "Write" => ("Записать файл", path("file_path")),
        "NotebookEdit" => ("Изменить блокнот", path("notebook_path")),
        "Read" => ("Прочитать файл", path("file_path")),
        "Glob" | "Grep" => ("Искать в файлах", text("pattern")),
        "WebFetch" => ("Открыть сайт", text("url")),
        "WebSearch" => ("Искать в интернете", text("query")),
        "Task" | "Agent" => ("Запустить помощника", text("description")),
        "Skill" => ("Запустить навык", text("skill")),
        "AskUserQuestion" => ("спрашивает", question(input)),
        "ExitPlanMode" => ("предлагает план", plan_title(input)),
        _ if tool.starts_with("mcp__") => {
            let mut parts = tool.splitn(3, "__").skip(1);
            let server = parts.next().unwrap_or_default();
            let name = parts.next().unwrap_or_default();
            return (format!("Инструмент {server}"), name.to_string());
        }
        _ => return (tool.to_string(), first_text(input)),
    };
    (what.to_string(), detail)
}

/// Что Claude делает, коротко — для второй строки карточки.
pub fn action(tool: &str, input: &Value, cwd: &Path) -> String {
    let text = |key: &str| input[key].as_str().unwrap_or_default().lines().next().unwrap_or_default().to_string();
    let path = |key: &str| relative(&text(key), cwd);
    match tool {
        "Bash" => format!("$ {}", text("command")),
        "Read" => format!("Читает {}", path("file_path")),
        "Edit" | "MultiEdit" => format!("Правит {}", path("file_path")),
        "Write" => format!("Пишет {}", path("file_path")),
        "NotebookEdit" => format!("Правит {}", path("notebook_path")),
        "Glob" | "Grep" => format!("Ищет {}", text("pattern")),
        "WebFetch" => format!("Открывает {}", domain(&text("url"))),
        "WebSearch" => format!("Ищет в сети: {}", text("query")),
        "Task" | "Agent" => format!("Помощник: {}", text("description")),
        "Skill" => format!("Навык {}", text("skill")),
        "AskUserQuestion" => format!("? {}", question(input)),
        "ExitPlanMode" => format!("План: {}", plan_title(input)),
        "TodoWrite" => input["todos"]
            .as_array()
            .and_then(|todos| todos.iter().find(|t| t["status"] == "in_progress"))
            .and_then(|t| t["activeForm"].as_str())
            .map_or_else(|| "Обновляет список задач".to_string(), str::to_string),
        _ if tool.starts_with("mcp__") => {
            let mut parts = tool.splitn(3, "__").skip(1);
            format!("{}:{}", parts.next().unwrap_or_default(), parts.next().unwrap_or_default())
        }
        _ => tool.to_string(),
    }
}

/// Путь от корня проекта, если файл внутри него.
fn relative(raw: &str, cwd: &Path) -> String {
    Path::new(raw).strip_prefix(cwd).map_or_else(|_| raw.to_string(), |p| p.display().to_string())
}

/// `https://docs.rs/serde/` → `docs.rs`.
fn domain(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    rest.split('/').next().unwrap_or_default().to_string()
}

/// Первый вопрос; если их несколько — сколько всего.
fn question(input: &Value) -> String {
    let questions = input["questions"].as_array().map(Vec::as_slice).unwrap_or_default();
    let first = questions.first().and_then(|q| q["question"].as_str()).unwrap_or_default();
    match questions.len() {
        0 | 1 => first.to_string(),
        n => format!("{first} (вопросов: {n})"),
    }
}

/// Первая непустая строка плана без `#`, иначе имя файла с планом.
fn plan_title(input: &Value) -> String {
    let plan = input["plan"].as_str().unwrap_or_default();
    plan.lines()
        .map(|line| line.trim_start_matches('#').trim())
        .find(|line| !line.is_empty())
        .map(str::to_string)
        .or_else(|| {
            let path = Path::new(input["planFilePath"].as_str()?);
            Some(path.file_name()?.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

/// Незнакомый инструмент: первое текстовое поле, а не весь JSON.
fn first_text(input: &Value) -> String {
    input.as_object().and_then(|fields| fields.values().find_map(Value::as_str)).unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decisions_match_claude_schema() {
        let allow: Value = serde_json::from_str(&decision_json(Decision::Allow)).unwrap();
        assert_eq!(allow["hookSpecificOutput"]["hookEventName"], "PermissionRequest");
        assert_eq!(allow["hookSpecificOutput"]["decision"]["behavior"], "allow");
        let deny: Value = serde_json::from_str(&decision_json(Decision::Deny)).unwrap();
        assert_eq!(deny["hookSpecificOutput"]["decision"]["behavior"], "deny");
        assert_eq!(decision_json(Decision::AsUsual), "");
    }

    #[test]
    fn settings_run_vv_without_shell() {
        let settings: Value = serde_json::from_str(&settings_json(Path::new("/bin/vv"))).unwrap();
        let hook = &settings["hooks"]["PermissionRequest"][0]["hooks"][0];
        assert_eq!(hook["command"], "/bin/vv");
        assert_eq!(hook["args"], json!(["hook", "permission"]));
        let stop = &settings["hooks"]["Stop"][0]["hooks"][0];
        assert_eq!(stop["args"], json!(["hook", "event"]));
        assert_eq!(stop["async"], true);
        assert_eq!(settings["hooks"]["PreToolUse"][0]["hooks"][0]["async"], true);
    }

    #[test]
    fn describes_current_action() {
        let cwd = Path::new("/p");
        assert_eq!(action("Bash", &json!({"command": "npm test\nnpm run lint"}), cwd), "$ npm test");
        assert_eq!(action("Edit", &json!({"file_path": "/p/src/a.rs"}), cwd), "Правит src/a.rs");
        assert_eq!(action("WebFetch", &json!({"url": "https://docs.rs/serde/latest"}), cwd), "Открывает docs.rs");
        assert_eq!(action("mcp__figma__use_figma", &json!({}), cwd), "figma:use_figma");
        let todos = json!({"todos": [{"status": "completed", "activeForm": "A"}, {"status": "in_progress", "activeForm": "Гоняю тесты"}]});
        assert_eq!(action("TodoWrite", &todos, cwd), "Гоняю тесты");
    }

    #[test]
    fn describes_common_tools() {
        let cwd = Path::new("/p");
        assert_eq!(
            describe("Bash", &json!({"command": "npm i"}), cwd),
            ("Выполнить команду".into(), "$ npm i".into())
        );
        assert_eq!(
            describe("Edit", &json!({"file_path": "/p/src/a.ts"}), cwd),
            ("Изменить файл".into(), "src/a.ts".into())
        );
        assert_eq!(describe("mcp__figma__use_figma", &json!({}), cwd), ("Инструмент figma".into(), "use_figma".into()));
    }

    #[test]
    fn describes_questions_and_plans_without_json() {
        let cwd = Path::new("/p");
        let one = json!({"questions": [{"question": "Какой объём?", "header": "Объём", "options": [{"label": "Всё"}]}]});
        assert_eq!(describe("AskUserQuestion", &one, cwd), ("спрашивает".into(), "Какой объём?".into()));
        let two = json!({"questions": [{"question": "Какой объём?"}, {"question": "Как CI?"}]});
        assert_eq!(describe("AskUserQuestion", &two, cwd).1, "Какой объём? (вопросов: 2)");
        let plan = json!({"plan": "\n## План: git в шапке\n\n1. Шапка", "planFilePath": "/tmp/plans/a.md"});
        assert_eq!(describe("ExitPlanMode", &plan, cwd).1, "План: git в шапке");
        assert_eq!(describe("ExitPlanMode", &json!({"planFilePath": "/tmp/plans/a.md"}), cwd).1, "a.md");
        assert_eq!(describe("Unknown", &json!({"n": 1, "name": "x"}), cwd), ("Unknown".into(), "x".into()));
    }

    #[test]
    fn hook_round_trip_through_socket() {
        let dir = std::env::temp_dir().join(format!("vv-test-{}", std::process::id()));
        let (tx, rx) = mpsc::channel();
        let server = HookServer::start(&dir, tx).unwrap();

        let path = server.path.clone();
        let client = thread::spawn(move || {
            let mut stream = UnixStream::connect(path).unwrap();
            let message = json!({"session": 7, "event": "permission", "payload": {"tool_name": "Bash", "tool_input": {"command": "ls"}}});
            stream.write_all(format!("{message}\n").as_bytes()).unwrap();
            let mut response = String::new();
            stream.read_to_string(&mut response).unwrap();
            response
        });

        let Ok(Event::Permission(session, request)) = rx.recv() else { panic!("нет запроса") };
        assert_eq!(session, 7);
        assert_eq!(request.tool, "Bash");
        request.answer(Decision::Allow);
        assert!(client.join().unwrap().contains("\"allow\""));

        // Хук закрылся, не дождавшись ответа, — vv узнаёт об этом.
        let stream = UnixStream::connect(&server.path).unwrap();
        let message = json!({"session": 7, "event": "permission", "payload": {"tool_name": "Bash"}});
        (&stream).write_all(format!("{message}\n").as_bytes()).unwrap();
        let Ok(Event::Permission(_, request)) = rx.recv() else { panic!("нет запроса") };
        drop(stream);
        let Ok(Event::PermissionGone(7, id)) = rx.recv() else { panic!("не заметили закрытие") };
        assert_eq!(id, request.id);
        drop(server);
        let _ = std::fs::remove_dir(&dir);
    }
}
