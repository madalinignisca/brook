import Foundation
import Security
import XCTest

@testable import BrookCore

/// The Keychain calls, faked: the unsigned test host can't reach the data-protection keychain.
final class FakeSecItem: SecItemCalls, @unchecked Sendable {
    // Tests call the slot from one thread; the lock only satisfies `Sendable`.
    private let lock = NSLock()
    private var items: [String: Data] = [:]
    private var queries: [[String: Any]] = []
    private var updates: [[String: Any]] = []
    var lastUpdate: [String: Any]? { lock.withLock { updates.last } }
    var failWith: OSStatus?

    var lastQuery: [String: Any]? { lock.withLock { queries.last } }
    private func record(_ q: [String: Any]) { lock.withLock { queries.append(q) } }
    private func key(_ q: [String: Any]) -> String { q[kSecAttrAccount as String] as? String ?? "" }

    func add(_ query: [String: Any]) -> OSStatus {
        record(query)
        if let failWith { return failWith }
        return lock.withLock {
            if items[key(query)] != nil { return errSecDuplicateItem }
            items[key(query)] = query[kSecValueData as String] as? Data
            return errSecSuccess
        }
    }
    func copyMatching(_ query: [String: Any]) -> (OSStatus, Data?) {
        record(query)
        if let failWith { return (failWith, nil) }
        let data = lock.withLock { items[key(query)] }
        return data.map { (errSecSuccess, $0) } ?? (errSecItemNotFound, nil)
    }
    func update(_ query: [String: Any], _ attributes: [String: Any]) -> OSStatus {
        record(query)
        lock.withLock { updates.append(attributes) }
        if let failWith { return failWith }
        return lock.withLock {
            guard items[key(query)] != nil else { return errSecItemNotFound }
            items[key(query)] = attributes[kSecValueData as String] as? Data
            return errSecSuccess
        }
    }
    func delete(_ query: [String: Any]) -> OSStatus {
        record(query)
        if let failWith { return failWith }
        return lock.withLock { items.removeValue(forKey: key(query)) } != nil ? errSecSuccess : errSecItemNotFound
    }
}

final class KeychainSlotTests: XCTestCase {
    func testTheItemIsDataProtectedThisDeviceOnlyAndNeverSynced() throws {
        let calls = FakeSecItem()
        let slot = KeychainSlot(accessGroup: "ABCDE12345.dev.brook.shared", calls: calls)
        try slot.create(slot: "session:x", bytes: Data([1, 2, 3]))
        let q = try XCTUnwrap(calls.lastQuery)
        XCTAssertEqual(q[kSecClass as String] as? String, kSecClassGenericPassword as String)
        XCTAssertEqual(q[kSecAttrService as String] as? String, "dev.brook.Brook.datakey")
        XCTAssertEqual(q[kSecAttrAccount as String] as? String, "session:x")
        XCTAssertEqual(q[kSecUseDataProtectionKeychain as String] as? Bool, true)
        XCTAssertEqual(q[kSecAttrSynchronizable as String] as? Bool, false)
        XCTAssertEqual(q[kSecAttrAccessible as String] as? String,
                       kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly as String)
        XCTAssertEqual(q[kSecAttrAccessGroup as String] as? String, "ABCDE12345.dev.brook.shared")
        // Queries, not just the add, say "never synced".
        _ = try slot.load(slot: "session:x")
        XCTAssertEqual(calls.lastQuery?[kSecAttrSynchronizable as String] as? Bool, false)
    }

    func testAReplaceSetsTheProtectionAgain() throws {
        let calls = FakeSecItem()
        let slot = KeychainSlot(accessGroup: nil, calls: calls)
        try slot.create(slot: "a", bytes: Data([1]))
        try slot.replace(slot: "a", bytes: Data([2]))
        XCTAssertEqual(calls.lastUpdate?[kSecAttrAccessible as String] as? String,
                       kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly as String)
    }

    func testCreateIsCreateOnlyAndReplaceOverwritesOrCreates() throws {
        let slot = KeychainSlot(accessGroup: nil, calls: FakeSecItem())
        try slot.create(slot: "a", bytes: Data([1]))
        XCTAssertThrowsError(try slot.create(slot: "a", bytes: Data([2]))) {
            XCTAssertEqual($0 as? FfiKeySlotError, .Exists)
        }
        XCTAssertEqual(try slot.load(slot: "a"), Data([1]), "create overwrote")
        try slot.replace(slot: "a", bytes: Data([3]))
        XCTAssertEqual(try slot.load(slot: "a"), Data([3]))
        try slot.replace(slot: "b", bytes: Data([4]))  // absent: created
        XCTAssertEqual(try slot.load(slot: "b"), Data([4]))
        try slot.delete(slot: "b")
        try slot.delete(slot: "b")  // absent: fine
        XCTAssertNil(try slot.load(slot: "b"))
    }

    /// Only "not found" means absent. Locked means try later; anything else is a fault. Neither
    /// is ever read as "absent" (which would make a new key or skip a restore wrongly).
    func testStatusesMapToWhatTheyMean() {
        for (status, expected): (OSStatus, FfiKeySlotError) in [
            (errSecInteractionNotAllowed, .Unavailable),
            (errSecAuthFailed, .Unavailable),
            (errSecMissingEntitlement, .Fatal(status: errSecMissingEntitlement)),
            (errSecParam, .Fatal(status: errSecParam)),
        ] {
            let calls = FakeSecItem()
            calls.failWith = status
            let slot = KeychainSlot(accessGroup: nil, calls: calls)
            XCTAssertThrowsError(try slot.load(slot: "a"), "\(status)") {
                XCTAssertEqual($0 as? FfiKeySlotError, expected, "\(status)")
            }
        }
        let slot = KeychainSlot(accessGroup: nil, calls: FakeSecItem())
        XCTAssertNil(try slot.load(slot: "missing"), "not found must be absent, not an error")
    }
}
