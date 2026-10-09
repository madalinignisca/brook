// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import java.net.URI
import java.net.URISyntaxException

/**
 * Validates what the user typed as the server address, before it reaches core or is
 * remembered. Port of the Mac's `ServerAddress.parse`. Only a bare address is accepted:
 * credentials in the URL would end up stored as the "last server", and a query or fragment
 * has no meaning for the API.
 */
object ServerAddress {
    enum class Problem { Invalid, NotJustAnAddress }

    sealed interface Parsed {
        data class Ok(val address: String) : Parsed
        data class Bad(val problem: Problem) : Parsed
    }

    fun parse(input: String): Parsed {
        val trimmed = input.trim()
        val uri = try {
            URI(trimmed)
        } catch (_: URISyntaxException) {
            return Parsed.Bad(Problem.Invalid)
        }
        val scheme = uri.scheme?.lowercase()
        // `host` is null for an address like "https://" or a name java.net.URI cannot parse as one.
        if (scheme != "http" && scheme != "https" || uri.host.isNullOrEmpty()) {
            return Parsed.Bad(Problem.Invalid)
        }
        if (uri.userInfo != null || uri.query != null || uri.fragment != null) {
            return Parsed.Bad(Problem.NotJustAnAddress)
        }
        return Parsed.Ok(trimmed)
    }
}
