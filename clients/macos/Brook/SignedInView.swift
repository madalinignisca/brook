import BrookCore
import SwiftUI

struct SignedInView: View {
    let user: FfiUser

    var body: some View {
        ContentUnavailableView {
            Label("Signed in as \(user.displayName)", systemImage: "person.crop.circle.badge.checkmark")
        } description: {
            Text("Chat, calls and files will live here.")
        }
    }
}
