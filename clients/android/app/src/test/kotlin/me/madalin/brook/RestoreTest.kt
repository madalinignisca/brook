// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.launch
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertNull
import org.junit.Assert.assertSame
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.brook_ffi.FfiAuthState
import uniffi.brook_ffi.FfiRestoreOutcome
import uniffi.brook_ffi.LoginException
import uniffi.brook_ffi.LoginResult

/**
 * Port of the Mac's `RestoreTests`: staying signed in, and the launch restore. Persistence is
 * always on in Android, so the Mac's "persistence off" cases become the "no last server" case,
 * and the Keychain/instance-lock tests are not ported (none of that exists here).
 */
@OptIn(ExperimentalCoroutinesApi::class)
class RestoreTest {
    private fun TestScope.model(
        fake: FakeClient,
        lastServer: String? = "https://h",
    ): Pair<SessionModel, FactoryRecorder> {
        val recorder = FactoryRecorder { fake }
        return sessionModel(settingsWith(lastServer), recorder, backgroundScope) to recorder
    }

    private suspend fun TestScope.restored(
        outcome: FfiRestoreOutcome,
        login: Result<LoginResult> = Result.failure(LoginException.UnexpectedResponse()),
    ): Triple<SessionModel, FakeClient, FactoryRecorder> {
        val fake = FakeClient(login)
        fake.restoreOutcome = outcome
        val (model, recorder) = model(fake)
        assertEquals("the form flashed before the restore", Phase.Restoring, model.phase.value)
        model.restoreAtLaunch()
        return Triple(model, fake, recorder)
    }

    @Test
    fun aStoredSessionSignsInAtLaunchOnTheLastServer() = runTest {
        val (model, fake, recorder) = restored(FfiRestoreOutcome.LoggedIn(alice))
        assertEquals(Phase.SignedIn(alice), model.phase.value)
        assertSame(fake, model.client)
        assertEquals(listOf("https://h"), recorder.all.map { it.server })
        assertEquals("restored without persistence on", listOf("/data"), fake.persistence)
    }

    @Test
    fun nothingStoredShowsTheFormQuietly() = runTest {
        val (model, _, _) = restored(FfiRestoreOutcome.NotSignedIn)
        assertEquals(Phase.SignedOut(null), model.phase.value)
        assertNull(model.client)
    }

    /** The Mac's `ALockedKeychainSaysSo`: core says `Unavailable` when the slot can't be read. */
    @Test
    fun anUnreadableStoredSessionSaysSo() = runTest {
        val (model, _, _) = restored(FfiRestoreOutcome.Unavailable)
        assertEquals(Phase.SignedOut(SessionModel.Message.storedSessionUnreadable), model.phase.value)
    }

    @Test
    fun offlineSaysTheSessionIsKept() = runTest {
        val (model, fake, _) = restored(FfiRestoreOutcome.Offline)
        assertEquals(Phase.SignedOut(SessionModel.Message.restoreOffline), model.phase.value)
        assertNull(model.client)
        assertEquals("the stored session must be left alone, only the client closed", 1, fake.closes)
    }

    /** Core's newer attempt owns the session, so this one shows the form and says nothing. */
    @Test
    fun aSupersededRestoreShowsTheFormQuietly() = runTest {
        val (model, _, _) = restored(FfiRestoreOutcome.Superseded)
        assertEquals(Phase.SignedOut(null), model.phase.value)
        assertNull(model.client)
    }

    @Test
    fun theRestoreRunsOncePerProcess() = runTest {
        val (model, fake, _) = restored(FfiRestoreOutcome.NotSignedIn)
        model.restoreAtLaunch()
        assertEquals(1, fake.restores)
    }

    /** The activity can start the restore twice while the first is still in flight. */
    @Test
    fun aSecondCallDuringTheRestoreDoesNothing() = runTest {
        val fake = FakeClient(Result.failure(LoginException.UnexpectedResponse()), gated = true)
        fake.restoreOutcome = FfiRestoreOutcome.LoggedIn(alice)
        val (model, recorder) = model(fake)
        val first = launch { model.restoreAtLaunch() }
        runCurrent()
        model.restoreAtLaunch() // returns at once: the first one owns the launch
        fake.release()
        first.join()
        assertEquals("a second client was made for the same launch", 1, recorder.all.size)
        assertEquals(Phase.SignedIn(alice), model.phase.value)
    }

    /** Persistence cannot be off on Android, so the only reason not to restore is no server. */
    @Test
    fun noLastServerNeverRestores() = runTest {
        val fake = FakeClient(Result.failure(LoginException.UnexpectedResponse()))
        val (model, recorder) = model(fake, lastServer = null)
        assertEquals(Phase.SignedOut(null), model.phase.value)
        model.restoreAtLaunch()
        assertEquals(0, fake.restores)
        assertTrue(recorder.all.isEmpty())
    }

    /**
     * The session is stored by the client that signed in, so persistence must be on before its
     * login runs (not just at some point), or the session would never be stored.
     */
    @Test
    fun everyClientGetsPersistenceBeforeItSignsIn() = runTest {
        val fake = FakeClient(Result.success(LoginResult.LoggedIn(aliceSession)))
        val (model, _) = model(fake, lastServer = null)
        model.signIn("https://h", "alice", "pw")
        assertEquals(Phase.SignedIn(alice), model.phase.value)
        assertEquals(listOf("/data"), fake.persistence)
        assertEquals(listOf("enablePersistence", "login"), fake.log.take(2))
    }

    @Test
    fun aRemoteSignOutAfterARestoreIsFollowed() = runTest {
        val (model, fake, _) = restored(FfiRestoreOutcome.LoggedIn(alice))
        fake.coreState = FfiAuthState.LoggedOut
        fake.emit(FfiAuthState.LoggedOut)
        runCurrent()
        assertEquals(Phase.SignedOut(SessionModel.Message.signedOut), model.phase.value)
    }

    @Test
    fun aSignOutThatCouldNotForgetTheStoredCopySaysSo() = runTest {
        val (model, fake, _) = restored(FfiRestoreOutcome.LoggedIn(alice))
        fake.signOutIsComplete = false
        model.signOut()
        // Typing into the form before core answers must not hide the warning.
        model.signIn("https://h", "", "")
        runCurrent()
        assertEquals(SessionModel.Message.signOutIncomplete, model.signOutWarning.value)
        assertEquals(Phase.SignedOut(SessionModel.Message.missingFields), model.phase.value)
    }

    /** A sign-out whose result arrives after a newer sign-in: its warning is moot. */
    @Test
    fun aLateSignOutResultNeverWarnsAfterANewSignIn() = runTest {
        val (model, fake, _) = restored(
            FfiRestoreOutcome.LoggedIn(alice), login = Result.success(LoginResult.LoggedIn(aliceSession)),
        )
        fake.signOutIsComplete = false
        fake.gateLogouts()
        model.signOut()
        runCurrent()
        assertEquals(1, fake.logouts)
        model.signIn("https://h", "alice", "pw")
        assertEquals(Phase.SignedIn(alice), model.phase.value)
        fake.releaseLogouts()
        runCurrent()
        assertNull("a stale sign-out result showed its warning", model.signOutWarning.value)
    }

    @Test
    fun aSignInClearsTheSignOutWarning() = runTest {
        val (model, fake, _) = restored(
            FfiRestoreOutcome.LoggedIn(alice), login = Result.success(LoginResult.LoggedIn(aliceSession)),
        )
        fake.signOutIsComplete = false
        model.signOut()
        runCurrent()
        assertEquals(SessionModel.Message.signOutIncomplete, model.signOutWarning.value)
        fake.signOutIsComplete = true
        model.signIn("https://h", "alice", "pw")
        assertEquals(Phase.SignedIn(alice), model.phase.value)
        assertNull(model.signOutWarning.value)
    }

    @Test
    fun aCompleteSignOutStaysQuiet() = runTest {
        val (model, fake, _) = restored(FfiRestoreOutcome.LoggedIn(alice))
        model.signOut()
        runCurrent()
        assertEquals(1, fake.logouts)
        assertEquals(Phase.SignedOut(null), model.phase.value)
        assertNull(model.signOutWarning.value)
    }
}
