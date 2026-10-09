// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.CoroutineScope
import uniffi.brook_ffi.AuthStateListener
import uniffi.brook_ffi.FfiAuthState
import uniffi.brook_ffi.FfiChannel
import uniffi.brook_ffi.FfiBrookClient
import uniffi.brook_ffi.FfiKeySlot
import uniffi.brook_ffi.FfiRestoreOutcome
import uniffi.brook_ffi.FfiServerEvent
import uniffi.brook_ffi.FfiSession
import uniffi.brook_ffi.FfiTotpChallenge
import uniffi.brook_ffi.FfiUser
import uniffi.brook_ffi.LoginResult
import uniffi.brook_ffi.NoHandle
import uniffi.brook_ffi.ServerEventListener
import uniffi.brook_ffi.Subscription

// Fakes shared by the session tests. Port of the Mac's FakeClient / FactoryRecorder
// (clients/apple-shared/BrookTests). They subclass the generated classes through the
// `NoHandle` constructor UniFFI provides for tests: no Rust object exists, so every method the
// model can reach must be overridden here (an inherited one would call Rust with handle 0).
// Tests run on one thread (the test scheduler), so the fakes need no locking; only the
// model's own delivery counter is shared with a real Rust thread in production.

val alice = FfiUser(id = "u1", handle = "alice", displayName = "Alice", globalRole = "admin", statusText = null)
val aliceSession = FfiSession(user = alice)

class FakeSubscription : Subscription(NoHandle) {
    var cancels = 0
        private set
    val cancelled get() = cancels > 0

    override fun cancel() {
        cancels++
    }
}

/** A challenge with no Rust side (the model only passes it back to the client). */
class FakeChallenge : FfiTotpChallenge(NoHandle) {
    override fun secondsLeft(): ULong = 300UL
}

/** A key slot that is never called (the model only hands it to the client). */
class UnusedSlot : FfiKeySlot {
    override fun load(slot: String): ByteArray? = error("unused")
    override fun create(slot: String, bytes: ByteArray) = error("unused")
    override fun replace(slot: String, bytes: ByteArray) = error("unused")
    override fun delete(slot: String) = error("unused")
}

class FakeClient(
    private val loginResult: Result<LoginResult>,
    /** `gated`: `login` and `restore` suspend until [release], so a test sees the in-flight state. */
    gated: Boolean = false,
) : FfiBrookClient(NoHandle) {
    data class Login(val handle: String, val password: String)

    val logins = mutableListOf<Login>()

    /** Every call the model made, in order, to check sequences ("logout" before "close"). */
    val log = mutableListOf<String>()

    private val gate = CompletableDeferred<Unit>().apply { if (!gated) complete(Unit) }
    private var listener: AuthStateListener? = null

    /** The auth subscription the model made, to check it is cancelled when the attempt ends. */
    var authSubscription: FakeSubscription? = null
    var logouts = 0
        private set
    var closes = 0
        private set
    var cancels = 0
        private set
    var restores = 0
        private set
    val persistence = mutableListOf<String>()
    val totpCalls = mutableListOf<String>()
    var totpResult: Result<UInt?> = Result.success(null)
    var restoreOutcome: FfiRestoreOutcome = FfiRestoreOutcome.NotSignedIn
    var signOutIsComplete = true
    var persistenceFails = false
    private var logoutGate: CompletableDeferred<Unit>? = null
    private var totpGate: CompletableDeferred<Unit>? = null

    /** What core's state is right now (`authState()`), independent of what was delivered. */
    var coreState: FfiAuthState = FfiAuthState.LoggedIn(alice)

    fun release() {
        gate.complete(Unit)
    }

    override suspend fun login(handle: String, password: String): LoginResult {
        log += "login"
        logins += Login(handle, password)
        gate.await()
        return loginResult.getOrThrow()
    }

    /** Keeps the listener so a test can deliver core's auth states in any order. */
    override fun subscribe(listener: AuthStateListener): Subscription {
        this.listener = listener
        return FakeSubscription().also { authSubscription = it }
    }

    /** Deliver an auth state as core would (on a Rust thread in production). */
    fun emit(state: FfiAuthState) {
        listener?.onState(state)
    }

    override fun authState(): FfiAuthState = coreState

    override suspend fun completeTotp(challenge: FfiTotpChallenge, code: String): UInt? {
        totpCalls += "code:$code"
        totpGate?.await()
        if (totpResult.isSuccess) coreState = FfiAuthState.LoggedIn(alice)
        return totpResult.getOrThrow()
    }

    override suspend fun completeRecovery(challenge: FfiTotpChallenge, recoveryCode: String): UInt? {
        totpCalls += "recovery:$recoveryCode"
        if (totpResult.isSuccess) coreState = FfiAuthState.LoggedIn(alice)
        return totpResult.getOrThrow()
    }

    override suspend fun cancelTotp(challenge: FfiTotpChallenge) {
        cancels++
    }

    override suspend fun logout() {
        log += "logout"
        logouts++
        logoutGate?.await()
    }

    /** `logout` suspends until [releaseLogouts] (a sign-out whose result comes late). */
    fun gateLogouts() {
        logoutGate = CompletableDeferred()
    }

    /** `completeTotp` suspends until [releaseTotp] (a code check still in flight). */
    fun gateTotp() {
        totpGate = CompletableDeferred()
    }

    fun releaseTotp() {
        totpGate?.complete(Unit)
    }

    fun releaseLogouts() {
        logoutGate?.complete(Unit)
    }

    override fun enablePersistence(slot: FfiKeySlot, dataDir: String) {
        log += "enablePersistence"
        if (persistenceFails) error("keystore unavailable")
        persistence += dataDir
    }

    /** Persistence must already be on (core restores nothing otherwise). */
    override suspend fun restore(): FfiRestoreOutcome {
        gate.await()
        restores++
        if (persistence.isEmpty()) return FfiRestoreOutcome.NotSignedIn
        (restoreOutcome as? FfiRestoreOutcome.LoggedIn)?.let { coreState = FfiAuthState.LoggedIn(it.user) }
        return restoreOutcome
    }

    // The channel list's side of the client. Separate from `log` so the session tests' exact
    // call sequences do not change when a sign-in starts the list model.
    /** "subscribeEvents", "startRealtime", "listChannels", in call order. */
    val listCalls = mutableListOf<String>()
    var channels: List<FfiChannel> = emptyList()
    var listFails = false
    var eventSubscription: FakeSubscription? = null
    private var eventListener: ServerEventListener? = null

    override fun subscribeEvents(listener: ServerEventListener): Subscription {
        listCalls += "subscribeEvents"
        eventListener = listener
        return FakeSubscription().also { eventSubscription = it }
    }

    override suspend fun startRealtime() {
        listCalls += "startRealtime"
    }

    override suspend fun listChannels(): List<FfiChannel> {
        listCalls += "listChannels"
        if (listFails) error("offline")
        // The server's answer is fixed when the read starts, so a gated read returns what was
        // true then, as a slow real read would.
        val answer = channels
        listGate?.await()
        return answer
    }

    private var listGate: CompletableDeferred<Unit>? = null

    /** From now on `listChannels` suspends until [releaseLists] (a read still in flight). */
    fun gateLists() {
        listGate = CompletableDeferred()
    }

    fun releaseLists() {
        listGate?.complete(Unit)
    }

    /** Deliver a realtime event as core would (on a Rust thread in production). */
    fun deliver(event: FfiServerEvent) {
        eventListener?.onEvent(event)
    }

    override fun signOutComplete(): Boolean {
        log += "signOutComplete"
        return signOutIsComplete
    }

    override fun close() {
        log += "close"
        closes++
        super.close()
    }
}

/** Records what the model asked the factory for, and hands out a prepared client. */
class FactoryRecorder(private val make: () -> FfiBrookClient) {
    data class Request(val server: String, val allowInsecureHttp: Boolean)

    val all = mutableListOf<Request>()

    fun factory(server: String, allowInsecureHttp: Boolean): FfiBrookClient {
        all += Request(server, allowInsecureHttp)
        return make()
    }
}

/** Settings over an in-memory store, with the last server already remembered when given. */
fun settingsWith(lastServer: String? = null, debug: Boolean = true): Settings =
    Settings(FakePrefs(), isDebugBuild = debug).also { if (lastServer != null) it.lastGoodServer = lastServer }

fun sessionModel(
    settings: Settings,
    recorder: FactoryRecorder,
    scope: CoroutineScope,
): SessionModel = SessionModel(settings, UnusedSlot(), "/data", recorder::factory, scope)
