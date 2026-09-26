import Foundation

/// The image decoder broker's one interface (spec 2026-09-26-mac-previews-spec.md §1): plain
/// values only, so nothing but bytes and numbers crosses from a service that runs decoders.
@objc protocol ImageDecoding {
    /// Decode `bytes` (core's sniffed `kind`, an `ImageKindCode`) into an RGBA thumbnail.
    /// The reply is `(code, width, height, rgba)`, `code` a `ReplyCode`; it's called once.
    func decode(_ bytes: Data, kind: Int, reply: @escaping (Int, Int, Int, Data) -> Void)
}

/// The broker's name inside Brook.app/Contents/XPCServices.
let imageDecoderServiceName = "dev.brook.BrookImageDecoder"

/// What a worker is asked to decode: core's four kinds, and in Debug builds only, test kinds
/// (compiled out of Release, so a Release broker can't be asked for them).
enum ImageKindCode: UInt8 {
    case png = 1
    case jpeg = 2
    case gif = 3
    case webp = 4
    #if DEBUG
    /// Spins forever: the broker's deadline must kill it.
    case hang = 200
    /// Reports what its sandbox allows, as a text line in the frame's bytes.
    case probe = 201
    #endif

    /// Only the kinds this build knows: anything else, and any `Int` that isn't a byte,
    /// is refused before it reaches a worker.
    init?(request: Int) {
        guard let byte = UInt8(exactly: request) else { return nil }
        self.init(rawValue: byte)
    }
}

/// How a decode ended.
enum ReplyCode: Int {
    case ok = 0
    /// Not the kind it claims (ImageIO disagrees with core's sniff), or a kind not allowed.
    case refused = 1
    /// Over the size caps, as ImageIO reads the image.
    case overCaps = 2
    case noImage = 3
    case drawFailed = 4
    /// The worker couldn't start, or exited wrongly.
    case workerFailed = 5
    /// The broker's deadline: the worker was killed.
    case timeout = 6
    /// The worker's reply wasn't a well-formed frame.
    case badFrame = 7
    #if DEBUG
    /// A test kind's report (text in the body), never an image.
    case report = 100
    #endif
}

/// Caps shared by the worker, the broker and the app.
enum PreviewCaps {
    /// The thumbnail's longest side, in pixels.
    static let maxSide = 720
    /// Core's caps on the image itself (preview.rs): a side, and the pixel count.
    static let imageMaxSide = 8192
    static let imageMaxPixels = 40_000_000
    /// Core's cap on a previewed file (`previewMaxBytes()`).
    static let maxInputBytes = 16 * 1024 * 1024
}
