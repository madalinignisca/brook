import SwiftUI

@main
struct BrookApp: App {
    @State private var store: SessionStore
    @State private var form: LoginForm

    init() {
        let store = SessionStore()
        _store = State(initialValue: store)
        _form = State(initialValue: LoginForm(store: store))
    }

    var body: some Scene {
        Window("Brook", id: "main") {
            Group {
                switch store.phase {
                case let .signedIn(user): SignedInView(user: user)
                case .signedOut, .signingIn: LoginView(form: form)
                }
            }
            .frame(minWidth: 380, minHeight: 480)
        }
        .defaultSize(width: 420, height: 560)
    }
}
