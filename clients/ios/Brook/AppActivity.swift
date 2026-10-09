// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import UIKit

/// Whether Brook is the active app (on screen and taking input). The shared `ChannelsModel`
/// names this type; each app defines its own (the Mac's reads `NSApp`), so the shared code has
/// no `#if os(...)`. `.inactive` (a call or the app switcher is covering the app) and
/// `.background` both count as not active.
enum AppActivity {
    @MainActor static var isActive: Bool { UIApplication.shared.applicationState == .active }
}
