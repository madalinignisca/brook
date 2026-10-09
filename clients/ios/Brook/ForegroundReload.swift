// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import SwiftUI

extension ChannelsModel {
    /// Re-read the list when the app comes back to the foreground. iOS suspends the socket in
    /// the background, so what changed meanwhile (a rename, a member added, a call ended) is
    /// never delivered as an event; it shows only after a re-read. The way back is
    /// background -> inactive -> active, so the trigger is "became active from anything else",
    /// not "from background": the last hop is inactive -> active.
    func sceneChanged(from old: ScenePhase, to new: ScenePhase) async {
        if new == .active, old != .active { await reloadList() }
    }
}
