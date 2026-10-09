// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI

/// One open conversation's lifecycle: creates its `TimelineModel` and runs its start, stop and
/// scene-phase handling. Like `SignedInSession`, it exists so the view only forwards to it and the
/// lifecycle can be tested without a view (a view cannot be driven from a unit test, which is how a
/// missing `stop()` goes unnoticed).
///
/// The view creates one each time the conversation is pushed and drops it when it is popped.
@MainActor
final class ConversationSession {
    let channelId: String
    let channels: ChannelsModel
    let timeline: TimelineModel
    /// The message box's model. Shared with the Mac; there is no outbox on iOS, so its send goes
    /// straight to the server with the draft's `client_id`.
    let composer: ComposerModel
    /// This user's id, for the follow rule ("mine").
    let me: String
    /// The title as it was when the conversation opened: after a removal the row is gone, but the
    /// screen is still up for a moment.
    private let openedTitle: String

    init(channelId: String, channels: ChannelsModel, client: any ChatClient, me: String,
         isActive: @escaping @MainActor () -> Bool = { AppActivity.isActive }) {
        self.channelId = channelId
        self.channels = channels
        self.me = me
        openedTitle = channels.channels.first { $0.id == channelId }.map(channels.title) ?? ""
        timeline = TimelineModel(
            channelId: channelId, client: client, me: me,
            // Read from the row each time: members change live. Weak, because the channel model
            // holds the timeline (`channels.timeline`) and this closure must not hold it back.
            members: { [weak channels] in channels?.channels.first { $0.id == channelId }?.members ?? [] },
            isActive: isActive,
            // iOS suspends the socket in the background and keeps no cache: a re-read of the newest
            // page is the only way to show what was sent meanwhile (see the flag).
            rereadOnReady: true)
        composer = ComposerModel(channelId: channelId, client: client, onMessage: { [weak timeline] in
            // The sent message shows from the server's answer; the live echo of it merges into
            // the same row (`merge` is by id). Weak, as the Mac does: the composer must not keep
            // the timeline alive.
            timeline?.merge([$0])
        })
    }

    func start() async {
        // Order matters. `openChannel` clears the row's badge; `timeline` makes the message events
        // reach the model. Both before the load, so a message that arrives during it is not lost.
        channels.openChannel = channelId
        channels.timeline = timeline
        await timeline.load()
    }

    /// Clears what `start()` set, but only when it is still this conversation's:
    /// - A fast back-then-open can start the next conversation (even of the same channel) before
    ///   this one stops. Its timeline is then the open one and must not be cut off, so a different
    ///   `openChannel` or a different timeline means "not mine".
    /// - After a removal, `ChannelsModel.cacheRemoved` has already set `timeline` to nil but left
    ///   `openChannel` set. Checking the timeline alone would then leave `openChannel` set for good,
    ///   and a re-added channel's mention badge would never show again. So a nil timeline still
    ///   counts as mine.
    func stop() {
        guard channels.openChannel == channelId else { return }
        if let current = channels.timeline, current !== timeline { return }
        channels.openChannel = nil
        channels.timeline = nil
    }

    /// Coming back to the foreground re-reads the conversation, then marks read what was owed.
    /// Synchronous on purpose: `reread()` takes the gap anchor at this call, so no live message can
    /// be merged between the scene change and the anchor. The returned task is for the caller (a
    /// test) to wait on; the view ignores it.
    ///
    /// The way back from suspension is background -> inactive -> active, so the trigger is "became
    /// active from anything else" (as `ForegroundReload` does for the list).
    @discardableResult
    func sceneChanged(from old: ScenePhase, to new: ScenePhase) -> Task<Void, Never>? {
        guard new == .active, old != .active else { return nil }
        let reread = timeline.reread()
        return Task { [timeline] in
            await reread.value
            // After the re-read, so the read goes against the newest message it brought.
            // `reread()` itself marks read only when the newest id changed; what was owed for a
            // message already shown (read while not active) is marked here.
            timeline.appBecameActive()
        }
    }

    /// `#name` for a channel, the other person for a DM; the one from opening once the row is gone.
    var title: String {
        channels.channels.first { $0.id == channelId }.map(channels.title) ?? openedTitle
    }

    /// The channel is archived: nothing can be written to it, so the view shows a note instead of
    /// the box. Read from the row each time, not once at init: `channel.update` changes the row
    /// while the conversation is open and the note must appear (or go) with it.
    var archived: Bool { channels.channels.first { $0.id == channelId }?.archived ?? false }

    /// The row left the list: the channel was deleted, or this user left or was removed. Not
    /// `channels.closed`: a live `channel.delete` sets that, but a list re-read after a reconnect
    /// just drops the row and never does, and `closed` is never cleared, so a second removal of
    /// the same channel would go unnoticed.
    var isRemoved: Bool { !channels.channels.contains { $0.id == channelId } }
}
