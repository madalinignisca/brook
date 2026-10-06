import AppKit
import Testing
import UniformTypeIdentifiers

@testable import Brook

@Suite struct PasteImageTests {
    @Test func anImageAloneIsStaged() {
        #expect(PasteImport.decide([.png, .tiff], canAttach: true) == .stage)
    }

    @Test func copiedFilesAreStagedEvenWithTheirNamesAsText() {
        #expect(PasteImport.decide([.fileURL, .utf8PlainText], canAttach: true) == .stage)
    }

    @Test func textBesideAnImageStaysText() {
        #expect(PasteImport.decide([.png, .utf8PlainText, .html], canAttach: true) == .text)
    }

    @Test func nothingIsStagedWhenAttachingIsClosed() {
        #expect(PasteImport.decide([.png], canAttach: false) == .text)
    }

    @Test func theNameHasNoColon() {
        let name = PasteImport.name(at: Date(timeIntervalSince1970: 0), ext: "png")
        #expect(name.hasPrefix("Pasted image 19") && name.hasSuffix(".png") && !name.contains(":"))
    }

    @Test func tiffBecomesAPngFile() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("paste-\(UUID())")
        defer { try? FileManager.default.removeItem(at: root) }
        let image = NSImage(size: NSSize(width: 4, height: 4), flipped: false) { rect in
            NSColor.red.setFill(); rect.fill(); return true
        }
        let tiff = try #require(image.tiffRepresentation)
        guard case let .copied(url, dir, _) = PasteImport.write(tiff, type: .tiff, root: root) else {
            Issue.record("not copied"); return
        }
        #expect(url.pathExtension == "png")
        #expect(dir.deletingLastPathComponent().path == root.path)
        let written = try Data(contentsOf: url)
        #expect(written.prefix(4) == Data([0x89, 0x50, 0x4E, 0x47])) // the PNG signature
    }

    @Test func aTooLargeImageIsRefusedAndLeavesNothing() throws {
        let root = FileManager.default.temporaryDirectory.appendingPathComponent("paste-\(UUID())")
        defer { try? FileManager.default.removeItem(at: root) }
        let outcome = PasteImport.write(Data(count: 10), type: .png, limit: 5, root: root)
        guard case .refused(.tooLarge) = outcome else { Issue.record("not refused"); return }
        #expect(!FileManager.default.fileExists(atPath: root.path))
    }
}
