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

## Разработка

```sh
scripts/dev.sh          # собрать vv и подменить его в установленном VibeTerminal
scripts/dev.sh --watch  # и дальше — при каждом изменении src/
```

Открытые окна VibeTerminal сами замечают новую версию `vv` и перезагружаются: сессии с Claude не прерываются, экран остаётся как был. Можно править `vv` прямо из Claude, запущенного в VibeTerminal. Изменения в приложении (Swift, Zig) так не доедут — для них `scripts/build-app.sh --install` и перезапуск.

## Выпуск версии

```sh
# поднять version в Cargo.toml, закоммитить, запушить main
scripts/release.sh              # сборка, VibeTerminal-<версия>.zip, релиз v<версия> на GitHub
scripts/release.sh notes.md     # то же, описание из файла
```

Нужен `gh auth login`. Установленные VibeTerminal увидят релиз при запуске (и раз в 6 часов) и предложат обновиться в один клик.

## Лицензии

`terminal/` основан на Ghostty и распространяется по его лицензии MIT (`terminal/LICENSE`).
