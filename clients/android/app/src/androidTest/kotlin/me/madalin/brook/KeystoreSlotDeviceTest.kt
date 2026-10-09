// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import android.content.Context
import androidx.test.core.app.ApplicationProvider
import java.io.File
import java.security.KeyStore
import org.junit.After
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.fail
import org.junit.Before
import org.junit.Test
import uniffi.brook_ffi.FfiKeySlotException

/**
 * [KeystoreSlot] against the real Android Keystore. Run by hand on the API 33 emulator
 * (`./gradlew connectedDebugAndroidTest`); CI has no emulator. The JVM tests cover the logic
 * with a fake key; this proves the real key spec (AES-256 GCM, no auth) really works.
 */
class KeystoreSlotDeviceTest {
    private lateinit var dir: File

    @Before
    fun setUp() {
        val context = ApplicationProvider.getApplicationContext<Context>()
        dir = File(context.cacheDir, "keystore-slot-test").apply { deleteRecursively() }
        deleteAlias()
    }

    @After
    fun tearDown() {
        dir.deleteRecursively()
        deleteAlias()
    }

    private fun deleteAlias() {
        KeyStore.getInstance("AndroidKeyStore").apply { load(null) }
            .deleteEntry(AndroidKeystoreKeys.ALIAS)
    }

    private fun slots() = KeystoreSlot(dir, AndroidKeystoreKeys())

    @Test
    fun roundTripThroughTheRealKeystore() {
        val s = slots()
        assertNull(s.load("session:https://host"))
        s.create("session:https://host", byteArrayOf(1, 2, 3))
        assertArrayEquals(byteArrayOf(1, 2, 3), s.load("session:https://host"))
        s.replace("session:https://host", byteArrayOf(4))
        assertArrayEquals(byteArrayOf(4), s.load("session:https://host"))
    }

    @Test
    fun aValueSurvivesANewInstance() {
        slots().create("a", byteArrayOf(9, 9))
        assertArrayEquals(byteArrayOf(9, 9), slots().load("a"))
    }

    @Test
    fun deletingTheAliasMakesAnExistingSlotFatal() {
        val s = slots()
        s.create("a", byteArrayOf(1))
        deleteAlias()
        try {
            s.load("a")
            fail("expected Fatal")
        } catch (e: FfiKeySlotException.Fatal) {
            assertEquals(-10, e.status)
        }
    }
}
