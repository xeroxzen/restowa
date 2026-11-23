import Foundation
import Observation
import RestowaKit

/// App-only preferences kept in UserDefaults. Values that the CLI also uses
/// (terminal, scan interval) live in config.toml and go through `restowa config`.
@MainActor
@Observable
final class SettingsStore {
    static let registryPath = (NSHomeDirectory() as NSString)
        .appendingPathComponent("Library/Application Support/restowa/registry.db")

    private enum Key {
        static let binaryOverride = "binaryPathOverride"
        static let hiddenTools = "hiddenTools"
    }

    private let defaults: UserDefaults

    var binaryOverride: String {
        didSet { defaults.set(binaryOverride, forKey: Key.binaryOverride) }
    }

    var hiddenTools: Set<Tool> {
        didSet { defaults.set(hiddenTools.map(\.rawValue).sorted(), forKey: Key.hiddenTools) }
    }

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        binaryOverride = defaults.string(forKey: Key.binaryOverride) ?? ""
        let raw = defaults.stringArray(forKey: Key.hiddenTools) ?? []
        hiddenTools = Set(raw.compactMap(Tool.init(rawValue:)))
    }

    var resolvedBinaryPath: String? {
        BinaryLocator.locate(override: binaryOverride, home: NSHomeDirectory())
    }

    var binaryNotFoundMessage: String {
        if !binaryOverride.trimmingCharacters(in: .whitespaces).isEmpty {
            return "restowa not found at \(binaryOverride) (Settings override)"
        }
        let tried = BinaryLocator.defaultCandidates(home: NSHomeDirectory())
        return "restowa not found; tried \(tried.joined(separator: ", "))"
    }

    func isToolShown(_ tool: Tool) -> Bool { !hiddenTools.contains(tool) }

    func setToolShown(_ tool: Tool, _ shown: Bool) {
        if shown { hiddenTools.remove(tool) } else { hiddenTools.insert(tool) }
    }
}
