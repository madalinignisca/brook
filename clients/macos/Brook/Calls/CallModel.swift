import BrookCore
import BrookMedia
import Foundation
import Observation
import Synchronization
@preconcurrency import WebRTC

/// The engine side the call UI needs (WebRTCEngine; a fake in tests).
protocol CallMedia: AnyObject, Sendable {
    func closed() async
    func startScreenShare(_ capture: VideoCapture) async throws
    func stopScreenShare() async
    func onScreenShareEnded(_ callback: @escaping @Sendable () -> Void)
    func onLocalVideoTrack(_ callback: @escaping @Sendable (RTCVideoTrack) -> Void)
    func onRemoteTracks(_ callback: @escaping @Sendable ([RemoteTrack]) -> Void)
}

/// What joining needs from an engine: core's side, the UI's side, and the attachment.
protocol CallEngine: FfiMediaEngine, CallMedia {
    func attach(_ events: EngineEvents)
}

extension WebRTCEngine: CallEngine {}

/// One live call: roster tiles, local controls, banners, and a bounded leave.
@MainActor
@Observable
final class CallModel {
    struct Tile: Identifiable {
        let id: String
        let name: String
        let isSelf: Bool
        var isScreen = false
        let audio: Bool
        let video: Bool
        let track: RTCVideoTrack?
    }

    let channelName: String
    let plan: JoinPlan
    private(set) var state: FfiCallState?
    private(set) var micOn: Bool
    private(set) var cameraOn: Bool
    private(set) var localVideo: RTCVideoTrack?
    private(set) var leaving = false
    private var leaveTask: Task<Void, Never>?
    private var mediaGeneration = 0
    /// The last state core accepted, and the chain that sends requests one at a time.
    private var confirmed: (audio: Bool, video: Bool)
    private var mediaChain: Task<Void, Never>?
    private var remote: [String: RTCVideoTrack] = [:]  // participant id → camera track
    private var remoteScreens: [String: RTCVideoTrack] = [:]  // participant id → screen track
    private(set) var sharing = false
    private(set) var sharingBusy = false
    private(set) var shareError: String?
    /// The system ended the share while it was still starting (before `sharing` was set).
    private var endedWhileStarting = false

    private let handle: any FfiCallHandleProtocol
    private let media: CallMedia
    private let closeBound: Duration
    private var stateSubscription: Subscription?

    init(
        channelName: String, plan: JoinPlan, handle: any FfiCallHandleProtocol, media: CallMedia,
        closeBound: Duration = .seconds(5)
    ) {
        self.channelName = channelName
        self.plan = plan
        self.handle = handle
        self.media = media
        self.closeBound = closeBound
        micOn = plan.microphone
        cameraOn = plan.camera
        confirmed = (plan.microphone, plan.camera)
    }

    func start() async {
        stateSubscription = handle.subscribeState(listener: StateBridge(self))
        media.onRemoteTracks { [weak self] tracks in
            func byParticipant(_ source: FfiMediaSource) -> [String: RTCVideoTrack] {
                Dictionary(
                    tracks.filter { $0.kind == .video && $0.source == source }
                        .compactMap { t in (t.track as? RTCVideoTrack).map { (t.participantId, $0) } },
                    uniquingKeysWith: { first, _ in first })
            }
            nonisolated(unsafe) let cameras = byParticipant(.camera)
            nonisolated(unsafe) let screens = byParticipant(.screen)
            DispatchQueue.main.async {
                MainActor.assumeIsolated {
                    self?.remote = cameras
                    self?.remoteScreens = screens
                }
            }
        }
        // The system ended the share (menu bar, display gone): the engine already set the
        // m-line inactive; tell the others.
        media.onScreenShareEnded { [weak self] in
            DispatchQueue.main.async {
                MainActor.assumeIsolated { self?.screenShareEnded() }
            }
        }
        // The camera track appears when the publish offer is built, which can be after this.
        media.onLocalVideoTrack { [weak self] track in
            nonisolated(unsafe) let track = track
            DispatchQueue.main.async {
                MainActor.assumeIsolated { self?.localVideo = track }
            }
        }
    }

    func apply(_ state: FfiCallState) {
        self.state = state
        if isEnded { sharing = false }  // the engine closed with the call
    }

    /// Self first (from local state: core's roster excludes self), then everyone else.
    var tiles: [Tile] {
        guard let state, !isEnded else { return [] }
        let me = Tile(
            id: state.selfParticipant ?? "self", name: "You", isSelf: true, audio: micOn,
            video: cameraOn, track: cameraOn ? localVideo : nil)
        let others = state.participants.filter { $0.participantId != state.selfParticipant }
        // Shared screens first: they are what everyone is looking at.
        let screens = others.compactMap { p -> Tile? in
            guard let track = remoteScreens[p.participantId] else { return nil }
            return Tile(
                id: p.participantId + ".screen", name: "\(p.displayName)'s screen", isSelf: false,
                isScreen: true, audio: true, video: true, track: track)
        }
        return screens + [me] + others.map { p in
            Tile(
                id: p.participantId, name: p.displayName, isSelf: false, audio: p.audio,
                video: p.video, track: remote[p.participantId])
        }
    }

    /// Share `capture` (the screen the user picked): a new or re-enabled m-line, then a
    /// renegotiation. Only listen-only calls cannot share (there is no publish connection).
    func shareScreen(_ capture: VideoCapture) async {
        guard plan.publishes, !sharing, !sharingBusy, !isEnded else { return }
        sharingBusy = true
        defer { sharingBusy = false }
        shareError = nil
        endedWhileStarting = false
        do {
            try await media.startScreenShare(capture)
        } catch {
            shareError = "Couldn't share the screen."
            return
        }
        do {
            try await handle.republish()
            // Ended meanwhile (by the system, or the call): it isn't sharing. The engine has
            // already set the m-line inactive; renegotiate once more if the call goes on.
            if endedWhileStarting || isEnded {
                if !isEnded { try? await handle.republish() }
                return
            }
            sharing = true
        } catch {
            await media.stopScreenShare()
            shareError = "Couldn't share the screen."
        }
    }

    func screenShareEnded() {
        if sharingBusy, !sharing {
            endedWhileStarting = true
            return
        }
        guard sharing else { return }
        sharing = false
        Task { [handle] in try? await handle.republish() }
    }

    func stopSharing() async {
        guard sharing, !sharingBusy else { return }
        sharingBusy = true
        defer { sharingBusy = false }
        await media.stopScreenShare()
        sharing = false
        try? await handle.republish()
    }

    /// Reconnecting / Ended banner text; nil while connected or joining.
    var banner: String? {
        switch state?.status {
        case .reconnecting: "Reconnecting…"
        case let .ended(reason): Self.endedText(reason)
        default: nil
        }
    }

    var isEnded: Bool {
        if case .ended = state?.status { return true }
        return false
    }

    static func endedText(_ reason: FfiEndReason) -> String {
        switch reason {
        case .left: "You left the call."
        case .sfuRestart: "The call was interrupted by a server restart."
        case .removed: "You were removed from the call."
        case .replaced: "You joined this call from another window or device."
        case .expired: "The connection was lost for too long."
        case .sessionChanged: "You signed out."
        case .engineFailed: "Media stopped working on this Mac."
        case .server, .unknown: "The call ended."
        }
    }

    func toggleMic() async {
        guard plan.microphone else { return }
        await setMedia(audio: !micOn, video: cameraOn)
    }

    func toggleCamera() async {
        guard plan.camera else { return }
        await setMedia(audio: micOn, video: !cameraOn)
    }

    /// The control answers at once with the newest request. Requests reach core one at a time,
    /// so their outcomes arrive in order: an accepted one becomes `confirmed`, and when the
    /// newest is refused the UI shows `confirmed`, which is what core rolls back to.
    private func setMedia(audio: Bool, video: Bool) async {
        (micOn, cameraOn) = (audio, video)
        mediaGeneration += 1
        let generation = mediaGeneration
        let previous = mediaChain
        let task = Task { [handle] in
            await previous?.value
            let accepted = (try? await handle.setMedia(audio: audio, video: video)) != nil
            if accepted { self.confirmed = (audio, video) }
            if generation == self.mediaGeneration, !accepted {
                (self.micOn, self.cameraOn) = self.confirmed
            }
        }
        mediaChain = task
        await task.value
    }

    /// Leave, then wait for the engine to close (capture provably stopped); the whole of it
    /// bounded. Every caller (Leave, quit) awaits the same leave.
    func leave() async {
        if let leaveTask {
            await leaveTask.value
            return
        }
        leaving = true
        stateSubscription?.cancel()
        let task = Task { [handle, media, closeBound] in
            await firstOf({
                try? await handle.leave()
                await media.closed()
            }, orAfter: closeBound)
        }
        leaveTask = task
        await task.value
    }
}

/// Waits for `work` or the bound, whichever comes first. Not a task group: a group waits for
/// every child, and a close that never completes (and ignores cancellation) would hang it.
func firstOf(_ work: @escaping @Sendable () async -> Void, orAfter bound: Duration) async {
    await withCheckedContinuation { (cont: CheckedContinuation<Void, Never>) in
        let done = Once()
        let finish: @Sendable () -> Void = { if done.claim() { cont.resume() } }
        Task {
            await work()
            finish()
        }
        Task {
            try? await Task.sleep(for: bound)
            finish()
        }
    }
}

/// True for the first caller only.
final class Once: Sendable {
    private let state = Mutex(false)
    func claim() -> Bool { state.withLock { done in defer { done = true }; return !done } }
}

final class StateBridge: CallStateListener, @unchecked Sendable {
    private weak var model: CallModel?
    init(_ model: CallModel) { self.model = model }

    func onState(state: FfiCallState) {
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { self?.model?.apply(state) }
        }
    }
}
