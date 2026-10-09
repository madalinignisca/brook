// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI

/// The app: the root follows `store.phase`, as on the Mac.
@main
struct BrookApp: App {
    @State private var store: SessionStore
    @State private var form: LoginForm

    init() {
        // `makeFeed: nil`: the iOS app keeps no local data (no cache, no offline outbox); only
        // the session is stored. `.live()` also runs the first-launch Keychain cleanup, before
        // the store exists and so before anything can restore.
        let store = SessionStore(persistence: .live(), makeFeed: nil)
        _store = State(initialValue: store)
        _form = State(initialValue: LoginForm(store: store))
    }

    var body: some Scene {
        WindowGroup {
            Group {
                switch store.phase {
                case let .signedIn(user):
                    // `store.client` is set whenever the phase is signed in.
                    if let client = store.client {
                        SignedInHome(store: store, client: client, user: user)
                    }
                case .restoring:
                    ProgressView("Signing in…")
                case .signedOut, .signingIn, .needsCode:
                    LoginView(form: form)
                }
            }
            // Once per process; later appearances no-op. Restores the stored session.
            .task { await store.restoreAtLaunch() }
        }
    }
}

/// Holds the signed-in session for as long as this view lives, so a sign-out (or a remote one)
/// drops it and the next sign-in starts from a fresh one.
private struct SignedInHome: View {
    let store: SessionStore
    let client: FfiBrookClient
    let user: FfiUser
    /// Built once, in `.task`. Building it in `init` would run on every re-render of the parent
    /// (a `State(initialValue:)` argument is evaluated each time and all but the first dropped).
    /// While it is nil the body must still show a real view (the `ProgressView`): `.task` is
    /// applied to the view's children, and a `Group` with no child has none. On a real iPhone
    /// (iOS 26.6.1) the task then never ran and the screen stayed black after sign-in. The iOS
    /// simulators do run it, so no unit test catches this (a hosted-view test passed with and
    /// without the placeholder); do not remove the `else` on the strength of a green test run.
    @State private var session: SignedInSession?

    var body: some View {
        Group {
            if let session {
                ChannelListView(store: store, session: session)
            } else {
                ProgressView()
            }
        }
        .task { if session == nil { session = SignedInSession(client: client, me: user.id) } }
    }
}
