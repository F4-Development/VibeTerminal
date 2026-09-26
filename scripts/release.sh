#!/bin/bash
# Выпуск новой версии: сборка, архив и релиз на GitHub. Приложения
# пользователей увидят его и предложат обновиться.
#
#   1. подними version в Cargo.toml и закоммить
#   2. scripts/release.sh [файл с описанием]
#
# Без файла описание — список коммитов с прошлого релиза.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="F4-Development/VibeTerminal"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
TAG="v$VERSION"

gh auth status >/dev/null 2>&1 || { echo "сначала войди в GitHub: gh auth login"; exit 1; }
[[ -z "$(git status --porcelain)" ]] || { echo "есть незакоммиченные изменения"; exit 1; }
git fetch --quiet origin
[[ "$(git rev-parse HEAD)" == "$(git rev-parse origin/main)" ]] || { echo "запушь main: релиз собирается из того, что на GitHub"; exit 1; }
if gh release view "$TAG" --repo "$REPO" >/dev/null 2>&1; then
    echo "релиз $TAG уже есть — подними version в Cargo.toml"
    exit 1
fi

NOTES="$(mktemp)"
trap 'rm -f "$NOTES"' EXIT
if [[ -n "${1:-}" ]]; then
    cp "$1" "$NOTES"
else
    LAST="$(git describe --tags --abbrev=0 2>/dev/null || true)"
    {
        echo "## Что нового"
        git log --no-merges --format='- %s' ${LAST:+"$LAST"..HEAD}
    } >"$NOTES"
fi

"$ROOT/scripts/build-app.sh"

ZIP="$ROOT/dist/VibeTerminal-$VERSION.zip"
rm -f "$ZIP"
# ditto, а не zip: сохраняет подпись и права внутри приложения.
ditto -c -k --keepParent "$ROOT/dist/VibeTerminal.app" "$ZIP"

gh release create "$TAG" "$ZIP" --repo "$REPO" --target "$(git rev-parse HEAD)" \
    --title "VibeTerminal $VERSION" --notes-file "$NOTES"
echo "выпущено: https://github.com/$REPO/releases/tag/$TAG"
