import AVFoundation
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
    /// nil: joining fails. Set: join waits for the gate, then returns a handle.
    var joinGate: Gate?
    func joinCall(channelId: String, engine: FfiMediaEngine, publish: Bool) async throws -> FfiCallHandle {
        guard let joinGate else { throw LoginError.Disconnected }
        await joinGate.wait()
        return FakeCallHandle()
    }
    func login(handle: String, password: String) async throws -> LoginResult { throw LoginError.Disconnected }
    func subscribe(listener: AuthStateListener) -> Subscription { FakeSubscription() }

    func deliver(_ event: FfiServerEvent) { listener.withLock { $0 }?.onEvent(event: event) }
}

final class FakeHandle: FfiCallHandleProtocol, @unchecked Sendable {
    let media = Mutex<[String]>([])
    let refuse: Bool
    let leaves = Mutex(0)
    let leaveDelay: Duration
    /// Per-call scripted setMedia results (true = refuse), consumed in order; `refuse` after.
    let script = Mutex<[(refuse: Bool, gate: Gate?)]>([])
    init(refuse: Bool = false, leaveDelay: Duration = .zero) {
        self.refuse = refuse
        self.leaveDelay = leaveDelay
    }
    func engineFailed(message: String) {}
    func leave() async throws {
        leaves.withLock { $0 += 1 }
        try? await Task.sleep(for: leaveDelay)
    }
    func localCandidate(pc: FfiPcKind, candidate: FfiIceCandidate?) {}
    func republish() async throws {}
    func setMedia(audio: Bool, video: Bool) async throws {
        media.withLock { $0.append("\(audio):\(video)") }
        let step = script.withLock { $0.isEmpty ? nil : $0.removeFirst() }
        if let gate = step?.gate { await gate.wait() }
        if step?.refuse ?? refuse { throw LoginError.Api(code: "invalid", message: "") }
    }
    func subscribeState(listener: CallStateListener) -> Subscription { FakeSubscription() }
}

/// A one-shot gate a fake can wait on.
final class Gate: @unchecked Sendable {
    private let state = Mutex<(open: Bool, waiters: [CheckedContinuation<Void, Never>])>((false, []))
    func wait() async {
        await withCheckedContinuation { c in
            let open = state.withLock { s -> Bool in
                if !s.open { s.waiters.append(c) }
                return s.open
            }
            if open { c.resume() }
        }
    }
    func open() {
        let waiters = state.withLock { s -> [CheckedContinuation<Void, Never>] in
            s.open = true
            defer { s.waiters = [] }
            return s.waiters
        }
        waiters.forEach { $0.resume() }
    }
}

final class FakeMedia: CallEngine, @unchecked Sendable {
    let neverCloses: Bool
    let closeGate: Gate?
    let localVideo = Mutex<(@Sendable (RTCVideoTrack) -> Void)?>(nil)
    init(neverCloses: Bool = false, closeGate: Gate? = nil) {
        self.neverCloses = neverCloses
        self.closeGate = closeGate
    }
    func closed() async {
        if let closeGate { await closeGate.wait() }
        if neverCloses { await withCheckedContinuation { (_: CheckedContinuation<Void, Never>) in } }
    }
    func onLocalVideoTrack(_ callback: @escaping @Sendable (RTCVideoTrack) -> Void) {
        localVideo.withLock { $0 = callback }
    }
    func onRemoteTracks(_ callback: @escaping @Sendable ([RemoteTrack]) -> Void) {}
    func attach(_ events: EngineEvents) {}
    // FfiMediaEngine (core's side; unused by these tests)
    func createPublishOffer() async throws -> String { "" }
    func applyPublishAnswer(sdp: String) async throws {}
    func applySubscribeOffer(sdp: String, streams: [FfiSubStream]) async throws -> String { "" }
    func addRemoteCandidate(pc: FfiPcKind, candidate: FfiIceCandidate?) throws {}
    func setLocalMedia(audio: Bool, video: Bool) throws {}
    func setIceServers(servers: [FfiIceServer]) {}
    func close() async {}
}

/// A call handle as join_call returns it (a class), for CallCenter.
final class FakeCallHandle: FfiCallHandle, @unchecked Sendable {
    init() { super.init(noHandle: NoHandle()) }
    required init(unsafeFromHandle _: UInt64) { fatalError("never lifted from Rust") }
    override func leave() async throws {}
    override func setMedia(audio: Bool, video: Bool) async throws {}
    override func subscribeState(listener: CallStateListener) -> Subscription { FakeSubscription() }
    override func localCandidate(pc: FfiPcKind, candidate: FfiIceCandidate?) {}
    override func engineFailed(message: String) {}
}

struct GrantedNothing: AuthorizationSource {
    func status(_ media: AVMediaType) -> Authorization { .denied }
    func request(_ media: AVMediaType) async -> Bool { false }
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
final class CallReviewFixTests: XCTestCase {
    let full = JoinPlan(microphone: true, camera: true, explanation: nil)

    /// Leave, then quit while that leave is still running: the second caller waits for the
    /// same leave instead of returning at once.
    func testSecondLeaveWaitsForTheFirst() async throws {
        let gate = Gate()
        let call = CallModel(channelName: "c", plan: full, handle: FakeHandle(), media: FakeMedia(closeGate: gate))
        let first = Task { await call.leave() }
        try await Task.sleep(for: .milliseconds(50))
        let done = Mutex(false)
        let second = Task {
            await call.leave()
            done.withLock { $0 = true }
        }
        try await Task.sleep(for: .milliseconds(200))
        XCTAssertFalse(done.withLock { $0 }, "second leave returned before the first finished")
        gate.open()
        await first.value
        await second.value
        XCTAssertTrue(done.withLock { $0 })
    }

    /// An older request refused after a newer one succeeded must not roll the UI back.
    func testOlderRefusalDoesNotOverrideNewerIntent() async throws {
        let handle = FakeHandle()
        let gate = Gate()
        handle.script.withLock { $0 = [(refuse: true, gate: gate), (refuse: false, gate: nil)] }
        let call = CallModel(channelName: "c", plan: full, handle: handle, media: FakeMedia())
        let older = Task { await call.toggleMic() }  // mic off, held, then refused
        try await Task.sleep(for: .milliseconds(50))
        let newer = Task { await call.toggleCamera() }  // camera off, accepted
        try await Task.sleep(for: .milliseconds(50))
        gate.open()
        await older.value
        await newer.value
        XCTAssertEqual(handle.media.withLock { $0 }, ["false:true", "false:false"])
        XCTAssertFalse(call.micOn, "UI rolled back to an intent core no longer holds")
        XCTAssertFalse(call.cameraOn)
    }

    /// Round 2: both requests refused. Whatever order their answers arrive in, the UI ends
    /// on the last state core accepted (here: the initial one), which is what core restores.
    func testTwoRefusalsEndOnTheConfirmedState() async throws {
        let handle = FakeHandle()
        let gate = Gate()
        handle.script.withLock { $0 = [(refuse: true, gate: gate), (refuse: true, gate: nil)] }
        let call = CallModel(channelName: "c", plan: full, handle: handle, media: FakeMedia())
        let a = Task { await call.toggleMic() }
        try await Task.sleep(for: .milliseconds(50))
        let b = Task { await call.toggleCamera() }
        try await Task.sleep(for: .milliseconds(50))
        gate.open()
        await a.value
        await b.value
        XCTAssertTrue(call.micOn, "shows muted while core kept the mic on")
        XCTAssertTrue(call.cameraOn)
    }

    /// The camera track appears after the window started: the self-view picks it up.
    func testSelfViewArrivesAfterStart() async throws {
        let media = FakeMedia()
        let call = CallModel(channelName: "c", plan: full, handle: FakeHandle(), media: media)
        await call.start()
        XCTAssertNil(call.localVideo)
        let factory = RTCPeerConnectionFactory()
        let track = factory.videoTrack(with: factory.videoSource(), trackId: "camera")
        media.localVideo.withLock { $0 }?(track)
        await drainMain()
        XCTAssertTrue(call.localVideo === track)
    }

    /// The bound covers the whole leave: server confirmation and engine close together.
    func testLeaveBoundCoversTheWholeLeave() async {
        let call = CallModel(
            channelName: "c", plan: full, handle: FakeHandle(leaveDelay: .seconds(3)),
            media: FakeMedia(neverCloses: true), closeBound: .milliseconds(300))
        let started = Date()
        await call.leave()
        XCTAssertLessThan(Date().timeIntervalSince(started), 1.5, "leave not bounded as a whole")
    }

    /// Quit while joining: core may already publish, so quit waits for the join and leaves.
    func testQuitDuringJoinWaitsForTheJoin() async throws {
        let client = FakeRealtime(channels: [])
        client.joinGate = Gate()
        var replies = 0
        let quit = QuitCoordinator(timeout: .seconds(5), reply: { _ in replies += 1 })
        let center = CallCenter(auth: GrantedNothing(), quit: quit, makeEngine: { _ in FakeMedia() })
        let joining = Task { await center.join(channel("c1", "general"), name: "general", client: client) }
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertEqual(quit.shouldTerminate(), .terminateLater, "quit mid-join skipped the handshake")
        client.joinGate?.open()
        await joining.value
        try await Task.sleep(for: .milliseconds(300))
        XCTAssertEqual(replies, 1)
        XCTAssertNil(center.call, "the joined call was not left")
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
