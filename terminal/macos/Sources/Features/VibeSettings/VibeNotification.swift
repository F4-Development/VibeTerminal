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
        DispatchQueue.global(qos: .userInitiated).async {
            _ = VibeSocket.request(["session": session, "event": "open"], to: socket)
        }
    }

    private static func isVvSocket(_ path: String) -> Bool {
        VibeSocket.isVvSocket(path)
    }
}

/// Сокет окна vv (`~/.vibeterminal/run/<pid>.sock`): строка JSON туда,
/// ответ — всё, что vv пришлёт до закрытия. Блокирует — звать не с главного
/// потока.
enum VibeSocket {
    static func request(_ message: [String: Any], to socket: String, timeout: TimeInterval = 5) -> [String: Any]? {
        guard isVvSocket(socket),
              let line = try? JSONSerialization.data(withJSONObject: message) else { return nil }
        let fd = Darwin.socket(AF_UNIX, SOCK_STREAM, 0)
        guard fd >= 0 else { return nil }
        defer { close(fd) }

        var wait = timeval(tv_sec: Int(timeout), tv_usec: 0)
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &wait, socklen_t(MemoryLayout<timeval>.size))
        var address = sockaddr_un()
        address.sun_family = sa_family_t(AF_UNIX)
        let path = Array(socket.utf8CString)
        guard path.count <= MemoryLayout.size(ofValue: address.sun_path) else { return nil }
        withUnsafeMutableBytes(of: &address.sun_path) { destination in
            path.withUnsafeBytes { destination.copyMemory(from: $0) }
        }
        let connected = withUnsafePointer(to: &address) {
            $0.withMemoryRebound(to: sockaddr.self, capacity: 1) {
                connect(fd, $0, socklen_t(MemoryLayout<sockaddr_un>.size))
            }
        }
        guard connected == 0 else { return nil }
        var payload = line
        payload.append(0x0A)
        let sent = payload.withUnsafeBytes { write(fd, $0.baseAddress, $0.count) }
        guard sent == payload.count else { return nil }

        var reply = Data()
        var buffer = [UInt8](repeating: 0, count: 64 * 1024)
        while true {
            let count = read(fd, &buffer, buffer.count)
            if count <= 0 { break }
            reply.append(buffer, count: count)
        }
        guard !reply.isEmpty else { return [:] }
        return (try? JSONSerialization.jsonObject(with: reply)) as? [String: Any]
    }

    /// Только сокеты окон vv: `~/.vibeterminal/run/<pid>.sock`.
    static func isVvSocket(_ path: String) -> Bool {
        let run = FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".vibeterminal/run/").path
        return path.hasPrefix(run) && path.hasSuffix(".sock") && !path.contains("..")
    }
}
#endif
