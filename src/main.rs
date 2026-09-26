mod app;
mod keys;
mod mouse;
mod session;
mod ui;
mod view;

const HELP: &str = "\
vv — Vibe Vim, терминальный пульт для вайбкодинга

Использование:
  vv              открыть Claude в текущей папке
  vv --version    версия

Внутри:
  Ctrl-\\          меню (режим NORMAL)
  Ctrl-\\ q        выйти
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
