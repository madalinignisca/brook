// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import Foundation

/// Which live messages count as unread and notify, and what a notification says (spec
/// 2026-09-26-mac-notifications, as GTK's "Make it alive" #2b and #3 and #177).
enum NotificationPlanner {
    /// Someone else's message that isn't being read: not the open channel with the app
    /// active. Never your own, a deleted one, or anything while your id is unknown.
    static func counts(_ m: FfiMessage, me: String?, openChannel: String?, appActive: Bool) -> Bool {
        guard let me, !me.isEmpty, m.authorId != me, !m.deleted else { return false }
        return !(m.channelId == openChannel && appActive)
    }

    /// Someone else's live message naming you or everyone (`@channel`, `@here`): the
    /// server's, core's and GTK's rule, whole here even though `counts` also checks the first
    /// two, so no caller can count your own `@channel` from another device.
    static func mentions(_ m: FfiMessage, me: String) -> Bool {
        !me.isEmpty && m.authorId != me && !m.deleted && (m.mentionEveryone || m.mentions.contains(me))
    }

    static func body(_ m: FfiMessage, me: String, showUsernames: Bool) -> String {
        let author = PersonName.label(m.authorDisplayName, handle: m.authorHandle, showUsernames: showUsernames) // raw name: input to the label
        if m.body.isEmpty, !m.attachments.isEmpty { return "\(author) sent a file" }
        return mentions(m, me: me) ? "\(author) mentioned you: \(m.body)" : "\(author): \(m.body)"
    }
}

/// Posts and removes this app's notifications (a fake in tests).
@MainActor
protocol Notifying: AnyObject {
    /// One per channel: a later one replaces it.
    func post(channelId: String, title: String, body: String)
    func remove(channelId: String)
}
