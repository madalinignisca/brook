@preconcurrency import WebRTC
import BrookCore
import Foundation
import Synchronization
import XCTest

@testable import BrookMedia

/// Routes one engine's candidates to the other's matching connection, holding them until that
/// connection has its remote description (core does the same ordering in production).
final class LoopbackWire: @unchecked Sendable {
    final class End: EngineEvents, @unchecked Sendable {
        let forward: @Sendable (FfiIceCandidate?) -> Void
        let failures = Mutex<[String]>([])
        init(_ forward: @escaping @Sendable (FfiIceCandidate?) -> Void) { self.forward = forward }
        func localCandidate(pc: FfiPcKind, candidate: FfiIceCandidate?) { forward(candidate) }
        func engineFailed(message: String) { failures.withLock { $0.append(message) } }
    }

    private struct Gate {
        var open = false
        var held: [FfiIceCandidate?] = []
    }

    private let toB = Locked(Gate())
    private let toA = Locked(Gate())
    let fromA: End
    let fromB: End

    init(publisher a: WebRTCEngine, subscriber b: WebRTCEngine) {
        let toB = toB, toA = toA
        fromA = End { c in
            let deliver = toB.withLock { g -> Bool in if !g.open { g.held.append(c) }; return g.open }
            if deliver { try? b.addRemoteCandidate(pc: .subscribe, candidate: c) }
        }
        fromB = End { c in
            let deliver = toA.withLock { g -> Bool in if !g.open { g.held.append(c) }; return g.open }
            if deliver { try? a.addRemoteCandidate(pc: .publish, candidate: c) }
        }
        self.a = a
        self.b = b
    }

    private let a: WebRTCEngine
    private let b: WebRTCEngine

    func subscriberHasOffer() {
        for c in toB.withLock({ g -> [FfiIceCandidate?] in g.open = true; defer { g.held = [] }; return g.held }) {
            try? b.addRemoteCandidate(pc: .subscribe, candidate: c)
        }
    }

    func publisherHasAnswer() {
        for c in toA.withLock({ g -> [FfiIceCandidate?] in g.open = true; defer { g.held = [] }; return g.held }) {
            try? a.addRemoteCandidate(pc: .publish, candidate: c)
        }
    }

    var failures: [String] { fromA.failures.withLock { $0 } + fromB.failures.withLock { $0 } }
}

final class FrameCounter: NSObject, RTCVideoRenderer, @unchecked Sendable {
    private let count = Atomic<Int>(0)
    var frames: Int { count.load(ordering: .relaxed) }
    func setSize(_ size: CGSize) {}
    func renderFrame(_ frame: RTCVideoFrame?) {
        if frame != nil { count.add(1, ordering: .relaxed) }
    }
}

/// The m-lines of an SDP in order, as (mid, kind).
func mediaSections(_ sdp: String) -> [(mid: String, kind: FfiMediaKind)] {
    var result: [(String, FfiMediaKind)] = []
    var kind: FfiMediaKind?
    for line in sdp.split(whereSeparator: \.isNewline) {
        if line.hasPrefix("m=audio") { kind = .audio }
        if line.hasPrefix("m=video") { kind = .video }
        if line.hasPrefix("a=mid:"), let k = kind { result.append((String(line.dropFirst(6)), k)) }
    }
    return result
}

func eventually(
    _ what: String, timeout: TimeInterval = 20, file: StaticString = #filePath, line: UInt = #line,
    _ ok: () async -> Bool
) async {
    let deadline = Date().addingTimeInterval(timeout)
    while Date() < deadline {
        if await ok() { return }
        try? await Task.sleep(for: .milliseconds(100))
    }
    XCTFail("timed out waiting for \(what)", file: file, line: line)
}

final class LoopbackTests: XCTestCase {
    /// Engine A publishes a synthetic camera and tone; engine B subscribes. Media must actually
    /// flow: frames decoded on B, the tone audible in B's playout, a nominated candidate pair.
    func testPublishToSubscribeCarriesVideoAndAudio() async throws {
        let mic = SyntheticAudioDevice(toneHz: 440)
        let speaker = SyntheticAudioDevice(toneHz: nil)
        let camera = SyntheticVideoCapture()
        let a = WebRTCEngine(options: MediaOptions(audio: true, video: camera, audioDevice: mic))
        let b = WebRTCEngine(options: MediaOptions(audio: false, video: nil, audioDevice: speaker))
        let wire = LoopbackWire(publisher: a, subscriber: b)
        a.attach(wire.fromA)
        b.attach(wire.fromB)
        let frames = FrameCounter()
        b.onRemoteTracks { tracks in
            for t in tracks where t.kind == .video { (t.track as? RTCVideoTrack)?.add(frames) }
        }

        let offer = try await a.createPublishOffer()
        let sections = mediaSections(offer)
        XCTAssertEqual(sections.map(\.kind), [.audio, .video], offer)
        let streams = sections.map {
            FfiSubStream(
                mid: $0.mid, participantId: "p-a", kind: $0.kind,
                source: $0.kind == .audio ? .mic : .camera)
        }
        let answer = try await b.applySubscribeOffer(sdp: offer, streams: streams)
        wire.subscriberHasOffer()
        try await a.applyPublishAnswer(sdp: answer)
        wire.publisherHasAnswer()

        // Counted by a renderer added inside the callback: the engine must keep the track's
        // wrapper alive, or its dealloc detaches the renderer and no frame is ever counted.
        await eventually("decoded video frames on the subscriber") { frames.frames > 30 }
        await eventually("the tone audible in the subscriber's playout") { speaker.audibleBuffers > 50 }
        let pair = await b.selectedCandidatePair(.subscribe)
        XCTAssertNotNil(pair, "no nominated candidate pair")
        XCTAssertEqual(wire.failures, [])

        await a.close()
        await b.close()
        XCTAssertFalse(camera.isCapturing, "capture still running after close")
    }

    /// Re-offers update ownership from the latest `streams` even though no new track callback
    /// fires: a reused mid keeps its track (and renderers) and takes the new owner; a mid no
    /// longer listed disappears.
    func testReofferMovesOwnershipAndKeepsTheTrack() async throws {
        let a = WebRTCEngine(options: MediaOptions(
            audio: true, video: SyntheticVideoCapture(), audioDevice: SyntheticAudioDevice(toneHz: 440)))
        let b = WebRTCEngine(options: MediaOptions(
            audio: false, video: nil, audioDevice: SyntheticAudioDevice(toneHz: nil)))
        let wire = LoopbackWire(publisher: a, subscriber: b)
        a.attach(wire.fromA)
        b.attach(wire.fromB)
        let seen = Locked([[RemoteTrack]]())
        b.onRemoteTracks { tracks in seen.withLock { $0.append(tracks) } }

        func negotiate(_ owners: [String: String]) async throws {
            let offer = try await a.createPublishOffer()
            let streams = mediaSections(offer).compactMap { s -> FfiSubStream? in
                owners[s.mid].map {
                    FfiSubStream(mid: s.mid, participantId: $0, kind: s.kind,
                                 source: s.kind == .audio ? .mic : .camera)
                }
            }
            let answer = try await b.applySubscribeOffer(sdp: offer, streams: streams)
            wire.subscriberHasOffer()
            try await a.applyPublishAnswer(sdp: answer)
            wire.publisherHasAnswer()
        }

        try await negotiate(["0": "p-a", "1": "p-a"])
        try await negotiate(["1": "p-b"])  // mid 1 reused by another participant; mid 0 unlisted

        let rounds = seen.withLock { $0 }
        XCTAssertEqual(rounds.count, 2)
        let first = Dictionary(uniqueKeysWithValues: rounds[0].map { ($0.mid, $0) })
        let second = Dictionary(uniqueKeysWithValues: rounds[1].map { ($0.mid, $0) })
        XCTAssertEqual(Set(first.keys), ["0", "1"])
        XCTAssertEqual(Set(second.keys), ["1"], "an unlisted mid still has an owner")
        XCTAssertEqual(second["1"]?.participantId, "p-b")
        XCTAssertTrue(first["1"]!.track === second["1"]!.track, "the reused mid got a new track wrapper")
        await a.close()
        await b.close()
    }

    /// A callback registered after the first subscribe offer still gets the current tracks
    /// (the UI registers after join_call returns, which can be after that offer).
    func testLateRemoteTrackCallbackGetsTheCurrentTracks() async throws {
        let a = WebRTCEngine(options: MediaOptions(
            audio: true, video: SyntheticVideoCapture(), audioDevice: SyntheticAudioDevice(toneHz: nil)))
        let b = WebRTCEngine(options: MediaOptions(
            audio: false, video: nil, audioDevice: SyntheticAudioDevice(toneHz: nil)))
        let offer = try await a.createPublishOffer()
        let streams = mediaSections(offer).map {
            FfiSubStream(mid: $0.mid, participantId: "p-a", kind: $0.kind, source: $0.kind == .audio ? .mic : .camera)
        }
        _ = try await b.applySubscribeOffer(sdp: offer, streams: streams)
        let seen = Locked([String]())
        b.onRemoteTracks { tracks in seen.withLock { $0 = tracks.map(\.mid) } }
        await eventually("current tracks replayed to a late callback", timeout: 2) {
            seen.withLock { $0.count } == 2
        }
        await a.close()
        await b.close()
    }

    /// Screen share on the same publish connection: the share's m-line is labelled `screen`
    /// and decodes on the other side; stopping leaves it inactive (still labelled, same mid);
    /// sharing again reuses that mid (never a recycled slot).
    func testScreenShareStartStopRestartOnOneMline() async throws {
        let a = WebRTCEngine(options: MediaOptions(
            audio: true, video: SyntheticVideoCapture(), audioDevice: SyntheticAudioDevice(toneHz: nil)))
        let b = WebRTCEngine(options: MediaOptions(
            audio: false, video: nil, audioDevice: SyntheticAudioDevice(toneHz: nil)))
        let wire = LoopbackWire(publisher: a, subscriber: b)
        a.attach(wire.fromA)
        b.attach(wire.fromB)
        let screenFrames = FrameCounter()
        let attached = Locked(Set<String>())
        b.onRemoteTracks { tracks in
            for t in tracks where t.source == .screen {
                let fresh = attached.withLock { $0.insert(t.mid).inserted }
                if fresh { (t.track as? RTCVideoTrack)?.add(screenFrames) }
            }
        }
        func negotiate() async throws -> FfiPublishOffer {
            let offer = try await a.createLabelledOffer()
            let sources = Dictionary(uniqueKeysWithValues: offer.tracks.map { ($0.mid, $0.source) })
            let streams = mediaSections(offer.sdp).map {
                FfiSubStream(mid: $0.mid, participantId: "p-a", kind: $0.kind, source: sources[$0.mid] ?? .unknown)
            }
            let answer = try await b.applySubscribeOffer(sdp: offer.sdp, streams: streams)
            wire.subscriberHasOffer()
            try await a.applyPublishAnswer(sdp: answer)
            wire.publisherHasAnswer()
            return offer
        }

        let first = try await negotiate()
        XCTAssertEqual(first.tracks.map(\.source), [.mic, .camera])

        try await a.startScreenShare(SyntheticVideoCapture(width: 1280, height: 720))
        let sharing = try await negotiate()
        let screen = try XCTUnwrap(sharing.tracks.first { $0.source == .screen }, "\(sharing.tracks)")
        XCTAssertEqual(sharing.tracks.count, 3)
        await eventually("screen frames decoded on the other side") { screenFrames.frames > 20 }

        await a.stopScreenShare()
        let stopped = try await negotiate()
        XCTAssertEqual(stopped.tracks.first { $0.mid == screen.mid }?.source, .screen, "stopped share lost its label")
        XCTAssertTrue(section(stopped.sdp, mid: screen.mid).contains("a=inactive"), "stopped share not inactive")
        let asleep = screenFrames.frames
        try? await Task.sleep(for: .milliseconds(500))
        XCTAssertLessThan(screenFrames.frames - asleep, 5, "frames still flowing after stop")

        try await a.startScreenShare(SyntheticVideoCapture(width: 1280, height: 720))
        let again = try await negotiate()
        XCTAssertEqual(again.tracks.filter { $0.source == .screen }.map(\.mid), [screen.mid], "not the same m-line")
        XCTAssertEqual(mediaSections(again.sdp).count, 3, "a new m-line was added")
        let before = screenFrames.frames
        await eventually("screen frames again after restart") { screenFrames.frames > before + 20 }
        await a.close()
        await b.close()
    }
}

/// The attribute lines of one m-section of an SDP, by mid.
func section(_ sdp: String, mid: String) -> String {
    var sections: [[Substring]] = []
    for line in sdp.split(whereSeparator: \.isNewline) {
        if line.hasPrefix("m=") { sections.append([line]) } else if !sections.isEmpty { sections[sections.count - 1].append(line) }
    }
    return sections.first { $0.contains("a=mid:\(mid)") }?.joined(separator: "\n") ?? ""
}
