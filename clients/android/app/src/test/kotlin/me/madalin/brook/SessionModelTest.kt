// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.launch
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotNull
import org.junit.Assert.assertNull
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.brook_ffi.FfiAuthState
import uniffi.brook_ffi.LoginException
import uniffi.brook_ffi.LoginResult

/**
 * Port of the Mac's `SessionStoreTests` (clients/apple-shared/BrookTests), by name: the `test`
 * prefix is dropped and the first letter lowered. Not ported: the LAN-permission wording
 * (macOS only) and the "few recovery codes left" flag (no UI for it in this scope). The Mac
 * waits with real sleeps; here the test scheduler runs the model's coroutines on demand
 * (`runCurrent`), so nothing sleeps and nothing is racy.
 *
 * The model's scope is `backgroundScope`: its event consumer never ends by itself, and
 * `runTest` would otherwise wait for it forever.
 */
@OptIn(ExperimentalCoroutinesApi::class)
class SessionModelTest {
    private fun loggedIn() = FakeClient(Result.success(LoginResult.LoggedIn(aliceSession)))

    private fun TestScope.model(
        client: FakeClient,
        settings: Settings = settingsWith(),
    ): Pair<SessionModel, FactoryRecorder> {
        val recorder = FactoryRecorder { client }
        return sessionModel(settings, recorder, backgroundScope) to recorder
    }

    private val SessionModel.state get() = phase.value

    // MARK: sign in

    @Test
    fun emptyHandleOrPasswordNeverReachesTheClient() = runTest {
        val (model, recorder) = model(loggedIn())
        model.signIn("https://h", "  ", "pw")
        assertEquals(Phase.SignedOut(SessionModel.Message.missingFields), model.state)
        model.signIn("https://h", "alice", "")
        assertEquals(Phase.SignedOut(SessionModel.Message.missingFields), model.state)
        assertTrue(recorder.all.isEmpty())
    }

    @Test
    fun passwordIsPassedExactlyWhileHandleAndServerAreTrimmed() = runTest {
        val fake = loggedIn()
        val (model, recorder) = model(fake)
        model.signIn(" https://h \n", " alice ", " p w ")
        assertEquals(listOf(FakeClient.Login("alice", " p w ")), fake.logins)
        assertEquals(listOf("https://h"), recorder.all.map { it.server })
    }

    @Test
    fun addressWithCredentialsIsRejectedBeforeAnythingHappens() = runTest {
        val settings = settingsWith()
        val (model, recorder) = model(loggedIn(), settings)
        for (bad in listOf("https://u:secret@h", "https://h?x=1", "https://h#f")) {
            model.signIn(bad, "alice", "pw")
            assertEquals(bad, Phase.SignedOut(SessionModel.Message.notJustAnAddress), model.state)
        }
        assertTrue(recorder.all.isEmpty())
        assertNull(settings.lastGoodServer)
    }

    @Test
    fun secondSignInWhileSigningInIsIgnored() = runTest {
        val fake = FakeClient(Result.success(LoginResult.LoggedIn(aliceSession)), gated = true)
        val (model, _) = model(fake)
        val first = launch { model.signIn("https://h", "alice", "pw") }
        runCurrent()
        assertEquals(Phase.SigningIn, model.state)
        model.signIn("https://h", "alice", "pw")
        fake.release()
        first.join()
        assertEquals(1, fake.logins.size)
    }

    @Test
    fun canRetryAfterAFailedLogin() = runTest {
        val wrong = LoginException.Api("auth.invalid_credentials", "Invalid handle or password")
        val fake = FakeClient(Result.failure(wrong))
        val (model, _) = model(fake)
        model.signIn("https://h", "alice", "bad")
        model.signIn("https://h", "alice", "bad2")
        assertEquals(2, fake.logins.size)
    }

    @Test
    fun constructorErrorIsShownAndLoginNeverCalled() = runTest {
        val recorder = FactoryRecorder { throw LoginException.InsecureServerUrl() }
        val model = sessionModel(settingsWith(), recorder, backgroundScope)
        model.signIn("http://h.example", "alice", "pw")
        assertEquals(Phase.SignedOut(SessionModel.Message.insecure), model.state)
        assertEquals(1, recorder.all.size)
    }

    @Test
    fun errorMessages() = runTest {
        val cases = listOf(
            LoginException.Api("auth.invalid_credentials", "x") to "Wrong handle or password.",
            // A code the app has no wording for shows the server's own text.
            LoginException.Api("some.other_code", "Slow down") to "Slow down",
            LoginException.Network("refused") to SessionModel.Message.unreachable,
            LoginException.InvalidServerUrl("x") to SessionModel.Message.invalidAddress,
            LoginException.UnexpectedResponse() to SessionModel.Message.unexpected,
            LoginException.NotAuthenticated() to SessionModel.Message.signedOut,
        )
        for ((error, expected) in cases) {
            val (model, _) = model(FakeClient(Result.failure(error)))
            model.signIn("https://chat.example.com", "alice", "pw")
            assertEquals("$error", Phase.SignedOut(expected), model.state)
        }
    }

    @Test
    fun rateLimitedHasItsOwnMessage() = runTest {
        val (model, _) = model(FakeClient(Result.failure(LoginException.Api("auth.rate_limited", "Too many attempts"))))
        model.signIn("https://h", "alice", "pw")
        assertEquals(Phase.SignedOut("Too many attempts. Wait a moment and try again."), model.state)
    }

    @Test
    fun serverIsSavedOnlyAfterSuccess() = runTest {
        val settings = settingsWith()
        val fake = FakeClient(Result.success(LoginResult.LoggedIn(aliceSession)), gated = true)
        // Built before the server is remembered: a model that knows a server starts in Restoring,
        // which ignores sign-ins.
        val (model, _) = model(fake, settings)
        settings.lastGoodServer = "https://previous"
        val signIn = launch { model.signIn("https://new", "alice", "pw") }
        runCurrent()
        assertEquals("https://previous", settings.lastGoodServer)
        fake.release()
        signIn.join()
        assertEquals("https://new", settings.lastGoodServer)
        assertEquals(Phase.SignedIn(alice), model.state)
    }

    @Test
    fun failedLoginDoesNotSaveTheServer() = runTest {
        val settings = settingsWith()
        val (model, _) = model(FakeClient(Result.failure(LoginException.UnexpectedResponse())), settings)
        model.signIn("https://new", "alice", "pw")
        assertNull(settings.lastGoodServer)
    }

    @Test
    fun factoryReceivesTheResolvedInsecureFlag() = runTest {
        val off = settingsWith()
        val on = settingsWith().also { it.allowInsecureHttp = true }
        val (offModel, offRecorder) = model(loggedIn(), off)
        offModel.signIn("https://h", "alice", "pw")
        val (onModel, onRecorder) = model(loggedIn(), on)
        onModel.signIn("https://h", "alice", "pw")
        assertEquals(listOf(false), offRecorder.all.map { it.allowInsecureHttp })
        assertEquals(listOf(true), onRecorder.all.map { it.allowInsecureHttp })
    }

    /** A release build can never ask core for plain http, whatever the stored switch says. */
    @Test
    fun releaseBuildNeverPassesInsecureHttp() = runTest {
        val prefs = FakePrefs()
        Settings(prefs, isDebugBuild = true).allowInsecureHttp = true
        val (model, recorder) = model(loggedIn(), Settings(prefs, isDebugBuild = false))
        model.signIn("https://h", "alice", "pw")
        assertEquals(listOf(false), recorder.all.map { it.allowInsecureHttp })
    }

    // MARK: sign out, and following a remote sign-out

    private suspend fun TestScope.signedIn(fake: FakeClient): SessionModel {
        val (model, _) = model(fake)
        model.signIn("https://h", "alice", "pw")
        fake.emit(FfiAuthState.LoggedIn(alice))
        runCurrent()
        assertEquals(Phase.SignedIn(alice), model.state)
        return model
    }

    @Test
    fun signOutEndsTheSessionQuietlyAndSignsOutOfCore() = runTest {
        val fake = loggedIn()
        val model = signedIn(fake)
        model.signOut()
        assertEquals(Phase.SignedOut(null), model.state)
        assertNull(model.client)
        runCurrent()
        assertEquals("core never signed out", 1, fake.logouts)
    }

    /** The Rust client and its socket go now, not at GC time, but only after its last call. */
    @Test
    fun signOutClosesTheClientAfterItsLastCall() = runTest {
        val fake = loggedIn()
        val model = signedIn(fake)
        fake.gateLogouts()
        model.signOut()
        runCurrent()
        assertEquals("closed while logout was still running", 0, fake.closes)
        fake.releaseLogouts()
        runCurrent()
        assertEquals(listOf("logout", "signOutComplete", "close"), fake.log.drop(fake.log.indexOf("logout")))
    }

    @Test
    fun aRemoteSignOutShowsTheSignInScreenWithTheMessage() = runTest {
        val fake = loggedIn()
        val model = signedIn(fake)
        fake.coreState = FfiAuthState.LoggedOut
        fake.emit(FfiAuthState.LoggedOut)
        runCurrent()
        assertEquals(Phase.SignedOut(SessionModel.Message.signedOut), model.state)
        assertNull(model.client)
        assertEquals("the dead client was not closed", 1, fake.closes)
    }

    @Test
    fun aFreshClientsInitialLoggedOutIsNotASignOut() = runTest {
        val fake = FakeClient(Result.success(LoginResult.LoggedIn(aliceSession)), gated = true)
        val (model, _) = model(fake)
        val signIn = launch { model.signIn("https://h", "alice", "pw") }
        runCurrent()
        fake.emit(FfiAuthState.LoggedOut) // core's initial state, before any sign-in
        fake.release()
        signIn.join()
        runCurrent()
        assertEquals(Phase.SignedIn(alice), model.state)
    }

    /**
     * Core signed in and then lost the session before the app handled its own login result:
     * that sign-out wins, and the late login result is ignored.
     */
    @Test
    fun aSignOutBeforeTheLoginResultIsHandledWins() = runTest {
        val fake = FakeClient(Result.success(LoginResult.LoggedIn(aliceSession)), gated = true)
        val (model, _) = model(fake)
        val signIn = launch { model.signIn("https://h", "alice", "pw") }
        runCurrent()
        fake.emit(FfiAuthState.LoggedIn(alice))
        fake.emit(FfiAuthState.LoggedOut)
        fake.coreState = FfiAuthState.LoggedOut
        fake.release()
        signIn.join()
        assertEquals(Phase.SignedOut(SessionModel.Message.signedOut), model.state)
        assertNull(model.client)
    }

    /**
     * The subscription keeps only the latest value: a LoggedIn then LoggedOut can arrive as just
     * LoggedOut. The app reads core's state when its login completes, so this sign-out is
     * caught, never mistaken for a fresh client's initial state.
     */
    @Test
    fun aCoalescedSignOutDuringSignInIsCaught() = runTest {
        val fake = FakeClient(Result.success(LoginResult.LoggedIn(aliceSession)), gated = true)
        val (model, _) = model(fake)
        val signIn = launch { model.signIn("https://h", "alice", "pw") }
        runCurrent()
        fake.emit(FfiAuthState.LoggedOut) // the LoggedIn before it was coalesced away
        fake.coreState = FfiAuthState.LoggedOut
        fake.release()
        signIn.join()
        assertEquals(Phase.SignedOut(SessionModel.Message.signedOut), model.state)
        assertNull(model.client)
    }

    /**
     * A LoggedOut delivered late (the fresh client's initial state, held up in delivery) while
     * core is in fact signed in: not a sign-out.
     */
    @Test
    fun aLateInitialLoggedOutNeverEndsAGoodSession() = runTest {
        val fake = loggedIn()
        val model = signedIn(fake) // core's state: LoggedIn
        fake.emit(FfiAuthState.LoggedOut)
        runCurrent()
        assertEquals(Phase.SignedIn(alice), model.state)
    }

    @Test
    fun aRemoteSignOutAfterTheUsersOwnHasNoMessage() = runTest {
        val fake = loggedIn()
        val model = signedIn(fake)
        model.signOut()
        fake.emit(FfiAuthState.LoggedOut)
        runCurrent()
        assertEquals(Phase.SignedOut(null), model.state)
    }

    @Test
    fun aLoggedOutFromThePreviousClientNeverSignsOutTheNextOne() = runTest {
        val first = loggedIn()
        val second = loggedIn()
        val clients = mutableListOf<FakeClient>(first, second)
        val recorder = FactoryRecorder { clients.removeAt(0) }
        val model = sessionModel(settingsWith(), recorder, backgroundScope)
        model.signIn("https://h", "alice", "pw")
        first.emit(FfiAuthState.LoggedIn(alice))
        model.signOut()
        model.signIn("https://h", "alice", "pw")
        second.emit(FfiAuthState.LoggedIn(alice))
        first.emit(FfiAuthState.LoggedOut) // late, from the dropped client
        runCurrent()
        assertEquals(Phase.SignedIn(alice), model.state)
    }

    // MARK: TOTP second step

    private suspend fun TestScope.atCodeStep(): Pair<SessionModel, FakeClient> {
        val fake = FakeClient(Result.success(LoginResult.TotpRequired(FakeChallenge())))
        fake.coreState = FfiAuthState.Authenticating
        val (model, _) = model(fake)
        model.signIn("https://h", "alice", "pw")
        return model to fake
    }

    @Test
    fun thePasswordAloneLeadsToTheCodeStep() = runTest {
        val (model, _) = atCodeStep()
        assertEquals(Phase.NeedsCode(null), model.state)
        assertNull("signed in with the password alone", model.client)
    }

    @Test
    fun theRightCodeSignsIn() = runTest {
        val (model, fake) = atCodeStep()
        model.submitCode("123 456")
        assertEquals(listOf("code:123456"), fake.totpCalls)
        assertEquals(Phase.SignedIn(alice), model.state)
        assertNotNull(model.client)
    }

    @Test
    fun aWrongCodeStaysOnTheCodeStepAndSaysSo() = runTest {
        val (model, fake) = atCodeStep()
        fake.totpResult = Result.failure(LoginException.Api("auth.invalid_code", "x"))
        model.submitCode("000000")
        assertEquals(Phase.NeedsCode(SessionModel.Message.wrongCode), model.state)
        model.submitRecovery("aaaa-bbbb")
        assertEquals(Phase.NeedsCode(SessionModel.Message.wrongRecoveryCode), model.state)
    }

    @Test
    fun anExpiredChallengeGoesBackToThePassword() = runTest {
        val (model, fake) = atCodeStep()
        fake.totpResult = Result.failure(LoginException.Api("auth.totp_expired", "x"))
        model.submitCode("123456")
        assertEquals(Phase.SignedOut(SessionModel.Message.codeStepExpired), model.state)
    }

    @Test
    fun backCancelsTheChallenge() = runTest {
        val (model, fake) = atCodeStep()
        model.back()
        assertEquals(Phase.SignedOut(null), model.state)
        runCurrent()
        assertEquals(1, fake.cancels)
        assertEquals("the client is closed once the challenge is cancelled", 1, fake.closes)
    }

    @Test
    fun aSupersededChallengeChangesNothing() = runTest {
        val (model, fake) = atCodeStep()
        fake.totpResult = Result.failure(LoginException.ChallengeSuperseded())
        model.submitCode("123456")
        assertEquals(Phase.NeedsCode(null), model.state)
    }

    @Test
    fun aMalformedCodeNeverReachesTheClient() = runTest {
        val (model, fake) = atCodeStep()
        model.submitCode("12345")
        assertEquals(Phase.NeedsCode(SessionModel.Message.codeFormat), model.state)
        model.submitRecovery("   ")
        assertEquals(Phase.NeedsCode(SessionModel.Message.recoveryFormat), model.state)
        assertTrue(fake.totpCalls.isEmpty())
    }
}
