import BrookCore
import XCTest
@testable import BrookMedia

final class OfferProbeTests: XCTestCase {
    /// Janus matches H.264 by exact profile-level-id: the offer must carry 42e01f with
    /// packetization-mode=1 (the interop point with the GStreamer client).
    func testOfferCarriesConstrainedBaselineH264() async throws {
        let e = WebRTCEngine(options: MediaOptions(
            audio: true, video: SyntheticVideoCapture(), audioDevice: SyntheticAudioDevice(toneHz: nil)))
        let offer = try await e.createPublishOffer()
        let fmtp = offer.split(whereSeparator: \.isNewline).filter { $0.contains("profile-level-id") }
        print("PROBE", fmtp.joined(separator: " | "))
        XCTAssertTrue(
            fmtp.contains { $0.contains("profile-level-id=42e01f") && $0.contains("packetization-mode=1") },
            "no 42e01f pmode=1 in \(fmtp)")
        XCTAssertTrue(offer.contains("VP8/90000"), "no VP8 fallback")
        await e.close()
    }
}
