// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import Darwin
import Foundation
import Security

/// The Mac's policy for `SessionPersistence`: a signed keychain group and one instance per
/// user. iOS has its own (`SessionPersistence+iOS.swift`).
extension SessionPersistence {
    static let groupSuffix = ".dev.brook.shared"

    /// The live choice for this process. The lock, once taken, is held until the process exits.
    static func live() -> SessionPersistence {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appending(path: "Brook", directoryHint: .isDirectory)
        return choose(accessGroup: signedAccessGroup(), dataDir: dir, calls: SystemSecItem(), lock: InstanceLock.acquire)
    }

    /// The group is read from the signature rather than configured, so a build without the
    /// entitlement can't be told to use one it doesn't have.
    static func choose(
        accessGroup: String?, dataDir: URL, calls: SecItemCalls, lock: (URL) -> Bool
    ) -> SessionPersistence {
        // `.off` here means the Mac can't keep a session: no keychain group in this build's
        // signature (no provisioning profile yet, #79), or the keychain refuses the group, or
        // the data directory can't be made. Nothing is stored; quitting signs out.
        guard let accessGroup else { return .off }
        guard (try? FileManager.default.createDirectory(at: dataDir, withIntermediateDirectories: true)) != nil
        else { return .off }
        guard lock(dataDir.appending(path: "instance.lock")) else { return .secondInstance }
        let slot = KeychainSlot(accessGroup: accessGroup, calls: calls)
        // Probe with a slot core never uses: a fault here (a missing or mismatched
        // entitlement, -34018) means the keychain will never work in this build. A locked
        // keychain is not a fault: core reports it at restore and deletes nothing.
        do { _ = try slot.load(slot: "probe") } catch FfiKeySlotError.Fatal { return .off } catch {}
        return .on(slot: slot, dataDir: dataDir.path(percentEncoded: false))
    }

    /// `keychain-access-groups` from this process's own signature: the one ending in
    /// `.dev.brook.shared` (`$(AppIdentifierPrefix)` expanded at signing).
    static func signedAccessGroup() -> String? {
        guard let task = SecTaskCreateFromSelf(nil),
              let value = SecTaskCopyValueForEntitlement(task, "keychain-access-groups" as CFString, nil),
              let groups = value as? [String]
        else { return nil }
        return groups.first { $0.hasSuffix(groupSuffix) }
    }
}

/// One Brook per user touches the stored session: an exclusive `flock`, never released
/// (the kernel drops it when the process exits, crash included).
enum InstanceLock {
    /// Returns whether this process now holds the lock at `url`. The descriptor is left open
    /// on purpose: closing it would release the lock.
    static func acquire(_ url: URL) -> Bool {
        let fd = open(url.path(percentEncoded: false), O_RDWR | O_CREAT | O_CLOEXEC, 0o600)
        guard fd >= 0 else { return false }
        guard flock(fd, LOCK_EX | LOCK_NB) == 0 else {
            close(fd)
            return false
        }
        return true
    }
}
