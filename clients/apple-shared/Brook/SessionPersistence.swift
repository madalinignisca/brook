// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore

/// Whether this launch keeps the session across launches (spec #46 §4, plan #80 P3).
enum SessionPersistence {
    /// Stored in the keychain `slot`, with core's sign-out fences in `dataDir`.
    case on(slot: FfiKeySlot, dataDir: String)
    /// Nothing is stored; quitting signs out. Each app says why in its own persistence file
    /// (`choose` on the Mac, `prepare` on iOS).
    case off
    /// Another Brook holds the instance lock: this one never touches the stored session.
    case secondInstance
}
