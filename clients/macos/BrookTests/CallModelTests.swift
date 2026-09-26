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
    var channels: [FfiChannel]
    init(channels: [FfiChannel]) { self.channels = channels }
    /// For the offline tests (#62): a failing realtime start or network list, and the cache's
    /// channels (nil: `local.unavailable`).
    var realtimeFails = false
    var listFails = false
    var cached: [FfiCachedChannel]?
    /// Holds the network list until opened (a read that finishes late).
    var listGate: Gate?

    func subscribeEvents(listener: ServerEventListener) -> Subscription {
        order.withLock { $0.append("subscribe") }
        self.listener.withLock { $0 = listener }
        return FakeSubscription()
    }
    func startRealtime() async throws {
        order.withLock { $0.append("start") }
        if realtimeFails { throw LoginError.Network(message: "offline") }
    }
    func listChannels() async throws -> [FfiChannel] {
        order.withLock { $0.append("list") }
        let snapshot = channels
        if let listGate { await listGate.wait() }
        if listFails { throw LoginError.Network(message: "offline") }
        return snapshot
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
    func changePassword(current: String, new: String, signOutOtherDevices: Bool) async throws -> Bool? { nil }
    func logout() async {}
    func enablePersistence(slot: FfiKeySlot, dataDir: String) {}
    func restore() async -> FfiRestoreOutcome { .notSignedIn }
    func signOutComplete() -> Bool { true }
    func authState() -> FfiAuthState { .loggedOut }
    func completeTotp(challenge: FfiTotpChallenge, code: String) async throws -> UInt32? { nil }
    func completeRecovery(challenge: FfiTotpChallenge, recoveryCode: String) async throws -> UInt32? { nil }
    func cancelTotp(challenge: FfiTotpChallenge) async {}
    func me() async throws -> FfiMe { throw LoginError.NotAuthenticated }
    func totpEnroll(password: String) async throws -> FfiTotpEnrollment { throw LoginError.NotAuthenticated }
    func totpActivate(code: String) async throws -> [String] { [] }
    func totpDisable(password: String, factor: FfiSecondFactor) async throws {}
    func totpRegenerateRecoveryCodes(password: String, factor: FfiSecondFactor) async throws -> [String] { [] }
    func adminResetTotp(userId: String, adminPassword: String) async throws {}
    func adminResetPassword(userId: String, adminPassword: String, new: String) async throws {}
    func listUsers() async throws -> [FfiUserSummary] { [] }

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
    let republishes = Mutex(0)
    var refuseRepublish = false
    var republishGate: Gate?
    func republish() async throws {
        republishes.withLock { $0 += 1 }
        if let republishGate { await republishGate.wait() }
        if refuseRepublish { throw LoginError.Disconnected }
    }
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
    let remoteTracks = Mutex<(@Sendable ([RemoteTrack]) -> Void)?>(nil)
    func onRemoteTracks(_ callback: @escaping @Sendable ([RemoteTrack]) -> Void) {
        remoteTracks.withLock { $0 = callback }
    }
    func attach(_ events: EngineEvents) {}
    let screen = Mutex<[String]>([])
    var refuseShare = false
    func startScreenShare(_ capture: VideoCapture) async throws {
        screen.withLock { $0.append("start") }
        if refuseShare { throw FfiEngineError.Failed(message: "no") }
    }
    func stopScreenShare() async { screen.withLock { $0.append("stop") } }
    let cameraProblem = Mutex<(@Sendable (String) -> Void)?>(nil)
    func onCameraProblem(_ callback: @escaping @Sendable (String) -> Void) { cameraProblem.withLock { $0 = callback } }
    let shareEnded = Mutex<(@Sendable () -> Void)?>(nil)
    func onScreenShareEnded(_ callback: @escaping @Sendable () -> Void) { shareEnded.withLock { $0 = callback } }
    func createLabelledOffer() async throws -> FfiPublishOffer { FfiPublishOffer(sdp: "", tracks: []) }
    // FfiMediaEngine (core's side; unused by these tests)
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
    FfiChannel(id: id, kind: "public", name: name, archived: false, members: [], ownerOffers: [])
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

    /// Signed out between the join being accepted and its work starting: still abandoned.
    func testSignOutRightAfterAJoinIsAcceptedAbandonsIt() async throws {
        let client = FakeRealtime(channels: [])
        client.joinGate = Gate()
        let center = CallCenter(auth: GrantedNothing(), makeEngine: { _ in FakeMedia() })
        let joining = Task { await center.join(channelId: "c1", name: "general", client: client) }
        await Task.yield() // join() registers its task; the join's work has not started yet
        await center.endAll()
        client.joinGate?.open()
        await joining.value
        XCTAssertNil(center.call, "a join accepted before sign-out became the live call")
    }

    /// Signed out while a join is still waiting: the late join is abandoned (left at once),
    /// never becomes the live call, and nothing is left joining.
    func testSignOutDuringAJoinAbandonsIt() async throws {
        let client = FakeRealtime(channels: [])
        client.joinGate = Gate()
        let center = CallCenter(auth: GrantedNothing(), makeEngine: { _ in FakeMedia() })
        let joining = Task { await center.join(channelId: "c1", name: "general", client: client) }
        try await Task.sleep(for: .milliseconds(100))
        XCTAssertTrue(center.joining)
        let ended = Task { await center.endAll() }
        client.joinGate?.open()
        await joining.value
        await ended.value
        XCTAssertNil(center.call, "a join finishing after sign-out became the live call")
        XCTAssertFalse(center.joining)
    }

    /// Quit while joining: core may already publish, so quit waits for the join and leaves.
    func testQuitDuringJoinWaitsForTheJoin() async throws {
        let client = FakeRealtime(channels: [])
        client.joinGate = Gate()
        var replies = 0
        let quit = QuitCoordinator(timeout: .seconds(5), reply: { _ in replies += 1 })
        let center = CallCenter(auth: GrantedNothing(), quit: quit, makeEngine: { _ in FakeMedia() })
        let joining = Task { await center.join(channelId: "c1", name: "general", client: client) }
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
final class ScreenShareModelTests: XCTestCase {
    let full = JoinPlan(microphone: true, camera: true, explanation: nil)
    let capture = SyntheticVideoCapture()

    func testShareStartsThenRenegotiatesOnce() async {
        let handle = FakeHandle(), media = FakeMedia()
        let call = CallModel(channelName: "c", plan: full, handle: handle, media: media)
        await call.shareScreen(capture)
        XCTAssertTrue(call.sharing)
        XCTAssertEqual(media.screen.withLock { $0 }, ["start"])
        XCTAssertEqual(handle.republishes.withLock { $0 }, 1)
        await call.stopSharing()
        XCTAssertFalse(call.sharing)
        XCTAssertEqual(media.screen.withLock { $0 }, ["start", "stop"])
        XCTAssertEqual(handle.republishes.withLock { $0 }, 2)
    }

    func testRefusedStartDoesNotRenegotiate() async {
        let handle = FakeHandle(), media = FakeMedia()
        media.refuseShare = true
        let call = CallModel(channelName: "c", plan: full, handle: handle, media: media)
        await call.shareScreen(capture)
        XCTAssertFalse(call.sharing)
        XCTAssertNotNil(call.shareError)
        XCTAssertEqual(handle.republishes.withLock { $0 }, 0)
    }

    /// A share the server never heard about must not keep capturing.
    func testFailedRenegotiationUndoesTheShare() async {
        let handle = FakeHandle(), media = FakeMedia()
        handle.refuseRepublish = true
        let call = CallModel(channelName: "c", plan: full, handle: handle, media: media)
        await call.shareScreen(capture)
        XCTAssertFalse(call.sharing)
        XCTAssertEqual(media.screen.withLock { $0 }, ["start", "stop"])
    }

    /// The system ended the share: the UI stops showing it and renegotiates once.
    func testShareEndedBySystemRenegotiates() async throws {
        let handle = FakeHandle(), media = FakeMedia()
        let call = CallModel(channelName: "c", plan: full, handle: handle, media: media)
        await call.start()
        await call.shareScreen(capture)
        XCTAssertEqual(handle.republishes.withLock { $0 }, 1)
        media.shareEnded.withLock { $0 }?()
        await drainMain()
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertFalse(call.sharing)
        XCTAssertEqual(handle.republishes.withLock { $0 }, 2)
    }

    /// The system ends the share while its renegotiation is still in flight: the UI must not
    /// then show it as shared.
    func testShareEndedWhileStartingIsNotShownAsSharing() async throws {
        let handle = FakeHandle(), media = FakeMedia()
        handle.republishGate = Gate()
        let call = CallModel(channelName: "c", plan: full, handle: handle, media: media)
        await call.start()
        let sharing = Task { await call.shareScreen(capture) }
        try await Task.sleep(for: .milliseconds(100))  // start done, republish held
        media.shareEnded.withLock { $0 }?()
        await drainMain()
        handle.republishGate?.open()
        await sharing.value
        XCTAssertFalse(call.sharing, "an ended share shown as sharing")
        XCTAssertEqual(handle.republishes.withLock { $0 }, 2, "no renegotiation for the end")
    }

    /// The camera failed: the UI shows it off, says why, and tells the others (camera off).
    func testCameraProblemTurnsTheCameraOffAndKeepsTheCall() async throws {
        let handle = FakeHandle(), media = FakeMedia()
        let call = CallModel(channelName: "c", plan: full, handle: handle, media: media)
        await call.start()
        media.cameraProblem.withLock { $0 }?("busy")
        await drainMain()
        try await Task.sleep(for: .milliseconds(50))
        XCTAssertFalse(call.cameraOn)
        XCTAssertEqual(call.cameraProblem, CallModel.cameraUnavailable)
        XCTAssertEqual(handle.media.withLock { $0 }, ["true:false"])
        XCTAssertFalse(call.isEnded)
        await call.toggleCamera()  // the user tries again
        XCTAssertNil(call.cameraProblem, "stale warning after a retry")
    }

    func testEndedCallIsNotSharing() async {
        let call = CallModel(channelName: "c", plan: full, handle: FakeHandle(), media: FakeMedia())
        await call.shareScreen(capture)
        call.apply(FfiCallState(status: .ended(reason: .removed), callId: "k1", selfParticipant: "p1", participants: []))
        XCTAssertFalse(call.sharing)
    }

    func testListenOnlyCannotShare() async {
        let media = FakeMedia()
        let listenOnly = JoinPlan(microphone: false, camera: false, explanation: JoinPlan.micDenied)
        let call = CallModel(channelName: "c", plan: listenOnly, handle: FakeHandle(), media: media)
        await call.shareScreen(capture)
        XCTAssertEqual(media.screen.withLock { $0 }, [])
    }

    /// Another participant's screen becomes its own tile, first.
    func testRemoteScreenBecomesAFirstTile() async throws {
        let media = FakeMedia()
        let call = CallModel(channelName: "c", plan: full, handle: FakeHandle(), media: media)
        await call.start()
        let linux = FfiParticipant(participantId: "p2", userId: "u2", displayName: "Linux", audio: true, video: true)
        call.apply(FfiCallState(status: .connected, callId: "k1", selfParticipant: "p1", participants: [linux]))
        let factory = RTCPeerConnectionFactory()
        let cam = factory.videoTrack(with: factory.videoSource(), trackId: "cam")
        let scr = factory.videoTrack(with: factory.videoSource(), trackId: "scr")
        media.remoteTracks.withLock { $0 }?([
            RemoteTrack(mid: "1", participantId: "p2", kind: .video, source: .camera, track: cam),
            RemoteTrack(mid: "2", participantId: "p2", kind: .video, source: .screen, track: scr),
        ])
        await drainMain()
        let tiles = call.tiles
        XCTAssertEqual(tiles.map(\.name), ["Linux's screen", "You", "Linux"])
        XCTAssertTrue(tiles[0].isScreen)
        XCTAssertTrue(tiles[0].track === scr)
        XCTAssertTrue(tiles[2].track === cam)
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

/// The chat, offline and transfer calls: not used by the call tests (the Mac tests stopped
/// building when the bindings grew them; they answer "not signed in" or are never called).
extension FakeRealtime {
    private var unused: LoginError { .NotAuthenticated }
    func acknowledgeOutboxLost(n: UInt64) {}
    func cachedChannels() async throws -> [FfiCachedChannel] {
        guard let cached else { throw LoginError.Api(code: "local.unavailable", message: "") }
        return cached
    }
    func cachedMessages(channelId: String, before: String?, limit: UInt32) async throws -> FfiCachedMessages { throw unused }
    func cancelTransfer(transferId: UInt64) {}
    func channelHistory(channelId: String, before: String?) async throws -> [FfiMessage] { throw unused }
    func deleteMessage(channelId: String, messageId: String) async throws { throw unused }
    func deletePending(clientId: String) async throws -> FfiDeleted { throw unused }
    func downloadFile(transferId: UInt64, fileId: String, sha256: String, size: UInt64, destination: String) async throws { throw unused }
    func editMessage(channelId: String, messageId: String, body: String) async throws -> FfiMessage { throw unused }
    func enableLocalData(slot: any FfiKeySlot, dataDir: String) async -> Bool { false }
    func loadHead(channelId: String, limit: UInt32) async throws { throw unused }
    func loadOlder(channelId: String, limit: UInt32) async throws { throw unused }
    func markRead(channelId: String, messageId: String?) async throws { throw unused }
    func removeMember(channelId: String, userId: String) async throws { throw unused }
    func leaveChannel(channelId: String) async throws { throw unused }
    func offerOwnership(channelId: String, handle: String) async throws -> FfiChannel { throw unused }
    func withdrawOwnershipOffer(channelId: String, userId: String) async throws { throw unused }
    func acceptOwnership(channelId: String) async throws -> FfiChannel { throw unused }
    func declineOwnership(channelId: String) async throws { throw unused }
    func updateProfile(displayName: String?, statusText: String?) async throws -> FfiMe { throw unused }
    func otherLocalUsers() async throws -> [FfiLocalUser] { throw unused }
    func outboxLost() -> UInt64? { nil }
    func pendingMessages(channelId: String) async throws -> [FfiPendingMessage] { throw unused }
    func retrySend(clientId: String) async throws { throw unused }
    func retryWithoutReply(clientId: String) async throws { throw unused }
    func sendMessage(channelId: String, body: String, replyToId: String?) async throws -> FfiMessage { throw unused }
    func sendQueued(channelId: String, body: String, replyToId: String?, clientId: String) async throws -> String { throw unused }
    func sendQueuedWithFiles(channelId: String, body: String, replyToId: String?, clientId: String, files: [FfiOutgoingFile]) async throws -> FfiSendReceipt { throw unused }
    func signOutAndForget() async throws { throw unused }
    func subscribeCacheEvents(listener: any CacheEventListener) -> Subscription { fatalError("unused") }
    func subscribeCacheState(listener: any CacheStateListener) -> Subscription { fatalError("unused") }
    func subscribeTransfers(listener: any TransferListener) -> Subscription { fatalError("unused") }
    // The file cache (#149, #155): unused by these tests.
    func cacheFile(transferId: UInt64, fileId: String) async throws { throw unused }
    func openFile(transferId: UInt64, fileId: String) async throws -> String { throw unused }
    func saveCachedFile(fileId: String, destination: String) async throws -> Bool { throw unused }
    func fileState(fileId: String) async throws -> FfiFileCacheState { throw unused }
    func clearOpenCopies() async {}
    func pinFile(fileId: String) async throws { throw unused }
    func unpinFile(fileId: String) async throws { throw unused }
    func pinnedBytes() async throws -> UInt64 { throw unused }
    func previewFile(transferId: UInt64, fileId: String) async throws -> FfiImagePreview { throw unused }
    func cachedUsers(ids: [String]) async throws -> [FfiMember] { throw unused }
    func closeLocalData() async {}
    func unsentCount() async -> UInt64 { 0 }
    func wipeOtherLocalUsers() async throws { throw unused }
}

extension FakeRealtime: OfflineClient {}
