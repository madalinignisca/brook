import SwiftUI

/// The Settings window (⌘,).
struct SettingsView: View {
    @AppStorage(Settings.showUsernamesKey) private var showUsernames = false

    var body: some View {
        Form {
            Toggle("Show usernames", isOn: $showUsernames)
            Text("Shows people as @username in the sidebar and window title. Message headers and member lists keep display names.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .formStyle(.grouped)
        .frame(width: 420)
        .fixedSize(horizontal: false, vertical: true)
    }
}
