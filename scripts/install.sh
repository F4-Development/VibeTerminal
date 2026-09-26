#!/bin/bash
# Установка VibeTerminal одной командой:
#
#   curl -fsSL https://raw.githubusercontent.com/F4-Development/VibeTerminal/main/scripts/install.sh | bash
#
# Скачивает последний релиз, сверяет контрольную сумму и ставит приложение
# в «Программы». Дальше VibeTerminal обновляется сам.
set -euo pipefail

REPO="F4-Development/VibeTerminal"
DEST="${VIBETERMINAL_INSTALL_DIR:-/Applications}"
TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

fail() {
    echo "✕ $*" >&2
    exit 1
}

[[ "$(uname -s)" == Darwin && "$(uname -m)" == arm64 ]] || fail "VibeTerminal работает на Mac с Apple Silicon"

echo "→ ищу последнюю версию"
curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" -o "$TMP/release.json" \
    || fail "не удалось связаться с GitHub"
field() { plutil -extract "$1" raw -o - "$TMP/release.json" 2>/dev/null; }
VERSION="$(field tag_name)"
URL="" DIGEST=""
for ((i = 0; i < $(field assets); i++)); do
    if [[ "$(field "assets.$i.name")" == *.zip ]]; then
        URL="$(field "assets.$i.browser_download_url")"
        DIGEST="$(field "assets.$i.digest" || true)"
        break
    fi
done
[[ -n "$URL" ]] || fail "в релизе $VERSION нет архива приложения"

echo "→ скачиваю VibeTerminal ${VERSION#v}"
curl -fL --progress-bar "$URL" -o "$TMP/VibeTerminal.zip" || fail "не скачалось"
if [[ "$DIGEST" == sha256:* ]]; then
    [[ "$(shasum -a 256 "$TMP/VibeTerminal.zip" | cut -d' ' -f1)" == "${DIGEST#sha256:}" ]] \
        || fail "архив повреждён — контрольная сумма не сходится"
fi

ditto -x -k "$TMP/VibeTerminal.zip" "$TMP/app"
APP="$TMP/app/VibeTerminal.app"
[[ -d "$APP" ]] || fail "в архиве нет приложения"
xattr -cr "$APP"
codesign --verify --deep --strict "$APP" 2>/dev/null || fail "подпись приложения не сходится"

[[ -w "$DEST" ]] || fail "нет прав на запись в $DEST — запусти так: VIBETERMINAL_INSTALL_DIR=~/Applications bash"
RUNNING=""
pgrep -qf "$DEST/VibeTerminal.app/Contents/MacOS/" && RUNNING=1
# Сначала рядом, потом подмена: не вышло скопировать — старая версия цела.
NEW="$DEST/.VibeTerminal.app.new" OLD="$DEST/.VibeTerminal.app.old"
rm -rf "$NEW" "$OLD"
ditto "$APP" "$NEW"
[[ -d "$DEST/VibeTerminal.app" ]] && mv "$DEST/VibeTerminal.app" "$OLD"
mv "$NEW" "$DEST/VibeTerminal.app"
rm -rf "$OLD"
echo "✓ VibeTerminal ${VERSION#v} установлен в $DEST"

command -v claude >/dev/null || echo "  Нужен Claude Code: https://docs.anthropic.com/en/docs/claude-code"
if [[ -n "$RUNNING" ]]; then
    echo "  VibeTerminal открыт — новая версия запустится после перезапуска"
else
    open "$DEST/VibeTerminal.app"
fi
