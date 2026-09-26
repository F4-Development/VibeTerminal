# VibeTerminal

Терминал для вайбкодинга с Claude Code под macOS. В одном окне работают несколько Claude, видно, кто работает и кто ждёт; на запросы разрешений отвечаешь кнопками. Интерфейс на русском.

## Из чего состоит

| Папка | Что там |
|---|---|
| `src/` | `vv` — всё, что внутри окна: сессии, меню, кнопки, разрешения (Rust) |
| `terminal/` | Приложение VibeTerminal — форк [Ghostty](https://ghostty.org) (Zig + Swift, MIT) |
| `assets/` | Иконка и русские тексты системных окон macOS |
| `scripts/build-app.sh` | Сборка `VibeTerminal.app` |
| `TZ.md` | Техническое задание и план по шагам |

## Сборка

Нужны Rust, Xcode 26 и Zig 0.15.2.

```sh
scripts/build-app.sh            # → dist/VibeTerminal.app
scripts/build-app.sh --install  # и в /Applications
```

Только `vv`, чтобы запускать в любом терминале:

```sh
cargo install --path .
vv
```

## Лицензии

`terminal/` основан на Ghostty и распространяется по его лицензии MIT (`terminal/LICENSE`).
