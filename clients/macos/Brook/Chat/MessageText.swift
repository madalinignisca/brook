// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import Foundation

/// The pure text helpers a message row needs (a quote's line, a time, a file's icon name). They
/// are not on the row views because the Mac and iOS draw their own rows, and both need the same
/// words; shared code keeps them here, free of any view.
enum MessageText {
    /// A quote's line, from its state rather than its text.
    static func excerpt(_ quote: FfiReplyExcerpt) -> String {
        if quote.deleted { return "a deleted message" }
        let flat = quote.body.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        if flat.isEmpty { return quote.attachments > 0 ? "a file" : "a message" }
        return String(flat.prefix(80))
    }

    static func time(_ iso: String) -> String {
        let parser = ISO8601DateFormatter()
        parser.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let date = parser.date(from: iso) ?? ISO8601DateFormatter().date(from: iso)
        guard let date else { return "" }
        return date.formatted(date: .omitted, time: .shortened)
    }

    static func fileIcon(_ type: String) -> String {
        if type.hasPrefix("image/") { return "photo" }
        if type.hasPrefix("video/") { return "film" }
        if type.hasPrefix("audio/") { return "waveform" }
        if type == "application/pdf" { return "doc.richtext" }
        return "doc"
    }
}
