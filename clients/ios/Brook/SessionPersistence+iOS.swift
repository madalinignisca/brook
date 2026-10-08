// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import Foundation

/// The iOS policy for `SessionPersistence`: the app's own Keychain group and its own data
/// directory in the app container. There is no instance lock (iOS runs one copy of an app),
/// so `.secondInstance` never happens here. The Mac's policy is in `SessionPersistence+Mac.swift`.
extension SessionPersistence {
    /// Set once the first-launch Keychain cleanup has succeeded. Its own key rather than "the
    /// defaults are empty": a developer may `defaults write ... AllowInsecureHTTP` into the
    /// simulator before the first launch, and that must not skip the cleanup.
    static let keychainClearedKey = "KeychainCleared"

    /// The live choice for this process, made once in `BrookApp.init`, before `SessionStore`
    /// exists, so it runs before anything can restore a session.
    static func live() -> SessionPersistence {
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appending(path: "Brook", directoryHint: .isDirectory)
        return prepare(dataDir: dir, calls: SystemSecItem(), defaults: .standard)
    }

    /// Why `.off` here: the first-launch cleanup failed, the data directory can't be made, or
    /// the Keychain is unusable in this build. Nothing is stored then; quitting signs out.
    static func prepare(dataDir: URL, calls: SecItemCalls, defaults: UserDefaults) -> SessionPersistence {
        // The access group is nil: the app's default group, which is its own and needs no
        // entitlement (an iOS Keychain item is visible only to the app that wrote it).
        let slot = KeychainSlot(accessGroup: nil, calls: calls)

        // 1. Deleting an iOS app removes its files and defaults but NOT its Keychain items, so a
        // reinstall would otherwise restore the previous install's sign-in. The first launch
        // after an install has no marker, so it deletes every item under Brook's service. The
        // marker is set only after the delete worked: on failure the next launch tries again,
        // and this launch stores nothing, so it can never restore an old session.
        if defaults.object(forKey: keychainClearedKey) == nil {
            do { try slot.deleteAll() } catch { return .off }
            defaults.set(true, forKey: keychainClearedKey)
        }

        // 2. Core keeps its sign-out fences here.
        guard (try? FileManager.default.createDirectory(at: dataDir, withIntermediateDirectories: true)) != nil
        else { return .off }

        // 3. Probe with a slot core never uses. A fault (-34018: the Keychain refuses this
        // build) means it will never work, so stay off. A locked Keychain is not a fault:
        // core reports it at restore and deletes nothing.
        do { _ = try slot.load(slot: "probe") } catch FfiKeySlotError.Fatal { return .off } catch {}

        return .on(slot: slot, dataDir: dataDir.path(percentEncoded: false))
    }
}
