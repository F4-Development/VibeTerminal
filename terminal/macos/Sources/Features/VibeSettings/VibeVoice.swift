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
            detail: "Чуть точнее, но в три раза больше · 1,6 ГБ",
            size: 1_624_555_275,
            sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69"),
        VoiceModel(
            id: "small",
            title: "Быстрая",
            detail: "Для слабых Mac, ошибается чаще · 488 МБ",
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

/// Вкладка «Голос»: модель (скачать, выбрать, удалить), язык, словарь,
/// что делать с текстом, микрофон.
struct VoiceSettings: View {
    @State private var settings = VvSettings.load()
    @ObservedObject private var downloads = VoiceDownloads.shared
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
                Text("Распознаёт на этом Mac — звук никуда не уходит. Без модели голосового ввода нет.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Распознавание") {
                Picker("Язык", selection: $settings.voiceLanguage) {
                    Text("Русский").tag("ru")
                    Text("Английский").tag("en")
                    Text("Определять сам").tag("auto")
                }
                VStack(alignment: .leading, spacing: 4) {
                    Text("Словарь")
                    TextField("", text: $settings.voiceWords, prompt: Text("Claude, commit, push, названия проектов…"), axis: .vertical)
                        .lineLimit(2...4)
                    Text("Слова, которые надо узнавать правильно, через запятую.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Toggle("Сразу отправлять Claude", isOn: $settings.voiceSend)
            }

            Section("Микрофон") {
                Picker("Микрофон", selection: $settings.voiceDevice) {
                    Text("Как в системе").tag("")
                    ForEach(microphones, id: \.self) { Text($0).tag($0) }
                }
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
    }

    @ViewBuilder
    private func modelRow(_ model: VoiceModel) -> some View {
        let chosen = settings.voiceModel == model.id
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                HStack(spacing: 6) {
                    Text(model.title).fontWeight(chosen ? .semibold : .regular)
                    if chosen {
                        Image(systemName: "checkmark.circle.fill").foregroundStyle(.green)
                    }
                }
                Text(model.detail).font(.caption).foregroundStyle(.secondary)
                if let error = downloads.errors[model.id] {
                    Text(error).font(.caption).foregroundStyle(.red)
                }
            }
            Spacer()
            if let fraction = downloads.progress[model.id] {
                ProgressView(value: fraction).frame(width: 110)
                Text("\(Int(fraction * 100))%").monospacedDigit().frame(width: 38, alignment: .trailing)
                Button("Отмена") { downloads.cancel(model) }
            } else if downloads.busy.contains(model.id) {
                ProgressView().controlSize(.small)
                Text("Проверяю…").foregroundStyle(.secondary)
            } else if model.isDownloaded {
                if !chosen {
                    Button("Выбрать") { settings.voiceModel = model.id }
                }
                Button(role: .destructive) {
                    downloads.delete(model)
                } label: {
                    Image(systemName: "trash")
                }
                .buttonStyle(.borderless)
                .help("Удалить модель")
            } else {
                Button("Скачать") { downloads.start(model) }
            }
        }
        .id("\(model.id)-\(downloads.revision)")
        .onChange(of: downloads.busy) { busy in
            // Скачалась, а модели ещё нет — сразу выбрать её.
            if !busy.contains(model.id), model.isDownloaded, settings.voiceModel.isEmpty {
                settings.voiceModel = model.id
            }
        }
    }
}
#endif
