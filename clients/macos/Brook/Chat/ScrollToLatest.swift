// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import Foundation

/// Pure arithmetic with no view in it, so the Mac and iOS conversation screens share one rule.
/// When the "jump to latest" button shows: the view has been scrolled up by more than a little.
enum ScrollToLatest {
    /// Space under the last message, above the composer.
    static let gap: CGFloat = 12
    /// Further up than this (points, beyond the gap) counts as away.
    static let threshold: CGFloat = 80

    static func isAway(contentHeight: CGFloat, offset: CGFloat, viewportHeight: CGFloat) -> Bool {
        // Content shorter than the view can't be scrolled up.
        contentHeight - (offset + viewportHeight) > threshold
    }
}
