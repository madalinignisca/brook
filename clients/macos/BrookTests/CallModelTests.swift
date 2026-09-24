import AppKit
import BrookCore
import BrookMedia
import Foundation
import Synchronization
import XCTest
@preconcurrency import WebRTC

@testable import Brook

final class FakeSubscription: Subscription, @unchecked Sendable {
    let cancelled = Mutex(false)
    init() { super.init(noHandle: NoHandle()) }
    required init(unsafeFromHandle _: UInt64) { fatalError("never lifted from Rust") }
    override func cancel() { cancelled.withLock { $0 = true } }
}

/// Records the order of calls; lets a test deliver server events.
final class FakeRealtime: FfiBrookClientProtocol, @unchecked Sendable {
    let order = Mutex<[String]>([])
    let listener = Mutex<ServerEventListener?>(nil)
    let channels: [FfiChannel]
    init(channels: [FfiChannel]) { self.channels = channels }

    func subscribeEvents(listener: ServerEventListener) -> Subscription {
        order.withLock { $0.append("subscribe") }
        self.listener.withLock { $0 = listener }
        return FakeSubscription()
    }
    func startRealtime() async throws { order.withLock { $0.append("start") } }
    func listChannels() async throws -> [FfiChannel] {
        order.withLock { $0.append("list") }
        return channels
    }
    func joinCall(channelId: String, engine: FfiMediaEngine, publish: Bool) async throws -> FfiCallHandle {
        throw LoginError.Disconnected
    }
    func login(handle: String, password: String) async throws -> LoginResult { throw LoginError.Disconnected }
    func subscribe(listener: AuthStateListener) -> Subscription { FakeSubscription() }

    func deliver(_ event: FfiServerEvent) { listener.withLock { $0 }?.onEvent(event: event) }
}

final class FakeHandle: FfiCallHandleProtocol, @unchecked Sendable {
    let media = Mutex<[String]>([])
    let refuse: Bool
    let leaves = Mutex(0)
    init(refuse: Bool = false) { self.refuse = refuse }
    func engineFailed(message: String) {}
    func leave() async throws { leaves.withLock { $0 += 1 } }
    func localCandidate(pc: FfiPcKind, candidate: FfiIceCandidate?) {}
    func republish() async throws {}
    func setMedia(audio: Bool, video: Bool) async throws {
        media.withLock { $0.append("\(audio):\(video)") }
        if refuse { throw LoginError.Api(code: "invalid", message: "") }
    }
    func subscribeState(listener: CallStateListener) -> Subscription { FakeSubscription() }
}

final class FakeMedia: CallMedia, @unchecked Sendable {
    let neverCloses: Bool
    init(neverCloses: Bool = false) { self.neverCloses = neverCloses }
    func closed() async {
        if neverCloses { await withCheckedContinuation { (_: CheckedContinuation<Void, Never>) in } }
    }
    func localVideoTrack() async -> RTCVideoTrack? { nil }
    func onRemoteTracks(_ callback: @escaping @Sendable ([RemoteTrack]) -> Void) {}
}

func channel(_ id: String, _ name: String) -> FfiChannel {
    FfiChannel(id: id, kind: "public", name: name, archived: false)
}

/// Let main-queue deliveries (the event/state bridges hop through it) run.
@MainActor
func drainMain() async {
    await withCheckedContinuation { c in DispatchQueue.main.async { c.resume() } }
}

@MainActor
final class ChannelsModelTests: XCTestCase {
    func testSubscribesBeforeStartingRealtime() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        let model = ChannelsModel(client: client)
        await model.start()
        XCTAssertEqual(client.order.withLock { $0 }, ["subscribe", "start", "list"])
        XCTAssertEqual(model.channels.map(\.id), ["c1"])
    }

    func testJoinIsDisabledUntilReady() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        let model = ChannelsModel(client: client)
        await model.start()
        XCTAssertFalse(model.canJoin(model.channels[0]))
        client.deliver(.ready)
        await drainMain()
        XCTAssertTrue(model.canJoin(model.channels[0]))
    }

    func testBadgesFollowChannelCallEvents() async {
        let client = FakeRealtime(channels: [channel("c1", "general"), channel("c2", "random")])
        let model = ChannelsModel(client: client)
        await model.start()
        client.deliver(.channelCall(channelId: "c1", callId: "k1", participantCount: 3))
        await drainMain()
        XCTAssertEqual(model.badge(model.channels[0]), "● Call · 3")
        XCTAssertNil(model.badge(model.channels[1]))
        client.deliver(.channelCall(channelId: "c1", callId: nil, participantCount: 0))
        await drainMain()
        XCTAssertNil(model.badge(model.channels[0]), "badge stayed after the call ended")
    }
}

@MainActor
final class CallModelTests: XCTestCase {
    func testAbsentTrackControlsDoNothing() async {
        let handle = FakeHandle()
        let plan = JoinPlan(microphone: true, camera: false, explanation: JoinPlan.cameraDenied)
        let call = CallModel(channelName: "general", plan: plan, handle: handle, media: FakeMedia())
        await call.toggleCamera()
        XCTAssertEqual(handle.media.withLock { $0 }, [], "asked to enable a camera that isn't there")
        await call.toggleMic()
        XCTAssertEqual(handle.media.withLock { $0 }, ["false:false"])
        XCTAssertFalse(call.micOn)
    }

    func testRefusedToggleShowsWhatIsReallySent() async {
        let handle = FakeHandle(refuse: true)
        let plan = JoinPlan(microphone: true, camera: true, explanation: nil)
        let call = CallModel(channelName: "general", plan: plan, handle: handle, media: FakeMedia())
        await call.toggleMic()
        XCTAssertTrue(call.micOn, "control shows muted although the server refused")
    }

    func testTilesAndBannersFollowState() {
        let plan = JoinPlan(microphone: true, camera: true, explanation: nil)
        let call = CallModel(channelName: "general", plan: plan, handle: FakeHandle(), media: FakeMedia())
        let other = FfiParticipant(participantId: "p2", userId: "u2", displayName: "Linux", audio: false, video: true)
        // core's roster excludes self: the self tile comes from local state.
        call.apply(FfiCallState(status: .connected, callId: "k1", selfParticipant: "p1", participants: [other]))
        XCTAssertEqual(call.tiles.map { (t: CallModel.Tile) in t.name }, ["You", "Linux"])
        XCTAssertTrue(call.tiles[0].isSelf)
        XCTAssertTrue(call.tiles[0].audio)
        XCTAssertFalse(call.tiles[1].audio)
        XCTAssertNil(call.banner)
        call.apply(FfiCallState(status: .connected, callId: "k1", selfParticipant: "p1", participants: []))
        XCTAssertEqual(call.tiles.map { (t: CallModel.Tile) in t.name }, ["You"], "alone: only the self tile")
        call.apply(FfiCallState(status: .reconnecting, callId: "k1", selfParticipant: "p1", participants: [other]))
        XCTAssertEqual(call.banner, "Reconnecting…")
        call.apply(FfiCallState(status: .ended(reason: .removed), callId: "k1", selfParticipant: "p1", participants: []))
        XCTAssertEqual(call.banner, "You were removed from the call.")
        XCTAssertTrue(call.isEnded)
    }

    func testLeaveIsBoundedWhenTheEngineNeverCloses() async {
        let handle = FakeHandle()
        let plan = JoinPlan(microphone: true, camera: true, explanation: nil)
        let call = CallModel(
            channelName: "general", plan: plan, handle: handle, media: FakeMedia(neverCloses: true),
            closeBound: .milliseconds(200))
        let started = Date()
        await call.leave()
        XCTAssertLessThan(Date().timeIntervalSince(started), 2)
        XCTAssertEqual(handle.leaves.withLock { $0 }, 1)
    }
}

@MainActor
final class QuitCoordinatorTests: XCTestCase {
    func testNoCallQuitsNow() {
        var replies = 0
        let quit = QuitCoordinator(reply: { _ in replies += 1 })
        XCTAssertEqual(quit.shouldTerminate(), .terminateNow)
        XCTAssertEqual(replies, 0)
    }

    func testRepliesOnceAfterTheLeaveCompletes() async throws {
        var replies = 0
        let quit = QuitCoordinator(timeout: .seconds(1), reply: { _ in replies += 1 })
        quit.leaveActiveCall = {}
        XCTAssertEqual(quit.shouldTerminate(), .terminateLater)
        XCTAssertEqual(quit.shouldTerminate(), .terminateLater)  // a second ⌘Q while waiting
        try await Task.sleep(for: .milliseconds(1500))  // past the bound too
        XCTAssertEqual(replies, 1)
    }

    func testRepliesOnceAtTheBoundWhenTheLeaveHangs() async throws {
        var replies = 0
        let quit = QuitCoordinator(timeout: .milliseconds(200), reply: { _ in replies += 1 })
        quit.leaveActiveCall = { await withCheckedContinuation { (_: CheckedContinuation<Void, Never>) in } }
        XCTAssertEqual(quit.shouldTerminate(), .terminateLater)
        try await Task.sleep(for: .milliseconds(600))
        XCTAssertEqual(replies, 1)
    }
}
