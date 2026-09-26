#if os(macOS)
import AVFoundation
import CryptoKit
import SwiftUI

/// Модель Whisper для голосового ввода vv. Файлы — в ~/.vibeterminal/models,
/// vv берёт выбранную по `voice_model` в vv.json.
struct VoiceModel: Identifiable {
    let id: String
    let title: String
    let detail: String
    let size: Int64
    let sha256: String

    var file: String { "ggml-\(id).bin" }
    var url: URL { URL(string: "https://huggingface.co/ggerganov/whisper.cpp/resolve/main/\(file)")! }
    var path: URL { Self.directory.appendingPathComponent(file) }
    var isDownloaded: Bool { FileManager.default.fileExists(atPath: path.path) }
    var sizeText: String { ByteCountFormatter.string(fromByteCount: size, countStyle: .file) }

    static var directory: URL {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".vibeterminal/models")
    }

    static let all = [
        VoiceModel(
            id: "large-v3-turbo-q5_0",
            title: "Точная сжатая",
            detail: "Рекомендуем: точно и быстро · 574 МБ",
            size: 574_041_195,
            sha256: "394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2"),
        VoiceModel(
            id: "large-v3-turbo",
            title: "Точная",
            detail: "Чуть точнее, но в три раза тяжелее · 1,6 ГБ",
            size: 1_624_555_275,
            sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69"),
        VoiceModel(
            id: "small",
            title: "Быстрая",
            detail: "Для старых Mac, ошибается чаще · 488 МБ",
            size: 487_601_967,
            sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b"),
    ]
}

/// Скачивание моделей: прогресс, отмена, проверка контрольной суммы. Живёт
/// всё время работы приложения — закрыл окно настроек, загрузка идёт дальше.
final class VoiceDownloads: ObservableObject {
    static let shared = VoiceDownloads()

    /// Сколько скачано, 0…1, пока качается.
    @Published private(set) var progress: [String: Double] = [:]
    /// Качается или проверяется — кнопки заняты.
    @Published private(set) var busy: Set<String> = []
    @Published private(set) var errors: [String: String] = [:]
    /// Чтобы вид перечитал, что уже скачано.
    @Published private(set) var revision = 0

    private var tasks: [String: URLSessionDownloadTask] = [:]
    private var timer: Timer?

    func start(_ model: VoiceModel) {
        guard !busy.contains(model.id) else { return }
        errors[model.id] = nil
        busy.insert(model.id)
        progress[model.id] = 0
        let part = model.path.appendingPathExtension("part")
        let task = URLSession.shared.downloadTask(with: model.url) { [weak self] location, response, error in
            // Файл из временной папки надо забрать, пока мы здесь.
            var failure = error.map { ($0 as NSError).code == NSURLErrorCancelled ? "" : $0.localizedDescription }
            if failure == nil, let location {
                if let http = response as? HTTPURLResponse, http.statusCode != 200 {
                    failure = "сервер ответил \(http.statusCode)"
                } else {
                    do {
                        try FileManager.default.createDirectory(at: VoiceModel.directory, withIntermediateDirectories: true)
                        try? FileManager.default.removeItem(at: part)
                        try FileManager.default.moveItem(at: location, to: part)
                    } catch {
                        failure = error.localizedDescription
                    }
                }
            }
            DispatchQueue.main.async { self?.downloaded(model, part: part, failure: failure) }
        }
        tasks[model.id] = task
        task.resume()
        startTimer()
    }

    func cancel(_ model: VoiceModel) {
        tasks[model.id]?.cancel()
    }

    func delete(_ model: VoiceModel) {
        try? FileManager.default.removeItem(at: model.path)
        revision += 1
    }

    private func downloaded(_ model: VoiceModel, part: URL, failure: String?) {
        tasks[model.id] = nil
        progress[model.id] = nil
        if let failure {
            try? FileManager.default.removeItem(at: part)
            busy.remove(model.id)
            if !failure.isEmpty { errors[model.id] = "Не скачалось: \(failure)" }
            return
        }
        // Проверка контрольной суммы — в фоне, файл большой.
        DispatchQueue.global(qos: .userInitiated).async { [weak self] in
            let ok = Self.checksum(of: part) == model.sha256
            if ok {
                try? FileManager.default.removeItem(at: model.path)
                try? FileManager.default.moveItem(at: part, to: model.path)
            } else {
                try? FileManager.default.removeItem(at: part)
            }
            DispatchQueue.main.async {
                guard let self else { return }
                self.busy.remove(model.id)
                if !ok { self.errors[model.id] = "Файл повреждён — скачай ещё раз" }
                self.revision += 1
            }
        }
    }

    private func startTimer() {
        guard timer == nil else { return }
        timer = Timer.scheduledTimer(withTimeInterval: 0.3, repeats: true) { [weak self] _ in
            guard let self else { return }
            for (id, task) in self.tasks {
                let expected = task.countOfBytesExpectedToReceive
                let total = expected > 0 ? expected : (VoiceModel.all.first { $0.id == id }?.size ?? 1)
                self.progress[id] = min(1, Double(task.countOfBytesReceived) / Double(total))
            }
            if self.tasks.isEmpty {
                self.timer?.invalidate()
                self.timer = nil
            }
        }
    }

    private static func checksum(of url: URL) -> String? {
        guard let handle = try? FileHandle(forReadingFrom: url) else { return nil }
        defer { try? handle.close() }
        var hasher = SHA256()
        while let chunk = try? handle.read(upToCount: 8 << 20), !chunk.isEmpty {
            hasher.update(data: chunk)
        }
        return hasher.finalize().map { String(format: "%02x", $0) }.joined()
    }
}

/// Вкладка «Голос»: модель (скачать, выбрать, удалить), как записывать, что
/// делать с текстом, язык, словарь, микрофон.
struct VoiceSettings: View {
    @State private var settings = VvSettings.load()
    @ObservedObject private var downloads = VoiceDownloads.shared
    @State private var deleting: VoiceModel?
    private let microphones = AVCaptureDevice.DiscoverySession(
        deviceTypes: [.builtInMicrophone, .externalUnknown], mediaType: .audio, position: .unspecified
    ).devices.map(\.localizedName)

    var body: some View {
        Form {
            Section {
                ForEach(VoiceModel.all) { model in
                    modelRow(model)
                }
            } header: {
                Text("Модель распознавания")
            } footer: {
                Hint(settings.voiceModel.isEmpty
                    ? "Скачай и выбери модель — без неё голосового ввода нет. Распознавание идёт на этом Mac, звук никуда не отправляется."
                    : "Распознавание идёт на этом Mac, звук никуда не отправляется.")
            }

            Section {
                Picker("Как записывать", selection: $settings.voiceMode) {
                    Text("Нажать и говорить").tag("press")
                    Text("Удерживать клавишу").tag("hold")
                }
                .pickerStyle(.radioGroup)
                Hint(settings.voiceMode == "hold"
                    ? "Запись идёт, пока держишь ⌘⇧Space. Отпустил — распознано. Микрофон в поле ввода работает как «Нажать и говорить»."
                    : "Нажми ⌘⇧Space или микрофон в поле ввода и говори. Enter — готово, Esc — отменить.")
                Picker("Распознанный текст", selection: $settings.voiceAfter) {
                    Text("Сразу отправить Claude").tag("send")
                    Text("Вставить в поле ввода — отправлю сам").tag("insert")
                }
                .pickerStyle(.radioGroup)
                Picker("Микрофон", selection: $settings.voiceDevice) {
                    Text("Как в настройках macOS").tag("")
                    if !microphones.isEmpty { Divider() }
                    ForEach(microphones, id: \.self) { Text($0).tag($0) }
                }
            } header: {
                Text("Запись")
            }

            Section {
                Picker("Язык речи", selection: $settings.voiceLanguage) {
                    Text("Русский").tag("ru")
                    Text("Английский").tag("en")
                    Text("Определять автоматически").tag("auto")
                }
                LabeledContent {
                    TextField("Словарь", text: $settings.voiceWords, prompt: Text("Claude, commit, push, vibe-ide"), axis: .vertical)
                        .labelsHidden()
                        .lineLimit(2...3)
                } label: {
                    Text("Словарь")
                    Text("Имена и термины через запятую — их распознают точнее")
                }
            } header: {
                Text("Распознавание")
            }
        }
        .formStyle(.grouped)
        .onAppear { settings = VvSettings.load() }
        .onChange(of: settings) { newValue in newValue.save() }
        // Удалили выбранную модель — выбора больше нет.
        .onChange(of: downloads.revision) { _ in
            if let chosen = VoiceModel.all.first(where: { $0.id == settings.voiceModel }), !chosen.isDownloaded {
                settings.voiceModel = ""
            }
        }
        .confirmationDialog(
            "Удалить модель «\(deleting?.title ?? "")»?",
            isPresented: Binding(get: { deleting != nil }, set: { if !$0 { deleting = nil } }),
            presenting: deleting
        ) { model in
            Button("Удалить", role: .destructive) { downloads.delete(model) }
            Button("Отмена", role: .cancel) {}
        } message: { model in
            Text("Файл \(model.sizeText) удалится с диска. Скачать модель можно снова.")
        }
    }

    @ViewBuilder
    private func modelRow(_ model: VoiceModel) -> some View {
        let chosen = settings.voiceModel == model.id
        LabeledContent {
            HStack(spacing: 8) {
                if let fraction = downloads.progress[model.id] {
                    ProgressView(value: fraction).frame(width: 100)
                    Text("\(Int(fraction * 100)) %").monospacedDigit().foregroundStyle(.secondary).frame(width: 40, alignment: .trailing)
                    Button("Отменить") { downloads.cancel(model) }
                } else if downloads.busy.contains(model.id) {
                    ProgressView().controlSize(.small)
                    Text("Проверка…").foregroundStyle(.secondary)
                } else if model.isDownloaded {
                    if chosen {
                        Label("Используется", systemImage: "checkmark.circle.fill")
                            .labelStyle(.titleAndIcon)
                            .foregroundStyle(.secondary)
                    } else {
                        Button("Использовать") { settings.voiceModel = model.id }
                    }
                    Menu {
                        Button("Показать в Finder") { NSWorkspace.shared.activateFileViewerSelecting([model.path]) }
                        Divider()
                        Button("Удалить…", role: .destructive) { deleting = model }
                    } label: {
                        Image(systemName: "ellipsis.circle")
                    }
                    .menuStyle(.borderlessButton)
                    .menuIndicator(.hidden)
                    .fixedSize()
                    .help("Ещё")
                } else {
                    Button("Скачать") { downloads.start(model) }
                }
            }
        } label: {
            Text(model.title)
            if let error = downloads.errors[model.id] {
                Text(error).foregroundStyle(.red)
            } else {
                Text(model.detail)
            }
        }
        .id("\(model.id)-\(downloads.revision)")
        .onChange(of: downloads.busy) { busy in
            // Скачалась, а модели ещё нет — сразу использовать её.
            if !busy.contains(model.id), model.isDownloaded, settings.voiceModel.isEmpty {
                settings.voiceModel = model.id
            }
        }
    }
}
#endif
