// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import android.app.Application
import android.content.Context

/**
 * The process-wide object: the manifest names it so the app has one place to own long-lived
 * state (the session model, a later step) that must outlive any single Activity.
 */
class BrookApp : Application() {
    /**
     * `BuildConfig.DEBUG` is what keeps the plain-`http` switch out of release builds (see
     * [Settings.allowInsecureHttp]); it is passed in here so tests can pass both values.
     */
    lateinit var settings: Settings
        private set

    override fun onCreate() {
        super.onCreate()
        settings = Settings(getSharedPreferences("brook", Context.MODE_PRIVATE), BuildConfig.DEBUG)
    }
}
