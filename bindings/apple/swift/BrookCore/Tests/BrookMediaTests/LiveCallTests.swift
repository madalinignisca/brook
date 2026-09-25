@preconcurrency import WebRTC
import BrookCore
import Foundation
import Synchronization
import XCTest

@testable import BrookMedia

final class LiveLog: ServerEventListener, CallStateListener, @unchecked Sendable {
    private let events = Mutex<[FfiServerEvent]>([])
    private let states = Mutex<[FfiCallState]>([])
    func onEvent(event: FfiServerEvent) { events.withLock { $0.append(event) } }
    func onState(state: FfiCallState) { states.withLock { $0.append(state) } }
    var ready: Bool { events.withLock { $0.contains(.ready) } }
    var last: FfiCallState? { states.withLock { $0.last } }
}

/// The real engine against the shared test server (configured by itest.sh), synthetic media
/// only: no camera, microphone or TCC. With BROOK_TEST_EXPECT_PEER=1 a second participant
/// (the Linux headless publisher) must be in the channel, and its media must decode here.
final class LiveCallTests: XCTestCase {
    func testRealEngineJoinsPublishesAndLeaves() async throws {
        let e = ProcessInfo.processInfo.environment
        guard let server = e["BROOK_TEST_SERVER"], let handleName = e["BROOK_TEST_HANDLE"],
              let password = e["BROOK_TEST_PASSWORD"], let channel = e["BROOK_TEST_CHANNEL"]
        else {
            if e["BROOK_REQUIRE_ITEST"] == "1" { XCTFail("integration environment not set") }
            throw XCTSkip("BROOK_TEST_SERVER / _HANDLE / _PASSWORD / _CHANNEL not set")
        }
        let expectPeer = e["BROOK_TEST_EXPECT_PEER"] == "1"

        let client = try FfiBrookClient(
            baseUrl: server, allowInsecureHttp: e["BROOK_TEST_ALLOW_INSECURE_HTTP"] == "1")
        _ = try await client.login(handle: handleName, password: password)
        let log = LiveLog()
        let events = client.subscribeEvents(listener: log)
        defer { events.cancel() }
        try await client.startRealtime()
        await eventually("Ready") { log.ready }

        let device = SyntheticAudioDevice(toneHz: 440)  // mic tone + playout meter
        let engine = WebRTCEngine(options: MediaOptions(
            audio: true, video: SyntheticVideoCapture(), audioDevice: device))
        let frames = FrameCounter()
        engine.onRemoteTracks { tracks in
            for t in tracks where t.kind == .video { (t.track as? RTCVideoTrack)?.add(frames) }
        }
        let call = try await client.joinCall(channelId: channel, engine: engine, publish: true)
        engine.attach(call)
        let state = call.subscribeState(listener: log)
        defer { state.cancel() }

        await eventually("Connected") { log.last?.status == .connected }
        await eventually("ICE to the SFU on the publish connection", timeout: 30) {
            await engine.selectedCandidatePair(.publish) != nil
        }
        await eventually("media sent to the SFU", timeout: 30) {
            let out = await engine.statistics(.publish, type: "outbound-rtp")
            return out.contains { ($0["kind"] == "video") && (Int($0["framesEncoded"] ?? "0") ?? 0) > 30 }
        }
        let pair = await engine.selectedCandidatePair(.publish)
        print("LIVE publish pair: \(pair ?? "-")")
        let codecs = await engine.statistics(.publish, type: "codec").map { $0["mimeType"] ?? "?" }
        let sent = await engine.statistics(.publish, type: "outbound-rtp")
            .map { "\($0["kind"] ?? "?"): \($0["encoderImplementation"] ?? "-") codec=\($0["codecId"] ?? "-")" }
        print("LIVE publish codecs in use: \(codecs) \(sent)")

        if expectPeer {
            // The roster excludes self: the peer is the only entry.
            await eventually("the peer in the roster", timeout: 30) {
                (log.last?.participants.count ?? 0) >= 1
            }
            print("LIVE roster: \(log.last?.participants.map { "\($0.displayName) a=\($0.audio) v=\($0.video)" } ?? [])")
            await eventually("peer video decoded here", timeout: 30) { frames.frames > 30 }
            await eventually("peer audio audible here", timeout: 30) { device.audibleBuffers > 50 }
            let received = await engine.statistics(.subscribe, type: "codec").map { $0["mimeType"] ?? "?" }
            print("LIVE subscribe codecs in use: \(received)")
            let codecs = await engine.statistics(.subscribe, type: "inbound-rtp")
                .map { "\($0["kind"] ?? "?"): \($0["decoderImplementation"] ?? "-") \($0["framesDecoded"] ?? "")" }
            print("LIVE subscribe: \(codecs)")
        }

        try await call.leave()
        await eventually("Ended(Left)") { log.last?.status == .ended(reason: .left) }
        await engine.closed()
    }
}
