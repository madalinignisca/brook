// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI

/// How a person reads on every surface (#238): `@handle` with Show usernames on, the display
/// name otherwise. People are kept as (name, handle) and labelled when drawn, so a toggle
/// relabels what is on screen without a reload.
enum PersonName {
    /// A person with neither a name nor a handle (a deleted author).
    static let unknown = "Someone"

    private static func trimmed(_ s: String?) -> String {
        (s ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
    }

    static func label(_ name: String?, handle: String?, showUsernames: Bool) -> String {
        let name = trimmed(name), handle = trimmed(handle)
        // Core's rule gives "@" for an empty handle; a bot or deleted author has none.
        if handle.isEmpty { return name.isEmpty ? unknown : name }
        return personLabel(displayName: name, handle: handle, showUsernames: showUsernames)
    }

    /// The form not shown, for a second line: nil when there is nothing to add.
    static func other(_ name: String?, handle: String?, showUsernames: Bool) -> String? {
        let n = trimmed(name), h = trimmed(handle)
        guard !n.isEmpty, !h.isEmpty else { return nil }
        let candidate = showUsernames ? n : "@\(h)"
        return candidate == label(name, handle: handle, showUsernames: showUsernames) ? nil : candidate
    }

    /// "label (other)", or the label alone.
    static func both(_ name: String?, handle: String?, showUsernames: Bool) -> String {
        let main = label(name, handle: handle, showUsernames: showUsernames)
        guard let other = other(name, handle: handle, showUsernames: showUsernames) else { return main }
        return "\(main) (\(other))"
    }
}

extension EnvironmentValues {
    /// The Settings window's "Show usernames", for every view under `FollowsShowUsernames`.
    @Entry var showUsernames = false
}

/// Puts the preference into the environment. Apply it as the outermost modifier of a scene's
/// root: sheets, popovers and dialogs inherit from there, and a separate `Window` scene
/// does not inherit from another.
struct FollowsShowUsernames: ViewModifier {
    @AppStorage(Settings.showUsernamesKey) private var on = false

    func body(content: Content) -> some View {
        content.environment(\.showUsernames, on)
    }
}
