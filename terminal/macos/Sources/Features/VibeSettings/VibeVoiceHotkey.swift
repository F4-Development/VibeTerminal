#if os(macOS)
import AppKit
import GhosttyKit

/// ⌘⇧Space — голосовой ввод vv. Сочетания с ⌘ до программ в терминале не
/// доходят, а для режима «удерживать клавишу» нужно ещё и отпускание — его
/// терминал программе вообще не сообщает. Поэтому клавишу ловит само
/// приложение и отдаёт vv в открытом терминале служебные клавиши:
/// F13 — нажали, F14 — отпустили.
final class VibeVoiceHotkey {
    static let shared = VibeVoiceHotkey()

    private static let spaceKey: UInt16 = 49
    private static let pressed = "text:\\x1b[25~"
    private static let released = "text:\\x1b[26~"

    private var monitor: Any?
    /// Клавиша зажата в этом терминале — ему и сообщим, что отпустили (даже
    /// если ⌘ отпустили раньше пробела).
    private weak var holding: Ghostty.SurfaceView?

    func install() {
        guard monitor == nil else { return }
        monitor = NSEvent.addLocalMonitorForEvents(matching: [.keyDown, .keyUp]) { [weak self] event in
            guard let self else { return event }
            return self.handle(event)
        }
    }

    private func handle(_ event: NSEvent) -> NSEvent? {
        guard event.keyCode == Self.spaceKey else { return event }
        if event.type == .keyUp {
            guard let surface = holding else { return event }
            holding = nil
            send(Self.released, to: surface)
            return nil
        }
        let mods = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        guard mods.contains([.command, .shift]), mods.isDisjoint(with: [.option, .control]) else { return event }
        // Автоповтор, пока держат, — не новое нажатие.
        if event.isARepeat { return holding == nil ? event : nil }
        // Не в терминале (например, в окне настроек) — не наше дело.
        guard let surface = (NSApp.keyWindow?.windowController as? BaseTerminalController)?.focusedSurface else {
            return event
        }
        holding = surface
        send(Self.pressed, to: surface)
        return nil
    }

    private func send(_ action: String, to view: Ghostty.SurfaceView) {
        guard let surface = view.surface else { return }
        _ = ghostty_surface_binding_action(surface, action, UInt(action.lengthOfBytes(using: .utf8)))
    }
}
#endif
