import RestowaKit
import SwiftUI

enum SectionKind {
    case live
    case lastBoot
    case pinned
}

struct SessionSection: View {
    let title: String
    let empty: String
    let sessions: [Session]
    let kind: SectionKind
    let model: AppModel
    let ui: MenuState

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text(title)
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(.secondary)
            if sessions.isEmpty {
                Text(model.hasLoaded ? empty : "Loading…")
                    .font(.callout)
                    .foregroundStyle(.tertiary)
            } else {
                ForEach(SessionGrouping.byProject(sessions)) { group in
                    Text(group.name)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .help(group.cwd)
                        .padding(.top, 2)
                    ForEach(group.sessions) { session in
                        SessionRow(session: session, kind: kind, model: model, ui: ui)
                    }
                }
            }
        }
    }
}

struct SessionRow: View {
    let session: Session
    let kind: SectionKind
    let model: AppModel
    @Bindable var ui: MenuState

    private var hovering: Bool { ui.hoveredKey == session.key }

    var body: some View {
        Group {
            switch ui.mode(for: session.key) {
            case .normal: normalRow
            case .renaming: renameRow
            case .confirmingForget: forgetRow
            }
        }
        .padding(.vertical, 4)
        .padding(.horizontal, 6)
        .background(RoundedRectangle(cornerRadius: 6).fill(.quaternary.opacity(hovering ? 1 : 0)))
        .onHover { inside in
            if inside {
                ui.hoveredKey = session.key
            } else if hovering {
                ui.hoveredKey = nil
            }
        }
    }

    private var normalRow: some View {
        HStack(spacing: 8) {
            Image(systemName: session.tool.symbolName)
                .frame(width: 16)
                .foregroundStyle(.secondary)
                .help(session.tool.displayName)
            VStack(alignment: .leading, spacing: 1) {
                Text(session.displayTitle).lineLimit(1).truncationMode(.tail)
                HStack(spacing: 4) {
                    Text(session.tool.displayName)
                    if session.bestGuess { Text("(best guess)").foregroundStyle(.orange) }
                    if session.pinned && kind != .pinned { Image(systemName: "pin.fill") }
                }
                .font(.caption)
                .foregroundStyle(.secondary)
            }
            Spacer(minLength: 4)
            Text(age).font(.caption).monospacedDigit().foregroundStyle(.secondary)
            if kind == .lastBoot {
                Button("Restore") { model.restore(session) }
                    .controlSize(.small)
                    .disabled(model.isRestoring)
            }
            Menu { contextMenu } label: { Image(systemName: "ellipsis.circle") }
                .menuStyle(.borderlessButton)
                .menuIndicator(.hidden)
                .fixedSize()
                .opacity(hovering ? 1 : 0)
                .help("Actions")
        }
        .contentShape(Rectangle())
        .help("\(session.cwd)\n\(session.key)")
        .contextMenu { contextMenu }
    }

    @ViewBuilder private var contextMenu: some View {
        Button("Restore") { model.restore(session) }
            .disabled(session.status == .live || model.isRestoring)
        Button("Copy Resume Command") { model.copyResumeCommand(session) }
        Button("Reveal Folder in Finder") { model.revealInFinder(session) }
        Divider()
        Button(session.pinned ? "Unpin" : "Pin") { model.setPinned(session, !session.pinned) }
        Button("Rename…") { ui.begin(.renaming, key: session.key, title: session.title ?? "") }
        Divider()
        Button("Forget…", role: .destructive) { ui.begin(.confirmingForget, key: session.key) }
    }

    private var renameRow: some View {
        HStack(spacing: 6) {
            TextField("Title", text: $ui.draftTitle)
                .textFieldStyle(.roundedBorder)
                .onSubmit(commitRename)
                .onExitCommand { ui.endEditing() }
            Button("Save", action: commitRename).controlSize(.small)
            Button("Cancel") { ui.endEditing() }.controlSize(.small)
        }
    }

    private var forgetRow: some View {
        HStack(spacing: 6) {
            Text("Forget \u{201C}\(session.displayTitle)\u{201D}?")
                .lineLimit(1)
            Spacer(minLength: 4)
            Button("Cancel") { ui.endEditing() }.controlSize(.small)
            Button("Forget", role: .destructive) {
                ui.endEditing()
                model.forget(session)
            }
            .controlSize(.small)
        }
        .help("Hides it from restowa. The tool's own session data is not touched.")
    }

    private func commitRename() {
        let title = ui.draftTitle
        ui.endEditing()
        model.rename(session, to: title)
    }

    private var age: String {
        let now = Int64(Date().timeIntervalSince1970)
        return SessionGrouping.relativeAge(seconds: now - session.lastActivity)
    }
}
