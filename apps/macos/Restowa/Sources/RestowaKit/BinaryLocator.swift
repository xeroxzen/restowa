import Foundation

public enum BinaryLocator {
    /// Search order from the spec: Settings override, Homebrew, cargo, /usr/local.
    public static func defaultCandidates(home: String) -> [String] {
        [
            "/opt/homebrew/bin/restowa",
            (home as NSString).appendingPathComponent(".cargo/bin/restowa"),
            "/usr/local/bin/restowa",
        ]
    }

    /// An explicit override is authoritative: if it is set but not executable
    /// the result is nil rather than silently using a different binary.
    public static func locate(
        override: String?,
        home: String,
        isExecutable: (String) -> Bool = { FileManager.default.isExecutableFile(atPath: $0) }
    ) -> String? {
        if let override, !override.trimmingCharacters(in: .whitespaces).isEmpty {
            let path = (override as NSString).expandingTildeInPath
            return isExecutable(path) ? path : nil
        }
        return defaultCandidates(home: home).first(where: isExecutable)
    }
}
