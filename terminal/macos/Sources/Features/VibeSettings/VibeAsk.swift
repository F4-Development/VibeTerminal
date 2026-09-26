#if os(macOS)
import AppKit
import SwiftUI

/// Всплывающее окно для ответа Claude, пока ты в другом приложении: разрешить,
/// ответить на вопрос, утвердить план — не переключаясь в терминал.
///
/// vv присылает OSC 777 с меткой `⟦vv-ask:сессия:запрос:сокет⟧` вместо
/// баннера. Описание запроса окно забирает у vv по его сокету, туда же
/// отправляет ответ, и раз в секунду проверяет, не ответили ли уже в
/// терминале — тогда закрывается само.
struct VibeAsk: Equatable {
    let session: Int
    let request: Int
    let socket: String

    init?(title: String) {
        guard title.hasPrefix("⟦vv-ask:"), let end = title.firstIndex(of: "⟧") else { return nil }
        let marker = title[title.index(title.startIndex, offsetBy: 8)..<end]
        let parts = marker.split(separator: ":", maxSplits: 2)
        guard parts.count == 3, let session = Int(parts[0]), let request = Int(parts[1]) else { return nil }
        let socket = String(parts[2])
        guard VibeSocket.isVvSocket(socket) else { return nil }
        self.session = session
        self.request = request
        self.socket = socket
    }
}

/// Что показать — так, как описал vv.
struct VibeAskInfo {
    enum Kind { case permission, question, plan }

    struct Question {
        let question: String
        let header: String
        let multi: Bool
        let options: [(label: String, description: String)]
    }

    let kind: Kind
    let title: String
    let session: String
    let place: String
    let detail: String
    let note: String
    /// Подпись «больше не спрашивать» — если Claude предлагает правило.
    let always: String?
    let plan: String
    let questions: [Question]

    init?(_ json: [String: Any]) {
        let text = { (key: String) in json[key] as? String ?? "" }
        switch text("kind") {
        case "permission": kind = .permission
        case "question": kind = .question
        case "plan": kind = .plan
        default: return nil
        }
        title = text("title")
        session = text("session")
        place = text("place")
        detail = text("detail")
        note = text("note")
        always = json["always"] as? String
        plan = text("plan")
        questions = (json["questions"] as? [[String: Any]] ?? []).map { q in
            Question(
                question: q["question"] as? String ?? "",
                header: q["header"] as? String ?? "",
                multi: q["multi"] as? Bool ?? false,
                options: (q["options"] as? [[String: Any]] ?? []).map {
                    (label: $0["label"] as? String ?? "", description: $0["description"] as? String ?? "")
                })
        }
    }
}

/// Показывает окна по одному: пока открыто одно, следующие ждут.
final class VibeAskPanel {
    static let shared = VibeAskPanel()

    private var panel: NSPanel?
    private var current: VibeAsk?
    private var queue: [VibeAsk] = []
    private var poll: Timer?
    private var activeObserver: Any?

    func show(_ ask: VibeAsk) {
        if current == ask || queue.contains(ask) { return }
        if current != nil {
            queue.append(ask)
            return
        }
        present(ask)
    }

    private func present(_ ask: VibeAsk) {
        current = ask
        DispatchQueue.global(qos: .userInitiated).async {
            let reply = VibeSocket.request(["event": "ask", "session": ask.session, "request": ask.request], to: ask.socket)
            DispatchQueue.main.async {
                guard self.current == ask else { return }
                guard let reply, let info = VibeAskInfo(reply) else {
                    // Уже ответили или vv закрылся — к следующему.
                    self.finish()
                    return
                }
                self.open(ask, info)
            }
        }
    }

    private func open(_ ask: VibeAsk, _ info: VibeAskInfo) {
        let view = VibeAskView(
            info: info,
            answer: { [weak self] answer, done in self?.send(answer, for: ask, done: done) },
            close: { [weak self] in self?.finish() },
            openTerminal: { [weak self] in
                VibeNotification.open(session: ask.session, socket: ask.socket)
                NSApp.activate(ignoringOtherApps: true)
                self?.finish()
            })
        let host = NSHostingController(rootView: view)
        let panel = NSPanel(
            contentRect: NSRect(x: 0, y: 0, width: 440, height: 200),
            styleMask: [.titled, .closable, .nonactivatingPanel, .fullSizeContentView],
            backing: .buffered, defer: false)
        panel.contentViewController = host
        panel.titleVisibility = .hidden
        panel.titlebarAppearsTransparent = true
        panel.standardWindowButton(.closeButton)?.isHidden = true
        panel.standardWindowButton(.miniaturizeButton)?.isHidden = true
        panel.standardWindowButton(.zoomButton)?.isHidden = true
        panel.isMovableByWindowBackground = true
        // Поверх всех программ, на любом рабочем столе, даже поверх
        // полноэкранного браузера, — и не отбирает фокус у того, где ты.
        panel.isFloatingPanel = true
        panel.level = .floating
        panel.hidesOnDeactivate = false
        panel.becomesKeyOnlyIfNeeded = false
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.setContentSize(host.view.fittingSize)
        place(panel)
        panel.orderFrontRegardless()
        panel.makeKey()
        self.panel = panel

        // Ответили в терминале — окно больше не нужно.
        poll = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in self?.checkGone(ask) }
        // Вернулся в VibeTerminal — там тот же вопрос, окно не нужно.
        activeObserver = NotificationCenter.default.addObserver(
            forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main
        ) { [weak self] _ in
            self?.queue.removeAll()
            self?.finish()
        }
    }

    /// Справа вверху экрана, где сейчас указатель.
    private func place(_ panel: NSPanel) {
        let mouse = NSEvent.mouseLocation
        let screen = NSScreen.screens.first { NSMouseInRect(mouse, $0.frame, false) } ?? NSScreen.main
        guard let visible = screen?.visibleFrame else { return }
        let size = panel.frame.size
        panel.setFrameOrigin(NSPoint(x: visible.maxX - size.width - 16, y: visible.maxY - size.height - 16))
    }

    private func checkGone(_ ask: VibeAsk) {
        DispatchQueue.global(qos: .utility).async {
            let reply = VibeSocket.request(["event": "ask", "session": ask.session, "request": ask.request], to: ask.socket, timeout: 2)
            let gone = reply == nil || reply?["gone"] as? Bool == true
            if gone {
                DispatchQueue.main.async {
                    if self.current == ask { self.finish() }
                }
            }
        }
    }

    private func send(_ answer: [String: Any], for ask: VibeAsk, done: @escaping (Bool) -> Void) {
        DispatchQueue.global(qos: .userInitiated).async {
            let reply = VibeSocket.request(
                ["event": "ask-answer", "session": ask.session, "request": ask.request, "answer": answer], to: ask.socket)
            let ok = reply?["ok"] as? Bool == true
            DispatchQueue.main.async {
                done(ok)
                if ok { self.finish() }
            }
        }
    }

    /// Закрыть это окно и показать следующее, если ждёт.
    private func finish() {
        poll?.invalidate()
        poll = nil
        if let activeObserver { NotificationCenter.default.removeObserver(activeObserver) }
        activeObserver = nil
        panel?.orderOut(nil)
        panel = nil
        current = nil
        if !queue.isEmpty {
            present(queue.removeFirst())
        }
    }
}

// MARK: - Содержимое окна

struct VibeAskView: View {
    let info: VibeAskInfo
    /// Ответ для vv; в замыкание — принят ли он.
    let answer: ([String: Any], @escaping (Bool) -> Void) -> Void
    let close: () -> Void
    let openTerminal: () -> Void

    @State private var dontAskAgain = false
    @State private var picked: [Int: Set<String>] = [:]
    @State private var own: [Int: String] = [:]
    @State private var feedback = ""
    @State private var sending = false
    @State private var failed = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            header
            Text(info.title).font(.title3.weight(.semibold))
            switch info.kind {
            case .permission: permission
            case .question: questions
            case .plan: plan
            }
            if failed {
                Label("Не получилось — ответь в терминале", systemImage: "exclamationmark.triangle.fill")
                    .foregroundStyle(.orange)
                    .font(.callout)
            }
            footer
        }
        .padding(18)
        .frame(width: 440)
        // Заголовка у окна нет — место под него не нужно.
        .ignoresSafeArea(edges: .top)
        .disabled(sending)
    }

    private var header: some View {
        HStack(spacing: 10) {
            Image(nsImage: NSApp.applicationIconImage)
                .resizable()
                .frame(width: 28, height: 28)
            VStack(alignment: .leading, spacing: 1) {
                Text(info.session).font(.headline)
                Text(info.place).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.middle)
            }
            Spacer()
            Button(action: close) {
                Image(systemName: "xmark.circle.fill")
                    .font(.title3)
                    .foregroundStyle(.secondary)
            }
            .buttonStyle(.plain)
            .help("Закрыть — ответить позже в терминале")
        }
    }

    // MARK: Разрешение

    private var permission: some View {
        VStack(alignment: .leading, spacing: 10) {
            if !info.detail.isEmpty {
                Text(info.detail)
                    .font(.system(.body, design: .monospaced))
                    .textSelection(.enabled)
                    .lineLimit(8)
                    .padding(10)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.06)))
            }
            if !info.note.isEmpty {
                Text(info.note).foregroundStyle(.secondary)
            }
            if let always = info.always {
                Toggle(isOn: $dontAskAgain) {
                    Text("Больше не спрашивать")
                    Text(always)
                }
                .toggleStyle(.checkbox)
            }
            HStack {
                Spacer()
                Button("Отклонить") { reply(["choice": "deny"]) }
                    .keyboardShortcut(.cancelAction)
                Button("Разрешить") { reply(["choice": dontAskAgain ? "always" : "allow"]) }
                    .keyboardShortcut(.defaultAction)
            }
        }
    }

    // MARK: Вопросы

    private var questions: some View {
        VStack(alignment: .leading, spacing: 16) {
            ForEach(Array(info.questions.enumerated()), id: \.offset) { index, question in
                VStack(alignment: .leading, spacing: 8) {
                    if !question.header.isEmpty {
                        Text(question.header.uppercased())
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.secondary)
                    }
                    Text(question.question).font(.body.weight(.medium))
                    ForEach(question.options, id: \.label) { option in
                        optionRow(index, question.multi, option.label, option.description)
                    }
                    TextField("Свой ответ", text: Binding(get: { own[index] ?? "" }, set: { own[index] = $0 }))
                        .textFieldStyle(.roundedBorder)
                }
            }
            HStack {
                Spacer()
                Button("Отправить") { reply(["answers": answers]) }
                    .keyboardShortcut(.defaultAction)
                    .disabled(!allAnswered)
            }
        }
    }

    private func optionRow(_ index: Int, _ multi: Bool, _ label: String, _ description: String) -> some View {
        let chosen = picked[index, default: []].contains(label)
        let icon = multi ? (chosen ? "checkmark.square.fill" : "square") : (chosen ? "largecircle.fill.circle" : "circle")
        return Button {
            var set = picked[index, default: []]
            if multi {
                if chosen { set.remove(label) } else { set.insert(label) }
            } else {
                set = [label]
            }
            picked[index] = set
        } label: {
            HStack(alignment: .firstTextBaseline, spacing: 8) {
                Image(systemName: icon).foregroundStyle(chosen ? Color.accentColor : .secondary)
                VStack(alignment: .leading, spacing: 2) {
                    Text(label)
                    if !description.isEmpty {
                        Text(description).font(.caption).foregroundStyle(.secondary)
                    }
                }
                Spacer(minLength: 0)
            }
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
    }

    /// Вопрос → выбранные варианты через запятую и свой ответ.
    private var answers: [String: String] {
        var out: [String: String] = [:]
        for (index, question) in info.questions.enumerated() {
            var parts = question.options.map(\.label).filter { picked[index, default: []].contains($0) }
            let custom = (own[index] ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
            if !custom.isEmpty { parts.append(custom) }
            out[question.question] = parts.joined(separator: ", ")
        }
        return out
    }

    private var allAnswered: Bool {
        !info.questions.isEmpty && answers.values.allSatisfy { !$0.isEmpty }
    }

    // MARK: План

    private var plan: some View {
        VStack(alignment: .leading, spacing: 10) {
            ScrollView {
                Text(planText)
                    .textSelection(.enabled)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(10)
            }
            .frame(maxHeight: 260)
            .background(RoundedRectangle(cornerRadius: 8).fill(Color.primary.opacity(0.06)))
            TextField("Что поправить — если отправляешь на доработку", text: $feedback, axis: .vertical)
                .textFieldStyle(.roundedBorder)
                .lineLimit(1...3)
            HStack {
                Spacer()
                Button("На доработку") { reply(["choice": "revise", "feedback": feedback]) }
                Button("Утвердить план") { reply(["choice": "approve"]) }
                    .keyboardShortcut(.defaultAction)
            }
        }
    }

    /// План в Markdown: заголовки `#` — жирным, остальное как есть.
    private var planText: AttributedString {
        let lines = info.plan.components(separatedBy: "\n").map { line -> String in
            let trimmed = line.trimmingCharacters(in: .whitespaces)
            guard trimmed.hasPrefix("#") else { return line }
            let heading = trimmed.drop { $0 == "#" }.trimmingCharacters(in: .whitespaces)
            return heading.isEmpty ? "" : "**\(heading)**"
        }
        let options = AttributedString.MarkdownParsingOptions(interpretedSyntax: .inlineOnlyPreservingWhitespace)
        let text = lines.joined(separator: "\n")
        return (try? AttributedString(markdown: text, options: options)) ?? AttributedString(info.plan)
    }

    // MARK: Низ

    private var footer: some View {
        Button("Открыть в VibeTerminal", action: openTerminal)
            .buttonStyle(.link)
            .font(.caption)
    }

    private func reply(_ value: [String: Any]) {
        sending = true
        failed = false
        answer(value) { ok in
            sending = false
            failed = !ok
        }
    }
}
#endif
