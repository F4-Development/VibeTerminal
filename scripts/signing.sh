# Подпись VibeTerminal — подключается из build-app.sh и dev.sh (нужен $ROOT).
#
# Есть сертификат «Developer ID Application» — подписываем им, с hardened
# runtime и меткой времени: так требует нотаризация, и macOS открывает
# скачанное приложение без предупреждений, а разрешения (микрофон) не
# слетают после обновления. Нет — локальная подпись, только для этого мака.
# Сертификат можно задать явно: SIGN_IDENTITY="Developer ID Application: …".

SIGN_IDENTITY="${SIGN_IDENTITY-$(security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/.*"\(Developer ID Application: [^"]*\)".*/\1/p' | head -1)}"

sign() {
    if [[ -n "$SIGN_IDENTITY" ]]; then
        codesign --force --timestamp --options runtime --sign "$SIGN_IDENTITY" "$@"
    else
        codesign --force --sign - "$@"
    fi
}

# vv записывает голос — ему нужен микрофон.
sign_vv() {
    sign --entitlements "$ROOT/assets/app/vv.entitlements" "$1"
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
