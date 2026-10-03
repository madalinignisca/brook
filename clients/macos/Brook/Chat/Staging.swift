import BrookCore
import Foundation
import UniformTypeIdentifiers

/// What staging needs from the file system (a fake in tests): the sandbox's security scope
/// for a URL the user chose, and the file's facts.
protocol FileAccess: Sendable {
    /// True only for a security-scoped URL (the picker's); a dropped file's URL is plain and
    /// gives false though the sandbox lets the process read it, so false is not "unreadable".
    func startAccessing(_ url: URL) -> Bool
    func stopAccessing(_ url: URL)
    /// Regular file (not a folder, package or device), size, content type; nil if unreadable.
    func facts(_ url: URL) -> (regular: Bool, size: UInt64, type: String?)?
    /// Whether the file can really be opened for reading (the sandbox and permissions decide).
    func canRead(_ url: URL) -> Bool
}

/// The real file system.
struct SystemFileAccess: FileAccess {
    func startAccessing(_ url: URL) -> Bool { url.startAccessingSecurityScopedResource() }
    func stopAccessing(_ url: URL) { url.stopAccessingSecurityScopedResource() }
    func facts(_ url: URL) -> (regular: Bool, size: UInt64, type: String?)? {
        guard let v = try? url.resourceValues(forKeys: [.isRegularFileKey, .fileSizeKey, .contentTypeKey]),
              let regular = v.isRegularFile
        else { return nil }
        return (regular, UInt64(v.fileSize ?? 0), v.contentType?.preferredMIMEType)
    }
    func canRead(_ url: URL) -> Bool {
        // Runs on the main actor, and the path may have become a pipe since `facts` looked: a
        // blocking open would wait for a writer and freeze the UI. So open non-blocking and judge
        // the descriptor itself (as core's snapshot does), never the path.
        let fd = open(url.path, O_RDONLY | O_NONBLOCK | O_CLOEXEC)
        guard fd >= 0 else { return false }
        defer { close(fd) }
        var st = stat()
        return fstat(fd, &st) == 0 && (st.st_mode & S_IFMT) == S_IFREG
    }
}

/// A file chosen for the next message, holding its security-scoped access (if it has one) from
/// staging until it's removed or sent (spec §1): the enqueue reads it after staging did.
final class StagedFile: Identifiable, @unchecked Sendable {
    let url: URL
    let name: String
    let size: UInt64
    let contentType: String
    /// Its upload's transfer id, fixed while staged (the retry key uses it, as GTK #173).
    let transferId: UInt64
    private let access: any FileAccess
    private let lock = NSLock()
    private var held: Bool

    var id: UInt64 { transferId }

    /// `scoped`: `startAccessing` returned true, so a `stopAccessing` is owed.
    fileprivate init(url: URL, size: UInt64, contentType: String, access: any FileAccess, scoped: Bool) {
        held = scoped
        self.url = url
        name = url.lastPathComponent
        self.size = size
        self.contentType = contentType
        transferId = UInt64.random(in: 1 ... UInt64.max)
        self.access = access
    }

    /// Stop its access (removed, or sent). Once.
    func release() {
        let wasHeld = lock.withLock { () -> Bool in
            defer { held = false }
            return held
        }
        if wasHeld { access.stopAccessing(url) }
    }

    var outgoing: FfiOutgoingFile {
        FfiOutgoingFile(path: url.path, filename: name, contentType: contentType, transferId: transferId)
    }
}

enum Staging {
    /// Stage `url` beside `already` (GTK's refusals and texts, spec §1): its scope, if it has
    /// one, is held only if it's staged, and stopped on every refusal.
    static func stage(_ url: URL, already: [StagedFile], access: any FileAccess) -> Result<StagedFile, StagingRefusal> {
        let name = url.lastPathComponent
        if already.contains(where: { $0.url.standardizedFileURL == url.standardizedFileURL }) {
            return .failure(.duplicate)
        }
        guard already.count < Int(maxFilesPerMessage()) else {
            return .failure(.tooMany)
        }
        // A dropped file has no scope (false) yet is readable: facts and a real open decide, and
        // only a scope that was started is ever stopped.
        let scoped = access.startAccessing(url)
        var staged = false
        defer { if scoped, !staged { access.stopAccessing(url) } }
        guard let facts = access.facts(url) else { return .failure(.unreadable(name)) }
        guard facts.regular else { return .failure(.notAFile) }
        // After the regular check: opening a pipe for reading can block.
        guard access.canRead(url) else { return .failure(.unreadable(name)) }
        guard facts.size > 0 else { return .failure(.empty(name)) }
        guard facts.size <= maxFileBytes() else { return .failure(.tooLarge(name)) }
        staged = true
        return .success(StagedFile(url: url, size: facts.size,
                                   contentType: facts.type ?? "application/octet-stream", access: access,
                                   scoped: scoped))
    }
}

enum StagingRefusal: Error, Equatable {
    case duplicate
    case tooMany
    case unreadable(String)
    case notAFile
    case empty(String)
    case tooLarge(String)

    var text: String? {
        switch self {
        case .duplicate: nil // already staged: nothing to say
        case .tooMany: "A message can carry up to \(maxFilesPerMessage()) files."
        case let .unreadable(name): "\(name) couldn't be read."
        case .notAFile: "Only files can be sent (not folders or devices)."
        case let .empty(name): "\(name) is empty."
        // The limit is binary (100 MiB): decimal units would say "104.9 MB".
        case let .tooLarge(name): "\(name) is larger than \(maxFileBytes() / (1024 * 1024)) MiB."
        }
    }
}
