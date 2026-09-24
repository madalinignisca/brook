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
}
