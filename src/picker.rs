//! Выбор папки для новой сессии: недавние проекты Claude, потом `~/Projects`,
//! поиск по мере ввода.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use crate::keys;
use crate::settings::Settings;
use crate::worktree;

pub struct Candidate {
    pub path: PathBuf,
    pub display: String,
    /// Подпись справа: откуда взялась папка.
    pub hint: &'static str,
}

pub struct Picker {
    pub query: String,
    candidates: Vec<Candidate>,
    /// Папка, вписанная прямо путём, если такая есть.
    typed: Option<Candidate>,
    /// Индексы подходящих кандидатов, лучшие первыми.
    matches: Vec<usize>,
    pub selected: usize,
    home: PathBuf,
}

impl Picker {
    pub fn new(home: &Path) -> Self {
        let mut picker = Self {
            query: String::new(),
            candidates: collect(home),
            typed: None,
            matches: Vec::new(),
            selected: 0,
            home: home.to_path_buf(),
        };
        picker.refilter();
        picker
    }

    pub fn push(&mut self, text: &str) {
        self.query.extend(text.chars().filter(|c| !c.is_control()));
        self.refilter();
    }

    pub fn backspace(&mut self) {
        self.query.pop();
        self.refilter();
    }

    pub fn delete_word(&mut self) {
        let trimmed = self.query.trim_end_matches(|c: char| c == '/' || c.is_whitespace());
        let cut = trimmed.rfind(['/', ' ']).map_or(0, |i| i + 1);
        self.query.truncate(cut);
        self.refilter();
    }

    pub fn clear(&mut self) {
        self.query.clear();
        self.refilter();
    }

    pub fn move_by(&mut self, delta: isize) {
        let len = self.len();
        if len > 0 {
            self.selected = (self.selected as isize + delta).rem_euclid(len as isize) as usize;
        }
    }

    pub fn len(&self) -> usize {
        self.typed.is_some() as usize + self.matches.len()
    }

    /// `i`-й пункт списка в текущем порядке.
    pub fn item(&self, i: usize) -> Option<&Candidate> {
        match (&self.typed, i) {
            (Some(typed), 0) => Some(typed),
            (Some(_), i) => self.matches.get(i - 1).map(|&m| &self.candidates[m]),
            (None, i) => self.matches.get(i).map(|&m| &self.candidates[m]),
        }
    }

    pub fn selected_path(&self) -> Option<PathBuf> {
        self.item(self.selected).map(|c| c.path.clone())
    }

    fn refilter(&mut self) {
        self.typed = typed_path(&self.query, &self.home).map(|path| Candidate {
            display: display_path(&path, &self.home),
            path,
            hint: "путь",
        });

        let query = self.query.trim().to_lowercase();
        let latin: String = query.chars().map(|c| keys::ru_to_en(c).unwrap_or(c)).collect();
        let mut scored: Vec<(i32, usize)> = self
            .candidates
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let target = c.display.to_lowercase();
                let best = score(&query, &target).max(score(&latin, &target))?;
                Some((best, i))
            })
            .collect();
        // Без запроса — порядок свежести; с запросом — лучшие, при равенстве короче.
        if !query.is_empty() {
            let len = |i: usize| self.candidates[i].display.len();
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(len(a.1).cmp(&len(b.1))).then(a.1.cmp(&b.1)));
        }
        self.matches = scored.into_iter().map(|(_, i)| i).collect();
        self.selected = 0;
    }
}

/// Нечёткое совпадение: все буквы запроса по порядку. Больше очков за
/// совпадения в имени папки, подряд и в начале слова. `None` — не подходит.
pub fn score(query: &str, target: &str) -> Option<i32> {
    let query: Vec<char> = query.chars().collect();
    let Some(&first) = query.first() else { return Some(0) };
    let chars: Vec<char> = target.chars().collect();
    let base_start = chars.iter().rposition(|&c| c == '/').map_or(0, |i| i + 1);
    // Жадно от каждого возможного начала — иначе «shop» зацепится за «s» в «projects».
    (0..chars.len())
        .filter(|&start| chars[start] == first)
        .filter_map(|start| score_from(&query, &chars, start, base_start))
        .max()
}

fn score_from(query: &[char], chars: &[char], start: usize, base_start: usize) -> Option<i32> {
    let mut total = 0;
    let mut pos = start;
    let mut prev: Option<usize> = None;
    for &q in query {
        let found = (pos..chars.len()).find(|&i| chars[i] == q)?;
        total += 1;
        if found >= base_start {
            total += 8;
        }
        if prev.is_some_and(|p| p + 1 == found) {
            total += 6;
        }
        if found == 0 || matches!(chars[found - 1], '/' | '-' | '_' | '.' | ' ') {
            total += 4;
        }
        prev = Some(found);
        pos = found + 1;
    }
    Some(total)
}

fn typed_path(query: &str, home: &Path) -> Option<PathBuf> {
    let query = query.trim();
    let path = if let Some(rest) = query.strip_prefix('~') {
        home.join(rest.trim_start_matches('/'))
    } else if query.starts_with('/') {
        PathBuf::from(query)
    } else {
        return None;
    };
    path.is_dir().then(|| path.canonicalize().unwrap_or(path))
}

fn collect(home: &Path) -> Vec<Candidate> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in claude_projects(home) {
        if path != home && seen.insert(path.clone()) {
            out.push(Candidate { display: display_path(&path, home), path, hint: "недавно" });
        }
    }
    // Оставленные копии проектов — чтобы вернуться к задаче.
    for path in worktree::kept(home) {
        if seen.insert(path.clone()) {
            out.push(Candidate { display: display_path(&path, home), path, hint: "копия" });
        }
    }
    for dir in Settings::load(home).projects_dirs(home) {
        for path in projects_dir(&dir) {
            if seen.insert(path.clone()) {
                out.push(Candidate { display: display_path(&path, home), path, hint: "" });
            }
        }
    }
    out
}

/// Папки, где уже запускали Claude, — по свежести последнего диалога.
fn claude_projects(home: &Path) -> Vec<PathBuf> {
    let Ok(text) = std::fs::read_to_string(home.join(".claude.json")) else {
        return Vec::new();
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&text) else {
        return Vec::new();
    };
    let Some(projects) = json.get("projects").and_then(|p| p.as_object()) else {
        return Vec::new();
    };
    let transcripts = home.join(".claude/projects");
    let mut found: Vec<(SystemTime, PathBuf)> = projects
        .keys()
        .map(PathBuf::from)
        .filter(|path| path.is_dir())
        .map(|path| {
            let modified = std::fs::metadata(transcripts.join(claude_dir_name(&path)))
                .and_then(|m| m.modified())
                .unwrap_or(SystemTime::UNIX_EPOCH);
            (modified, path)
        })
        .collect();
    found.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    found.into_iter().map(|(_, path)| path).collect()
}

/// В папке уже говорили с Claude — есть что продолжить. Claude называет
/// папку диалогов по настоящему пути (`/tmp` → `/private/tmp`).
pub fn has_dialogs(home: &Path, dir: &Path) -> bool {
    let dir = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let dialogs = home.join(".claude/projects").join(claude_dir_name(&dir));
    std::fs::read_dir(dialogs)
        .is_ok_and(|entries| entries.flatten().any(|e| e.path().extension().is_some_and(|ext| ext == "jsonl")))
}

/// Как Claude называет папку с диалогами проекта: всё, кроме букв и цифр, — `-`.
fn claude_dir_name(path: &Path) -> String {
    path.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn projects_dir(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Имя папки для подписи: `~/Projects/shop` → `shop`.
pub fn folder_name(dir: &Path) -> String {
    dir.file_name().map_or_else(|| dir.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// `/Users/me/Projects/x` → `~/Projects/x`.
pub fn display_path(path: &Path, home: &Path) -> String {
    match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".into(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn score_requires_all_letters_in_order() {
        assert!(score("shp", "~/projects/shop-api").is_some());
        assert!(score("phs", "~/projects/shop-api").is_none());
    }

    #[test]
    fn score_prefers_folder_name_and_word_starts() {
        let in_name = score("api", "~/projects/shop-api").unwrap();
        let spread = score("api", "~/a/p/i/zzz").unwrap();
        assert!(in_name > spread);
        let exact = score("shop", "~/projects/shop-api").unwrap();
        let scattered = score("shop", "~/projects/s-h-o-p").unwrap();
        assert!(exact > scattered);
    }

    #[test]
    fn claude_dir_name_matches_claude_scheme() {
        assert_eq!(claude_dir_name(Path::new("/Users/me/Projects/shop.example")), "-Users-me-Projects-shop-example");
    }

    #[test]
    fn display_path_uses_tilde() {
        let home = Path::new("/Users/me");
        assert_eq!(display_path(Path::new("/Users/me/Projects/x"), home), "~/Projects/x");
        assert_eq!(display_path(Path::new("/tmp"), home), "/tmp");
    }

    fn picker_with(paths: &[&str]) -> Picker {
        let home = PathBuf::from("/nonexistent-home");
        let candidates = paths
            .iter()
            .map(|p| Candidate { path: PathBuf::from(p), display: p.to_string(), hint: "" })
            .collect();
        let mut picker = Picker {
            query: String::new(),
            candidates,
            typed: None,
            matches: Vec::new(),
            selected: 0,
            home,
        };
        picker.refilter();
        picker
    }

    #[test]
    fn russian_layout_query_finds_latin_names() {
        let mut picker = picker_with(&["~/Projects/landing", "~/Projects/shop-api"]);
        picker.push("ырщз"); // «shop» в русской раскладке
        assert_eq!(picker.len(), 1);
        assert_eq!(picker.item(0).unwrap().display, "~/Projects/shop-api");
    }

    #[test]
    fn best_match_goes_first() {
        let mut picker = picker_with(&["~/Projects/api-docs", "~/Projects/shop-api", "~/Projects/shop"]);
        picker.push("shop");
        assert_eq!(picker.item(0).unwrap().display, "~/Projects/shop");
        assert_eq!(picker.len(), 2);
    }
}
