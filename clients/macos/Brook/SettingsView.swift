// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import SwiftUI

/// The Settings window (⌘,).
struct SettingsView: View {
    @AppStorage(Settings.showUsernamesKey) private var showUsernames = false
    @AppStorage(Settings.showImagePreviewsKey) private var showImagePreviews = Settings.showImagePreviewsDefault
    @AppStorage(Settings.ringForCallsKey) private var ringForCalls = true

    static let showUsernamesCaption =
        "Shows people as @username instead of their display name, everywhere they appear."

    var body: some View {
        Form {
            Toggle("Show usernames", isOn: $showUsernames)
            Text(Self.showUsernamesCaption)
                .font(.caption).foregroundStyle(.secondary)
            Toggle("Show image previews", isOn: $showImagePreviews)
            Toggle("Ring for incoming calls", isOn: $ringForCalls)
            Text("Small images in conversations show by themselves. When off, an image loads only when you click Show preview.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .formStyle(.grouped)
        .frame(width: 420)
        .fixedSize(horizontal: false, vertical: true)
    }
}
