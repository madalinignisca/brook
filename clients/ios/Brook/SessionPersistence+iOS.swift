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
        // The data directory holds only core's sign-out fences (no secret), and it stays in the
        // device backup ON PURPOSE. The Keychain item is ThisDeviceOnly, but an encrypted backup
        // restored to the same device brings it back. If the fences were left out of that backup,
        // the restored item would be a signed-in session whose sign-out fence is gone, so a
        // session the user had signed out of would come back. Backed up together they stay
        // consistent. Do not add `isExcludedFromBackup` here.
        let dir = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appending(path: "Brook", directoryHint: .isDirectory)
        return prepare(
            dataDir: dir, calls: SystemSecItem(), defaults: .standard,
            defaultsDomain: Bundle.main.bundleIdentifier ?? "",
            protectedDataAvailable: { protectedDataAvailable(in: dir) })
    }

    /// Whether files that unlock at the first unlock after a reboot (the class the defaults and
    /// the Keychain item use) can be read yet. This is a file probe and not
    /// `UIApplication.shared.isProtectedDataAvailable`: `UIApplication.shared` is still nil
    /// while `App.init` runs (measured: it answers false there, always), so that would keep the
    /// app off at every launch.
    /// The probe file is written once with that protection class; later it can be read only
    /// when the device has been unlocked since boot. A failed first write also means locked.
    static func protectedDataAvailable(in dir: URL) -> Bool {
        let fm = FileManager.default
        guard (try? fm.createDirectory(at: dir, withIntermediateDirectories: true)) != nil else { return false }
        let probe = dir.appending(path: "protected-data-probe")
        if fm.fileExists(atPath: probe.path(percentEncoded: false)) {
            return (try? Data(contentsOf: probe)) != nil
        }
        // The protection class below is what makes this probe work. "Until first user
        // authentication" is the class of the Keychain item and the defaults file: readable from
        // the first unlock after a boot, not before. So a failed write here means the phone has
        // not been unlocked since boot. Do not weaken it (`.none`, `.completeFileProtectionUnlessOpen`):
        // the probe would then be readable before the first unlock, say "available", and reopen
        // the pre-unlock hole: the cleanup would run against a Keychain it cannot see and set its
        // marker. Do not strengthen it to `.complete` either: the probe would then say "locked"
        // whenever the screen is locked. The simulator cannot catch a wrong choice (it does not
        // enforce file protection); only a real device, restarted and left locked, can.
        return (try? Data("x".utf8).write(to: probe, options: .completeFileProtectionUntilFirstUserAuthentication)) != nil
    }

    /// Why `.off` here: the first-launch cleanup failed, the data directory can't be made, or
    /// the Keychain is unusable in this build. A launch before the first unlock after a reboot
    /// is `.lockedUntilFirstUnlock` instead (also nothing stored, but the sign-in screen says
    /// why). Nothing is stored then; quitting signs out.
    ///
    /// `defaultsDomain` is the name of the persistent domain `defaults` writes to (the bundle
    /// identifier for `.standard`). `protectedDataAvailable` is injected so a test can fake a
    /// locked device.
    static func prepare(
        dataDir: URL, calls: SecItemCalls, defaults: UserDefaults, defaultsDomain: String,
        protectedDataAvailable: () -> Bool
    ) -> SessionPersistence {
        // 0. Before the first unlock after a reboot, files and Keychain items of class "after
        // first unlock" can't be read. The defaults file then reads as empty (no marker) and the
        // Keychain delete finds nothing (`errSecItemNotFound` counts as success), so the cleanup
        // would set the marker while the old install's token is still there. So wait for an
        // unlocked launch, and store nothing meanwhile. This runs before the marker is read.
        // Not plain `.off`: the sign-in screen then tells the user to unlock the phone once.
        guard protectedDataAvailable() else { return .lockedUntilFirstUnlock }

        // The access group is nil: the app's default group, which is its own and needs no
        // entitlement (an iOS Keychain item is visible only to the app that wrote it).
        let slot = KeychainSlot(accessGroup: nil, calls: calls)

        // 1. Deleting an iOS app removes its files and defaults but NOT its Keychain items, so a
        // reinstall would otherwise restore the previous install's sign-in. The first launch
        // after an install has no marker, so it deletes every item under Brook's service. The
        // marker is set only after the delete worked: on failure the next launch tries again,
        // and this launch stores nothing, so it can never restore an old session.
        // The marker is read from the persistent domain only: `defaults.object(forKey:)` also
        // searches the launch-argument domain, so `-KeychainCleared YES` would skip the cleanup.
        if defaults.persistentDomain(forName: defaultsDomain)?[keychainClearedKey] == nil {
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
