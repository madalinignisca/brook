// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import android.app.Application
import android.content.Context
import java.io.File
import kotlinx.coroutines.MainScope
import kotlinx.coroutines.launch
import uniffi.brook_ffi.FfiBrookClient

/**
 * The process-wide object. It owns the one [SessionModel] and starts the launch restore, and
 * not an Activity, for two reasons: a rotation (or any configuration change) recreates the
 * Activity, and a model made there would sign in again and lose the session in flight; and the
 * restore must run once per process, not once per Activity.
 */
class BrookApp : Application() {
    lateinit var session: SessionModel
        private set

    /**
     * What the user is typing on the sign-in screens. It lives here, in process memory only, so
     * a rotation (which recreates the Activity) does not wipe it. It is never put in saved
     * instance state, so the password is gone if the process dies.
     */
    lateinit var signInForm: SignInForm
        private set

    override fun onCreate() {
        super.onCreate()
        // `BuildConfig.DEBUG` is what keeps the plain-`http` switch out of release builds (see
        // [Settings.allowInsecureHttp]); it is passed in here so tests can pass both values.
        // This reads the preferences file on the main thread, once at startup. Left as it is: the
        // file holds a couple of values, so the read is quick and not worth restructuring.
        val settings = Settings(getSharedPreferences("brook", Context.MODE_PRIVATE), BuildConfig.DEBUG)
        // `noBackupFilesDir`, not `filesDir`: Android backup and device transfer skip it, and it
        // is gone after uninstall, so neither the encrypted session nor core's sign-out fences
        // can end up on another phone, where the Keystore key would not exist.
        val slot = KeystoreSlot(File(noBackupFilesDir, "keyslots"), AndroidKeystoreKeys())
        val coreDir = File(noBackupFilesDir, "core").path
        // The main thread is where the model lives. That is safe because its blocking work
        // (network, core's database) is in `suspend` calls that run on Rust's own threads.
        val scope = MainScope()
        session = SessionModel(settings, slot, coreDir, ::FfiBrookClient, scope)
        signInForm = SignInForm(session)
        scope.launch { session.restoreAtLaunch() }
    }
}
