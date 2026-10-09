// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import AppKit
import BrookCore
import Foundation
import UniformTypeIdentifiers

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

extension ComposerModel {
    /// How the Mac copies a dropped or pasted provider out. It lives here, not in `ComposerModel`,
    /// because it names `PasteImport`, which needs AppKit, and the shared conversation models
    /// (which iOS compiles too) have none. `ChatView.makeComposer` sets it on the composer.
    static let macImporter: (NSItemProvider) async -> DropImport.Outcome = { provider in
        let types = provider.registeredTypeIdentifiers.compactMap { UTType($0) }
        // An image with no file behind it (the clipboard's) is written out; anything else is a file.
        let imageOnly = !types.contains { $0.conforms(to: .fileURL) } && types.contains { $0.conforms(to: .image) }
        return imageOnly ? await PasteImport.copy(provider) : await DropImport.copy(provider)
    }
}
