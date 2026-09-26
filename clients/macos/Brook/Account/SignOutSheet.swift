import BrookCore
import SwiftUI

/// Sign out, when this Mac holds local data (#62 spec item 6, as GTK's sheet): "Remove this
/// device's data" ticked by default (#46 §8), the text following the box, and a warning about
/// unsent messages while it's ticked.
@MainActor
@Observable
final class SignOutModel {
    var removeData = true
    /// Unsent messages; nil when this user's stores aren't known to be open, so a count
    /// (which reads 0 while they're closed) can't be trusted.
    private(set) var unsent: UInt64?

    private let client: any OfflineClient

    init(client: any OfflineClient) {
        self.client = client
    }

    /// A cheap cached call first: its success proves the stores are open.
    func probe() async {
        if (try? await client.cachedChannels()) != nil {
            unsent = await client.unsentCount()
        } else {
            unsent = nil
        }
    }

    var body: String {
        removeData
            ? "Your messages and files saved on this Mac will be removed."
            : "Your messages stay saved on this Mac for the next time you sign in."
    }

    /// Only while removing: the count, or "may" when it can't be trusted.
    var warning: String? {
        guard removeData else { return nil }
        guard let unsent else { return "Unsent messages on this Mac may be deleted." }
        switch unsent {
        case 0: return nil
        case 1: return "1 message hasn't been sent yet. Removing this device's data deletes it."
        default: return "\(unsent) messages haven't been sent yet. Removing this device's data deletes them."
        }
    }
}

struct SignOutSheet: View {
    @State var model: SignOutModel
    let signOut: (_ removeData: Bool) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Sign out of Brook?").font(.headline)
            Toggle("Remove this device's data", isOn: $model.removeData)
            Text(model.body).foregroundStyle(.secondary)
            if let warning = model.warning {
                Label(warning, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
            }
            HStack {
                Spacer()
                Button("Cancel") { dismiss() }
                    .keyboardShortcut(.defaultAction) // the safe choice is the default
                Button("Sign Out", role: .destructive) {
                    dismiss()
                    signOut(model.removeData)
                }
            }
        }
        .padding(20)
        .frame(width: 380)
        .task { await model.probe() }
    }
}
