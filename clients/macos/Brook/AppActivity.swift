// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import AppKit

/// Whether Brook is the active app (its window can be seen and used).
enum AppActivity {
    @MainActor static var isActive: Bool { NSApp?.isActive ?? true }
}

enum NSAppActivator {
    @MainActor static func activate() { NSApp.activate() }
}
