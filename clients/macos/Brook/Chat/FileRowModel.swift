import BrookCore
import CoreGraphics
import Foundation
import Network
import Observation

/// One attachment's Open, keep-offline toggle and inline preview (spec
/// 2026-09-26-mac-file-row). Without local data (every call answers `local.unavailable`)
/// it offers nothing, and the row is Save only, as before #79.
@MainActor
@Observable
final class FileRowModel {
    enum Keep: Equatable {
        case off
        /// Pinned, downloading: its transfer id when the download is running.
        case fetching(UInt64?)
        case kept
    }

    enum Preview {
        case none
        /// Too large to fetch by itself (or an expensive connection): "Show preview".
        case offer
        case loading
        case shown(CGImage)
    }

    let file: FfiFileInfo
    /// This Mac's storage answered for this file: Open and the toggle show.
    private(set) var hasLocalData = false
    private(set) var keep: Keep = .off
    private(set) var keepBusy = false
    private(set) var opening = false
    private(set) var gone = false
    /// Open's or the toggle's last problem.
    private(set) var message: String?
    private(set) var preview: Preview = .none
    /// On screen: a preview request of a row that went is dropped.
    var onScreen = false {
        didSet { visible.set(onScreen) }
    }
    /// `onScreen`, readable from any thread: the decoder's queue asks from its own actor.
    private let visible = Flag()

    private let client: any OfflineClient
    private let opener: (URL) -> Bool
    private let exists: (String) -> Bool
    private let expensive: () -> Bool
    private let decode: (FfiImagePreview, @escaping ImageDecoder.Alive) async -> CGImage?
    private var reads = 0

    /// Automatic previews up to this size (larger ones wait for "Show preview").
    static let autoMaxBytes: UInt64 = 4 * 1024 * 1024
    static let previewTypes: Set<String> = ["image/png", "image/jpeg", "image/gif", "image/webp"]

    init(file: FfiFileInfo, client: any OfflineClient,
         opener: @escaping (URL) -> Bool = { NSWorkspaceOpener.open($0) },
         exists: @escaping (String) -> Bool = { FileManager.default.fileExists(atPath: $0) },
         expensive: @escaping () -> Bool = { NetworkCost.shared.expensive },
         decode: @escaping (FfiImagePreview, @escaping ImageDecoder.Alive) async -> CGImage? = FileRowModel.liveDecode) {
        self.file = file
        self.client = client
        self.opener = opener
        self.exists = exists
        self.expensive = expensive
        self.decode = decode
    }

    // ---- Keep available offline ----

    /// Read the cache's state; only the newest read applies.
    func reloadKeep() async {
        reads += 1
        let mine = reads
        let answer: FfiFileCacheState
        do { answer = try await client.fileState(fileId: file.id) } catch {
            if mine == reads, error.isLocalUnavailable { hasLocalData = false }
            return
        }
        guard mine == reads else { return }
        hasLocalData = true
        keep = Self.view(answer)
    }

    static func view(_ state: FfiFileCacheState) -> Keep {
        switch state {
        case let .pinned(cached, _, _, transfer): cached ? .kept : .fetching(transfer)
        case .notCached, .partial, .cached: .off
        }
    }

    /// Pin or unpin; one call at a time, then the cache's word (also after a failure).
    func toggleKeep() async {
        guard !keepBusy, hasLocalData else { return }
        keepBusy = true
        defer { keepBusy = false }
        do {
            if keep == .off { try await client.pinFile(fileId: file.id) } else { try await client.unpinFile(fileId: file.id) }
            message = nil
        } catch {
            message = Self.keepErrorText(error)
        }
        await reloadKeep()
    }

    static func keepErrorText(_ error: Error) -> String {
        switch error as? LoginError {
        case let .Api(code, _) where code == "local.unavailable": "Keeping files offline needs this Mac's storage"
        case let .Api(code, _) where code == "file.unknown": "Not available yet. Try again in a moment"
        case let .Api(code, _) where code == "file.gone": "No longer available."
        default: "Couldn't change it. Try again."
        }
    }

    // ---- Open ----

    /// Core's private copy, opened exactly where core put it.
    func open() async {
        guard hasLocalData, !opening else { return }
        opening = true
        defer { opening = false }
        let path: String
        do {
            path = try await client.openFile(transferId: UInt64.random(in: 1 ... UInt64.max), fileId: file.id)
        } catch {
            if case let .Api(code, _) = error as? LoginError, code == "file.gone" { gone = true }
            message = Self.openErrorText(error)
            return
        }
        guard exists(path) else {
            message = "Not available yet. Try again in a moment"
            return
        }
        message = opener(URL(fileURLWithPath: path)) ? nil : "No app opens this. Save it instead"
    }

    static func openErrorText(_ error: Error) -> String {
        switch error as? LoginError {
        case let .Api(code, _) where code == "file.open_refused": "Can't be opened from Brook. Save it instead"
        case let .Api(code, _) where code == "local.unavailable": "Open needs this Mac's storage. Save it instead"
        case let .Api(code, _) where code == "file.unknown": "Not available yet. Try again in a moment"
        case let .Api(code, _) where code == "file.gone": "No longer available."
        default: "Couldn't open the file."
        }
    }

    // ---- Inline preview ----

    /// A declared image type the preview may be tried for (core's sniff still decides).
    var previewable: Bool {
        let type = file.contentType.split(separator: ";").first.map { $0.trimmingCharacters(in: .whitespaces).lowercased() } ?? ""
        return Self.previewTypes.contains(type) && file.size <= previewMaxBytes()
    }

    /// What the row starts with: automatic for small images (unless the connection is
    /// expensive and the file isn't here), "Show preview" otherwise, nothing for the rest.
    func startPreview() async {
        guard hasLocalData, previewable, case .none = preview else { return }
        var here = keep == .kept
        if !here, case .cached? = try? await client.fileState(fileId: file.id) { here = true }
        if file.size <= Self.autoMaxBytes, !expensive() || here {
            await showPreview()
        } else {
            preview = .offer
        }
    }

    /// Fetch into the cache, decode in the sandboxed broker, show; any failure: no preview.
    func showPreview() async {
        guard hasLocalData, previewable, onScreen else { return }
        preview = .loading
        let bytes: FfiImagePreview
        do {
            bytes = try await client.previewFile(transferId: UInt64.random(in: 1 ... UInt64.max), fileId: file.id)
        } catch {
            preview = .none
            return
        }
        let visible = self.visible
        let image = await decode(bytes, { visible.value })
        preview = image.map(Preview.shown) ?? .none
    }

    nonisolated static func liveDecode(_ p: FfiImagePreview, _ alive: @escaping ImageDecoder.Alive) async -> CGImage? {
        let kind: ImageKindCode = switch p.kind {
        case .png: .png
        case .jpeg: .jpeg
        case .gif: .gif
        case .webp: .webp
        }
        return await ImageDecoder.shared.thumbnail(bytes: p.bytes, kind: kind,
                                                   header: (Int(p.width), Int(p.height)), alive: alive)
    }
}

/// A flag any thread may read.
final class Flag: @unchecked Sendable {
    private let lock = NSLock()
    private var current = false
    var value: Bool { lock.withLock { current } }
    func set(_ v: Bool) { lock.withLock { current = v } }
}

/// Opens a file in its default app (the system picks by type).
enum NSWorkspaceOpener {
    @MainActor static func open(_ url: URL) -> Bool {
        NSWorkspaceBridge.open(url)
    }
}

/// Whether the current connection is expensive or constrained (cellular, a hotspot, Low
/// Data Mode): automatic previews wait then.
final class NetworkCost: @unchecked Sendable {
    static let shared = NetworkCost()
    private let monitor = NWPathMonitor()
    private let lock = NSLock()
    private var path: NWPath?

    private init() {
        monitor.pathUpdateHandler = { [weak self] p in self?.lock.withLock { self?.path = p } }
        monitor.start(queue: DispatchQueue(label: "dev.brook.network-cost"))
    }

    var expensive: Bool {
        lock.withLock { path.map { $0.isExpensive || $0.isConstrained } ?? false }
    }
}
