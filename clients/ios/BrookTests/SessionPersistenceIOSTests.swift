// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
@testable import Brook
import Foundation
import Security
import Synchronization
import XCTest

/// A Keychain that records every call and answers as scripted, so these tests never touch the
/// real one (an unsigned test host can't reach it anyway).
private final class RecordingKeychain: SecItemCalls, @unchecked Sendable {
    // The tests call from one thread; the lock only satisfies `Sendable`.
    private let lock = NSLock()
    private var recordedDeletes: [[String: Any]] = []
    private var recordedLoads = 0
    private var deleteStatus: OSStatus = errSecSuccess
    private var loadStatus: OSStatus = errSecItemNotFound

    var deletes: [[String: Any]] { lock.withLock { recordedDeletes } }
    var loadCount: Int { lock.withLock { recordedLoads } }
    func failDeletes(with status: OSStatus) { lock.withLock { deleteStatus = status } }
    func failLoads(with status: OSStatus) { lock.withLock { loadStatus = status } }

    func add(_: [String: Any]) -> OSStatus { errSecSuccess }
    func copyMatching(_: [String: Any]) -> (OSStatus, Data?) {
        lock.withLock {
            recordedLoads += 1
            return (loadStatus, nil)
        }
    }
    func update(_: [String: Any], _: [String: Any]) -> OSStatus { errSecSuccess }
    func delete(_ query: [String: Any]) -> OSStatus {
        lock.withLock {
            recordedDeletes.append(query)
            return deleteStatus
        }
    }
}

/// The iOS persistence policy (`SessionPersistence.prepare`) and the first-launch Keychain
/// cleanup: a reinstalled iOS app keeps the old install's Keychain items, so the first launch
/// must delete them before anything can restore.
@MainActor
final class SessionPersistenceIOSTests: XCTestCase {
    private var tmp: URL!

    override func setUp() async throws {
        tmp = FileManager.default.temporaryDirectory.appending(path: "brook-persist-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: tmp, withIntermediateDirectories: true)
    }

    override func tearDown() async throws {
        try? FileManager.default.removeItem(at: tmp)
    }

    private var dataDir: URL { tmp.appending(path: "Brook", directoryHint: .isDirectory) }
    private func cleared(_ defaults: UserDefaults) -> Bool {
        defaults.object(forKey: SessionPersistence.keychainClearedKey) != nil
    }
    private func isOff(_ p: SessionPersistence) -> Bool {
        if case .off = p { return true }
        return false
    }

    /// Case 1.
    func testFirstLaunchDeletesTheWholeServiceOnceMarksItAndTurnsPersistenceOn() throws {
        let keychain = RecordingKeychain()
        let defaults = isolatedDefaults()
        let result = SessionPersistence.prepare(dataDir: dataDir, calls: keychain, defaults: defaults)

        XCTAssertEqual(keychain.deletes.count, 1)
        let q = try XCTUnwrap(keychain.deletes.first)
        XCTAssertEqual(q[kSecAttrService as String] as? String, "dev.brook.Brook.datakey")
        XCTAssertNil(q[kSecAttrAccount as String], "an account would delete one slot, not the service")
        XCTAssertNil(q[kSecAttrAccessGroup as String], "the app's own default group")
        XCTAssertTrue(cleared(defaults))
        XCTAssertTrue(FileManager.default.fileExists(atPath: dataDir.path))
        guard case let .on(_, path) = result else { return XCTFail("expected .on, got \(result)") }
        XCTAssertEqual(path, dataDir.path(percentEncoded: false))
    }

    /// Case 2.
    func testSecondLaunchDoesNotDeleteAgain() {
        let keychain = RecordingKeychain()
        let defaults = isolatedDefaults()
        _ = SessionPersistence.prepare(dataDir: dataDir, calls: keychain, defaults: defaults)
        let second = SessionPersistence.prepare(dataDir: dataDir, calls: keychain, defaults: defaults)
        XCTAssertEqual(keychain.deletes.count, 1, "the session of this install would be deleted at every launch")
        guard case .on = second else { return XCTFail("expected .on, got \(second)") }
    }

    /// Case 3. The marker must wait for the delete: set before it, a failed delete would never
    /// be retried and the old install's session could be restored.
    func testAFailingDeleteStaysOffLeavesTheMarkerUnsetAndIsRetried() {
        let keychain = RecordingKeychain()
        keychain.failDeletes(with: errSecInteractionNotAllowed)
        let defaults = isolatedDefaults()
        let first = SessionPersistence.prepare(dataDir: dataDir, calls: keychain, defaults: defaults)
        XCTAssertTrue(isOff(first))
        XCTAssertFalse(cleared(defaults), "the marker was set although the delete failed")
        XCTAssertEqual(keychain.loadCount, 0, "a launch that could not clear must not even read the Keychain")

        keychain.failDeletes(with: errSecSuccess)
        let next = SessionPersistence.prepare(dataDir: dataDir, calls: keychain, defaults: defaults)
        XCTAssertEqual(keychain.deletes.count, 2, "the next launch must try the delete again")
        XCTAssertTrue(cleared(defaults))
        guard case .on = next else { return XCTFail("expected .on, got \(next)") }
    }

    /// Case 4: a path under a regular file can't be a directory.
    func testADataDirectoryThatCannotBeMadeStaysOff() throws {
        let file = tmp.appending(path: "plain-file")
        try Data("x".utf8).write(to: file)
        let result = SessionPersistence.prepare(
            dataDir: file.appending(path: "Brook", directoryHint: .isDirectory),
            calls: RecordingKeychain(), defaults: isolatedDefaults())
        XCTAssertTrue(isOff(result))
    }

    /// Case 5: -34018 is "this build has no Keychain access", which never fixes itself.
    func testAFatalProbeStaysOff() {
        let keychain = RecordingKeychain()
        keychain.failLoads(with: errSecMissingEntitlement)
        let result = SessionPersistence.prepare(dataDir: dataDir, calls: keychain, defaults: isolatedDefaults())
        XCTAssertEqual(errSecMissingEntitlement, -34018)
        XCTAssertTrue(isOff(result))
    }

    /// A locked Keychain is not a fault: core reports it at restore, so persistence stays on.
    func testALockedProbeStaysOn() {
        let keychain = RecordingKeychain()
        keychain.failLoads(with: errSecInteractionNotAllowed)
        let result = SessionPersistence.prepare(dataDir: dataDir, calls: keychain, defaults: isolatedDefaults())
        guard case .on = result else { return XCTFail("expected .on, got \(result)") }
    }

    /// Case 6: `defaults write ... AllowInsecureHTTP` in the simulator before the first launch
    /// leaves the app's defaults non-empty with no marker. The cleanup must still run.
    func testOtherDefaultsAlreadySetDoNotSkipTheFirstLaunchCleanup() {
        let keychain = RecordingKeychain()
        let defaults = isolatedDefaults()
        defaults.set(true, forKey: Settings.allowInsecureKey)
        _ = SessionPersistence.prepare(dataDir: dataDir, calls: keychain, defaults: defaults)
        XCTAssertEqual(keychain.deletes.count, 1, "AllowInsecureHTTP made the cleanup skip")
        XCTAssertTrue(cleared(defaults))
    }
}
