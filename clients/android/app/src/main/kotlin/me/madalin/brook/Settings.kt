// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import android.content.SharedPreferences
import androidx.core.content.edit

/**
 * The few things the app remembers outside core. Plain preferences are fine: nothing here is
 * a secret ([ServerAddress] refuses user info before an address reaches [lastGoodServer]).
 *
 * [isDebugBuild] is `BuildConfig.DEBUG`, passed in by `BrookApp` so tests can pass both values.
 */
class Settings(private val prefs: SharedPreferences, private val isDebugBuild: Boolean) {
    /** The last server address that signed in, to prefill the sign-in screen. */
    var lastGoodServer: String?
        get() = prefs.getString(LAST_GOOD_SERVER, null)
        set(value) = prefs.edit { putString(LAST_GOOD_SERVER, value) }

    /**
     * Lets core talk plain `http` to a non-loopback server (a dev server on the LAN).
     *
     * Always `false` in a release build, whatever is stored. This matters more on Android than
     * elsewhere: core opens its own sockets, so Android's cleartext policy
     * (`usesCleartextTraffic`) never applies to them. Core's `allow_insecure_http` is the only
     * gate, and this getter is what keeps release builds `https`-only (core always allows
     * loopback).
     */
    var allowInsecureHttp: Boolean
        get() = isDebugBuild && prefs.getBoolean(ALLOW_INSECURE_HTTP, false)
        set(value) = prefs.edit { putBoolean(ALLOW_INSECURE_HTTP, value) }

    private companion object {
        const val LAST_GOOD_SERVER = "last_good_server"
        const val ALLOW_INSECURE_HTTP = "allow_insecure_http"
    }
}
