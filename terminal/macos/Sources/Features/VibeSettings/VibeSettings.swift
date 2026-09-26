#if os(macOS)
import AppKit
import GhosttyKit
import SwiftUI

/// Окно настроек VibeTerminal (⌘,): только то, что нужно для работы с Claude.
///
/// Claude и уведомления — в ~/.config/vibeterminal/vv.json, его читает vv.
/// Шрифт — в файл настроек терминала, применяется сразу.
final class VibeSettingsController: NSWindowController {
    private static var shared: VibeSettingsController?
    private let state = VibeSettingsState()

    /// `tab` — какую вкладку открыть (`voice` из vv: «выбери модель»).
    static func show(tab: VibeSettingsTab? = nil) {
        let controller = shared ?? VibeSettingsController()
        shared = controller
        if let tab { controller.state.tab = tab }
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
        let window = NSWindow(contentViewController: NSHostingController(rootView: VibeSettingsView(state: state)))
        window.title = "Настройки"
        window.styleMask = [.titled, .closable, .miniaturizable]
        window.center()
        super.init(window: window)
    }

    required init?(coder: NSCoder) {
        fatalError("не используется")
    }
}

enum VibeSettingsTab: String {
    case claude, notifications, voice, look
}

final class VibeSettingsState: ObservableObject {
    @Published var tab = VibeSettingsTab.claude
}

struct VibeSettingsView: View {
    @ObservedObject var state: VibeSettingsState

    var body: some View {
        TabView(selection: $state.tab) {
            ClaudeSettings()
                .tabItem { Label("Claude", systemImage: "sparkles") }
                .tag(VibeSettingsTab.claude)
            NotificationSettings()
                .tabItem { Label("Уведомления", systemImage: "bell") }
                .tag(VibeSettingsTab.notifications)
            VoiceSettings()
                .tabItem { Label("Голос", systemImage: "mic") }
                .tag(VibeSettingsTab.voice)
            LookSettings()
                .tabItem { Label("Вид", systemImage: "textformat.size") }
                .tag(VibeSettingsTab.look)
        }
        .frame(width: 580, height: 540)
    }
}

// MARK: - Терминал

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

/// Шрифт терминала — всё остальное про вид задаёт сам vv.
struct LookSettings: View {
    @State private var config = TerminalConfigFile.load()
    private let fonts = monospacedFamilies()

    var body: some View {
        Form {
            Section("Шрифт") {
                Picker("Шрифт", selection: fontFamily) {
                    Text("По умолчанию (JetBrains Mono)").tag("")
                    ForEach(fonts, id: \.self) { Text($0).tag($0) }
                }
                Stepper(value: fontSize, in: 8...36, step: 1) {
                    Text("Размер: \(Int(fontSize.wrappedValue))")
                }
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

// MARK: - Claude

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
    /// Голосовой ввод: модель (пусто — выключен), язык, словарь, отправка, микрофон.
    var voiceModel = ""
    var voiceLanguage = "ru"
    var voiceWords = ""
    var voiceSend = false
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
        case voiceLanguage = "voice_language"
        case voiceWords = "voice_words"
        case voiceSend = "voice_send"
        case voiceDevice = "voice_device"
    }

    init() {}

    init(from decoder: Decoder) throws {
        let container = try decoder.container(keyedBy: CodingKeys.self)
        let defaults = VvSettings()
        projectsDirs = try container.decodeIfPresent([String].self, forKey: .projectsDirs) ?? defaults.projectsDirs
        permissionMode = try container.decodeIfPresent(String.self, forKey: .permissionMode) ?? defaults.permissionMode
        model = try container.decodeIfPresent(String.self, forKey: .model) ?? defaults.model
        extraArgs = try container.decodeIfPresent(String.self, forKey: .extraArgs) ?? defaults.extraArgs
        notifyWaiting = try container.decodeIfPresent(Bool.self, forKey: .notifyWaiting) ?? defaults.notifyWaiting
        notifyDone = try container.decodeIfPresent(Bool.self, forKey: .notifyDone) ?? defaults.notifyDone
        notifyDoneAfter = try container.decodeIfPresent(Int.self, forKey: .notifyDoneAfter) ?? defaults.notifyDoneAfter
        notifyFailed = try container.decodeIfPresent(Bool.self, forKey: .notifyFailed) ?? defaults.notifyFailed
        notifyBanner = try container.decodeIfPresent(Bool.self, forKey: .notifyBanner) ?? defaults.notifyBanner
        notifySound = try container.decodeIfPresent(String.self, forKey: .notifySound) ?? defaults.notifySound
        usageShown = try container.decodeIfPresent([String].self, forKey: .usageShown) ?? defaults.usageShown
        voiceModel = try container.decodeIfPresent(String.self, forKey: .voiceModel) ?? defaults.voiceModel
        voiceLanguage = try container.decodeIfPresent(String.self, forKey: .voiceLanguage) ?? defaults.voiceLanguage
        voiceWords = try container.decodeIfPresent(String.self, forKey: .voiceWords) ?? defaults.voiceWords
        voiceSend = try container.decodeIfPresent(Bool.self, forKey: .voiceSend) ?? defaults.voiceSend
        voiceDevice = try container.decodeIfPresent(String.self, forKey: .voiceDevice) ?? defaults.voiceDevice
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

struct ClaudeSettings: View {
    @State private var settings = VvSettings.load()

    var body: some View {
        Form {
            Section {
                ForEach(settings.projectsDirs, id: \.self) { dir in
                    HStack {
                        Image(systemName: "folder")
                        Text(dir)
                        Spacer()
                        Button {
                            settings.projectsDirs.removeAll { $0 == dir }
                        } label: {
                            Image(systemName: "minus.circle")
                        }
                        .buttonStyle(.borderless)
                        .help("Убрать папку")
                    }
                }
                Button("Добавить папку…", action: addFolder)
            } header: {
                Text("Папки с проектами")
            } footer: {
                Text("Их содержимое показывается в «Новая сессия» после недавних проектов.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section {
                Picker("Разрешения", selection: $settings.permissionMode) {
                    Text("Как в настройках Claude").tag("")
                    Text("Спрашивать каждый раз").tag("default")
                    Text("Разрешать правки файлов").tag("acceptEdits")
                    Text("Авто").tag("auto")
                    Text("Только план, без изменений").tag("plan")
                }
                Picker("Модель", selection: $settings.model) {
                    Text("Как в настройках Claude").tag("")
                    Text("Opus").tag("opus")
                    Text("Sonnet").tag("sonnet")
                    Text("Haiku").tag("haiku")
                }
                TextField("Дополнительные флаги", text: $settings.extraArgs, prompt: Text("например, --add-dir ../shared"))
            } header: {
                Text("Новые сессии")
            } footer: {
                Text("Действуют для сессий, открытых после изменения.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
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

/// Когда vv зовёт тебя и как: баннер macOS и звук. Пишется в vv.json,
/// действует сразу.
struct NotificationSettings: View {
    @State private var settings = VvSettings.load()

    private static let sounds = [
        "Basso", "Blow", "Bottle", "Frog", "Funk", "Glass", "Hero",
        "Morse", "Ping", "Pop", "Purr", "Sosumi", "Submarine", "Tink",
    ]
    private static let delays: [(String, Int)] = [
        ("сколько угодно", 0), ("10 секунд", 10), ("30 секунд", 30),
        ("1 минуту", 60), ("2 минуты", 120), ("5 минут", 300),
    ]

    var body: some View {
        Form {
            Section {
                Toggle("Сессия ждёт тебя: разрешение, вопрос, план", isOn: $settings.notifyWaiting)
                Toggle("Claude закончил", isOn: $settings.notifyDone)
                Picker("Если работал хотя бы", selection: $settings.notifyDoneAfter) {
                    ForEach(Self.delays, id: \.1) { Text($0.0).tag($0.1) }
                }
                .disabled(!settings.notifyDone)
                Toggle("Ошибка: лимит, вход, сервер", isOn: $settings.notifyFailed)
            } header: {
                Text("Когда звать")
            } footer: {
                Text("Не беспокоим, если ты смотришь на эту сессию.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section {
                Toggle("Баннер, когда окно VibeTerminal не на переднем плане", isOn: $settings.notifyBanner)
                HStack {
                    Picker("Звук", selection: $settings.notifySound) {
                        Text("Без звука").tag("")
                        ForEach(Self.sounds, id: \.self) { Text($0).tag($0) }
                    }
                    Button {
                        NSSound(named: NSSound.Name(settings.notifySound))?.play()
                    } label: {
                        Image(systemName: "play.circle")
                    }
                    .buttonStyle(.borderless)
                    .help("Прослушать")
                    .disabled(settings.notifySound.isEmpty)
                }
                Button("Уведомления в настройках macOS…") {
                    let link = "x-apple.systempreferences:com.apple.Notifications-Settings.extension"
                    if let url = URL(string: link) { NSWorkspace.shared.open(url) }
                }
            } header: {
                Text("Как")
            } footer: {
                Text("Окно открыто, но ты в другой сессии — только звук и подсказка внизу. "
                    + "Клик по баннеру открывает нужную сессию.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .formStyle(.grouped)
        .onAppear { settings = VvSettings.load() }
        .onChange(of: settings) { newValue in newValue.save() }
    }
}
#endif
