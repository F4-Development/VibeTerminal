mod app;
mod git;
mod gitui;
mod caps;
mod ci;
mod hooks;
mod keys;
mod menu;
mod mouse;
mod notify;
mod picker;
mod reload;
mod saved;
mod session;
mod settings;
mod status;
mod ui;
mod usage;
mod userenv;
mod view;
mod voice;

const HELP: &str = "\
vv — VibeTerminal, терминальный пульт для вайбкодинга

Использование:
  vv              открыть Claude в текущей папке
  vv --version    версия

Внутри:
  всё, что печатаешь, уходит в Claude
  Ctrl-\\          меню: сессии, новая, закрыть, выход
  мышь           клик по сессиям и кнопкам
";

fn main() -> anyhow::Result<()> {
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-V" | "--version" => println!("vv {}", env!("CARGO_PKG_VERSION")),
            "-h" | "--help" => print!("{HELP}"),
            // Зовёт Claude Code из хуков, руками не нужен.
            "hook" => hooks::run_hook(&std::env::args().nth(2).unwrap_or_default()),
            other => {
                eprintln!("vv: неизвестный аргумент «{other}»\n\n{HELP}");
                std::process::exit(2);
            }
        }
        return Ok(());
    }
    userenv::ensure_path();
    app::run()
}
