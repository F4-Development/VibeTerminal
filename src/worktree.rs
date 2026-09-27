//! Отдельная копия проекта на задачу — git worktree. В копии своя ветка и
//! свои файлы: два Claude в одном проекте не правят одно и то же. Ветки,
//! коммиты и история у копии общие с основной папкой.
//!
//! Копии лежат в `~/.vibeterminal/worktrees/<проект>/<ветка>` — вне проекта,
//! чтобы редактор не принял их за часть проекта. Claude доверяет копии, если
//! доверяет основному проекту, — лишнего вопроса не будет.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

use crate::git::{self, Branch};

/// Где лежат копии.
pub fn root(home: &Path) -> PathBuf {
    home.join(".vibeterminal/worktrees")
}

/// Копию сделал vv: она лежит у него.
pub fn is_ours(home: &Path, dir: &Path) -> bool {
    let root = root(home);
    let root = root.canonicalize().unwrap_or(root);
    dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf()).starts_with(root)
}

/// Основной проект, если папка — его копия. `None` — сам проект или не git.
pub fn main_repo(dir: &Path) -> Option<PathBuf> {
    let output = git::run(dir, &["rev-parse", "--path-format=absolute", "--git-dir", "--git-common-dir"]).ok()?;
    let mut lines = output.lines();
    let (git_dir, common) = (PathBuf::from(lines.next()?), PathBuf::from(lines.next()?));
    // У копии своя папка git внутри общей `<проект>/.git`.
    if git_dir == common || common.file_name()? != ".git" {
        return None;
    }
    common.parent().map(Path::to_path_buf)
}

/// Основной проект папки: для копии — тот, с которого она сделана.
pub fn project_root(dir: &Path) -> Option<PathBuf> {
    main_repo(dir).or_else(|| git::run(dir, &["rev-parse", "--show-toplevel"]).ok().map(|s| PathBuf::from(s.trim())))
}

/// Одна и та же папка, даже если путь записан по-разному (`/tmp` и `/private/tmp`).
pub fn same_dir(a: &Path, b: &Path) -> bool {
    a == b || a.canonicalize().ok().zip(b.canonicalize().ok()).is_some_and(|(a, b)| a == b)
}

/// Настоящий путь папки, а нет такой — как есть.
pub fn canonical(dir: &Path) -> PathBuf {
    dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf())
}

/// Копия проекта (или сама основная папка) и ветка в ней.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    /// `None` — не на ветке.
    pub branch: Option<String>,
}

/// Все копии проекта, первой — основная папка.
pub fn list(dir: &Path) -> Vec<Worktree> {
    git::run(dir, &["worktree", "list", "--porcelain"]).map(|output| parse_list(&output)).unwrap_or_default()
}

fn parse_list(output: &str) -> Vec<Worktree> {
    output
        .split("\n\n")
        .filter_map(|block| {
            let path = block.lines().find_map(|line| line.strip_prefix("worktree "))?;
            let branch = block.lines().find_map(|line| line.strip_prefix("branch refs/heads/"));
            Some(Worktree { path: PathBuf::from(path), branch: branch.map(str::to_string) })
        })
        .collect()
}

/// На какой ветке откроется копия.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Start {
    /// Новая ветка от того, что открыто в папке.
    New,
    /// Такая ветка уже есть — копия откроется на ней.
    Existing,
    /// Есть только на сервере (`origin/feat`) — заберём оттуда.
    Remote(String),
    /// Ветка уже открыта в этой папке — вторую копию на ней git не даст.
    Busy(PathBuf),
    /// Так ветку назвать нельзя.
    Invalid,
}

/// Что будет с веткой `name` в проекте с такими ветками и копиями.
pub fn start(name: &str, branches: &[Branch], worktrees: &[Worktree]) -> Start {
    if !valid_branch(name) {
        return Start::Invalid;
    }
    if let Some(open) = worktrees.iter().find(|w| w.branch.as_deref() == Some(name)) {
        return Start::Busy(open.path.clone());
    }
    if branches.iter().any(|b| !b.remote && b.name == name) {
        return Start::Existing;
    }
    // Одна и та же ветка на нескольких серверах — с origin.
    let remote = branches
        .iter()
        .filter(|b| b.remote && b.name.split_once('/').is_some_and(|(_, local)| local == name))
        .min_by_key(|b| !b.name.starts_with("origin/"));
    remote.map_or(Start::New, |b| Start::Remote(b.name.clone()))
}

/// Имя ветки из того, что ввели: без пробелов по краям, пробелы внутри — `-`.
pub fn branch_name(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join("-")
}

/// Правила имён веток git (`git check-ref-format --branch`).
pub fn valid_branch(name: &str) -> bool {
    !name.is_empty()
        && name != "@"
        && !name.starts_with('-')
        && !name.ends_with('.')
        && !name.ends_with(".lock")
        && !name.contains("..")
        && !name.contains("@{")
        && !name.chars().any(|c| c.is_control() || c.is_whitespace() || "~^:?*[\\".contains(c))
        && name.split('/').all(|part| !part.is_empty() && !part.starts_with('.'))
}

/// Сделать копию проекта папки `from` на ветке `branch`. Новая ветка
/// начинается с того, что сейчас открыто в `from`. Возвращает папку копии.
pub fn create(home: &Path, from: &Path, branch: &str) -> Result<PathBuf, String> {
    let project = project_root(from).ok_or("Это не git-репозиторий — копию сделать нельзя.")?;
    // Копии, чьи папки удалили руками, git ещё считает открытыми.
    let _ = git::run(from, &["worktree", "prune"]);
    let start = start(branch, &git::branches(from), &list(from));
    let name = project.file_name().map_or_else(|| "project".into(), |n| n.to_string_lossy().into_owned());
    let path = free_path(&root(home).join(name).join(branch.replace('/', "-")));
    let dir = path.to_string_lossy();
    let args: Vec<&str> = match &start {
        Start::Invalid => return Err(format!("Так ветку назвать нельзя: {branch}")),
        Start::Busy(open) => return Err(format!("Ветка {branch} уже открыта в {}", open.display())),
        Start::New => vec!["worktree", "add", "-b", branch, &dir, "HEAD"],
        Start::Existing => vec!["worktree", "add", &dir, branch],
        Start::Remote(remote) => vec!["worktree", "add", "--track", "-b", branch, &dir, remote],
    };
    let parent = path.parent().unwrap_or(&path);
    std::fs::create_dir_all(parent).map_err(|e| format!("Не создать папку для копий: {e}"))?;
    if let Err(text) = git::run(from, &args) {
        // Не вышло — не оставлять пустую папку проекта.
        let _ = std::fs::remove_dir(parent);
        return Err(text);
    }
    Ok(path)
}

// ── Что перенести в копию ─────────────────────────────────────────────────

/// Что переносить без `.worktreeinclude`: переменные окружения и
/// разрешения, которые ты уже дал Claude в этом проекте.
const CARRY_DEFAULT: [&str; 2] = [".env*", ".claude/settings.local.json"];

/// Перенести в копию то, чего нет в git, но без чего проект не работает.
/// Что именно — в `.worktreeinclude` проекта (как у Claude Code, шаблоны
/// как в `.gitignore`), без него — `.env*` и `.claude/settings.local.json`.
/// Возвращает, что перенесли.
pub fn carry(project: &Path, copy: &Path) -> Vec<String> {
    let patterns = match std::fs::read_to_string(project.join(".worktreeinclude")) {
        Ok(text) => {
            text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with('#')).map(str::to_string).collect()
        }
        Err(_) => CARRY_DEFAULT.map(str::to_string).to_vec(),
    };
    // Только то, что git не хранит; целиком игнорируемые папки — одной строкой.
    let args = ["ls-files", "-z", "--others", "--ignored", "--exclude-standard", "--directory"];
    let Ok(ignored) = git::run(project, &args) else { return Vec::new() };
    let mut carried = Vec::new();
    for entry in ignored.split('\0').filter(|e| !e.is_empty() && included(&patterns, e)) {
        let path = entry.trim_end_matches('/');
        let target = copy.join(path);
        if !target.exists() && copy_all(&project.join(path), &target).is_ok() {
            carried.push(path.to_string());
        }
    }
    carried
}

/// Решает последний подходящий шаблон; `!шаблон` — не переносить.
fn included(patterns: &[String], entry: &str) -> bool {
    patterns.iter().rev().find_map(|p| match p.strip_prefix('!') {
        Some(negated) => matches(negated, entry).then_some(false),
        None => matches(p, entry).then_some(true),
    }) == Some(true)
}

/// Подходит ли путь от корня проекта (папка — со `/` на конце) под шаблон
/// как в `.gitignore`. Без классов `[...]`.
fn matches(pattern: &str, entry: &str) -> bool {
    let is_dir = entry.ends_with('/');
    let path: Vec<char> = entry.trim_end_matches('/').chars().collect();
    let dir_only = pattern.ends_with('/');
    let pattern = pattern.trim_end_matches('/');
    if dir_only && !is_dir {
        return false;
    }
    // Со `/` — от корня проекта, без — по имени на любой глубине.
    if pattern.contains('/') {
        glob(&pattern.trim_start_matches('/').chars().collect::<Vec<_>>(), &path)
    } else {
        let name = path.iter().rposition(|&c| c == '/').map_or(&path[..], |i| &path[i + 1..]);
        glob(&pattern.chars().collect::<Vec<_>>(), name)
    }
}

/// `*` и `?` — внутри одного имени, `**` — через папки, `**/` — и ни одной.
fn glob(pattern: &[char], text: &[char]) -> bool {
    match pattern {
        [] => text.is_empty(),
        ['*', '*', '/', rest @ ..] => (0..=text.len()).any(|i| (i == 0 || text[i - 1] == '/') && glob(rest, &text[i..])),
        ['*', '*', rest @ ..] => (0..=text.len()).any(|i| glob(rest, &text[i..])),
        ['*', rest @ ..] => (0..=text.len()).take_while(|&i| i == 0 || text[i - 1] != '/').any(|i| glob(rest, &text[i..])),
        ['?', rest @ ..] => text.first().is_some_and(|&c| c != '/') && glob(rest, &text[1..]),
        [c, rest @ ..] => text.first() == Some(c) && glob(rest, &text[1..]),
    }
}

/// Скопировать файл или папку целиком. На APFS файлы клонируются — даже
/// большая папка не займёт места, пока её не поменяют.
fn copy_all(from: &Path, to: &Path) -> std::io::Result<()> {
    let meta = std::fs::symlink_metadata(from)?;
    if let Some(parent) = to.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if meta.file_type().is_symlink() {
        std::os::unix::fs::symlink(std::fs::read_link(from)?, to)
    } else if meta.is_dir() {
        std::fs::create_dir_all(to)?;
        for entry in std::fs::read_dir(from)? {
            let entry = entry?;
            copy_all(&entry.path(), &to.join(entry.file_name()))?;
        }
        Ok(())
    } else {
        std::fs::copy(from, to).map(drop)
    }
}

// ── Подготовка копии ──────────────────────────────────────────────────────

/// Зависимости в копию не переносятся — их ставят заново. Команда по
/// файлу проекта: первый найденный.
const SETUP_BY_FILE: [(&str, &str); 10] = [
    ("pnpm-lock.yaml", "pnpm install"),
    ("yarn.lock", "yarn install"),
    ("bun.lock", "bun install"),
    ("bun.lockb", "bun install"),
    ("package-lock.json", "npm install"),
    ("package.json", "npm install"),
    ("composer.json", "composer install"),
    ("Gemfile", "bundle install"),
    ("uv.lock", "uv sync"),
    ("poetry.lock", "poetry install"),
];

/// Команда подготовки по файлам проекта и по какому файлу она выбрана.
pub fn detect_setup(project: &Path) -> Option<(&'static str, &'static str)> {
    SETUP_BY_FILE.iter().find(|(file, _)| project.join(file).exists()).map(|&(file, command)| (command, file))
}

/// Что vv помнит о копиях: команду подготовки каждого проекта.
#[derive(Default, Serialize, Deserialize)]
#[serde(default)]
struct Memory {
    setup: BTreeMap<String, String>,
}

fn memory_path(home: &Path) -> PathBuf {
    home.join(".vibeterminal/copies.json")
}

fn memory(home: &Path) -> Memory {
    std::fs::read_to_string(memory_path(home)).ok().and_then(|text| serde_json::from_str(&text).ok()).unwrap_or_default()
}

/// Команда подготовки, выбранная для проекта в прошлый раз (пусто — без подготовки).
pub fn remembered_setup(home: &Path, project: &Path) -> Option<String> {
    memory(home).setup.remove(&project.to_string_lossy().into_owned())
}

pub fn remember_setup(home: &Path, project: &Path, command: &str) {
    let mut memory = memory(home);
    memory.setup.insert(project.to_string_lossy().into_owned(), command.to_string());
    let path = memory_path(home);
    if let (Some(dir), Ok(json)) = (path.parent(), serde_json::to_string_pretty(&memory)) {
        let _ = std::fs::create_dir_all(dir);
        let _ = std::fs::write(&path, json + "\n");
    }
}

/// Убрать копию проекта. Ветка остаётся со всеми коммитами. `force` —
/// даже с незакоммиченными изменениями (они пропадут).
pub fn remove(project: &Path, dir: &Path, force: bool) -> Result<(), String> {
    // Сессию могли открыть в подпапке копии — убираем копию целиком.
    let top = git::run(dir, &["rev-parse", "--show-toplevel"]).map(|s| PathBuf::from(s.trim()));
    let dir = top.as_deref().unwrap_or(dir);
    let dir_arg = dir.to_string_lossy();
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(&dir_arg);
    git::run(project, &args)?;
    // Была последней копией проекта — убрать и пустую папку проекта.
    if let Some(parent) = dir.parent() {
        let _ = std::fs::remove_dir(parent);
    }
    Ok(())
}

/// Оставленные копии — свежие первыми.
pub fn kept(home: &Path) -> Vec<PathBuf> {
    let Ok(projects) = std::fs::read_dir(root(home)) else { return Vec::new() };
    let mut found: Vec<(SystemTime, PathBuf)> = projects
        .flatten()
        .filter_map(|project| std::fs::read_dir(project.path()).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        // В копии `.git` — файл со ссылкой на проект.
        .filter(|path| path.join(".git").is_file())
        .map(|path| (path.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH), path))
        .collect();
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.into_iter().map(|(_, path)| path).collect()
}

/// Готовая копия: где она, что в неё перенесли и чем её подготовить.
pub struct Prepared {
    pub dir: PathBuf,
    pub carried: Vec<String>,
    pub setup: String,
}

/// Копия целиком: сделать, перенести `.env` и т.п., запомнить команду
/// подготовки для следующего раза. Саму подготовку запускает сессия — на виду.
pub fn prepare(home: &Path, from: &Path, branch: &str, setup: &str) -> Result<Prepared, String> {
    let dir = create(home, from, branch)?;
    let project = project_root(from).unwrap_or_else(|| from.to_path_buf());
    let carried = carry(&project, &dir);
    remember_setup(home, &project, setup);
    Ok(Prepared { dir, carried, setup: setup.to_string() })
}

/// `feat-login`, а если такая папка уже есть — `feat-login-2`…
fn free_path(path: &Path) -> PathBuf {
    if !path.exists() {
        return path.to_path_buf();
    }
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    (2..).map(|n| path.with_file_name(format!("{name}-{n}"))).find(|p| !p.exists()).unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn branch(name: &str, remote: bool) -> Branch {
        Branch { name: name.into(), remote, current: false, ahead: 0, behind: 0 }
    }

    #[test]
    fn checks_branch_names() {
        assert!(valid_branch("feat-login"));
        assert!(valid_branch("feat/login"));
        assert!(valid_branch("задача"));
        for bad in ["", "-x", "a..b", "a b", "a:b", "a/", "/a", "a//b", "a.lock", ".a", "a/.b", "a@{1}", "@", "a~1"] {
            assert!(!valid_branch(bad), "{bad:?}");
        }
        assert_eq!(branch_name("  новая задача  "), "новая-задача");
    }

    #[test]
    fn decides_where_branch_comes_from() {
        let branches = [
            branch("main", false),
            branch("feat", false),
            // Локальная ветка со `/` в имени — не удалённая.
            branch("origin/fix", false),
            branch("up/fix", true),
            branch("origin/fix", true),
        ];
        let worktrees = [Worktree { path: "/p".into(), branch: Some("main".into()) }];
        assert_eq!(start("main", &branches, &worktrees), Start::Busy("/p".into()));
        assert_eq!(start("feat", &branches, &worktrees), Start::Existing);
        assert_eq!(start("fix", &branches, &worktrees), Start::Remote("origin/fix".into()));
        assert_eq!(start("new", &branches, &worktrees), Start::New);
        assert_eq!(start("a b", &branches, &worktrees), Start::Invalid);
    }

    #[test]
    fn parses_worktree_list() {
        let output = "worktree /p\nHEAD abc\nbranch refs/heads/main\n\nworktree /w/x\nHEAD def\ndetached\n\n";
        assert_eq!(
            parse_list(output),
            [Worktree { path: "/p".into(), branch: Some("main".into()) }, Worktree { path: "/w/x".into(), branch: None }]
        );
    }

    #[test]
    fn matches_like_gitignore() {
        assert!(matches(".env*", ".env"));
        assert!(matches(".env*", "apps/api/.env.local"));
        assert!(!matches(".env*", "a.env"));
        assert!(matches(".claude/settings.local.json", ".claude/settings.local.json"));
        assert!(!matches(".claude/settings.local.json", "x/.claude/settings.local.json"));
        assert!(matches("node_modules/", "node_modules/"));
        assert!(!matches("node_modules/", "node_modules"));
        assert!(matches("config/*.json", "config/local.json"));
        assert!(!matches("config/*.json", "config/a/local.json"));
        assert!(matches("**/secrets", "secrets/"));
        assert!(matches("**/secrets", "a/b/secrets"));
        assert!(!matches("**/secrets", "a/bsecrets"));
        assert!(matches("data/**", "data/a/b.txt"));
        let patterns = [".env*".to_string(), "!.env.example".to_string()];
        assert!(included(&patterns, ".env"));
        assert!(!included(&patterns, ".env.example"));
        assert!(!included(&patterns, "readme.md"));
    }

    /// Настоящий git: копия на новой ветке, на существующей, занятая ветка.
    #[test]
    fn creates_copies_in_real_repo() {
        let base = std::env::temp_dir().join(format!("vv-worktree-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let (home, repo) = (base.join("home"), base.join("shop"));
        std::fs::create_dir_all(&repo).unwrap();
        git::run(&repo, &["init", "-q", "-b", "main"]).unwrap();
        git::run(&repo, &["config", "user.email", "t@t"]).unwrap();
        git::run(&repo, &["config", "user.name", "t"]).unwrap();
        std::fs::write(repo.join("a.txt"), "1").unwrap();
        git::run(&repo, &["add", "."]).unwrap();
        git::run(&repo, &["commit", "-qm", "первый"]).unwrap();
        git::run(&repo, &["branch", "old"]).unwrap();

        let copy = create(&home, &repo, "feat/login").unwrap();
        assert_eq!(copy, root(&home).join("shop/feat-login"));
        assert_eq!(std::fs::read_to_string(copy.join("a.txt")).unwrap(), "1");
        assert_eq!(git::status(&copy).unwrap().branch.as_deref(), Some("feat/login"));
        assert_eq!(main_repo(&copy).map(|p| p.canonicalize().unwrap()), Some(repo.canonicalize().unwrap()));
        assert_eq!(main_repo(&repo), None);
        assert_eq!(project_root(&copy).map(|p| p.canonicalize().unwrap()), Some(repo.canonicalize().unwrap()));

        // Из копии — ещё одна копия того же проекта, на уже существующей ветке.
        let second = create(&home, &copy, "old").unwrap();
        assert_eq!(git::status(&second).unwrap().branch.as_deref(), Some("old"));
        assert_eq!(list(&repo).len(), 3);

        let busy = create(&home, &repo, "feat/login").unwrap_err();
        assert!(busy.contains("уже открыта"), "{busy}");
        // Не вышло с первой копией проекта — пустой папки проекта не остаётся.
        let other = base.join("other");
        std::fs::create_dir_all(&other).unwrap();
        git::run(&other, &["init", "-q", "-b", "main"]).unwrap();
        assert!(create(&home, &other, "x").is_err(), "без коммитов копию не сделать");
        assert!(!root(&home).join("other").exists());

        // .env и разрешения Claude — в копию; то, что в git, и прочий мусор — нет.
        std::fs::write(repo.join(".gitignore"), ".env*\n.claude/settings.local.json\ncache/\n").unwrap();
        git::run(&repo, &["add", ".gitignore"]).unwrap();
        git::run(&repo, &["commit", "-qm", "ignore"]).unwrap();
        std::fs::write(repo.join(".env"), "KEY=1").unwrap();
        std::fs::create_dir_all(repo.join(".claude")).unwrap();
        std::fs::write(repo.join(".claude/settings.local.json"), "{}").unwrap();
        std::fs::create_dir_all(repo.join("cache")).unwrap();
        std::fs::write(repo.join("cache/x"), "").unwrap();
        let third = create(&home, &repo, "env").unwrap();
        let mut carried = carry(&repo, &third);
        carried.sort();
        assert_eq!(carried, [".claude/settings.local.json", ".env"]);
        assert_eq!(std::fs::read_to_string(third.join(".env")).unwrap(), "KEY=1");
        assert!(!third.join("cache").exists());

        // Своё правило: папку целиком.
        std::fs::write(repo.join(".worktreeinclude"), "cache/\n").unwrap();
        let fourth = create(&home, &repo, "cache").unwrap();
        assert_eq!(carry(&repo, &fourth), ["cache"]);
        assert!(fourth.join("cache/x").exists());

        assert_eq!(detect_setup(&repo), None);
        std::fs::write(repo.join("package.json"), "{}").unwrap();
        std::fs::write(repo.join("yarn.lock"), "").unwrap();
        assert_eq!(detect_setup(&repo), Some(("yarn install", "yarn.lock")));
        assert_eq!(remembered_setup(&home, &repo), None);
        remember_setup(&home, &repo, "");
        assert_eq!(remembered_setup(&home, &repo).as_deref(), Some(""));

        // Копии видны для выбора; удалённая исчезает, ветка остаётся.
        assert_eq!(kept(&home).len(), 4);
        std::fs::write(third.join("new.txt"), "").unwrap();
        assert!(remove(&repo, &third, false).is_err(), "с изменениями — только принудительно");
        remove(&repo, &third, true).unwrap();
        // Сессия была в подпапке копии — удаляется копия целиком.
        remove(&repo, &fourth.join("cache"), false).unwrap();
        assert!(!third.exists() && !fourth.exists());
        assert_eq!(kept(&home).len(), 2);
        assert!(git::branches(&repo).iter().any(|b| b.name == "env"));
        let _ = std::fs::remove_dir_all(&base);
    }
}
