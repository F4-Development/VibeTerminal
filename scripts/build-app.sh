#!/bin/bash
# Сборка VibeTerminal.app: терминал (terminal/, форк Ghostty) и vv внутри.
#
#   scripts/build-app.sh            → dist/VibeTerminal.app
#   scripts/build-app.sh --install  → и в /Applications
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ZIG="${ZIG:-$HOME/.local/share/zig/zig-aarch64-macos-0.15.2/zig}"
OUT="$ROOT/dist/VibeTerminal.app"

echo "→ собираю vv"
(cd "$ROOT" && cargo build --release --quiet)

echo "→ собираю терминал (Zig + Xcode, первый раз долго)"
(cd "$ROOT/terminal" && "$ZIG" build -Doptimize=ReleaseFast)

echo "→ упаковываю"
rm -rf "$OUT"
mkdir -p "$ROOT/dist"
cp -R "$ROOT/terminal/zig-out/Ghostty.app" "$OUT"
cp "$ROOT/target/release/vv" "$OUT/Contents/MacOS/vv"
# Имя в строке меню берётся из CFBundleName, а Xcode ставит туда имя цели
# (Ghostty). Меняем в готовом приложении, чтобы не трогать сборку Ghostty.
/usr/libexec/PlistBuddy -c "Set :CFBundleName VibeTerminal" "$OUT/Contents/Info.plist"

# Подпись для запуска на этом маке: сначала vv, потом всё приложение,
# сохраняя entitlements, с которыми его подписал Xcode.
codesign --force --sign - "$OUT/Contents/MacOS/vv"
codesign --force --sign - --preserve-metadata=entitlements,requirements,flags,runtime "$OUT"
codesign --verify --deep --strict "$OUT"
echo "готово: $OUT"

if [[ "${1:-}" == "--install" ]]; then
    ditto "$OUT" /Applications/VibeTerminal.app
    # ditto переносит служебные атрибуты, с ними проверка подписи не проходит.
    xattr -cr /Applications/VibeTerminal.app
    codesign --verify --deep --strict /Applications/VibeTerminal.app
    echo "установлено: /Applications/VibeTerminal.app"
fi
