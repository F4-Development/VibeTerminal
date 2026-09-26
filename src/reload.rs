//! Живая перезагрузка vv. Файл vv обновился (собрали или поставили новую
//! версию) — vv перезапускается в том же окне тем же процессом (`exec`,
//! pid прежний), а открытые терминалы с Claude отдаёт новой версии вместе
//! с тем, что было на экране. Claude ничего не замечает и работает дальше.
//!
//! Перед перезапуском новая версия проверяется (`vv --version`): сломанная
//! сборка не заберёт сессии с собой.

use std::os::unix::fs::MetadataExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use serde::{Deserialize, Serialize};

/// Через эту переменную новая версия узнаёт, где лежит переданное состояние.
const ENV_HANDOFF: &str = "VV_HANDOFF";
/// Файл сначала дописывают, потом подписывают — ждём, пока он успокоится.
const SETTLE: Duration = Duration::from_millis(1500);
const CHECK_EVERY: Duration = Duration::from_secs(1);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// Что передаётся новой версии.
#[derive(Serialize, Deserialize)]
pub struct Handoff {
    pub sessions: Vec<HandedSession>,
    pub selected: usize,
    pub show_sidebar: bool,
}

/// Работающая сессия: открытый терминал, процесс и что было на экране.
#[derive(Serialize, Deserialize)]
pub struct HandedSession {
    /// Номер прежний — по нему Claude шлёт события.
    pub id: u64,
    pub name: String,
    pub cwd: PathBuf,
    /// Открытый конец терминала, переживает `exec`.
    pub fd: i32,
    pub pid: i32,
    pub is_command: bool,
    pub claude_id: Option<String>,
    pub transcript: Option<PathBuf>,
    pub state: String,
    pub detail: String,
    pub since_secs: u64,
    /// Экран и режимы терминала, как их понимает vt100.
    pub screen: String,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Stamp {
    len: u64,
    modified: SystemTime,
    inode: u64,
}

fn stamp(path: &Path) -> Option<Stamp> {
    let meta = std::fs::metadata(path).ok()?;
    Some(Stamp { len: meta.len(), modified: meta.modified().ok()?, inode: meta.ino() })
}

/// Следит за файлом vv.
pub struct Watch {
    path: PathBuf,
    running: Option<Stamp>,
    /// Новый вид файла и с каких пор он не меняется.
    seen: Option<(Stamp, Instant)>,
    /// Этот вид уже проверяли — второй раз не надо.
    tried: Option<Stamp>,
    last_check: Instant,
}

impl Watch {
    pub fn new() -> Option<Self> {
        let path = std::env::current_exe().ok()?;
        let running = stamp(&path);
        Some(Self { path, running, seen: None, tried: None, last_check: Instant::now() })
    }

    /// Раз в секунду смотрит на файл. Поменялся и полторы секунды не
    /// меняется — пора проверять новую версию: возвращает путь.
    pub fn poll(&mut self) -> Option<PathBuf> {
        if self.last_check.elapsed() < CHECK_EVERY {
            return None;
        }
        self.last_check = Instant::now();
        let now = stamp(&self.path)?;
        if Some(now) == self.running || Some(now) == self.tried {
            self.seen = None;
            return None;
        }
        match self.seen {
            Some((seen, since)) if seen == now && since.elapsed() >= SETTLE => {
                self.tried = Some(now);
                self.seen = None;
                Some(self.path.clone())
            }
            Some((seen, _)) if seen == now => None,
            _ => {
                self.seen = Some((now, Instant::now()));
                None
            }
        }
    }
}

/// Новая версия запускается и отвечает? Долго — звать из фонового потока.
pub fn works(path: &Path) -> bool {
    let child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut stdout) = child.stdout.take() {
                    let _ = std::io::Read::read_to_string(&mut stdout, &mut out);
                }
                return status.success() && out.starts_with("vv ");
            }
            Ok(None) if started.elapsed() < VERSION_TIMEOUT => thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Записать состояние для новой версии.
pub fn write(home: &Path, handoff: &Handoff) -> std::io::Result<PathBuf> {
    let dir = home.join(".vibeterminal/reload");
    std::fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.json", std::process::id()));
    let json = serde_json::to_string(handoff).map_err(std::io::Error::other)?;
    std::fs::write(&path, json)?;
    Ok(path)
}

/// Перезапуститься новой версией с тем же pid. Возвращает, только если
/// не вышло.
pub fn exec(path: &Path, handoff: &Path) -> std::io::Error {
    let args: Vec<String> = std::env::args().skip(1).collect();
    Command::new(path).args(args).env(ENV_HANDOFF, handoff).exec()
}

/// Новая версия при запуске: забрать переданное состояние. Звать до
/// запуска потоков — убирает переменную окружения, чтобы её не унаследовали
/// сессии.
pub fn take() -> Option<Handoff> {
    let path = PathBuf::from(std::env::var_os(ENV_HANDOFF)?);
    // SAFETY: зовётся в начале main, других потоков ещё нет.
    unsafe { std::env::remove_var(ENV_HANDOFF) };
    let text = std::fs::read_to_string(&path).ok();
    let _ = std::fs::remove_file(&path);
    serde_json::from_str(&text?).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waits_until_new_binary_settles() {
        let dir = std::env::temp_dir().join(format!("vv-reload-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("vv");
        std::fs::write(&file, "old").unwrap();
        let mut watch = Watch { path: file.clone(), running: stamp(&file), seen: None, tried: None, last_check: Instant::now() };
        let tick = |watch: &mut Watch| {
            watch.last_check = Instant::now() - CHECK_EVERY;
            watch.poll()
        };
        assert_eq!(tick(&mut watch), None, "не менялся");
        std::fs::write(&file, "new version").unwrap();
        assert_eq!(tick(&mut watch), None, "только что поменялся — ждём");
        watch.seen = watch.seen.map(|(s, _)| (s, Instant::now() - SETTLE));
        assert_eq!(tick(&mut watch), Some(file.clone()), "успокоился — проверять");
        assert_eq!(tick(&mut watch), None, "этот вид уже проверяли");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn broken_binary_does_not_work() {
        assert!(!works(Path::new("/nonexistent/vv")));
        assert!(!works(Path::new("/usr/bin/false")));
    }
}
