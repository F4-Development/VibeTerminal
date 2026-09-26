//! Headless-прогон `vv` для проверки: запускает его в PTY, жмёт клавиши
//! и печатает, что на экране.
//!
//! cargo run --example pty_drive -- <папка> 'wait:3000' 'type:привет' 'key:enter' 'dump'

use std::io::{Read, Write};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use portable_pty::{CommandBuilder, PtySize, native_pty_system};

#[derive(Default)]
struct Replies(Vec<u8>);

impl vt100::Callbacks for Replies {
    fn unhandled_csi(&mut self, _: &mut vt100::Screen, i1: Option<u8>, _: Option<u8>, _: &[&[u16]], c: char) {
        if i1.is_none() && c == 'c' {
            self.0.extend_from_slice(b"\x1b[?62;22c");
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("папка");
    let (rows, cols) = (32, 110);
    let pty = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .unwrap();
    // DRIVE_CMD — запустить другую программу вместо vv; DRIVE_RAW — сохранить сырой вывод в файл.
    let program = std::env::var("DRIVE_CMD")
        .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string() + "/target/debug/vv");
    let mut raw = std::env::var("DRIVE_RAW").ok().map(|path| std::fs::File::create(path).unwrap());
    let mut cmd = CommandBuilder::new(program);
    for var in ["CLAUDECODE", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_ENTRYPOINT"] {
        cmd.env_remove(var);
    }
    cmd.cwd(&dir);
    cmd.env("TERM", "xterm-256color");
    if let Ok(script) = std::env::var("VV_CLAUDE") {
        cmd.env("VV_CLAUDE", script);
    }
    let mut child = pty.slave.spawn_command(cmd).unwrap();
    drop(pty.slave);

    let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(rows, cols, 0, Replies::default())));
    let writer = Arc::new(Mutex::new(pty.master.take_writer().unwrap()));
    {
        let parser = parser.clone();
        let writer = writer.clone();
        let mut reader = pty.master.try_clone_reader().unwrap();
        std::thread::spawn(move || {
            let mut buf = [0u8; 8192];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 {
                    break;
                }
                if let Some(file) = raw.as_mut() {
                    file.write_all(&buf[..n]).unwrap();
                }
                let mut p = parser.lock().unwrap();
                p.process(&buf[..n]);
                let replies = std::mem::take(&mut p.callbacks_mut().0);
                if !replies.is_empty() {
                    writer.lock().unwrap().write_all(&replies).unwrap();
                }
            }
        });
    }

    let send = |bytes: &[u8]| {
        let mut w = writer.lock().unwrap();
        w.write_all(bytes).unwrap();
        w.flush().unwrap();
    };
    for step in args {
        let (op, arg) = step.split_once(':').unwrap_or((step.as_str(), ""));
        match op {
            "wait" => std::thread::sleep(Duration::from_millis(arg.parse().unwrap())),
            "type" => send(arg.as_bytes()),
            "paste" => send(format!("\x1b[200~{}\x1b[201~", arg.replace("\\n", "\n")).as_bytes()),
            "key" => send(match arg {
                "enter" => b"\r",
                "esc" => b"\x1b",
                "prefix" => b"\x1c",
                "wheelup" => b"\x1b[<64;10;10M",
                "wheeldown" => b"\x1b[<65;10;10M",
                "pgup" => b"\x1b[5~",
                other => panic!("неизвестная клавиша {other}"),
            }),
            "resize" => {
                let (r, c) = arg.split_once('x').unwrap();
                let (r, c): (u16, u16) = (r.parse().unwrap(), c.parse().unwrap());
                pty.master.resize(PtySize { rows: r, cols: c, pixel_width: 0, pixel_height: 0 }).unwrap();
                parser.lock().unwrap().screen_mut().set_size(r, c);
            }
            "dump" => {
                let p = parser.lock().unwrap();
                println!("──── экран ({arg}) ────");
                for line in p.screen().contents().lines() {
                    println!("│{}", line.trim_end());
                }
            }
            other => panic!("неизвестный шаг {other}"),
        }
    }
    std::thread::sleep(Duration::from_millis(300));
    match child.try_wait().unwrap() {
        Some(status) => println!("vv завершился, код {}", status.exit_code()),
        None => {
            println!("vv всё ещё работает — убиваю");
            let _ = child.kill();
        }
    }
}
