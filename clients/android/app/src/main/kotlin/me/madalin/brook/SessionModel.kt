// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.async
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import uniffi.brook_ffi.AuthStateListener
import uniffi.brook_ffi.FfiAuthState
import uniffi.brook_ffi.FfiBrookClient
import uniffi.brook_ffi.FfiKeySlot
import uniffi.brook_ffi.FfiRestoreOutcome
import uniffi.brook_ffi.FfiTotpChallenge
import uniffi.brook_ffi.FfiUser
import uniffi.brook_ffi.LoginException
import uniffi.brook_ffi.LoginResult
import uniffi.brook_ffi.Subscription

/** What the app is doing about the session; the screens are a `when` over this. */
sealed interface Phase {
    /** At launch: signing in with the stored session ("Signing in...", no form). */
    data object Restoring : Phase
    data class SignedOut(val error: String?) : Phase
    data object SigningIn : Phase

    /** The password was right; the account's TOTP code (or a recovery code) comes next. */
    data class NeedsCode(val error: String?) : Phase
    data class SignedIn(val user: FfiUser) : Phase
}

/**
 * The app's single owner of the Rust core client: sign in, TOTP, sign out, restore at launch,
 * and following core when it loses the session by itself (a remote sign-out: a password
 * change elsewhere, an admin reset). Port of the Mac's `SessionStore`, minus local data, the
 * second-instance lock and the macOS-only wording.
 *
 * Every sign-in is an *attempt* with its own number. Core's auth events for an attempt arrive
 * from a Rust thread, are numbered as they arrive, and are consumed in order by one coroutine.
 * Core's subscription keeps only the latest value (a quick `LoggedIn` then `LoggedOut` can
 * arrive as just `LoggedOut`), so the login's completion reads core's state directly and
 * settles every event numbered before it; any `LoggedOut` after that is a remote sign-out only
 * if core still says so. Anything that arrives for an attempt that is no longer current (a
 * dropped client's late event, a stale login result) is ignored, and the first transition out
 * of signed-in wins.
 *
 * Everything here runs on [scope]'s thread (the main thread in the app). That is safe because
 * the core calls that can block are `suspend` functions (they run on Rust's own runtime), and
 * the plain ones used here (`authState`, `signOutComplete`, `enablePersistence`, `subscribe`)
 * only read or set in-memory state. [dataDir] must be under `noBackupFilesDir` (core keeps its
 * sign-out fences there).
 *
 * A new client is made for every attempt, as on the Mac and as core's persistence assumes: the
 * newest client owns the stored session.
 */
class SessionModel(
    val settings: Settings,
    private val slot: FfiKeySlot,
    private val dataDir: String,
    // The class, not `FfiBrookClientInterface`: `close()` comes from `AutoCloseable` on the class.
    private val makeClient: (server: String, allowInsecureHttp: Boolean) -> FfiBrookClient,
    private val scope: CoroutineScope,
) {
    object Message {
        const val missingFields = "Enter your handle and password."
        const val invalidAddress = "That server address isn't valid."
        const val notJustAnAddress = "Enter just the server address, like https://chat.example.com"
        const val wrongCredentials = "Wrong handle or password."
        const val rateLimited = "Too many attempts. Wait a moment and try again."
        const val unreachable = "Couldn't reach the server. Check the address."
        const val insecure = "The server address must start with https://"
        const val unexpected = "The server sent an unexpected response."
        const val signedOut = "You're signed out. Sign in again."
        const val wrongCode = "Wrong or already-used code. Wait for the next one."
        const val wrongRecoveryCode = "That recovery code is wrong or already used. Try another one."
        const val codeStepExpired = "That took too long. Enter your password again."
        const val codeFormat = "Enter the 6-digit code from your authenticator app."
        const val recoveryFormat = "Enter one of your recovery codes."
        const val storedSessionUnreadable = "Your saved sign-in couldn't be read. Sign in again."
        const val restoreOffline =
            "Couldn't reach the server to resume your session. It's kept for next time; you can also sign in again."
        const val signOutIncomplete =
            "This phone couldn't forget your saved sign-in, so Brook may sign you in again at the next launch. " +
                "Sign in and out again to retry."
    }

    // Persistence is always on, so a remembered server means there may be a stored session.
    // Start on "Signing in..." rather than flash the form the restore may replace.
    private val _phase = MutableStateFlow<Phase>(
        if (settings.lastGoodServer != null) Phase.Restoring else Phase.SignedOut(null),
    )
    val phase: StateFlow<Phase> = _phase.asStateFlow()

    /** A code is being checked (the button stays disabled). */
    private val _codeBusy = MutableStateFlow(false)
    val codeBusy: StateFlow<Boolean> = _codeBusy.asStateFlow()

    /** Set when a sign-out couldn't make the stored session unusable; shown until a sign-in. */
    private val _signOutWarning = MutableStateFlow<String?>(null)
    val signOutWarning: StateFlow<String?> = _signOutWarning.asStateFlow()

    /** The client of the current attempt (in flight, at the code step, or signed in). */
    private var current: FfiBrookClient? = null

    /** The signed-in client: later phases talk to the server through it. */
    val client: FfiBrookClient? get() = if (_phase.value is Phase.SignedIn) current else null

    /**
     * The conversations of the signed-in user: made when the sign-in completes, stopped when the
     * attempt ends. Set before [phase] becomes `SignedIn`, so a screen showing that phase can
     * rely on it.
     */
    var channelList: ChannelListModel? = null
        private set

    /** The current attempt; bumping it ends the previous one. */
    private var attempt = 0
    private var subscription: Subscription? = null
    private var events: Job? = null
    private var delivery: Delivery? = null

    /** Events numbered up to this were settled by the login's completion (it read core's state). */
    private var settled = Int.MAX_VALUE

    /** Completed sign-ins, counted: a sign-out's late result applies only if none came after. */
    private var signIns = 0

    /** The launch restore runs at most once per process. */
    private var restoreStarted = false

    /** The code step: the client that proved the password, and its challenge. */
    private class PendingCode(
        val client: FfiBrookClient,
        val challenge: FfiTotpChallenge,
        val address: String,
        val attempt: Int,
    )

    private var pending: PendingCode? = null

    /**
     * At launch, with the last server: sign in with the stored session. It is an attempt like
     * a sign-in, so a sign-out meanwhile wins and its late result is ignored.
     */
    suspend fun restoreAtLaunch() {
        val address = settings.lastGoodServer
        if (_phase.value != Phase.Restoring || restoreStarted || address == null) return
        restoreStarted = true
        attempt++
        val mine = attempt
        val client = try {
            clientFor(address)
        } catch (_: Exception) {
            showForm(null)
            return
        }
        follow(client, mine)
        val outcome = client.restore()
        if (mine != attempt) return
        when (outcome) {
            is FfiRestoreOutcome.LoggedIn -> finishSignIn(client, outcome.user, address)
            FfiRestoreOutcome.NotSignedIn -> showForm(null)
            FfiRestoreOutcome.Unavailable -> showForm(Message.storedSessionUnreadable)
            // The stored session is kept: the next launch tries again.
            FfiRestoreOutcome.Offline -> showForm(Message.restoreOffline)
            // Core's newer attempt owns the session; it isn't ours to report on.
            FfiRestoreOutcome.Superseded -> showForm(null)
        }
    }

    /**
     * The password is used exactly as typed: the server hashes it verbatim. Returns whether a
     * login was actually attempted (false: rejected locally or ignored).
     */
    suspend fun signIn(server: String, handle: String, password: String): Boolean {
        val now = _phase.value
        if (now is Phase.SigningIn || now is Phase.Restoring) return false
        val handle = handle.trim()
        if (handle.isEmpty() || password.isEmpty()) {
            _phase.value = Phase.SignedOut(Message.missingFields)
            return false
        }
        val address = when (val parsed = ServerAddress.parse(server)) {
            is ServerAddress.Parsed.Ok -> parsed.address
            is ServerAddress.Parsed.Bad -> {
                _phase.value = Phase.SignedOut(
                    if (parsed.problem == ServerAddress.Problem.NotJustAnAddress) {
                        Message.notJustAnAddress
                    } else {
                        Message.invalidAddress
                    },
                )
                return false
            }
        }

        _phase.value = Phase.SigningIn
        // A new attempt replaces any earlier one still held (the code step's client): end it
        // first, or its client and subscription would leak when `current` is overwritten.
        end()?.close()
        val mine = attempt
        // Runs in the model's scope, not the caller's: a screen's scope dies with the Activity
        // (a rotation), and a login cancelled halfway would leave the phase on SigningIn
        // forever, refusing every retry. If the caller is cancelled, only its wait ends.
        scope.async { attemptLogin(address, handle, password, mine) }.await()
        return true
    }

    private suspend fun attemptLogin(address: String, handle: String, password: String, mine: Int) {
        try {
            val client = clientFor(address)
            follow(client, mine)
            val result = client.login(handle, password)
            if (mine != attempt) return // ended meanwhile (a sign-out won)
            when (result) {
                is LoginResult.LoggedIn -> finishSignIn(client, result.session.user, address)
                is LoginResult.TotpRequired -> {
                    // The password was right; nothing is signed in until the code step succeeds.
                    pending = PendingCode(client, result.challenge, address, mine)
                    _phase.value = Phase.NeedsCode(null)
                }
            }
        } catch (e: CancellationException) {
            throw e
        } catch (e: Exception) {
            if (mine != attempt) return
            showForm(if (e is LoginException) message(e) else Message.unexpected)
        }
    }

    /** The 6-digit code (spaces allowed, as pasted from "123 456"). */
    suspend fun submitCode(code: String) {
        val digits = code.filterNot { it.isWhitespace() }
        if (digits.length != 6 || !digits.all { it in '0'..'9' }) {
            if (_phase.value is Phase.NeedsCode) _phase.value = Phase.NeedsCode(Message.codeFormat)
            return
        }
        complete(recovery = false) { client, challenge -> client.completeTotp(challenge, digits) }
    }

    /** A recovery code instead of the 6-digit code. */
    suspend fun submitRecovery(code: String) {
        val trimmed = code.trim()
        if (trimmed.isEmpty()) {
            if (_phase.value is Phase.NeedsCode) _phase.value = Phase.NeedsCode(Message.recoveryFormat)
            return
        }
        complete(recovery = true) { client, challenge -> client.completeRecovery(challenge, trimmed) }
    }

    /** Back to the password: this challenge ends (core refuses it from now on). */
    fun back() {
        val code = pending
        if (_phase.value !is Phase.NeedsCode || code == null) return
        val old = end()
        _phase.value = Phase.SignedOut(null)
        scope.launch {
            try {
                code.client.cancelTotp(code.challenge)
            } finally {
                old?.close()
            }
        }
    }

    // The recovery-codes-left count that both calls return is ignored: no UI for it in this scope.
    private suspend fun complete(
        recovery: Boolean,
        call: suspend (FfiBrookClient, FfiTotpChallenge) -> UInt?,
    ) {
        val code = pending
        if (_phase.value !is Phase.NeedsCode || code == null || _codeBusy.value) return
        _codeBusy.value = true
        // In the model's scope for the same reason as the login in [signIn].
        scope.async { checkCode(code, recovery, call) }.await()
    }

    private suspend fun checkCode(
        code: PendingCode,
        recovery: Boolean,
        call: suspend (FfiBrookClient, FfiTotpChallenge) -> UInt?,
    ) {
        try {
            call(code.client, code.challenge)
            if (code.attempt != attempt) return
            pending = null
            val state = code.client.authState()
            if (state !is FfiAuthState.LoggedIn) {
                showForm(Message.signedOut)
                return
            }
            finishSignIn(code.client, state.user, code.address)
        } catch (e: CancellationException) {
            throw e
        } catch (_: LoginException.ChallengeSuperseded) {
            // Back, a newer attempt or a sign-out already decided what shows.
        } catch (e: Exception) {
            if (code.attempt != attempt) return
            when {
                e is LoginException.Api && e.code == "auth.invalid_code" ->
                    _phase.value = Phase.NeedsCode(if (recovery) Message.wrongRecoveryCode else Message.wrongCode)
                e is LoginException.Api && e.code == "auth.totp_expired" -> showForm(Message.codeStepExpired)
                e is LoginException -> _phase.value = Phase.NeedsCode(message(e))
                else -> _phase.value = Phase.NeedsCode(Message.unexpected)
            }
        } finally {
            _codeBusy.value = false
        }
    }

    /**
     * Account menu -> Sign out. The attempt ends first, so a remote `LoggedOut` arriving after it
     * changes nothing (the user just chose to sign out; no message needed).
     */
    fun signOut() {
        if (_phase.value !is Phase.SignedIn) return
        val client = end() ?: return
        _phase.value = Phase.SignedOut(null)
        val before = signIns
        scope.launch {
            val incomplete = try {
                client.logout() // core forgets the stored copy, then revokes (best effort)
                // Both the Keystore delete and core's fence failed: the next launch could sign in
                // again. Read before `close()`: a closed client refuses every call.
                !client.signOutComplete()
            } finally {
                // The last call on this client: free the Rust client and its socket now, not at GC.
                client.close()
            }
            // A sign-in completed since replaced the stored copy: then the result is moot. It is
            // its own value, not the form's error: typing into the form meanwhile must not hide it.
            if (before == signIns && incomplete) _signOutWarning.value = Message.signOutIncomplete
        }
    }

    /** The client for `address`, with persistence on before anything signs in or restores. */
    private fun clientFor(address: String): FfiBrookClient {
        val client = makeClient(address, settings.allowInsecureHttp)
        try {
            client.enablePersistence(slot, dataDir)
        } catch (e: Exception) {
            // Nothing owns this client yet (`current` is not set), so free it here.
            client.close()
            throw e
        }
        current = client
        return client
    }

    /** Signed in: from here on core's state is the truth (the stream may skip states). */
    private fun finishSignIn(client: FfiBrookClient, user: FfiUser, address: String) {
        settled = delivery?.count() ?: 0
        if (client.authState() is FfiAuthState.LoggedOut) {
            showForm(Message.signedOut)
            return
        }
        signIns++
        _signOutWarning.value = null // the new sign-in replaced the stored copy
        settings.lastGoodServer = address
        channelList = ChannelListModel(client, user.id, scope).also { it.start() }
        _phase.value = Phase.SignedIn(user)
    }

    /** End the attempt and show the sign-in form (with `error`), closing the dead client. */
    private fun showForm(error: String?) {
        end()?.close()
        _phase.value = Phase.SignedOut(error)
    }

    /** Core's auth events for `attempt`, in order, on [scope]. */
    private fun follow(client: FfiBrookClient, mine: Int) {
        val delivery = Delivery()
        this.delivery = delivery
        settled = Int.MAX_VALUE // nothing counts until the login completes
        subscription = client.subscribe(object : AuthStateListener {
            override fun onState(state: FfiAuthState) = delivery.deliver(state)
        })
        events = scope.launch {
            for ((n, state) in delivery.events) {
                if (mine != attempt) return@launch
                // Before or at the completion's settle point: already accounted for.
                if (n <= settled || state !is FfiAuthState.LoggedOut) continue
                // Delivery order says nothing about when core published it (a fresh client's
                // initial LoggedOut can be delivered late): only core's state now decides.
                if (client.authState() !is FfiAuthState.LoggedOut) continue
                showForm(Message.signedOut)
                return@launch
            }
        }
    }

    /**
     * End the current attempt: nothing that arrives for it applies any more. Returns the old
     * client, which the caller closes once nothing more will call it.
     */
    private fun end(): FfiBrookClient? {
        attempt++
        // Stopped before the caller closes the client: the list model calls it.
        channelList?.stop()
        channelList = null
        subscription?.let {
            it.cancel()
            it.close()
        }
        subscription = null
        events?.cancel()
        events = null
        delivery?.close()
        delivery = null
        settled = Int.MAX_VALUE
        pending = null
        return current.also { current = null }
    }

    private fun message(error: LoginException): String = when (error) {
        is LoginException.Api -> when (error.code) {
            "auth.invalid_credentials" -> Message.wrongCredentials
            "auth.rate_limited" -> Message.rateLimited
            else -> error.detail
        }
        is LoginException.Network -> Message.unreachable
        is LoginException.InsecureServerUrl -> Message.insecure
        is LoginException.InvalidServerUrl -> Message.invalidAddress
        is LoginException.UnexpectedResponse -> Message.unexpected
        is LoginException.NotAuthenticated -> Message.signedOut
        is LoginException.ChallengeSuperseded -> Message.unexpected
        is LoginException.Disconnected, is LoginException.Timeout -> Message.unreachable
        is LoginException.CallEnded, is LoginException.Busy, is LoginException.TooLarge -> Message.unexpected
    }
}

/**
 * Numbers auth events in delivery order. Core calls [deliver] on a Rust worker thread; the
 * number and the send happen under one lock, so the channel's order and the numbers agree
 * (the Mac's `DeliveryCount`). The channel is unlimited so the Rust thread never waits on us.
 */
private class Delivery {
    private val lock = Any()
    private var delivered = 0
    val events = Channel<Pair<Int, FfiAuthState>>(Channel.UNLIMITED)

    fun deliver(state: FfiAuthState) {
        synchronized(lock) {
            delivered++
            events.trySend(delivered to state)
        }
    }

    fun count(): Int = synchronized(lock) { delivered }

    /** After this a late callback from the dropped client is silently discarded. */
    fun close() {
        events.close()
    }
}
