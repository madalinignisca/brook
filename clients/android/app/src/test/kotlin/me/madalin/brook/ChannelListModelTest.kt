// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

package me.madalin.brook

import kotlinx.coroutines.ExperimentalCoroutinesApi
import kotlinx.coroutines.test.TestScope
import kotlinx.coroutines.test.runCurrent
import kotlinx.coroutines.test.runTest
import org.junit.Assert.assertEquals
import org.junit.Assert.assertTrue
import org.junit.Test
import uniffi.brook_ffi.FfiChannel
import uniffi.brook_ffi.FfiMember
import uniffi.brook_ffi.FfiServerEvent
import uniffi.brook_ffi.LoginResult

/**
 * The live channel list. The cases come from the Mac's `ChannelEventsTests`
 * (clients/apple-shared/BrookTests), by name where they apply; the Mac's unread, open-channel
 * and call-badge cases have nothing to port to here.
 *
 * The labels and sort keys come from core through the FFI, which a JVM test cannot load, so
 * the model is given plain Kotlin stand-ins with the same shape: a channel reads "#name", a DM
 * reads the other member's display name, and the sort key is the lowercase label.
 */
@OptIn(ExperimentalCoroutinesApi::class)
class ChannelListModelTest {
    private val bob = FfiMember(id = "u2", handle = "bob", displayName = "Bob", role = null)
    private val carol = FfiMember(id = "u3", handle = "carol", displayName = "Carol", role = null)

    private fun channel(id: String, name: String, archived: Boolean = false) = FfiChannel(
        id = id, kind = "channel", name = name, archived = archived, topic = null, isPublic = true,
        unreadMentions = 0, members = emptyList(), ownerOffers = emptyList(),
    )

    private fun dm(id: String, with: FfiMember) = FfiChannel(
        id = id, kind = "dm", name = null, archived = false, topic = null, isPublic = false,
        unreadMentions = 0, members = listOf(alice.asMember(), with), ownerOffers = emptyList(),
    )

    private fun label(kind: String, name: String?, members: List<FfiMember>, me: String): String =
        if (kind == "dm") members.first { it.id != me }.displayName else "#$name"

    private fun sortKey(kind: String, name: String?, members: List<FfiMember>, me: String): String =
        label(kind, name, members, me).lowercase()

    private fun fake(vararg channels: FfiChannel) =
        FakeClient(Result.success(LoginResult.LoggedIn(aliceSession))).also { it.channels = channels.toList() }

    /** Starts a model over `client` and lets its first load finish. */
    private fun TestScope.started(client: FakeClient): ChannelListModel {
        val model = ChannelListModel(client, alice.id, backgroundScope, ::label, ::sortKey)
        model.start()
        runCurrent()
        return model
    }

    private fun ChannelListModel.loaded() = state.value as ListState.Loaded

    private fun ChannelListModel.channelIds() = loaded().channels.map { it.id }

    private fun ChannelListModel.dmIds() = loaded().dms.map { it.id }

    private fun lists(client: FakeClient) = client.listCalls.count { it == "listChannels" }

    // MARK: sections

    @Test
    fun channelsThenDirectMessagesEachAlphabeticalByLabel() = runTest {
        val model = started(
            fake(
                channel("c2", "random"), dm("d2", carol), channel("c1", "General"),
                dm("d1", bob), channel("c3", "alpha"),
            ),
        )
        // Case-insensitive, as core's sort key is: "alpha" < "General" < "random".
        assertEquals(listOf("c3", "c1", "c2"), model.channelIds())
        assertEquals(listOf("d1", "d2"), model.dmIds())
        assertEquals(listOf("#alpha", "#General", "#random"), model.loaded().channels.map { it.label })
        assertEquals(listOf("Bob", "Carol"), model.loaded().dms.map { it.label })
    }

    @Test
    fun aSectionWithNoRowsIsEmptySoTheScreenHidesIt() = runTest {
        val model = started(fake(channel("c1", "general")))
        assertEquals(listOf("c1"), model.channelIds())
        assertTrue(model.loaded().dms.isEmpty())
        val onlyDms = started(fake(dm("d1", bob)))
        assertTrue(onlyDms.loaded().channels.isEmpty())
    }

    @Test
    fun anAccountWithNothingIsLoadedAndEmpty() = runTest {
        val model = started(fake())
        assertEquals(ListState.Loaded(emptyList(), emptyList()), model.state.value)
    }

    @Test
    fun anArchivedChannelIsListedByARead() = runTest {
        val model = started(fake(channel("c1", "general"), channel("c2", "old", archived = true)))
        assertEquals(listOf("c1", "c2"), model.channelIds())
    }

    // MARK: events

    @Test
    fun anUpdateReplacesTheRowInPlace() = runTest {
        val client = fake(channel("c1", "general"), channel("c2", "random"))
        val model = started(client)
        client.deliver(FfiServerEvent.ChannelUpdate(channel("c1", "gen-eral")))
        runCurrent()
        assertEquals(listOf("#gen-eral", "#random"), model.loaded().channels.map { it.label })
        assertEquals("a known channel is replaced, not re-read", 1, lists(client))
    }

    @Test
    fun anArchivingUpdateKeepsTheRow() = runTest {
        val client = fake(channel("c1", "general"), channel("c2", "random"))
        val model = started(client)
        client.deliver(FfiServerEvent.ChannelUpdate(channel("c2", "random", archived = true)))
        runCurrent()
        assertEquals(listOf("c1", "c2"), model.channelIds())
    }

    @Test
    fun anUpdateForAChannelNotListedReadsTheList() = runTest {
        val client = fake(channel("c1", "general"))
        val model = started(client)
        client.channels = listOf(channel("c1", "general"), channel("c3", "new")) // added to c3
        client.deliver(FfiServerEvent.ChannelUpdate(channel("c3", "new")))
        runCurrent()
        assertEquals(listOf("c1", "c3"), model.channelIds())
        assertEquals(2, lists(client))
    }

    @Test
    fun aDeleteRemovesTheRow() = runTest {
        val client = fake(channel("c1", "general"), channel("c2", "random"))
        val model = started(client)
        client.deliver(FfiServerEvent.ChannelDelete("c2"))
        runCurrent()
        assertEquals(listOf("c1"), model.channelIds())
        assertEquals("a delete needs no re-read", 1, lists(client))
    }

    /** A late update for a channel just left never brings it back: only the server's list can. */
    @Test
    fun anUpdateAfterADeleteDoesNotBringTheChannelBack() = runTest {
        val client = fake(channel("c1", "general"), channel("c2", "random"))
        val model = started(client)
        client.deliver(FfiServerEvent.ChannelDelete("c2"))
        client.channels = listOf(channel("c1", "general")) // the server no longer lists it
        client.deliver(FfiServerEvent.ChannelUpdate(channel("c2", "random")))
        runCurrent()
        assertEquals(listOf("c1"), model.channelIds())
    }

    @Test
    fun readyReloadsTheList() = runTest {
        val client = fake(channel("c1", "general"))
        val model = started(client)
        client.channels = listOf(channel("c1", "renamed"))
        client.deliver(FfiServerEvent.Ready)
        runCurrent()
        assertEquals(listOf("#renamed"), model.loaded().channels.map { it.label })
    }

    @Test
    fun resyncReloadsTheList() = runTest {
        val client = fake(channel("c1", "general"))
        val model = started(client)
        client.channels = listOf(channel("c1", "general"), channel("c2", "missed"))
        client.deliver(FfiServerEvent.Resync)
        runCurrent()
        assertEquals(listOf("c1", "c2"), model.channelIds())
    }

    @Test
    fun otherEventsAreIgnored() = runTest {
        val client = fake(channel("c1", "general"))
        val model = started(client)
        client.deliver(FfiServerEvent.ChannelCall("c1", "k1", 2u))
        runCurrent()
        assertEquals(1, lists(client))
        assertEquals(listOf("c1"), model.channelIds())
    }

    // MARK: loading

    @Test
    fun aFailedLoadIsAnErrorAndRetryLoads() = runTest {
        val client = fake(channel("c1", "general"))
        client.listFails = true
        val model = started(client)
        assertEquals(ListState.Error, model.state.value)
        client.listFails = false
        model.retry()
        runCurrent()
        assertEquals(listOf("c1"), model.channelIds())
    }

    // MARK: subscription

    @Test
    fun eventsAreSubscribedToBeforeRealtimeStartsAndTheListIsReadLast() = runTest {
        val client = fake(channel("c1", "general"))
        started(client)
        // Subscribing first means the `Ready` that startRealtime provokes cannot be missed.
        assertEquals(listOf("subscribeEvents", "startRealtime", "listChannels"), client.listCalls)
    }

    @Test
    fun stoppingCancelsTheSubscriptionAndEndsTheUpdates() = runTest {
        val client = fake(channel("c1", "general"))
        val model = started(client)
        model.stop()
        assertTrue(client.eventSubscription!!.cancelled)
        client.channels = emptyList()
        client.deliver(FfiServerEvent.Resync)
        runCurrent()
        assertEquals(1, lists(client))
        assertEquals(listOf("c1"), model.channelIds())
    }
}

private fun uniffi.brook_ffi.FfiUser.asMember() = FfiMember(id = id, handle = handle, displayName = displayName, role = null)
