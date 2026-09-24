import BrookCore
import BrookMedia
import Foundation
import Observation
import Synchronization
@preconcurrency import WebRTC

/// The engine side the call UI needs (WebRTCEngine; a fake in tests).
protocol CallMedia: AnyObject, Sendable {
    func closed() async
    func localVideoTrack() async -> RTCVideoTrack?
    func onRemoteTracks(_ callback: @escaping @Sendable ([RemoteTrack]) -> Void)
}

extension WebRTCEngine: CallMedia {}

/// One live call: roster tiles, local controls, banners, and a bounded leave.
@MainActor
@Observable
final class CallModel {
    struct Tile: Identifiable {
        let id: String
        let name: String
        let isSelf: Bool
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
    private var remote: [String: RTCVideoTrack] = [:]  // participant id → camera track

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
    }

    func start() async {
        stateSubscription = handle.subscribeState(listener: StateBridge(self))
        media.onRemoteTracks { [weak self] tracks in
            let video = tracks.filter { $0.kind == .video && $0.source == .camera }
            let byParticipant = Dictionary(
                video.compactMap { t in (t.track as? RTCVideoTrack).map { (t.participantId, $0) } },
                uniquingKeysWith: { first, _ in first })
            nonisolated(unsafe) let owned = byParticipant
            DispatchQueue.main.async {
                MainActor.assumeIsolated { self?.remote = owned }
            }
        }
        localVideo = await media.localVideoTrack()
    }

    func apply(_ state: FfiCallState) { self.state = state }

    /// Self first (from local state: core's roster excludes self), then everyone else.
    var tiles: [Tile] {
        guard let state, !isEnded else { return [] }
        let me = Tile(
            id: state.selfParticipant ?? "self", name: "You", isSelf: true, audio: micOn,
            video: cameraOn, track: cameraOn ? localVideo : nil)
        return [me] + state.participants.filter { $0.participantId != state.selfParticipant }.map { p in
            Tile(
                id: p.participantId, name: p.displayName, isSelf: false, audio: p.audio,
                video: p.video, track: remote[p.participantId])
        }
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

    private func setMedia(audio: Bool, video: Bool) async {
        let before = (micOn, cameraOn)
        (micOn, cameraOn) = (audio, video)  // the control answers at once
        do {
            try await handle.setMedia(audio: audio, video: video)
        } catch {
            (micOn, cameraOn) = before  // refused: show what is really sent
        }
    }

    /// Leave, then wait for the engine to close (capture provably stopped), bounded.
    func leave() async {
        guard !leaving else { return }
        leaving = true
        stateSubscription?.cancel()
        try? await handle.leave()
        await firstOf({ [media] in await media.closed() }, orAfter: closeBound)
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
