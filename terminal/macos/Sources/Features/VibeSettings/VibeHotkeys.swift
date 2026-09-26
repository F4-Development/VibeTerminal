#if os(macOS)
import AppKit
import GhosttyKit

/// Горячие клавиши vv ловит само приложение: сочетания с ⌘ до программ в
/// терминале не доходят, а отпускание клавиши (голос «пока держишь»)
/// терминал не сообщает вовсе. vv получает служебную клавишу: F13–F17 без
/// модификаторов, с Shift, Ctrl, Option, Ctrl+Shift — по номеру `slot`.
/// Сочетания, занятые нашими действиями, снимаются с пунктов меню.
final class VibeHotkeys {
    static let shared = VibeHotkeys()

    private struct Binding {
        let combo: KeyCombo
        let slot: Int
        let hotkey: VibeHotkey?
    }

    private var monitor: Any?
    private var bindings: [UInt16: [Binding]] = [:]
    /// Клавиша голосового ввода зажата в этом терминале — ему и сообщим,
    /// что отпустили (даже если модификаторы отпустили раньше).
    private weak var holding: Ghostty.SurfaceView?
    private var holdingKey: UInt16?
    /// Пункты меню, у которых мы забрали сочетание, — чтобы вернуть.
    private var taken: [(item: NSMenuItem, key: String, mask: NSEvent.ModifierFlags)] = []

    func install() {
        guard monitor == nil else { return }
        reload()
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp]) { [weak self] event in
            self?.handle(event) ?? event
        }
    }

    /// Сочетания поменялись в настройках.
    func reload() {
        let keys = VvSettings.load().keys
        var all: [Binding] = VibeHotkey.allCases.compactMap { hotkey in
            KeyCombo(hotkey.combo(in: keys)).map { Binding(combo: $0, slot: hotkey.slot, hotkey: hotkey) }
        }
        if let mods = KeyCombo(VibeHotkey.sessions(in: keys) + "+1")?.modifiers, !mods.isEmpty {
            all += (0..<9).map { Binding(combo: KeyCombo(key: "\($0 + 1)", modifiers: mods), slot: VibeHotkey.sessionSlot($0), hotkey: nil) }
        }
        bindings = Dictionary(grouping: all.filter { $0.combo.keyCode != nil }, by: { $0.combo.keyCode! })
        syncMenu()
    }

    /// Свободно ли сочетание от наших действий (для пунктов меню).
    func isTaken(_ combo: KeyCombo) -> Bool {
        guard let code = combo.keyCode else { return false }
        return bindings[code]?.contains { $0.combo == combo } == true
    }

    private func handle(_ event: NSEvent) -> NSEvent? {
        if event.type == .keyUp {
            guard event.keyCode == holdingKey, let surface = holding else { return event }
            holding = nil
            holdingKey = nil
            send(slot: VibeHotkey.voiceReleaseSlot, to: surface)
            return nil
        }
        let mods = event.modifierFlags.intersection(KeyCombo.relevant)
        guard let binding = bindings[event.keyCode]?.first(where: { $0.combo.modifiers == mods }) else { return event }
        // Не в терминале (окно настроек, запись сочетания) — не наше дело.
        guard let surface = (NSApp.keyWindow?.windowController as? BaseTerminalController)?.focusedSurface else {
            return event
        }
        if event.isARepeat {
            // Держат «следующую сессию» — листаем дальше; остальное — одно нажатие.
            guard binding.hotkey == .next || binding.hotkey == .previous else { return nil }
        } else if binding.hotkey == .voice {
            holding = surface
            holdingKey = event.keyCode
        }
        send(slot: binding.slot, to: surface)
        return nil
    }

    private func send(slot: Int, to view: Ghostty.SurfaceView) {
        guard let surface = view.surface else { return }
        let action = "text:" + VibeHotkey.serviceText(slot: slot)
        _ = ghostty_surface_binding_action(surface, action, UInt(action.lengthOfBytes(using: .utf8)))
    }

    /// Сочетания наших действий — не у пунктов меню: иначе там написано
    /// «Новая вкладка ⌘T», а ⌘T открывает сессию. Зовётся и после того, как
    /// меню заново настроил сам терминал.
    func syncMenu() {
        for (item, key, mask) in taken where item.keyEquivalent.isEmpty {
            item.keyEquivalent = key
            item.keyEquivalentModifierMask = mask
        }
        taken.removeAll()
        guard let menu = NSApp.mainMenu else { return }
        var queue = menu.items
        while let item = queue.popLast() {
            if let submenu = item.submenu { queue += submenu.items }
            guard let combo = Self.combo(of: item), isTaken(combo) else { continue }
            taken.append((item, item.keyEquivalent, item.keyEquivalentModifierMask))
            item.keyEquivalent = ""
            item.keyEquivalentModifierMask = []
        }
    }

    private static func combo(of item: NSMenuItem) -> KeyCombo? {
        let equivalent = item.keyEquivalent
        guard equivalent.count == 1 else { return nil }
        var mods = item.keyEquivalentModifierMask
        var key = equivalent
        // Заглавная буква в меню — это ещё и Shift.
        if key != key.lowercased() {
            mods.insert(.shift)
            key = key.lowercased()
        }
        // `{` и `}` — это Shift+[ и Shift+].
        let shifted: [String: String] = ["{": "[", "}": "]", "+": "=", "_": "-", "|": "\\", ":": ";", "<": ",", ">": ".", "?": "/"]
        if let base = shifted[key] {
            mods.insert(.shift)
            key = base
        }
        if key == " " { key = "space" }
        guard KeyCombo.codes[key] != nil else { return nil }
        return KeyCombo(key: key, modifiers: mods)
    }
}
#endif
