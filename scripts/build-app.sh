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

# Версия одна на приложение и vv — из Cargo.toml. По ней приложение
# сравнивает себя с последним релизом на GitHub. Номер сборки — число
# коммитов: растёт с каждой версией.
VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' "$ROOT/Cargo.toml" | head -1)"
BUILD="$(git -C "$ROOT" rev-list --count HEAD)"
/usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $VERSION" "$OUT/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleVersion $BUILD" "$OUT/Contents/Info.plist"

# Приложение русскоязычное: системные пункты меню и окна macOS — по-русски,
# тексты запросов доступа (микрофон и т.п.) — из assets/app/ru.lproj.
PLIST="$OUT/Contents/Info.plist"
/usr/libexec/PlistBuddy -c "Delete :CFBundleDevelopmentRegion" "$PLIST" 2>/dev/null || true
/usr/libexec/PlistBuddy -c "Add :CFBundleDevelopmentRegion string ru" "$PLIST"
/usr/libexec/PlistBuddy -c "Delete :CFBundleLocalizations" "$PLIST" 2>/dev/null || true
/usr/libexec/PlistBuddy -c "Add :CFBundleLocalizations array" -c "Add :CFBundleLocalizations:0 string ru" "$PLIST"
cp -R "$ROOT/assets/app/ru.lproj" "$OUT/Contents/Resources/"

# Подпись: Developer ID, если есть сертификат, иначе локальная.
source "$ROOT/scripts/signing.sh"
echo "→ подписываю: ${SIGN_IDENTITY:-локально, только для этого мака}"
sign_app "$OUT"
codesign --verify --deep --strict "$OUT"
echo "готово: $OUT ($VERSION, сборка $BUILD)"

if [[ "${1:-}" == "--install" ]]; then
    ditto "$OUT" /Applications/VibeTerminal.app
    # ditto переносит служебные атрибуты, с ними проверка подписи не проходит.
    xattr -cr /Applications/VibeTerminal.app
    codesign --verify --deep --strict /Applications/VibeTerminal.app
    echo "установлено: /Applications/VibeTerminal.app"
fi
