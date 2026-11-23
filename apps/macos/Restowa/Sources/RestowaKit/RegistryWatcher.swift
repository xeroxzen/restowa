import Foundation

/// What counts as "the registry changed". A read-only `restowa list` bumps
/// the mtime of an empty `-wal` file, so the WAL only counts while it holds
/// data; otherwise every refresh would trigger the next one.
public struct RegistrySignature: Equatable, Sendable {
    public let dbModified: Date?
    public let dbSize: Int64
    public let walModified: Date?
    public let walSize: Int64

    public static func read(dbPath: String) -> RegistrySignature {
        let db = attributes(dbPath)
        let wal = attributes(dbPath + "-wal")
        return RegistrySignature(
            dbModified: db.modified,
            dbSize: db.size,
            walModified: wal.size > 0 ? wal.modified : nil,
            walSize: wal.size
        )
    }

    private static func attributes(_ path: String) -> (modified: Date?, size: Int64) {
        guard let attrs = try? FileManager.default.attributesOfItem(atPath: path) else {
            return (nil, 0)  // Missing file: treated as empty, not an error.
        }
        let size = (attrs[.size] as? NSNumber)?.int64Value ?? 0
        return (attrs[.modificationDate] as? Date, size)
    }
}

/// Watches `registry.db` and its `-wal` with kqueue while started, and calls
/// `onChange` (debounced) when the signature changes. Does nothing while
/// stopped: no sources, no timers.
@MainActor
public final class RegistryWatcher {
    private let dbPath: String
    private let debounce: TimeInterval
    private var sources: [any DispatchSourceFileSystemObject] = []
    private var lastSignature: RegistrySignature?
    private var pending: DispatchWorkItem?
    private var onChange: (@MainActor () -> Void)?

    public init(dbPath: String, debounce: TimeInterval = 0.3) {
        self.dbPath = dbPath
        self.debounce = debounce
    }

    public var isRunning: Bool { onChange != nil }

    public func start(onChange: @escaping @MainActor () -> Void) {
        stop()
        self.onChange = onChange
        lastSignature = RegistrySignature.read(dbPath: dbPath)
        arm()
    }

    public func stop() {
        pending?.cancel()
        pending = nil
        sources.forEach { $0.cancel() }
        sources.removeAll()
        onChange = nil
    }

    private func arm() {
        assert(sources.isEmpty, "arm() called with sources still open")
        for path in [dbPath, dbPath + "-wal"] {
            let fd = open(path, O_EVTONLY)
            guard fd >= 0 else { continue }  // Not created yet; the other file still fires.
            let source = DispatchSource.makeFileSystemObjectSource(
                fileDescriptor: fd, eventMask: [.write, .extend, .delete, .rename], queue: .main)
            source.setEventHandler { [weak self] in
                let replaced = !source.data.isDisjoint(with: [.delete, .rename])
                MainActor.assumeIsolated { self?.fileEvent(replaced: replaced) }
            }
            source.setCancelHandler { close(fd) }
            source.resume()
            sources.append(source)
        }
    }

    private func fileEvent(replaced: Bool) {
        guard isRunning else { return }
        if replaced {
            // The inode we watch is gone; watch whatever now sits at the path.
            sources.forEach { $0.cancel() }
            sources.removeAll()
            arm()
        }
        pending?.cancel()
        let work = DispatchWorkItem { [weak self] in
            MainActor.assumeIsolated { self?.fireIfChanged() }
        }
        pending = work
        DispatchQueue.main.asyncAfter(deadline: .now() + debounce, execute: work)
    }

    private func fireIfChanged() {
        guard let onChange else { return }
        let now = RegistrySignature.read(dbPath: dbPath)
        guard now != lastSignature else { return }
        lastSignature = now
        onChange()
    }
}
