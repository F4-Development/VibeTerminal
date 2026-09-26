#if os(macOS)
import AppKit
import GhosttyKit
import SwiftUI

/// Окно настроек VibeTerminal (⌘,) — как принято в macOS: вкладки-иконки в
/// панели инструментов, заголовок окна — название вкладки, высота окна
/// подстраивается под вкладку, всё применяется сразу, без кнопки «Сохранить».
///
/// Claude, уведомления и голос — в ~/.config/vibeterminal/vv.json, его читает
/// vv. Шрифт — в файл настроек терминала.
final class VibeSettingsController: NSWindowController {
    private static var shared: VibeSettingsController?
    private let tabs = SettingsTabs()

    /// `tab` — какую вкладку открыть (`voice` из vv: «выбери модель»).
    static func show(tab: VibeSettingsTab? = nil) {
        let controller = shared ?? VibeSettingsController()
        shared = controller
        if let tab { controller.tabs.select(tab) }
        controller.showWindow(nil)
        controller.window?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    /// `vibeterminal://settings/voice` — так vv открывает нужную вкладку.
    static func open(_ url: URL) {
        guard url.scheme == "vibeterminal", url.host == "settings" else { return }
        show(tab: VibeSettingsTab(rawValue: url.lastPathComponent))
    }

    private init() {
        tabs.tabStyle = .toolbar
        tabs.canPropagateSelectedChildViewControllerTitle = true
        for tab in VibeSettingsTab.allCases {
            tabs.add(tab)
        }
        let window = NSWindow(contentViewController: tabs)
        window.styleMask = [.titled, .closable]
        window.toolbarStyle = .preference
        super.init(window: window)
        tabs.fitWindow(animate: false)
        window.center()
    }

    required init?(coder: NSCoder) {
        fatalError("не используется")
    }
}

enum VibeSettingsTab: String, CaseIterable {
    case claude, notifications, voice, look

    var title: String {
        switch self {
        case .claude: "Claude"
        case .notifications: "Уведомления"
        case .voice: "Голос"
        case .look: "Вид"
        }
    }

    var symbol: String {
        switch self {
        case .claude: "sparkles"
        case .notifications: "bell.badge"
        case .voice: "mic"
        case .look: "textformat.size"
        }
    }

    /// Высота вкладки: окно настроек не тянут руками, оно по содержимому.
    var height: CGFloat {
        switch self {
        case .claude: 430
        case .notifications: 470
        case .voice: 640
        case .look: 250
        }
    }

    @MainActor var view: AnyView {
        switch self {
        case .claude: AnyView(ClaudeSettings())
        case .notifications: AnyView(NotificationSettings())
        case .voice: AnyView(VoiceSettings())
        case .look: AnyView(LookSettings())
        }
    }
}

/// Вкладки в панели инструментов; при переключении окно плавно меняет
/// высоту, верхний край остаётся на месте.
private final class SettingsTabs: NSTabViewController {
    static let width: CGFloat = 540

    func add(_ tab: VibeSettingsTab) {
        let host = NSHostingController(rootView: tab.view.frame(width: Self.width, height: tab.height))
        host.title = tab.title
        let item = NSTabViewItem(viewController: host)
        item.label = tab.title
        item.identifier = tab.rawValue
        item.image = NSImage(systemSymbolName: tab.symbol, accessibilityDescription: tab.title)
        addTabViewItem(item)
    }

    func select(_ tab: VibeSettingsTab) {
        guard let index = tabViewItems.firstIndex(where: { ($0.identifier as? String) == tab.rawValue }) else { return }
        selectedTabViewItemIndex = index
    }

    override func tabView(_ tabView: NSTabView, didSelect tabViewItem: NSTabViewItem?) {
        super.tabView(tabView, didSelect: tabViewItem)
        fitWindow(animate: true)
    }

    func fitWindow(animate: Bool) {
        guard let window = view.window,
              let id = tabView.selectedTabViewItem?.identifier as? String,
              let tab = VibeSettingsTab(rawValue: id) else { return }
        let content = window.contentRect(forFrameRect: window.frame)
        let size = NSSize(width: Self.width, height: tab.height)
        let target = NSRect(x: content.minX, y: content.maxY - size.height, width: size.width, height: size.height)
        window.setFrame(window.frameRect(forContentRect: target), display: true, animate: animate && window.isVisible)
    }
}

// MARK: - Настройки vv

/// Настройки vv (`~/.config/vibeterminal/vv.json`). Те же поля читает vv.
struct VvSettings: Codable, Equatable {
    var projectsDirs = ["~/Projects"]
    var permissionMode = ""
    var model = ""
    var extraArgs = ""
    var notifyWaiting = true
    var notifyDone = true
    var notifyDoneAfter = 30
    var notifyFailed = true
    var notifyBanner = true
    var notifySound = "Glass"
    /// Какие лимиты vv показывает внизу — выбирают галочками в самом vv.
    var usageShown = ["context", "session"]
    /// Голосовой ввод: модель (пусто — выключен), как записывать, что делать
    /// с текстом, язык, словарь, микрофон.
    var voiceModel = ""
    var voiceMode = "press"
    var voiceAfter = "send"
    var voiceLanguage = "ru"
    var voiceWords = ""
    var voiceDevice = ""

    enum CodingKeys: String, CodingKey {
        case projectsDirs = "projects_dirs"
        case permissionMode = "permission_mode"
        case model
        case extraArgs = "extra_args"
        case notifyWaiting = "notify_waiting"
        case notifyDone = "notify_done"
        case notifyDoneAfter = "notify_done_after"
        case notifyFailed = "notify_failed"
        case notifyBanner = "notify_banner"
        case notifySound = "notify_sound"
        case usageShown = "usage_shown"
        case voiceModel = "voice_model"
        case voiceMode = "voice_mode"
        case voiceAfter = "voice_after"
        case voiceLanguage = "voice_language"
        case voiceWords = "voice_words"
        case voiceDevice = "voice_device"
    }

    init() {}

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let d = VvSettings()
        projectsDirs = try c.decodeIfPresent([String].self, forKey: .projectsDirs) ?? d.projectsDirs
        permissionMode = try c.decodeIfPresent(String.self, forKey: .permissionMode) ?? d.permissionMode
        model = try c.decodeIfPresent(String.self, forKey: .model) ?? d.model
        extraArgs = try c.decodeIfPresent(String.self, forKey: .extraArgs) ?? d.extraArgs
        notifyWaiting = try c.decodeIfPresent(Bool.self, forKey: .notifyWaiting) ?? d.notifyWaiting
        notifyDone = try c.decodeIfPresent(Bool.self, forKey: .notifyDone) ?? d.notifyDone
        notifyDoneAfter = try c.decodeIfPresent(Int.self, forKey: .notifyDoneAfter) ?? d.notifyDoneAfter
        notifyFailed = try c.decodeIfPresent(Bool.self, forKey: .notifyFailed) ?? d.notifyFailed
        notifyBanner = try c.decodeIfPresent(Bool.self, forKey: .notifyBanner) ?? d.notifyBanner
        notifySound = try c.decodeIfPresent(String.self, forKey: .notifySound) ?? d.notifySound
        usageShown = try c.decodeIfPresent([String].self, forKey: .usageShown) ?? d.usageShown
        voiceModel = try c.decodeIfPresent(String.self, forKey: .voiceModel) ?? d.voiceModel
        voiceMode = try c.decodeIfPresent(String.self, forKey: .voiceMode) ?? d.voiceMode
        voiceAfter = try c.decodeIfPresent(String.self, forKey: .voiceAfter) ?? d.voiceAfter
        voiceLanguage = try c.decodeIfPresent(String.self, forKey: .voiceLanguage) ?? d.voiceLanguage
        voiceWords = try c.decodeIfPresent(String.self, forKey: .voiceWords) ?? d.voiceWords
        voiceDevice = try c.decodeIfPresent(String.self, forKey: .voiceDevice) ?? d.voiceDevice
    }

    static var url: URL {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".config/vibeterminal/vv.json")
    }

    static func load() -> VvSettings {
        guard let data = try? Data(contentsOf: url),
              let settings = try? JSONDecoder().decode(VvSettings.self, from: data) else { return VvSettings() }
        return settings
    }

    func save() {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        guard let data = try? encoder.encode(self) else { return }
        try? FileManager.default.createDirectory(
            at: Self.url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? data.write(to: Self.url, options: .atomic)
    }
}

/// Пояснение под пунктом — мелко и серым, как в системных настройках.
struct Hint: View {
    let text: String

    init(_ text: String) {
        self.text = text
    }

    var body: some View {
        Text(text).font(.callout).foregroundStyle(.secondary).fixedSize(horizontal: false, vertical: true)
    }
}

// MARK: - Claude

struct ClaudeSettings: View {
    @State private var settings = VvSettings.load()

    var body: some View {
        Form {
            Section {
                ForEach(settings.projectsDirs, id: \.self) { dir in
                    LabeledContent {
                        Button {
                            settings.projectsDirs.removeAll { $0 == dir }
                        } label: {
                            Image(systemName: "minus.circle.fill").foregroundStyle(.secondary)
                        }
                        .buttonStyle(.borderless)
                        .help("Убрать из списка")
                    } label: {
                        Label(dir, systemImage: "folder")
                    }
                }
                Button("Добавить папку…", action: addFolder)
            } header: {
                Text("Где лежат проекты")
            } footer: {
                Hint("Проекты из этих папок предлагаются, когда открываешь новую сессию.")
            }

            Section {
                Picker("Разрешения", selection: $settings.permissionMode) {
                    Text("Как в настройках Claude").tag("")
                    Divider()
                    Text("Спрашивать каждый раз").tag("default")
                    Text("Разрешать правку файлов").tag("acceptEdits")
                    Text("Решает сам Claude").tag("auto")
                    Text("Только план, без изменений").tag("plan")
                }
                Picker("Модель", selection: $settings.model) {
                    Text("Как в настройках Claude").tag("")
                    Divider()
                    Text("Opus").tag("opus")
                    Text("Sonnet").tag("sonnet")
                    Text("Haiku").tag("haiku")
                }
                TextField("Флаги claude", text: $settings.extraArgs, prompt: Text("--add-dir ../shared"))
            } header: {
                Text("Новые сессии")
            } footer: {
                Hint("Применяется к сессиям, которые откроешь после изменения.")
            }
        }
        .formStyle(.grouped)
        .onAppear { settings = VvSettings.load() }
        .onChange(of: settings) { newValue in newValue.save() }
    }

    private func addFolder() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = true
        panel.prompt = "Добавить"
        panel.message = "Выбери папки, где лежат проекты"
        guard panel.runModal() == .OK else { return }
        let home = FileManager.default.homeDirectoryForCurrentUser.path
        for url in panel.urls {
            let path = url.path.hasPrefix(home) ? "~" + url.path.dropFirst(home.count) : url.path
            if !settings.projectsDirs.contains(path) {
                settings.projectsDirs.append(path)
            }
        }
    }
}

// MARK: - Уведомления

/// Когда vv зовёт тебя и как: баннер macOS и звук.
struct NotificationSettings: View {
    @State private var settings = VvSettings.load()

    private static let sounds = [
        "Basso", "Blow", "Bottle", "Frog", "Funk", "Glass", "Hero",
        "Morse", "Ping", "Pop", "Purr", "Sosumi", "Submarine", "Tink",
    ]
    private static let delays: [(String, Int)] = [
        ("Сколько угодно", 0), ("10 секунд", 10), ("30 секунд", 30),
        ("1 минуту", 60), ("2 минуты", 120), ("5 минут", 300),
    ]

    var body: some View {
        Form {
            Section {
                Toggle(isOn: $settings.notifyWaiting) {
                    Text("Сессия ждёт ответа")
                    Text("Разрешение, вопрос с вариантами или план")
                }
                Toggle(isOn: $settings.notifyDone) {
                    Text("Claude закончил работу")
                }
                Picker("Если работал хотя бы", selection: $settings.notifyDoneAfter) {
                    ForEach(Self.delays, id: \.1) { Text($0.0).tag($0.1) }
                }
                .disabled(!settings.notifyDone)
                Toggle(isOn: $settings.notifyFailed) {
                    Text("Ошибка")
                    Text("Упёрся в лимит, нужен вход, сервер перегружен")
                }
            } header: {
                Text("Звать, когда")
            } footer: {
                Hint("Если смотришь на эту сессию, уведомлений не будет.")
            }

            Section {
                Toggle(isOn: $settings.notifyBanner) {
                    Text("Баннер")
                    Text("Когда окно VibeTerminal не на переднем плане. Клик открывает нужную сессию.")
                }
                LabeledContent("Звук") {
                    HStack(spacing: 8) {
                        Picker("Звук", selection: $settings.notifySound) {
                            Text("Без звука").tag("")
                            Divider()
                            ForEach(Self.sounds, id: \.self) { Text($0).tag($0) }
                        }
                        .labelsHidden()
                        .fixedSize()
                        Button {
                            NSSound(named: NSSound.Name(settings.notifySound))?.play()
                        } label: {
                            Image(systemName: "play.fill")
                        }
                        .help("Прослушать")
                        .disabled(settings.notifySound.isEmpty)
                    }
                }
            } header: {
                Text("Как звать")
            } footer: {
                HStack {
                    Spacer()
                    Button("Настройки уведомлений macOS…") {
                        let link = "x-apple.systempreferences:com.apple.Notifications-Settings.extension"
                        if let url = URL(string: link) { NSWorkspace.shared.open(url) }
                    }
                }
            }
        }
        .formStyle(.grouped)
        .onAppear { settings = VvSettings.load() }
        .onChange(of: settings) { newValue in newValue.save() }
    }
}

// MARK: - Вид

/// Файл настроек терминала (формат Ghostty: `ключ = значение`). Меняем
/// только свои ключи, остальные строки остаются как написал пользователь.
struct TerminalConfigFile {
    let url: URL
    private var lines: [String]

    static func load() -> TerminalConfigFile {
        let path = Ghostty.AllocatedString(ghostty_config_open_path()).string
        let url = URL(fileURLWithPath: path)
        let text = (try? String(contentsOf: url, encoding: .utf8)) ?? ""
        return TerminalConfigFile(url: url, lines: text.isEmpty ? [] : text.components(separatedBy: "\n"))
    }

    private init(url: URL, lines: [String]) {
        self.url = url
        self.lines = lines
    }

    /// Последнее значение ключа — так его читает и сам терминал.
    func value(_ key: String) -> String? {
        lines.reversed().lazy.compactMap(Self.parse).first { $0.key == key }?.value
    }

    /// `nil` — убрать ключ, терминал возьмёт значение по умолчанию.
    mutating func set(_ key: String, _ value: String?, quoted: Bool = false) {
        lines.removeAll { Self.parse($0)?.key == key }
        while lines.last?.trimmingCharacters(in: .whitespaces).isEmpty == true {
            lines.removeLast()
        }
        if let value {
            lines.append("\(key) = \(quoted ? "\"\(value)\"" : value)")
        }
        save()
    }

    private func save() {
        try? FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        try? (lines.joined(separator: "\n") + "\n").write(to: url, atomically: true, encoding: .utf8)
        (NSApp.delegate as? AppDelegate)?.ghostty.reloadConfig()
    }

    private static func parse(_ line: String) -> (key: String, value: String)? {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard !trimmed.hasPrefix("#"), let equals = trimmed.firstIndex(of: "=") else { return nil }
        let key = trimmed[..<equals].trimmingCharacters(in: .whitespaces)
        var value = trimmed[trimmed.index(after: equals)...].trimmingCharacters(in: .whitespaces)
        if value.count >= 2, value.hasPrefix("\""), value.hasSuffix("\"") {
            value = String(value.dropFirst().dropLast())
        }
        return (key, value)
    }
}

private let defaultFontSize = 13.0
private let defaultFontFamily = "JetBrains Mono"

/// Шрифт терминала — всё остальное про вид задаёт сам vv.
struct LookSettings: View {
    @State private var config = TerminalConfigFile.load()
    private let fonts = monospacedFamilies()

    var body: some View {
        Form {
            Section {
                Picker("Шрифт", selection: fontFamily) {
                    Text("\(defaultFontFamily) — по умолчанию").tag("")
                    Divider()
                    ForEach(fonts, id: \.self) { Text($0).tag($0) }
                }
                Stepper(value: fontSize, in: 8...36, step: 1) {
                    LabeledContent("Размер", value: "\(Int(fontSize.wrappedValue)) пт")
                }
                Text("❯ Сделай рефакторинг модуля авторизации")
                    .font(.custom(fontFamily.wrappedValue.isEmpty ? defaultFontFamily : fontFamily.wrappedValue,
                                  size: fontSize.wrappedValue))
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            } header: {
                Text("Шрифт терминала")
            } footer: {
                Hint("Меняется сразу во всех окнах.")
            }
        }
        .formStyle(.grouped)
        .onAppear { config = TerminalConfigFile.load() }
    }

    private var fontFamily: Binding<String> {
        Binding(
            get: { config.value("font-family") ?? "" },
            set: { config.set("font-family", $0.isEmpty ? nil : $0, quoted: true) })
    }

    private var fontSize: Binding<Double> {
        Binding(
            get: { config.value("font-size").flatMap(Double.init) ?? defaultFontSize },
            set: { config.set("font-size", $0 == defaultFontSize ? nil : String(Int($0))) })
    }
}

private func monospacedFamilies() -> [String] {
    let names = NSFontManager.shared.availableFontNames(with: .fixedPitchFontMask) ?? []
    let families = Set(names.compactMap { NSFont(name: $0, size: 12)?.familyName })
    return families.filter { !$0.hasPrefix(".") }.sorted()
}
#endif
