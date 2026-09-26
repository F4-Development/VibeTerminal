//! Окружение пользователя. Приложение из Dock запускает vv без PATH из
//! `~/.zshrc`, и `claude` может не найтись — тогда спрашиваем PATH у
//! login-оболочки пользователя, как это делают редакторы кода.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const MARK: &str = "__VV_PATH__";
const SHELL_TIMEOUT: Duration = Duration::from_secs(5);

/// Вызывать в самом начале `main`, пока не запущены потоки.
pub fn ensure_path() {
    if find_in_path("claude").is_some() {
        return;
    }
    if let Some(path) = login_shell_path() {
        // SAFETY: вызывается до запуска любых потоков.
        unsafe { std::env::set_var("PATH", path) };
    }
}

pub fn find_in_path(program: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|dir| dir.join(program)).find(|p| is_executable(p))
}

fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

fn login_shell_path() -> Option<String> {
    use std::os::unix::process::CommandExt;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut command = Command::new(shell);
    command
        .args(["-l", "-i", "-c", &format!("printf '{MARK}%s{MARK}' \"$PATH\"")])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    // Без терминала: иначе интерактивный zsh и его плагины начинают
    // переговариваться с экраном и ждать ответов.
    // SAFETY: setsid безопасен между fork и exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = command.spawn().ok()?;
    // Тяжёлый .zshrc не должен повесить запуск.
    let started = Instant::now();
    while child.try_wait().ok()?.is_none() {
        if started.elapsed() > SHELL_TIMEOUT {
            let _ = child.kill();
            return None;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().ok()?;
    extract(&String::from_utf8_lossy(&output.stdout))
}

/// PATH между метками: оболочка могла напечатать что-то ещё.
fn extract(output: &str) -> Option<String> {
    let start = output.find(MARK)? + MARK.len();
    let len = output[start..].find(MARK)?;
    Some(output[start..start + len].to_string()).filter(|p| !p.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_path_between_marks() {
        let noisy = format!("Welcome!\n{MARK}/opt/homebrew/bin:/usr/bin{MARK}\n");
        assert_eq!(extract(&noisy).as_deref(), Some("/opt/homebrew/bin:/usr/bin"));
        assert_eq!(extract("no marks"), None);
    }
}
