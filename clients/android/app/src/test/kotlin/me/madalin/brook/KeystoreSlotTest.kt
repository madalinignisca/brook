// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import java.io.File
import java.io.IOException
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.security.KeyStoreException
import java.security.ProviderException
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertEquals
import org.junit.Assert.assertFalse
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Assert.fail
import org.junit.Rule
import org.junit.Test
import org.junit.rules.TemporaryFolder
import uniffi.brook_ffi.FfiKeySlotException

/**
 * Runs [KeystoreSlot] on the JVM. The real Android Keystore does not exist here, so a fake
 * [SlotKeys] hands out an ordinary in-memory AES key; the file format, the error mapping and
 * the create-only / atomic-replace rules are all in [KeystoreSlot] itself and are tested for
 * real. The Keystore itself is covered by the device test (androidTest).
 */
class KeystoreSlotTest {
    @get:Rule
    val tmp = TemporaryFolder()

    /** An in-memory stand-in for the Keystore: one key, which a test can "lose". */
    private class FakeKeys : SlotKeys {
        var key: SecretKey? = null
        var existingFails: Exception? = null
        var created = 0

        override fun existing(): SecretKey? {
            existingFails?.let { throw it }
            return key
        }

        override fun create(): SecretKey {
            created++
            return KeyGenerator.getInstance("AES").apply { init(256) }.generateKey().also { key = it }
        }

        fun lose() {
            key = null
        }
    }

    private val keys = FakeKeys()
    private fun dir() = File(tmp.root, "slots")
    private fun slots() = KeystoreSlot(dir(), keys)

    private fun status(block: () -> Unit): Int {
        try {
            block()
        } catch (e: FfiKeySlotException.Fatal) {
            return e.status
        }
        fail("expected FfiKeySlotException.Fatal")
        return 0
    }

    @Test
    fun absentLoadsNull() {
        assertNull(slots().load("session"))
    }

    @Test
    fun loadNeverCreatesAKey() {
        slots().load("session")
        assertEquals(0, keys.created)
    }

    @Test
    fun createThenLoadRoundTripsAndASecondCreateIsExists() {
        val s = slots()
        s.create("session", byteArrayOf(1, 2, 3))
        assertArrayEquals(byteArrayOf(1, 2, 3), s.load("session"))
        try {
            s.create("session", byteArrayOf(9))
            fail("a second create must throw Exists")
        } catch (_: FfiKeySlotException.Exists) {
            // expected
        }
        // The failed create must not have touched the stored value.
        assertArrayEquals(byteArrayOf(1, 2, 3), s.load("session"))
    }

    @Test
    fun replaceOverwritesAndCreatesWhenAbsent() {
        val s = slots()
        s.replace("a", byteArrayOf(1))
        assertArrayEquals(byteArrayOf(1), s.load("a"))
        s.replace("a", byteArrayOf(2, 2))
        assertArrayEquals(byteArrayOf(2, 2), s.load("a"))
    }

    @Test
    fun deleteOfAnAbsentSlotIsFineAndDeleteRemoves() {
        val s = slots()
        s.delete("nothing")
        s.create("a", byteArrayOf(1))
        s.delete("a")
        assertNull(s.load("a"))
        // Delete never touches the key.
        assertTrue(keys.key != null)
    }

    @Test
    fun slotNameWithColonAndSlashWorks() {
        val s = slots()
        val name = "session:https://host/x"
        s.create(name, byteArrayOf(7))
        assertArrayEquals(byteArrayOf(7), s.load(name))
    }

    @Test
    fun noFileContainsThePlaintext() {
        val sentinel = "PLAINTEXT-SENTINEL-0123456789".toByteArray()
        val s = slots()
        s.create("a", sentinel)
        s.replace("b", sentinel)
        val files = dir().listFiles()!!
        assertTrue(files.isNotEmpty())
        for (f in files) {
            val text = String(f.readBytes(), Charsets.ISO_8859_1)
            assertFalse(f.name, text.contains(String(sentinel, Charsets.ISO_8859_1)))
        }
    }

    @Test
    fun noTempFileIsLeftBehind() {
        val s = slots()
        s.create("a", byteArrayOf(1))
        s.replace("a", byteArrayOf(2))
        assertEquals(1, dir().listFiles()!!.size)
    }

    @Test
    fun aFileMovedToAnotherSlotNameIsFatalNotAbsent() {
        val s = slots()
        s.create("one", byteArrayOf(1))
        s.create("two", byteArrayOf(2))
        // Put slot one's bytes under slot two's file name: the AAD (the slot name) no longer matches.
        Files.move(
            File(dir(), hex("one")).toPath(), File(dir(), hex("two")).toPath(),
            StandardCopyOption.REPLACE_EXISTING,
        )
        assertEquals(-11, status { s.load("two") })
    }

    @Test
    fun afterTheKeyIsLostAnExistingSlotIsFatalNotAbsent() {
        val s = slots()
        s.create("a", byteArrayOf(1))
        keys.lose()
        assertEquals(-10, status { s.load("a") })
        // Loading must not have made a new key.
        assertEquals(1, keys.created)
    }

    @Test
    fun afterTheKeyIsLostReplaceMakesANewKeyAndOtherSlotsStayFatal() {
        val s = slots()
        s.create("old", byteArrayOf(1))
        keys.lose()
        s.replace("new", byteArrayOf(5))
        assertArrayEquals(byteArrayOf(5), s.load("new"))
        assertEquals(-11, status { s.load("old") })
    }

    @Test
    fun aTruncatedFileIsFatal() {
        val s = slots()
        s.create("a", byteArrayOf(1, 2, 3))
        val f = File(dir(), hex("a"))
        for (keep in listOf(0, 5, 12, f.length().toInt() - 1)) {
            f.writeBytes(f.readBytes().copyOf(keep))
            assertEquals("kept $keep", -11, status { s.load("a") })
            s.replace("a", byteArrayOf(1, 2, 3))
        }
    }

    @Test
    fun keystoreAndIoFaultsAreUnavailableNotFatalNotAbsent() {
        val s = slots()
        s.create("a", byteArrayOf(1))
        for (fault in listOf<Exception>(KeyStoreException("locked"), IOException("io"), ProviderException("p"))) {
            keys.existingFails = fault
            try {
                s.load("a")
                fail("expected Unavailable for $fault")
            } catch (_: FfiKeySlotException.Unavailable) {
                // expected
            }
        }
    }

    @Test
    fun aDirectoryInPlaceOfTheFileIsUnavailable() {
        val s = slots()
        s.create("a", byteArrayOf(1))
        val f = File(dir(), hex("a"))
        f.delete()
        f.mkdir()
        try {
            s.load("a")
            fail("expected Unavailable")
        } catch (_: FfiKeySlotException.Unavailable) {
            // expected
        }
    }

    private fun hex(name: String) = name.toByteArray().joinToString("") { "%02x".format(it) }
}
