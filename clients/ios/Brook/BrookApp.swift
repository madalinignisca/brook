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

/// Owns the channel list's model for one signed-in session. It is created with the view and
/// dropped with it, so a sign-out (or a remote one) and the next sign-in start from a fresh model.
private struct SignedInHome: View {
    let store: SessionStore
    @State private var channels: ChannelsModel

    init(store: SessionStore, client: FfiBrookClient, user: FfiUser) {
        self.store = store
        // No notifier: iOS notifications are later work. `AppActivity` is iOS's default.
        _channels = State(initialValue: ChannelsModel(client: client, me: user.id))
    }

    var body: some View {
        ChannelListView(store: store, channels: channels)
    }
}
