import CoreGraphics
import ImageIO
import UniformTypeIdentifiers
import XCTest

@testable import Brook

/// Through the real broker and its workers (previews spec §5), from inside the sandboxed app.
/// The worker is signed with exactly its own entitlements even in test builds, so this is the
/// shipped sandbox. Called on the raw proxy: a probe's report isn't an image.
final class ImageDecoderServiceTests: XCTestCase {
    private func decode(_ bytes: Data, kind: ImageKindCode) async -> (code: Int, w: Int, h: Int, body: Data) {
        let r = await ImageDecoder.request(bytes: bytes, kind: kind)
        guard let r else {
            XCTFail("the broker couldn't be reached")
            return (-1, 0, 0, Data())
        }
        return (r.0, r.1, r.2, r.3)
    }

    private func report(_ body: Data) -> [String: String] {
        var out: [String: String] = [:]
        for line in String(decoding: body, as: UTF8.self).split(separator: "\n") {
            let kv = line.split(separator: "=", maxSplits: 1)
            if kv.count == 2 { out[String(kv[0])] = String(kv[1]) }
        }
        return out
    }

    // ---- The sandbox ----

    func testTheWorkerHasNoNetworkNoUserDataNoChildrenAndOnlyItsPipes() async {
        let r = await decode(Data(), kind: .probe)
        XCTAssertEqual(r.code, ReplyCode.report.rawValue)
        let p = report(r.body)
        XCTAssertNotEqual(p["connect"], "0", "the worker reached the network")
        XCTAssertNotEqual(p["writeHome"], "0", "the worker wrote under the home folder")
        XCTAssertNotEqual(p["fork"], "0", "the worker forked")
        XCTAssertNotEqual(p["spawn"], "0", "the worker spawned a process")
        XCTAssertEqual(p["fds"], "0,1,2", "the worker holds descriptors beyond its own pipes")
        XCTAssertEqual(p["cpu"], "10/10")
        XCTAssertEqual(p["nproc"], "0/0")
        XCTAssertEqual(p["brokerClient"], "no", "the worker used the broker as a client")
        XCTAssertEqual(p["ppid"], p["brokerPid"], "the worker isn't the broker's child")
    }

    func testEachImageGetsItsOwnWorkerUnderOneBroker() async {
        let a = report(await decode(Data(), kind: .probe).body)
        let b = report(await decode(Data(), kind: .probe).body)
        XCTAssertNotNil(a["pid"])
        XCTAssertNotEqual(a["pid"], b["pid"], "two images shared a worker")
        XCTAssertEqual(a["ppid"], b["ppid"], "the broker was relaunched between images")
        XCTAssertEqual(a["ppid"], a["brokerPid"])
    }

    // ---- Timeouts ----

    func testAHungWorkerIsKilledAtTheDeadlineAndNothingOfItSurvives() async {
        let started = Date()
        let r = await decode(Data(), kind: .hang)
        let took = Date().timeIntervalSince(started)
        XCTAssertEqual(r.code, ReplyCode.timeout.rawValue)
        XCTAssert((7 ... 9.5).contains(took), "took \(took) s")
        let p = report(r.body)
        XCTAssertNotNil(p["reaped"])
        XCTAssertEqual(p["group"], "empty")
    }

    func testTwoHungWorkersRunTogether() async {
        let started = Date()
        async let a = ImageDecoder.request(bytes: Data(), kind: .hang)
        async let b = ImageDecoder.request(bytes: Data(), kind: .hang)
        let (ra, rb) = await (a, b)
        XCTAssertEqual(ra?.0, ReplyCode.timeout.rawValue)
        XCTAssertEqual(rb?.0, ReplyCode.timeout.rawValue)
        XCTAssertLessThan(Date().timeIntervalSince(started), 9.5, "the second waited for the first")
    }

    func testTwoHungWorkersOnOneConnectionRunTogether() async {
        // The app uses a connection per request, but the broker mustn't serialize one
        // connection's requests either (NSXPC delivers them on one serial queue).
        let connection = NSXPCConnection(serviceName: imageDecoderServiceName)
        connection.remoteObjectInterface = ImageDecoder.interface()
        connection.resume()
        defer { connection.invalidate() }
        let started = Date()
        let codes: [Int] = await withCheckedContinuation { done in
            let replies = Replies(expected: 2, done)
            let proxy = connection.remoteObjectProxyWithErrorHandler { _ in replies.add(-1) } as? ImageDecoding
            for _ in 0 ..< 2 {
                proxy?.decode(Data(), kind: Int(ImageKindCode.hang.rawValue)) { code, _, _, _ in replies.add(code) }
            }
        }
        XCTAssertEqual(codes, [ReplyCode.timeout.rawValue, ReplyCode.timeout.rawValue])
        XCTAssertLessThan(Date().timeIntervalSince(started), 9.5, "one connection's requests ran in turn")
    }

    // ---- What decodes, and what doesn't ----

    private func encode(_ type: UTType, width: Int = 64, height: Int = 48, frames: Int = 1,
                        properties: [CFString: Any] = [:], bits: Int = 8,
                        space: CGColorSpace = CGColorSpace(name: CGColorSpace.sRGB)!) -> Data {
        let out = NSMutableData()
        let dest = CGImageDestinationCreateWithData(out, type.identifier as CFString, frames, nil)!
        let components = space.numberOfComponents + (space.model == .cmyk ? 0 : 1)
        let info: UInt32 = space.model == .cmyk ? CGImageAlphaInfo.none.rawValue
            : CGImageAlphaInfo.premultipliedLast.rawValue | (bits == 16 ? CGBitmapInfo.byteOrder16Big.rawValue : 0)
        let ctx = CGContext(data: nil, width: width, height: height, bitsPerComponent: bits,
                            bytesPerRow: 0, space: space, bitmapInfo: info)!
        _ = components
        ctx.setFillColor(CGColor(srgbRed: 0.2, green: 0.5, blue: 0.9, alpha: 1))
        ctx.fill(CGRect(x: 0, y: 0, width: width, height: height))
        let image = ctx.makeImage()!
        for _ in 0 ..< frames { CGImageDestinationAddImage(dest, image, properties as CFDictionary) }
        XCTAssertTrue(CGImageDestinationFinalize(dest))
        return out as Data
    }

    /// A 1×1 lossless WebP (ImageIO can't write WebP).
    private let webp = Data(base64Encoded: "UklGRhoAAABXRUJQVlA4TA0AAAAvAAAAEAcQERGIiP4HAA==")!

    func testTheFourKindsDecodeWithinTheCaps() async {
        let cases: [(String, Data, ImageKindCode)] = [
            ("png", encode(.png), .png),
            ("png 16-bit", encode(.png, bits: 16), .png),
            ("jpeg", encode(.jpeg), .jpeg),
            ("jpeg cmyk", encode(.jpeg, space: CGColorSpace(name: CGColorSpace.genericCMYK)!), .jpeg),
            ("gif, two frames", encode(.gif, frames: 2), .gif),
            ("webp", webp, .webp),
        ]
        for (what, bytes, kind) in cases {
            let r = await decode(bytes, kind: kind)
            XCTAssertEqual(r.code, 0, what)
            XCTAssert(r.w > 0 && r.w <= 720 && r.h > 0 && r.h <= 720, what)
            XCTAssertEqual(r.body.count, r.w * r.h * 4, what)
        }
    }

    func testARotatedJpegComesBackTurned() async {
        let rotated = encode(.jpeg, width: 64, height: 48,
                             properties: [kCGImagePropertyOrientation: 6]) // 90° clockwise
        let r = await decode(rotated, kind: .jpeg)
        XCTAssertEqual(r.code, 0)
        XCTAssertEqual([r.w, r.h], [48, 64])
    }

    func testALargeImageComesBackAsAThumbnail() async {
        let r = await decode(encode(.png, width: 4000, height: 3000), kind: .png)
        XCTAssertEqual(r.code, 0)
        XCTAssertEqual(max(r.w, r.h), 720)
    }

    func testAnythingElseIsRefused() async {
        let pdf: Data = {
            let out = NSMutableData()
            var box = CGRect(x: 0, y: 0, width: 10, height: 10)
            let ctx = CGContext(consumer: CGDataConsumer(data: out)!, mediaBox: &box, nil)!
            ctx.beginPDFPage(nil); ctx.endPDFPage(); ctx.closePDF()
            return out as Data
        }()
        let svg = Data("<svg xmlns='http://www.w3.org/2000/svg' width='10' height='10'/>".utf8)
        let cases: [(String, Data, ImageKindCode)] = [
            ("a png labelled as a jpeg", encode(.png), .jpeg),
            ("tiff", encode(.tiff), .png),
            ("heic", encode(.heic), .jpeg),
            ("pdf", pdf, .png),
            ("svg", svg, .png),
            ("nothing", Data(), .png),
        ]
        for (what, bytes, kind) in cases {
            let r = await decode(bytes, kind: kind)
            XCTAssertNotEqual(r.code, 0, what)
            XCTAssertTrue(r.body.isEmpty, what)
        }
    }

    func testAnImageOverTheCapsIsRefusedAsImageIOReadsIt() async {
        let r = await decode(encode(.png, width: 8193, height: 2), kind: .png)
        XCTAssertEqual(r.code, ReplyCode.overCaps.rawValue)
    }

    func testFortyMegapixelsDecodesAndItsPeakMemoryIsRecorded() async throws {
        let big = encode(.png, width: 8000, height: 5000) // flat colour: compresses small
        XCTAssertLessThan(big.count, 16 * 1024 * 1024)
        let r = await decode(big, kind: .png)
        XCTAssertEqual(r.code, 0)
        // The peak itself is written down from the probe run (see the PR); here, it decodes.
    }

    func testAnUnknownKindNeverReachesAWorker() async {
        let r = await ImageDecoder.request(bytes: Data(), kind: .png).map { $0 } // sanity: reachable
        XCTAssertNotNil(r)
        let connection = NSXPCConnection(serviceName: imageDecoderServiceName)
        connection.remoteObjectInterface = ImageDecoder.interface()
        connection.resume()
        defer { connection.invalidate() }
        let code: Int = await withCheckedContinuation { done in
            let proxy = connection.remoteObjectProxyWithErrorHandler { _ in done.resume(returning: -1) }
            (proxy as? ImageDecoding)?.decode(Data(), kind: 256 + 1) { code, _, _, _ in done.resume(returning: code) }
        }
        XCTAssertEqual(code, ReplyCode.refused.rawValue, "a kind that wraps to png as a byte")
    }
}

/// Collects a fixed number of reply codes, then resumes once.
private final class Replies: @unchecked Sendable {
    private let lock = NSLock()
    private var codes: [Int] = []
    private let expected: Int
    private var done: CheckedContinuation<[Int], Never>?
    init(expected: Int, _ done: CheckedContinuation<[Int], Never>) {
        self.expected = expected
        self.done = done
    }
    func add(_ code: Int) {
        let finished = lock.withLock { () -> ([Int], CheckedContinuation<[Int], Never>)? in
            codes.append(code)
            guard codes.count == expected, let d = done else { return nil }
            done = nil
            return (codes, d)
        }
        if let (codes, d) = finished { d.resume(returning: codes) }
    }
}
