// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

/// The few facts about the device that the shared code puts into text or defaults. The iOS
/// app's own `ThisDevice`; the Mac defines its own in `clients/macos/Brook/ThisDevice.swift`,
/// so the shared code names the type and never the platform.
enum ThisDevice {
    /// What the device is called in "This iPhone couldn't forget...".
    static let name = "iPhone"
    /// In "If your iPhone asked to allow local network access...". Includes "your" because
    /// iOS names the device, not the operating system, in that prompt.
    static let system = "your iPhone"
    /// Empty: on a first launch the server field is blank and the view shows the placeholder
    /// `https://chat.example.com`. A made-up default would only send people to the wrong host.
    static let fallbackServer = ""
}
