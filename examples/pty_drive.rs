//! Headless-прогон `vv` для проверки: запускает его в PTY, жмёт клавиши
//! и печатает, что на экране.
//!
//! cargo run --example pty_drive -- <папка> 'wait:3000' 'type:привет' 'key:enter' 'dump'
//!
//! DRIVE_CMD — запустить другую программу вместо vv; DRIVE_RAW — сохранить
//! сырой вывод в файл. `hangup` закрывает терминал, как закрытие вкладки, и
//! проверяет, что все процессы внутри умерли.

use std::io::{Read, Write};
use std::process::Command;
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

fn descendants(pid: u32) -> Vec<u32> {
    let out = Command::new("pgrep").args(["-P", &pid.to_string()]).output().unwrap();
    let children: Vec<u32> = String::from_utf8_lossy(&out.stdout).lines().filter_map(|l| l.parse().ok()).collect();
    children.iter().flat_map(|&c| std::iter::once(c).chain(descendants(c))).collect()
}

fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as i32, 0) == 0 }
}

fn key_bytes(name: &str) -> Vec<u8> {
    if let Some(letter) = name.strip_prefix("ctrl-") {
        return vec![letter.as_bytes()[0] & 0x1f];
    }
    match name {
        "enter" => b"\r".to_vec(),
        "esc" => b"\x1b".to_vec(),
        "tab" => b"\t".to_vec(),
        "prefix" => b"\x1c".to_vec(),
        "up" => b"\x1b[A".to_vec(),
        "down" => b"\x1b[B".to_vec(),
        "backspace" => b"\x7f".to_vec(),
        "wheelup" => b"\x1b[<64;60;10M".to_vec(),
        "wheeldown" => b"\x1b[<65;60;10M".to_vec(),
        "pgup" => b"\x1b[5~".to_vec(),
        other if other.starts_with("move:") => {
            let (col, row) = other[5..].split_once(',').unwrap();
            format!("\x1b[<35;{col};{row}M").into_bytes()
        }
        other if other.starts_with("click:") => {
            let (col, row) = other[6..].split_once(',').unwrap();
            format!("\x1b[<0;{col};{row}M\x1b[<0;{col};{row}m").into_bytes()
        }
        other => panic!("неизвестная клавиша {other}"),
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("папка");
    let (rows, cols) = (32, 110);
    let pty = native_pty_system()
        .openpty(PtySize { rows, cols, pixel_width: 0, pixel_height: 0 })
        .unwrap();
    let program = std::env::var("DRIVE_CMD")
        .unwrap_or_else(|_| env!("CARGO_MANIFEST_DIR").to_string() + "/target/debug/vv");
    let mut raw = std::env::var("DRIVE_RAW").ok().map(|path| std::fs::File::create(path).unwrap());
    let mut cmd = CommandBuilder::new(program);
    for var in ["CLAUDECODE", "CLAUDE_CODE_CHILD_SESSION", "CLAUDE_CODE_SESSION_ID", "CLAUDE_CODE_ENTRYPOINT"] {
        cmd.env_remove(var);
    }
    cmd.cwd(&dir);
    cmd.env("TERM", "xterm-256color");
    let mut child = pty.slave.spawn_command(cmd).unwrap();
    let vv_pid = child.process_id().unwrap();
    drop(pty.slave);

    let parser = Arc::new(Mutex::new(vt100::Parser::new_with_callbacks(rows, cols, 0, Replies::default())));
    let writer = Arc::new(Mutex::new(Some(pty.master.take_writer().unwrap())));
    let mut master = Some(pty.master);
    {
        let parser = parser.clone();
        let writer = writer.clone();
        let mut reader = master.as_ref().unwrap().try_clone_reader().unwrap();
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
                if let (false, Some(w)) = (replies.is_empty(), writer.lock().unwrap().as_mut()) {
                    let _ = w.write_all(&replies);
                }
            }
        });
    }

    let send = |bytes: &[u8]| {
        if let Some(w) = writer.lock().unwrap().as_mut() {
            w.write_all(bytes).unwrap();
            w.flush().unwrap();
        }
        std::thread::sleep(Duration::from_millis(60));
    };
    for step in args {
        let (op, arg) = step.split_once(':').unwrap_or((step.as_str(), ""));
        match op {
            "wait" => std::thread::sleep(Duration::from_millis(arg.parse().unwrap())),
            "type" => send(arg.as_bytes()),
            "paste" => send(format!("\x1b[200~{}\x1b[201~", arg.replace("\\n", "\n")).as_bytes()),
            "key" => send(&key_bytes(arg)),
            "resize" => {
                let (r, c) = arg.split_once('x').unwrap();
                let (r, c): (u16, u16) = (r.parse().unwrap(), c.parse().unwrap());
                master.as_ref().unwrap().resize(PtySize { rows: r, cols: c, pixel_width: 0, pixel_height: 0 }).unwrap();
                parser.lock().unwrap().screen_mut().set_size(r, c);
            }
            "dump" => {
                let p = parser.lock().unwrap();
                let (row, col) = p.screen().cursor_position();
                let shown = if p.screen().hide_cursor() { "скрыт" } else { "виден" };
                println!("──── экран ({arg}) · курсор {shown}, строка {row}, колонка {col} ────");
                for line in p.screen().contents().lines() {
                    println!("│{}", line.trim_end());
                }
            }
            "cell" => {
                let (col, row) = arg.split_once(',').unwrap();
                let (col, row): (u16, u16) = (col.parse().unwrap(), row.parse().unwrap());
                let p = parser.lock().unwrap();
                let cell = p.screen().cell(row, col).unwrap();
                println!(
                    "клетка {col},{row} «{}»: цвет {:?}, фон {:?}, жирный {}",
                    cell.contents(),
                    cell.fgcolor(),
                    cell.bgcolor(),
                    cell.bold()
                );
            }
            "hangup" => {
                let inside = descendants(vv_pid);
                println!("──── закрываю терминал: vv {vv_pid}, внутри {} процессов {inside:?}", inside.len());
                // Как при закрытии вкладки: ядро шлёт SIGHUP группе процессов на переднем плане.
                unsafe { libc::kill(-(vv_pid as i32), libc::SIGHUP) };
                master.take();
                std::thread::sleep(Duration::from_millis(3000));
                let _ = child.try_wait();
                let survivors: Vec<u32> = std::iter::once(vv_pid).chain(inside).filter(|&p| alive(p)).collect();
                if survivors.is_empty() {
                    println!("всё остановилось");
                } else {
                    let out = Command::new("ps").args(["-o", "pid,stat,command", "-p"])
                        .arg(survivors.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(","))
                        .output().unwrap();
                    println!("ЖИВЫ: {survivors:?}\n{}", String::from_utf8_lossy(&out.stdout));
                }
                return;
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
