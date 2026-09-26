#if os(macOS)
import AppKit

/// Действия vv с горячей клавишей. Сочетания выбирают на вкладке «Клавиши»,
/// в vv.json (`keys`) — только изменённые, пусто — выключено. Имена и
/// сочетания по умолчанию — как `BINDINGS` в src/hotkeys.rs.
enum VibeHotkey: String, CaseIterable, Identifiable {
    case voice, menu, new, next, previous, waiting, git, usage, rename, close

    var id: String { rawValue }

    var title: String {
        switch self {
        case .voice: "Голосовой ввод"
        case .menu: "Меню"
        case .new: "Новая сессия"
        case .next: "Следующая сессия"
        case .previous: "Предыдущая сессия"
        case .waiting: "К следующей, кто ждёт"
        case .git: "Ветки и git"
        case .usage: "Лимиты Claude"
        case .rename: "Переименовать сессию"
        case .close: "Закрыть сессию"
        }
    }

    var defaultCombo: String {
        switch self {
        case .voice: "cmd+shift+space"
        case .menu: "ctrl+\\"
        case .new: "cmd+t"
        case .next: "cmd+shift+]"
        case .previous: "cmd+shift+["
        case .waiting: "cmd+j"
        case .git: "cmd+b"
        case .usage: "cmd+l"
        case .rename: "cmd+r"
        case .close: "cmd+w"
        }
    }

    /// Место в `SLOTS` (src/hotkeys.rs) — какую служебную клавишу получит vv.
    var slot: Int {
        switch self {
        case .voice: 0
        case .menu: 2
        case .new: 3
        case .next: 4
        case .previous: 5
        case .waiting: 6
        case .git: 7
        case .usage: 8
        case .rename: 9
        case .close: 10
        }
    }

    /// Служебная клавиша по номеру: коды F13–F17 и модификаторы в том же
    /// порядке, что в src/hotkeys.rs.
    static func serviceText(slot: Int) -> String {
        let codes = [25, 26, 32, 33, 34]
        let modifiers = [1, 2, 5, 3, 6]
        let (code, modifier) = (codes[slot % 5], modifiers[slot / 5])
        return modifier == 1 ? "\\x1b[\(code)~" : "\\x1b[\(code);\(modifier)~"
    }

    static let voiceReleaseSlot = 1
    static func sessionSlot(_ index: Int) -> Int { 11 + index }

    /// Сессии 1–9 по номеру: в `keys` под этим именем — модификаторы для цифр.
    static let sessionsKey = "sessions"
    static let sessionsDefault = "cmd"

    func combo(in keys: [String: String]) -> String {
        keys[rawValue] ?? defaultCombo
    }

    static func sessions(in keys: [String: String]) -> String {
        keys[sessionsKey] ?? sessionsDefault
    }
}

/// Сочетание клавиш. Клавиша — по месту на клавиатуре (как в раскладке US),
/// поэтому работает и в русской раскладке. В vv.json — `cmd+shift+]`.
struct KeyCombo: Equatable {
    let key: String
    let modifiers: NSEvent.ModifierFlags

    static let relevant: NSEvent.ModifierFlags = [.control, .option, .shift, .command]

    init(key: String, modifiers: NSEvent.ModifierFlags) {
        self.key = key
        self.modifiers = modifiers.intersection(Self.relevant)
    }

    init?(_ text: String) {
        let parts: [Substring]
        var key: String
        if text.hasSuffix("++") {
            parts = text.dropLast(2).split(separator: "+")
            key = "+"
        } else {
            var all = text.split(separator: "+", omittingEmptySubsequences: false)
            guard let last = all.popLast(), !last.isEmpty else { return nil }
            parts = all
            key = String(last)
        }
        var modifiers: NSEvent.ModifierFlags = []
        for part in parts {
            switch part {
            case "cmd": modifiers.insert(.command)
            case "shift": modifiers.insert(.shift)
            case "alt": modifiers.insert(.option)
            case "ctrl": modifiers.insert(.control)
            default: return nil
            }
        }
        key = key.lowercased()
        guard Self.codes[key] != nil else { return nil }
        self.init(key: key, modifiers: modifiers)
    }

    /// Нажатие → сочетание; `nil` — клавиша, которой нет в таблице.
    init?(event: NSEvent) {
        guard let key = Self.names[event.keyCode] else { return nil }
        self.init(key: key, modifiers: event.modifierFlags)
    }

    var text: String {
        var parts: [String] = []
        if modifiers.contains(.control) { parts.append("ctrl") }
        if modifiers.contains(.option) { parts.append("alt") }
        if modifiers.contains(.shift) { parts.append("shift") }
        if modifiers.contains(.command) { parts.append("cmd") }
        return (parts + [key]).joined(separator: "+")
    }

    /// Как в меню macOS: `⇧⌘]`, `⌃\`, `⌘Space`.
    var display: String {
        var text = ""
        if modifiers.contains(.control) { text += "⌃" }
        if modifiers.contains(.option) { text += "⌥" }
        if modifiers.contains(.shift) { text += "⇧" }
        if modifiers.contains(.command) { text += "⌘" }
        return text + (Self.glyphs[key] ?? key.uppercased())
    }

    var keyCode: UInt16? { Self.codes[key] }

    /// Есть ⌘, ⌃ или ⌥ — иначе это просто ввод текста.
    var hasCommandModifier: Bool { !modifiers.isDisjoint(with: [.command, .control, .option]) }

    /// Коды клавиш macOS (kVK_ANSI_…) по имени в раскладке US.
    static let codes: [String: UInt16] = [
        "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9,
        "b": 11, "q": 12, "w": 13, "e": 14, "r": 15, "y": 16, "t": 17, "1": 18, "2": 19,
        "3": 20, "4": 21, "6": 22, "5": 23, "=": 24, "9": 25, "7": 26, "-": 27, "8": 28,
        "0": 29, "]": 30, "o": 31, "u": 32, "[": 33, "i": 34, "p": 35, "return": 36,
        "l": 37, "j": 38, "'": 39, "k": 40, ";": 41, "\\": 42, ",": 43, "/": 44, "n": 45,
        "m": 46, ".": 47, "tab": 48, "space": 49, "`": 50, "backspace": 51,
        "f1": 122, "f2": 120, "f3": 99, "f4": 118, "f5": 96, "f6": 97, "f7": 98, "f8": 100,
        "f9": 101, "f10": 109, "f11": 103, "f12": 111,
        "left": 123, "right": 124, "down": 125, "up": 126,
        "home": 115, "end": 119, "page_up": 116, "page_down": 121, "delete": 117,
    ]
    static let names: [UInt16: String] = Dictionary(uniqueKeysWithValues: codes.map { ($1, $0) })
    private static let glyphs: [String: String] = [
        "space": "Space", "return": "↩", "tab": "⇥", "backspace": "⌫", "delete": "⌦",
        "left": "←", "right": "→", "down": "↓", "up": "↑", "home": "↖", "end": "↘",
        "page_up": "⇞", "page_down": "⇟",
    ]
}
#endif
