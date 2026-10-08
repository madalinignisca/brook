// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

/// The few facts about the device that the shared code puts into text or defaults. Each app
/// defines its own `ThisDevice` in its own file (the shared code names the type, not the
/// platform), so there is no `#if os(...)` in shared files. This is the Mac's; the iOS app
/// defines its own in `clients/ios/Brook/ThisDevice.swift`.
enum ThisDevice {
    /// What the device is called in "This Mac couldn't forget...".
    static let name = "Mac"
    /// The operating system, in "If macOS asked to allow local network access...".
    static let system = "macOS"
    /// What the server field holds on a first launch, before any server was remembered.
    static let fallbackServer = "https://localhost"
}
