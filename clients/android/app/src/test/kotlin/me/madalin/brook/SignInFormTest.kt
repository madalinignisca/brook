// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Test
import uniffi.brook_ffi.LoginException
import uniffi.brook_ffi.LoginResult

/**
 * Port of the Mac's `LoginFormTests` into [SignInForm]. These run in a plain JVM test: Compose's
 * `mutableStateOf` works outside a composition, so no fallback to `MutableStateFlow` was needed.
 */
class SignInFormTest {
    private fun TestScope.form(
        result: Result<LoginResult>,
        settings: Settings = settingsWith(),
    ): SignInForm {
        val recorder = FactoryRecorder { FakeClient(result) }
        return SignInForm(sessionModel(settings, recorder, backgroundScope)).also {
            it.server = "https://chat.example.com"
            it.handle = "alice"
            it.password = "secret"
        }
    }

    @Test
    fun passwordClearedAfterSuccessfulLogin() = runTest {
        val form = form(Result.success(LoginResult.LoggedIn(aliceSession)))
        form.submit()
        assertEquals("", form.password)
        assertEquals(Phase.SignedIn(alice), form.model.phase.value)
    }

    @Test
    fun passwordClearedAfterTheServerRejectsIt() = runTest {
        val form = form(Result.failure(LoginException.Api("auth.invalid_credentials", "no")))
        form.submit()
        assertEquals("", form.password)
        assertEquals("Wrong handle or password.", form.error)
    }

    @Test
    fun passwordKeptWhenTheFormIsRejectedLocally() = runTest {
        val form = form(Result.success(LoginResult.LoggedIn(aliceSession)))
        form.handle = ""
        form.submit()
        assertEquals("secret", form.password)
    }

    @Test
    fun prefillsTheSavedServer() = runTest {
        val recorder = FactoryRecorder { FakeClient(Result.failure(LoginException.UnexpectedResponse())) }
        val model = sessionModel(settingsWith(lastServer = "https://saved.example"), recorder, backgroundScope)
        assertEquals("https://saved.example", SignInForm(model).server)
    }

    @Test
    fun insecureWarningOnlyWhenOptedIn() = runTest {
        val failure = Result.failure<LoginResult>(LoginException.UnexpectedResponse())
        assertNull(form(failure).insecureWarning)
        val optedIn = settingsWith().also { it.allowInsecureHttp = true }
        assertNotNull(form(failure, optedIn).insecureWarning)
    }
}
