import Foundation
import ServiceManagement

/// Launch at login. `SMAppService.mainApp` is the normal route; locally built
/// ad-hoc signed bundles can be refused by it, so registration failures fall
/// back to a per-user LaunchAgent that runs `open -a` on this bundle.
@MainActor
enum LoginItem {
    static let fallbackLabel = "dev.restowa.menubar"

    static var fallbackPlistURL: URL {
        URL(fileURLWithPath: NSHomeDirectory())
            .appendingPathComponent("Library/LaunchAgents/\(fallbackLabel).plist")
    }

    static var isEnabled: Bool {
        SMAppService.mainApp.status == .enabled
            || FileManager.default.fileExists(atPath: fallbackPlistURL.path)
    }

    static var needsApproval: Bool {
        SMAppService.mainApp.status == .requiresApproval
    }

    /// Returns a note for the user when the result needs their attention.
    static func setEnabled(_ enabled: Bool) throws -> String? {
        enabled ? try enable() : try disable()
    }

    private static func enable() throws -> String? {
        do {
            try SMAppService.mainApp.register()
        } catch {
            try writeFallbackAgent()
            return "Using a LaunchAgent (\(error.localizedDescription))"
        }
        if needsApproval {
            SMAppService.openSystemSettingsLoginItems()
            return "Approve Restowa in System Settings > Login Items"
        }
        return nil
    }

    private static func disable() throws -> String? {
        if SMAppService.mainApp.status == .enabled || needsApproval {
            try SMAppService.mainApp.unregister()
        }
        if FileManager.default.fileExists(atPath: fallbackPlistURL.path) {
            try FileManager.default.removeItem(at: fallbackPlistURL)
        }
        return nil
    }

    private static func writeFallbackAgent() throws {
        let plist: [String: Any] = [
            "Label": fallbackLabel,
            "ProgramArguments": ["/usr/bin/open", "-a", Bundle.main.bundlePath],
            "RunAtLoad": true,
            "LimitLoadToSessionType": "Aqua",
        ]
        let data = try PropertyListSerialization.data(
            fromPropertyList: plist, format: .xml, options: 0)
        try FileManager.default.createDirectory(
            at: fallbackPlistURL.deletingLastPathComponent(), withIntermediateDirectories: true)
        try data.write(to: fallbackPlistURL, options: .atomic)
    }
}
