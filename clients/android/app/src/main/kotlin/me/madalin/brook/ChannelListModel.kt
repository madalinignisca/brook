// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import kotlinx.coroutines.launch
import uniffi.brook_ffi.FfiBrookClient
import uniffi.brook_ffi.FfiChannel
import uniffi.brook_ffi.FfiMember
import uniffi.brook_ffi.FfiServerEvent
import uniffi.brook_ffi.ServerEventListener
import uniffi.brook_ffi.Subscription
import uniffi.brook_ffi.conversationLabel
import uniffi.brook_ffi.sortKey as coreSortKey

/** One line of the list: what to show, and the channel's id (the avatar color follows it). */
data class ConversationRow(val id: String, val label: String)

/** What the channel list screen shows. */
sealed interface ListState {
    data object Loading : ListState
    data object Error : ListState

    /** Both lists are already in display order. A list with no rows is not shown at all. */
    data class Loaded(val channels: List<ConversationRow>, val dms: List<ConversationRow>) : ListState
}

/**
 * The signed-in user's conversations, kept current by core's realtime events. Plays the part of
 * the Mac's `ChannelsModel`, minus unread counts, calls and local data (none in this scope).
 *
 * Order matters at start: subscribe to events, then open the realtime connection, then read the
 * list. That way the `Ready` that opening provokes cannot be missed. Core's reconnect loop
 * retries a socket that fails to open, so a `startRealtime` error is ignored.
 *
 * Events arrive on a Rust thread. They go into an unlimited channel (so that thread never
 * waits) and one coroutine on [scope] handles them in arrival order, one at a time, including
 * the reads they cause. That is what keeps a slow read from landing after a later delete and
 * bringing the channel back.
 *
 * [label] and [sortKey] are core's functions (so every client names and orders a conversation
 * the same way); they are parameters only because the FFI library cannot be loaded in JVM unit
 * tests, which pass plain Kotlin stand-ins.
 */
class ChannelListModel(
    private val client: FfiBrookClient,
    private val meId: String,
    private val scope: CoroutineScope,
    private val label: (kind: String, name: String?, members: List<FfiMember>, me: String) -> String =
        { kind, name, members, me -> conversationLabel(kind, name, members, me, false) },
    private val sortKey: (kind: String, name: String?, members: List<FfiMember>, me: String) -> String =
        ::coreSortKey,
) {
    private val _state = MutableStateFlow<ListState>(ListState.Loading)
    val state: StateFlow<ListState> = _state.asStateFlow()

    /** The channels as the server listed them, with events applied; [_state] is built from it. */
    private var channels: List<FfiChannel> = emptyList()

    private val events = Channel<FfiServerEvent>(Channel.UNLIMITED)
    private var subscription: Subscription? = null
    private var worker: Job? = null

    /** Call once. The subscription is made before this returns; the rest happens on [scope]. */
    fun start() {
        subscription = client.subscribeEvents(object : ServerEventListener {
            override fun onEvent(event: FfiServerEvent) {
                events.trySend(event)
            }
        })
        worker = scope.launch {
            try {
                client.startRealtime()
            } catch (e: CancellationException) {
                throw e
            } catch (_: Exception) {
                // Core's reconnect loop retries; the list read below does not depend on it.
            }
            load()
            for (event in events) handle(event)
        }
    }

    /** "Try again" on the error state. A `Resync` is exactly "read the list again". */
    fun retry() {
        _state.value = ListState.Loading
        events.trySend(FfiServerEvent.Resync)
    }

    /** Ends the updates. The caller closes the client afterwards. */
    fun stop() {
        subscription?.let {
            it.cancel()
            it.close()
        }
        subscription = null
        worker?.cancel()
        worker = null
        events.close()
    }

    private suspend fun handle(event: FfiServerEvent) {
        when (event) {
            FfiServerEvent.Ready, FfiServerEvent.Resync -> load()
            is FfiServerEvent.ChannelUpdate -> {
                val updated = event.channel
                if (channels.none { it.id == updated.id }) {
                    // Not ours yet (added to a channel): only the server's list says what it is.
                    load()
                } else {
                    channels = channels.map { if (it.id == updated.id) updated else it }
                    publish()
                }
            }
            is FfiServerEvent.ChannelDelete -> {
                channels = channels.filterNot { it.id == event.channelId }
                publish()
            }
            else -> {}
        }
    }

    private suspend fun load() {
        try {
            channels = client.listChannels()
            publish()
        } catch (e: CancellationException) {
            throw e
        } catch (_: Exception) {
            _state.value = ListState.Error
        }
    }

    /** Both sections, each by core's sort key (ties by id, so the order never shuffles). */
    private fun publish() {
        fun section(kind: String) = channels
            .filter { it.kind == kind }
            // Each key is computed once (it is an FFI call), not on every comparison.
            .map { Pair(sortKey(it.kind, it.name, it.members, meId), it) }
            .sortedWith(compareBy({ it.first }, { it.second.id }))
            .map { (_, c) -> ConversationRow(c.id, label(c.kind, c.name, c.members, meId)) }
        _state.value = ListState.Loaded(section("channel"), section("dm"))
    }
}
