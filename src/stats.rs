//! Статистика Claude Code: сколько токенов ушло и во что это обошлось бы по
//! ценам API. Окно «Справка → Статистика Claude» в приложении зовёт
//! `vv stats` и рисует то, что он напечатает (JSON).
//!
//! Считаем по логам самого Claude Code на этом Маке —
//! `~/.claude/projects/**/*.jsonl`: у каждого ответа там модель, время, папка
//! и токены по видам. У подписки нет отчёта в токенах, поэтому аккаунт не
//! спрашиваем: другие компьютеры и claude.ai сюда не попадают, а старые логи
//! Claude удаляет сам.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use memchr::memmem;
use serde::{Deserialize, Serialize};

use crate::usage::{local, make_time, now};
use crate::worktree;

/// Цены API за миллион токенов. Запись в кэш — 1,25× ввода на 5 минут и 2×
/// на час, быстрый режим — всё вдвое (как у Opus 5 и 5.5).
struct Price {
    input: f64,
    output: f64,
    cache_read: f64,
}

const PRICES: [(&str, Price); 10] = [
    ("fable-5-1", Price { input: 10.0, output: 50.0, cache_read: 0.25 }),
    ("fable-5", Price { input: 10.0, output: 50.0, cache_read: 1.0 }),
    ("opus-5-5", Price { input: 4.0, output: 20.0, cache_read: 0.2 }),
    ("opus-5", Price { input: 5.0, output: 25.0, cache_read: 0.5 }),
    ("opus-4-8", Price { input: 5.0, output: 25.0, cache_read: 0.5 }),
    ("opus-4-7", Price { input: 5.0, output: 25.0, cache_read: 0.5 }),
    ("opus-4-6", Price { input: 5.0, output: 25.0, cache_read: 0.5 }),
    ("sonnet-5", Price { input: 2.0, output: 10.0, cache_read: 0.2 }),
    ("sonnet-4-6", Price { input: 3.0, output: 15.0, cache_read: 0.3 }),
    ("haiku-4-5", Price { input: 1.0, output: 5.0, cache_read: 0.1 }),
];
/// Поиск в интернете — $10 за тысячу.
const SEARCH_PRICE: f64 = 0.01;
/// Самых дорогих сессий в окне.
const TOP_SESSIONS: usize = 10;

/// `vv stats`: посчитать и напечатать JSON для окна.
pub fn run() -> anyhow::Result<()> {
    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
    let claude = std::env::var_os("CLAUDE_CONFIG_DIR").map_or_else(|| home.join(".claude"), PathBuf::from);
    let records = read_logs(&claude.join("projects"));
    println!("{}", serde_json::to_string(&summarize(records, &home, now()))?);
    Ok(())
}

// MARK: - Логи

#[derive(Clone, Copy, Default, Debug, PartialEq, Serialize)]
pub struct Tokens {
    input: u64,
    output: u64,
    cache_write_5m: u64,
    cache_write_1h: u64,
    cache_read: u64,
    searches: u64,
}

impl Tokens {
    fn add(&mut self, other: &Tokens) {
        self.input += other.input;
        self.output += other.output;
        self.cache_write_5m += other.cache_write_5m;
        self.cache_write_1h += other.cache_write_1h;
        self.cache_read += other.cache_read;
        self.searches += other.searches;
    }

    /// Одна и та же запись ответа в разных строках: где-то ещё нули, где-то
    /// вывод дописан не до конца — берём наибольшее.
    fn max(self, other: Tokens) -> Tokens {
        Tokens {
            input: self.input.max(other.input),
            output: self.output.max(other.output),
            cache_write_5m: self.cache_write_5m.max(other.cache_write_5m),
            cache_write_1h: self.cache_write_1h.max(other.cache_write_1h),
            cache_read: self.cache_read.max(other.cache_read),
            searches: self.searches.max(other.searches),
        }
    }
}

/// Один ответ Claude.
#[derive(Clone, Debug)]
struct Record {
    at: i64,
    session: String,
    cwd: String,
    model: String,
    /// Ответ субагента, а не основного Claude.
    subagent: bool,
    fast: bool,
    tokens: Tokens,
}

#[derive(Deserialize)]
struct Line {
    #[serde(rename = "type")]
    kind: Option<String>,
    timestamp: Option<String>,
    #[serde(rename = "sessionId")]
    session: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "requestId")]
    request: Option<String>,
    uuid: Option<String>,
    #[serde(rename = "isSidechain")]
    sidechain: Option<bool>,
    message: Option<Message>,
    #[serde(rename = "aiTitle")]
    title: Option<String>,
}

#[derive(Deserialize)]
struct Message {
    id: Option<String>,
    model: Option<String>,
    usage: Option<Usage>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    cache_creation_input_tokens: Option<u64>,
    cache_read_input_tokens: Option<u64>,
    cache_creation: Option<CacheCreation>,
    server_tool_use: Option<ServerTools>,
    speed: Option<String>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct CacheCreation {
    ephemeral_5m_input_tokens: Option<u64>,
    ephemeral_1h_input_tokens: Option<u64>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ServerTools {
    web_search_requests: Option<u64>,
}

impl Usage {
    fn tokens(&self) -> Tokens {
        let written = self.cache_creation_input_tokens.unwrap_or(0);
        // Без разбивки по сроку — считаем, что на 5 минут: так дешевле.
        let (cache_write_5m, cache_write_1h) = match &self.cache_creation {
            Some(split) => (split.ephemeral_5m_input_tokens.unwrap_or(0), split.ephemeral_1h_input_tokens.unwrap_or(0)),
            None => (written, 0),
        };
        Tokens {
            input: self.input_tokens.unwrap_or(0),
            output: self.output_tokens.unwrap_or(0),
            cache_write_5m,
            cache_write_1h,
            cache_read: self.cache_read_input_tokens.unwrap_or(0),
            searches: self.server_tool_use.as_ref().and_then(|s| s.web_search_requests).unwrap_or(0),
        }
    }
}

/// Что нашлось в одном файле: ответы (с ключом, по которому ловим повторы)
/// и названия сессий.
#[derive(Default)]
struct FileScan {
    /// Имя папки лога — по нему узнаём, где запустили Claude.
    slug: String,
    /// Папка запуска по `cwd` ответа.
    launch_dirs: HashMap<String, Option<String>>,
    /// Последняя известная папка запуска: Claude мог уйти `cd` за пределы проекта.
    launched: Option<String>,
    records: Vec<(Option<String>, Record)>,
    titles: Vec<(String, String)>,
}

impl FileScan {
    /// Папка, где запустили Claude. `cwd` в логе — где он сейчас, после `cd`
    /// это бывает подпапка. Лог лежит в папке, названной путём запуска, где
    /// всё, кроме латиницы и цифр, заменено дефисами: ищем такого предка.
    fn launch_dir(&mut self, cwd: String) -> String {
        let slug = &self.slug;
        let found = self
            .launch_dirs
            .entry(cwd.clone())
            .or_insert_with(|| Path::new(&cwd).ancestors().find(|dir| slugify(dir) == *slug).map(|d| d.display().to_string()))
            .clone();
        if found.is_some() {
            self.launched = found;
        }
        self.launched.clone().unwrap_or(cwd)
    }

    fn line(&mut self, line: &[u8]) {
        // Разбирать каждую строку долго: логи — гигабайты, а нужны только
        // ответы и названия.
        let answer = memmem::find(line, b"\"usage\"").is_some();
        if !answer && memmem::find(line, b"\"aiTitle\"").is_none() {
            return;
        }
        let Ok(line) = serde_json::from_slice::<Line>(line) else { return };
        match line.kind.as_deref() {
            Some("ai-title") => {
                if let (Some(session), Some(title)) = (line.session, line.title) {
                    self.titles.push((session, title));
                }
            }
            Some("assistant") => {
                let Some(message) = line.message else { return };
                let (Some(usage), Some(model)) = (message.usage, message.model) else { return };
                // `<synthetic>` — сообщения самого Claude Code, без модели.
                let Some(at) = line.timestamp.as_deref().and_then(parse_time) else { return };
                if model.starts_with('<') {
                    return;
                }
                let key = match (message.id, line.request) {
                    (Some(id), Some(request)) => Some(format!("{id}|{request}")),
                    _ => line.uuid,
                };
                let record = Record {
                    at,
                    session: line.session.unwrap_or_default(),
                    cwd: self.launch_dir(line.cwd.unwrap_or_default()),
                    model,
                    subagent: line.sidechain.unwrap_or(false),
                    fast: usage.speed.as_deref() == Some("fast"),
                    tokens: usage.tokens(),
                };
                self.records.push((key, record));
            }
            _ => {}
        }
    }
}

fn scan_file(path: &Path, projects: &Path) -> FileScan {
    let slug = path.strip_prefix(projects).ok().and_then(|p| p.iter().next()).map(|s| s.to_string_lossy().into_owned());
    let mut scan = FileScan { slug: slug.unwrap_or_default(), ..FileScan::default() };
    let Ok(file) = File::open(path) else { return scan };
    let mut reader = BufReader::with_capacity(1 << 20, file);
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => scan.line(&line),
        }
    }
    scan
}

fn log_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => dirs.push(path),
                Ok(kind) if kind.is_file() && path.extension().is_some_and(|e| e == "jsonl") => files.push(path),
                _ => {}
            }
        }
    }
    files.sort();
    files
}

/// Все ответы из логов, каждый один раз. Файлы читаем в несколько потоков.
fn read_logs(dir: &Path) -> Records {
    let files = log_files(dir);
    let next = AtomicUsize::new(0);
    let threads = thread::available_parallelism().map_or(4, |n| n.get()).min(8);
    let mut scans: Vec<(usize, FileScan)> = thread::scope(|scope| {
        let workers: Vec<_> = (0..threads)
            .map(|_| {
                scope.spawn(|| {
                    let mut done = Vec::new();
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(file) = files.get(index) else { break };
                        done.push((index, scan_file(file, dir)));
                    }
                    done
                })
            })
            .collect();
        workers.into_iter().flat_map(|worker| worker.join().unwrap_or_default()).collect()
    });
    // Порядок файлов — как на диске, чтобы повтор ответа всегда склеивался
    // с одной и той же записью.
    scans.sort_by_key(|(index, _)| *index);
    merge(scans.into_iter().map(|(_, scan)| scan))
}

struct Records {
    list: Vec<Record>,
    titles: HashMap<String, String>,
}

/// Один ответ Claude пишет несколькими строками, а продолженная сессия
/// повторяет старые ответы в новом файле — считаем каждый ответ один раз.
fn merge(scans: impl Iterator<Item = FileScan>) -> Records {
    let mut list: Vec<Record> = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut titles = HashMap::new();
    for scan in scans {
        for (key, record) in scan.records {
            if let Some(key) = key {
                if let Some(&index) = seen.get(&key) {
                    list[index].tokens = list[index].tokens.max(record.tokens);
                    continue;
                }
                seen.insert(key, list.len());
            }
            list.push(record);
        }
        titles.extend(scan.titles);
    }
    list.sort_by_key(|record| record.at);
    Records { list, titles }
}

/// Как Claude Code называет папку лога: `/Users/me/a.b` → `-Users-me-a-b`.
fn slugify(dir: &Path) -> String {
    dir.to_string_lossy().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect()
}

/// `2026-09-28T02:43:21.395Z` → секунды Unix.
fn parse_time(text: &str) -> Option<i64> {
    let number = |from: usize, to: usize| text.get(from..to)?.parse::<i32>().ok();
    // SAFETY: tm — простая структура из чисел и указателя, нули допустимы.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    tm.tm_year = number(0, 4)? - 1900;
    tm.tm_mon = number(5, 7)? - 1;
    tm.tm_mday = number(8, 10)?;
    tm.tm_hour = number(11, 13)?;
    tm.tm_min = number(14, 16)?;
    tm.tm_sec = number(17, 19)?;
    // SAFETY: timegm читает и нормализует только переданную структуру.
    let at = unsafe { libc::timegm(&mut tm) };
    (at != -1).then_some(at as i64)
}

// MARK: - Цены и имена

/// `claude-haiku-4-5-20251001`, `claude-opus-5[1m]` → `haiku-4-5`, `opus-5`.
fn short_model(model: &str) -> &str {
    let model = model.strip_prefix("claude-").unwrap_or(model);
    let model = model.split('[').next().unwrap_or(model);
    match model.rsplit_once('-') {
        Some((rest, date)) if date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) => rest,
        _ => model,
    }
}

fn price(model: &str) -> Option<&'static Price> {
    let short = short_model(model);
    PRICES.iter().find(|(id, _)| *id == short).map(|(_, price)| price)
}

/// `claude-opus-5-5` → «Opus 5.5».
fn model_name(model: &str) -> String {
    let short = short_model(model);
    let mut parts = short.split('-');
    let family = parts.next().unwrap_or_default();
    let version: Vec<&str> = parts.collect();
    let mut letters = family.chars();
    let Some(first) = letters.next() else { return model.to_string() };
    if version.is_empty() || !version.iter().all(|p| p.bytes().all(|b| b.is_ascii_digit())) {
        return model.to_string();
    }
    format!("{}{} {}", first.to_uppercase(), letters.as_str(), version.join("."))
}

/// Во что обошёлся бы ответ по ценам API, по видам токенов.
#[derive(Clone, Copy, Default, Debug, PartialEq, Serialize)]
pub struct Costs {
    input: f64,
    output: f64,
    cache_write: f64,
    cache_read: f64,
    searches: f64,
}

impl Costs {
    fn of(record: &Record) -> Costs {
        let Some(price) = price(&record.model) else { return Costs::default() };
        let t = &record.tokens;
        let per_token = if record.fast { 2.0 } else { 1.0 } / 1e6;
        Costs {
            input: t.input as f64 * price.input * per_token,
            output: t.output as f64 * price.output * per_token,
            cache_write: (t.cache_write_5m as f64 * 1.25 + t.cache_write_1h as f64 * 2.0) * price.input * per_token,
            cache_read: t.cache_read as f64 * price.cache_read * per_token,
            searches: t.searches as f64 * SEARCH_PRICE,
        }
    }

    /// Без кэша: всё, что записано в кэш и прочитано из него, — по цене
    /// обычного ввода.
    fn without_cache(record: &Record) -> f64 {
        let Some(price) = price(&record.model) else { return 0.0 };
        let t = &record.tokens;
        let per_token = if record.fast { 2.0 } else { 1.0 } / 1e6;
        let read = t.input + t.cache_write_5m + t.cache_write_1h + t.cache_read;
        (read as f64 * price.input + t.output as f64 * price.output) * per_token + t.searches as f64 * SEARCH_PRICE
    }

    fn total(&self) -> f64 {
        self.input + self.output + self.cache_write + self.cache_read + self.searches
    }

    fn add(&mut self, other: &Costs) {
        self.input += other.input;
        self.output += other.output;
        self.cache_write += other.cache_write;
        self.cache_read += other.cache_read;
        self.searches += other.searches;
    }
}

/// Проект по папке сессии: имя и путь (`~/Projects/erp`). Копия проекта от
/// vv (`~/.vibeterminal/worktrees/<проект>/<ветка>`) — это её проект, путь
/// у неё не показываем.
fn project(cwd: &str, home: &Path) -> (String, Option<String>) {
    let path = Path::new(cwd);
    if let Some(name) = path.strip_prefix(worktree::root(home)).ok().and_then(|rest| rest.iter().next()) {
        return (name.to_string_lossy().into_owned(), None);
    }
    let shown = match path.strip_prefix(home) {
        Ok(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => cwd.to_string(),
    };
    let name = if shown == "~" { shown.clone() } else { path.file_name().map_or(cwd.into(), |n| n.to_string_lossy().into_owned()) };
    (name, Some(shown))
}

// MARK: - Сводка

#[derive(Serialize)]
pub struct Stats {
    /// Самый ранний ответ в логах: что раньше, Claude уже удалил.
    since: Option<i64>,
    generated: i64,
    periods: Vec<Period>,
}

#[derive(Serialize)]
pub struct Period {
    key: &'static str,
    title: &'static str,
    from: i64,
    to: i64,
    /// Столбики графика: `hour` за сегодня, `day` за остальные периоды.
    unit: &'static str,
    cost: f64,
    /// Сколько стоило бы без кэша — разница и есть экономия.
    cost_without_cache: f64,
    subagent_cost: f64,
    costs: Costs,
    tokens: Tokens,
    responses: u64,
    sessions: usize,
    active_days: usize,
    models: Vec<ModelRow>,
    projects: Vec<ProjectRow>,
    top_sessions: Vec<SessionRow>,
    chart: Vec<ChartPoint>,
}

#[derive(Serialize)]
struct ModelRow {
    id: String,
    name: String,
    /// Модели нет в таблице цен — у неё только токены.
    priced: bool,
    cost: f64,
    responses: u64,
    tokens: Tokens,
}

#[derive(Serialize)]
struct ProjectRow {
    name: String,
    path: String,
    cost: f64,
    responses: u64,
    sessions: usize,
    output: u64,
    last: i64,
}

#[derive(Serialize)]
struct SessionRow {
    id: String,
    title: String,
    project: String,
    cost: f64,
    responses: u64,
    first: i64,
    last: i64,
}

#[derive(Serialize)]
struct ChartPoint {
    /// Начало часа или дня.
    at: i64,
    model: String,
    cost: f64,
}

/// Начало часа и дня по местному времени. Часовые пояса сдвинуты на число,
/// кратное 15 минутам, — внутри такого отрезка час и день одни и те же,
/// поэтому считаем один раз на отрезок.
#[derive(Default)]
struct Clock {
    starts: HashMap<i64, (i64, i64)>,
}

impl Clock {
    fn starts(&mut self, at: i64) -> (i64, i64) {
        let slot = at.div_euclid(900);
        *self.starts.entry(slot).or_insert_with(|| {
            let Some(tm) = local(slot * 900) else { return (at, at) };
            let hour = make_time(libc::tm { tm_min: 0, tm_sec: 0, tm_isdst: -1, ..tm }).unwrap_or(at);
            let day = make_time(libc::tm { tm_hour: 0, tm_min: 0, tm_sec: 0, tm_isdst: -1, ..tm }).unwrap_or(at);
            (hour, day)
        })
    }
}

/// Полночь `days` дней назад.
fn midnight(now: i64, days: i32) -> i64 {
    let Some(tm) = local(now) else { return now };
    make_time(libc::tm { tm_mday: tm.tm_mday - days, tm_hour: 0, tm_min: 0, tm_sec: 0, tm_isdst: -1, ..tm }).unwrap_or(now)
}

fn summarize(records: Records, home: &Path, now: i64) -> Stats {
    let mut clock = Clock::default();
    let first_day = records.list.first().map(|r| clock.starts(r.at).1);
    let periods = [
        ("today", "Сегодня", midnight(now, 0), "hour"),
        ("week", "7 дней", midnight(now, 6), "day"),
        ("month", "30 дней", midnight(now, 29), "day"),
        ("all", "Всё время", first_day.unwrap_or_else(|| midnight(now, 0)), "day"),
    ];
    let mut projects = HashMap::new();
    let periods = periods
        .into_iter()
        .map(|(key, title, from, unit)| {
            let mut period = Period::new(key, title, from, now, unit);
            let list = records.list.iter().filter(|r| r.at >= from);
            period.fill(list, &records.titles, &mut clock, &mut |cwd| {
                projects.entry(cwd.to_string()).or_insert_with(|| project(cwd, home)).clone()
            });
            period
        })
        .collect();
    Stats { since: records.list.first().map(|r| r.at), generated: now, periods }
}

impl Period {
    fn new(key: &'static str, title: &'static str, from: i64, to: i64, unit: &'static str) -> Period {
        Period {
            key,
            title,
            from,
            to,
            unit,
            cost: 0.0,
            cost_without_cache: 0.0,
            subagent_cost: 0.0,
            costs: Costs::default(),
            tokens: Tokens::default(),
            responses: 0,
            sessions: 0,
            active_days: 0,
            models: Vec::new(),
            projects: Vec::new(),
            top_sessions: Vec::new(),
            chart: Vec::new(),
        }
    }

    fn fill<'a>(
        &mut self,
        records: impl Iterator<Item = &'a Record>,
        titles: &HashMap<String, String>,
        clock: &mut Clock,
        project_of: &mut dyn FnMut(&str) -> (String, Option<String>),
    ) {
        // По имени: `haiku-4-5` и `haiku-4-5-20251001` — одна модель.
        let mut models: HashMap<String, ModelRow> = HashMap::new();
        let mut projects: HashMap<String, (ProjectRow, HashSet<&str>)> = HashMap::new();
        let mut sessions: HashMap<&str, SessionRow> = HashMap::new();
        let mut chart: HashMap<(i64, String), f64> = HashMap::new();
        let mut days = HashSet::new();
        for record in records {
            let costs = Costs::of(record);
            let cost = costs.total();
            self.costs.add(&costs);
            self.tokens.add(&record.tokens);
            self.cost_without_cache += Costs::without_cache(record);
            self.responses += 1;
            if record.subagent {
                self.subagent_cost += cost;
            }
            let (hour, day) = clock.starts(record.at);
            days.insert(day);
            let name = model_name(&record.model);
            *chart.entry((if self.unit == "hour" { hour } else { day }, name.clone())).or_default() += cost;

            let model = models.entry(name.clone()).or_insert_with(|| ModelRow {
                id: record.model.clone(),
                name,
                priced: price(&record.model).is_some(),
                cost: 0.0,
                responses: 0,
                tokens: Tokens::default(),
            });
            model.cost += cost;
            model.responses += 1;
            model.tokens.add(&record.tokens);

            let (project_name, path) = project_of(&record.cwd);
            let (project, project_sessions) = projects.entry(project_name.clone()).or_insert_with(|| {
                let row = ProjectRow { name: project_name.clone(), path: String::new(), cost: 0.0, responses: 0, sessions: 0, output: 0, last: 0 };
                (row, HashSet::new())
            });
            if project.path.is_empty() {
                project.path = path.unwrap_or_default();
            }
            project.cost += cost;
            project.responses += 1;
            project.output += record.tokens.output;
            project.last = project.last.max(record.at);
            project_sessions.insert(&record.session);

            let session = sessions.entry(&record.session).or_insert_with(|| SessionRow {
                id: record.session.clone(),
                title: titles.get(&record.session).cloned().unwrap_or_else(|| "Без названия".into()),
                project: project_name,
                cost: 0.0,
                responses: 0,
                first: record.at,
                last: record.at,
            });
            session.cost += cost;
            session.responses += 1;
            session.last = session.last.max(record.at);
        }
        self.cost = self.costs.total();
        self.sessions = sessions.len();
        self.active_days = days.len();

        self.models = models.into_values().collect();
        self.models.sort_by(|a, b| b.cost.total_cmp(&a.cost).then(b.responses.cmp(&a.responses)));
        self.projects = projects
            .into_values()
            .map(|(mut row, sessions)| {
                row.sessions = sessions.len();
                row
            })
            .collect();
        self.projects.sort_by(|a, b| b.cost.total_cmp(&a.cost).then(b.responses.cmp(&a.responses)));
        let mut sessions: Vec<SessionRow> = sessions.into_values().collect();
        sessions.sort_by(|a, b| b.cost.total_cmp(&a.cost));
        sessions.truncate(TOP_SESSIONS);
        self.top_sessions = sessions;
        self.chart = chart.into_iter().map(|((at, model), cost)| ChartPoint { at, model, cost }).collect();
        self.chart.sort_by(|a, b| a.at.cmp(&b.at).then_with(|| a.model.cmp(&b.model)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(id: &str, at: &str, output: u64, extra: &str) -> String {
        format!(
            r#"{{"type":"assistant","timestamp":"{at}","sessionId":"s1","cwd":"/Users/me/Projects/erp","requestId":"req_{id}","uuid":"u{id}{output}","isSidechain":false,"message":{{"id":"msg_{id}","model":"claude-opus-5-5","content":[{{"type":"text","text":"\"usage\""}}],"usage":{{"input_tokens":2,"output_tokens":{output},"cache_read_input_tokens":1000000,"cache_creation_input_tokens":500000,"cache_creation":{{"ephemeral_5m_input_tokens":0,"ephemeral_1h_input_tokens":500000}}{extra}}}}}}}"#
        )
    }

    fn scan(lines: &[String]) -> Records {
        let mut file = FileScan { slug: "-Users-me-Projects-erp".into(), ..FileScan::default() };
        for line in lines {
            file.line(line.as_bytes());
        }
        merge(std::iter::once(file))
    }

    #[test]
    fn counts_each_answer_once_with_largest_numbers() {
        let records = scan(&[
            answer("a", "2026-09-28T02:43:21.395Z", 0, ""),
            answer("a", "2026-09-28T02:43:21.395Z", 157, ""),
            answer("a", "2026-09-28T02:43:22.000Z", 157, ""),
            answer("b", "2026-09-28T02:44:00.000Z", 10, ""),
            r#"{"type":"ai-title","aiTitle":"Статистика","sessionId":"s1"}"#.to_string(),
            r#"{"type":"user","message":{"role":"user","content":"\"usage\""}}"#.to_string(),
        ]);
        assert_eq!(records.list.len(), 2);
        assert_eq!(records.list[0].tokens.output, 157);
        assert_eq!(records.list[0].at, 1_790_563_401);
        assert_eq!(records.titles["s1"], "Статистика");
    }

    #[test]
    fn prices_like_claude_code() {
        let records = scan(&[answer("a", "2026-09-28T02:43:21Z", 1000, "")]);
        let costs = Costs::of(&records.list[0]);
        // Opus 5.5: вывод $20, запись в кэш на час $8, чтение $0.20 за миллион.
        assert!((costs.output - 0.02).abs() < 1e-9);
        assert!((costs.cache_write - 4.0).abs() < 1e-9);
        assert!((costs.cache_read - 0.2).abs() < 1e-9);
        assert!((Costs::without_cache(&records.list[0]) - (1_500_002.0 * 4.0 + 1000.0 * 20.0) / 1e6).abs() < 1e-9);

        let fast = scan(&[answer("a", "2026-09-28T02:43:21Z", 1000, r#","speed":"fast","server_tool_use":{"web_search_requests":3}"#)]);
        let costs = Costs::of(&fast.list[0]);
        assert!((costs.output - 0.04).abs() < 1e-9);
        assert!((costs.searches - 0.03).abs() < 1e-9);
    }

    #[test]
    fn names_models() {
        assert_eq!(model_name("claude-opus-5-5"), "Opus 5.5");
        assert_eq!(model_name("claude-haiku-4-5-20251001"), "Haiku 4.5");
        assert_eq!(model_name("claude-fable-5-1"), "Fable 5.1");
        assert_eq!(model_name("claude-opus-5[1m]"), "Opus 5");
        assert_eq!(model_name("gpt-oss"), "gpt-oss");
        assert!(price("claude-haiku-4-5-20251001").is_some());
        assert!(price("claude-opus-5").is_some_and(|p| p.input == 5.0));
        assert!(price("claude-future-9").is_none());
    }

    #[test]
    fn project_is_where_claude_started() {
        let lines = [
            answer("a", "2026-09-28T02:43:21Z", 1, ""),
            answer("b", "2026-09-28T02:44:00Z", 1, "").replace("/Projects/erp", "/Projects/erp/web/src"),
            answer("c", "2026-09-28T02:45:00Z", 1, "").replace("/Users/me/Projects/erp", "/tmp"),
        ];
        let records = scan(&lines);
        let dirs: Vec<_> = records.list.iter().map(|r| r.cwd.as_str()).collect();
        assert_eq!(dirs, ["/Users/me/Projects/erp"; 3]);
        assert_eq!(slugify(Path::new("/Users/me/.vibeterminal/worktrees/a.b/x")), "-Users-me--vibeterminal-worktrees-a-b-x");
    }

    #[test]
    fn copies_belong_to_their_project() {
        let home = Path::new("/Users/me");
        assert_eq!(project("/Users/me/Projects/erp", home), ("erp".into(), Some("~/Projects/erp".into())));
        assert_eq!(project("/Users/me/.vibeterminal/worktrees/erp/fix-login", home), ("erp".into(), None));
        assert_eq!(project("/Users/me", home), ("~".into(), Some("~".into())));
        assert_eq!(project("/private/tmp", home), ("tmp".into(), Some("/private/tmp".into())));
    }

    #[test]
    fn splits_into_periods() {
        let now = parse_time("2026-09-30T12:00:00Z").unwrap();
        let mut lines = vec![answer("old", "2026-08-01T12:00:00Z", 100, ""), answer("today", "2026-09-30T11:00:00Z", 100, "")];
        let mut sub = answer("sub", "2026-09-29T12:00:00Z", 100, "");
        sub = sub.replace(r#""isSidechain":false"#, r#""isSidechain":true"#).replace("/Projects/erp", "/.vibeterminal/worktrees/erp/x");
        lines.push(sub);
        let stats = summarize(scan(&lines), Path::new("/Users/me"), now);
        let responses: Vec<_> = stats.periods.iter().map(|p| (p.key, p.responses)).collect();
        assert_eq!(responses, [("today", 1), ("week", 2), ("month", 2), ("all", 3)]);
        let week = &stats.periods[1];
        assert_eq!(week.projects.len(), 1);
        assert_eq!((week.projects[0].name.as_str(), week.projects[0].path.as_str()), ("erp", "~/Projects/erp"));
        assert_eq!(week.active_days, 2);
        assert!(week.subagent_cost > 0.0 && week.subagent_cost < week.cost);
        assert_eq!(week.top_sessions[0].title, "Без названия");
        assert_eq!(week.chart.len(), 2);
        assert!((week.chart.iter().map(|p| p.cost).sum::<f64>() - week.cost).abs() < 1e-9);
    }
}
