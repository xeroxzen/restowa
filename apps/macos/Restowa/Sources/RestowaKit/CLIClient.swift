import Foundation

public struct CLIError: LocalizedError, Sendable {
    public let message: String

    public init(_ message: String) { self.message = message }

    public var errorDescription: String? { message }
}

public enum ListScope: String, Sendable {
    case live = "--live"
    case lastBoot = "--last-boot"
    case pinned = "--pinned"
}

/// The app's only route to the registry: every read and write is a `restowa`
/// invocation with `--json`. Exit codes follow the spec: 0 ok, 1 error,
/// 2 nothing to do.
public struct CLIClient: Sendable {
    public let binaryPath: String
    public let environment: [String: String]

    public init(binaryPath: String, environment: [String: String]) {
        self.binaryPath = binaryPath
        self.environment = environment
    }

    public func list(_ scope: ListScope) async throws -> [Session] {
        try await decode([Session].self, ["list", scope.rawValue], timeout: 15)
    }

    public func restore(keys: [String]) async throws -> RestoreReport {
        precondition(!keys.isEmpty, "restore(keys:) needs at least one key")
        return try await decode(RestoreReport.self, ["restore", "--"] + keys, timeout: 120)
    }

    public func restoreLastBoot() async throws -> RestoreReport {
        try await decode(RestoreReport.self, ["restore", "--last-boot"], timeout: 120)
    }

    /// The `cd ... && <resume>` line restore would run for one session.
    public func shellCommand(key: String) async throws -> String {
        let report = try await decode(
            RestoreReport.self, ["restore", "--dry-run", "--", key], timeout: 15)
        guard let item = report.restored.first(where: { $0.key == key }) else {
            let reason = report.failed.first?.reason ?? "no resume command for \(key)"
            throw CLIError(reason)
        }
        return item.shellCommand
    }

    public func pin(_ key: String) async throws { try await mutate(["pin", "--", key]) }
    public func unpin(_ key: String) async throws { try await mutate(["unpin", "--", key]) }
    public func forget(_ key: String) async throws { try await mutate(["forget", "--", key]) }

    /// Long timeout: the first call waits on the macOS Automation prompt.
    public func focus(_ key: String) async throws {
        let args = ["focus", "--", key]
        try Self.check(try await invoke(args, timeout: 120), args: args)
    }

    public func rename(_ key: String, title: String) async throws {
        try await mutate(["rename", "--", key, title])
    }

    public func doctor() async throws -> DoctorReport {
        // doctor exits 1 when a check fails but still prints the full report.
        let result = try await invoke(["doctor"], timeout: 30)
        return try CLIJSON.decoder().decode(DoctorReport.self, from: result.stdout)
    }

    public func config() async throws -> AppConfig {
        try await decode(AppConfig.self, ["config"], timeout: 15)
    }

    public func setConfig(terminal: String? = nil, scanIntervalSecs: Int? = nil) async throws
        -> AppConfig
    {
        var args = ["config", "set"]
        if let terminal { args += ["--terminal", terminal] }
        if let scanIntervalSecs { args += ["--scan-interval", String(scanIntervalSecs)] }
        precondition(args.count > 2, "setConfig needs at least one value")
        return try await decode(AppConfig.self, args, timeout: 30)
    }

    private func mutate(_ args: [String]) async throws {
        let result = try await invoke(args, timeout: 15)
        try Self.check(result, args: args)
    }

    private func decode<T: Decodable>(_ type: T.Type, _ args: [String], timeout: TimeInterval)
        async throws -> T
    {
        let result = try await invoke(args, timeout: timeout)
        try Self.check(result, args: args)
        do {
            return try CLIJSON.decoder().decode(type, from: result.stdout)
        } catch {
            throw CLIError("unexpected output from restowa \(args[0]): \(error)")
        }
    }

    private func invoke(_ args: [String], timeout: TimeInterval) async throws -> ProcessResult {
        try await ProcessRunner.run(
            executable: binaryPath, arguments: ["--json"] + args,
            environment: environment, timeout: timeout)
    }

    static func check(_ result: ProcessResult, args: [String]) throws {
        if result.timedOut {
            throw CLIError("restowa \(args[0]) timed out")
        }
        guard result.exitCode == 0 || result.exitCode == 2 else {
            let stderr = String(decoding: result.stderr, as: UTF8.self)
                .trimmingCharacters(in: .whitespacesAndNewlines)
            let message = stderr.hasPrefix("error: ") ? String(stderr.dropFirst(7)) : stderr
            throw CLIError(message.isEmpty ? "restowa \(args[0]) exited \(result.exitCode)" : message)
        }
    }
}
