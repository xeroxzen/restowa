import Foundation

/// Apps launched from Finder or at login get launchd's minimal PATH, which
/// would make `restowa doctor` report every agent CLI as missing. Ask the
/// user's login shell for its PATH instead.
public enum ShellEnvironment {
    public static let fallbackDirs = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin",
                                      "/usr/sbin", "/sbin"]

    public static func resolve(
        base: [String: String] = ProcessInfo.processInfo.environment
    ) async -> [String: String] {
        var env = base
        let shell = base["SHELL"].flatMap { $0.isEmpty ? nil : $0 } ?? "/bin/zsh"
        let shellPath = await loginShellPath(shell: shell)
        env["PATH"] = mergePath(primary: shellPath, existing: base["PATH"], home: base["HOME"])
        return env
    }

    /// Login-shell PATH first, then whatever we inherited, then well-known
    /// dirs; duplicates and empty entries dropped, order otherwise kept.
    public static func mergePath(primary: String?, existing: String?, home: String?) -> String {
        var dirs: [String] = []
        for source in [primary, existing] {
            dirs += (source ?? "").split(separator: ":").map(String.init)
        }
        if let home {
            dirs += [".local/bin", ".cargo/bin"].map { (home as NSString).appendingPathComponent($0) }
        }
        dirs += fallbackDirs
        var seen = Set<String>()
        return dirs.filter { !$0.isEmpty && seen.insert($0).inserted }.joined(separator: ":")
    }

    /// Returns nil when the shell fails or takes longer than 3 s; the caller
    /// then falls back to the inherited PATH plus well-known dirs.
    private static func loginShellPath(shell: String) async -> String? {
        let marker = "__RESTOWA_PATH__"
        let result: ProcessResult
        do {
            result = try await ProcessRunner.run(
                executable: shell,
                arguments: ["-l", "-c", "printf '\(marker)%s' \"$PATH\""],
                timeout: 3)
        } catch {
            return nil
        }
        guard !result.timedOut, result.exitCode == 0 else { return nil }
        let text = String(decoding: result.stdout, as: UTF8.self)
        guard let range = text.range(of: marker, options: .backwards) else { return nil }
        let path = String(text[range.upperBound...]).trimmingCharacters(in: .whitespacesAndNewlines)
        return path.isEmpty ? nil : path
    }
}
