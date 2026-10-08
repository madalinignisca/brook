// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI

/// Placeholder until the sign-in and channel list arrive (#272, steps 6 and 7). It builds what
/// they will need: the store, with session persistence on and local data off, and the login form.
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
            // A call into Rust made by the app (not only by the test bundle), visible on screen:
            // it reads "#general" when the core is linked and running.
            Text(conversationLabel(kind: "public", name: "general", members: [], me: "", showUsernames: false))
        }
    }
}
