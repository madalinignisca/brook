import AppKit
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
    /// What the user chose, for telling duplicates apart: a dropped file's copy has a new path
    /// each time, its source does not.
    let source: URL
    private let access: any FileAccess
    /// A dropped file's private copy: the folder holding it, removed with the file.
    private let ownedDir: URL?
    private let lock = NSLock()
    private var held: Bool

    var id: UInt64 { transferId }

    /// `scoped`: `startAccessing` returned true, so a `stopAccessing` is owed.
    fileprivate init(url: URL, size: UInt64, contentType: String, access: any FileAccess, scoped: Bool,
                     ownedDir: URL?, source: URL) {
        held = scoped
        self.ownedDir = ownedDir
        self.source = source.standardizedFileURL
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
        if let ownedDir { try? FileManager.default.removeItem(at: ownedDir) }
    }

    var outgoing: FfiOutgoingFile {
        FfiOutgoingFile(path: url.path, filename: name, contentType: contentType, transferId: transferId)
    }
}

enum Staging {
    /// Stage `url` beside `already` (GTK's refusals and texts, spec §1): its scope, if it has
    /// one, is held only if it's staged, and stopped on every refusal.
    static func stage(_ url: URL, already: [StagedFile], access: any FileAccess,
                      ownedDir: URL? = nil, source: URL? = nil) -> Result<StagedFile, StagingRefusal> {
        var staged = false
        // A dropped file's copy goes with a refusal; a staged one goes when its file is released.
        defer { if !staged, let ownedDir { try? FileManager.default.removeItem(at: ownedDir) } }
        let name = url.lastPathComponent
        if already.contains(where: { $0.source == (source ?? url).standardizedFileURL }) {
            return .failure(.duplicate)
        }
        guard already.count < Int(maxFilesPerMessage()) else {
            return .failure(.tooMany)
        }
        // A dropped file has no scope (false) yet is readable: facts and a real open decide, and
        // only a scope that was started is ever stopped.
        let scoped = access.startAccessing(url)
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
                                   scoped: scoped, ownedDir: ownedDir, source: source ?? url))
    }
}

/// Files dropped on the conversation. A drop hands each file over through its item provider,
/// readable only while the system's load callback runs (the sandbox's access to a file the
/// user dragged in, an iCloud file's download): the plain URL SwiftUI would give is not
/// reliably readable ("couldn't be read"). So each file is copied at once into this app's own
/// temp folder, and the copy is what gets staged.
enum DropImport {
    enum Outcome {
        /// The copy, in its own folder (removed with the staged file), and the file it came from.
        case copied(URL, dir: URL, source: URL)
        case refused(StagingRefusal)
    }

    static let root = FileManager.default.temporaryDirectory.appendingPathComponent("brook-drops", isDirectory: true)

    /// `limit`: larger files aren't copied at all (the staging limit would refuse them later).
    @MainActor static func copy(_ provider: NSItemProvider, limit: UInt64 = maxFileBytes(), root: URL = DropImport.root) async -> Outcome {
        let types = provider.registeredTypeIdentifiers.compactMap { UTType($0) }
        if types.contains(where: { $0.conforms(to: .folder) }) { return .refused(.notAFile) }
        let suggested = provider.suggestedName
        // A file URL names the file itself, so it comes first: another representation beside it
        // (plain text, a link's text) may be something else entirely, and loading "any item" of
        // a provider that offers only a URL would hand over the URL's text as the file.
        var failure = Outcome.refused(.unreadable(safeName(suggested)))
        if types.contains(where: { $0.conforms(to: .fileURL) }) {
            let viaURL: Outcome = await withCheckedContinuation { c in
                provider.loadItem(forTypeIdentifier: UTType.fileURL.identifier, options: nil) { item, _ in
                    let url = (item as? URL) ?? (item as? Data).flatMap { URL(dataRepresentation: $0, relativeTo: nil) }
                    guard let url, url.isFileURL else {
                        return c.resume(returning: .refused(.unreadable(safeName(suggested))))
                    }
                    c.resume(returning: take(url, suggested: suggested, limit: limit, root: root))
                }
            }
            // Only an unreadable URL falls back to the provider's own copy of the file; a folder
            // or a file too large is final.
            guard case .refused(.unreadable) = viaURL else { return viaURL }
            failure = viaURL
        }
        // The file's own type, in place (not a URL type).
        guard let own = types.first(where: { ($0.conforms(to: .data) || $0.conforms(to: .content)) && !$0.conforms(to: .url) })
        else { return failure }
        return await withCheckedContinuation { c in
            _ = provider.loadInPlaceFileRepresentation(forTypeIdentifier: own.identifier) { url, _, error in
                guard let url, error == nil else { return c.resume(returning: failure) }
                c.resume(returning: take(url, suggested: suggested, limit: limit, root: root))
            }
        }
    }

    /// Copies `url` into its own folder, coordinated (an iCloud file is downloaded first), after
    /// the checks that need no copy.
    private static func take(_ url: URL, suggested: String?, limit: UInt64, root: URL) -> Outcome {
        let name = displayName(of: url, suggested: suggested)
        var outcome = Outcome.refused(.unreadable(name))
        var coordinationError: NSError?
        NSFileCoordinator().coordinate(readingItemAt: url, options: [], error: &coordinationError) { readURL in
            let fm = FileManager.default
            guard let values = try? readURL.resourceValues(forKeys: [.fileSizeKey, .isDirectoryKey]),
                  let size = values.fileSize
            else { return }
            if values.isDirectory == true { outcome = .refused(.notAFile); return }
            if UInt64(size) > limit { outcome = .refused(.tooLarge(name)); return }
            let dir = root.appendingPathComponent(UUID().uuidString, isDirectory: true)
            do {
                try fm.createDirectory(at: dir, withIntermediateDirectories: true)
                let dest = dir.appendingPathComponent(name)
                try fm.copyItem(at: readURL, to: dest)
                // A file dragged from a folder is its own source; a handed-over temporary has none.
                let hidden = url.lastPathComponent.hasPrefix(Self.temporaryPrefix)
                outcome = .copied(dest, dir: dir, source: hidden ? dest : url)
            } catch {
                try? fm.removeItem(at: dir)
            }
        }
        return coordinationError == nil ? outcome : .refused(.unreadable(name))
    }

    private static let temporaryPrefix = ".com.apple.Foundation.NSItemProvider"

    /// A name safe to use as one path component, never empty.
    static func safeName(_ name: String?) -> String {
        let last = ((name ?? "") as NSString).lastPathComponent
        return last.isEmpty || last == "." || last == ".." ? "Dropped file" : last
    }

    /// The name to send under: the file's own, unless the system's is a hidden temporary one, then
    /// the provider's suggested name with the file's extension.
    static func displayName(of url: URL, suggested: String?) -> String {
        let own = url.lastPathComponent
        guard own.hasPrefix(temporaryPrefix) else { return safeName(own) }
        let ext = url.pathExtension
        let base = safeName(suggested)
        return ext.isEmpty || base.lowercased().hasSuffix("." + ext.lowercased()) ? base : base + "." + ext
    }

    /// Copies left by a run that quit before sending (staged files don't outlive the app): those older
    /// than `age` (0: all). The launch passes 0 and only when no other instance runs, since a second
    /// instance shares this folder.
    static func sweep(root: URL = DropImport.root, olderThan age: TimeInterval, now: Date = Date()) {
        let fm = FileManager.default
        guard let items = try? fm.contentsOfDirectory(at: root, includingPropertiesForKeys: [.contentModificationDateKey]) else { return }
        for item in items {
            let modified = (try? item.resourceValues(forKeys: [.contentModificationDateKey]))?.contentModificationDate
            if let modified, now.timeIntervalSince(modified) > age { try? fm.removeItem(at: item) }
        }
    }
}

/// An image on the clipboard (a screenshot, an image copied from an app): written into its own folder as a
/// file, which is then staged like a dropped one.
enum PasteImport {
    /// What Cmd+V does with the clipboard's types.
    enum Decision: Equatable {
        /// Files (copied in Finder) or an image: stage them.
        case stage
        /// Text, or nothing of ours: the field's own paste.
        case text
    }

    static func decide(_ types: [UTType], canAttach: Bool) -> Decision {
        guard canAttach else { return .text }
        // Copied files also carry their names as text, so a file URL decides before text does.
        if types.contains(where: { $0.conforms(to: .fileURL) }) { return .stage }
        // A cell copied from a sheet carries text beside its picture: text wins.
        if types.contains(where: { $0.conforms(to: .text) }) { return .text }
        return types.contains(where: { $0.conforms(to: .image) }) ? .stage : .text
    }

    /// The clipboard's files, or else its image, as the providers staging takes. Built by hand: a pasteboard
    /// can't hand out NSItemProvider objects (it isn't pasteboard-readable, so asking returns nil).
    static func providers(from pasteboard: NSPasteboard) -> [NSItemProvider] {
        let files = pasteboard.readObjects(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL] ?? []
        if !files.isEmpty { return files.compactMap { NSItemProvider(contentsOf: $0) } }
        // One image: the clipboard's other types are the same picture again (PNG, TIFF, ...).
        let types = (pasteboard.types ?? []).compactMap { UTType($0.rawValue) }
        guard let type = types.first(where: { $0.conforms(to: .png) }) ?? types.first(where: { $0.conforms(to: .image) }),
              let data = pasteboard.data(forType: NSPasteboard.PasteboardType(type.identifier))
        else { return [] }
        // Read now: the clipboard may change before the provider is asked.
        let provider = NSItemProvider()
        provider.registerDataRepresentation(forTypeIdentifier: type.identifier, visibility: .all) { done in
            done(data, nil)
            return nil
        }
        return [provider]
    }

    /// "Pasted image 2026-10-06 19.40.12.png": no colons (they show as slashes in Finder).
    static func name(at date: Date, ext: String) -> String {
        let f = DateFormatter()
        f.locale = Locale(identifier: "en_US_POSIX")
        f.dateFormat = "yyyy-MM-dd HH.mm.ss"
        return "Pasted image \(f.string(from: date)).\(ext)"
    }

    /// The image data as a file of its own (TIFF, which macOS puts on the clipboard for most copies,
    /// becomes PNG). Nil if it can't be made one.
    static func write(_ data: Data, type: UTType, limit: UInt64 = maxFileBytes(), now: Date = Date(),
                      root: URL = DropImport.root) -> DropImport.Outcome {
        var data = data
        var ext = type.preferredFilenameExtension ?? "png"
        if type.conforms(to: .tiff) {
            guard let png = NSBitmapImageRep(data: data)?.representation(using: .png, properties: [:])
            else { return .refused(.unreadable("The pasted image")) }
            data = png
            ext = "png"
        }
        let name = name(at: now, ext: ext)
        if UInt64(data.count) > limit { return .refused(.tooLarge(name)) }
        let dir = root.appendingPathComponent(UUID().uuidString, isDirectory: true)
        let dest = dir.appendingPathComponent(name)
        do {
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
            try data.write(to: dest)
        } catch {
            try? FileManager.default.removeItem(at: dir)
            return .refused(.unreadable(name))
        }
        return .copied(dest, dir: dir, source: dest)
    }

    /// The provider's image, as `write` takes it.
    @MainActor static func copy(_ provider: NSItemProvider) async -> DropImport.Outcome {
        let types = provider.registeredTypeIdentifiers.compactMap { UTType($0) }
        // PNG first (no conversion), then whatever image type is offered.
        guard let type = types.first(where: { $0.conforms(to: .png) }) ?? types.first(where: { $0.conforms(to: .image) })
        else { return .refused(.unreadable("The pasted image")) }
        let data: Data? = await withCheckedContinuation { c in
            _ = provider.loadDataRepresentation(forTypeIdentifier: type.identifier) { data, _ in c.resume(returning: data) }
        }
        guard let data else { return .refused(.unreadable("The pasted image")) }
        return write(data, type: type)
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
