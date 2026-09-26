#if os(macOS)
import Darwin
import Foundation

/// Уведомление от vv. vv пишет в терминал обычное OSC 777, а в начале
/// заголовка — метку `⟦vv:сессия:сокет⟧`. Метку убираем, звук не ставим
/// (vv играет выбранный в настройках сам), а по клику просим vv открыть
/// эту сессию.
struct VibeNotification {
    let title: String
    let session: Int
    let socket: String

    init?(title: String) {
        guard title.hasPrefix("⟦vv:"), let end = title.firstIndex(of: "⟧") else { return nil }
        let marker = title[title.index(title.startIndex, offsetBy: 4)..<end]
        guard let colon = marker.firstIndex(of: ":"),
              let session = Int(marker[..<colon]) else { return nil }
        let socket = String(marker[marker.index(after: colon)...])
        guard Self.isVvSocket(socket) else { return nil }
        self.session = session
        self.socket = socket
        self.title = String(title[title.index(after: end)...])
    }

    /// Одна сессия — одно уведомление: новое заменяет старое.
    var identifier: String { "vv-\(socket)-\(session)" }

    /// Попросить vv открыть сессию: одна строка JSON в сокет его окна.
    static func open(session: Int, socket: String) {
        guard isVvSocket(socket) else { return }
        DispatchQueue.global(qos: .userInitiated).async {
            let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
            guard fd >= 0 else { return }
            defer { close(fd) }

            var address = sockaddr_un()
            address.sun_family = sa_family_t(AF_UNIX)
            let path = Array(socket.utf8CString)
            guard path.count <= MemoryLayout.size(ofValue: address.sun_path) else { return }
            withUnsafeMutableBytes(of: &address.sun_path) { destination in
                path.withUnsafeBytes { destination.copyMemory(from: $0) }
            }
            let connected = withUnsafePointer(to: &address) {
                $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                    connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
                }
            }
            guard connected == 0 else { return }
            let line = "{\"session\":\(session),\"event\":\"open\"}\n"
            _ = line.withCString { write(fd, $0, strlen($0)) }
        }
    }

    /// Только сокеты окон vv: `~/.vibeterminal/run/<pid>.sock`.
    private static func isVvSocket(_ path: String) -> Bool {
        let run = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".vibeterminal/run/").path
        return path.hasPrefix(run) && path.hasSuffix(".sock") && !path.contains("..")
    }
}
#endif
