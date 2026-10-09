// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import AppKit
import BrookCore
import XCTest

@testable import Brook

/// The composer the Mac ships (`ChatView.makeComposer`) must write a pasted image out as a PNG
/// file. The shared `ComposerModel` cannot do that itself (it has no AppKit), so the Mac sets
/// `importer` in `makeComposer`; this test fails if that line goes.
@MainActor
final class MacComposerTests: XCTestCase {
    func testTheMacComposerWritesAPastedImageOut() async throws {
        let chat = FakeChat()
        chat.local = true // attaching files needs local data
        let composer = ChatView.makeComposer(
            channelId: "c", client: chat, timeline: TimelineModel(channelId: "c", client: chat), pending: nil)

        // What the clipboard holds for a screenshot: image data, and no file behind it.
        let image = NSImage(size: NSSize(width: 4, height: 4), flipped: false) { rect in
            NSColor.green.setFill(); rect.fill(); return true
        }
        let tiff = try XCTUnwrap(image.tiffRepresentation)
        let provider = NSItemProvider()
        provider.registerDataRepresentation(forTypeIdentifier: "public.tiff", visibility: .all) { done in
            done(tiff, nil)
            return nil
        }

        await composer.attach(dropped: [provider])

        // DropImport.copy would have refused this (no file), or named it differently.
        XCTAssertNil(composer.error)
        let file = try XCTUnwrap(composer.staged.first)
        XCTAssertTrue(file.name.hasPrefix("Pasted image"), file.name)
        XCTAssertTrue(file.name.hasSuffix(".png"), file.name)
    }
}
