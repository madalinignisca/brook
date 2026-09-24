@preconcurrency import AVFoundation
@preconcurrency import WebRTC
import Foundation

/// The system camera through WebRTC's capturer, at most 720p30 (design §3.3).
public final class CameraCapture: VideoCapture, @unchecked Sendable {
    private let capturer = Locked<RTCCameraVideoCapturer?>(nil)

    public init() {}

    public var isCapturing: Bool { capturer.withLock { $0 != nil } }

    public func start(into source: RTCVideoSource) async throws {
        guard let device = AVCaptureDevice.default(for: .video) else {
            throw EngineFailure("no camera")
        }
        guard let format = Self.format(for: device) else {
            throw EngineFailure("the camera has no usable format")
        }
        let fps = Self.frameRate(for: format)
        let capturer = RTCCameraVideoCapturer(delegate: source)
        try await withCheckedThrowingContinuation { (cont: CheckedContinuation<Void, Error>) in
            capturer.startCapture(with: device, format: format, fps: fps) { error in
                if let error { cont.resume(throwing: error) } else { cont.resume() }
            }
        }
        self.capturer.withLock { $0 = capturer }
    }

    /// Stopping ends the capture session: the camera and its privacy indicator turn off.
    public func stop() async {
        let capturer = self.capturer.withLock { c in defer { c = nil }; return c }
        guard let capturer else { return }
        await withCheckedContinuation { (cont: CheckedContinuation<Void, Never>) in
            capturer.stopCapture { cont.resume() }
        }
    }

    /// The largest format no bigger than 1280×720.
    static func format(for device: AVCaptureDevice) -> AVCaptureDevice.Format? {
        var best: (format: AVCaptureDevice.Format, pixels: Int)?
        for format in RTCCameraVideoCapturer.supportedFormats(for: device) {
            let size = CMVideoFormatDescriptionGetDimensions(format.formatDescription)
            guard size.width <= 1280, size.height <= 720 else { continue }
            let pixels = Int(size.width) * Int(size.height)
            if pixels > best?.pixels ?? 0 { best = (format, pixels) }
        }
        return best?.format
    }

    static func frameRate(for format: AVCaptureDevice.Format) -> Int {
        let best = format.videoSupportedFrameRateRanges.map(\.maxFrameRate).max() ?? 30
        return Int(min(best, 30))
    }
}

/// What the system says about one capture permission.
public enum Authorization: Sendable { case granted, denied, notDetermined }

/// The system's capture authorization, injectable so the join decision can be tested.
public protocol AuthorizationSource: Sendable {
    func status(_ media: AVMediaType) -> Authorization
    func request(_ media: AVMediaType) async -> Bool
}

public struct SystemAuthorization: AuthorizationSource {
    public init() {}

    public func status(_ media: AVMediaType) -> Authorization {
        switch AVCaptureDevice.authorizationStatus(for: media) {
        case .authorized: .granted
        case .notDetermined: .notDetermined
        // Restricted (parental controls, MDM) behaves as denied: nothing the user can grant here.
        default: .denied
        }
    }

    public func request(_ media: AVMediaType) async -> Bool {
        await AVCaptureDevice.requestAccess(for: media)
    }
}

/// How to join, decided before `join_call` so a permission prompt's human time never eats into
/// the engine's capture budget (design §3.4).
public struct JoinPlan: Equatable, Sendable {
    public let microphone: Bool
    public let camera: Bool
    /// `publish` for `join_call`: listen-only when there is no microphone.
    public var publishes: Bool { microphone }
    /// Shown to the user when the call is degraded; nil when everything was granted.
    public let explanation: String?

    public static let micDenied =
        "Brook can't use your microphone, so you joined listen-only. Allow it in System Settings › Privacy & Security › Microphone, then rejoin to speak."
    public static let cameraDenied =
        "Brook can't use your camera, so you joined with audio only. Allow it in System Settings › Privacy & Security › Camera, then rejoin to be seen."
    /// Tooltip for the mute control (mute sends silence; the microphone stays open).
    public static let muteTooltip =
        "Mute sends silence. The microphone stays open while you're in the call."

    /// Resolve permissions, microphone first. A denied (or cancelled) microphone joins
    /// listen-only and the camera is never asked; a denied camera joins audio-only.
    public static func resolve(_ auth: AuthorizationSource) async -> JoinPlan {
        guard await granted(.audio, auth) else {
            return JoinPlan(microphone: false, camera: false, explanation: micDenied)
        }
        guard await granted(.video, auth) else {
            return JoinPlan(microphone: true, camera: false, explanation: cameraDenied)
        }
        return JoinPlan(microphone: true, camera: true, explanation: nil)
    }

    private static func granted(_ media: AVMediaType, _ auth: AuthorizationSource) async -> Bool {
        switch auth.status(media) {
        case .granted: true
        case .denied: false
        case .notDetermined: await auth.request(media)
        }
    }
}
