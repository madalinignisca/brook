// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test

class SettingsTest {
    @Test
    fun lastGoodServerRoundTrips() {
        val prefs = FakePrefs()
        assertNull(Settings(prefs, isDebugBuild = false).lastGoodServer)
        Settings(prefs, isDebugBuild = false).lastGoodServer = "https://chat.example.com"
        assertEquals("https://chat.example.com", Settings(prefs, isDebugBuild = false).lastGoodServer)
    }

    @Test
    fun debugBuildHonoursTheStoredSwitch() {
        val s = Settings(FakePrefs(), isDebugBuild = true)
        assertFalse(s.allowInsecureHttp)
        s.allowInsecureHttp = true
        assertTrue(s.allowInsecureHttp)
    }

    /** The release gate: a stored `true` (an old debug install, edited prefs) must read `false`. */
    @Test
    fun releaseBuildForcesTheSwitchOffWhateverIsStored() {
        val prefs = FakePrefs()
        Settings(prefs, isDebugBuild = true).allowInsecureHttp = true
        assertTrue(prefs.getBoolean("allow_insecure_http", false))
        assertFalse(Settings(prefs, isDebugBuild = false).allowInsecureHttp)
    }
}
