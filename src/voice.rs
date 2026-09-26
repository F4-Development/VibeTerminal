//! Голосовой ввод: запись с микрофона и распознавание Whisper прямо на Mac —
//! звук никуда не уходит. Модель выбирают и качают в настройках
//! VibeTerminal (вкладка «Голос»), файлы — в `~/.vibeterminal/models`.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Sender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, StreamConfig};
use whisper_rs::{FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

/// Whisper ждёт 16 кГц моно.
const WHISPER_RATE: u32 = 16_000;
/// Столбик эквалайзера — громкость за столько.
const LEVEL_WINDOW: f32 = 0.04;
const LEVELS_KEPT: usize = 400;
/// Короче — это случайный клик, а не фраза.
const MIN_SPEECH: Duration = Duration::from_millis(400);
/// Запас окна Whisper сверх длины записи (шагов по 20 мс).
const AUDIO_CTX_MARGIN: i32 = 256;

/// Файл модели по её имени из настроек. Имя — только буквы, цифры, `-`, `_`, `.`.
pub fn model_path(home: &Path, model: &str) -> Option<PathBuf> {
    let valid = !model.is_empty()
        && !model.starts_with('.')
        && model.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    let path = home.join(".vibeterminal/models").join(format!("ggml-{model}.bin"));
    (valid && path.is_file()).then_some(path)
}

/// Идёт запись. Поток микрофона живёт в своём потоке — на macOS его нельзя
/// передавать между потоками.
pub struct Recording {
    stop: Sender<()>,
    levels: Arc<Mutex<VecDeque<f32>>>,
    started: Instant,
    thread: JoinHandle<Result<Captured, String>>,
}

pub struct Captured {
    samples: Vec<f32>,
    rate: u32,
}

impl Recording {
    /// `device` — имя микрофона из настроек, пусто — системный.
    /// `VV_VOICE_FILE` — вместо микрофона «слушать» WAV-файл (для проверки).
    pub fn start(device: &str) -> Result<Self, String> {
        if let Ok(file) = std::env::var("VV_VOICE_FILE") {
            return Self::from_file(Path::new(&file));
        }
        let levels = Arc::new(Mutex::new(VecDeque::with_capacity(LEVELS_KEPT)));
        let (stop, stopped) = mpsc::channel::<()>();
        let (ready, is_ready) = mpsc::channel::<Result<(), String>>();
        let shared = levels.clone();
        let name = device.to_string();
        let thread = thread::spawn(move || {
            let samples = Arc::new(Mutex::new(Vec::<f32>::new()));
            let stream = match open_stream(&name, samples.clone(), shared) {
                Ok((stream, rate)) => {
                    let _ = ready.send(Ok(()));
                    (stream, rate)
                }
                Err(err) => {
                    let _ = ready.send(Err(err.clone()));
                    return Err(err);
                }
            };
            let _ = stopped.recv();
            drop(stream.0);
            let samples = std::mem::take(&mut *samples.lock().unwrap());
            Ok(Captured { samples, rate: stream.1 })
        });
        match is_ready.recv_timeout(Duration::from_secs(5)) {
            Ok(Ok(())) => Ok(Self { stop, levels, started: Instant::now(), thread }),
            Ok(Err(err)) => Err(err),
            Err(_) => Err("микрофон не ответил".into()),
        }
    }

    /// Файл вместо микрофона: звук идёт с настоящей скоростью, эквалайзер живой.
    fn from_file(path: &Path) -> Result<Self, String> {
        let (samples, rate) = read_wav(path)?;
        let levels = Arc::new(Mutex::new(VecDeque::with_capacity(LEVELS_KEPT)));
        let shared = levels.clone();
        let (stop, stopped) = mpsc::channel::<()>();
        let thread = thread::spawn(move || {
            let mut meter = Meter::new(rate);
            let chunk = (rate as f32 * LEVEL_WINDOW) as usize;
            let mut heard = Vec::new();
            for piece in samples.chunks(chunk.max(1)) {
                if stopped.try_recv().is_ok() {
                    return Ok(Captured { samples: heard, rate });
                }
                heard.extend_from_slice(piece);
                if let Some(level) = piece.iter().filter_map(|&s| meter.add(s)).last() {
                    let mut levels = shared.lock().unwrap();
                    if levels.len() == LEVELS_KEPT {
                        levels.pop_front();
                    }
                    levels.push_back(level);
                }
                thread::sleep(Duration::from_secs_f32(LEVEL_WINDOW));
            }
            let _ = stopped.recv();
            Ok(Captured { samples: heard, rate })
        });
        Ok(Self { stop, levels, started: Instant::now(), thread })
    }

    /// Последние `count` уровней громкости, 0…1, старые слева.
    pub fn levels(&self, count: usize) -> Vec<f32> {
        let levels = self.levels.lock().unwrap();
        let skip = levels.len().saturating_sub(count);
        let mut out: Vec<f32> = levels.iter().skip(skip).copied().collect();
        // Только начали — слева тишина.
        while out.len() < count {
            out.insert(0, 0.0);
        }
        out
    }

    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Остановить и забрать записанное.
    pub fn finish(self) -> Result<Captured, String> {
        let _ = self.stop.send(());
        self.thread.join().map_err(|_| "запись оборвалась".to_string())?
    }

    /// Остановить и выбросить.
    pub fn cancel(self) {
        let _ = self.stop.send(());
    }
}

fn open_stream(
    name: &str,
    samples: Arc<Mutex<Vec<f32>>>,
    levels: Arc<Mutex<VecDeque<f32>>>,
) -> Result<(cpal::Stream, u32), String> {
    let host = cpal::default_host();
    let device = if name.is_empty() {
        host.default_input_device()
    } else {
        // Выбранного микрофона нет (отключили) — берём системный.
        host.input_devices()
            .ok()
            .and_then(|mut devices| devices.find(|d| d.to_string() == name))
            .or_else(|| host.default_input_device())
    }
    .ok_or("микрофон не найден")?;
    let supported = device.default_input_config().map_err(|e| format!("микрофон недоступен: {e}"))?;
    let format = supported.sample_format();
    let config: StreamConfig = supported.config();
    let channels = config.channels.max(1) as usize;
    let rate = config.sample_rate;
    let mut meter = Meter::new(rate);
    let mut push = move |mono: &mut dyn Iterator<Item = f32>| {
        let mut buffer = samples.lock().unwrap();
        for sample in mono {
            buffer.push(sample);
            if let Some(level) = meter.add(sample) {
                let mut levels = levels.lock().unwrap();
                if levels.len() == LEVELS_KEPT {
                    levels.pop_front();
                }
                levels.push_back(level);
            }
        }
    };
    let error = |_| {};
    let stream = match format {
        SampleFormat::F32 => device.build_input_stream::<f32, _, _>(
            config,
            move |data, _| push(&mut data.chunks(channels).map(|frame| frame.iter().sum::<f32>() / channels as f32)),
            error,
            None,
        ),
        SampleFormat::I16 => device.build_input_stream::<i16, _, _>(
            config,
            move |data, _| {
                push(&mut data.chunks(channels).map(|frame| {
                    frame.iter().map(|&s| s as f32 / i16::MAX as f32).sum::<f32>() / channels as f32
                }))
            },
            error,
            None,
        ),
        other => return Err(format!("микрофон отдаёт звук в формате {other:?}, такой не умею")),
    }
    .map_err(|e| describe_error(&e.to_string()))?;
    stream.play().map_err(|e| describe_error(&e.to_string()))?;
    Ok((stream, rate))
}

fn describe_error(text: &str) -> String {
    let lower = text.to_lowercase();
    if lower.contains("permission") || lower.contains("denied") || lower.contains("not permitted") {
        "нет доступа к микрофону — разреши в Настройках macOS → Конфиденциальность → Микрофон".into()
    } else {
        format!("микрофон: {text}")
    }
}

/// Громкость кусочками по 40 мс для эквалайзера.
struct Meter {
    window: usize,
    sum: f32,
    count: usize,
}

impl Meter {
    fn new(rate: u32) -> Self {
        Self { window: ((rate as f32 * LEVEL_WINDOW) as usize).max(1), sum: 0.0, count: 0 }
    }

    fn add(&mut self, sample: f32) -> Option<f32> {
        self.sum += sample * sample;
        self.count += 1;
        if self.count < self.window {
            return None;
        }
        let rms = (self.sum / self.count as f32).sqrt();
        self.sum = 0.0;
        self.count = 0;
        // Тишина −55 дБ → 0, громкая речь −10 дБ → 1.
        let db = 20.0 * rms.max(1e-6).log10();
        Some(((db + 55.0) / 45.0).clamp(0.0, 1.0))
    }
}

/// Загруженная модель: грузится сотни мегабайт, держим между диктовками.
static MODEL: Mutex<Option<(PathBuf, WhisperContext)>> = Mutex::new(None);

/// Выгрузить модель. Обязательно перед выходом: Metal в ggml при выходе
/// проверяет, что всё освобождено, и роняет процесс, если нет.
pub fn unload() {
    if let Ok(mut model) = MODEL.lock() {
        *model = None;
    }
}

/// Загрузить модель заранее — пока человек говорит. Первый раз это
/// секунды: файл с диска и сборка шейдеров Metal.
pub fn preload(model: &Path) {
    if let Ok(mut loaded) = MODEL.lock() {
        let _ = load(&mut loaded, model);
    }
}

fn load<'a>(loaded: &'a mut Option<(PathBuf, WhisperContext)>, model: &Path) -> Result<&'a WhisperContext, String> {
    whisper_rs::install_logging_hooks();
    if loaded.as_ref().is_none_or(|(path, _)| path != model) {
        *loaded = None;
        let path = model.to_str().ok_or("странный путь к модели")?;
        let context = WhisperContext::new_with_params(path, WhisperContextParameters::default())
            .map_err(|e| format!("модель не загрузилась: {e}"))?;
        *loaded = Some((model.to_path_buf(), context));
    }
    loaded.as_ref().map(|(_, context)| context).ok_or_else(|| "модели нет".to_string())
}

/// Распознать. Долго (секунды) — звать из фонового потока.
pub fn transcribe(captured: Captured, model: &Path, language: &str, words: &str) -> Result<String, String> {
    let audio = resample(&captured.samples, captured.rate, WHISPER_RATE);
    if audio.len() < (WHISPER_RATE as f32 * MIN_SPEECH.as_secs_f32()) as usize {
        return Err("слишком коротко — скажи ещё раз".into());
    }
    let mut loaded = MODEL.lock().map_err(|_| "модель сломалась".to_string())?;
    let context = load(&mut loaded, model)?;
    let mut state = context.create_state().map_err(|e| format!("Whisper: {e}"))?;

    let mut params = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
    params.set_language(Some(if language.is_empty() { "auto" } else { language }));
    if !words.trim().is_empty() {
        params.set_initial_prompt(words.trim());
    }
    let threads = thread::available_parallelism().map_or(4, |n| n.get()).min(8) as i32;
    params.set_n_threads(threads);
    // Whisper всегда считает окно в 30 с; для короткой фразы его обрезаем —
    // в 3–5 раз быстрее. 50 шагов на секунду плюс запас: с меньшим запасом
    // он повторяет фразу дважды.
    let seconds = audio.len() as f32 / WHISPER_RATE as f32;
    params.set_audio_ctx(((seconds * 50.0) as i32 + AUDIO_CTX_MARGIN).min(1500));
    // Одним куском: с обрезанным окном Whisper иначе повторяет фразу дважды.
    params.set_single_segment(true);
    params.set_no_context(true);
    params.set_no_timestamps(true);
    params.set_suppress_blank(true);
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    state.full(params, &audio).map_err(|e| format!("Whisper: {e}"))?;

    let text: Vec<String> = state
        .as_iter()
        .filter_map(|segment| segment.to_str_lossy().ok().map(|s| s.trim().to_string()))
        .filter(|s| !s.is_empty() && !is_noise(s))
        .collect();
    let text = text.join(" ");
    if text.is_empty() { Err("ничего не расслышал".into()) } else { Ok(text) }
}

/// Whisper подписывает тишину и шум: `[BLANK_AUDIO]`, `(музыка)`, `*кашель*`.
fn is_noise(segment: &str) -> bool {
    let wrapped = |open: char, close: char| segment.starts_with(open) && segment.ends_with(close);
    wrapped('[', ']') || wrapped('(', ')') || wrapped('*', '*')
}

/// WAV: 16-битный PCM или 32-битный float, каналы сводятся в моно.
fn read_wav(path: &Path) -> Result<(Vec<f32>, u32), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err("это не WAV".into());
    }
    let (mut format, mut channels, mut rate, mut bits) = (0u16, 1usize, 0u32, 0u16);
    let mut at = 12;
    while at + 8 <= bytes.len() {
        let id = &bytes[at..at + 4];
        let size = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = &bytes[at + 8..(at + 8 + size).min(bytes.len())];
        if id == b"fmt " && body.len() >= 16 {
            format = u16::from_le_bytes([body[0], body[1]]);
            channels = u16::from_le_bytes([body[2], body[3]]).max(1) as usize;
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
            bits = u16::from_le_bytes([body[14], body[15]]);
        } else if id == b"data" {
            let mono: Vec<f32> = match (format, bits) {
                (1, 16) => body
                    .chunks_exact(2 * channels)
                    .map(|frame| {
                        frame.as_chunks::<2>().0.iter().map(|&s| i16::from_le_bytes(s) as f32 / i16::MAX as f32).sum::<f32>()
                            / channels as f32
                    })
                    .collect(),
                (3, 32) => body
                    .chunks_exact(4 * channels)
                    .map(|frame| {
                        frame.as_chunks::<4>().0.iter().map(|&s| f32::from_le_bytes(s)).sum::<f32>()
                            / channels as f32
                    })
                    .collect(),
                _ => return Err(format!("WAV формата {format}/{bits} бит не умею")),
            };
            return Ok((mono, rate));
        }
        at += 8 + size + size % 2;
    }
    Err("в WAV нет звука".into())
}

/// Простая линейная передискретизация — для речи хватает.
fn resample(samples: &[f32], from: u32, to: u32) -> Vec<f32> {
    if from == to || samples.is_empty() {
        return samples.to_vec();
    }
    let ratio = from as f64 / to as f64;
    let len = (samples.len() as f64 / ratio) as usize;
    (0..len)
        .map(|i| {
            let position = i as f64 * ratio;
            let index = position as usize;
            let fraction = (position - index as f64) as f32;
            let a = samples[index.min(samples.len() - 1)];
            let b = samples[(index + 1).min(samples.len() - 1)];
            a + (b - a) * fraction
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resamples_to_whisper_rate() {
        let second = vec![0.5f32; 48_000];
        let out = resample(&second, 48_000, WHISPER_RATE);
        assert_eq!(out.len(), 16_000);
        assert!(out.iter().all(|&s| (s - 0.5).abs() < 1e-6));
    }

    #[test]
    fn meter_maps_loudness() {
        let mut meter = Meter::new(1000);
        let quiet = (0..40).filter_map(|_| meter.add(0.0001)).last().unwrap();
        let loud = (0..40).filter_map(|_| meter.add(0.3)).last().unwrap();
        assert!(quiet < 0.05 && loud > 0.8, "{quiet} {loud}");
    }

    #[test]
    fn drops_noise_labels() {
        assert!(is_noise("[BLANK_AUDIO]"));
        assert!(is_noise("(музыка)"));
        assert!(!is_noise("Сделай рефакторинг"));
    }

    /// Настоящая речь → текст. Нужны скачанная модель и `say` с русским голосом:
    /// `cargo test -- --ignored transcribes_real_speech`.
    #[test]
    #[ignore]
    fn transcribes_real_speech() {
        let home = PathBuf::from(std::env::var("HOME").unwrap());
        let model = model_path(&home, "large-v3-turbo-q5_0").expect("модель не скачана");
        let dir = std::env::temp_dir();
        let (aiff, wav) = (dir.join("vv-say.aiff"), dir.join("vv-say.wav"));
        let say = std::process::Command::new("say")
            .args(["-v", "Milena", "Сделай рефакторинг модуля авторизации и запусти тесты", "-o"])
            .arg(&aiff)
            .status()
            .unwrap();
        assert!(say.success());
        let convert = std::process::Command::new("afconvert")
            .args(["-f", "WAVE", "-d", "LEI16@16000", "-c", "1"])
            .args([&aiff, &wav])
            .status()
            .unwrap();
        assert!(convert.success());
        let (samples, rate) = read_wav(&wav).unwrap();
        let mut text = String::new();
        for attempt in 1..=2 {
            let started = Instant::now();
            text = transcribe(Captured { samples: samples.clone(), rate }, &model, "ru", "").unwrap();
            eprintln!("{attempt}-й раз за {:?}: {text}", started.elapsed());
        }
        unload();
        let lower = text.to_lowercase();
        assert!(lower.contains("рефакторинг") && lower.contains("тест"), "{text}");
        assert_eq!(lower.matches("рефакторинг").count(), 1, "фраза не должна повторяться: {text}");
    }

    #[test]
    fn model_names_are_safe() {
        let home = std::env::temp_dir().join(format!("vv-voice-{}", std::process::id()));
        let dir = home.join(".vibeterminal/models");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ggml-small.bin"), "x").unwrap();
        assert!(model_path(&home, "small").is_some());
        assert!(model_path(&home, "../small").is_none());
        assert!(model_path(&home, "").is_none());
        assert!(model_path(&home, "large-v3-turbo").is_none(), "не скачана");
        let _ = std::fs::remove_dir_all(&home);
    }
}
