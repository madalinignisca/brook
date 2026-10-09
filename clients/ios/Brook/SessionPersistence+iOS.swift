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
            protectedData: { protectedData(in: dir) })
    }

    /// What the probe found.
    enum ProtectedData: Equatable {
        /// Files of the "after first unlock" class can be read.
        case available
        /// The protection class refused: the device has not been unlocked since boot.
        case locked
        /// Something else went wrong (disk full, directory can't be made, probe file malformed).
        /// This says nothing about the lock, so it must not tell the user to unlock the phone.
        case failed
    }

    /// Whether files that unlock at the first unlock after a reboot (the class the defaults and
    /// the Keychain item use) can be read yet. This is a file probe and not
    /// `UIApplication.shared.isProtectedDataAvailable`: `UIApplication.shared` is still nil
    /// while `App.init` runs (measured: it answers false there, always), so that would keep the
    /// app off at every launch.
    /// The probe file is written once with that protection class; later it can be read only
    /// when the device has been unlocked since boot. Only a permission-style error from the
    /// protection class (`isProtectionRefusal`) means `.locked`; any other error is `.failed`,
    /// which `prepare` treats as plain `.off`.
    static func protectedData(in dir: URL) -> ProtectedData {
        let fm = FileManager.default
        let probe = dir.appending(path: "protected-data-probe")
        do {
            try fm.createDirectory(at: dir, withIntermediateDirectories: true)
            if fm.fileExists(atPath: probe.path(percentEncoded: false)) {
                _ = try Data(contentsOf: probe)
            } else {
                // The protection class below is what makes this probe work. "Until first user
                // authentication" is the class of the Keychain item and the defaults file: readable from
                // the first unlock after a boot, not before. So a refused write here means the phone has
                // not been unlocked since boot. Do not weaken it (`.none`, `.completeFileProtectionUnlessOpen`):
                // the probe would then be readable before the first unlock, say "available", and reopen
                // the pre-unlock hole: the cleanup would run against a Keychain it cannot see and set its
                // marker. Do not strengthen it to `.complete` either: the probe would then say "locked"
                // whenever the screen is locked. The simulator cannot catch a wrong choice (it does not
                // enforce file protection); only a real device, restarted and left locked, can.
                try Data("x".utf8).write(to: probe, options: .completeFileProtectionUntilFirstUserAuthentication)
            }
            return .available
        } catch {
            return isProtectionRefusal(error) ? .locked : .failed
        }
    }

    /// Whether an error is the protection class saying "not yet" rather than some other I/O
    /// failure: EPERM or EACCES, bare or as the underlying error of a Cocoa error, or the Cocoa
    /// no-permission codes. (Which of these a locked device actually returns is not measured
    /// here; the simulator does not enforce file protection. All of them are permission errors,
    /// and none of the failures that must stay silent -- ENOSPC, EISDIR, a corrupt file -- is.)
    static func isProtectionRefusal(_ error: Error) -> Bool {
        let e = error as NSError
        switch (e.domain, e.code) {
        case (NSPOSIXErrorDomain, Int(EPERM)), (NSPOSIXErrorDomain, Int(EACCES)),
             (NSCocoaErrorDomain, NSFileReadNoPermissionError), (NSCocoaErrorDomain, NSFileWriteNoPermissionError):
            return true
        default:
            guard let underlying = e.userInfo[NSUnderlyingErrorKey] as? Error else { return false }
            return isProtectionRefusal(underlying)
        }
    }

    /// Why `.off` here: the first-launch cleanup failed, the data directory can't be made, the
    /// probe failed for a reason other than the lock, or the Keychain is unusable in this build. A launch before the first unlock after a reboot
    /// is `.lockedUntilFirstUnlock` instead (also nothing stored, but the sign-in screen says
    /// why). Nothing is stored then; quitting signs out.
    ///
    /// `defaultsDomain` is the name of the persistent domain `defaults` writes to (the bundle
    /// identifier for `.standard`). `protectedData` is injected so a test can fake a
    /// locked device.
    static func prepare(
        dataDir: URL, calls: SecItemCalls, defaults: UserDefaults, defaultsDomain: String,
        protectedData: () -> ProtectedData
    ) -> SessionPersistence {
        // 0. Before the first unlock after a reboot, files and Keychain items of class "after
        // first unlock" can't be read. The defaults file then reads as empty (no marker) and the
        // Keychain delete finds nothing (`errSecItemNotFound` counts as success), so the cleanup
        // would set the marker while the old install's token is still there. So wait for an
        // unlocked launch, and store nothing meanwhile. This runs before the marker is read.
        // Not plain `.off`: the sign-in screen then tells the user to unlock the phone once.
        // A probe that failed for another reason is plain `.off`: same safety (nothing is read,
        // deleted or marked), but no message, because the phone is not known to be locked.
        switch protectedData() {
        case .available: break
        case .locked: return .lockedUntilFirstUnlock
        case .failed: return .off
        }

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
