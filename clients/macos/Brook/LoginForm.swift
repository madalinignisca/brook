import Observation

/// What the login form shows and edits. Holds the password only while the user is typing
/// it: once an attempt has actually been made (successful or rejected by the server), the
/// field is cleared so the password does not linger in memory or on screen. A local
/// validation error (e.g. empty handle) keeps it, so the user is not made to retype it.
@MainActor
@Observable
final class LoginForm {
    var server: String
    var handle = ""
    var password = ""
    let store: SessionStore

    init(store: SessionStore) {
        self.store = store
        server = store.settings.serverPrefill
    }

    var isBusy: Bool { store.phase == .signingIn }

    var error: String? {
        if case let .signedOut(error) = store.phase { return error }
        return nil
    }

    var insecureWarning: String? {
        store.settings.allowInsecureHTTP
            ? "Insecure connections allowed — your password is sent unencrypted." : nil
    }

    func submit() async {
        if await store.signIn(server: server, handle: handle, password: password) {
            password = ""
        }
    }
}
