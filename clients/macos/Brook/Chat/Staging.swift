import BrookCore
import Foundation
import UniformTypeIdentifiers

/// What staging needs from the file system (a fake in tests): the sandbox's security scope
/// for a URL the user chose, and the file's facts.
protocol FileAccess: Sendable {
    func startAccessing(_ url: URL) -> Bool
    func stopAccessing(_ url: URL)
    /// Regular file (not a folder, package or device), size, content type; nil if unreadable.
    func facts(_ url: URL) -> (regular: Bool, size: UInt64, type: String?)?
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
}

/// A file chosen for the next message, holding its security-scoped access from staging until
/// it's removed or sent (spec §1): the enqueue reads it after staging did.
final class StagedFile: Identifiable, @unchecked Sendable {
    let url: URL
    let name: String
    let size: UInt64
    let contentType: String
    /// Its upload's transfer id, fixed while staged (the retry key uses it, as GTK #173).
    let transferId: UInt64
    private let access: any FileAccess
    private let lock = NSLock()
    private var held = true

    var id: UInt64 { transferId }

    fileprivate init(url: URL, size: UInt64, contentType: String, access: any FileAccess) {
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
    /// Stage `url` beside `already` (GTK's refusals and texts, spec §1): its access is held
    /// only if it's staged, and stopped on every refusal.
    static func stage(_ url: URL, already: [StagedFile], access: any FileAccess) -> Result<StagedFile, StagingRefusal> {
        let name = url.lastPathComponent
        if already.contains(where: { $0.url.standardizedFileURL == url.standardizedFileURL }) {
            return .failure(.duplicate)
        }
        guard already.count < Int(maxFilesPerMessage()) else {
            return .failure(.tooMany)
        }
        guard access.startAccessing(url) else { return .failure(.unreadable(name)) }
        var staged = false
        defer { if !staged { access.stopAccessing(url) } }
        guard let facts = access.facts(url) else { return .failure(.unreadable(name)) }
        guard facts.regular else { return .failure(.notAFile) }
        guard facts.size > 0 else { return .failure(.empty(name)) }
        guard facts.size <= maxFileBytes() else { return .failure(.tooLarge(name)) }
        staged = true
        return .success(StagedFile(url: url, size: facts.size,
                                   contentType: facts.type ?? "application/octet-stream", access: access))
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
