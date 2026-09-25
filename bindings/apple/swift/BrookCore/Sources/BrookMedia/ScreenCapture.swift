@preconcurrency import ScreenCaptureKit
@preconcurrency import WebRTC
import CoreMedia
import Foundation

/// A display or window through ScreenCaptureKit, at most 1920×1080 and 10 fps: screens are
/// mostly static, and the per-publisher bitrate cap is shared with the camera. The content
/// comes from the system picker (`SCContentSharingPicker`), which needs no screen-recording
/// entitlement: the user's pick is the consent.
public final class ScreenCapture: NSObject, VideoCapture, SCStreamOutput, SCStreamDelegate,
    @unchecked Sendable
{
    private let filter: SCContentFilter
    private let queue = DispatchQueue(label: "brook.media.screen")
    private let state = Locked<(stream: SCStream?, capturer: RTCVideoCapturer?, source: RTCVideoSource?)>(
        (nil, nil, nil))

    public init(filter: SCContentFilter) {
        self.filter = filter
    }

    private let ended = Locked<(@Sendable () -> Void)?>(nil)

    public var isCapturing: Bool { state.withLock { $0.stream != nil } }

    public func setEndedHandler(_ handler: @escaping @Sendable () -> Void) {
        ended.withLock { $0 = handler }
    }

    /// The system stopped the stream (menu bar "Stop Sharing", display or window gone, error).
    public func stream(_ stream: SCStream, didStopWithError error: any Error) {
        ended.withLock { $0 }?()
    }

    public func start(into source: RTCVideoSource) async throws {
        let config = SCStreamConfiguration()
        let size = Self.size(for: filter.contentRect.size, scale: CGFloat(filter.pointPixelScale))
        config.width = size.width
        config.height = size.height
        config.minimumFrameInterval = CMTime(value: 1, timescale: 10)
        config.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
        config.showsCursor = true
        config.queueDepth = 5
        let stream = SCStream(filter: filter, configuration: config, delegate: self)
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        state.withLock { $0 = (stream, RTCVideoCapturer(delegate: source), source) }
        do {
            try await stream.startCapture()
        } catch {
            state.withLock { $0 = (nil, nil, nil) }
            throw error
        }
    }

    /// Frames stop at once (the output is detached first), then the system capture is
    /// stopped, retried once. If that still fails the stream is released anyway, which ends
    /// it; nothing is left to retry, so the state is cleared either way.
    public func stop() async {
        let stream = state.withLock { s -> SCStream? in defer { s = (nil, nil, nil) }; return s.stream }
        guard let stream else { return }
        try? stream.removeStreamOutput(self, type: .screen)
        do {
            try await stream.stopCapture()
        } catch {
            try? await stream.stopCapture()
        }
    }

    public func stream(_ stream: SCStream, didOutputSampleBuffer buffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, buffer.isValid, Self.isComplete(buffer),
              let pixels = buffer.imageBuffer
        else { return }
        let (capturer, source) = state.withLock { ($0.capturer, $0.source) }
        guard let capturer, let source else { return }
        let time = CMSampleBufferGetPresentationTimeStamp(buffer)
        let frame = RTCVideoFrame(
            buffer: RTCCVPixelBuffer(pixelBuffer: pixels), rotation: ._0,
            timeStampNs: Int64(CMTimeGetSeconds(time) * 1_000_000_000))
        source.capturer(capturer, didCapture: frame)
    }

    /// ScreenCaptureKit also delivers idle/blank status buffers; only complete frames carry pixels.
    private static func isComplete(_ buffer: CMSampleBuffer) -> Bool {
        guard let attachments = CMSampleBufferGetSampleAttachmentsArray(buffer, createIfNecessary: false)
                as? [[SCStreamFrameInfo: Any]],
              let raw = attachments.first?[.status] as? Int,
              let status = SCFrameStatus(rawValue: raw)
        else { return false }
        return status == .complete
    }

    /// Pixel size of the content, scaled down to fit 1920×1080, even dimensions.
    static func size(for points: CGSize, scale: CGFloat) -> (width: Int, height: Int) {
        let w = max(points.width * scale, 2)
        let h = max(points.height * scale, 2)
        let fit = min(1, 1920 / w, 1080 / h)
        return (Int(w * fit) & ~1, Int(h * fit) & ~1)
    }
}
