//! Настройки vv: `~/.config/vibeterminal/vv.json`. Их пишет окно настроек
//! VibeTerminal (⌘,), можно и руками. Читаются при каждом использовании —
//! изменения действуют сразу, без перезапуска.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

const VIBETERMINAL_BUNDLE_ID: &str = "com.vibeterminal.app";

#[derive(Deserialize, Serialize)]
#[serde(default)]
pub struct Settings {
    /// Где искать проекты для «Новая сессия».
    pub projects_dirs: Vec<String>,
    /// Режим разрешений новых сессий: `default`, `acceptEdits`, `auto`, `plan`.
    /// Пусто — как в настройках самого Claude.
    pub permission_mode: String,
    /// Модель новых сессий: `opus`, `sonnet`, `haiku`. Пусто — как у Claude.
    pub model: String,
    /// Любые дополнительные флаги `claude`.
    pub extra_args: String,
    /// Звуковой сигнал, когда Claude просит разрешение.
    pub permission_sound: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            projects_dirs: vec!["~/Projects".into()],
            permission_mode: String::new(),
            model: String::new(),
            extra_args: String::new(),
            permission_sound: true,
        }
    }
}

impl Settings {
    pub fn path(home: &Path) -> PathBuf {
        home.join(".config/vibeterminal/vv.json")
    }

    /// Нет файла или он сломан — настройки по умолчанию, vv не падает.
    pub fn load(home: &Path) -> Self {
        std::fs::read_to_string(Self::path(home))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    /// Открыть настройки. В VibeTerminal — его окно настроек (через
    /// AppleScript), в другом терминале — файл настроек в редакторе.
    pub fn open(home: &Path) -> std::io::Result<()> {
        if std::env::var("__CFBundleIdentifier").as_deref() == Ok(VIBETERMINAL_BUNDLE_ID) {
            let script = format!(
                "tell application id \"{VIBETERMINAL_BUNDLE_ID}\" to perform action \"open_config\" \
                 on focused terminal of selected tab of front window"
            );
            Command::new("osascript").args(["-e", &script]).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
            return Ok(());
        }
        let path = Self::path(home);
        if !path.exists() {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let json = serde_json::to_string_pretty(&Settings::default()).unwrap_or_default();
            std::fs::write(&path, json + "\n")?;
        }
        Command::new("open").arg("-t").arg(&path).spawn()?;
        Ok(())
    }

    pub fn projects_dirs(&self, home: &Path) -> Vec<PathBuf> {
        self.projects_dirs.iter().map(|dir| expand_home(dir, home)).collect()
    }

    /// Флаги `claude` для новой сессии.
    pub fn claude_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        if !self.permission_mode.trim().is_empty() {
            args.extend(["--permission-mode".to_string(), self.permission_mode.trim().to_string()]);
        }
        if !self.model.trim().is_empty() {
            args.extend(["--model".to_string(), self.model.trim().to_string()]);
        }
        args.extend(self.extra_args.split_whitespace().map(str::to_string));
        args
    }
}

fn expand_home(path: &str, home: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Some(rest) => home.join(rest.trim_start_matches('/')),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_fields_take_defaults() {
        let settings: Settings = serde_json::from_str(r#"{"model": "sonnet"}"#).unwrap();
        assert_eq!(settings.model, "sonnet");
        assert!(settings.permission_sound);
        assert_eq!(settings.projects_dirs, vec!["~/Projects"]);
    }

    #[test]
    fn builds_claude_args() {
        let settings = Settings {
            permission_mode: "acceptEdits".into(),
            model: "opus".into(),
            extra_args: "--verbose  --add-dir /x".into(),
            ..Settings::default()
        };
        assert_eq!(
            settings.claude_args(),
            ["--permission-mode", "acceptEdits", "--model", "opus", "--verbose", "--add-dir", "/x"]
        );
        assert!(Settings::default().claude_args().is_empty());
    }

    #[test]
    fn expands_tilde() {
        let home = Path::new("/Users/me");
        let settings = Settings { projects_dirs: vec!["~/Projects".into(), "/code".into()], ..Settings::default() };
        assert_eq!(settings.projects_dirs(home), [PathBuf::from("/Users/me/Projects"), PathBuf::from("/code")]);
    }
}
