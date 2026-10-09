// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import android.security.keystore.KeyGenParameterSpec
import android.security.keystore.KeyProperties
import java.io.File
import java.io.FileOutputStream
import java.io.IOException
import java.nio.channels.FileChannel
import java.nio.file.Files
import java.nio.file.StandardCopyOption
import java.nio.file.StandardOpenOption
import java.security.GeneralSecurityException
import java.security.KeyStore
import java.security.KeyStoreException
import java.security.ProviderException
import javax.crypto.AEADBadTagException
import javax.crypto.Cipher
import javax.crypto.KeyGenerator
import javax.crypto.SecretKey
import javax.crypto.spec.GCMParameterSpec
import uniffi.brook_ffi.FfiKeySlot
import uniffi.brook_ffi.FfiKeySlotException

/** Where [KeystoreSlot] gets its one AES key. A seam so JVM tests can use an in-memory key. */
interface SlotKeys {
    /**
     * The key, or `null` only when the alias is truly absent. Any other trouble (a locked or
     * unreachable Keystore) must THROW, never return `null`: `null` lets [KeystoreSlot] make a
     * new key, which would orphan every slot already written under the old one.
     */
    fun existing(): SecretKey?

    /** Makes the key. Called only after [existing] returned `null`. */
    fun create(): SecretKey
}

/**
 * The real thing: one AES-256 key in the Android Keystore under [ALIAS].
 *
 * No user authentication, no `setUnlockedDeviceRequired`, no StrongBox. Core calls slots from
 * background work with no UI and needs an answer within seconds (the `KeySlot` contract in
 * core/src/keyslot.rs); an auth prompt or a slow StrongBox call would break that.
 */
class AndroidKeystoreKeys : SlotKeys {
    private fun store(): KeyStore = KeyStore.getInstance(PROVIDER).apply { load(null) }

    override fun existing(): SecretKey? {
        val store = store()
        if (!store.containsAlias(ALIAS)) return null
        // The alias is there, so a missing key now is a Keystore fault, not "absent": throw.
        return store.getKey(ALIAS, null) as? SecretKey
            ?: throw KeyStoreException("alias present but no secret key")
    }

    override fun create(): SecretKey {
        val spec = KeyGenParameterSpec.Builder(
            ALIAS,
            KeyProperties.PURPOSE_ENCRYPT or KeyProperties.PURPOSE_DECRYPT,
        )
            .setBlockModes(KeyProperties.BLOCK_MODE_GCM)
            .setEncryptionPaddings(KeyProperties.ENCRYPTION_PADDING_NONE)
            .setKeySize(256)
            .build()
        return KeyGenerator.getInstance(KeyProperties.KEY_ALGORITHM_AES, PROVIDER)
            .apply { init(spec) }
            .generateKey()
    }

    companion object {
        const val ALIAS = "brook-keyslots"
        private const val PROVIDER = "AndroidKeyStore"
    }
}

/**
 * Core's [FfiKeySlot] on Android: named byte slots, each an AES-GCM file under [dir] whose key
 * lives in the Keystore. The bytes are core's refresh token and database key.
 *
 * File: `dir/<hex of the slot name's UTF-8>` (so a name like `session:https://host` is a safe
 * file name), holding `iv (12 bytes) || ciphertext+tag`. The IV is whatever the Keystore picks
 * (a caller-chosen IV is refused for Keystore keys). The AAD is the slot name, so a file copied
 * to another slot's name fails to decrypt instead of loading the wrong session.
 *
 * The rules below are core's `KeySlot` contract (core/src/keyslot.rs), and the point of them is
 * that core never mistakes "I cannot read this" for "nothing is stored":
 *  - Absent (`null`) means exactly one thing: no file. Core treats absent as "signed out, start
 *    fresh" and may overwrite, so reporting it for a damaged slot would silently destroy a
 *    session.
 *  - A file with no key, or one that will not decrypt, is [FfiKeySlotException.Fatal]: retrying
 *    cannot fix it. Statuses (numbers only, so no text can carry a secret): -10 no key, -11 bad
 *    tag or truncated file, -12 anything else unexpected.
 *  - A Keystore, provider or IO fault is [FfiKeySlotException.Unavailable]: nothing is wrong
 *    with the data, so core keeps everything and tries later.
 *  - `load` never creates a key; only a write does.
 *
 * Every method is `@Synchronized`: the app is one process and `BrookApp` holds the only
 * instance, so create-only and replace are race-free without file locks.
 *
 * Nothing here logs: plaintext, tokens and slot contents must never reach logcat.
 */
class KeystoreSlot(private val dir: File, private val keys: SlotKeys) : FfiKeySlot {
    @Synchronized
    override fun load(slot: String): ByteArray? = guarded {
        val file = fileFor(slot)
        if (!file.exists()) return@guarded null
        val key = keys.existing() ?: throw FfiKeySlotException.Fatal(NO_KEY)
        val raw = Files.readAllBytes(file.toPath())
        // Too short to hold an IV and a tag: it cannot be authentic.
        if (raw.size < IV_BYTES + TAG_BYTES) throw FfiKeySlotException.Fatal(BAD_TAG)
        val cipher = Cipher.getInstance(TRANSFORM)
        cipher.init(Cipher.DECRYPT_MODE, key, GCMParameterSpec(TAG_BYTES * 8, raw, 0, IV_BYTES))
        cipher.updateAAD(slot.toByteArray())
        try {
            cipher.doFinal(raw, IV_BYTES, raw.size - IV_BYTES)
        } catch (_: AEADBadTagException) {
            throw FfiKeySlotException.Fatal(BAD_TAG)
        }
    }

    @Synchronized
    override fun create(slot: String, bytes: ByteArray) = guarded {
        // Create-only: core uses `Exists` to learn that another writer got there first.
        if (fileFor(slot).exists()) throw FfiKeySlotException.Exists()
        write(slot, bytes)
    }

    @Synchronized
    override fun replace(slot: String, bytes: ByteArray) = guarded { write(slot, bytes) }

    @Synchronized
    override fun delete(slot: String) = guarded {
        // Absent is fine. The key stays: other slots still need it.
        Files.deleteIfExists(fileFor(slot).toPath())
        syncDir()
    }

    private fun fileFor(slot: String): File =
        File(dir, slot.toByteArray().joinToString("") { "%02x".format(it) })

    /**
     * Encrypts, writes a temp file, fsyncs it, renames it over the target, then fsyncs the
     * directory. The rename is atomic, so a crash leaves the old value or the new, never half;
     * the directory fsync makes the rename itself survive a crash right after.
     */
    private fun write(slot: String, bytes: ByteArray) {
        // The key is made only when the alias is truly absent (see SlotKeys.existing). Slots
        // written under an earlier, lost key stay Fatal on load, never absent.
        val key = keys.existing() ?: keys.create()
        val cipher = Cipher.getInstance(TRANSFORM)
        cipher.init(Cipher.ENCRYPT_MODE, key)
        cipher.updateAAD(slot.toByteArray())
        val sealed = cipher.iv + cipher.doFinal(bytes)

        dir.mkdirs()
        val target = fileFor(slot)
        val temp = File(dir, target.name + ".tmp")
        try {
            FileOutputStream(temp).use { out ->
                out.write(sealed)
                out.fd.sync()
            }
            Files.move(
                temp.toPath(), target.toPath(),
                StandardCopyOption.ATOMIC_MOVE, StandardCopyOption.REPLACE_EXISTING,
            )
        } finally {
            temp.delete() // only still there if the move did not happen
        }
        syncDir()
    }

    private fun syncDir() {
        if (!dir.isDirectory) return
        FileChannel.open(dir.toPath(), StandardOpenOption.READ).use { it.force(true) }
    }

    /** Maps every failure to the three outcomes core understands. */
    private fun <T> guarded(op: () -> T): T = try {
        op()
    } catch (e: FfiKeySlotException) {
        throw e
    } catch (_: KeyStoreException) {
        throw FfiKeySlotException.Unavailable()
    } catch (_: ProviderException) {
        throw FfiKeySlotException.Unavailable()
    } catch (_: IOException) {
        throw FfiKeySlotException.Unavailable()
    } catch (_: GeneralSecurityException) {
        throw FfiKeySlotException.Fatal(OTHER)
    } catch (_: RuntimeException) {
        throw FfiKeySlotException.Fatal(OTHER)
    }

    private companion object {
        const val TRANSFORM = "AES/GCM/NoPadding"
        const val IV_BYTES = 12
        const val TAG_BYTES = 16
        const val NO_KEY = -10
        const val BAD_TAG = -11
        const val OTHER = -12
    }
}
