@preconcurrency import WebRTC
import BrookCore
import Foundation
import Synchronization
import WebRTCAudioDevice

/// Where the engine's own events go: the call handle in the app, a direct wire in tests.
public protocol EngineEvents: AnyObject, Sendable {
    func localCandidate(pc: FfiPcKind, candidate: FfiIceCandidate?)
    func engineFailed(message: String)
}

extension FfiCallHandle: EngineEvents {}

/// A camera, or a stand-in for one. `start` returns once frames are flowing.
public protocol VideoCapture: AnyObject, Sendable {
    func start(into source: RTCVideoSource) async throws
    func stop() async
    var isCapturing: Bool { get }
}

/// A remote track and who it belongs to, from the latest subscribe offer's `streams`.
public struct RemoteTrack: @unchecked Sendable {
    public let mid: String
    public let participantId: String
    public let kind: FfiMediaKind
    public let source: FfiMediaSource
    public let track: RTCMediaStreamTrack
}

public struct MediaOptions: @unchecked Sendable {
    /// Publish a microphone track (false when permission was denied, or listen-only).
    public var audio: Bool
    /// Publish a camera track fed by this (nil: no camera).
    public var video: VideoCapture?
    /// nil: WebRTC's default audio device (the real microphone and speakers).
    public var audioDevice: (any RTCAudioDevice)?
    /// A capture that has not started by then fails the operation (design §3.3).
    public var captureTimeout: Duration

    public init(
        audio: Bool, video: VideoCapture?, audioDevice: (any RTCAudioDevice)? = nil,
        captureTimeout: Duration = .seconds(5)
    ) {
        self.audio = audio
        self.video = video
        self.audioDevice = audioDevice
        self.captureTimeout = captureTimeout
    }
}

struct CaptureTimeout: Error, CustomStringConvertible {
    let timeout: Duration
    var description: String { "camera did not start within \(timeout)" }
}

struct EngineFailure: Error, CustomStringConvertible {
    let description: String
    init(_ description: String) { self.description = description }
}

/// core's `MediaEngine` over libwebrtc (design §3.3).
///
/// All WebRTC work runs in `EngineCore`, an actor whose executor is one serial queue. The sync
/// trait methods are called by Rust worker threads inline in core's call task, so they only
/// enqueue onto that queue and return: WebRTC setters can block on WebRTC's own threads, and
/// FIFO order is kept (a `Task` per call would not keep it). Async methods await the actor.
public final class WebRTCEngine: FfiMediaEngine, @unchecked Sendable {
    private let core: EngineCore
    private let hasAudio: Bool
    private let hasVideo: Bool
    /// Set before anything else in `close()`, so work already running sees it.
    private let fence: Fence

    public init(options: MediaOptions) {
        let fence = Fence()
        self.fence = fence
        hasAudio = options.audio
        hasVideo = options.video != nil
        core = EngineCore(options: options, fence: fence)
    }

    /// Deliver candidates and failures here; what was produced before is delivered first, in
    /// order. Held weakly: the call task retains the engine, so a strong reference to the handle
    /// would keep the call alive after the UI dropped it and defeat core's drop-to-leave.
    public func attach(_ events: EngineEvents) {
        core.enqueue { $0.attach(events) }
    }

    /// Latest remote tracks with their owners, on every subscribe offer (engine queue).
    /// Replays the current tracks at once if there are any (the UI registers after join_call
    /// returns, which can be after the first subscribe offer).
    public func onRemoteTracks(_ callback: @escaping @Sendable ([RemoteTrack]) -> Void) {
        core.enqueue { $0.setRemoteTracksCallback(callback) }
    }

    /// The camera track for the self-view, as soon as it exists (replayed if it already does):
    /// the publish offer that creates it runs independently of join_call returning.
    public func onLocalVideoTrack(_ callback: @escaping @Sendable (RTCVideoTrack) -> Void) {
        core.enqueue { $0.setLocalVideoCallback(callback) }
    }

    /// The camera track for the self-view (nil without a camera, or before the first offer).
    /// The engine keeps this wrapper alive, so renderers added to it stay attached.
    public func localVideoTrack() async -> RTCVideoTrack? {
        await core.localVideoTrack
    }

    /// Completes once `close()` has stopped capture and closed both connections.
    public func closed() async {
        await core.waitClosed()
    }

    /// The nominated candidate pair of a connection, as "local ↔ remote" (diagnostics, tests).
    public func selectedCandidatePair(_ pc: FfiPcKind) async -> String? {
        await core.selectedCandidatePair(pc)
    }

    /// WebRTC stats entries of one type ("outbound-rtp", "inbound-rtp", …) for diagnostics.
    public func statistics(_ pc: FfiPcKind, type: String) async -> [[String: String]] {
        await core.statistics(pc, type: type)
    }

    /// Tests: run `work` on the engine queue, in order with everything else (e.g. to stall it).
    func onEngineQueue(_ work: @escaping @Sendable () -> Void) {
        core.enqueue { _ in work() }
    }

    // MARK: FfiMediaEngine

    public func createPublishOffer() async throws -> String {
        try await mapped { try await self.core.createPublishOffer() }
    }

    public func applyPublishAnswer(sdp: String) async throws {
        try await mapped { try await self.core.applyPublishAnswer(sdp) }
    }

    public func applySubscribeOffer(sdp: String, streams: [FfiSubStream]) async throws -> String {
        try await mapped { try await self.core.applySubscribeOffer(sdp, streams) }
    }

    public func addRemoteCandidate(pc: FfiPcKind, candidate: FfiIceCandidate?) throws {
        core.enqueue { $0.addRemote(pc, candidate) }
    }

    /// Whether a track exists is fixed at creation, so this answers without touching WebRTC.
    /// Only enabling an absent track is an error: a camera-denied user can still mute.
    public func setLocalMedia(audio: Bool, video: Bool) throws {
        if audio && !hasAudio { throw FfiEngineError.Failed(message: "no microphone is published") }
        if video && !hasVideo { throw FfiEngineError.Failed(message: "no camera is published") }
        core.enqueue { $0.setMedia(audio: audio, video: video) }
    }

    public func setIceServers(servers: [FfiIceServer]) {
        core.enqueue { $0.setIceServers(servers) }
    }

    public func close() async {
        fence.set()
        await core.close()
    }

    private func mapped<T>(_ op: () async throws -> T) async throws -> T {
        do {
            return try await op()
        } catch let error as FfiEngineError {
            throw error
        } catch {
            throw FfiEngineError.Failed(message: String(describing: error))
        }
    }
}

final class Fence: Sendable {
    private let flag = Atomic<Bool>(false)
    func set() { flag.store(true, ordering: .sequentiallyConsistent) }
    var isSet: Bool { flag.load(ordering: .sequentiallyConsistent) }
}

/// Everything WebRTC, confined to one serial queue (the actor's executor).
actor EngineCore {
    private let queue = DispatchSerialQueue(label: "brook.media.engine")
    nonisolated var unownedExecutor: UnownedSerialExecutor { queue.asUnownedSerialExecutor() }

    private let fence: Fence
    private let factory: RTCPeerConnectionFactory
    private let wantsAudio: Bool
    private let capture: VideoCapture?
    private let captureTimeout: Duration

    private var iceServers: [RTCIceServer] = []
    private var publish: RTCPeerConnection?
    private var subscribe: RTCPeerConnection?
    private var delegates: [FfiPcKind: PeerDelegate] = [:]
    private var audioTrack: RTCAudioTrack?
    private var videoSource: RTCVideoSource?
    private(set) var localVideoTrack: RTCVideoTrack?
    /// The user's intent; applied to tracks and capture as they come to exist.
    private var media = (audio: true, video: true)
    private var captureRunning = false
    private var captureSyncing = false

    private weak var events: EngineEvents?
    private var pending: [(EngineEvents) -> Void] = []
    private var attached = false

    private var owners: [String: FfiSubStream] = [:]
    /// The ObjC wrapper of each remote track, kept while its mid is owned: `receiver.track`
    /// returns a fresh wrapper each time, and a wrapper's dealloc detaches every renderer that
    /// was added through it, so a transient wrapper would leave a tile blank.
    private var remoteTracks: [String: RemoteTrack] = [:]
    private var remoteTracksCallback: (@Sendable ([RemoteTrack]) -> Void)?
    private var localVideoCallback: (@Sendable (RTCVideoTrack) -> Void)?
    /// A capture start timed out: that start may still complete later, so the capture object
    /// is never started again (a second start could be stopped by the first one's clean-up).
    private var captureBroken = false
    private var captureIdleWaiters: [CheckedContinuation<Void, Never>] = []

    private var closing = false
    private var isClosed = false
    private var closedWaiters: [CheckedContinuation<Void, Never>] = []

    init(options: MediaOptions, fence: Fence) {
        EngineCore.initializeSSL
        self.fence = fence
        wantsAudio = options.audio
        capture = options.video
        captureTimeout = options.captureTimeout
        factory = RTCPeerConnectionFactory(
            encoderFactory: RTCDefaultVideoEncoderFactory(),
            decoderFactory: RTCDefaultVideoDecoderFactory(),
            audioDevice: options.audioDevice
        )
    }

    private static let initializeSSL: Void = { RTCInitializeSSL() }()

    /// Run `work` on the engine queue after everything enqueued before it.
    nonisolated func enqueue(_ work: @escaping @Sendable (isolated EngineCore) -> Void) {
        queue.async { self.assumeIsolated { work($0) } }
    }

    // MARK: fence

    private func check() throws {
        if fence.isSet || closing { throw EngineFailure("engine closed") }
    }

    // MARK: publish

    func createPublishOffer() async throws -> String {
        try check()
        let pc = try publish ?? makePublish()
        if capture != nil, media.video { try await syncCapture() }
        try check()
        let offer = try await pc.offer(for: Self.noConstraints)
        try check()
        try await pc.setLocalDescription(offer)
        try check()
        return offer.sdp
    }

    private func makePublish() throws -> RTCPeerConnection {
        let pc = try makePeerConnection(.publish)
        if wantsAudio {
            let track = factory.audioTrack(
                with: factory.audioSource(with: Self.noConstraints), trackId: "mic")
            track.isEnabled = media.audio
            try addSendOnly(pc, track)
            audioTrack = track
        }
        if capture != nil {
            let source = factory.videoSource()
            let track = factory.videoTrack(with: source, trackId: "camera")
            try addSendOnly(pc, track)
            videoSource = source
            localVideoTrack = track
            localVideoCallback?(track)
        }
        publish = pc
        return pc
    }

    private func addSendOnly(_ pc: RTCPeerConnection, _ track: RTCMediaStreamTrack) throws {
        let initial = RTCRtpTransceiverInit()
        initial.direction = .sendOnly  // the default is sendrecv; publish only sends (§3.1)
        initial.streamIds = ["brook"]
        guard pc.addTransceiver(with: track, init: initial) != nil else {
            throw EngineFailure("could not add the \(track.kind) track")
        }
    }

    func applyPublishAnswer(_ sdp: String) async throws {
        try check()
        guard let pc = publish else { throw EngineFailure("answer before the publish offer") }
        try await pc.setRemoteDescription(RTCSessionDescription(type: .answer, sdp: sdp))
        try check()
    }

    // MARK: subscribe

    func applySubscribeOffer(_ sdp: String, _ streams: [FfiSubStream]) async throws -> String {
        try check()
        let pc = try subscribe ?? makeSubscribe()
        try await pc.setRemoteDescription(RTCSessionDescription(type: .offer, sdp: sdp))
        try check()
        for transceiver in pc.transceivers where !transceiver.isStopped {
            // Explicit: whatever the offer's m-line says, the subscribe side only receives.
            var error: NSError?
            transceiver.setDirection(.recvOnly, error: &error)
        }
        let answer = try await pc.answer(for: Self.noConstraints)
        try check()
        try await pc.setLocalDescription(answer)
        try check()
        // Ownership from the latest streams, even when no new track callback fires (mid reuse).
        owners = Dictionary(streams.map { ($0.mid, $0) }, uniquingKeysWith: { _, new in new })
        publishRemoteTracks(pc)
        return answer.sdp
    }

    private func makeSubscribe() throws -> RTCPeerConnection {
        let pc = try makePeerConnection(.subscribe)
        subscribe = pc
        return pc
    }

    func setRemoteTracksCallback(_ callback: @escaping @Sendable ([RemoteTrack]) -> Void) {
        remoteTracksCallback = callback
        if !remoteTracks.isEmpty { callback(remoteTracks.values.sorted { $0.mid < $1.mid }) }
    }

    func setLocalVideoCallback(_ callback: @escaping @Sendable (RTCVideoTrack) -> Void) {
        localVideoCallback = callback
        if let localVideoTrack { callback(localVideoTrack) }
    }

    private func publishRemoteTracks(_ pc: RTCPeerConnection) {
        var next: [String: RemoteTrack] = [:]
        for t in pc.transceivers where !t.isStopped {
            guard let owner = owners[t.mid], let track = t.receiver.track else { continue }
            let kept = remoteTracks[t.mid]
            let same = kept.map { $0.track.trackId == track.trackId } ?? false
            // A reused mid keeps its wrapper (and renderers) and takes the latest owner.
            next[t.mid] = RemoteTrack(
                mid: t.mid, participantId: owner.participantId, kind: owner.kind,
                source: owner.source, track: same ? kept!.track : track)
        }
        remoteTracks = next
        remoteTracksCallback?(next.values.sorted { $0.mid < $1.mid })
    }

    // MARK: candidates and events

    func addRemote(_ kind: FfiPcKind, _ candidate: FfiIceCandidate?) {
        guard !closing, let candidate else { return }  // no end-of-candidates API in the ObjC SDK
        guard let pc = kind == .publish ? publish : subscribe else {
            fail("remote candidate before the \(kind) connection exists")
            return
        }
        let ice = RTCIceCandidate(
            sdp: candidate.candidate,
            sdpMLineIndex: Int32(candidate.sdpMlineIndex ?? 0),
            sdpMid: candidate.sdpMid)
        pc.add(ice) { [weak self] error in
            guard let self, let error else { return }
            self.enqueue { $0.fail("remote candidate: \(error.localizedDescription)") }
        }
    }

    func onLocalCandidate(_ kind: FfiPcKind, _ candidate: FfiIceCandidate?) {
        guard !closing, !fence.isSet else { return }
        emit { $0.localCandidate(pc: kind, candidate: candidate) }
    }

    func fail(_ message: String) {
        guard !closing, !fence.isSet else { return }
        emit { $0.engineFailed(message: message) }
    }

    func attach(_ events: EngineEvents) {
        self.events = events
        attached = true
        if closing || fence.isSet {
            pending = []  // nothing is delivered once close has begun
            return
        }
        let backlog = pending
        pending = []
        // close() may raise the fence (from another thread) mid-replay: check per event.
        for event in backlog where !fence.isSet { event(events) }
    }

    /// Before attachment: buffered. After: delivered, unless the handle is gone (then nothing).
    private func emit(_ event: @escaping (EngineEvents) -> Void) {
        if !attached {
            pending.append(event)
        } else if let events {
            event(events)
        }
    }

    // MARK: local media

    func setMedia(audio: Bool, video: Bool) {
        media = (audio, video)
        audioTrack?.isEnabled = audio
        // Camera off stops capture (the camera and its indicator go off); the track and its
        // transceiver stay, so turning it back on needs no renegotiation.
        guard capture != nil, videoSource != nil else { return }
        Task { [weak self] in
            do {
                try await self?.syncCapture()
            } catch {
                await self?.fail("camera: \(error)")
            }
        }
    }

    /// Bring capture to the intent. One loop at a time; it re-reads the intent after each
    /// await, so toggles made meanwhile are honoured.
    private func syncCapture() async throws {
        guard let capture, let source = videoSource, !captureSyncing else { return }
        if captureBroken { throw EngineFailure("the camera did not start earlier") }
        captureSyncing = true
        defer {
            captureSyncing = false
            let waiters = captureIdleWaiters
            captureIdleWaiters = []
            for w in waiters { w.resume() }
        }
        while !closing && media.video != captureRunning {
            if media.video {
                do {
                    try await startBounded(capture, source)
                } catch let error as CaptureTimeout {
                    captureBroken = true
                    throw EngineFailure(error.description)
                }
                captureRunning = true
                if fence.isSet || closing {
                    await capture.stop()
                    captureRunning = false
                    throw EngineFailure("engine closed")
                }
            } else {
                await capture.stop()
                captureRunning = false
            }
        }
    }

    /// Start capture, bounded by `captureTimeout`. A start that completes after the bound (or
    /// after the fence) is stopped at once, so capture never runs without a caller.
    private func startBounded(_ capture: VideoCapture, _ source: RTCVideoSource) async throws {
        let timeout = captureTimeout
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            let once = Once()
            Task {
                do {
                    try await capture.start(into: source)
                    if once.claim() {
                        cont.resume()  // in time; the caller re-checks the fence
                    } else {
                        await capture.stop()  // too late: the operation already failed
                    }
                } catch {
                    if once.claim() { cont.resume(throwing: error) }
                }
            }
            Task {
                try? await Task.sleep(for: timeout)
                if once.claim() { cont.resume(throwing: CaptureTimeout(timeout: timeout)) }
            }
        }
    }

    func setIceServers(_ servers: [FfiIceServer]) {
        iceServers = servers.map {
            RTCIceServer(urlStrings: $0.urls, username: $0.username, credential: $0.credential)
        }
        for pc in [publish, subscribe].compactMap({ $0 }) {
            let config = pc.configuration
            config.iceServers = iceServers
            _ = pc.setConfiguration(config)
        }
    }

    // MARK: close

    func close() async {
        if closing {
            await waitClosed()
            return
        }
        closing = true
        pending = []
        // A capture sync in flight (bounded by the capture timeout) finishes first: it sees the
        // fence and stops what it started, so `closed` never precedes a start.
        if captureSyncing {
            await withCheckedContinuation { captureIdleWaiters.append($0) }
        }
        if let capture, captureRunning || capture.isCapturing {
            await capture.stop()
            captureRunning = false
        }
        publish?.close()
        subscribe?.close()
        publish = nil
        subscribe = nil
        delegates = [:]
        audioTrack = nil
        videoSource = nil
        localVideoTrack = nil
        remoteTracks = [:]
        pending = []
        isClosed = true
        let waiters = closedWaiters
        closedWaiters = []
        for waiter in waiters { waiter.resume() }
    }

    func waitClosed() async {
        if isClosed { return }
        await withCheckedContinuation { closedWaiters.append($0) }
    }

    // MARK: helpers

    private func makePeerConnection(_ kind: FfiPcKind) throws -> RTCPeerConnection {
        let config = RTCConfiguration()
        config.sdpSemantics = .unifiedPlan
        config.bundlePolicy = .maxBundle
        config.rtcpMuxPolicy = .require
        config.continualGatheringPolicy = .gatherOnce
        config.iceServers = iceServers
        let delegate = PeerDelegate(kind: kind, core: self)
        guard
            let pc = factory.peerConnection(
                with: config, constraints: Self.noConstraints, delegate: delegate)
        else { throw EngineFailure("could not create the \(kind) connection") }
        delegates[kind] = delegate
        return pc
    }

    func selectedCandidatePair(_ kind: FfiPcKind) async -> String? {
        guard let pc = kind == .publish ? publish : subscribe else { return nil }
        let report = await pc.statistics()
        let stats = report.statistics
        for s in stats.values where s.type == "candidate-pair" {
            let nominated = (s.values["nominated"] as? NSNumber)?.boolValue ?? false
            guard nominated, (s.values["state"] as? String) == "succeeded" else { continue }
            let local = (s.values["localCandidateId"] as? String).flatMap { stats[$0] }
            let remote = (s.values["remoteCandidateId"] as? String).flatMap { stats[$0] }
            func describe(_ c: RTCStatistics?) -> String {
                let v = c?.values ?? [:]
                let type = v["candidateType"] as? String ?? "?"
                let proto = v["protocol"] as? String ?? "?"
                return "\(type) \(proto)"
            }
            return "\(describe(local)) ↔ \(describe(remote))"
        }
        return nil
    }

    func statistics(_ kind: FfiPcKind, type: String) async -> [[String: String]] {
        guard let pc = kind == .publish ? publish : subscribe else { return [] }
        let report = await pc.statistics()
        return report.statistics.values.filter { $0.type == type }.map { s in
            s.values.mapValues { "\($0)" }
        }
    }

    private static let noConstraints = RTCMediaConstraints(
        mandatoryConstraints: nil, optionalConstraints: nil)
}

/// Resumes a continuation exactly once across racing tasks.
final class Once: Sendable {
    private let state = Mutex<Bool>(false)
    /// True for the first caller only.
    func claim() -> Bool { state.withLock { done in defer { done = true }; return !done } }
}

/// WebRTC calls its delegate on its own signaling thread: hop onto the engine queue at once.
final class PeerDelegate: NSObject, RTCPeerConnectionDelegate, @unchecked Sendable {
    private let kind: FfiPcKind
    private weak var core: EngineCore?

    init(kind: FfiPcKind, core: EngineCore) {
        self.kind = kind
        self.core = core
    }

    func peerConnection(_ pc: RTCPeerConnection, didGenerate candidate: RTCIceCandidate) {
        let ice = FfiIceCandidate(
            candidate: candidate.sdp, sdpMid: candidate.sdpMid,
            sdpMlineIndex: UInt32(max(0, candidate.sdpMLineIndex)))
        let kind = kind
        core?.enqueue { $0.onLocalCandidate(kind, ice) }
    }

    func peerConnection(_ pc: RTCPeerConnection, didChange newState: RTCIceGatheringState) {
        guard newState == .complete else { return }
        let kind = kind
        core?.enqueue { $0.onLocalCandidate(kind, nil) }  // end-of-candidates
    }

    func peerConnection(_ pc: RTCPeerConnection, didChange newState: RTCPeerConnectionState) {
        guard newState == .failed else { return }
        let kind = kind
        core?.enqueue { $0.fail("\(kind) connection failed") }
    }

    func peerConnection(_ pc: RTCPeerConnection, didChange stateChanged: RTCSignalingState) {}
    func peerConnection(_ pc: RTCPeerConnection, didAdd stream: RTCMediaStream) {}
    func peerConnection(_ pc: RTCPeerConnection, didRemove stream: RTCMediaStream) {}
    func peerConnectionShouldNegotiate(_ pc: RTCPeerConnection) {}
    func peerConnection(_ pc: RTCPeerConnection, didChange newState: RTCIceConnectionState) {}
    func peerConnection(_ pc: RTCPeerConnection, didRemove candidates: [RTCIceCandidate]) {}
    func peerConnection(_ pc: RTCPeerConnection, didOpen dataChannel: RTCDataChannel) {}
}
