// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import AppKit

/// AppKit's opener, kept apart so the model file stays free of AppKit.
enum NSWorkspaceBridge {
    @MainActor static func open(_ url: URL) -> Bool { NSWorkspace.shared.open(url) }
}
