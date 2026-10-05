import Foundation
import UniformTypeIdentifiers
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
        guard case let .copied(url, dir) = await DropImport.copy(provider(src), root: root) else {
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
        guard case let .copied(url, dir) = await DropImport.copy(provider(src), root: root) else {
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
        guard case let .copied(url, dir) = await DropImport.copy(provider(src), root: root) else {
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
