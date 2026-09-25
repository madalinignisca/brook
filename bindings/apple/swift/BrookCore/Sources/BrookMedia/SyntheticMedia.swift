@preconcurrency import WebRTC
import AudioToolbox
import CoreVideo
import Foundation
import Synchronization
import WebRTCAudioDevice

/// A camera stand-in: a moving pattern at 30 fps. No device, no permission prompt.
public final class SyntheticVideoCapture: VideoCapture, @unchecked Sendable {
    private let width: Int
    private let height: Int
    private let lock = Mutex<State>(State())
    private let queue = DispatchQueue(label: "brook.media.synthetic-video")

    private struct State {
        var timer: DispatchSourceTimer?
        var frames: Int64 = 0
    }

    public init(width: Int = 640, height: Int = 360) {
        self.width = width
        self.height = height
    }

    public var isCapturing: Bool { lock.withLock { $0.timer != nil } }

    public func start(into source: RTCVideoSource) async throws {
        let capturer = RTCVideoCapturer(delegate: source)
        var pool: CVPixelBufferPool?
        let attrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            kCVPixelBufferWidthKey: width,
            kCVPixelBufferHeightKey: height,
            kCVPixelBufferIOSurfacePropertiesKey: [:] as CFDictionary,
        ]
        CVPixelBufferPoolCreate(nil, nil, attrs as CFDictionary, &pool)
        guard let pool else { throw EngineFailure("no pixel buffer pool") }
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now(), repeating: .milliseconds(33))
        timer.setEventHandler { [weak self] in
            guard let self else { return }
            let n = self.lock.withLock { s -> Int64 in s.frames += 1; return s.frames }
            var buffer: CVPixelBuffer?
            CVPixelBufferPoolCreatePixelBuffer(nil, pool, &buffer)
            guard let buffer else { return }
            Self.paint(buffer, frame: n)
            let frame = RTCVideoFrame(
                buffer: RTCCVPixelBuffer(pixelBuffer: buffer), rotation: ._0,
                timeStampNs: Int64(DispatchTime.now().uptimeNanoseconds))
            source.capturer(capturer, didCapture: frame)
        }
        lock.withLock { $0.timer = timer }
        timer.resume()
    }

    public func stop() async {
        let timer = lock.withLock { s -> DispatchSourceTimer? in defer { s.timer = nil }; return s.timer }
        timer?.cancel()
    }

    /// A bar that moves every frame, so the encoder always has something to send.
    private static func paint(_ buffer: CVPixelBuffer, frame: Int64) {
        CVPixelBufferLockBaseAddress(buffer, [])
        defer { CVPixelBufferUnlockBaseAddress(buffer, []) }
        let w = CVPixelBufferGetWidthOfPlane(buffer, 0)
        let h = CVPixelBufferGetHeightOfPlane(buffer, 0)
        let stride = CVPixelBufferGetBytesPerRowOfPlane(buffer, 0)
        if let y = CVPixelBufferGetBaseAddressOfPlane(buffer, 0)?.assumingMemoryBound(to: UInt8.self) {
            let bar = Int(frame * 8) % w
            for row in 0..<h {
                let line = y + row * stride
                for col in 0..<w { line[col] = abs(col - bar) < 24 ? 235 : 32 }
            }
        }
        if let uv = CVPixelBufferGetBaseAddressOfPlane(buffer, 1) {
            let rows = CVPixelBufferGetHeightOfPlane(buffer, 1)
            memset(uv, 128, CVPixelBufferGetBytesPerRowOfPlane(buffer, 1) * rows)
        }
    }
}

/// An audio device without hardware: records a tone (or silence) and plays out into a meter.
/// Stands in for the microphone and speakers in tests, so no TCC prompt and no sound.
public final class SyntheticAudioDevice: NSObject, RTCAudioDevice, @unchecked Sendable {
    public static let sampleRate = 48_000.0
    private static let frames: UInt32 = 480  // 10 ms

    private let toneHz: Double?
    private let queue = DispatchQueue(label: "brook.media.synthetic-audio")
    /// NSLock rather than Mutex: the state holds WebRTC's non-Sendable delegate.
    private let state = Locked(State())

    private struct State {
        var delegate: (any RTCAudioDeviceDelegate)?
        var recordTimer: DispatchSourceTimer?
        var playTimer: DispatchSourceTimer?
        var playoutInitialized = false
        var recordingInitialized = false
        var phase = 0.0
        var audibleBuffers = 0
    }

    /// `toneHz`: what the "microphone" hears; nil for silence.
    public init(toneHz: Double?) {
        self.toneHz = toneHz
    }

    /// Played-out 10 ms buffers whose peak was clearly above silence.
    public var audibleBuffers: Int { state.withLock { $0.audibleBuffers } }

    public var deviceInputSampleRate: Double { Self.sampleRate }
    public var inputIOBufferDuration: TimeInterval { 0.01 }
    public var inputNumberOfChannels: Int { 1 }
    public var inputLatency: TimeInterval { 0 }
    public var deviceOutputSampleRate: Double { Self.sampleRate }
    public var outputIOBufferDuration: TimeInterval { 0.01 }
    public var outputNumberOfChannels: Int { 1 }
    public var outputLatency: TimeInterval { 0 }
    public var isInitialized: Bool { state.withLock { $0.delegate != nil } }

    public func initialize(with delegate: any RTCAudioDeviceDelegate) -> Bool {
        state.withLock { $0.delegate = delegate }
        return true
    }

    public func terminateDevice() -> Bool {
        _ = stopPlayout()
        _ = stopRecording()
        state.withLock { $0.delegate = nil }
        return true
    }

    public var isPlayoutInitialized: Bool { state.withLock { $0.playoutInitialized } }
    public func initializePlayout() -> Bool {
        state.withLock { $0.playoutInitialized = true }
        return true
    }
    public var isPlaying: Bool { state.withLock { $0.playTimer != nil } }

    public func startPlayout() -> Bool {
        let timer = tick { [weak self] in self?.pullPlayout() }
        state.withLock { $0.playTimer = timer }
        return true
    }

    public func stopPlayout() -> Bool {
        state.withLock { s in s.playTimer?.cancel(); s.playTimer = nil }
        return true
    }

    public var isRecordingInitialized: Bool { state.withLock { $0.recordingInitialized } }
    public func initializeRecording() -> Bool {
        state.withLock { $0.recordingInitialized = true }
        return true
    }
    public var isRecording: Bool { state.withLock { $0.recordTimer != nil } }

    public func startRecording() -> Bool {
        let timer = tick { [weak self] in self?.pushRecording() }
        state.withLock { $0.recordTimer = timer }
        return true
    }

    public func stopRecording() -> Bool {
        state.withLock { s in s.recordTimer?.cancel(); s.recordTimer = nil }
        return true
    }

    private func tick(_ body: @escaping () -> Void) -> DispatchSourceTimer {
        let timer = DispatchSource.makeTimerSource(queue: queue)
        timer.schedule(deadline: .now(), repeating: .milliseconds(10))
        timer.setEventHandler(handler: body)
        timer.resume()
        return timer
    }

    private func pushRecording() {
        let (delegate, phase) = state.withLock { ($0.delegate, $0.phase) }
        guard let delegate else { return }
        var samples = [Int16](repeating: 0, count: Int(Self.frames))
        var next = phase
        if let toneHz {
            let step = 2 * Double.pi * toneHz / Self.sampleRate
            for i in samples.indices {
                samples[i] = Int16(8_000 * sin(next))
                next += step
            }
            state.withLock { $0.phase = next.truncatingRemainder(dividingBy: 2 * .pi) }
        }
        samples.withUnsafeMutableBytes { raw in
            var list = AudioBufferList(
                mNumberBuffers: 1,
                mBuffers: AudioBuffer(
                    mNumberChannels: 1, mDataByteSize: UInt32(raw.count), mData: raw.baseAddress))
            var flags = AudioUnitRenderActionFlags()
            var time = AudioTimeStamp()
            _ = delegate.deliverRecordedData(&flags, &time, 1, Self.frames, &list, nil, nil)
        }
    }

    private func pullPlayout() {
        guard let delegate = state.withLock({ $0.delegate }) else { return }
        var samples = [Int16](repeating: 0, count: Int(Self.frames))
        let peak = samples.withUnsafeMutableBytes { raw -> Int16 in
            var list = AudioBufferList(
                mNumberBuffers: 1,
                mBuffers: AudioBuffer(
                    mNumberChannels: 1, mDataByteSize: UInt32(raw.count), mData: raw.baseAddress))
            var flags = AudioUnitRenderActionFlags()
            var time = AudioTimeStamp()
            _ = delegate.getPlayoutData(&flags, &time, 0, Self.frames, &list)
            return raw.bindMemory(to: Int16.self).map { $0 == .min ? .max : abs($0) }.max() ?? 0
        }
        if peak > 1_000 { state.withLock { $0.audibleBuffers += 1 } }
    }
}

/// A value behind an NSLock, for state that holds non-Sendable Objective-C objects.
final class Locked<Value>: @unchecked Sendable {
    private let lock = NSLock()
    private var value: Value
    init(_ value: Value) { self.value = value }
    func withLock<R>(_ body: (inout Value) -> R) -> R {
        lock.lock()
        defer { lock.unlock() }
        return body(&value)
    }
}
