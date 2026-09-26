#if os(macOS)
import AppKit
import Combine
import CryptoKit
import Security
import SwiftUI

/// Обновления VibeTerminal. При запуске и раз в 6 часов смотрим последний
/// релиз на GitHub. Есть новее — окно «Доступно обновление»; «Обновить» —
/// скачать, проверить (контрольная сумма, подпись, то же приложение),
/// заменить себя и перезапуститься. Сессии возвращаются, как после обычного
/// перезапуска: vv их помнит.
final class VibeUpdater: ObservableObject {
    static let shared = VibeUpdater()

    static let repository = "F4-Development/VibeTerminal"
    static let bundleIdentifier = "com.vibeterminal.app"
    private static let checkEvery: TimeInterval = 6 * 3600
    private static let autoCheckKey = "VibeAutoCheckUpdates"

    enum Phase: Equatable {
        case idle
        case checking
        case available
        case downloading(Double)
        case verifying
        case installing
        case upToDate
        case failed(String)
    }

    struct Release: Equatable {
        let version: String
        let notes: String
        let asset: URL
        let page: URL?
        /// SHA-256 архива, если GitHub его знает.
        let sha256: String?
    }

    @Published private(set) var phase: Phase = .idle
    @Published private(set) var release: Release?
    /// Уже ставим — выход без вопроса «точно закрыть?».
    private(set) var isInstalling = false

    private var panel: NSPanel?
    private var timer: Timer?
    private var download: URLSessionDownloadTask?
    private var progressTimer: Timer?

    // Для проверки без GitHub и без замены настоящего приложения.
    private let env = ProcessInfo.processInfo.environment
    private var feed: URL {
        URL(string: env["VIBETERMINAL_UPDATE_URL"] ?? "https://api.github.com/repos/\(Self.repository)/releases/latest")!
    }
    var target: URL {
        env["VIBETERMINAL_UPDATE_TARGET"].map(URL.init(fileURLWithPath:)) ?? Bundle.main.bundleURL
    }
    private var relaunches: Bool { env["VIBETERMINAL_UPDATE_NO_RELAUNCH"] == nil }

    var current: String {
        env["VIBETERMINAL_UPDATE_CURRENT"]
            ?? Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "0"
    }

    static var autoCheck: Bool {
        get { UserDefaults.ghostty.object(forKey: autoCheckKey) as? Bool ?? true }
        set { UserDefaults.ghostty.set(newValue, forKey: autoCheckKey) }
    }

    /// При запуске приложения: проверить чуть погодя и дальше раз в 6 часов.
    func start() {
        DispatchQueue.main.asyncAfter(deadline: .now() + 5) { [weak self] in
            if Self.autoCheck { self?.check(manual: false) }
        }
        timer = Timer.scheduledTimer(withTimeInterval: Self.checkEvery, repeats: true) { [weak self] _ in
            if Self.autoCheck { self?.check(manual: false) }
        }
    }

    /// `manual` — из меню или настроек: показать и «у тебя последняя версия».
    func check(manual: Bool) {
        switch phase {
        case .checking, .downloading, .verifying, .installing: return
        default: break
        }
        phase = .checking
        if manual { showPanel() }
        var request = URLRequest(url: feed, timeoutInterval: 20)
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        URLSession.shared.dataTask(with: request) { [weak self] data, _, error in
            let release = data.flatMap { Self.parse($0) }
            DispatchQueue.main.async {
                guard let self else { return }
                if let release, Self.isNewer(release.version, than: self.current) {
                    self.release = release
                    self.phase = .available
                    self.showPanel()
                } else if manual {
                    self.phase = release == nil && error != nil ? .failed("Не удалось связаться с GitHub") : .upToDate
                } else {
                    self.phase = .idle
                }
            }
        }.resume()
    }

    /// Ответ GitHub → релиз с архивом приложения.
    static func parse(_ data: Data) -> Release? {
        guard let json = (try? JSONSerialization.jsonObject(with: data)) as? [String: Any],
              let tag = json["tag_name"] as? String,
              let assets = json["assets"] as? [[String: Any]],
              let zip = assets.first(where: { ($0["name"] as? String)?.hasSuffix(".zip") == true }),
              let link = zip["browser_download_url"] as? String,
              let asset = URL(string: link) else { return nil }
        let digest = (zip["digest"] as? String).flatMap { $0.hasPrefix("sha256:") ? String($0.dropFirst(7)) : nil }
        return Release(
            version: tag.hasPrefix("v") ? String(tag.dropFirst()) : tag,
            notes: json["body"] as? String ?? "",
            asset: asset,
            page: (json["html_url"] as? String).flatMap(URL.init(string:)),
            sha256: digest)
    }

    /// `0.10.0` новее `0.9.3`: сравниваем числа, а не строки.
    static func isNewer(_ version: String, than current: String) -> Bool {
        let parts = { (text: String) in text.split(separator: ".").map { Int($0.prefix { $0.isNumber }) ?? 0 } }
        let (a, b) = (parts(version), parts(current))
        for index in 0..<max(a.count, b.count) {
            let (x, y) = (index < a.count ? a[index] : 0, index < b.count ? b[index] : 0)
            if x != y { return x > y }
        }
        return false
    }

    // MARK: Установка

    func install() {
        guard let release, phase == .available || isFailed else { return }
        phase = .downloading(0)
        let folder = FileManager.default.temporaryDirectory.appendingPathComponent("VibeTerminalUpdate-\(UUID().uuidString)")
        let zip = folder.appendingPathComponent("VibeTerminal.zip")
        let task = URLSession.shared.downloadTask(with: release.asset) { [weak self] location, response, error in
            // Файл из временной папки забираем, пока мы здесь.
            var failure = error.map { ($0 as NSError).code == NSURLErrorCancelled ? "" : $0.localizedDescription }
            if failure == nil, let location {
                if let http = response as? HTTPURLResponse, http.statusCode != 200 {
                    failure = "сервер ответил \(http.statusCode)"
                } else {
                    do {
                        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
                        try FileManager.default.moveItem(at: location, to: zip)
                    } catch {
                        failure = error.localizedDescription
                    }
                }
            }
            DispatchQueue.main.async {
                self?.progressTimer?.invalidate()
                self?.download = nil
                guard let self else { return }
                if let failure {
                    self.phase = failure.isEmpty ? .available : .failed("Не скачалось: \(failure)")
                    return
                }
                self.phase = .verifying
                DispatchQueue.global(qos: .userInitiated).async { self.unpackAndInstall(zip, in: folder, release) }
            }
        }
        download = task
        task.resume()
        progressTimer = Timer.scheduledTimer(withTimeInterval: 0.2, repeats: true) { [weak self] _ in
            guard let self, let task = self.download, task.countOfBytesExpectedToReceive > 0 else { return }
            self.phase = .downloading(min(1, Double(task.countOfBytesReceived) / Double(task.countOfBytesExpectedToReceive)))
        }
    }

    func cancelDownload() {
        download?.cancel()
    }

    private var isFailed: Bool {
        if case .failed = phase { return true }
        return false
    }

    /// Фоновый поток: проверить архив, распаковать, проверить приложение,
    /// заменить и перезапуститься.
    private func unpackAndInstall(_ zip: URL, in folder: URL, _ release: Release) {
        let fail = { (text: String) in
            try? FileManager.default.removeItem(at: folder)
            DispatchQueue.main.async { self.phase = .failed(text) }
        }
        if let expected = release.sha256, Self.sha256(of: zip) != expected.lowercased() {
            return fail("Архив повреждён — контрольная сумма не сходится")
        }
        let unpacked = folder.appendingPathComponent("app")
        guard Self.run("/usr/bin/ditto", ["-x", "-k", zip.path, unpacked.path]) else {
            return fail("Не удалось распаковать обновление")
        }
        guard let app = (try? FileManager.default.contentsOfDirectory(at: unpacked, includingPropertiesForKeys: nil))?
            .first(where: { $0.pathExtension == "app" }),
            let info = NSDictionary(contentsOf: app.appendingPathComponent("Contents/Info.plist")) else {
            return fail("В обновлении нет приложения")
        }
        guard info["CFBundleIdentifier"] as? String == Self.bundleIdentifier else {
            return fail("В обновлении чужое приложение")
        }
        guard info["CFBundleShortVersionString"] as? String == release.version else {
            return fail("Версия в архиве не та, что в релизе")
        }
        // Служебные метки «скачано из интернета» — прочь, иначе macOS не даст запустить.
        _ = Self.run("/usr/bin/xattr", ["-cr", app.path])
        // Подписано нами: если это приложение подписано Developer ID, то и
        // обновление — той же командой и одобрено Apple (нотаризация).
        var verify = ["--verify", "--deep", "--strict"]
        if let team = Self.team {
            verify.append("-R=anchor apple generic and certificate leaf[subject.OU] = \"\(team)\"")
        }
        guard Self.run("/usr/bin/codesign", verify + [app.path]) else {
            return fail("Подпись обновления не сходится")
        }
        if Self.team != nil, !Self.run("/usr/sbin/spctl", ["--assess", "--type", "execute", app.path]) {
            return fail("Обновление не одобрено Apple")
        }
        DispatchQueue.main.async { self.phase = .installing }
        do {
            _ = try FileManager.default.replaceItemAt(target, withItemAt: app)
        } catch {
            return fail("Не удалось заменить приложение: \(error.localizedDescription)")
        }
        try? FileManager.default.removeItem(at: folder)
        DispatchQueue.main.async { self.relaunch() }
    }

    /// Закрыться и открыть новую версию, когда этот процесс завершится.
    /// vv при закрытии запоминает сессии, новая версия их вернёт.
    private func relaunch() {
        guard relaunches else {
            phase = .idle
            closePanel()
            return
        }
        let pid = ProcessInfo.processInfo.processIdentifier
        let script = "while /bin/kill -0 \(pid) 2>/dev/null; do /bin/sleep 0.2; done; /bin/sleep 0.5; /usr/bin/open \"$0\""
        let helper = Process()
        helper.executableURL = URL(fileURLWithPath: "/bin/sh")
        helper.arguments = ["-c", script, target.path]
        do {
            try helper.run()
        } catch {
            // Новая версия уже на месте — её откроет обычный перезапуск.
            phase = .failed("Обновление установлено — закрой и открой VibeTerminal")
            release = nil
            return
        }
        isInstalling = true
        NSApp.terminate(nil)
    }

    /// Команда разработчика из подписи этого приложения; `nil` — локальная
    /// сборка без Developer ID.
    static let team: String? = {
        var code: SecCode?
        var staticCode: SecStaticCode?
        var info: CFDictionary?
        guard SecCodeCopySelf([], &code) == errSecSuccess, let code,
              SecCodeCopyStaticCode(code, [], &staticCode) == errSecSuccess, let staticCode,
              SecCodeCopySigningInformation(staticCode, SecCSFlags(rawValue: kSecCSSigningInformation), &info) == errSecSuccess
        else { return nil }
        return (info as? [String: Any])?[kSecCodeInfoTeamIdentifier as String] as? String
    }()

    static func sha256(of url: URL) -> String? {
        guard let handle = try? FileHandle(forReadingFrom: url) else { return nil }
        defer { try? handle.close() }
        var hasher = SHA256()
        while let chunk = try? handle.read(upToCount: 8 << 20), !chunk.isEmpty {
            hasher.update(data: chunk)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }

    @discardableResult
    static func run(_ tool: String, _ arguments: [String]) -> Bool {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: tool)
        process.arguments = arguments
        process.standardOutput = FileHandle.nullDevice
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
            process.waitUntilExit()
            return process.terminationStatus == 0
        } catch {
            return false
        }
    }

    // MARK: Окно

    /// Такое же окно, как для ответа Claude: справа вверху, поверх всех
    /// программ, фокус не отбирает.
    func showPanel() {
        if let panel {
            panel.orderFrontRegardless()
            return
        }
        let host = NSHostingController(rootView: VibeUpdateView(updater: self, close: { [weak self] in self?.closePanel() }))
        // Заголовка не видно — его место не прибавлять к высоте окна.
        if #available(macOS 13.3, *) { host.safeAreaRegions = [] }
        let panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 440, height: 200),
            styleMask: [.titled, .closable, .nonactivatingPanel, .fullSizeContentView],
            backing: .buffered, defer: false)
        panel.contentViewController = host
        panel.titleVisibility = .hidden
        panel.titlebarAppearsTransparent = true
        panel.standardWindowButton(.closeButton)?.isHidden = true
        panel.standardWindowButton(.miniaturizeButton)?.isHidden = true
        panel.standardWindowButton(.zoomButton)?.isHidden = true
        panel.isMovableByWindowBackground = true
        panel.isFloatingPanel = true
        panel.level = .floating
        panel.hidesOnDeactivate = false
        panel.becomesKeyOnlyIfNeeded = false
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.setContentSize(host.view.fittingSize)
        place(panel)
        panel.orderFrontRegardless()
        panel.makeKey()
        self.panel = panel
        // Содержимое меняется (описание → полоска загрузки) — окно по нему,
        // верхний край на месте.
        resize = $phase
            .map { (phase: Phase) -> Phase in if case .downloading = phase { .downloading(0) } else { phase } }
            .removeDuplicates()
            .dropFirst()
            .sink { [weak self] _ in DispatchQueue.main.async { self?.fit() } }
    }

    private var resize: AnyCancellable?

    private func fit() {
        guard let panel, let view = panel.contentViewController?.view else { return }
        let size = view.fittingSize
        let frame = panel.frameRect(forContentRect: NSRect(origin: .zero, size: size))
        panel.setFrame(NSRect(x: panel.frame.minX, y: panel.frame.maxY - frame.height,
                              width: frame.width, height: frame.height), display: true, animate: false)
    }

    /// Справа вверху экрана, где сейчас указатель.
    private func place(_ panel: NSPanel) {
        let mouse = NSEvent.mouseLocation
        let screen = NSScreen.screens.first { NSMouseInRect(mouse, $0.frame, false) } ?? NSScreen.main
        guard let visible = screen?.visibleFrame else { return }
        let size = panel.frame.size
        panel.setFrameOrigin(NSPoint(x: visible.maxX - size.width - 16, y: visible.maxY - size.height - 16))
    }

    func closePanel() {
        switch phase {
        case .downloading, .verifying, .installing: return
        case .upToDate, .failed, .checking: phase = release == nil ? .idle : .available
        default: break
        }
        resize = nil
        panel?.orderOut(nil)
        panel = nil
    }
}

// MARK: - Настройки

struct UpdateSettings: View {
    @ObservedObject private var updater = VibeUpdater.shared
    @State private var autoCheck = VibeUpdater.autoCheck

    var body: some View {
        Form {
            Section {
                LabeledContent("Версия", value: updater.current)
                Toggle("Проверять обновления автоматически", isOn: $autoCheck)
                    .onChange(of: autoCheck) { VibeUpdater.autoCheck = $0 }
                LabeledContent {
                    Button("Проверить сейчас") { updater.check(manual: true) }
                        .disabled(updater.phase == .checking)
                } label: {
                    EmptyView()
                }
            } footer: {
                Hint("При запуске и раз в несколько часов. Когда выйдет новая версия, появится окно — обновишься одной кнопкой, сессии останутся.")
            }
        }
        .formStyle(.grouped)
    }
}

// MARK: - Содержимое окна

struct VibeUpdateView: View {
    @ObservedObject var updater: VibeUpdater
    let close: () -> Void

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header
            Text(title).font(.title3.weight(.semibold))
            content
        }
        .padding(18)
        .frame(width: 440)
        // Заголовка у окна нет — место под него не нужно.
        .ignoresSafeArea(edges: .top)
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(nsImage: NSApp.applicationIconImage)
                .resizable()
                .frame(width: 28, height: 28)
            VStack(alignment: .leading, spacing: 1) {
                Text("VibeTerminal").font(.headline)
                Text(versions).font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            if canClose {
                Button(action: close) {
                    Image(systemName: "xmark.circle.fill")
                        .font(.title3)
                        .foregroundStyle(.secondary)
                }
                .buttonStyle(.plain)
                .help("Закрыть")
            }
        }
    }

    private var title: String {
        switch updater.phase {
        case .checking: "Проверяю обновления…"
        case .upToDate: "У тебя последняя версия"
        case .failed: "Обновить не вышло"
        case .downloading, .verifying, .installing: "Обновляю VibeTerminal"
        default: "Доступно новое обновление"
        }
    }

    private var versions: String {
        if let release = updater.release, updater.phase != .upToDate {
            return "Версия \(release.version) · сейчас \(updater.current)"
        }
        return "Версия \(updater.current)"
    }

    private var canClose: Bool {
        switch updater.phase {
        case .downloading, .verifying, .installing: false
        default: true
        }
    }

    @ViewBuilder
    private var content: some View {
        switch updater.phase {
        case .checking:
            ProgressView().progressViewStyle(.linear)
        case .upToDate:
            HStack {
                Spacer()
                Button("Готово", action: close).keyboardShortcut(.defaultAction)
            }
        case .available:
            notes
            Text("VibeTerminal перезапустится сам — открытые сессии вернутся.")
                .font(.callout)
                .foregroundStyle(.secondary)
            HStack {
                Spacer()
                Button("Позже", action: close).keyboardShortcut(.cancelAction)
                Button("Обновить") { updater.install() }.keyboardShortcut(.defaultAction)
            }
        case .downloading(let fraction):
            ProgressView(value: fraction)
            HStack {
                Text("Скачиваю… \(Int(fraction * 100)) %").monospacedDigit().foregroundStyle(.secondary)
                Spacer()
                Button("Отменить") { updater.cancelDownload() }.keyboardShortcut(.cancelAction)
            }
        case .verifying, .installing:
            ProgressView().progressViewStyle(.linear)
            Text(updater.phase == .verifying ? "Проверяю обновление…" : "Устанавливаю и перезапускаю…")
                .foregroundStyle(.secondary)
        case .failed(let text):
            Label(text, systemImage: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
                .font(.callout)
            HStack {
                if let page = updater.release?.page {
                    Button("Страница релиза") { NSWorkspace.shared.open(page) }.buttonStyle(.link)
                }
                Spacer()
                Button("Закрыть", action: close).keyboardShortcut(.cancelAction)
                if updater.release != nil {
                    Button("Ещё раз") { updater.install() }.keyboardShortcut(.defaultAction)
                }
            }
        case .idle:
            EmptyView()
        }
    }

    /// Что нового — из описания релиза: заголовки `#` жирным, пункты `-`
    /// точками. Короткое — целиком, длинное — с прокруткой.
    private var notes: some View {
        let text = updater.release?.notes.trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        let body = Text(Self.markdown(text))
            .textSelection(.enabled)
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(10)
        return Group {
            if !text.isEmpty {
                ViewThatFits(in: .vertical) {
                    body.fixedSize(horizontal: false, vertical: true)
                    ScrollView { body }
                }
                .frame(maxHeight: 220)
                .fixedSize(horizontal: false, vertical: true)
                .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.06)))
            }
        }
    }

    static func markdown(_ text: String) -> AttributedString {
        let lines = text.components(separatedBy: "\n").map { line -> String in
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            if trimmed.hasPrefix("- ") || trimmed.hasPrefix("* ") {
                return "•  " + trimmed.dropFirst(2)
            }
            guard trimmed.hasPrefix("#") else { return line }
            let heading = trimmed.drop { $0 == "#" }.trimmingCharacters(in: .whitespaces)
            return heading.isEmpty ? "" : "**\(heading)**"
        }
        let options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        return (try? AttributedString(markdown: lines.joined(separator: "\n"), options: options)) ?? AttributedString(text)
    }
}
#endif
