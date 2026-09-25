import Foundation
import Security

/// The Keychain calls `KeychainSlot` makes, so tests can fake them (an unsigned test host can't
/// reach the data-protection keychain).
public protocol SecItemCalls: Sendable {
    func add(_ query: [String: Any]) -> OSStatus
    func copyMatching(_ query: [String: Any]) -> (OSStatus, Data?)
    func update(_ query: [String: Any], _ attributes: [String: Any]) -> OSStatus
    func delete(_ query: [String: Any]) -> OSStatus
}

public struct SystemSecItem: SecItemCalls {
    public init() {}
    public func add(_ query: [String: Any]) -> OSStatus { SecItemAdd(query as CFDictionary, nil) }
    public func copyMatching(_ query: [String: Any]) -> (OSStatus, Data?) {
        var out: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &out)
        return (status, out as? Data)
    }
    public func update(_ query: [String: Any], _ attributes: [String: Any]) -> OSStatus {
        SecItemUpdate(query as CFDictionary, attributes as CFDictionary)
    }
    public func delete(_ query: [String: Any]) -> OSStatus { SecItemDelete(query as CFDictionary) }
}

/// Core's key slots in the **data-protection** Keychain (spec #46 §4): no prompts, this device
/// only, never synced, readable after the first unlock. One generic-password item per slot.
public final class KeychainSlot: FfiKeySlot, Sendable {
    private let accessGroup: String?
    private let calls: SecItemCalls

    /// `accessGroup`: `$(AppIdentifierPrefix)dev.brook.shared` from the signed app (nil only in
    /// tests and unsigned builds).
    public init(accessGroup: String?, calls: SecItemCalls = SystemSecItem()) {
        self.accessGroup = accessGroup
        self.calls = calls
    }

    private func query(_ slot: String) -> [String: Any] {
        var q: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: "dev.brook.Brook.datakey",
            kSecAttrAccount as String: slot,
            kSecUseDataProtectionKeychain as String: true,
            kSecAttrSynchronizable as String: false,
        ]
        if let accessGroup { q[kSecAttrAccessGroup as String] = accessGroup }
        return q
    }

    /// Only "not found" is absent; locked is try-later; anything else is a fault (a status code
    /// only, never text).
    private static func failure(_ status: OSStatus) -> FfiKeySlotError {
        switch status {
        case errSecInteractionNotAllowed, errSecAuthFailed: .Unavailable
        default: .Fatal(status: status)
        }
    }

    public func load(slot: String) throws -> Data? {
        var q = query(slot)
        q[kSecReturnData as String] = true
        q[kSecMatchLimit as String] = kSecMatchLimitOne
        let (status, data) = calls.copyMatching(q)
        switch status {
        case errSecSuccess: return data
        case errSecItemNotFound: return nil
        default: throw Self.failure(status)
        }
    }

    public func create(slot: String, bytes: Data) throws {
        var q = query(slot)
        q[kSecValueData as String] = bytes
        q[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        switch calls.add(q) {
        case errSecSuccess: return
        case errSecDuplicateItem: throw FfiKeySlotError.Exists
        case let status: throw Self.failure(status)
        }
    }

    /// Atomic overwrite (one `SecItemUpdate`), or create when absent. The protection is set
    /// again with the data: an item that somehow had a weaker one never gets fresh secrets
    /// under it.
    public func replace(slot: String, bytes: Data) throws {
        let attributes: [String: Any] = [
            kSecValueData as String: bytes,
            kSecAttrAccessible as String: kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly,
        ]
        switch calls.update(query(slot), attributes) {
        case errSecSuccess: return
        case errSecItemNotFound:
            do { try create(slot: slot, bytes: bytes) } catch FfiKeySlotError.Exists {
                try replace(slot: slot, bytes: bytes) // created meanwhile: overwrite it
            }
        case let status: throw Self.failure(status)
        }
    }

    public func delete(slot: String) throws {
        switch calls.delete(query(slot)) {
        case errSecSuccess, errSecItemNotFound: return
        case let status: throw Self.failure(status)
        }
    }
}
