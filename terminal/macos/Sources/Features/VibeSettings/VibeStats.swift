#if os(macOS)
import AppKit
import Charts
import SwiftUI

/// Окно «Статистика Claude» (Справка → Статистика Claude…): сколько токенов
/// ушло на Claude и во что это обошлось бы по ценам API — по дням, моделям,
/// проектам и сессиям. Считает vv (`vv stats`) по логам Claude Code на этом
/// Маке, окно только рисует.
final class VibeStatsController: NSWindowController {
    private static var shared: VibeStatsController?
    private let loader = ClaudeStatsLoader()

    static func show() {
        let controller = shared ?? VibeStatsController()
        shared = controller
        controller.loader.load()
        controller.showWindow(nil)
        controller.window?.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    private init() {
        let host = NSHostingController(rootView: ClaudeStatsView(loader: loader))
        host.sizingOptions = []
        let window = NSWindow(contentViewController: host)
        window.title = "Статистика Claude"
        window.styleMask = [.titled, .closable, .miniaturizable, .resizable]
        window.setContentSize(NSSize(width: 780, height: 780))
        window.contentMinSize = NSSize(width: 660, height: 480)
        super.init(window: window)
        window.center()
        window.setFrameAutosaveName("VibeClaudeStats")
    }

    required init?(coder: NSCoder) {
        fatalError("не используется")
    }
}

/// Зовёт `vv stats` в фоне. Пока считает, в окне остаются прежние цифры.
final class ClaudeStatsLoader: ObservableObject {
    @Published private(set) var report: ClaudeStats.Report?
    @Published private(set) var loading = false
    @Published private(set) var error: String?

    func load() {
        guard !loading else { return }
        loading = true
        DispatchQueue.global(qos: .userInitiated).async {
            let result = Self.run()
            DispatchQueue.main.async {
                self.loading = false
                switch result {
                case .success(let report):
                    self.report = report
                    self.error = nil
                case .failure(let error):
                    self.error = error.localizedDescription
                }
            }
        }
    }

    private static func run() -> Result<ClaudeStats.Report, Error> {
        // vv лежит в приложении рядом с терминалом: Contents/MacOS/vv.
        guard let vv = Bundle.main.url(forAuxiliaryExecutable: "vv") else {
            return .failure(StatsError("vv не найден в приложении"))
        }
        let process = Process()
        process.executableURL = vv
        process.arguments = ["stats"]
        let output = Pipe()
        process.standardOutput = output
        process.standardError = FileHandle.nullDevice
        do {
            try process.run()
            let data = output.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            guard process.terminationStatus == 0 else { return .failure(StatsError("vv не смог посчитать статистику")) }
            let decoder = JSONDecoder()
            decoder.keyDecodingStrategy = .convertFromSnakeCase
            return .success(try decoder.decode(ClaudeStats.Report.self, from: data))
        } catch is DecodingError {
            return .failure(StatsError("Не разобрал ответ vv — обнови VibeTerminal"))
        } catch {
            return .failure(error)
        }
    }
}

private struct StatsError: LocalizedError {
    let errorDescription: String?

    init(_ text: String) {
        errorDescription = text
    }
}

/// Что печатает `vv stats` (src/stats.rs). Время — секунды Unix, деньги —
/// доллары по ценам API.
enum ClaudeStats {
    struct Report: Decodable {
        /// Самый ранний ответ в логах: что раньше, Claude уже удалил.
        let since: Int?
        let generated: Int
        let periods: [Period]
    }

    struct Period: Decodable, Identifiable {
        let key: String
        let title: String
        let from: Int
        let to: Int
        /// `hour` — столбики по часам (сегодня), `day` — по дням.
        let unit: String
        let cost: Double
        let costWithoutCache: Double
        let subagentCost: Double
        let costs: Costs
        let tokens: Tokens
        let responses: Int
        let sessions: Int
        let activeDays: Int
        let models: [Model]
        let projects: [Project]
        let topSessions: [Session]
        let chart: [Point]

        var id: String { key }
        var byHour: Bool { unit == "hour" }
    }

    struct Tokens: Decodable {
        let input: Int
        let output: Int
        let cacheWrite5m: Int
        let cacheWrite1h: Int
        let cacheRead: Int
        let searches: Int

        var cacheWrite: Int { cacheWrite5m + cacheWrite1h }
        /// Всё, что Claude прочитал: и из кэша, и мимо него.
        var read: Int { input + cacheWrite + cacheRead }

        enum CodingKeys: String, CodingKey {
            case input, output, cacheRead, searches
            // convertFromSnakeCase делает из `cache_write_5m` «cacheWrite5M».
            case cacheWrite5m = "cacheWrite5M"
            case cacheWrite1h = "cacheWrite1H"
        }
    }

    struct Costs: Decodable {
        let input: Double
        let output: Double
        let cacheWrite: Double
        let cacheRead: Double
        let searches: Double
    }

    struct Model: Decodable, Identifiable {
        let id: String
        let name: String
        /// Модели нет в таблице цен vv — у неё только токены.
        let priced: Bool
        let cost: Double
        let responses: Int
        let tokens: Tokens
    }

    struct Project: Decodable, Identifiable {
        let name: String
        /// Пусто — проект знаем только по копиям от vv.
        let path: String
        let cost: Double
        let responses: Int
        let sessions: Int
        let output: Int
        let last: Int

        var id: String { name }
    }

    struct Session: Decodable, Identifiable {
        let id: String
        let title: String
        let project: String
        let cost: Double
        let responses: Int
        let first: Int
        let last: Int
    }

    struct Point: Decodable {
        let at: Int
        let model: String
        let cost: Double

        var date: Date { Date(timeIntervalSince1970: TimeInterval(at)) }
    }
}

// MARK: - Окно

struct ClaudeStatsView: View {
    @ObservedObject var loader: ClaudeStatsLoader
    @State private var periodKey = "week"

    var body: some View {
        Group {
            if let report = loader.report, let period = report.periods.first(where: { $0.key == periodKey }) ?? report.periods.first {
                content(report, period)
            } else if let error = loader.error {
                placeholder { Label(error, systemImage: "exclamationmark.triangle") }
            } else {
                placeholder { ProgressView("Считаю по логам Claude…") }
            }
        }
        .frame(minWidth: 660, minHeight: 480)
    }

    private func placeholder(@ViewBuilder _ content: () -> some View) -> some View {
        content().foregroundStyle(.secondary).frame(maxWidth: .infinity, maxHeight: .infinity)
    }

    private func content(_ report: ClaudeStats.Report, _ period: ClaudeStats.Period) -> some View {
        // Цвет модели — по её месту за всё время: при смене периода не меняется.
        let palette = ModelPalette(report.periods.last?.models ?? period.models)
        return ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                header(report)
                if period.responses == 0 {
                    Card(title: period.title) {
                        Text("Claude за это время не работал.").foregroundStyle(.secondary)
                    }
                } else {
                    Tiles(period: period)
                    ChartCard(period: period, palette: palette)
                    SpendCard(period: period)
                    ModelsCard(period: period, palette: palette)
                    ProjectsCard(period: period)
                    SessionsCard(period: period)
                }
                Hint(footer(report))
            }
            .padding(20)
        }
    }

    private func header(_ report: ClaudeStats.Report) -> some View {
        HStack(spacing: 10) {
            Picker("Период", selection: $periodKey) {
                ForEach(report.periods) { Text($0.title).tag($0.key) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()
            Spacer()
            if loader.loading {
                ProgressView().controlSize(.small)
            }
            Button {
                loader.load()
            } label: {
                Image(systemName: "arrow.clockwise")
            }
            .help("Пересчитать")
            .disabled(loader.loading)
        }
    }

    private func footer(_ report: ClaudeStats.Report) -> String {
        let since = report.since.map { " с \(StatsFormat.longDay($0))" } ?? ""
        return "Считается по логам Claude Code на этом Маке\(since): более ранние Claude уже удалил, другие компьютеры и claude.ai сюда не попадают. "
            + "Суммы — по ценам API Anthropic. По подписке ты платишь фиксированную цену, а тратишь лимиты."
    }
}

private struct Tiles: View {
    let period: ClaudeStats.Period

    var body: some View {
        let saved = max(0, period.costWithoutCache - period.cost)
        // Плитки одной высоты, даже если подпись где-то в две строки.
        HStack(alignment: .top, spacing: 12) {
            Tile(title: "По ценам API", value: StatsFormat.money(period.cost), note: perDay)
            Tile(title: "Кэш сэкономил", value: StatsFormat.money(saved),
                 note: "без кэша вышло бы \(StatsFormat.money(period.costWithoutCache))")
            Tile(title: "Claude написал", value: StatsFormat.tokens(period.tokens.output),
                 note: "токенов, прочитал \(StatsFormat.tokens(period.tokens.read))")
            Tile(title: "Сессии", value: StatsFormat.number(Double(period.sessions)),
                 note: StatsFormat.count(period.responses, "ответ", "ответа", "ответов") + " Claude")
        }
        .fixedSize(horizontal: false, vertical: true)
    }

    private var perDay: String {
        if period.byHour { return "с полуночи" }
        let days = StatsFormat.calendarDays(from: period.from)
        return "≈ \(StatsFormat.money(period.cost / Double(days))) в день"
    }
}

// MARK: - График

private struct ChartCard: View {
    let period: ClaudeStats.Period
    let palette: ModelPalette
    @State private var hovered: Date?

    var body: some View {
        let buckets = Dictionary(grouping: period.chart, by: \.at)
        let names = period.models.map(\.name)
        Card(title: period.byHour ? "По часам" : "По дням", note: note) {
            VStack(alignment: .leading, spacing: 10) {
                Text(caption(buckets))
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
                    .lineLimit(1)
                Chart(period.chart, id: \.key) { point in
                    BarMark(
                        x: .value("Время", point.date, unit: period.byHour ? .hour : .day),
                        y: .value("Стоимость", point.cost))
                        .foregroundStyle(by: .value("Модель", point.model))
                        .opacity(hovered == nil || hovered == point.date ? 1 : 0.35)
                }
                .chartForegroundStyleScale(domain: names, range: names.map(palette.color))
                .chartXScale(domain: domain)
                .chartXAxis {
                    AxisMarks(values: .automatic(desiredCount: period.byHour ? 8 : 10)) { _ in
                        AxisGridLine()
                        AxisValueLabel(format: period.byHour ? Date.FormatStyle.dateTime.hour() : Date.FormatStyle.dateTime.day().month(.abbreviated))
                    }
                }
                .chartYAxis {
                    AxisMarks { value in
                        AxisGridLine()
                        AxisValueLabel {
                            if let cost = value.as(Double.self) { Text(StatsFormat.money(cost)) }
                        }
                    }
                }
                .chartOverlay { proxy in
                    GeometryReader { geometry in
                        Rectangle().fill(.clear).contentShape(Rectangle())
                            .onContinuousHover { phase in
                                switch phase {
                                case .active(let location):
                                    let x = location.x - plotFrame(proxy, geometry).minX
                                    let date: Date? = proxy.value(atX: x)
                                    hovered = date.map(bucket).flatMap { buckets[Int($0.timeIntervalSince1970)] == nil ? nil : $0 }
                                case .ended:
                                    hovered = nil
                                }
                            }
                    }
                }
                .environment(\.locale, StatsFormat.locale)
                .frame(height: 200)
            }
        }
    }

    private func plotFrame(_ proxy: ChartProxy, _ geometry: GeometryProxy) -> CGRect {
        if #available(macOS 14, *) {
            return proxy.plotFrame.map { geometry[$0] } ?? .zero
        }
        return geometry[proxy.plotAreaFrame]
    }

    private var note: String? {
        guard !period.byHour else { return nil }
        let days = StatsFormat.calendarDays(from: period.from)
        return "Claude работал \(StatsFormat.count(period.activeDays, "день", "дня", "дней")) из \(days)"
    }

    private var domain: ClosedRange<Date> {
        let calendar = Calendar.current
        let end = calendar.date(byAdding: .day, value: 1, to: calendar.startOfDay(for: Date())) ?? Date()
        return Date(timeIntervalSince1970: TimeInterval(period.from))...end
    }

    private func bucket(_ date: Date) -> Date {
        let calendar = Calendar.current
        if period.byHour { return calendar.dateInterval(of: .hour, for: date)?.start ?? date }
        return calendar.startOfDay(for: date)
    }

    /// Под курсором — столбик по моделям, иначе самый дорогой.
    private func caption(_ buckets: [Int: [ClaudeStats.Point]]) -> String {
        if let hovered, let points = buckets[Int(hovered.timeIntervalSince1970)] {
            let total = points.reduce(0) { $0 + $1.cost }
            let parts = points.sorted { $0.cost > $1.cost }.map { "\($0.model) \(StatsFormat.money($0.cost))" }
            return "\(label(hovered)): \(StatsFormat.money(total)) — " + parts.joined(separator: " · ")
        }
        let totals = buckets.mapValues { $0.reduce(0) { $0 + $1.cost } }
        guard let (at, cost) = totals.max(by: { $0.value < $1.value }) else { return " " }
        let when = label(Date(timeIntervalSince1970: TimeInterval(at)))
        return (period.byHour ? "Дороже всего — в \(when): " : "Дороже всего — \(when): ") + StatsFormat.money(cost)
    }

    private func label(_ date: Date) -> String {
        let at = Int(date.timeIntervalSince1970)
        if period.byHour {
            let hour = Calendar.current.component(.hour, from: date)
            return String(format: "%d:00", hour)
        }
        return StatsFormat.day(at)
    }
}

private extension ClaudeStats.Point {
    var key: String { "\(at)|\(model)" }
}

// MARK: - Разбивки

private struct SpendCard: View {
    let period: ClaudeStats.Period

    var body: some View {
        let tokens = period.tokens
        let costs = period.costs
        let all: [(String, String, Double)] = [
            ("Чтение из кэша", Self.tokens(tokens.cacheRead), costs.cacheRead),
            ("Запись в кэш", Self.tokens(tokens.cacheWrite), costs.cacheWrite),
            ("Вывод и размышления", Self.tokens(tokens.output), costs.output),
            ("Ввод без кэша", Self.tokens(tokens.input), costs.input),
            ("Поиск в интернете", StatsFormat.count(tokens.searches, "запрос", "запроса", "запросов"), costs.searches),
        ]
        let rows = all.filter { $0.2 > 0 }.sorted { $0.2 > $1.2 }
        Card(title: "На что ушли деньги") {
            VStack(alignment: .leading, spacing: 10) {
                ForEach(rows, id: \.0) { row in
                    ShareRow(title: row.0, detail: row.1, share: share(row.2), value: StatsFormat.money(row.2))
                }
                if period.subagentCost > 0 {
                    Divider()
                    ShareRow(title: "Из них субагенты", detail: "Claude, которых запускал сам Claude",
                             share: share(period.subagentCost), value: StatsFormat.money(period.subagentCost))
                }
                Hint("Каждый ответ Claude заново читает весь разговор. Из кэша это в 10–40 раз дешевле обычного ввода, но в длинных сессиях набегает больше всего — /clear между задачами помогает.")
            }
        }
    }

    private func share(_ cost: Double) -> Double {
        period.cost > 0 ? cost / period.cost : 0
    }

    private static func tokens(_ count: Int) -> String {
        StatsFormat.tokens(count) + " токенов"
    }
}

private struct ModelsCard: View {
    let period: ClaudeStats.Period
    let palette: ModelPalette

    var body: some View {
        Card(title: "Модели") {
            VStack(alignment: .leading, spacing: 10) {
                ForEach(period.models) { model in
                    let detail = StatsFormat.count(model.responses, "ответ", "ответа", "ответов")
                        + " · написала \(StatsFormat.tokens(model.tokens.output)), прочитала \(StatsFormat.tokens(model.tokens.read))"
                    ShareRow(
                        title: model.name,
                        detail: detail,
                        swatch: palette.color(model.name),
                        share: period.cost > 0 ? model.cost / period.cost : 0,
                        value: model.priced ? StatsFormat.money(model.cost) : "—",
                        color: palette.color(model.name),
                        help: model.priced ? model.id : "\(model.id): цены этой модели vv не знает, поэтому только токены")
                }
            }
        }
    }
}

private struct ProjectsCard: View {
    let period: ClaudeStats.Period
    @State private var showAll = false
    private static let shown = 10

    var body: some View {
        let projects = showAll ? period.projects : Array(period.projects.prefix(Self.shown))
        Card(title: "Проекты") {
            VStack(alignment: .leading, spacing: 10) {
                ForEach(projects) { project in
                    let sessions = StatsFormat.count(project.sessions, "сессия", "сессии", "сессий")
                    ShareRow(
                        title: project.name,
                        detail: "\(sessions) · последняя \(StatsFormat.day(project.last))",
                        share: period.cost > 0 ? project.cost / period.cost : 0,
                        value: StatsFormat.money(project.cost),
                        help: project.path.isEmpty ? nil : project.path)
                }
                if period.projects.count > Self.shown {
                    Button(showAll ? "Свернуть" : "Все проекты (\(period.projects.count))") { showAll.toggle() }
                        .buttonStyle(.link)
                }
            }
        }
    }
}

private struct SessionsCard: View {
    let period: ClaudeStats.Period

    var body: some View {
        Card(title: "Самые дорогие сессии") {
            VStack(alignment: .leading, spacing: 10) {
                ForEach(period.topSessions) { session in
                    let responses = StatsFormat.count(session.responses, "ответ", "ответа", "ответов")
                    ShareRow(
                        title: session.title,
                        detail: "\(session.project) · \(StatsFormat.day(session.first)) · \(responses)",
                        share: period.cost > 0 ? session.cost / period.cost : 0,
                        value: StatsFormat.money(session.cost))
                }
            }
        }
    }
}

// MARK: - Детали

/// Карточка раздела: заголовок и содержимое на светлой подложке.
private struct Card<Content: View>: View {
    let title: String
    var note: String?
    @ViewBuilder let content: Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(alignment: .firstTextBaseline) {
                Text(title).font(.headline)
                Spacer()
                if let note {
                    Text(note).font(.callout).foregroundStyle(.secondary)
                }
            }
            content
        }
        .padding(16)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(StatsSurface())
    }
}

private struct Tile: View {
    let title: String
    let value: String
    let note: String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(title).font(.callout).foregroundStyle(.secondary)
            Text(value)
                .font(.system(size: 24, weight: .semibold))
                .lineLimit(1)
                .minimumScaleFactor(0.6)
            Text(note)
                .font(.caption)
                .foregroundStyle(.secondary)
                .lineLimit(2)
                .fixedSize(horizontal: false, vertical: true)
        }
        .padding(14)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .background(StatsSurface())
    }
}

private struct StatsSurface: View {
    var body: some View {
        RoundedRectangle(cornerRadius: 10)
            .fill(Color(nsColor: .controlBackgroundColor))
            .overlay(RoundedRectangle(cornerRadius: 10).strokeBorder(Color.primary.opacity(0.08)))
    }
}

/// Строка разбивки: название, пояснение, доля от всего периода полоской и сумма.
private struct ShareRow: View {
    let title: String
    let detail: String
    var swatch: Color?
    let share: Double
    let value: String
    /// Полоски не про модели — нейтральные, чтобы не спутать с цветом модели.
    var color = Color.primary.opacity(0.45)
    var help: String?

    var body: some View {
        HStack(spacing: 12) {
            if let swatch {
                Circle().fill(swatch).frame(width: 8, height: 8)
            }
            VStack(alignment: .leading, spacing: 2) {
                Text(title).lineLimit(1).truncationMode(.tail)
                Text(detail).font(.caption).foregroundStyle(.secondary).lineLimit(1).truncationMode(.tail)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            ShareBar(share: share, color: color).frame(width: 140)
            Text(StatsFormat.percent(share))
                .font(.caption)
                .foregroundStyle(.secondary)
                .monospacedDigit()
                .frame(width: 36, alignment: .trailing)
            Text(value).monospacedDigit().frame(width: 84, alignment: .trailing)
        }
        .contentShape(Rectangle())
        .help(help ?? "")
    }
}

private struct ShareBar: View {
    let share: Double
    let color: Color

    var body: some View {
        GeometryReader { geometry in
            ZStack(alignment: .leading) {
                Capsule().fill(Color.primary.opacity(0.08))
                Capsule().fill(color).frame(width: share > 0 ? max(3, geometry.size.width * min(1, share)) : 0)
            }
        }
        .frame(height: 6)
    }
}

/// Цвета моделей: восемь проверенных на различимость (и при нарушениях
/// цветового зрения), свои для светлой и тёмной темы. Модели дальше
/// восьмой — серые.
private struct ModelPalette {
    private static let light: [UInt32] = [0x2A78D6, 0xEB6834, 0x1BAF7A, 0xEDA100, 0xE87BA4, 0x008300, 0x4A3AA7, 0xE34948]
    private static let dark: [UInt32] = [0x3987E5, 0xD95926, 0x199E70, 0xC98500, 0xD55181, 0x008300, 0x9085E9, 0xE66767]
    private let order: [String]

    init(_ models: [ClaudeStats.Model]) {
        order = models.map(\.name)
    }

    func color(_ name: String) -> Color {
        guard let index = order.firstIndex(of: name), index < Self.light.count else { return .gray }
        let color = NSColor(name: nil) { appearance in
            let dark = appearance.bestMatch(from: [.aqua, .darkAqua]) == .darkAqua
            return Self.rgb((dark ? Self.dark : Self.light)[index])
        }
        return Color(nsColor: color)
    }

    private static func rgb(_ hex: UInt32) -> NSColor {
        NSColor(
            srgbRed: CGFloat((hex >> 16) & 0xFF) / 255,
            green: CGFloat((hex >> 8) & 0xFF) / 255,
            blue: CGFloat(hex & 0xFF) / 255,
            alpha: 1)
    }
}

/// Числа и даты по-русски: «$10 495», «41,8 млн», «28 сен».
enum StatsFormat {
    static let locale = Locale(identifier: "ru_RU")
    private static let months = [
        "января", "февраля", "марта", "апреля", "мая", "июня",
        "июля", "августа", "сентября", "октября", "ноября", "декабря",
    ]

    static func number(_ value: Double, digits: Int = 0, minimum: Int? = nil) -> String {
        let formatter = NumberFormatter()
        formatter.locale = locale
        formatter.numberStyle = .decimal
        formatter.minimumFractionDigits = minimum ?? digits
        formatter.maximumFractionDigits = digits
        return formatter.string(from: NSNumber(value: value)) ?? "\(value)"
    }

    /// От сотни долларов и круглые суммы — без центов.
    static func money(_ value: Double) -> String {
        if value > 0, value < 0.01 { return "<$0,01" }
        return "$" + number(value, digits: value >= 100 || value == value.rounded() ? 0 : 2)
    }

    static func tokens(_ value: Int) -> String {
        let value = Double(value)
        let (scaled, unit): (Double, String) =
            value >= 1e9 ? (value / 1e9, " млрд")
            : value >= 1e6 ? (value / 1e6, " млн")
            : value >= 1e3 ? (value / 1e3, " тыс.")
            : (value, "")
        return number(scaled, digits: unit.isEmpty || scaled >= 100 ? 0 : 1, minimum: 0) + unit
    }

    static func percent(_ share: Double) -> String {
        if share > 0, share < 0.005 { return "<1%" }
        return "\(Int((share * 100).rounded()))%"
    }

    /// «1 ответ», «3 ответа», «25 ответов».
    static func count(_ value: Int, _ one: String, _ few: String, _ many: String) -> String {
        let (tens, units) = (value % 100, value % 10)
        let word = (11...14).contains(tens) ? many : units == 1 ? one : (2...4).contains(units) ? few : many
        return "\(number(Double(value))) \(word)"
    }

    /// «сегодня», «вчера», «28 сент.», «28 сент. 2025 г.» — как на оси графика.
    static func day(_ at: Int) -> String {
        let calendar = Calendar.current
        let date = Date(timeIntervalSince1970: TimeInterval(at))
        if calendar.isDateInToday(date) { return "сегодня" }
        if calendar.isDateInYesterday(date) { return "вчера" }
        var style = Date.FormatStyle.dateTime.day().month(.abbreviated).locale(locale)
        if !calendar.isDate(date, equalTo: Date(), toGranularity: .year) {
            style = style.year()
        }
        return date.formatted(style)
    }

    /// «25 июля».
    static func longDay(_ at: Int) -> String {
        let parts = Calendar.current.dateComponents([.day, .month], from: Date(timeIntervalSince1970: TimeInterval(at)))
        return "\(parts.day ?? 0) \(months[((parts.month ?? 1) - 1) % 12])"
    }

    /// Сколько календарных дней от `from` до сегодня включительно.
    static func calendarDays(from: Int) -> Int {
        let calendar = Calendar.current
        let start = calendar.startOfDay(for: Date(timeIntervalSince1970: TimeInterval(from)))
        let days = calendar.dateComponents([.day], from: start, to: calendar.startOfDay(for: Date())).day ?? 0
        return max(1, days + 1)
    }
}
#endif
