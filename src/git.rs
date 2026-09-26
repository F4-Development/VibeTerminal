//! Git для шапки и меню веток. Всё через системный `git` — с настройками и
//! ключами пользователя. Команды отвязаны от терминала: `ssh` не может
//! спросить пароль поверх интерфейса, а честно ошибается, и vv показывает
//! понятное сообщение.

use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RepoStatus {
    /// `None` — HEAD не на ветке (detached).
    pub branch: Option<String>,
    pub head: String,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    /// Изменённые и новые файлы.
    pub changed: u32,
    pub conflicts: u32,
    pub operation: Option<Operation>,
}

impl RepoStatus {
    /// Что показывать как ветку: имя или короткий хеш.
    pub fn head_label(&self) -> String {
        self.branch.clone().unwrap_or_else(|| format!("({})", self.head))
    }
}

/// Незаконченное слияние, перебазирование и т.п.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
}

impl Operation {
    pub fn label(self) -> &'static str {
        match self {
            Operation::Merge => "слияние",
            Operation::Rebase => "перебазирование",
            Operation::CherryPick => "cherry-pick",
            Operation::Revert => "revert",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    /// `main` или `origin/feat`.
    pub name: String,
    pub remote: bool,
    pub current: bool,
    pub ahead: u32,
    pub behind: u32,
}

/// Изменённый файл для окна коммита.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChangedFile {
    pub path: String,
    /// Буква для списка: M — изменён, A — новый, D — удалён, R — переименован, ? — не в git.
    pub kind: char,
}

/// `git` в папке, без терминала и без вопросов.
pub fn git(cwd: &Path) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_EDITOR", "true")
        .env("GIT_SEQUENCE_EDITOR", "true")
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    // SAFETY: setsid безопасен между fork и exec.
    unsafe {
        command.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    command
}

/// Запустить и вернуть вывод; при ошибке — текст из stderr.
pub fn run(cwd: &Path, args: &[&str]) -> Result<String, String> {
    let output = git(cwd).args(args).output().map_err(|e| format!("git не запустился: {e}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    if output.status.success() {
        Ok(stdout)
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() { stdout.trim().to_string() } else { stderr })
    }
}

/// Состояние репозитория в папке; `None` — это не репозиторий.
pub fn status(cwd: &Path) -> Option<RepoStatus> {
    let output = git(cwd)
        .args(["--no-optional-locks", "status", "--porcelain=v2", "--branch", "-z"])
        .stderr(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let mut status = parse_status(&output.stdout);
    status.operation = operation(cwd);
    Some(status)
}

fn parse_status(bytes: &[u8]) -> RepoStatus {
    let mut status = RepoStatus::default();
    let mut entries = bytes.split(|&b| b == 0).filter(|e| !e.is_empty());
    while let Some(entry) = entries.next() {
        let line = String::from_utf8_lossy(entry);
        if let Some(oid) = line.strip_prefix("# branch.oid ") {
            status.head = oid.chars().take(7).collect();
        } else if let Some(head) = line.strip_prefix("# branch.head ") {
            status.branch = (head != "(detached)").then(|| head.to_string());
        } else if let Some(upstream) = line.strip_prefix("# branch.upstream ") {
            status.upstream = Some(upstream.to_string());
        } else if let Some(ab) = line.strip_prefix("# branch.ab ") {
            for part in ab.split_whitespace() {
                if let Some(n) = part.strip_prefix('+') {
                    status.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix('-') {
                    status.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("? ") {
            status.changed += 1;
        } else if line.starts_with("2 ") {
            status.changed += 1;
            // У переименования следом идёт старый путь.
            entries.next();
        } else if line.starts_with("u ") {
            status.conflicts += 1;
            status.changed += 1;
        }
    }
    status
}

fn operation(cwd: &Path) -> Option<Operation> {
    let git_dir = run(cwd, &["rev-parse", "--absolute-git-dir"]).ok()?;
    let dir = PathBuf::from(git_dir.trim());
    if dir.join("rebase-merge").exists() || dir.join("rebase-apply").exists() {
        Some(Operation::Rebase)
    } else if dir.join("MERGE_HEAD").exists() {
        Some(Operation::Merge)
    } else if dir.join("CHERRY_PICK_HEAD").exists() {
        Some(Operation::CherryPick)
    } else if dir.join("REVERT_HEAD").exists() {
        Some(Operation::Revert)
    } else {
        None
    }
}

/// Ветки: текущая, потом локальные и удалённые — свежие первыми.
pub fn branches(cwd: &Path) -> Vec<Branch> {
    let format = "%(HEAD)%00%(refname)%00%(upstream:track,nobracket)%00%(committerdate:unix)";
    let Ok(output) = run(cwd, &["for-each-ref", &format!("--format={format}"), "refs/heads", "refs/remotes"]) else {
        return Vec::new();
    };
    parse_branches(&output)
}

fn parse_branches(output: &str) -> Vec<Branch> {
    let mut found: Vec<(i64, Branch)> = output
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let head = fields.next()?;
            let refname = fields.next()?;
            let track = fields.next().unwrap_or_default();
            let date: i64 = fields.next().unwrap_or_default().parse().unwrap_or(0);
            let (name, remote) = if let Some(local) = refname.strip_prefix("refs/heads/") {
                (local.to_string(), false)
            } else {
                let remote = refname.strip_prefix("refs/remotes/")?;
                // origin/HEAD — не ветка, а указатель.
                if remote.ends_with("/HEAD") {
                    return None;
                }
                (remote.to_string(), true)
            };
            let (mut ahead, mut behind) = (0, 0);
            for part in track.split(", ") {
                if let Some(n) = part.strip_prefix("ahead ") {
                    ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = part.strip_prefix("behind ") {
                    behind = n.parse().unwrap_or(0);
                }
            }
            Some((date, Branch { name, remote, current: head == "*", ahead, behind }))
        })
        .collect();
    found.sort_by(|a, b| {
        let rank = |b: &Branch| (!b.current, b.remote);
        rank(&a.1).cmp(&rank(&b.1)).then(b.0.cmp(&a.0))
    });
    found.into_iter().map(|(_, b)| b).collect()
}

/// Изменённые файлы для окна коммита.
pub fn changed_files(cwd: &Path) -> Vec<ChangedFile> {
    let Ok(output) = run(cwd, &["--no-optional-locks", "status", "--porcelain=v1", "-z", "--untracked-files=all"])
    else {
        return Vec::new();
    };
    parse_changed(&output)
}

fn parse_changed(output: &str) -> Vec<ChangedFile> {
    let mut files = Vec::new();
    let mut entries = output.split('\0').filter(|e| e.len() > 3);
    while let Some(entry) = entries.next() {
        let (xy, path) = entry.split_at(3);
        let xy: Vec<char> = xy.chars().collect();
        let kind = match (xy[0], xy[1]) {
            ('?', _) => '?',
            ('R', _) | (_, 'R') => {
                // В -z у переименования следом идёт старый путь.
                entries.next();
                'R'
            }
            ('A', _) => 'A',
            ('D', _) | (_, 'D') => 'D',
            _ => 'M',
        };
        files.push(ChangedFile { path: path.to_string(), kind });
    }
    files
}

/// Что сделать с репозиторием. Выполняется в фоне.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitOp {
    Fetch { quiet: bool },
    Pull,
    PullMerge,
    PullRebase,
    Push,
    PushForce,
    Switch { branch: String, remote: bool, carry: bool },
    NewBranch { name: String, base: Option<String> },
    Merge(String),
    Rebase(String),
    Rename { old: String, new: String },
    Delete { name: String, force: bool },
    Continue(Operation),
    Abort(Operation),
    Commit { message: String, files: Vec<String>, push: bool },
}

impl GitOp {
    /// Что писать в шапке, пока идёт.
    pub fn busy_label(&self) -> &'static str {
        match self {
            GitOp::Fetch { .. } => "получаю",
            GitOp::Pull | GitOp::PullMerge | GitOp::PullRebase => "обновляю",
            GitOp::Push | GitOp::PushForce => "отправляю",
            GitOp::Switch { .. } => "переключаю",
            GitOp::Commit { .. } => "коммичу",
            _ => "работаю",
        }
    }
}

/// Почему не вышло — чтобы предложить подходящий выход.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Незакоммиченные изменения мешают переключиться.
    DirtyCheckout,
    /// Ветка разошлась с сервером — нужен merge или rebase.
    Diverged,
    /// Сервер опередил — push отклонён.
    PushRejected,
    /// Ветка не слита — удаление только принудительно.
    NotMerged,
    /// Конфликты после merge/rebase.
    Conflicts,
    /// Нет доступа к серверу.
    Auth,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpError {
    pub failure: Failure,
    pub text: String,
}

pub type OpResult = Result<String, OpError>;

/// Выполнить действие. Возвращает, что сказать пользователю.
pub fn execute(cwd: &Path, op: &GitOp) -> OpResult {
    match op {
        GitOp::Fetch { .. } => {
            checked(cwd, &["fetch", "--all", "--prune"])?;
            Ok("Изменения с сервера получены".into())
        }
        GitOp::Pull => {
            checked(cwd, &["pull", "--ff-only"])?;
            Ok("Ветка обновлена".into())
        }
        GitOp::PullMerge => {
            checked(cwd, &["pull", "--no-rebase", "--no-edit"])?;
            Ok("Ветка обновлена слиянием".into())
        }
        GitOp::PullRebase => {
            checked(cwd, &["pull", "--rebase"])?;
            Ok("Ветка обновлена перебазированием".into())
        }
        GitOp::Push | GitOp::PushForce => push(cwd, matches!(op, GitOp::PushForce)),
        GitOp::Switch { branch, remote, carry } => switch(cwd, branch, *remote, *carry),
        GitOp::NewBranch { name, base } => {
            let mut args = vec!["switch", "-c", name.as_str()];
            if let Some(base) = base {
                args.push(base);
            }
            checked(cwd, &args)?;
            Ok(format!("Создана ветка {name}"))
        }
        GitOp::Merge(branch) => {
            checked(cwd, &["merge", "--no-edit", branch])?;
            Ok(format!("{branch} слита в текущую ветку"))
        }
        GitOp::Rebase(branch) => {
            checked(cwd, &["rebase", branch])?;
            Ok(format!("Ветка перебазирована на {branch}"))
        }
        GitOp::Rename { old, new } => {
            checked(cwd, &["branch", "-m", old, new])?;
            Ok(format!("{old} теперь называется {new}"))
        }
        GitOp::Delete { name, force } => {
            checked(cwd, &["branch", if *force { "-D" } else { "-d" }, name])?;
            Ok(format!("Ветка {name} удалена"))
        }
        GitOp::Continue(operation) => {
            checked(cwd, &[operation_command(*operation), "--continue"])?;
            Ok(format!("{} завершено", capitalize(operation.label())))
        }
        GitOp::Abort(operation) => {
            checked(cwd, &[operation_command(*operation), "--abort"])?;
            Ok(format!("{} отменено", capitalize(operation.label())))
        }
        GitOp::Commit { message, files, push: then_push } => {
            let mut add = vec!["add", "--"];
            add.extend(files.iter().map(String::as_str));
            checked(cwd, &add)?;
            let mut commit = vec!["commit", "-m", message.as_str(), "--"];
            commit.extend(files.iter().map(String::as_str));
            checked(cwd, &commit)?;
            if *then_push {
                push(cwd, false)?;
                return Ok("Закоммичено и отправлено".into());
            }
            Ok("Закоммичено".into())
        }
    }
}

fn operation_command(operation: Operation) -> &'static str {
    match operation {
        Operation::Merge => "merge",
        Operation::Rebase => "rebase",
        Operation::CherryPick => "cherry-pick",
        Operation::Revert => "revert",
    }
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
}

fn push(cwd: &Path, force: bool) -> OpResult {
    let status = status(cwd).unwrap_or_default();
    let branch = status.branch.ok_or_else(|| OpError {
        failure: Failure::Other,
        text: "HEAD не на ветке — отправлять нечего.".into(),
    })?;
    let mut args = vec!["push"];
    if force {
        args.push("--force-with-lease");
    }
    // Новая ветка: заводим её на сервере и связываем.
    let remote;
    if status.upstream.is_none() {
        remote = default_remote(cwd).ok_or_else(|| OpError {
            failure: Failure::Other,
            text: "У репозитория нет сервера (remote) — отправлять некуда.".into(),
        })?;
        args.extend(["-u", remote.as_str(), branch.as_str()]);
    }
    checked(cwd, &args)?;
    Ok(format!("Ветка {branch} отправлена"))
}

fn default_remote(cwd: &Path) -> Option<String> {
    let remotes = run(cwd, &["remote"]).ok()?;
    let remotes: Vec<&str> = remotes.lines().collect();
    remotes.iter().find(|r| **r == "origin").or(remotes.first()).map(|r| r.to_string())
}

fn switch(cwd: &Path, branch: &str, remote: bool, carry: bool) -> OpResult {
    // Удалённая ветка: создаём локальную с тем же именем и связываем.
    let local = if remote { branch.split_once('/').map_or(branch, |(_, name)| name) } else { branch };
    let mut args = vec!["switch"];
    if remote {
        args.extend(["-c", local, "--track", branch]);
    } else {
        args.push(local);
    }
    if carry {
        checked(cwd, &["stash", "push", "--include-untracked", "-m", &format!("vv: перенос на {local}")])?;
        let switched = checked(cwd, &args);
        let popped = checked(cwd, &["stash", "pop"]);
        switched?;
        if let Err(error) = popped {
            return Err(OpError {
                failure: Failure::Conflicts,
                text: format!(
                    "Переключился на {local}, но изменения легли с конфликтами. Они сохранены в stash.\n\n{}",
                    error.text
                ),
            });
        }
        return Ok(format!("Переключился на {local}, изменения перенесены"));
    }
    checked(cwd, &args)?;
    Ok(format!("Переключился на {local}"))
}

/// `run`, но ошибка сразу разобрана — что именно пошло не так.
fn checked(cwd: &Path, args: &[&str]) -> Result<String, OpError> {
    run(cwd, args).map_err(|text| {
        let conflicts = status(cwd).is_some_and(|s| s.conflicts > 0);
        OpError { failure: classify(&text, conflicts), text }
    })
}

fn classify(text: &str, conflicts: bool) -> Failure {
    let lower = text.to_lowercase();
    if conflicts || lower.contains("conflict") {
        Failure::Conflicts
    } else if lower.contains("would be overwritten by checkout") || lower.contains("please commit your changes or stash") {
        Failure::DirtyCheckout
    } else if lower.contains("not possible to fast-forward") || lower.contains("divergent branches") {
        Failure::Diverged
    } else if lower.contains("rejected") && (lower.contains("fetch first") || lower.contains("non-fast-forward")) {
        Failure::PushRejected
    } else if lower.contains("not fully merged") {
        Failure::NotMerged
    } else if lower.contains("permission denied")
        || lower.contains("authentication failed")
        || lower.contains("could not read from remote")
        || lower.contains("terminal prompts disabled")
    {
        Failure::Auth
    } else {
        Failure::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_porcelain_v2() {
        let raw = b"# branch.oid 1234567890abcdef\0# branch.head main\0# branch.upstream origin/main\0\
# branch.ab +2 -1\0\
1 .M N... 100644 100644 100644 aaa bbb src/app.rs\0\
2 R. N... 100644 100644 100644 aaa bbb R100 new.rs\0old.rs\0\
u UU N... 100644 100644 100644 100644 a b c conflict.rs\0\
? notes.txt\0";
        let status = parse_status(raw);
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.head, "1234567");
        assert_eq!(status.upstream.as_deref(), Some("origin/main"));
        assert_eq!((status.ahead, status.behind), (2, 1));
        assert_eq!(status.changed, 4);
        assert_eq!(status.conflicts, 1);
    }

    #[test]
    fn detached_head_has_no_branch() {
        let status = parse_status(b"# branch.oid abcdef1234\0# branch.head (detached)\0");
        assert_eq!(status.branch, None);
        assert_eq!(status.head_label(), "(abcdef1)");
    }

    #[test]
    fn parses_branches_current_first() {
        let out = " \x00refs/heads/old\x00\x00100\n*\x00refs/heads/main\x00ahead 2, behind 1\x00200\n \x00refs/heads/feat\x00\x00300\n \x00refs/remotes/origin/HEAD\x00\x00300\n \x00refs/remotes/origin/dev\x00\x00250\n";
        let branches = parse_branches(out);
        let names: Vec<&str> = branches.iter().map(|b| b.name.as_str()).collect();
        assert_eq!(names, ["main", "feat", "old", "origin/dev"]);
        assert_eq!((branches[0].ahead, branches[0].behind), (2, 1));
        assert!(branches[3].remote);
    }

    #[test]
    fn parses_changed_files() {
        let out = " M src/app.rs\0A  new.rs\0R  renamed.rs\0orig.rs\0?? notes.txt\0 D gone.rs\0";
        let files = parse_changed(out);
        let kinds: Vec<(char, &str)> = files.iter().map(|f| (f.kind, f.path.as_str())).collect();
        assert_eq!(kinds, [('M', "src/app.rs"), ('A', "new.rs"), ('R', "renamed.rs"), ('?', "notes.txt"), ('D', "gone.rs")]);
    }

    #[test]
    fn classifies_errors() {
        assert_eq!(classify("error: Your local changes to the following files would be overwritten by checkout", false), Failure::DirtyCheckout);
        assert_eq!(classify("fatal: Not possible to fast-forward, aborting.", false), Failure::Diverged);
        assert_eq!(classify("! [rejected] main -> main (fetch first)", false), Failure::PushRejected);
        assert_eq!(classify("error: The branch 'x' is not fully merged.", false), Failure::NotMerged);
        assert_eq!(classify("git@host: Permission denied (publickey).", false), Failure::Auth);
        assert_eq!(classify("anything", true), Failure::Conflicts);
    }

    /// Настоящий git во временной папке: коммит, ветка, переключение с переносом изменений.
    #[test]
    fn real_repo_round_trip() {
        let dir = std::env::temp_dir().join(format!("vv-git-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        run(&dir, &["init", "-q", "-b", "main"]).unwrap();
        run(&dir, &["config", "user.email", "t@t"]).unwrap();
        run(&dir, &["config", "user.name", "t"]).unwrap();
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        execute(&dir, &GitOp::Commit { message: "первый".into(), files: vec!["a.txt".into()], push: false }).unwrap();

        let status = status(&dir).unwrap();
        assert_eq!(status.branch.as_deref(), Some("main"));
        assert_eq!(status.changed, 0);

        execute(&dir, &GitOp::NewBranch { name: "feat".into(), base: None }).unwrap();
        std::fs::write(dir.join("a.txt"), "2").unwrap();
        execute(&dir, &GitOp::Commit { message: "в feat".into(), files: vec!["a.txt".into()], push: false }).unwrap();
        std::fs::write(dir.join("a.txt"), "3").unwrap();

        let error = execute(&dir, &GitOp::Switch { branch: "main".into(), remote: false, carry: false }).unwrap_err();
        assert_eq!(error.failure, Failure::DirtyCheckout);

        let branches = branches(&dir);
        assert_eq!(branches[0].name, "feat");
        assert!(branches[0].current);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
