// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import Foundation
import Testing

@testable import Brook

@MainActor
private final class FakeRinger: Ringer {
    var playing = false
    var starts = 0
    func start() { playing = true; starts += 1 }
    func stop() { playing = false }
}

@MainActor
private func make(limit: Duration = .seconds(60), dms: Set<String> = ["dm"], known: Bool = true)
    -> (IncomingCalls, FakeRinger, Box) {
    let ringer = FakeRinger()
    let box = Box()
    let incoming = IncomingCalls(ringer: ringer, limit: limit)
    incoming.isDM = { id in known ? dms.contains(id) : nil }
    incoming.localChannel = { box.local }
    incoming.alert = { id, on in box.alerts.append("\(id):\(on)") }
    return (incoming, ringer, box)
}

@MainActor
private final class Box {
    var local: String?
    var alerts: [String] = []
}

@MainActor
@Suite struct IncomingCallsTests {
    @Test func aDMCallWithOnePersonRings() {
        let (incoming, ringer, box) = make()
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        #expect(incoming.ringing == .init(channelId: "dm", callId: "c1"))
        #expect(ringer.playing)
        #expect(box.alerts == ["dm:true"])
    }

    @Test func aChannelCallNeverRings() {
        let (incoming, ringer, _) = make()
        incoming.observe(channelId: "general", callId: "c1", count: 1)
        #expect(incoming.ringing == nil)
        #expect(!ringer.playing)
    }

    @Test func theCallEndingStopsTheRing() {
        let (incoming, ringer, box) = make()
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        incoming.observe(channelId: "dm", callId: nil, count: 0)
        #expect(incoming.ringing == nil)
        #expect(!ringer.playing)
        #expect(box.alerts == ["dm:true", "dm:false"])
    }

    @Test func answeredElsewhereStopsAndNeverRingsAgain() {
        let (incoming, ringer, _) = make()
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        incoming.observe(channelId: "dm", callId: "c1", count: 2)
        #expect(incoming.ringing == nil)
        // The callee hangs up on the other device: the caller is alone again, no second ring.
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        #expect(incoming.ringing == nil)
        #expect(ringer.starts == 1)
    }

    @Test func declineSilencesThisCallForGood() {
        let (incoming, ringer, _) = make()
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        incoming.decline()
        #expect(incoming.ringing == nil)
        #expect(!ringer.playing)
        incoming.observe(channelId: "dm", callId: "c1", count: 1) // a reconnect's snapshot
        #expect(incoming.ringing == nil)
    }

    @Test func aNewCallAfterADeclineRings() {
        let (incoming, _, _) = make()
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        incoming.decline()
        incoming.observe(channelId: "dm", callId: nil, count: 0)
        incoming.observe(channelId: "dm", callId: "c2", count: 1)
        #expect(incoming.ringing?.callId == "c2")
    }

    @Test func answerStopsTheRingAndGivesTheCall() {
        let (incoming, ringer, _) = make()
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        #expect(incoming.answer() == .init(channelId: "dm", callId: "c1"))
        #expect(incoming.ringing == nil)
        #expect(!ringer.playing)
    }

    @Test func myOwnCallBeingJoinedNeverRings() {
        let (incoming, ringer, box) = make()
        box.local = "dm" // the join began before the server announced it
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        #expect(incoming.ringing == nil)
        #expect(!ringer.playing)
    }

    @Test func leavingMyCallDoesNotRingForThePersonLeftInIt() {
        let (incoming, _, box) = make()
        box.local = "dm"
        incoming.observe(channelId: "dm", callId: "c1", count: 2)
        box.local = nil // hung up here
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        #expect(incoming.ringing == nil)
    }

    @Test func joiningByAnotherRouteStopsTheRing() {
        let (incoming, ringer, _) = make()
        let box = Box()
        incoming.localChannel = { box.local }
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        box.local = "dm" // the toolbar's Join
        incoming.reevaluate()
        #expect(incoming.ringing == nil)
        #expect(!ringer.playing)
    }

    @Test func anAnnouncementBeforeTheListRingsOnceTheListSaysDM() {
        var known = false
        let ringer = FakeRinger()
        let incoming = IncomingCalls(ringer: ringer)
        incoming.isDM = { id in known ? id == "dm" : nil }
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        #expect(incoming.ringing == nil)
        known = true
        incoming.reevaluate()
        #expect(incoming.ringing?.callId == "c1")
    }

    @Test func theRingGivesUpAfterTheLimit() async throws {
        let (incoming, ringer, box) = make(limit: .milliseconds(20))
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        try await Task.sleep(for: .milliseconds(200))
        #expect(incoming.ringing == nil)
        #expect(!ringer.playing)
        #expect(box.alerts.last == "dm:false")
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        #expect(incoming.ringing == nil) // a missed ring isn't repeated
    }

    @Test func aDMRemovedWhileRingingStopsIt() {
        var listed = true
        let ringer = FakeRinger()
        let incoming = IncomingCalls(ringer: ringer)
        incoming.isDM = { id in listed && id == "dm" ? true : nil }
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        #expect(ringer.playing)
        listed = false
        incoming.reevaluate()
        #expect(incoming.ringing == nil)
        #expect(!ringer.playing)
    }

    @Test func stopEndsEverything() {
        let (incoming, ringer, box) = make()
        incoming.observe(channelId: "dm", callId: "c1", count: 1)
        incoming.stop()
        #expect(incoming.ringing == nil)
        #expect(!ringer.playing)
        #expect(box.alerts.last == "dm:false")
    }
}
