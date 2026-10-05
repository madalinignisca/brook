import Foundation
import UniformTypeIdentifiers
import BrookCore
import XCTest

@testable import Brook

/// Dropped files are copied out of their provider at once and the copy is staged (the plain
/// dropped URL was not reliably readable in the sandbox, "couldn't be read").
@MainActor final class DropImportTests: XCTestCase {
    private var tmp: URL!
    private var root: URL!

    override func setUp() async throws {
        tmp = FileManager.default.temporaryDirectory.appendingPathComponent("drop-\(UUID().uuidString)", isDirectory: true)
        root = tmp.appendingPathComponent("copies", isDirectory: true)
        try FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
    }

    override func tearDown() async throws { try? FileManager.default.removeItem(at: tmp) }

    /// A provider as Finder makes one: the file, with its name suggested.
    private func provider(_ url: URL) -> NSItemProvider {
        let p = NSItemProvider(contentsOf: url)!
        p.suggestedName = url.lastPathComponent
        return p
    }

    private func source(_ name: String, bytes: Int = 10) throws -> URL {
        let url = tmp.appendingPathComponent(name)
        try Data(repeating: 7, count: bytes).write(to: url)
        return url
    }

    func testCopiesTheFileUnderItsNameIntoItsOwnFolder() async throws {
        let src = try source("1.png")
        guard case let .copied(url, dir, _) = await DropImport.copy(provider(src), root: root) else {
            return XCTFail("expected a copy")
        }
        XCTAssertEqual(url.lastPathComponent, "1.png")
        XCTAssertEqual(url.deletingLastPathComponent(), dir)
        XCTAssertTrue(dir.path.hasPrefix(root.path))
        XCTAssertEqual(try Data(contentsOf: url), try Data(contentsOf: src))
    }

    func testAFileOverTheLimitIsNotCopied() async throws {
        let src = try source("big.bin", bytes: 100)
        let outcome = await DropImport.copy(provider(src), limit: 50, root: root)
        guard case let .refused(refusal) = outcome else { return XCTFail("expected a refusal") }
        XCTAssertEqual(refusal, .tooLarge("big.bin"))
        XCTAssertFalse(FileManager.default.fileExists(atPath: root.path), "nothing was copied")
    }

    func testAFolderIsRefusedAsNotAFile() async throws {
        let folder = tmp.appendingPathComponent("a-folder", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        // A folder as Finder offers it: a provider whose type is a folder.
        // Its loader must never run: a folder is refused by its type, before anything is read.
        let p = NSItemProvider()
        p.registerFileRepresentation(forTypeIdentifier: UTType.folder.identifier, fileOptions: [], visibility: .all) { done in
            XCTFail("a folder's content was loaded")
            done(folder, true, nil)
            return nil
        }
        guard case let .refused(refusal) = await DropImport.copy(p, root: root) else {
            return XCTFail("expected a refusal")
        }
        XCTAssertEqual(refusal, .notAFile)
    }

    func testTheCopyGoesWithItsStagedFile() async throws {
        let src = try source("2.png")
        guard case let .copied(url, dir, _) = await DropImport.copy(provider(src), root: root) else {
            return XCTFail("expected a copy")
        }
        guard case let .success(file) = Staging.stage(url, already: [], access: SystemFileAccess(), ownedDir: dir) else {
            return XCTFail("expected it staged")
        }
        XCTAssertTrue(FileManager.default.fileExists(atPath: url.path))
        file.release()
        XCTAssertFalse(FileManager.default.fileExists(atPath: dir.path), "released: the copy is gone")
        XCTAssertTrue(FileManager.default.fileExists(atPath: src.path), "the user's own file is untouched")
    }

    func testARefusedCopyLeavesNothingBehind() async throws {
        let src = try source("empty.bin", bytes: 0)
        guard case let .copied(url, dir, _) = await DropImport.copy(provider(src), root: root) else {
            return XCTFail("expected a copy")
        }
        XCTAssertEqual(Staging.stage(url, already: [], access: SystemFileAccess(), ownedDir: dir).failure, .empty("empty.bin"))
        XCTAssertFalse(FileManager.default.fileExists(atPath: dir.path))
    }

    func testAHiddenTemporaryNameBecomesTheSuggestedName() {
        let hidden = URL(fileURLWithPath: "/x/.com.apple.Foundation.NSItemProvider123.png")
        XCTAssertEqual(DropImport.displayName(of: hidden, suggested: "Photo"), "Photo.png")
        XCTAssertEqual(DropImport.displayName(of: hidden, suggested: "Photo.png"), "Photo.png", "not doubled")
        XCTAssertEqual(DropImport.displayName(of: hidden, suggested: nil), "Dropped file.png")
        XCTAssertEqual(DropImport.displayName(of: hidden, suggested: ""), "Dropped file.png")
        XCTAssertEqual(DropImport.displayName(of: URL(fileURLWithPath: "/x/IMG-99.jpg"), suggested: "other"), "IMG-99.jpg")
    }

    /// A provider that offers only a file URL (as a Finder drag may): the file is read through the
    /// URL, and what is staged is the file, never the URL's text.
    func testAProviderWithOnlyAFileURLCopiesTheFileNotTheURLText() async throws {
        let src = try source("only-url.png", bytes: 33)
        let p = NSItemProvider()
        p.suggestedName = "only-url"
        p.registerDataRepresentation(forTypeIdentifier: UTType.fileURL.identifier, visibility: .all) { done in
            done(src.dataRepresentation, nil)
            return nil
        }
        guard case let .copied(url, _, _) = await DropImport.copy(p, root: root) else { return XCTFail("expected a copy") }
        XCTAssertEqual(try Data(contentsOf: url), try Data(contentsOf: src), "the file's bytes, not 'file:///…'")
        XCTAssertEqual(url.lastPathComponent, "only-url.png")
    }

    func testAProviderWithNothingFileLikeIsRefused() async {
        let p = NSItemProvider()
        p.registerDataRepresentation(forTypeIdentifier: UTType.url.identifier, visibility: .all) { done in
            done(Data("https://example.com".utf8), nil)
            return nil
        }
        guard case let .refused(refusal) = await DropImport.copy(p, root: root) else { return XCTFail("expected a refusal") }
        XCTAssertEqual(refusal, .unreadable("Dropped file"))
    }

    func testNamesAreSafePathComponentsAndExtensionsMatchWithoutCase() {
        XCTAssertEqual(DropImport.safeName("../../evil.png"), "evil.png")
        XCTAssertEqual(DropImport.safeName(".."), "Dropped file")
        XCTAssertEqual(DropImport.safeName(""), "Dropped file")
        XCTAssertEqual(DropImport.safeName(nil), "Dropped file")
        let hidden = URL(fileURLWithPath: "/x/.com.apple.Foundation.NSItemProvider1.PNG")
        XCTAssertEqual(DropImport.displayName(of: hidden, suggested: "Photo.png"), "Photo.png", "no Photo.png.PNG")
    }

    func testTheSameFileDroppedTwiceIsStagedOnceAndTheSecondCopyIsRemoved() async throws {
        let src = try source("same.png")
        // A Finder drop is in place: the provider hands over the file where it lies.
        func inPlace() -> NSItemProvider {
            let p = NSItemProvider()
            p.registerFileRepresentation(forTypeIdentifier: UTType.png.identifier, fileOptions: [.openInPlace], visibility: .all) { done in
                done(src, true, nil)
                return nil
            }
            return p
        }
        guard case let .copied(u1, d1, s1) = await DropImport.copy(inPlace(), root: root),
              case let .copied(u2, d2, s2) = await DropImport.copy(inPlace(), root: root)
        else { return XCTFail("expected copies") }
        XCTAssertNotEqual(u1, u2, "each drop has its own copy")
        guard case let .success(first) = Staging.stage(u1, already: [], access: SystemFileAccess(), ownedDir: d1, source: s1)
        else { return XCTFail("expected it staged") }
        XCTAssertEqual(Staging.stage(u2, already: [first], access: SystemFileAccess(), ownedDir: d2, source: s2).failure, .duplicate)
        XCTAssertFalse(FileManager.default.fileExists(atPath: d2.path), "the refused copy is gone")
        XCTAssertTrue(FileManager.default.fileExists(atPath: d1.path))
    }

    // ---- The composer's side ----

    private func composer() -> ComposerModel {
        let chat = FakeChat()
        chat.local = true
        return ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
    }

    func testAComposerThatClosedDuringTheCopyStagesNothingAndRemovesTheCopy() async throws {
        let c = composer()
        let src = try source("late.png")
        c.importer = { [root] p in
            c.readOnly = true // the composer moved on while the file was being copied
            return await DropImport.copy(p, root: root!)
        }
        await c.attach(dropped: [provider(src)])
        XCTAssertTrue(c.staged.isEmpty)
        XCTAssertEqual(c.error, "late.png wasn't attached.")
        let left = (try? FileManager.default.contentsOfDirectory(atPath: root.path)) ?? []
        XCTAssertTrue(left.isEmpty, "no copy left behind")
    }

    func testAFullMessageCopiesNothingMore() async throws {
        let c = composer()
        var copies = 0
        c.importer = { [root] p in
            copies += 1
            return await DropImport.copy(p, root: root!)
        }
        let limit = Int(maxFilesPerMessage())
        var providers: [NSItemProvider] = []
        for i in 0 ..< limit + 3 { providers.append(provider(try source("f\(i).png"))) }
        await c.attach(dropped: providers)
        XCTAssertEqual(c.staged.count, limit)
        XCTAssertEqual(copies, limit, "the files past the limit were not copied")
        XCTAssertEqual(c.error, StagingRefusal.tooMany.text)
    }

    func testStagedCopiesGoWhenTheComposerGoes() async throws {
        var dir: URL?
        do {
            let c = composer()
            c.importer = { [root] p in await DropImport.copy(p, root: root!) }
            await c.attach(dropped: [provider(try source("abandoned.png"))])
            dir = c.staged.first?.url.deletingLastPathComponent()
            XCTAssertNotNil(dir)
            XCTAssertTrue(FileManager.default.fileExists(atPath: dir!.path))
        }
        XCTAssertFalse(FileManager.default.fileExists(atPath: dir!.path), "the abandoned draft's copy is gone")
    }

    func testSweepRemovesOnlyOldCopies() throws {
        let fm = FileManager.default
        let old = root.appendingPathComponent("old"), fresh = root.appendingPathComponent("fresh")
        try fm.createDirectory(at: old, withIntermediateDirectories: true)
        try fm.createDirectory(at: fresh, withIntermediateDirectories: true)
        try fm.setAttributes([.modificationDate: Date(timeIntervalSinceNow: -200_000)], ofItemAtPath: old.path)
        DropImport.sweep(root: root)
        XCTAssertFalse(fm.fileExists(atPath: old.path))
        XCTAssertTrue(fm.fileExists(atPath: fresh.path))
    }
}
