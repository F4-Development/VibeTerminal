#if os(macOS)
import AppKit
import SwiftUI

/// Вкладка «Клавиши»: сочетание на каждое действие vv. Нажал на сочетание —
/// нажимаешь новое; Esc — отмена, ⌫ — убрать.
struct KeySettings: View {
    @State private var settings = VvSettings.load()
    @StateObject private var recorder = KeyRecorder()

    var body: some View {
        Form {
            Section {
                row(.new)
                row(.next)
                row(.previous)
                row(.waiting)
                row(.rename)
                row(.close)
                Toggle("Сессия по номеру — \(sessionsExample)", isOn: sessionsByNumber)
            } header: {
                Text("Сессии")
            }

            Section {
                row(.menu)
                row(.git)
                row(.usage)
                row(.voice)
            } header: {
                Text("Окна vv и голос")
            } footer: {
                VStack(alignment: .leading, spacing: 6) {
                    if let note = recorder.note {
                        Label(note, systemImage: "exclamationmark.circle")
                            .font(.callout)
                            .foregroundStyle(.orange)
                    }
                    Hint("Нажми на сочетание, потом новое: Esc — отмена, ⌫ — убрать. ⌃\\ открывает меню всегда. Сочетания работают в окнах VibeTerminal, в другом терминале — только с ⌃ и ⌥. Как записывать голос — на вкладке «Голос».")
                    Button("Вернуть все по умолчанию") {
                        settings.keys.removeAll()
                        recorder.note = nil
                    }
                    .disabled(settings.keys.isEmpty)
                }
                .multilineTextAlignment(.leading)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
        .formStyle(.grouped)
        .onAppear { settings = VvSettings.load() }
        .onDisappear { recorder.stop() }
        .onChange(of: settings) { newValue in
            newValue.save()
            VibeHotkeys.shared.reload()
        }
    }

    private func row(_ hotkey: VibeHotkey) -> some View {
        let combo = KeyCombo(hotkey.combo(in: settings.keys))
        let isDefault = combo == KeyCombo(hotkey.defaultCombo)
        return LabeledContent(hotkey.title) {
            HStack(spacing: 6) {
                KeyField(
                    text: recorder.recording == hotkey.rawValue ? recorder.live : combo?.display,
                    recording: recorder.recording == hotkey.rawValue
                ) {
                    if recorder.recording == hotkey.rawValue {
                        recorder.stop()
                    } else {
                        recorder.start(hotkey.rawValue) { assign($0, to: hotkey) }
                    }
                }
                Button {
                    settings.keys.removeValue(forKey: hotkey.rawValue)
                    recorder.note = nil
                } label: {
                    Image(systemName: "arrow.counterclockwise")
                }
                .buttonStyle(.borderless)
                .help("По умолчанию: \(KeyCombo(hotkey.defaultCombo)?.display ?? "")")
                .opacity(isDefault ? 0 : 1)
                .disabled(isDefault)
            }
        }
    }

    /// Записали сочетание (`nil` — убрать). Занято другим действием — там
    /// становится пусто, и об этом пишем.
    private func assign(_ combo: KeyCombo?, to hotkey: VibeHotkey) {
        recorder.note = nil
        guard let combo else {
            settings.keys[hotkey.rawValue] = ""
            return
        }
        if let mods = KeyCombo(VibeHotkey.sessions(in: settings.keys) + "+1")?.modifiers, !mods.isEmpty,
           combo.modifiers == mods, ("1"..."9").contains(combo.key) {
            recorder.note = "\(combo.display) открывает сессию по номеру — выбери другое"
            return
        }
        for other in VibeHotkey.allCases where other != hotkey && KeyCombo(other.combo(in: settings.keys)) == combo {
            settings.keys[other.rawValue] = ""
            recorder.note = "\(combo.display) было у «\(other.title)» — там теперь пусто"
        }
        settings.keys[hotkey.rawValue] = combo == KeyCombo(hotkey.defaultCombo) ? nil : combo.text
    }

    private var sessionsExample: String {
        let mods = KeyCombo(VibeHotkey.sessionsDefault + "+1")?.display.dropLast() ?? "⌘"
        return "\(mods)1 … \(mods)9"
    }

    private var sessionsByNumber: Binding<Bool> {
        Binding(
            get: { !VibeHotkey.sessions(in: settings.keys).isEmpty },
            set: { settings.keys[VibeHotkey.sessionsKey] = $0 ? nil : "" })
    }
}

/// Поле сочетания как в настройках клавиатуры macOS.
private struct KeyField: View {
    let text: String?
    let recording: Bool
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Text(text ?? (recording ? "Нажми сочетание…" : "—"))
                .font(.system(.body, design: .rounded))
                .monospacedDigit()
                .foregroundStyle(text == nil && !recording ? .secondary : .primary)
                .frame(minWidth: 118)
                .padding(.vertical, 3)
                .padding(.horizontal, 8)
                .background(
                    RoundedRectangle(cornerRadius: 6)
                        .fill(recording ? Color.accentColor.opacity(0.15) : Color.primary.opacity(0.06)))
                .overlay(
                    RoundedRectangle(cornerRadius: 6)
                        .strokeBorder(recording ? Color.accentColor : .clear, lineWidth: 1.5))
        }
        .buttonStyle(.plain)
        .help(recording ? "Esc — отмена, ⌫ — убрать" : "Нажми, чтобы задать другое сочетание")
    }
}

/// Запись сочетания: пока идёт, все нажатия в окне настроек — сюда.
final class KeyRecorder: ObservableObject {
    @Published private(set) var recording: String?
    /// Что держат прямо сейчас: `⌘⇧…`.
    @Published private(set) var live: String?
    @Published var note: String?

    private var monitor: Any?
    private var done: ((KeyCombo?) -> Void)?

    /// Системные сочетания и нужные Claude — не забираем.
    private static let reserved: Set<String> = [
        "cmd+q", "cmd+h", "alt+cmd+h", "cmd+m", "cmd+,", "cmd+c", "cmd+v", "cmd+x", "cmd+a",
        "cmd+z", "shift+cmd+z", "cmd+tab", "cmd+`", "ctrl+c", "ctrl+d",
    ]

    func start(_ name: String, done: @escaping (KeyCombo?) -> Void) {
        stop()
        recording = name
        live = nil
        note = nil
        self.done = done
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .flagsChanged]) { [weak self] event in
            self?.handle(event)
            return nil
        }
    }

    func stop() {
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
        recording = nil
        live = nil
        done = nil
    }

    private func handle(_ event: NSEvent) {
        let mods = event.modifierFlags.intersection(KeyCombo.relevant)
        if event.type == .flagsChanged {
            live = mods.isEmpty ? nil : KeyCombo(key: "", modifiers: mods).display + "…"
            return
        }
        switch (event.keyCode, mods.isEmpty) {
        case (53, true):
            stop()
            return
        case (51, true), (117, true):
            finish(nil)
            return
        default:
            break
        }
        guard let combo = KeyCombo(event: event) else {
            note = "Эту клавишу назначить нельзя"
            return
        }
        guard combo.hasCommandModifier else {
            note = "Нужна клавиша вместе с ⌘, ⌃ или ⌥"
            return
        }
        guard !Self.reserved.contains(combo.text) else {
            note = "\(combo.display) занято системой или Claude — выбери другое"
            return
        }
        finish(combo)
    }

    private func finish(_ combo: KeyCombo?) {
        let done = done
        stop()
        done?(combo)
    }
}
#endif
