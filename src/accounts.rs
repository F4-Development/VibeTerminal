//! Несколько аккаунтов Claude: кончились лимиты на одном — переключился на
//! другой, и сессии продолжают те же диалоги.
//!
//! Аккаунт — это папка Claude (`CLAUDE_CONFIG_DIR`): у каждой свой вход.
//! Основной — обычный `~/.claude`. В папке дополнительного аккаунта свои
//! только вход и `.claude.json`, остальное — ссылки на `~/.claude`: те же
//! настройки, скиллы, плагины и, главное, те же диалоги, поэтому
//! `claude --resume` находит их под любым аккаунтом.
//!
//! Список — `~/.config/vibeterminal/accounts.json`. Отдельно от `vv.json`:
//! окно настроек VibeTerminal пересохраняет тот целиком, со своими полями.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::session::PARENT_CLAUDE_VARS;

/// В папке Claude это своё у каждого аккаунта: вход, служебное, запущенное.
/// Остальное — общее.
const OWN: [&str; 13] = [
    ".claude.json",
    ".credentials.json",
    ".last-cleanup",
    "backups",
    "cache",
    "daemon",
    "daemon.log",
    "jobs",
    "session-env",
    "sessions",
    "shell-snapshots",
    "state",
    "telemetry",
];
/// Из `~/.claude.json` в новый аккаунт: пройденное знакомство, доверенные
/// папки, MCP. Вход и всё, что Claude узнал про подписку, — нет.
const CARRY_CONFIG: [&str; 7] =
    ["hasCompletedOnboarding", "lastOnboardingVersion", "mcpServers", "projects", "githubRepoPaths", "autoUpdates", "installMethod"];

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Account {
    /// Папка Claude; пусто — основной `~/.claude`. Она же — ключ аккаунта.
    pub dir: String,
    /// Почта и подписка — какими их назвал `claude auth status`.
    pub email: String,
    pub plan: String,
}

impl Account {
    /// «me@mail.ru», пока не вошли — «не выполнен вход».
    pub fn label(&self) -> String {
        match (self.email.is_empty(), self.dir.is_empty()) {
            (false, _) => self.email.clone(),
            (true, true) => "Основной".into(),
            (true, false) => "не выполнен вход".into(),
        }
    }

    /// Для строки внизу: почта до `@`.
    pub fn short(&self) -> String {
        self.label().split('@').next().unwrap_or_default().to_string()
    }

    /// `Some` — дополнительный аккаунт, Claude запускать с этой папкой.
    pub fn config_dir(&self) -> Option<&Path> {
        (!self.dir.is_empty()).then(|| Path::new(&self.dir))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Accounts {
    /// Папка аккаунта, на котором работаем; пусто — основной.
    pub current: String,
    /// Первый — всегда основной.
    pub list: Vec<Account>,
}

impl Accounts {
    fn path(home: &Path) -> PathBuf {
        home.join(".config/vibeterminal/accounts.json")
    }

    /// Нет файла или он сломан — один основной аккаунт.
    pub fn load(home: &Path) -> Self {
        let mut accounts: Self = std::fs::read_to_string(Self::path(home))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default();
        if !accounts.list.first().is_some_and(|a| a.dir.is_empty()) {
            accounts.list.retain(|a| !a.dir.is_empty());
            accounts.list.insert(0, Account::default());
        }
        if !accounts.list.iter().any(|a| a.dir == accounts.current) {
            accounts.current.clear();
        }
        accounts
    }

    pub fn save(&self, home: &Path) -> std::io::Result<()> {
        let path = Self::path(home);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(path, json + "\n")
    }

    pub fn current(&self) -> &Account {
        self.list.iter().find(|a| a.dir == self.current).unwrap_or(&self.list[0])
    }

    pub fn get(&self, dir: &str) -> Option<&Account> {
        self.list.iter().find(|a| a.dir == dir)
    }

    /// Запомнить, кем назвался аккаунт. `true` — что-то поменялось.
    pub fn note_status(&mut self, dir: &str, status: &Status) -> bool {
        let Some(account) = self.list.iter_mut().find(|a| a.dir == dir) else { return false };
        let changed = account.email != status.email || account.plan != status.plan;
        account.email.clone_from(&status.email);
        account.plan.clone_from(&status.plan);
        changed
    }

    /// Новый аккаунт: папка с общими ссылками, без входа. Возвращает его папку.
    pub fn add(&mut self, home: &Path) -> std::io::Result<String> {
        let root = home.join(".config/vibeterminal/claude-accounts");
        let dir = (2..).map(|n| root.join(n.to_string())).find(|dir| !dir.exists()).expect("номера не кончаются");
        prepare(home, &dir)?;
        let dir = dir.to_string_lossy().into_owned();
        self.list.push(Account { dir: dir.clone(), ..Account::default() });
        Ok(dir)
    }

    /// Убрать дополнительный аккаунт из списка. Папку — `forget`.
    pub fn remove(&mut self, dir: &str) {
        if dir.is_empty() {
            return;
        }
        self.list.retain(|a| a.dir != dir);
        if self.current == dir {
            self.current.clear();
        }
    }
}

/// Выйти из аккаунта и стереть его папку. Без выхода вход остался бы в
/// Связке ключей, и следующий аккаунт в той же папке вошёл бы сам. Долго —
/// из фонового потока.
pub fn forget(dir: &Path) {
    let mut command = Command::new("claude");
    command.args(["auth", "logout"]).env("CLAUDE_CONFIG_DIR", dir);
    command.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    for var in PARENT_CLAUDE_VARS {
        command.env_remove(var);
    }
    let _ = command.status();
    let _ = delete(dir);
}

/// Стереть папку аккаунта. Общее не трогается: там только ссылки.
fn delete(dir: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(dir) {
        Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
        _ => Ok(()),
    }
}

/// Папка Claude основного аккаунта: своя `CLAUDE_CONFIG_DIR`, если задана.
fn main_dir(home: &Path) -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR").filter(|_| !cfg!(test)).map_or_else(|| home.join(".claude"), PathBuf::from)
}

/// Довести папку аккаунта до дела: ссылки на всё общее из `~/.claude`
/// (новое там появляется — при следующем переключении появится и тут),
/// а при первом разе — `.claude.json` без чужого входа.
pub fn prepare(home: &Path, dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let main = main_dir(home);
    for entry in std::fs::read_dir(&main)?.flatten() {
        let name = entry.file_name();
        let shared = name.to_str().is_some_and(|n| !OWN.contains(&n) && !n.ends_with(".lock"));
        let link = dir.join(&name);
        if shared && link.symlink_metadata().is_err() {
            std::os::unix::fs::symlink(entry.path(), link)?;
        }
    }
    let config = dir.join(".claude.json");
    if !config.exists() {
        // Со своей CLAUDE_CONFIG_DIR Claude держит `.claude.json` внутри неё.
        let source = if main == home.join(".claude") { home.join(".claude.json") } else { main.join(".claude.json") };
        let carried = carry_config(&std::fs::read_to_string(source).unwrap_or_default());
        std::fs::write(config, carried)?;
    }
    Ok(())
}

/// `.claude.json` для нового аккаунта: только то, что не про вход.
fn carry_config(text: &str) -> String {
    let main: Value = serde_json::from_str(text).unwrap_or_default();
    let carried: serde_json::Map<String, Value> = CARRY_CONFIG
        .iter()
        .filter_map(|key| Some((key.to_string(), main.get(*key)?.clone())))
        .collect();
    serde_json::to_string_pretty(&carried).unwrap_or_default() + "\n"
}

/// Команда для сессии входа в аккаунт с этой папкой.
pub fn login_command(dir: &str) -> String {
    format!("CLAUDE_CONFIG_DIR='{}' claude auth login", dir.replace('\'', r"'\''"))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Status {
    /// Пусто — не вошли.
    pub email: String,
    pub plan: String,
}

/// Кто вошёл в аккаунт: `claude auth status`. Долго — из фонового потока.
pub fn status(dir: Option<&Path>) -> Option<Status> {
    let mut command = Command::new("claude");
    command.args(["auth", "status", "--json"]).stdin(Stdio::null()).stderr(Stdio::null());
    if let Some(dir) = dir {
        command.env("CLAUDE_CONFIG_DIR", dir);
    }
    for var in PARENT_CLAUDE_VARS {
        command.env_remove(var);
    }
    let output = command.output().ok()?;
    parse_status(&String::from_utf8_lossy(&output.stdout))
}

fn parse_status(text: &str) -> Option<Status> {
    let json: Value = serde_json::from_str(text).ok()?;
    if json["loggedIn"] != Value::Bool(true) {
        return Some(Status::default());
    }
    let plan = json["subscriptionType"].as_str().unwrap_or_default();
    let mut chars = plan.chars();
    let plan = chars.next().map(|c| c.to_uppercase().chain(chars).collect()).unwrap_or_default();
    Some(Status { email: json["email"].as_str().unwrap_or_default().to_string(), plan })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_account_always_first() {
        let home = std::env::temp_dir().join(format!("vv-accounts-{}", std::process::id()));
        let accounts = Accounts::load(&home);
        assert_eq!(accounts.list, [Account::default()]);
        assert_eq!(accounts.current().label(), "Основной");

        let accounts = Accounts { current: "/gone".into(), list: vec![Account { dir: "/x".into(), ..Account::default() }] };
        accounts.save(&home).unwrap();
        let accounts = Accounts::load(&home);
        assert_eq!(accounts.list.len(), 2);
        assert!(accounts.list[0].dir.is_empty());
        assert!(accounts.current.is_empty(), "аккаунта нет в списке — работаем на основном");
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn new_account_shares_all_but_login() {
        let home = std::env::temp_dir().join(format!("vv-accounts-prepare-{}", std::process::id()));
        let main = home.join(".claude");
        std::fs::create_dir_all(main.join("projects")).unwrap();
        std::fs::create_dir_all(main.join("daemon")).unwrap();
        std::fs::write(main.join("settings.json"), "{}").unwrap();
        std::fs::write(main.join("history.jsonl.lock"), "").unwrap();
        std::fs::write(
            home.join(".claude.json"),
            r#"{"oauthAccount": {"emailAddress": "a@b"}, "projects": {"/p": {}}, "hasCompletedOnboarding": true}"#,
        )
        .unwrap();

        let mut accounts = Accounts::load(&home);
        let dir = PathBuf::from(accounts.add(&home).unwrap());
        assert!(dir.join("projects").symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(dir.join("settings.json")).unwrap(), main.join("settings.json"));
        assert!(!dir.join("daemon").exists());
        assert!(!dir.join("history.jsonl.lock").exists());
        let config: Value = serde_json::from_str(&std::fs::read_to_string(dir.join(".claude.json")).unwrap()).unwrap();
        assert!(config.get("oauthAccount").is_none());
        assert_eq!(config["hasCompletedOnboarding"], true);
        assert!(config["projects"]["/p"].is_object());

        // Ссылки убираются вместе с папкой, а сами диалоги остаются.
        accounts.remove(&dir.to_string_lossy());
        assert_eq!(accounts.list.len(), 1);
        delete(&dir).unwrap();
        assert!(!dir.exists());
        assert!(main.join("projects").is_dir());
        std::fs::remove_dir_all(&home).unwrap();
    }

    #[test]
    fn reads_auth_status() {
        let text = r#"{"loggedIn": true, "email": "me@x.io", "subscriptionType": "max"}"#;
        assert_eq!(parse_status(text), Some(Status { email: "me@x.io".into(), plan: "Max".into() }));
        assert_eq!(parse_status(r#"{"loggedIn": false}"#), Some(Status::default()));
        assert_eq!(parse_status("oops"), None);
    }

    #[test]
    fn quotes_login_dir() {
        assert_eq!(login_command("/a/b"), "CLAUDE_CONFIG_DIR='/a/b' claude auth login");
    }
}
