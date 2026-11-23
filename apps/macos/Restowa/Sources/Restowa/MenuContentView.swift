import RestowaKit
import SwiftUI

struct MenuContentView: View {
    let model: AppModel
    let settings: SettingsStore
    @Bindable var ui: MenuState
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        VStack(spacing: 0) {
            header
            if let status = model.status {
                StatusBanner(status: status) { model.status = nil }
            }
            Divider()
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    SessionSection(title: "Open now", empty: "No open sessions",
                                   sessions: visible(model.live), kind: .live, model: model, ui: ui)
                    SessionSection(title: "Last boot", empty: "Nothing left to restore",
                                   sessions: visible(model.pendingRestore), kind: .lastBoot,
                                   model: model, ui: ui)
                    SessionSection(title: "Pinned", empty: "No pinned sessions",
                                   sessions: visible(model.pinned), kind: .pinned, model: model, ui: ui)
                }
                .padding(.horizontal, 12)
                .padding(.vertical, 10)
            }
            .frame(maxHeight: 460)
            Divider()
            footer
        }
        .frame(width: 360)
        .background(WindowVisibility(onOpen: model.menuOpened, onClose: {
            ui.endEditing()
            model.menuClosed()
        }))
    }

    private func visible(_ sessions: [Session]) -> [Session] {
        SessionGrouping.filter(sessions, query: ui.query, hiddenTools: settings.hiddenTools)
    }

    @ViewBuilder private var header: some View {
        let pending = model.pendingRestore.count
        if pending > 0 {
            Button(action: model.restoreLastBoot) {
                HStack {
                    Image(systemName: "arrow.counterclockwise")
                    Text("Restore \(pending) session\(pending == 1 ? "" : "s") from last boot")
                    Spacer()
                    if model.isRestoring { ProgressView().controlSize(.small) }
                }
                .frame(maxWidth: .infinity)
            }
            .buttonStyle(.borderedProminent)
            .controlSize(.large)
            .disabled(model.isRestoring)
            .padding(12)
        } else {
            HStack {
                Text("Restowa").font(.headline)
                Spacer()
                if model.isRefreshing { ProgressView().controlSize(.small) }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 10)
        }
    }

    private var footer: some View {
        HStack(spacing: 8) {
            TextField("Search", text: $ui.query)
                .textFieldStyle(.roundedBorder)
            Button {
                openWindow(id: SettingsView.windowID)
                NSApp.activate()
            } label: {
                Image(systemName: "gearshape")
            }
            .help("Settings")
            Button {
                NSApp.terminate(nil)
            } label: {
                Image(systemName: "power")
            }
            .help("Quit Restowa")
        }
        .buttonStyle(.borderless)
        .padding(10)
    }
}

private struct StatusBanner: View {
    let status: StatusLine
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Image(systemName: status.isError ? "exclamationmark.triangle.fill" : "checkmark.circle")
                .foregroundStyle(status.isError ? .orange : .secondary)
            Text(status.text)
                .font(.callout)
                .textSelection(.enabled)
                .fixedSize(horizontal: false, vertical: true)
            Spacer(minLength: 0)
            Button(action: dismiss) { Image(systemName: "xmark") }
                .buttonStyle(.borderless)
                .help("Dismiss")
        }
        .padding(.horizontal, 12)
        .padding(.bottom, 8)
    }
}
