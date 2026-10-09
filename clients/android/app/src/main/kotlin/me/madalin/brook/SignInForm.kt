// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.setValue

/**
 * What the sign-in screens show and edit. Port of the Mac's `LoginForm`.
 *
 * Holds the password only while the user is typing it: once an attempt has actually been made
 * (successful or rejected by the server), the field is cleared so the password does not linger
 * in memory or on screen. A local validation error (e.g. an empty handle) keeps it, so the user
 * is not made to retype it.
 *
 * The fields are Compose state, so the screens recompose when they change. The derived values
 * read the model's flows with `.value`; a screen that shows them must also collect
 * [SessionModel.phase] (and `codeBusy`) so it recomposes when those change.
 */
class SignInForm(val model: SessionModel) {
    var server by mutableStateOf(model.settings.lastGoodServer ?: "")
    var handle by mutableStateOf("")
    var password by mutableStateOf("")

    /** The code step (TOTP): a 6-digit code, or a recovery code when [useRecovery]. */
    var code by mutableStateOf("")
    var useRecovery by mutableStateOf(false)

    val isBusy: Boolean get() = model.phase.value is Phase.SigningIn || model.codeBusy.value

    val needsCode: Boolean get() = model.phase.value is Phase.NeedsCode

    val error: String?
        get() = when (val phase = model.phase.value) {
            is Phase.SignedOut -> phase.error
            is Phase.NeedsCode -> phase.error
            else -> null
        }

    val insecureWarning: String?
        get() = if (model.settings.allowInsecureHttp) {
            "Insecure connections allowed - your password is sent unencrypted."
        } else {
            null
        }

    suspend fun submit() {
        if (model.signIn(server, handle, password)) password = ""
    }

    /** Send the code (or recovery code); the field is cleared after each attempt. */
    suspend fun submitCode() {
        val entered = code
        code = ""
        if (useRecovery) model.submitRecovery(entered) else model.submitCode(entered)
    }

    fun back() {
        code = ""
        useRecovery = false
        model.back()
    }
}
