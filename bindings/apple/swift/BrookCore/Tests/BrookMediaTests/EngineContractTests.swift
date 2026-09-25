@preconcurrency import WebRTC
import BrookCore
import Foundation
import Synchronization
import XCTest

@testable import BrookMedia

/// A capture stand-in whose start can be held, released late, or never finish.
final class ScriptedCapture: VideoCapture, @unchecked Sendable {
    enum Mode { case immediate, held, never }
    private let state: Locked<State>
    private struct State {
        var mode: Mode
        var running = false
        var starts = 0
        var stops = 0
        var held: CheckedContinuation<Void, Never>?
        var holdStop = false
        var heldStop: CheckedContinuation<Void, Never>?
    }
    /// Ordered record of "started" / "stopped" (tests append their own marks).
    let events = Locked([String]())
    private let ended = Locked<(@Sendable () -> Void)?>(nil)
    func setEndedHandler(_ handler: @escaping @Sendable () -> Void) { ended.withLock { $0 = handler } }
    /// What the system does when it ends a capture on its own (e.g. the user stops sharing
    /// from the menu bar).
    func endBySystem() {
        state.withLock { $0.running = false }
        ended.withLock { $0 }?()
    }

    init(_ mode: Mode) { state = Locked(State(mode: mode)) }

    var isCapturing: Bool { state.withLock { $0.running } }
    var starts: Int { state.withLock { $0.starts } }
    var stops: Int { state.withLock { $0.stops } }

    func start(into source: RTCVideoSource) async throws {
        let mode = state.withLock { s -> Mode in s.starts += 1; return s.mode }
        switch mode {
        case .immediate: break
        case .held: await withCheckedContinuation { c in state.withLock { $0.held = c } }
        case .never: await withCheckedContinuation { (_: CheckedContinuation<Void, Never>) in }
        }
        state.withLock { $0.running = true }
        events.withLock { $0.append("started") }
    }

    /// Let a held start finish now.
    func release() {
        let c = state.withLock { s -> CheckedContinuation<Void, Never>? in defer { s.held = nil }; return s.held }
        c?.resume()
    }

    var isHeld: Bool { state.withLock { $0.held != nil } }

    /// Make the next stop wait for `releaseStop()`.
    func holdNextStop() { state.withLock { $0.holdStop = true } }
    var isStopHeld: Bool { state.withLock { $0.heldStop != nil } }
    func releaseStop() {
        let c = state.withLock { s -> CheckedContinuation<Void, Never>? in defer { s.heldStop = nil }; return s.heldStop }
        c?.resume()
    }

    func stop() async {
        if state.withLock({ s -> Bool in defer { s.holdStop = false }; return s.holdStop }) {
            await withCheckedContinuation { c in state.withLock { $0.heldStop = c } }
        }
        state.withLock { $0.running = false; $0.stops += 1 }
        events.withLock { $0.append("stopped") }
    }
}

final class RecordingEvents: EngineEvents, @unchecked Sendable {
    let log = Locked([String]())
    func localCandidate(pc: FfiPcKind, candidate: FfiIceCandidate?) {
        log.withLock { $0.append(candidate == nil ? "\(pc):end" : "\(pc):\(candidate!.candidate)") }
    }
    func engineFailed(message: String) { log.withLock { $0.append("failed:\(message)") } }
    var entries: [String] { log.withLock { $0 } }
}

func engine(capture: VideoCapture?, audio: Bool = true, timeout: Duration = .seconds(5)) -> WebRTCEngine {
    WebRTCEngine(options: MediaOptions(
        audio: audio, video: capture, audioDevice: SyntheticAudioDevice(toneHz: nil),
        captureTimeout: timeout))
}

final class EngineContractTests: XCTestCase {
    // MARK: fence

    /// close() while the offer waits on capture: the offer fails, the late capture start is
    /// stopped at once, and `closed` completes.
    func testCloseDuringCaptureStartFencesTheOffer() async throws {
        let capture = ScriptedCapture(.held)
        let e = engine(capture: capture)
        let offer = Task { try await e.createPublishOffer() }
        await eventually("capture start reached") { capture.isHeld }
        let closing = Task { await e.close() }
        try? await Task.sleep(for: .milliseconds(100))
        capture.release()  // the start completes after the fence
        await closing.value
        await e.closed()
        do {
            _ = try await offer.value
            XCTFail("offer succeeded after close")
        } catch {}
        await eventually("late capture stopped") { !capture.isCapturing }
    }

    func testOperationsAfterCloseFail() async throws {
        let e = engine(capture: ScriptedCapture(.immediate))
        await e.close()
        await e.closed()
        await XCTAssertThrowsAsync { _ = try await e.createPublishOffer() }
        await XCTAssertThrowsAsync { try await e.applyPublishAnswer(sdp: "v=0") }
        await XCTAssertThrowsAsync { _ = try await e.applySubscribeOffer(sdp: "v=0", streams: []) }
    }

    func testCloseStopsARunningCaptureAndIsIdempotent() async throws {
        let capture = ScriptedCapture(.immediate)
        let e = engine(capture: capture)
        _ = try await e.createPublishOffer()
        XCTAssertTrue(capture.isCapturing)
        async let first: Void = e.close()
        async let second: Void = e.close()
        _ = await (first, second)
        XCTAssertFalse(capture.isCapturing)
        XCTAssertEqual(capture.stops, 1, "capture stopped more than once")
    }

    /// A capture start in flight when close() begins: `closed` completes only after that
    /// start finished and was stopped, never before (capture must not run after `closed`).
    func testClosedWaitsForAnInFlightCaptureStart() async throws {
        let capture = ScriptedCapture(.held)
        let e = engine(capture: capture)
        let offer = Task { try await e.createPublishOffer() }
        await eventually("capture start reached") { capture.isHeld }
        let closing = Task {
            await e.close()
            capture.events.withLock { $0.append("closed") }
        }
        try? await Task.sleep(for: .milliseconds(100))
        capture.release()
        await closing.value
        _ = try? await offer.value
        XCTAssertEqual(capture.events.withLock { $0 }, ["started", "stopped", "closed"])
    }

    /// Nothing is delivered once close has begun: not a buffered backlog flushed by a late
    /// attach while close awaits the capture stop.
    func testNoEventsDeliveredWhileClosing() async throws {
        let capture = ScriptedCapture(.immediate)
        let e = engine(capture: capture)
        _ = try await e.createPublishOffer()
        try? await Task.sleep(for: .milliseconds(300))  // candidates buffered, unattached
        capture.holdNextStop()
        let closing = Task { await e.close() }
        await eventually("close is stopping capture") { capture.isStopHeld }
        let events = RecordingEvents()
        e.attach(events)
        try? await Task.sleep(for: .milliseconds(200))
        capture.releaseStop()
        await closing.value
        XCTAssertEqual(events.entries, [], "delivered while closing")
    }

    // MARK: screen share lifecycle

    private func sharingEngine() async throws -> WebRTCEngine {
        let e = engine(capture: nil)
        _ = try await e.createPublishOffer()  // a publish connection to share on
        return e
    }

    /// Stop while the share's capture is still starting: once the start completes it must not
    /// be left running (no owner could stop it afterwards).
    func testStopDuringAStartingShareLeavesNothingRunning() async throws {
        let e = try await sharingEngine()
        let screen = ScriptedCapture(.held)
        let start = Task { try await e.startScreenShare(screen) }
        await eventually("share start reached") { screen.isHeld }
        let stop = Task { await e.stopScreenShare() }
        try? await Task.sleep(for: .milliseconds(100))
        screen.release()
        _ = try? await start.value
        await stop.value
        try? await Task.sleep(for: .milliseconds(100))
        XCTAssertFalse(screen.isCapturing, "screen capture left running")
        let sharing = await e.isSharingScreen()
        XCTAssertFalse(sharing)
        await e.close()
    }

    /// close() during a starting share: `closed` completes only once that capture stopped.
    func testClosedWaitsForAStartingShare() async throws {
        let e = try await sharingEngine()
        let screen = ScriptedCapture(.held)
        let start = Task { try await e.startScreenShare(screen) }
        await eventually("share start reached") { screen.isHeld }
        let closing = Task {
            await e.close()
            screen.events.withLock { $0.append("closed") }
        }
        try? await Task.sleep(for: .milliseconds(100))
        screen.release()
        await closing.value
        _ = try? await start.value
        XCTAssertEqual(screen.events.withLock { $0 }, ["started", "stopped", "closed"])
    }

    /// The system ends the capture while it is still starting: the end is not lost.
    func testShareEndedWhileStartingIsNotLost() async throws {
        let e = try await sharingEngine()
        let screen = ScriptedCapture(.held)
        let told = Locked(0)
        e.onScreenShareEnded { told.withLock { $0 += 1 } }
        let start = Task { try await e.startScreenShare(screen) }
        await eventually("share start reached") { screen.isHeld }
        screen.release()
        screen.endBySystem()  // before the engine resumed from the start
        _ = try? await start.value
        await eventually("end reported") { told.withLock { $0 } == 1 }
        let sharing = await e.isSharingScreen()
        XCTAssertFalse(sharing, "an ended capture recorded as sharing")
        await e.close()
    }

    /// The engine's ended handler does not keep a stopped capture alive.
    func testStoppedShareCaptureIsReleased() async throws {
        let e = try await sharingEngine()
        var screen: ScriptedCapture? = ScriptedCapture(.immediate)
        weak let watched = screen
        try await e.startScreenShare(screen!)
        await e.stopScreenShare()
        screen = nil
        XCTAssertNil(watched, "capture retained after stop")
        await e.close()
    }

    /// A share whose capture never starts fails within the bound, and its capture is told to
    /// stop (a stream set up before the hang must not stay outstanding).
    func testTimedOutShareIsStopped() async throws {
        let e = WebRTCEngine(options: MediaOptions(
            audio: true, video: nil, audioDevice: SyntheticAudioDevice(toneHz: nil),
            captureTimeout: .milliseconds(200)))
        _ = try await e.createPublishOffer()
        let screen = ScriptedCapture(.never)
        await XCTAssertThrowsAsync { try await e.startScreenShare(screen) }
        XCTAssertGreaterThanOrEqual(screen.stops, 1, "timed-out share never stopped")
        let sharing = await e.isSharingScreen()
        XCTAssertFalse(sharing)
        await e.close()
    }

    /// The system ends the share (menu bar "Stop Sharing", display gone): the engine drops it,
    /// the m-line goes inactive, and the UI is told so it renegotiates.
    func testShareEndedBySystemIsReported() async throws {
        let e = try await sharingEngine()
        let screen = ScriptedCapture(.immediate)
        let told = Locked(0)
        e.onScreenShareEnded { told.withLock { $0 += 1 } }
        try await e.startScreenShare(screen)
        screen.endBySystem()
        await eventually("UI told the share ended") { told.withLock { $0 } == 1 }
        let sharing = await e.isSharingScreen()
        XCTAssertFalse(sharing)
        let offer = try await e.createLabelledOffer()
        let mid = try XCTUnwrap(offer.tracks.first { $0.source == .screen }?.mid)
        XCTAssertTrue(section(offer.sdp, mid: mid).contains("a=inactive"), "ended share still sending")
        await e.close()
    }

    // MARK: capture bound

    /// After a start timed out, the capture object is not trusted again: a later toggle must
    /// not start it a second time (the first start may still complete and stop the second).
    func testNoSecondStartAfterATimedOutStart() async throws {
        let capture = ScriptedCapture(.never)
        let e = engine(capture: capture, timeout: .milliseconds(200))
        await XCTAssertThrowsAsync { _ = try await e.createPublishOffer() }
        try e.setLocalMedia(audio: true, video: false)
        try e.setLocalMedia(audio: true, video: true)
        try? await Task.sleep(for: .milliseconds(500))
        XCTAssertEqual(capture.starts, 1, "started again after a timed-out start")
        await e.close()
    }

    func testCaptureThatNeverStartsFailsWithinTheBound() async throws {
        let capture = ScriptedCapture(.never)
        let e = engine(capture: capture, timeout: .milliseconds(300))
        let started = Date()
        await XCTAssertThrowsAsync { _ = try await e.createPublishOffer() }
        XCTAssertLessThan(Date().timeIntervalSince(started), 2, "not bounded")
        await e.close()
    }

    // MARK: sync methods never block

    /// Core calls these inline in its call task: with the engine queue stalled they must still
    /// return at once (they only enqueue).
    func testSyncMethodsReturnWhileTheQueueIsStalled() throws {
        let e = engine(capture: ScriptedCapture(.immediate))
        let stall = DispatchSemaphore(value: 0)
        e.onEngineQueue { stall.wait() }
        // Called from another thread with a deadline, so a regression fails instead of hanging:
        // the stall is released either way.
        let returned = DispatchSemaphore(value: 0)
        Thread {
            try? e.addRemoteCandidate(
                pc: .publish,
                candidate: FfiIceCandidate(
                    candidate: "candidate:1 1 udp 1 127.0.0.1 9 typ host", sdpMid: "0",
                    sdpMlineIndex: 0))
            try? e.setLocalMedia(audio: false, video: true)
            e.setIceServers(servers: [
                FfiIceServer(urls: ["stun:127.0.0.1:3478"], username: nil, credential: nil)
            ])
            returned.signal()
        }.start()
        let result = returned.wait(timeout: .now() + 1)
        stall.signal()
        XCTAssertEqual(result, .success, "a sync method waited on the engine queue")
        let closed = expectation(description: "closed")
        Task {
            await e.close()
            closed.fulfill()
        }
        wait(for: [closed], timeout: 5)
    }

    /// Enabling an absent track fails immediately; disabling it is fine (a camera-denied user
    /// can still mute the microphone).
    func testOnlyEnablingAnAbsentTrackFails() throws {
        let noCamera = engine(capture: nil)
        XCTAssertThrowsError(try noCamera.setLocalMedia(audio: true, video: true))
        XCTAssertNoThrow(try noCamera.setLocalMedia(audio: false, video: false))
        let noMic = engine(capture: ScriptedCapture(.immediate), audio: false)
        XCTAssertThrowsError(try noMic.setLocalMedia(audio: true, video: false))
        XCTAssertNoThrow(try noMic.setLocalMedia(audio: false, video: true))
    }

    // MARK: attachment

    /// Candidates gathered before the handle is attached arrive first, in order, ending with
    /// end-of-candidates, and nothing is delivered twice.
    func testCandidatesBeforeAttachAreDeliveredInOrderOnce() async throws {
        let e = engine(capture: nil)
        _ = try await e.createPublishOffer()
        try? await Task.sleep(for: .milliseconds(500))  // gathering completes unattached
        let events = RecordingEvents()
        e.attach(events)
        await eventually("end-of-candidates delivered") { events.entries.contains("publish:end") }
        let log = events.entries
        XCTAssertEqual(log.last, "publish:end", "\(log)")
        XCTAssertEqual(log.filter { $0 == "publish:end" }.count, 1)
        XCTAssertEqual(Set(log).count, log.count, "a candidate was delivered twice: \(log)")
        XCTAssertGreaterThan(log.count, 1, "no candidates before end: \(log)")
        await e.close()
    }

    /// The engine holds the handle weakly: dropping it lets it go (core's drop-to-leave).
    func testAttachedHandleIsHeldWeakly() async throws {
        let e = engine(capture: nil)
        var events: RecordingEvents? = RecordingEvents()
        weak let watched = events
        e.attach(events!)
        await withCheckedContinuation { c in e.onEngineQueue { c.resume() } }  // attach ran
        events = nil
        XCTAssertNil(watched, "the engine kept the handle alive")
        _ = try await e.createPublishOffer()  // events after the drop go nowhere, no crash
        await e.close()
    }
}

func XCTAssertThrowsAsync(
    file: StaticString = #filePath, line: UInt = #line, _ body: () async throws -> Void
) async {
    do {
        try await body()
        XCTFail("did not throw", file: file, line: line)
    } catch {}
}
