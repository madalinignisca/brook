import Foundation
import Synchronization
import XCTest

@testable import BrookCore

/// A Swift media engine driven by Rust core: records what core asks of it.
final class RecordingEngine: FfiMediaEngine, @unchecked Sendable {
    private let entries = Mutex<[String]>([])
    let offer: String

    init(offer: String) { self.offer = offer }

    var log: [String] { entries.withLock { $0 } }
    private func record(_ entry: String) { entries.withLock { $0.append(entry) } }

    func createPublishOffer() async throws -> String {
        record("createPublishOffer")
        return offer
    }
    func applyPublishAnswer(sdp: String) async throws { record("applyPublishAnswer") }
    /// Other participants in the shared channel cause subscribe offers. A fake can't answer
    /// them, and failing would end the call, so they are held unanswered until close()
    /// (the contract lets an operation stay pending until the fence).
    func applySubscribeOffer(sdp: String, streams: [FfiSubStream]) async throws -> String {
        record("applySubscribeOffer")
        await withCheckedContinuation { c in
            let closed = held.withLock { h -> Bool in
                if !h.closed { h.waiters.append(c) }
                return h.closed
            }
            if closed { c.resume() }
        }
        throw FfiEngineError.Failed(message: "closed")
    }
    func addRemoteCandidate(pc: FfiPcKind, candidate: FfiIceCandidate?) throws { record("addRemoteCandidate") }
    func setLocalMedia(audio: Bool, video: Bool) throws { record("setLocalMedia") }
    func setIceServers(servers: [FfiIceServer]) { record("setIceServers") }
    func close() async {
        record("close")
        let waiters = held.withLock { h -> [CheckedContinuation<Void, Never>] in
            h.closed = true
            defer { h.waiters = [] }
            return h.waiters
        }
        waiters.forEach { $0.resume() }
    }
    private let held = Mutex<(closed: Bool, waiters: [CheckedContinuation<Void, Never>])>((false, []))
}

final class EventLog: ServerEventListener, CallStateListener, @unchecked Sendable {
    private let events = Mutex<[FfiServerEvent]>([])
    private let states = Mutex<[FfiCallState]>([])
    func onEvent(event: FfiServerEvent) { events.withLock { $0.append(event) } }
    func onState(state: FfiCallState) { states.withLock { $0.append(state) } }
    var allEvents: [FfiServerEvent] { events.withLock { $0 } }
    var lastState: FfiCallState? { states.withLock { $0.last } }
}

/// Swift → Rust → Swift against the shared test server (configured by itest.sh). Under
/// BROOK_REQUIRE_ITEST=1 a missing configuration fails instead of skipping.
final class CallRoundTripTests: XCTestCase {
    private func env() throws -> (server: String, handle: String, password: String, channel: String, insecure: Bool) {
        let e = ProcessInfo.processInfo.environment
        guard let server = e["BROOK_TEST_SERVER"], let handle = e["BROOK_TEST_HANDLE"],
              let password = e["BROOK_TEST_PASSWORD"], let channel = e["BROOK_TEST_CHANNEL"]
        else {
            let why = "BROOK_TEST_SERVER / _HANDLE / _PASSWORD / _CHANNEL not set"
            if e["BROOK_REQUIRE_ITEST"] == "1" { XCTFail(why); throw XCTSkip("failed above") }
            throw XCTSkip(why)
        }
        return (server, handle, password, channel, e["BROOK_TEST_ALLOW_INSECURE_HTTP"] == "1")
    }

    private func until(_ what: String, timeout: TimeInterval = 15, _ ok: () -> Bool) {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if ok() { return }
            Thread.sleep(forTimeInterval: 0.05)
        }
        XCTFail("timed out waiting for \(what)")
    }

    /// Sign in, subscribe to events BEFORE starting realtime, and wait for `Ready`.
    private func connected() async throws -> (FfiBrookClient, EventLog, Subscription, String) {
        let cfg = try env()
        let client = try FfiBrookClient(baseUrl: cfg.server, allowInsecureHttp: cfg.insecure)
        _ = try await client.login(handle: cfg.handle, password: cfg.password)
        let log = EventLog()
        let events = client.subscribeEvents(listener: log)
        try await client.startRealtime()
        until("Ready") { log.allEvents.contains(.ready) }
        return (client, log, events, cfg.channel)
    }

    /// Listen-only: Swift joins through Rust, sees Connected, leaves; core closes the engine once.
    func testListenOnlyJoinAndLeave() async throws {
        let (client, log, events, channel) = try await connected()
        defer { events.cancel() }
        let engine = RecordingEngine(offer: "unused")
        let call = try await client.joinCall(channelId: channel, engine: engine, publish: false)
        let state = call.subscribeState(listener: log)
        defer { state.cancel() }
        until("Connected") { log.lastState?.status == .connected }
        try await call.leave()
        until("Ended(Left)") { log.lastState?.status == .ended(reason: .left) }
        until("engine closed") { engine.log.contains("close") }
        XCTAssertEqual(engine.log.filter { $0 == "close" }.count, 1)
        XCTAssertFalse(engine.log.contains("createPublishOffer"), "listen-only must not publish")
    }

    /// A well-formed audio-only WebRTC offer (what libwebrtc emits, minus candidates: ICE
    /// trickles). The media server only answers an offer it can parse, so an answer arriving
    /// back in Swift proves this exact SDP crossed Swift → Rust → server.
    static let audioOffer = [
        "v=0", "o=- 4611731400430051336 2 IN IP4 127.0.0.1", "s=-", "t=0 0",
        "a=group:BUNDLE 0", "a=extmap-allow-mixed", "a=msid-semantic: WMS brook",
        "m=audio 9 UDP/TLS/RTP/SAVPF 111", "c=IN IP4 0.0.0.0", "a=rtcp:9 IN IP4 0.0.0.0",
        "a=ice-ufrag:bRk1", "a=ice-pwd:brookroundtriptestpassword0", "a=ice-options:trickle",
        "a=fingerprint:sha-256 "
            + "6B:8B:F0:65:5F:78:E2:51:3B:AC:6F:F3:3F:46:1B:35:DC:B8:5F:64:1A:24:C2:43:F0:A1:58:D0:A1:2C:19:08",
        "a=setup:actpass", "a=mid:0", "a=sendonly", "a=msid:brook audio0", "a=rtcp-mux",
        "a=rtpmap:111 opus/48000/2", "a=fmtp:111 minptime=10;useinbandfec=1",
        "a=ssrc:1001 cname:brookroundtrip", "a=ssrc:1001 msid:brook audio0",
    ].joined(separator: "\r\n") + "\r\n"

    /// Publish: core awaits the Swift engine's async offer, sends it, and the server's answer
    /// to it comes back into Swift. Leave then closes the engine exactly once.
    func testPublishOfferRoundTripsToTheServer() async throws {
        let (client, log, events, channel) = try await connected()
        defer { events.cancel() }
        let engine = RecordingEngine(offer: Self.audioOffer)
        let call = try await client.joinCall(channelId: channel, engine: engine, publish: true)
        let state = call.subscribeState(listener: log)
        defer { state.cancel() }
        until("offer requested from Swift") { engine.log.contains("createPublishOffer") }
        until("server's answer to our offer applied in Swift") {
            engine.log.contains("applyPublishAnswer")
        }
        XCTAssertEqual(log.lastState?.status, .connected, "\(String(describing: log.lastState))")
        try await call.leave()
        until("engine closed") { engine.log.contains("close") }
        XCTAssertEqual(engine.log.filter { $0 == "close" }.count, 1)
    }
}
