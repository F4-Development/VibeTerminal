#!/bin/bash
# Разработка vv: собрать и подменить vv в установленном VibeTerminal. Все
# открытые окна заметят новую версию и перезагрузятся сами — сессии с Claude
# не прервутся, экран останется как был.
#
#   scripts/dev.sh          собрать и подменить один раз
#   scripts/dev.sh --watch  и дальше — при каждом изменении src/
#
# Изменения в самом приложении (Swift, Zig) так не доедут — для них
# scripts/build-app.sh --install и перезапуск VibeTerminal.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
APP="${VV_APP:-/Applications/VibeTerminal.app}"
cd "$ROOT"
source "$ROOT/scripts/signing.sh"

if [ ! -d "$APP" ]; then
    echo "нет $APP — сначала scripts/build-app.sh --install" >&2
    exit 1
fi

install_vv() {
    if ! cargo build --release --quiet; then
        echo "$(date +%H:%M:%S) сборка не удалась — окна остаются на прежней версии" >&2
        return 1
    fi
    # Новый файл рядом и переименование: работающий vv не видит полузаписанный
    # файл, а у нового свой inode — macOS не спутает подпись со старой.
    local next="$APP/Contents/MacOS/.vv.next"
    cp target/release/vv "$next"
    sign_vv "$next" 2>/dev/null
    mv -f "$next" "$APP/Contents/MacOS/vv"
    echo "$(date +%H:%M:%S) vv обновлён — окна VibeTerminal перезагрузятся за несколько секунд"
}

install_vv || true

if [ "${1:-}" = "--watch" ]; then
    stamp="$(mktemp)"
    trap 'rm -f "$stamp"' EXIT
    echo "слежу за src/ — Ctrl-C, чтобы остановить"
    while sleep 1; do
        if [ -n "$(find src Cargo.toml Cargo.lock -newer "$stamp" -print -quit 2>/dev/null)" ]; then
            touch "$stamp"
            install_vv || true
        fi
    done
fi
