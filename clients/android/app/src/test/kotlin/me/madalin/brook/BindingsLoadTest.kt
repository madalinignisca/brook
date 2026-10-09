// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import org.junit.Assert.assertArrayEquals
import org.junit.Assert.assertNull
import org.junit.Assert.fail
import org.junit.Test
import uniffi.brook_ffi.FfiKeySlot
import uniffi.brook_ffi.FfiKeySlotException

/**
 * Proves the generated Kotlin bindings compile and a Kotlin class can implement core's
 * callback interface. It runs on the JVM with no native library: `FfiKeySlot` is a plain
 * interface, so nothing here crosses into Rust. Step 11 replaces it with real KeySlot tests.
 */
class BindingsLoadTest {
    /** The smallest possible slot store: a map, with core's create-only rule. */
    private class MemorySlot : FfiKeySlot {
        private val slots = HashMap<String, ByteArray>()

        override fun load(slot: String): ByteArray? = slots[slot]

        override fun create(slot: String, bytes: ByteArray) {
            // Core relies on this: create never overwrites, it reports Exists.
            if (slots.containsKey(slot)) throw FfiKeySlotException.Exists()
            slots[slot] = bytes
        }

        override fun replace(slot: String, bytes: ByteArray) {
            slots[slot] = bytes
        }

        override fun delete(slot: String) {
            slots.remove(slot)
        }
    }

    @Test
    fun createOnATakenSlotThrowsExists() {
        val store = MemorySlot()
        assertNull(store.load("session"))
        store.create("session", byteArrayOf(1, 2, 3))
        assertArrayEquals(byteArrayOf(1, 2, 3), store.load("session"))
        try {
            store.create("session", byteArrayOf(4))
            fail("create on a taken slot must throw FfiKeySlotException.Exists")
        } catch (_: FfiKeySlotException.Exists) {
            // expected
        }
    }
}
