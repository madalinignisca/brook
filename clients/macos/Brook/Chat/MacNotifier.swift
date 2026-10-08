// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import Foundation
// UserNotifications is not Sendable-annotated yet; the delegate callbacks below need this.
@preconcurrency import UserNotifications

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
