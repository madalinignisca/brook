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
    /// The code step (TOTP): a 6-digit code, or a recovery code when `useRecovery`.
    var code = ""
    var useRecovery = false
    let store: SessionStore

    init(store: SessionStore) {
        self.store = store
        server = store.settings.serverPrefill
    }

    var isBusy: Bool { store.phase == .signingIn || store.codeBusy }

    var needsCode: Bool {
        if case .needsCode = store.phase { return true }
        return false
    }

    var error: String? {
        switch store.phase {
        case let .signedOut(error), let .needsCode(error): error
        default: nil
        }
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

    /// Send the code (or recovery code); the field is cleared after each attempt.
    func submitCode() async {
        let entered = code
        code = ""
        if useRecovery {
            await store.submitRecovery(entered)
        } else {
            await store.submitCode(entered)
        }
    }

    func back() {
        code = ""
        useRecovery = false
        store.back()
    }
}
