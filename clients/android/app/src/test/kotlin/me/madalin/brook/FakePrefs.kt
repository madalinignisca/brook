// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import android.content.SharedPreferences

/**
 * An in-memory [SharedPreferences] for JVM tests (there is no Robolectric). Only what
 * [Settings] uses is real: string and boolean get/put through `edit().apply()`.
 */
class FakePrefs : SharedPreferences {
    val values = HashMap<String, Any?>()

    override fun getString(key: String, defValue: String?): String? = values[key] as? String ?: defValue
    override fun getBoolean(key: String, defValue: Boolean): Boolean = values[key] as? Boolean ?: defValue
    override fun contains(key: String): Boolean = values.containsKey(key)
    override fun getAll(): MutableMap<String, *> = values
    override fun getInt(key: String, defValue: Int): Int = throw UnsupportedOperationException()
    override fun getLong(key: String, defValue: Long): Long = throw UnsupportedOperationException()
    override fun getFloat(key: String, defValue: Float): Float = throw UnsupportedOperationException()
    override fun getStringSet(key: String, defValues: MutableSet<String>?): MutableSet<String>? =
        throw UnsupportedOperationException()
    override fun registerOnSharedPreferenceChangeListener(l: SharedPreferences.OnSharedPreferenceChangeListener) =
        throw UnsupportedOperationException()
    override fun unregisterOnSharedPreferenceChangeListener(l: SharedPreferences.OnSharedPreferenceChangeListener) =
        throw UnsupportedOperationException()

    override fun edit(): SharedPreferences.Editor = object : SharedPreferences.Editor {
        private val pending = HashMap<String, Any?>()
        private var clear = false

        override fun putString(key: String, value: String?) = apply { pending[key] = value }
        override fun putBoolean(key: String, value: Boolean) = apply { pending[key] = value }
        override fun remove(key: String) = apply { pending[key] = null }
        override fun clear() = apply { clear = true }
        override fun putInt(key: String, value: Int) = throw UnsupportedOperationException()
        override fun putLong(key: String, value: Long) = throw UnsupportedOperationException()
        override fun putFloat(key: String, value: Float) = throw UnsupportedOperationException()
        override fun putStringSet(key: String, values: MutableSet<String>?) = throw UnsupportedOperationException()

        override fun commit(): Boolean {
            apply()
            return true
        }

        override fun apply() {
            if (clear) values.clear()
            for ((k, v) in pending) if (v == null) values.remove(k) else values[k] = v
        }
    }
}
