// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import org.junit.Assert.assertEquals
import org.junit.Test

/** Ports the four tests of the Mac's `ServerAddressTests`. */
class ServerAddressTest {
    @Test
    fun acceptsPlainServerAddresses() {
        for (ok in listOf("https://chat.example.com", "http://192.168.1.192:8080", "https://host.lan/brook")) {
            assertEquals(ok, ServerAddress.parse(ok), ServerAddress.Parsed.Ok(ok))
        }
    }

    @Test
    fun trimsSurroundingWhitespace() {
        assertEquals(
            ServerAddress.Parsed.Ok("https://chat.example.com"),
            ServerAddress.parse("  https://chat.example.com \n"),
        )
    }

    /** User info would be remembered as the "last server": a secret in plain preferences. */
    @Test
    fun rejectsCredentialsQueryAndFragment() {
        for (bad in listOf("https://u:secret@host", "https://u@host", "https://host?x=1", "https://host#f")) {
            assertEquals(bad, ServerAddress.Parsed.Bad(ServerAddress.Problem.NotJustAnAddress), ServerAddress.parse(bad))
        }
    }

    @Test
    fun rejectsMissingSchemeOrHost() {
        for (bad in listOf("chat.example.com", "https://", "ftp://host", "")) {
            assertEquals(bad, ServerAddress.Parsed.Bad(ServerAddress.Problem.Invalid), ServerAddress.parse(bad))
        }
    }
}
