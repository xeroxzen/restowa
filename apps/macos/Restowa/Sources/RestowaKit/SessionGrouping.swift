import Foundation

public struct ProjectGroup: Identifiable, Hashable, Sendable {
    public let cwd: String
    public let name: String
    public let sessions: [Session]

    public var id: String { cwd }
}

public enum SessionGrouping {
    /// Groups by working directory. Groups and their sessions are ordered by
    /// most recent activity first; ties break on key so output is stable.
    public static func byProject(_ sessions: [Session]) -> [ProjectGroup] {
        let buckets = Dictionary(grouping: sessions, by: \.cwd)
        let groups = buckets.map { cwd, members in
            ProjectGroup(
                cwd: cwd,
                name: members[0].projectName,
                sessions: members.sorted(by: newerFirst)
            )
        }
        let sorted = groups.sorted { a, b in
            let ta = a.sessions[0].lastActivity
            let tb = b.sessions[0].lastActivity
            return ta != tb ? ta > tb : a.cwd < b.cwd
        }
        assert(sorted.reduce(0) { $0 + $1.sessions.count } == sessions.count)
        return sorted
    }

    /// Case-insensitive match on title, project, tool or session ID prefix,
    /// excluding tools the user has hidden in Settings.
    public static func filter(
        _ sessions: [Session], query: String, hiddenTools: Set<Tool>
    ) -> [Session] {
        let needle = query.trimmingCharacters(in: .whitespaces).lowercased()
        return sessions.filter { s in
            guard !hiddenTools.contains(s.tool) else { return false }
            guard !needle.isEmpty else { return true }
            return s.displayTitle.lowercased().contains(needle)
                || s.projectName.lowercased().contains(needle)
                || s.tool.rawValue.contains(needle)
                || s.sessionId.lowercased().hasPrefix(needle)
        }
    }

    /// Sessions from the last-boot set that are not open right now.
    public static func pendingRestore(lastBoot: [Session], live: [Session]) -> [Session] {
        let liveKeys = Set(live.map(\.key))
        return lastBoot.filter { !liveKeys.contains($0.key) && $0.status != .live }
    }

    /// Compact age like the CLI's SEEN column: "now", "5m", "3h", "2d".
    public static func relativeAge(seconds: Int64) -> String {
        let s = max(seconds, 0)
        if s < 60 { return "now" }
        if s < 3600 { return "\(s / 60)m" }
        if s < 86_400 { return "\(s / 3600)h" }
        return "\(s / 86_400)d"
    }

    private static func newerFirst(_ a: Session, _ b: Session) -> Bool {
        a.lastActivity != b.lastActivity ? a.lastActivity > b.lastActivity : a.key < b.key
    }
}
