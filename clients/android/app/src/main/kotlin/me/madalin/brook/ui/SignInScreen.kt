// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook.ui

import androidx.activity.compose.BackHandler
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.safeDrawingPadding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.text.KeyboardOptions
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.Switch
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.collectAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.rememberCoroutineScope
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.autofill.ContentType
import androidx.compose.ui.semantics.contentType
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.input.KeyboardType
import androidx.compose.ui.text.input.PasswordVisualTransformation
import androidx.compose.ui.unit.dp
import kotlinx.coroutines.launch
import me.madalin.brook.BuildConfig
import me.madalin.brook.SignInForm

/**
 * The sign-in form, or the code step after it when the account has TOTP. One screen, so the
 * typed server and handle stay while the code step is shown.
 */
@Composable
fun SignInScreen(form: SignInForm) {
    // The form reads the model's flows with `.value`, which Compose cannot see. Collecting them
    // here is what makes this screen recompose when the phase or the code check changes.
    val model = form.model
    model.phase.collectAsState().value
    model.codeBusy.collectAsState().value
    val warning by model.signOutWarning.collectAsState()
    val scope = rememberCoroutineScope()

    // The system back on the code step is the same as the Back button (it cancels the challenge).
    BackHandler(enabled = form.needsCode) { form.back() }

    Column(
        modifier = Modifier
            .fillMaxSize()
            .safeDrawingPadding()
            .imePadding()
            .verticalScroll(rememberScrollState())
            .padding(16.dp),
        verticalArrangement = Arrangement.spacedBy(12.dp),
    ) {
        Text("Brook", style = MaterialTheme.typography.headlineLarge)
        if (form.needsCode) {
            CodeStep(form, onSubmit = { scope.launch { form.submitCode() } })
        } else {
            PasswordStep(form, onSubmit = { scope.launch { form.submit() } })
        }
        form.error?.let { Text(it, color = MaterialTheme.colorScheme.error) }
        warning?.let { Text(it, color = MaterialTheme.colorScheme.error) }
    }
}

@Composable
private fun PasswordStep(form: SignInForm, onSubmit: () -> Unit) {
    OutlinedTextField(
        value = form.server,
        onValueChange = { form.server = it },
        label = { Text("Server") },
        singleLine = true,
        enabled = !form.isBusy,
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Uri),
        modifier = Modifier.fillMaxWidth(),
    )
    OutlinedTextField(
        value = form.handle,
        onValueChange = { form.handle = it },
        label = { Text("Handle") },
        singleLine = true,
        enabled = !form.isBusy,
        modifier = Modifier.fillMaxWidth().semantics { contentType = ContentType.Username },
    )
    OutlinedTextField(
        value = form.password,
        onValueChange = { form.password = it },
        label = { Text("Password") },
        singleLine = true,
        enabled = !form.isBusy,
        visualTransformation = PasswordVisualTransformation(),
        keyboardOptions = KeyboardOptions(keyboardType = KeyboardType.Password),
        modifier = Modifier.fillMaxWidth().semantics { contentType = ContentType.Password },
    )
    if (BuildConfig.DEBUG) InsecureSwitch(form)
    SubmitButton("Sign in", busy = form.isBusy, onClick = onSubmit)
}

/**
 * Debug builds only (the release build's [me.madalin.brook.Settings] answers `false` whatever is
 * stored, and this switch is not even compiled in). The setting is not observable, so the
 * switch keeps its own copy for Compose.
 */
@Composable
private fun InsecureSwitch(form: SignInForm) {
    val settings = form.model.settings
    var allowed by remember { mutableStateOf(settings.allowInsecureHttp) }
    Row(verticalAlignment = Alignment.CenterVertically, horizontalArrangement = Arrangement.spacedBy(12.dp)) {
        Text("Allow insecure http (dev)", modifier = Modifier.weight(1f))
        Switch(
            checked = allowed,
            onCheckedChange = {
                settings.allowInsecureHttp = it
                allowed = it
            },
            enabled = !form.isBusy,
        )
    }
    if (allowed) form.insecureWarning?.let { Text(it, color = MaterialTheme.colorScheme.error) }
}

@Composable
private fun CodeStep(form: SignInForm, onSubmit: () -> Unit) {
    Text(
        if (form.useRecovery) "Enter one of your recovery codes." else "Enter the 6-digit code from your authenticator app.",
        style = MaterialTheme.typography.bodyLarge,
    )
    OutlinedTextField(
        value = form.code,
        onValueChange = { form.code = it },
        label = { Text(if (form.useRecovery) "Recovery code" else "Code") },
        singleLine = true,
        enabled = !form.isBusy,
        keyboardOptions = KeyboardOptions(
            keyboardType = if (form.useRecovery) KeyboardType.Text else KeyboardType.Number,
        ),
        modifier = Modifier.fillMaxWidth().let {
            // "2faAppOTPCode" is `androidx.autofill`'s HintConstants.AUTOFILL_HINT_2FA_APP_OTP: the
            // hint for an authenticator app's code. `SmsOtpCode` would be wrong (Brook sends no
            // SMS), and one string does not justify the androidx.autofill dependency. A recovery
            // code is not something a password manager offers by hint, so it gets none.
            if (form.useRecovery) it else it.semantics { contentType = ContentType("2faAppOTPCode") }
        },
    )
    SubmitButton("Verify", busy = form.isBusy, onClick = onSubmit)
    TextButton(onClick = { form.useRecovery = !form.useRecovery; form.code = "" }, enabled = !form.isBusy) {
        Text(if (form.useRecovery) "Use the authenticator code instead" else "Use a recovery code instead")
    }
    TextButton(onClick = form::back) { Text("Back") }
}

/** Disabled while busy, with a spinner in place of nothing, so a second tap cannot double-submit. */
@Composable
private fun SubmitButton(text: String, busy: Boolean, onClick: () -> Unit) {
    Button(onClick = onClick, enabled = !busy, modifier = Modifier.fillMaxWidth()) {
        if (busy) {
            CircularProgressIndicator(modifier = Modifier.size(18.dp), strokeWidth = 2.dp)
        } else {
            Text(text)
        }
    }
}
