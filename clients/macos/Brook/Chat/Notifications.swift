import BrookCore
import Foundation
@preconcurrency import UserNotifications

/// Which live messages count as unread and notify, and what a notification says (spec
/// 2026-09-26-mac-notifications, as GTK's "Make it alive" #2b and #3 and #177).
enum NotificationPlanner {
    /// Someone else's message that isn't being read: not the open channel with the app
    /// active. Never your own, a deleted one, or anything while your id is unknown.
    static func counts(_ m: FfiMessage, me: String?, openChannel: String?, appActive: Bool) -> Bool {
        guard let me, !me.isEmpty, m.authorId != me, !m.deleted else { return false }
        return !(m.channelId == openChannel && appActive)
    }

    /// It names you, or everyone (`@channel`, `@here`).
    static func mentions(_ m: FfiMessage, me: String) -> Bool {
        m.mentionEveryone || m.mentions.contains(me)
    }

    static func body(_ m: FfiMessage, me: String) -> String {
        let author = m.authorDisplayName ?? m.authorHandle ?? "Someone"
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

/// The system's notifications: permission asked once, at the first post; the setting read
/// before each post, so turning notifications on later in System Settings works.
@MainActor
final class MacNotifier: NSObject, Notifying, UNUserNotificationCenterDelegate {
    static let shared = MacNotifier()
    /// A notification was clicked: open its channel.
    var onOpen: ((String) -> Void)?
    private var asked = false

    private var center: UNUserNotificationCenter { UNUserNotificationCenter.current() }

    /// Set as the center's delegate early (clicks arrive through it).
    func install() { center.delegate = self }

    func post(channelId: String, title: String, body: String) {
        let center = self.center
        Task {
            var settings = await center.notificationSettings()
            // Asked at the first notification, and again only while still undecided (a
            // request that failed); a decision, either way, is the user's.
            if settings.authorizationStatus == .notDetermined, !asked {
                asked = true
                let granted = (try? await center.requestAuthorization(options: [.alert, .sound])) ?? false
                settings = await center.notificationSettings()
                if !granted, settings.authorizationStatus == .notDetermined { asked = false }
            }
            guard settings.authorizationStatus == .authorized || settings.authorizationStatus == .provisional
            else { return } // denied or not decided: nothing is posted
            let content = UNMutableNotificationContent()
            content.title = title
            content.body = body
            content.sound = .default
            content.threadIdentifier = channelId
            content.userInfo = ["channelId": channelId]
            try? await center.add(UNNotificationRequest(identifier: channelId, content: content, trigger: nil))
        }
    }

    func remove(channelId: String) {
        center.removeDeliveredNotifications(withIdentifiers: [channelId])
        center.removePendingNotificationRequests(withIdentifiers: [channelId])
    }

    nonisolated func userNotificationCenter(_ center: UNUserNotificationCenter,
                                            didReceive response: UNNotificationResponse) async {
        let channel = response.notification.request.content.userInfo["channelId"] as? String
        await MainActor.run {
            NSAppActivator.activate()
            if let channel { MacNotifier.shared.onOpen?(channel) }
        }
    }
}
