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
    private var recordedEvents: [String] = []
    private var onDelete: (() -> Void)?
    private var deleteStatus: OSStatus = errSecSuccess
    private var loadStatus: OSStatus = errSecItemNotFound

    var deletes: [[String: Any]] { lock.withLock { recordedDeletes } }
    /// Every delete and load, in the order they happened.
    var events: [String] { lock.withLock { recordedEvents } }
    /// Runs inside `delete`, before it answers: lets a test look at the world at that moment.
    func beforeDelete(_ check: @escaping () -> Void) { lock.withLock { onDelete = check } }
    var loadCount: Int { lock.withLock { recordedLoads } }
    func failDeletes(with status: OSStatus) { lock.withLock { deleteStatus = status } }
    func failLoads(with status: OSStatus) { lock.withLock { loadStatus = status } }

    func add(_: [String: Any]) -> OSStatus { errSecSuccess }
    func copyMatching(_: [String: Any]) -> (OSStatus, Data?) {
        lock.withLock {
            recordedLoads += 1
            recordedEvents.append("load")
            return (loadStatus, nil)
        }
    }
    func update(_: [String: Any], _: [String: Any]) -> OSStatus { errSecSuccess }
    func delete(_ query: [String: Any]) -> OSStatus {
        // The hook runs outside the lock so it may call back into the fake.
        let hook = lock.withLock { onDelete }
        hook?()
        return lock.withLock {
            recordedDeletes.append(query)
            recordedEvents.append("delete")
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

    /// Isolated defaults whose persistent domain name is known (`isolatedDefaults()` doesn't
    /// return it, and `prepare` reads the marker from that domain only).
    private func suite() -> (defaults: UserDefaults, domain: String) {
        let domain = "brook.tests.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: domain)!
        addTeardownBlock { defaults.removePersistentDomain(forName: domain) }
        return (defaults, domain)
    }
    private func prepare(
        _ keychain: SecItemCalls, _ s: (defaults: UserDefaults, domain: String), dir: URL? = nil,
        probe: SessionPersistence.ProtectedData = .available
    ) -> SessionPersistence {
        SessionPersistence.prepare(
            dataDir: dir ?? dataDir, calls: keychain, defaults: s.defaults, defaultsDomain: s.domain,
            protectedData: { probe })
    }
    private var dataDir: URL { tmp.appending(path: "Brook", directoryHint: .isDirectory) }
    private func cleared(_ s: (defaults: UserDefaults, domain: String)) -> Bool {
        s.defaults.persistentDomain(forName: s.domain)?[SessionPersistence.keychainClearedKey] != nil
    }
    private func isOff(_ p: SessionPersistence) -> Bool {
        if case .off = p { return true }
        return false
    }

    /// Case 1.
    func testFirstLaunchDeletesTheWholeServiceOnceMarksItAndTurnsPersistenceOn() throws {
        let keychain = RecordingKeychain()
        let defaults = suite()
        keychain.beforeDelete { [self] in XCTAssertFalse(cleared(defaults), "the marker was set before the delete") }
        let result = prepare(keychain, defaults)

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
        let defaults = suite()
        _ = prepare(keychain, defaults)
        let second = prepare(keychain, defaults)
        XCTAssertEqual(keychain.deletes.count, 1, "the session of this install would be deleted at every launch")
        guard case .on = second else { return XCTFail("expected .on, got \(second)") }
    }

    /// Case 3. The marker must wait for the delete: set before it, a failed delete would never
    /// be retried and the old install's session could be restored.
    func testAFailingDeleteStaysOffLeavesTheMarkerUnsetAndIsRetried() {
        let keychain = RecordingKeychain()
        keychain.failDeletes(with: errSecInteractionNotAllowed)
        let defaults = suite()
        keychain.beforeDelete { [self] in XCTAssertFalse(cleared(defaults), "the marker was set before the delete") }
        let first = prepare(keychain, defaults)
        XCTAssertTrue(isOff(first))
        XCTAssertFalse(cleared(defaults), "the marker was set although the delete failed")
        XCTAssertEqual(keychain.loadCount, 0, "a launch that could not clear must not even read the Keychain")

        keychain.failDeletes(with: errSecSuccess)
        let next = prepare(keychain, defaults)
        XCTAssertEqual(keychain.deletes.count, 2, "the next launch must try the delete again")
        XCTAssertTrue(cleared(defaults))
        guard case .on = next else { return XCTFail("expected .on, got \(next)") }
    }

    /// Case 4: a path under a regular file can't be a directory.
    func testADataDirectoryThatCannotBeMadeStaysOff() throws {
        let file = tmp.appending(path: "plain-file")
        try Data("x".utf8).write(to: file)
        let result = prepare(
            RecordingKeychain(), suite(), dir: file.appending(path: "Brook", directoryHint: .isDirectory))
        XCTAssertTrue(isOff(result))
    }

    /// Case 5: -34018 is "this build has no Keychain access", which never fixes itself.
    func testAFatalProbeStaysOff() {
        let keychain = RecordingKeychain()
        keychain.failLoads(with: errSecMissingEntitlement)
        let result = prepare(keychain, suite())
        XCTAssertEqual(errSecMissingEntitlement, -34018)
        XCTAssertTrue(isOff(result))
    }

    /// A locked Keychain is not a fault: core reports it at restore, so persistence stays on.
    func testALockedProbeStaysOn() {
        let keychain = RecordingKeychain()
        keychain.failLoads(with: errSecInteractionNotAllowed)
        let result = prepare(keychain, suite())
        guard case .on = result else { return XCTFail("expected .on, got \(result)") }
    }

    /// Case 6: `defaults write ... AllowInsecureHTTP` in the simulator before the first launch
    /// leaves the app's defaults non-empty with no marker. The cleanup must still run.
    func testOtherDefaultsAlreadySetDoNotSkipTheFirstLaunchCleanup() {
        let keychain = RecordingKeychain()
        let defaults = suite()
        defaults.defaults.set(true, forKey: Settings.allowInsecureKey)
        _ = prepare(keychain, defaults)
        XCTAssertEqual(keychain.deletes.count, 1, "AllowInsecureHTTP made the cleanup skip")
        XCTAssertTrue(cleared(defaults))
    }

    /// L1: before the first unlock after a reboot the defaults read as empty and the Keychain
    /// delete finds nothing, so the cleanup would "succeed" and set the marker with the old
    /// token still stored. Nothing may happen until protected data is available.
    func testProtectedDataUnavailableStaysOffWithoutDeletingOrMarking() {
        let keychain = RecordingKeychain()
        let defaults = suite()
        let locked = prepare(keychain, defaults, probe: .locked)
        guard case .lockedUntilFirstUnlock = locked else { return XCTFail("expected .lockedUntilFirstUnlock, got \(locked)") }
        XCTAssertEqual(keychain.events, [], "a locked launch touched the Keychain")
        XCTAssertFalse(cleared(defaults), "a locked launch set the marker")
        XCTAssertFalse(FileManager.default.fileExists(atPath: dataDir.path))

        // The next launch, unlocked, does the cleanup as for a first launch.
        let next = prepare(keychain, defaults)
        XCTAssertEqual(keychain.deletes.count, 1)
        guard case .on = next else { return XCTFail("expected .on, got \(next)") }
    }

    /// L2: a launch argument (`-KeychainCleared YES`) lives in the argument domain, which
    /// `UserDefaults.object(forKey:)` searches. Only the persistent domain may count.
    func testAMarkerOnlyInANonPersistentDomainDoesNotCount() {
        let keychain = RecordingKeychain()
        let defaults = suite()
        // The argument domain is shared by every UserDefaults in the process, so put back what
        // was there rather than leave the key for other tests to find.
        let before = defaults.defaults.volatileDomain(forName: UserDefaults.argumentDomain)
        defaults.defaults.setVolatileDomain(
            before.merging([SessionPersistence.keychainClearedKey: true]) { $1 },
            forName: UserDefaults.argumentDomain)
        addTeardownBlock { defaults.defaults.setVolatileDomain(before, forName: UserDefaults.argumentDomain) }
        XCTAssertNotNil(defaults.defaults.object(forKey: SessionPersistence.keychainClearedKey),
                        "the premise: a plain object(forKey:) does see the argument")
        _ = prepare(keychain, defaults)
        XCTAssertEqual(keychain.deletes.count, 1, "a launch argument skipped the cleanup")
        XCTAssertTrue(cleared(defaults))
    }

    /// The order the cleanup depends on: delete, and only then (a) the marker, (b) any read of
    /// the Keychain. A restore is a read, so nothing can restore the old install's session
    /// before the delete. (The marker half is asserted inside the fake's delete, above.)
    func testTheCleanupDeletesBeforeAnyReadAndBeforeTheMarker() {
        let keychain = RecordingKeychain()
        let defaults = suite()
        var markerSeenAtDelete: Bool?
        keychain.beforeDelete { [self] in markerSeenAtDelete = cleared(defaults) }
        _ = prepare(keychain, defaults)
        XCTAssertEqual(keychain.events, ["delete", "load"], "the probe read came before the delete")
        XCTAssertEqual(markerSeenAtDelete, false, "the marker was already set when the delete ran")
    }

    /// A probe that failed for a reason other than the lock (disk full, directory can't be made,
    /// probe file malformed) is not "locked": the screen must not tell the user to unlock a
    /// phone that is unlocked. It stays off, silently, as before step 6, and touches nothing.
    func testAProbeThatFailedButIsNotLockedStaysOffWithoutTouchingAnything() {
        let keychain = RecordingKeychain()
        let defaults = suite()
        let result = prepare(keychain, defaults, probe: .failed)
        XCTAssertTrue(isOff(result), "expected .off, got \(result)")
        XCTAssertEqual(keychain.events, [], "a failed probe touched the Keychain")
        XCTAssertFalse(cleared(defaults), "a failed probe set the marker")
    }

    /// The real check: a fresh directory counts as available and gets its probe file.
    func testTheFileProbeSaysAvailableWhenReadable() throws {
        XCTAssertEqual(SessionPersistence.protectedData(in: dataDir), .available)
        let probe = dataDir.appending(path: "protected-data-probe")
        XCTAssertTrue(FileManager.default.fileExists(atPath: probe.path))
        XCTAssertEqual(SessionPersistence.protectedData(in: dataDir), .available, "second call reads it back")
    }

    /// Failures that are not the lock: a probe that exists but can't be read (a directory stands
    /// in for a malformed one), and a data directory that can't be made (its parent is a file).
    func testTheFileProbeSaysFailedForOtherFailures() throws {
        _ = SessionPersistence.protectedData(in: dataDir)
        let probe = dataDir.appending(path: "protected-data-probe")
        try FileManager.default.removeItem(at: probe)
        try FileManager.default.createDirectory(at: probe, withIntermediateDirectories: false)
        XCTAssertEqual(SessionPersistence.protectedData(in: dataDir), .failed, "an unreadable probe is not a lock")

        let file = tmp.appending(path: "a-file")
        try Data("x".utf8).write(to: file)
        XCTAssertEqual(
            SessionPersistence.protectedData(in: file.appending(path: "Brook", directoryHint: .isDirectory)), .failed,
            "a directory that can't be made is not a lock")
    }

    /// Only a permission-style refusal is the protection class saying "not yet": EPERM or EACCES,
    /// bare or inside a Cocoa error, or the Cocoa no-permission codes. A device that is really
    /// locked can't be simulated here, so this pins the rule the probe applies to its errors.
    func testOnlyAPermissionRefusalCountsAsLocked() {
        func posix(_ code: Int32) -> NSError { NSError(domain: NSPOSIXErrorDomain, code: Int(code)) }
        func cocoa(_ code: Int, underlying: NSError? = nil) -> NSError {
            NSError(domain: NSCocoaErrorDomain, code: code,
                    userInfo: underlying.map { [NSUnderlyingErrorKey: $0] } ?? [:])
        }
        let locked: [NSError] = [
            posix(EPERM), posix(EACCES),
            cocoa(NSFileReadNoPermissionError), cocoa(NSFileWriteNoPermissionError),
            cocoa(NSFileReadUnknownError, underlying: posix(EPERM)),
            cocoa(NSFileWriteUnknownError, underlying: posix(EACCES)),
        ]
        for error in locked { XCTAssertTrue(SessionPersistence.isProtectionRefusal(error), "\(error)") }
        let other: [NSError] = [
            posix(ENOSPC), posix(EISDIR), posix(ENOENT),
            cocoa(NSFileWriteOutOfSpaceError), cocoa(NSFileReadCorruptFileError),
            cocoa(NSFileReadUnknownError, underlying: posix(EISDIR)),
        ]
        for error in other { XCTAssertFalse(SessionPersistence.isProtectionRefusal(error), "\(error)") }
    }
}
