mod app;
mod keys;
mod mouse;
mod picker;
mod session;
mod ui;
mod view;

const HELP: &str = "\
vv — Vibe Vim, терминальный пульт для вайбкодинга

Использование:
  vv              открыть Claude в текущей папке
  vv --version    версия

Внутри:
  Ctrl-\\          меню (режим NORMAL), там ? — все клавиши
  Ctrl-\\ n        новая сессия
  Ctrl-\\ q        выйти (все Claude остановятся)
";

fn main() -> anyhow::Result<()> {
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-V" | "--version" => println!("vv {}", env!("CARGO_PKG_VERSION")),
            "-h" | "--help" => print!("{HELP}"),
            other => {
                eprintln!("vv: неизвестный аргумент «{other}»\n\n{HELP}");
                std::process::exit(2);
            }
        }
        return Ok(());
    }
    app::run()
}
