import Foundation
import Testing

@testable import RestowaKit

private func session(
    _ id: String, cwd: String = "/w/alpha", status: SessionStatus = .live,
    seen: Int64 = 100, title: String? = nil, tool: Tool = .claude
) -> Session {
    Session(
        key: "\(tool.rawValue):\(id)", tool: tool, sessionId: id, cwd: cwd, title: title,
        status: status, firstSeen: 1, lastSeenLive: seen, bestGuess: false, pinned: false,
        hidden: false)
}

@Suite struct DecodingTests {
    @Test func decodesCLIListOutput() throws {
        let json = """
            [{"key":"codex:019a","tool":"codex","session_id":"019a","cwd":"/w/x","title":null,
              "transcript":null,"pid":12,"pid_start_us":5,"status":"closed","source":"scan",
              "first_seen":10,"last_seen_live":null,"last_boot_id":"b","ended_by_user":false,
              "best_guess":true,"pinned":true,"hidden":false}]
            """
        let sessions = try CLIJSON.decoder().decode([Session].self, from: Data(json.utf8))
        #expect(sessions.count == 1)
        #expect(sessions[0].tool == .codex)
        #expect(sessions[0].bestGuess)
        #expect(sessions[0].lastActivity == 10)
        #expect(sessions[0].displayTitle == "019a")
    }

    @Test func decodesRestoreReport() throws {
        let json = """
            {"terminal":"ghostty","dry_run":true,"restored":[{"key":"claude:a","tool":"claude",
            "session_id":"a","cwd":"/w","title":null,"best_guess":false,
            "resume_command":"claude --resume 'a'","shell_command":"cd '/w' && claude --resume 'a'"}],
            "skipped":[],"failed":[{"key":"claude:b","cwd":"/x","reason":"cwd missing"}]}
            """
        let report = try CLIJSON.decoder().decode(RestoreReport.self, from: Data(json.utf8))
        #expect(report.restored[0].shellCommand == "cd '/w' && claude --resume 'a'")
        #expect(report.summary == "Restored 1, failed 1")
    }
}

@Suite struct GroupingTests {
    @Test func groupsByProjectNewestFirst() {
        let groups = SessionGrouping.byProject([
            session("a", cwd: "/w/alpha", seen: 100),
            session("b", cwd: "/w/beta", seen: 300),
            session("c", cwd: "/w/alpha", seen: 200),
        ])
        #expect(groups.map(\.name) == ["beta", "alpha"])
        #expect(groups[1].sessions.map(\.sessionId) == ["c", "a"])
    }

    @Test func pendingExcludesLiveSessions() {
        let pending = SessionGrouping.pendingRestore(
            lastBoot: [session("a", status: .closed), session("b", status: .closed)],
            live: [session("b")])
        #expect(pending.map(\.sessionId) == ["a"])
    }

    @Test func filterMatchesTitleProjectAndHidesTools() {
        let all = [
            session("a1", cwd: "/w/sample-api", title: "Audit schema"),
            session("b2", cwd: "/w/sample-lib", title: "Port handshake", tool: .codex),
        ]
        #expect(SessionGrouping.filter(all, query: "audit", hiddenTools: []).count == 1)
        #expect(SessionGrouping.filter(all, query: "SAMPLE-LIB", hiddenTools: []).count == 1)
        #expect(SessionGrouping.filter(all, query: "", hiddenTools: [.codex]).count == 1)
        #expect(SessionGrouping.filter(all, query: "sample-lib", hiddenTools: [.codex]).isEmpty)
    }

    @Test(arguments: [(Int64(-5), "now"), (59, "now"), (60, "1m"), (7200, "2h"), (172_800, "2d")])
    func relativeAge(seconds: Int64, expected: String) {
        #expect(SessionGrouping.relativeAge(seconds: seconds) == expected)
    }
}

@Suite struct LocatorAndEnvironmentTests {
    @Test func overrideIsAuthoritative() {
        let found = BinaryLocator.locate(override: "/nope/restowa", home: "/h") { _ in false }
        #expect(found == nil)
        let any = BinaryLocator.locate(override: "/x/restowa", home: "/h") { $0 == "/x/restowa" }
        #expect(any == "/x/restowa")
    }

    @Test func defaultOrderPrefersHomebrewThenCargo() {
        let path = BinaryLocator.locate(override: nil, home: "/h") {
            $0 == "/h/.cargo/bin/restowa" || $0 == "/usr/local/bin/restowa"
        }
        #expect(path == "/h/.cargo/bin/restowa")
    }

    @Test func mergePathKeepsOrderAndDedupes() {
        let merged = ShellEnvironment.mergePath(
            primary: "/a:/opt/homebrew/bin", existing: "/usr/bin::/a", home: "/h")
        let dirs = merged.split(separator: ":").map(String.init)
        #expect(Array(dirs.prefix(4)) == ["/a", "/opt/homebrew/bin", "/usr/bin", "/h/.local/bin"])
        #expect(Set(dirs).count == dirs.count)
    }
}

@Suite struct ProcessAndSignatureTests {
    @Test func cliErrorsUseStderrMessage() async throws {
        let result = try await ProcessRunner.run(
            executable: "/bin/sh", arguments: ["-c", "echo 'error: session not found: x' >&2; exit 1"],
            timeout: 5)
        #expect(throws: CLIError.self) { try CLIClient.check(result, args: ["pin"]) }
        do {
            try CLIClient.check(result, args: ["pin"])
        } catch let error as CLIError {
            #expect(error.message == "session not found: x")
        }
    }

    @Test func runnerTimesOut() async throws {
        let result = try await ProcessRunner.run(
            executable: "/bin/sleep", arguments: ["5"], timeout: 0.2)
        #expect(result.timedOut)
    }

    @Test func emptyWalMtimeDoesNotChangeSignature() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("restowa-sig-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let db = dir.appendingPathComponent("registry.db").path
        let wal = db + "-wal"
        #expect(FileManager.default.createFile(atPath: db, contents: Data("x".utf8)))
        #expect(FileManager.default.createFile(atPath: wal, contents: Data()))
        let before = RegistrySignature.read(dbPath: db)

        let later = Date().addingTimeInterval(10)
        try FileManager.default.setAttributes([.modificationDate: later], ofItemAtPath: wal)
        #expect(RegistrySignature.read(dbPath: db) == before)

        try Data("page".utf8).write(to: URL(fileURLWithPath: wal))
        #expect(RegistrySignature.read(dbPath: db) != before)
    }
}
