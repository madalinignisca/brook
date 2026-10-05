import SwiftUI

/// The Settings window (⌘,).
struct SettingsView: View {
    @AppStorage(Settings.showUsernamesKey) private var showUsernames = false
    @AppStorage(Settings.showImagePreviewsKey) private var showImagePreviews = Settings.showImagePreviewsDefault

    var body: some View {
        Form {
            Toggle("Show usernames", isOn: $showUsernames)
            Text("Shows people as @username in the sidebar and window title. Message headers and member lists keep display names.")
                .font(.caption).foregroundStyle(.secondary)
            Toggle("Show image previews", isOn: $showImagePreviews)
            Text("Small images in conversations show by themselves. When off, an image loads only when you click Show preview.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .formStyle(.grouped)
        .frame(width: 420)
        .fixedSize(horizontal: false, vertical: true)
    }
}
