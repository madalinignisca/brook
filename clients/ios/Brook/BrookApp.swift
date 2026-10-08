// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI

/// Placeholder app for the first step of the iOS skeleton (#272): it only proves that the
/// app binary itself links the Rust core and calls it. The sign-in and channel list replace it.
@main
struct BrookApp: App {
    var body: some Scene {
        WindowGroup {
            // A call into Rust made by the app (not only by the test bundle), visible on screen:
            // it reads "#general" when the core is linked and running.
            Text(conversationLabel(kind: "public", name: "general", members: [], me: "", showUsernames: false))
        }
    }
}
