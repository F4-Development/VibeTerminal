//! Статус CI/CD: GitLab (и свои серверы) через `glab`, GitHub через `gh`.
//! Вход — их собственный (`glab auth login`, `gh auth login`): vv только
//! спрашивает и показывает. Команды отвязаны от терминала, как и git.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

use crate::git;
use crate::userenv;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Provider {
    GitLab { host: String },
    GitHub { host: String },
}

impl Provider {
    pub fn cli(&self) -> &'static str {
        match self {
            Provider::GitLab { .. } => "glab",
            Provider::GitHub { .. } => "gh",
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            Provider::GitLab { .. } => "GitLab",
            Provider::GitHub { .. } => "GitHub",
        }
    }

    pub fn host(&self) -> &str {
        match self {
            Provider::GitLab { host } | Provider::GitHub { host } => host,
        }
    }

    /// Вход: vv запускает это в отдельной сессии, там можно войти через браузер.
    pub fn login_command(&self) -> String {
        match self {
            Provider::GitLab { host } => format!("glab auth login --hostname {host} --git-protocol ssh"),
            Provider::GitHub { host } => format!("gh auth login --hostname {host} --git-protocol ssh --web"),
        }
    }

    pub fn install_command(&self) -> String {
        format!("brew install {}", self.cli())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiStatus {
    Running,
    Pending,
    Success,
    Failed,
    Canceled,
    Skipped,
    Manual,
}

impl CiStatus {
    fn from_gitlab(status: &str) -> Self {
        match status {
            "success" => CiStatus::Success,
            "failed" => CiStatus::Failed,
            "running" => CiStatus::Running,
            "canceled" | "canceling" => CiStatus::Canceled,
            "skipped" => CiStatus::Skipped,
            "manual" => CiStatus::Manual,
            _ => CiStatus::Pending, // created, pending, preparing, scheduled, waiting_for_resource
        }
    }

    fn from_github(status: &str, conclusion: &str) -> Self {
        match (status, conclusion) {
            ("completed", "success") => CiStatus::Success,
            ("completed", "failure" | "timed_out" | "startup_failure") => CiStatus::Failed,
            ("completed", "cancelled") => CiStatus::Canceled,
            ("completed", "skipped" | "neutral") => CiStatus::Skipped,
            ("completed", "action_required") => CiStatus::Manual,
            ("in_progress", _) => CiStatus::Running,
            _ => CiStatus::Pending,
        }
    }

    pub fn icon(self) -> &'static str {
        match self {
            CiStatus::Success => "✓",
            CiStatus::Failed => "✕",
            CiStatus::Running => "●",
            CiStatus::Pending => "◌",
            CiStatus::Canceled => "⊘",
            CiStatus::Skipped => "↷",
            CiStatus::Manual => "▶",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            CiStatus::Success => "прошёл",
            CiStatus::Failed => "упал",
            CiStatus::Running => "идёт",
            CiStatus::Pending => "ждёт",
            CiStatus::Canceled => "отменён",
            CiStatus::Skipped => "пропущен",
            CiStatus::Manual => "ждёт запуска",
        }
    }

    /// Ещё идёт — проверяем чаще.
    pub fn active(self) -> bool {
        matches!(self, CiStatus::Running | CiStatus::Pending)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pipeline {
    pub id: String,
    pub status: CiStatus,
    pub url: String,
    /// «#1842» или название workflow.
    pub title: String,
    /// Коммит, на котором запущен.
    pub sha: String,
    /// Задачи, которые сейчас важны: идут, упали или ждут.
    pub current: Vec<String>,
}

impl Pipeline {
    /// «CI: test идёт», «CI: test, lint упали», «CI: build и ещё 3 ждут».
    pub fn summary(&self) -> String {
        let (one, many) = match self.status {
            CiStatus::Running => ("идёт", "идут"),
            CiStatus::Failed => ("упал", "упали"),
            CiStatus::Pending => ("ждёт", "ждут"),
            status => return format!("CI {}", status.label()),
        };
        match self.current.as_slice() {
            [] => format!("CI {one}"),
            [job] => format!("CI: {} {one}", short_name(job)),
            [a, b] => format!("CI: {}, {} {many}", short_name(a), short_name(b)),
            [first, rest @ ..] => format!("CI: {} и ещё {} {many}", short_name(first), rest.len()),
        }
    }
}

fn short_name(name: &str) -> String {
    const MAX: usize = 24;
    match name.char_indices().nth(MAX) {
        Some((cut, _)) => format!("{}…", &name[..cut]),
        None => name.to_string(),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Job {
    pub id: String,
    pub name: String,
    pub stage: String,
    pub status: CiStatus,
    pub seconds: Option<f64>,
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CiState {
    /// Не GitLab и не GitHub — CI не показываем.
    Unsupported,
    NeedCli(Provider),
    NeedLogin(Provider),
    NoPipeline(Provider),
    Pipeline(Provider, Pipeline),
    Error(Provider, String),
}

/// Какой сервер у репозитория — по адресу remote.
pub fn detect(cwd: &Path) -> Option<Provider> {
    let url = git::run(cwd, &["remote", "get-url", "origin"])
        .or_else(|_| {
            let remotes = git::run(cwd, &["remote"])?;
            let first = remotes.lines().next().unwrap_or_default().to_string();
            git::run(cwd, &["remote", "get-url", &first])
        })
        .ok()?;
    let host = remote_host(url.trim())?;
    if host.contains("github") {
        Some(Provider::GitHub { host })
    } else if host.contains("gitlab") {
        Some(Provider::GitLab { host })
    } else {
        None
    }
}

/// `git@host:path`, `ssh://git@host:22/path`, `https://host/path` → `host`.
fn remote_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let rest = rest.split_once('@').map_or(rest, |(_, rest)| rest);
    let host = rest.split([':', '/']).next()?;
    (!host.is_empty()).then(|| host.to_lowercase())
}

/// Состояние CI для ветки.
pub fn state(cwd: &Path, branch: &str) -> CiState {
    let Some(provider) = detect(cwd) else { return CiState::Unsupported };
    if find_cli(provider.cli()).is_none() {
        return CiState::NeedCli(provider);
    }
    let result = match &provider {
        Provider::GitLab { .. } => {
            let endpoint = format!("projects/:fullpath/pipelines?ref={}&per_page=1", percent(branch));
            json(cwd, &provider, &["api", &endpoint]).map(|v| {
                v.as_array().and_then(|list| list.first()).map(|p| Pipeline {
                    id: id_of(&p["id"]),
                    status: CiStatus::from_gitlab(p["status"].as_str().unwrap_or_default()),
                    url: p["web_url"].as_str().unwrap_or_default().to_string(),
                    title: format!("#{}", id_of(&p["iid"]).trim_matches('"')),
                    sha: p["sha"].as_str().unwrap_or_default().to_string(),
                    current: Vec::new(),
                })
            })
        }
        Provider::GitHub { .. } => {
            let fields = "databaseId,status,conclusion,url,displayTitle,workflowName,headSha";
            json(cwd, &provider, &["run", "list", "--branch", branch, "--limit", "1", "--json", fields]).map(|v| {
                v.as_array().and_then(|list| list.first()).map(|r| Pipeline {
                    id: id_of(&r["databaseId"]),
                    status: CiStatus::from_github(
                        r["status"].as_str().unwrap_or_default(),
                        r["conclusion"].as_str().unwrap_or_default(),
                    ),
                    url: r["url"].as_str().unwrap_or_default().to_string(),
                    title: r["workflowName"].as_str().unwrap_or_default().to_string(),
                    sha: r["headSha"].as_str().unwrap_or_default().to_string(),
                    current: Vec::new(),
                })
            })
        }
    };
    match result {
        Ok(Some(mut pipeline)) => {
            pipeline.current = current_jobs(cwd, &provider, &pipeline);
            CiState::Pipeline(provider, pipeline)
        }
        Ok(None) => CiState::NoPipeline(provider),
        Err(CliError::Login) => CiState::NeedLogin(provider),
        // Закрытый проект без входа GitLab показывает как «не найден».
        Err(CliError::Other(_)) if !logged_in(cwd, &provider) => CiState::NeedLogin(provider),
        Err(CliError::Other(text)) => CiState::Error(provider, text),
    }
}

/// Какие задачи назвать в шапке: те, что идут, упали или ждут — смотря
/// в каком состоянии весь пайплайн. Не узнали — без имён.
fn current_jobs(cwd: &Path, provider: &Provider, pipeline: &Pipeline) -> Vec<String> {
    let wanted = match pipeline.status {
        CiStatus::Running | CiStatus::Failed | CiStatus::Pending => pipeline.status,
        _ => return Vec::new(),
    };
    let jobs = jobs(cwd, provider, pipeline).unwrap_or_default();
    jobs.into_iter().filter(|job| job.status == wanted).map(|job| job.name).collect()
}

/// Есть ли вход на этот сервер.
fn logged_in(cwd: &Path, provider: &Provider) -> bool {
    run(cwd, provider, &["auth", "status", "--hostname", provider.host()]).is_ok()
}

/// Задачи пайплайна по порядку запуска.
pub fn jobs(cwd: &Path, provider: &Provider, pipeline: &Pipeline) -> Result<Vec<Job>, String> {
    let to_text = |e: CliError| match e {
        CliError::Login => format!("Нужно войти в {}", provider.name()),
        CliError::Other(text) => text,
    };
    match provider {
        Provider::GitLab { .. } => {
            let endpoint = format!("projects/:fullpath/pipelines/{}/jobs?per_page=100", pipeline.id);
            let value = json(cwd, provider, &["api", &endpoint]).map_err(to_text)?;
            let mut jobs: Vec<Job> = value
                .as_array()
                .into_iter()
                .flatten()
                .map(|j| Job {
                    id: id_of(&j["id"]),
                    name: j["name"].as_str().unwrap_or_default().to_string(),
                    stage: j["stage"].as_str().unwrap_or_default().to_string(),
                    status: CiStatus::from_gitlab(j["status"].as_str().unwrap_or_default()),
                    seconds: j["duration"].as_f64(),
                    url: j["web_url"].as_str().unwrap_or_default().to_string(),
                })
                .collect();
            jobs.sort_by_key(|j| j.id.parse::<u64>().unwrap_or(0));
            Ok(jobs)
        }
        Provider::GitHub { .. } => {
            let value = json(cwd, provider, &["run", "view", &pipeline.id, "--json", "jobs"]).map_err(to_text)?;
            Ok(value["jobs"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|j| Job {
                    id: id_of(&j["databaseId"]),
                    name: j["name"].as_str().unwrap_or_default().to_string(),
                    stage: String::new(),
                    status: CiStatus::from_github(
                        j["status"].as_str().unwrap_or_default(),
                        j["conclusion"].as_str().unwrap_or_default(),
                    ),
                    seconds: None,
                    url: j["url"].as_str().unwrap_or_default().to_string(),
                })
                .collect())
        }
    }
}

/// Перезапустить упавшие задачи.
pub fn retry(cwd: &Path, provider: &Provider, pipeline: &Pipeline) -> Result<(), String> {
    let args: Vec<String> = match provider {
        Provider::GitLab { .. } => {
            vec!["api".into(), "-X".into(), "POST".into(), format!("projects/:fullpath/pipelines/{}/retry", pipeline.id)]
        }
        Provider::GitHub { .. } => vec!["run".into(), "rerun".into(), pipeline.id.clone(), "--failed".into()],
    };
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    run(cwd, provider, &args).map(|_| ()).map_err(|e| match e {
        CliError::Login => format!("Нужно войти в {}", provider.name()),
        CliError::Other(text) => text,
    })
}

/// Логи упавших задач в файл — для Claude. Возвращает путь к файлу.
pub fn failed_log(cwd: &Path, provider: &Provider, pipeline: &Pipeline, jobs: &[Job]) -> Result<PathBuf, String> {
    const TAIL: usize = 300;
    let mut text = String::new();
    match provider {
        Provider::GitLab { .. } => {
            for job in jobs.iter().filter(|j| j.status == CiStatus::Failed) {
                let endpoint = format!("projects/:fullpath/jobs/{}/trace", job.id);
                let trace = run(cwd, provider, &["api", &endpoint]).unwrap_or_default();
                text.push_str(&format!("===== Задача «{}» ({}) =====\n", job.name, job.url));
                text.push_str(&tail(&clean_log(&trace), TAIL));
                text.push_str("\n\n");
            }
        }
        Provider::GitHub { .. } => {
            let log = run(cwd, provider, &["run", "view", &pipeline.id, "--log-failed"]).unwrap_or_default();
            text.push_str(&tail(&clean_log(&log), TAIL * 2));
        }
    }
    if text.trim().is_empty() {
        return Err("Не получилось скачать логи упавших задач.".into());
    }
    let dir = std::env::temp_dir().join("vibeterminal-ci");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let folder = cwd.file_name().map_or("project".into(), |n| n.to_string_lossy().into_owned());
    let path = dir.join(format!("{folder}-{}.log", pipeline.id));
    std::fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(path)
}

/// Что сказать Claude, когда CI упал.
pub fn fix_prompt(pipeline: &Pipeline, log: &Path) -> String {
    format!(
        "CI упал: {}. Логи упавших задач — в файле {}. Разберись, в чём причина, почини и проверь локально. \
         Не пушь, пока я не посмотрю.",
        pipeline.url,
        log.display()
    )
}

enum CliError {
    Login,
    Other(String),
}

fn run(cwd: &Path, provider: &Provider, args: &[&str]) -> Result<String, CliError> {
    let cli = find_cli(provider.cli()).ok_or_else(|| CliError::Other(format!("{} не найден", provider.cli())))?;
    let mut command = Command::new(cli);
    command
        .args(args)
        .current_dir(cwd)
        .env("NO_COLOR", "1")
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_NO_UPDATE_NOTIFIER", "1")
        .env("GLAB_CHECK_UPDATE", "false")
        .env("GLAB_NO_PROMPT", "true")
        .stdin(Stdio::null());
    // SAFETY: setsid безопасен между fork и exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let output = command.output().map_err(|e| CliError::Other(e.to_string()))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let text = String::from_utf8_lossy(&output.stderr).trim().to_string();
    let lower = text.to_lowercase();
    if lower.contains("auth login") || lower.contains("401") || lower.contains("unauthorized") || lower.contains("not logged") {
        Err(CliError::Login)
    } else {
        Err(CliError::Other(text.lines().take(4).collect::<Vec<_>>().join("\n")))
    }
}

fn json(cwd: &Path, provider: &Provider, args: &[&str]) -> Result<Value, CliError> {
    let text = run(cwd, provider, args)?;
    serde_json::from_str(&text).map_err(|e| CliError::Other(format!("непонятный ответ {}: {e}", provider.cli())))
}

/// `glab`/`gh` в PATH или там, куда их ставит Homebrew.
fn find_cli(name: &str) -> Option<PathBuf> {
    userenv::find_in_path(name).or_else(|| {
        ["/opt/homebrew/bin", "/usr/local/bin"].iter().map(|dir| Path::new(dir).join(name)).find(|p| p.exists())
    })
}

fn id_of(value: &Value) -> String {
    match value {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    }
}

/// Имя ветки для адреса: `feat/auth` → `feat%2Fauth`.
fn percent(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Без цветовых кодов и служебных пометок GitLab.
fn clean_log(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else if c != '\r' {
            out.push(c);
        }
    }
    out.lines()
        .filter(|l| !l.starts_with("section_start:") && !l.starts_with("section_end:"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

/// «1 мин 12 с».
pub fn duration(seconds: f64) -> String {
    let total = seconds.round() as u64;
    match (total / 60, total % 60) {
        (0, s) => format!("{s} с"),
        (m, 0) => format!("{m} мин"),
        (m, s) => format!("{m} мин {s} с"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_hosts() {
        assert_eq!(remote_host("git@gitlab.example.org:team/app.git").as_deref(), Some("gitlab.example.org"));
        assert_eq!(remote_host("ssh://git@gitlab.example.com:2222/x/y.git").as_deref(), Some("gitlab.example.com"));
        assert_eq!(remote_host("https://github.com/me/repo").as_deref(), Some("github.com"));
    }

    #[test]
    fn names_current_jobs() {
        let pipeline = |status, current: &[&str]| Pipeline {
            id: "1".into(),
            status,
            url: String::new(),
            title: String::new(),
            sha: String::new(),
            current: current.iter().map(|s| s.to_string()).collect(),
        };
        assert_eq!(pipeline(CiStatus::Running, &["test"]).summary(), "CI: test идёт");
        assert_eq!(pipeline(CiStatus::Failed, &["test", "lint"]).summary(), "CI: test, lint упали");
        assert_eq!(pipeline(CiStatus::Pending, &["a", "b", "c", "d"]).summary(), "CI: a и ещё 3 ждут");
        assert_eq!(pipeline(CiStatus::Failed, &[]).summary(), "CI упал");
        assert_eq!(pipeline(CiStatus::Success, &[]).summary(), "CI прошёл");
        assert_eq!(pipeline(CiStatus::Running, &["build:docker-image:production-eu"]).summary(), "CI: build:docker-image:produ… идёт");
    }

    #[test]
    fn statuses() {
        assert_eq!(CiStatus::from_gitlab("success"), CiStatus::Success);
        assert_eq!(CiStatus::from_gitlab("created"), CiStatus::Pending);
        assert_eq!(CiStatus::from_github("completed", "failure"), CiStatus::Failed);
        assert_eq!(CiStatus::from_github("in_progress", ""), CiStatus::Running);
        assert!(CiStatus::Running.active() && !CiStatus::Failed.active());
    }

    #[test]
    fn encodes_branch_names() {
        assert_eq!(percent("feat/auth-2"), "feat%2Fauth-2");
        assert_eq!(percent("фича"), "%D1%84%D0%B8%D1%87%D0%B0");
    }

    #[test]
    fn cleans_logs() {
        let raw = "section_start:123:build\r\n\u{1b}[32;1m$ make\u{1b}[0m\nerror: boom\nsection_end:124:build\n";
        assert_eq!(clean_log(raw), "$ make\nerror: boom");
        assert_eq!(tail("a\nb\nc", 2), "b\nc");
    }

    #[test]
    fn formats_duration() {
        assert_eq!(duration(72.4), "1 мин 12 с");
        assert_eq!(duration(40.0), "40 с");
        assert_eq!(duration(120.0), "2 мин");
    }
}
