#!/bin/bash
# Выпуск новой версии: сборка с подписью Developer ID, нотаризация у Apple,
# архив и релиз на GitHub. Приложения пользователей увидят его и предложат
# обновиться.
#
#   1. подними version в Cargo.toml и закоммить, запушь main
#   2. scripts/release.sh [файл с описанием]
#
# Без файла описание — список коммитов с прошлого релиза.
#
# Один раз на маке: сертификат «Developer ID Application» в связке ключей и
# доступ к нотаризации (ключ App Store Connect API):
#   xcrun notarytool store-credentials vibeterminal --key AuthKey_XXXX.p8 --key-id XXXX --issuer <uuid>
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="F4-Development/VibeTerminal"
NOTARY_PROFILE="${NOTARY_PROFILE:-vibeterminal}"
APP="$ROOT/dist/VibeTerminal.app"
cd "$ROOT"

VERSION="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
TAG="v$VERSION"
ZIP="$ROOT/dist/VibeTerminal-$VERSION.zip"

source "$ROOT/scripts/signing.sh"
[[ -n "$SIGN_IDENTITY" ]] || { echo "нет сертификата Developer ID Application — без него macOS не откроет скачанное приложение"; exit 1; }
xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1 \
    || { echo "нет доступа к нотаризации — см. начало scripts/release.sh"; exit 1; }
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

# ditto, а не zip: сохраняет подпись и права внутри приложения.
pack() {
    rm -f "$ZIP"
    ditto -c -k --keepParent "$APP" "$ZIP"
}

echo "→ нотаризация у Apple (обычно пара минут)"
pack
RESULT="$(xcrun notarytool submit "$ZIP" --keychain-profile "$NOTARY_PROFILE" --wait --output-format json)"
STATUS="$(plutil -extract status raw -o - - <<<"$RESULT")"
if [[ "$STATUS" != "Accepted" ]]; then
    ID="$(plutil -extract id raw -o - - <<<"$RESULT")"
    echo "Apple не приняла приложение ($STATUS):"
    xcrun notarytool log "$ID" --keychain-profile "$NOTARY_PROFILE"
    exit 1
fi
# Билет нотаризации — внутрь приложения: открывается и без интернета.
xcrun stapler staple "$APP"
spctl --assess --type execute "$APP"
pack

gh release create "$TAG" "$ZIP" --repo "$REPO" --target "$(git rev-parse HEAD)" \
    --title "VibeTerminal $VERSION" --notes-file "$NOTES"
echo "выпущено: https://github.com/$REPO/releases/tag/$TAG"
