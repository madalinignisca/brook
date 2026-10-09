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

    /// Whether the top of the content is within one screen of the visible area: time to ask for the
    /// older page, so it is there before the user reaches the top. `visibleMinY` is the visible
    /// rect's top edge in content coordinates (0 at the very top). iOS only; the Mac's loader asks
    /// when it appears.
    static func isNearTop(visibleMinY: CGFloat, viewportHeight: CGFloat) -> Bool {
        visibleMinY < viewportHeight
    }

    /// Whether the user has not scrolled since an older page was asked for: the visible top is
    /// within `tolerance` points of where it was at the ask. Only then is the reading place put
    /// back when the page lands; otherwise the user moved on and a scroll would pull them back.
    static func stayedPut(askedAt: CGFloat, now: CGFloat, tolerance: CGFloat = 44) -> Bool {
        abs(now - askedAt) <= tolerance
    }

    /// Whether the view must scroll to keep the bottom in view: it was at the very bottom (within
    /// `gap` of it, not the looser "away" band) before the content got taller (a row grew, as when
    /// a reaction lands on the last message, or rows were added), and the user is not scrolling.
    /// The strict distance and the idle check keep it from fighting a slow drag up while the lazy
    /// stack measures rows. Without it the growth pushes the bottom out of view, the jump button
    /// shows, and the next new message is not followed. iOS only.
    static func pinsToBottom(oldDistanceFromBottom: CGFloat, oldContentHeight: CGFloat,
                             newContentHeight: CGFloat, scrolling: Bool) -> Bool {
        !scrolling && oldDistanceFromBottom <= gap && newContentHeight > oldContentHeight
    }

    /// The same pin for a shorter visible area with unchanged content: the keyboard rising, or the
    /// message box growing to more lines, shrinks the viewport from the bottom, which covers the
    /// newest message and turns "away" on by itself. If the view was at the very bottom and idle,
    /// scroll to keep it there. Not while scrolling (a drag back down during the keyboard's
    /// animation must not be pulled), and not when above the bottom (a reader in the history stays).
    /// A viewport that grows (the keyboard going away) needs nothing: the bottom stays in view.
    static func pinsAfterShrink(oldDistanceFromBottom: CGFloat, oldViewportHeight: CGFloat,
                                newViewportHeight: CGFloat, scrolling: Bool) -> Bool {
        !scrolling && oldDistanceFromBottom <= gap && newViewportHeight < oldViewportHeight
    }

    /// Whether the view scrolls to a new newest message. At the bottom it follows; scrolled up, only
    /// the user's own message moves it (spec 2026-10-09-ios-conversation, decision 3). The Mac
    /// does not call this yet.
    static func follows(away: Bool, mine: Bool) -> Bool { !away || mine }
}
