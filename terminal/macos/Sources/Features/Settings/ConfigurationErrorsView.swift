import SwiftUI

protocol ConfigurationErrorsViewModel: ObservableObject {
    var errors: [String] { get set }
}

struct ConfigurationErrorsView<ViewModel: ConfigurationErrorsViewModel>: View {
    @ObservedObject var model: ViewModel

    var body: some View {
        VStack {
            HStack {
                Image(systemName: "exclamationmark.triangle.fill")
                    .foregroundColor(.yellow)
                    .font(.system(size: 52))
                    .padding()
                    .frame(alignment: .center)

                Text("""
                    В файле настроек нашлись ошибки: \(model.errors.count). \
                    Исправь их и перечитай настройки или пропусти эти строки.
                    """)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding()
            }

            GeometryReader { geo in
                ScrollView {
                    VStack(alignment: .leading) {
                        ForEach(model.errors, id: \.self) { error in
                            Text(error)
                                .lineLimit(nil)
                                .font(.system(size: 12).monospaced())
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .topLeading)
                        }

                        Spacer()
                    }
                    .padding(.all)
                    .frame(minHeight: geo.size.height)
                    .background(Color(.controlBackgroundColor))
                }
            }

            HStack {
                Spacer()
                Button("Пропустить") { model.errors = [] }
                Button("Перечитать настройки") { reloadConfig() }
            }
            .padding([.bottom, .trailing])
        }
        .frame(minWidth: 480, maxWidth: 960, minHeight: 270)
    }

    private func reloadConfig() {
        guard let delegate = NSApplication.shared.delegate as? AppDelegate else { return }
        delegate.reloadConfig(nil)
    }
}
