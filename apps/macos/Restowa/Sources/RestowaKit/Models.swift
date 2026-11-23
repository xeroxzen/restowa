import Foundation

/// Mirrors `restowa_core::model::Tool`.
public enum Tool: String, Codable, CaseIterable, Sendable, Identifiable {
    case claude
    case codex
    case opencode
    case antigravity

    public var id: String { rawValue }

    public var displayName: String {
        switch self {
        case .claude: "Claude Code"
        case .codex: "Codex"
        case .opencode: "OpenCode"
        case .antigravity: "Antigravity"
        }
    }

    public var symbolName: String {
        switch self {
        case .claude: "sparkle"
        case .codex: "chevron.left.forwardslash.chevron.right"
        case .opencode: "curlybraces"
        case .antigravity: "arrow.up.circle"
        }
    }
}

public enum SessionStatus: String, Codable, Sendable {
    case live
    case closed
    case missing
}

/// Subset of `restowa_core::model::Session` that the app displays.
public struct Session: Codable, Hashable, Identifiable, Sendable {
    public let key: String
    public let tool: Tool
    public let sessionId: String
    public let cwd: String
    public let title: String?
    public let status: SessionStatus
    public let firstSeen: Int64
    public let lastSeenLive: Int64?
    public let bestGuess: Bool
    public let pinned: Bool
    public let hidden: Bool

    public var id: String { key }

    public var shortId: String { String(sessionId.prefix(8)) }

    public var projectName: String {
        let name = (cwd as NSString).lastPathComponent
        return name.isEmpty ? cwd : name
    }

    public var displayTitle: String {
        guard let title, !title.isEmpty else { return shortId }
        return title
    }

    /// Unix seconds of the most recent sign of life.
    public var lastActivity: Int64 { lastSeenLive ?? firstSeen }
}

public struct RestoreItem: Codable, Hashable, Sendable {
    public let key: String
    public let cwd: String
    public let bestGuess: Bool
    public let resumeCommand: String
    public let shellCommand: String
}

public struct RestoreNote: Codable, Hashable, Sendable {
    public let key: String
    public let cwd: String
    public let reason: String
}

public struct RestoreReport: Codable, Hashable, Sendable {
    public let terminal: String
    public let dryRun: Bool
    public let restored: [RestoreItem]
    public let skipped: [RestoreNote]
    public let failed: [RestoreNote]

    public var summary: String {
        var parts = ["Restored \(restored.count)"]
        if !skipped.isEmpty { parts.append("skipped \(skipped.count)") }
        if !failed.isEmpty { parts.append("failed \(failed.count)") }
        return parts.joined(separator: ", ")
    }
}

public struct DoctorCheck: Codable, Hashable, Sendable, Identifiable {
    public let name: String
    public let status: String
    public let detail: String

    public var id: String { name }
}

public struct DoctorReport: Codable, Hashable, Sendable {
    public let checks: [DoctorCheck]
    public let hasErrors: Bool
    public let hasWarnings: Bool
}

/// Subset of `config.toml` the Settings window edits.
public struct AppConfig: Codable, Hashable, Sendable {
    public let terminal: String
    public let scanIntervalSecs: Int
}

public enum CLIJSON {
    public static func decoder() -> JSONDecoder {
        let d = JSONDecoder()
        d.keyDecodingStrategy = .convertFromSnakeCase
        return d
    }
}
