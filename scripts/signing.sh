# Подпись VibeTerminal — подключается из build-app.sh, dev.sh и release.sh
# (нужен $ROOT).
#
# Сертификат — первый, что найдётся в связке ключей:
#   1. «Developer ID Application» — с меткой времени: так требует нотаризация,
#      и macOS открывает скачанное приложение без предупреждений.
#   2. «Apple Development» команды $SIGN_TEAM — для сборок на этом маке. В
#      релизы не идёт (release.sh ставит SIGN_RELEASE=1): нотаризацию такая
#      подпись не проходит, а в ней виден личный email.
#   3. Локальная подпись, только для этого мака.
# С сертификатом — hardened runtime, и разрешения (микрофон) не слетают после
# пересборки и обновления: macOS узнаёт приложение по сертификату, а не по
# хешу. Сертификат можно задать явно: SIGN_IDENTITY="…" (имя или SHA-1),
# SIGN_IDENTITY= — локальная подпись.

SIGN_TEAM="${SIGN_TEAM:-98SBSKJT3M}"
SIGN_TIMESTAMP=--timestamp

# SHA-1 действующего сертификата «Apple Development» команды $SIGN_TEAM. Имя
# не годится: у продлённого сертификата оно то же, и codesign не выберет.
development_identity() {
    local hash
    for hash in $(security find-identity -v -p codesigning 2>/dev/null \
        | awk '/"Apple Development: / {print $2}'); do
        if security find-certificate -a -Z -p -c "Apple Development:" 2>/dev/null \
            | awk -v h="$hash" '/^SHA-/ {on = ($0 == "SHA-1 hash: " h); next} on' \
            | openssl x509 -noout -subject 2>/dev/null | grep -q "OU *= *$SIGN_TEAM"; then
            echo "$hash"
            return
        fi
    done
}

if [[ -z "${SIGN_IDENTITY+set}" ]]; then
    SIGN_IDENTITY="$(security find-identity -v -p codesigning 2>/dev/null \
        | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)"
    if [[ -z "$SIGN_IDENTITY" && -z "${SIGN_RELEASE:-}" ]]; then
        SIGN_IDENTITY="$(development_identity)"
        # Метка времени нужна только нотаризации — не ходим за ней в сеть.
        SIGN_TIMESTAMP=--timestamp=none
    fi
fi

sign() {
    if [[ -n "$SIGN_IDENTITY" ]]; then
        codesign --force "$SIGN_TIMESTAMP" --options runtime --sign "$SIGN_IDENTITY" "$@"
    else
        codesign --force --sign - "$@"
    fi
}

# vv записывает голос — ему нужен микрофон. Идентификатор явно: dev.sh
# подписывает файл под другим именем, а приложение запомнило vv по нему.
sign_vv() {
    sign --identifier com.vibeterminal.vv --entitlements "$ROOT/assets/app/vv.entitlements" "$1"
}

# Чем подписано приложение — для вывода.
signed_by() {
    codesign -dvv "$1" 2>&1 | sed -n 's/^Authority=//p' | head -1 | grep . \
        || echo "локально, только для этого мака"
}

sign_app() {
    local app="$1"
    if [[ -z "$SIGN_IDENTITY" ]]; then
        codesign --force --sign - "$app/Contents/MacOS/vv"
        # Сохраняем entitlements, с которыми приложение подписал Xcode.
        codesign --force --sign - --preserve-metadata=entitlements,requirements,flags,runtime "$app"
        return
    fi
    # Изнутри наружу: сначала всё вложенное, приложение последним.
    local sparkle="$app/Contents/Frameworks/Sparkle.framework/Versions/B"
    local item
    for item in "$sparkle/XPCServices/Downloader.xpc" "$sparkle/XPCServices/Installer.xpc" \
        "$sparkle/Autoupdate" "$sparkle/Updater.app" "$app/Contents/Frameworks/Sparkle.framework" \
        "$app/Contents/PlugIns/DockTilePlugin.plugin"; do
        sign "$item"
    done
    sign_vv "$app/Contents/MacOS/vv"
    sign --entitlements "$ROOT/terminal/macos/Ghostty.entitlements" "$app"
}
