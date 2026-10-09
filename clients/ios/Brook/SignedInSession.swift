// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI

/// One signed-in session's channel list: creates its `ChannelsModel` and runs its start, stop
/// and scene-phase handling. It exists so the view only forwards to it and this lifecycle can be
/// tested without a view: a view cannot be driven from a unit test, which is how a missing
/// `stop()` (a leaked event subscription after sign-out) would have gone unnoticed.
///
/// The view creates it once per sign-in and drops it with the view, so the next sign-in gets a
/// fresh model and nothing carries over between sessions.
@MainActor
final class SignedInSession {
    let channels: ChannelsModel
    /// This user's id. The open conversation needs it (whose messages are "mine" for the follow
    /// rule), and this session is what knows it.
    let me: String

    init(client: any FfiBrookClientProtocol, me: String, defaults: UserDefaults = .standard) {
        self.me = me
        // No notifier: iOS notifications are later work. `AppActivity` is iOS's default.
        // `rereadOnReconnect`: iOS has no cache feed to re-read after a reconnect (see the flag).
        channels = ChannelsModel(client: client, me: me, defaults: defaults, rereadOnReconnect: true)
    }

    func start() async { await channels.start() }

    /// Cancels the event subscription. Called when the view goes away (sign-out, local or remote).
    func stop() { channels.stop() }

    /// Coming back to the foreground re-reads the list (see `ChannelsModel.sceneChanged`).
    func sceneChanged(from old: ScenePhase, to new: ScenePhase) async {
        await channels.sceneChanged(from: old, to: new)
    }
}
