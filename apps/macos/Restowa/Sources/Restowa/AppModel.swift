import AppKit
import Observation
import RestowaKit

/// One-line feedback shown under the header after an action.
struct StatusLine: Equatable {
    let text: String
    let isError: Bool
}

/// Holds the last snapshot from the CLI. The snapshot is shown immediately
/// when the menu opens and refreshed in the background, because
/// `restowa list --live` takes longer than the 150 ms open budget.
@MainActor
@Observable
final class AppModel {
    private(set) var live: [Session] = []
    private(set) var lastBoot: [Session] = []
    private(set) var pinned: [Session] = []
    private(set) var isRefreshing = false
    private(set) var isRestoring = false
    private(set) var hasLoaded = false
    var status: StatusLine?

    let settings: SettingsStore
    private let watcher: RegistryWatcher
    private var environment: [String: String]?
    private var refreshQueued = false

    init(settings: SettingsStore) {
        self.settings = settings
        watcher = RegistryWatcher(dbPath: SettingsStore.registryPath)
    }

    var pendingRestore: [Session] {
        SessionGrouping.pendingRestore(lastBoot: lastBoot, live: live)
    }

    // MARK: Menu lifecycle

    func menuOpened() {
        watcher.start { [weak self] in self?.refresh() }
        refresh()
    }

    func menuClosed() {
        watcher.stop()
    }

    // MARK: Reads

    func refresh() {
        guard !isRefreshing else {
            refreshQueued = true  // Coalesce: at most one follow-up refresh.
            return
        }
        isRefreshing = true
        Task {
            defer { finishRefresh() }
            do {
                let cli = try await client()
                async let l = cli.list(.live)
                async let b = cli.list(.lastBoot)
                async let p = cli.list(.pinned)
                (live, lastBoot, pinned) = try await (l, b, p)
                hasLoaded = true
            } catch {
                status = StatusLine(text: error.localizedDescription, isError: true)
            }
        }
    }

    private func finishRefresh() {
        isRefreshing = false
        if refreshQueued && watcher.isRunning {
            refreshQueued = false
            refresh()
        }
        refreshQueued = false
    }

    func client() async throws -> CLIClient {
        guard let path = settings.resolvedBinaryPath else {
            throw CLIError(settings.binaryNotFoundMessage)
        }
        if environment == nil {
            environment = await ShellEnvironment.resolve()
        }
        return CLIClient(binaryPath: path, environment: environment ?? [:])
    }

    // MARK: Writes

    func restoreLastBoot() {
        restore { try await $0.restoreLastBoot() }
    }

    func restore(_ session: Session) {
        restore { try await $0.restore(keys: [session.key]) }
    }

    private func restore(_ op: @escaping @MainActor (CLIClient) async throws -> RestoreReport) {
        guard !isRestoring else { return }
        isRestoring = true
        status = StatusLine(text: "Restoring…", isError: false)
        Task {
            defer { isRestoring = false }
            do {
                let report = try await op(try await client())
                let detail = report.failed.first.map { " (\($0.reason))" } ?? ""
                status = StatusLine(text: report.summary + detail, isError: !report.failed.isEmpty)
            } catch {
                status = StatusLine(text: error.localizedDescription, isError: true)
            }
            refresh()
        }
    }

    func setPinned(_ session: Session, _ pinned: Bool) {
        mutate(pinned ? "Pinned" : "Unpinned") { cli in
            pinned ? try await cli.pin(session.key) : try await cli.unpin(session.key)
        }
    }

    func rename(_ session: Session, to title: String) {
        let trimmed = title.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, trimmed != session.title else { return }
        mutate("Renamed") { try await $0.rename(session.key, title: trimmed) }
    }

    func forget(_ session: Session) {
        mutate("Forgot \(session.displayTitle)") { try await $0.forget(session.key) }
    }

    private func mutate(_ done: String, _ op: @escaping @MainActor (CLIClient) async throws -> Void) {
        Task {
            do {
                try await op(try await client())
                status = StatusLine(text: done, isError: false)
            } catch {
                status = StatusLine(text: error.localizedDescription, isError: true)
            }
            refresh()
        }
    }

    // MARK: Local actions

    func copyResumeCommand(_ session: Session) {
        Task {
            do {
                let command = try await client().shellCommand(key: session.key)
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(command, forType: .string)
                status = StatusLine(text: "Copied resume command", isError: false)
            } catch {
                status = StatusLine(text: error.localizedDescription, isError: true)
            }
        }
    }

    func revealInFinder(_ session: Session) {
        let url = URL(fileURLWithPath: session.cwd, isDirectory: true)
        guard FileManager.default.fileExists(atPath: url.path) else {
            status = StatusLine(text: "Folder no longer exists: \(session.cwd)", isError: true)
            return
        }
        NSWorkspace.shared.activateFileViewerSelecting([url])
    }
}
