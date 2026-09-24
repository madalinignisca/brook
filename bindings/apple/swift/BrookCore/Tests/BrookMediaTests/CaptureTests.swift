@preconcurrency import AVFoundation
import BrookCore
import Foundation
import XCTest

@testable import BrookMedia

/// Scripted system answers; records which permissions were asked.
final class FakeAuthorization: AuthorizationSource, @unchecked Sendable {
    let statuses: [AVMediaType: Authorization]
    let answers: [AVMediaType: Bool]
    let asked = Locked([AVMediaType]())
    init(_ statuses: [AVMediaType: Authorization], answers: [AVMediaType: Bool] = [:]) {
        self.statuses = statuses
        self.answers = answers
    }
    func status(_ media: AVMediaType) -> Authorization { statuses[media] ?? .notDetermined }
    func request(_ media: AVMediaType) async -> Bool {
        asked.withLock { $0.append(media) }
        return answers[media] ?? false
    }
}

final class JoinPlanTests: XCTestCase {
    func testBothGrantedPublishesEverythingWithoutExplanation() async {
        let plan = await JoinPlan.resolve(FakeAuthorization([.audio: .granted, .video: .granted]))
        XCTAssertEqual(plan, JoinPlan(microphone: true, camera: true, explanation: nil))
        XCTAssertTrue(plan.publishes)
    }

    func testMicrophoneDeniedJoinsListenOnlyAndNeverAsksTheCamera() async {
        let auth = FakeAuthorization([.audio: .denied, .video: .notDetermined])
        let plan = await JoinPlan.resolve(auth)
        XCTAssertFalse(plan.publishes)
        XCTAssertFalse(plan.camera)
        XCTAssertEqual(plan.explanation, JoinPlan.micDenied)
        XCTAssertEqual(auth.asked.withLock { $0 }, [], "camera asked after the microphone was denied")
    }

    func testMicrophonePromptCancelledIsListenOnly() async {
        let auth = FakeAuthorization([.audio: .notDetermined], answers: [.audio: false])
        let plan = await JoinPlan.resolve(auth)
        XCTAssertFalse(plan.publishes)
        XCTAssertEqual(auth.asked.withLock { $0 }, [.audio])
    }

    func testCameraDeniedJoinsAudioOnly() async {
        let plan = await JoinPlan.resolve(FakeAuthorization([.audio: .granted, .video: .denied]))
        XCTAssertEqual(plan, JoinPlan(microphone: true, camera: false, explanation: JoinPlan.cameraDenied))
    }

    func testPromptsAskMicrophoneFirst() async {
        let auth = FakeAuthorization([:], answers: [.audio: true, .video: true])
        let plan = await JoinPlan.resolve(auth)
        XCTAssertEqual(auth.asked.withLock { $0 }, [.audio, .video])
        XCTAssertTrue(plan.camera)
    }
}

final class CameraToggleTests: XCTestCase {
    /// Camera off stops capture (not just disabling the track: the camera and its indicator must
    /// turn off); on restarts it. The engine never renegotiates by itself (only core's `republish`
    /// does), so the track and transceiver staying is what keeps this offer-free.
    func testCameraOffStopsCaptureAndOnRestartsItWithoutRenegotiation() async throws {
        let capture = ScriptedCapture(.immediate)
        let e = engine(capture: capture)
        _ = try await e.createPublishOffer()
        XCTAssertTrue(capture.isCapturing)
        try e.setLocalMedia(audio: true, video: false)
        await eventually("capture stopped") { !capture.isCapturing }
        try e.setLocalMedia(audio: true, video: true)
        await eventually("capture restarted") { capture.isCapturing }
        XCTAssertEqual(capture.starts, 2)
        await e.close()
    }

    /// Camera off before the first offer: the offer carries the video m-line, capture never runs.
    func testCameraOffBeforeTheOfferNeverCaptures() async throws {
        let capture = ScriptedCapture(.immediate)
        let e = engine(capture: capture)
        try e.setLocalMedia(audio: true, video: false)
        let offer = try await e.createPublishOffer()
        XCTAssertTrue(offer.contains("m=video"))
        XCTAssertEqual(capture.starts, 0)
        await e.close()
    }
}
