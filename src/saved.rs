//! Сессии переживают перезапуск. vv помнит открытые сессии (папку, имя,
//! диалог Claude), и следующий vv открывает их снова через `claude --resume`
//! — с той же историей. Claude при этом ничего не делает сам: показывает
//! диалог и ждёт тебя. Закрытые тобой сессии не запоминаются.
//!
//! Каждое окно vv пишет свой файл `~/.vibeterminal/sessions/<pid>.json`.
//! Новый vv забирает файл одного окна, которого уже нет (самый свежий), —
//! так после перезапуска с несколькими окнами каждое получает свои сессии.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};

/// Совсем старые сохранения не открываем — просто убираем.
const KEEP_FOR: Duration = Duration::from_secs(30 * 24 * 3600);

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct SavedSession {
    pub name: String,
    pub cwd: PathBuf,
    /// Диалог Claude — его и продолжаем.
    pub claude_id: Option<String>,
    /// Файл диалога: нет файла — нечего продолжать, начинаем заново.
    pub transcript: Option<PathBuf>,
}

impl SavedSession {
    /// ID диалога, если его можно продолжить.
    pub fn resumable(&self) -> Option<&str> {
        let id = self.claude_id.as_deref()?;
        // Только вид UUID: ни пробелов, ни `-` в начале — `claude` не примет это за флаг.
        let valid = id.starts_with(|c: char| c.is_ascii_hexdigit()) && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
        (valid && self.transcript.as_deref().is_some_and(Path::exists)).then_some(id)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SavedWindow {
    pub sessions: Vec<SavedSession>,
    /// Какая была открыта.
    pub selected: usize,
}

fn dir(home: &Path) -> PathBuf {
    home.join(".vibeterminal/sessions")
}

/// Запомнить сессии этого окна. Нечего помнить — файл убираем.
pub fn save(home: &Path, window: &SavedWindow) -> std::io::Result<()> {
    let dir = dir(home);
    let path = dir.join(format!("{}.json", std::process::id()));
    if window.sessions.is_empty() {
        return match std::fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
            _ => Ok(()),
        };
    }
    std::fs::create_dir_all(&dir)?;
    let json = serde_json::to_string_pretty(window).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, json + "\n")?;
    std::fs::rename(tmp, path)
}

/// Забрать сессии окна, которого уже нет: самые свежие. Забирает
/// переименованием — два vv, стартующих разом, не получат одно и то же.
pub fn claim(home: &Path) -> Option<SavedWindow> {
    let dir = dir(home);
    let run = home.join(".vibeterminal/run");
    let me = std::process::id();
    let mut files: Vec<(SystemTime, PathBuf)> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| {
            let pid: u32 = path.file_stem()?.to_str()?.parse().ok()?;
            if pid == me || alive(pid, &run) {
                return None;
            }
            let modified = path.metadata().and_then(|m| m.modified()).ok()?;
            Some((modified, path))
        })
        .collect();
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    for (modified, path) in files {
        let claimed = dir.join(format!("{me}.claimed"));
        if std::fs::rename(&path, &claimed).is_err() {
            continue;
        }
        let window = std::fs::read_to_string(&claimed).ok().and_then(|text| serde_json::from_str(&text).ok());
        let _ = std::fs::remove_file(&claimed);
        let fresh = modified.elapsed().is_ok_and(|age| age < KEEP_FOR);
        if let Some(window) = window.filter(|_: &SavedWindow| fresh) {
            return Some(window);
        }
    }
    None
}

/// Окно vv ещё открыто: процесс жив и его сокет на месте (vv убирает сокет,
/// когда закрывается, — так не спутаем с чужим процессом с тем же pid).
fn alive(pid: u32, run: &Path) -> bool {
    // SAFETY: kill с сигналом 0 только проверяет, есть ли процесс.
    let exists = unsafe { libc::kill(pid as libc::pid_t, 0) == 0 };
    exists && run.join(format!("{pid}.sock")).exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_home(name: &str) -> PathBuf {
        let home = std::env::temp_dir().join(format!("vv-saved-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&home);
        std::fs::create_dir_all(&home).unwrap();
        home
    }

    fn session(name: &str) -> SavedSession {
        SavedSession { name: name.into(), cwd: "/tmp".into(), ..SavedSession::default() }
    }

    #[test]
    fn claims_windows_that_are_gone_one_at_a_time() {
        let home = temp_home("claim");
        let dir = dir(&home);
        std::fs::create_dir_all(&dir).unwrap();
        // Два окна, которых уже нет (таких pid не бывает).
        let old = SavedWindow { sessions: vec![session("старое")], selected: 0 };
        let new = SavedWindow { sessions: vec![session("a"), session("b")], selected: 1 };
        std::fs::write(dir.join("999991.json"), serde_json::to_string(&old).unwrap()).unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(dir.join("999992.json"), serde_json::to_string(&new).unwrap()).unwrap();

        assert_eq!(claim(&home), Some(new), "сначала самое свежее окно");
        assert_eq!(claim(&home), Some(old));
        assert_eq!(claim(&home), None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn saves_own_window_and_forgets_empty() {
        let home = temp_home("save");
        let window = SavedWindow { sessions: vec![session("api")], selected: 0 };
        save(&home, &window).unwrap();
        let path = dir(&home).join(format!("{}.json", std::process::id()));
        assert!(path.exists());
        // Своё окно живо — само у себя не забирает.
        assert_eq!(claim(&home), None);
        save(&home, &SavedWindow::default()).unwrap();
        assert!(!path.exists());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn resumes_only_existing_dialogs() {
        let home = temp_home("resume");
        let transcript = home.join("d.jsonl");
        std::fs::write(&transcript, "{}").unwrap();
        let mut saved = SavedSession {
            claude_id: Some("16902727-53b3-42e4-b3e8-5fe59a2cdbd0".into()),
            transcript: Some(transcript),
            ..session("api")
        };
        assert_eq!(saved.resumable(), Some("16902727-53b3-42e4-b3e8-5fe59a2cdbd0"));
        saved.claude_id = Some("--dangerous".into());
        assert_eq!(saved.resumable(), None);
        saved.claude_id = Some("abc".into());
        saved.transcript = Some(home.join("нет.jsonl"));
        assert_eq!(saved.resumable(), None);
        let _ = std::fs::remove_dir_all(&home);
    }
}
