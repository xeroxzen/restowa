import Foundation

public struct ProcessResult: Sendable {
    public let exitCode: Int32
    public let stdout: Data
    public let stderr: Data
    public let timedOut: Bool
}

/// Collects one pipe's bytes on a background thread so neither pipe can fill
/// up and block the child while the other is being drained.
private final class PipeDrain: @unchecked Sendable {
    private let lock = NSLock()
    private var data = Data()
    private let done = DispatchSemaphore(value: 0)

    init(_ handle: FileHandle) {
        DispatchQueue.global(qos: .userInitiated).async { [self] in
            let bytes = handle.readDataToEndOfFile()
            lock.lock()
            data = bytes
            lock.unlock()
            done.signal()
        }
    }

    func wait() -> Data {
        done.wait()
        lock.lock()
        defer { lock.unlock() }
        return data
    }
}

public enum ProcessRunner {
    /// Runs `executable` to completion off the main thread. A child still
    /// running at `timeout` is terminated and reported with `timedOut = true`.
    public static func run(
        executable: String,
        arguments: [String],
        environment: [String: String]? = nil,
        timeout: TimeInterval
    ) async throws -> ProcessResult {
        precondition(timeout > 0, "timeout must be positive")
        return try await withCheckedThrowingContinuation { cont in
            DispatchQueue.global(qos: .userInitiated).async {
                do {
                    let result = try runBlocking(executable, arguments, environment, timeout)
                    cont.resume(returning: result)
                } catch {
                    cont.resume(throwing: error)
                }
            }
        }
    }

    private static func runBlocking(
        _ executable: String, _ arguments: [String],
        _ environment: [String: String]?, _ timeout: TimeInterval
    ) throws -> ProcessResult {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: executable)
        process.arguments = arguments
        if let environment { process.environment = environment }
        let out = Pipe()
        let err = Pipe()
        process.standardOutput = out
        process.standardError = err
        process.standardInput = FileHandle.nullDevice

        try process.run()
        let outDrain = PipeDrain(out.fileHandleForReading)
        let errDrain = PipeDrain(err.fileHandleForReading)

        let exited = DispatchSemaphore(value: 0)
        DispatchQueue.global(qos: .userInitiated).async {
            process.waitUntilExit()
            exited.signal()
        }
        let timedOut = exited.wait(timeout: .now() + timeout) == .timedOut
        if timedOut {
            process.terminate()
            exited.wait()
        }
        return ProcessResult(
            exitCode: process.terminationStatus,
            stdout: outDrain.wait(),
            stderr: errDrain.wait(),
            timedOut: timedOut
        )
    }
}
