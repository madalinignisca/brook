import Darwin
import XCTest

@testable import Brook

/// The worker's reply frame, read over a real pipe (previews spec §1): only an exact frame
/// followed by EOF is accepted, and a silent worker is a timeout.
final class FrameTests: XCTestCase {
    /// Write `bytes` into a pipe (then close it unless `keepOpen`), and read a frame back.
    private func read(_ bytes: Data, keepOpen: Bool = false, within: TimeInterval = 2)
        -> Result<Frame, Frame.ReadError> {
        var fds: [Int32] = [0, 0]
        XCTAssertEqual(pipe(&fds), 0)
        bytes.withUnsafeBytes { _ = write(fds[1], $0.baseAddress, $0.count) }
        if !keepOpen { close(fds[1]) }
        defer {
            close(fds[0])
            if keepOpen { close(fds[1]) }
        }
        return Frame.read(fd: fds[0], deadline: .now() + within)
    }

    private func header(_ code: UInt32, _ w: UInt32, _ h: UInt32, _ length: UInt64) -> Data {
        Frame(code: code, width: w, height: h, body: Data()).encoded().prefix(12)
            + withUnsafeBytes(of: length.littleEndian) { Data($0) }
    }

    func testAnExactFrameIsRead() {
        let frame = Frame(code: 0, width: 2, height: 3, body: Data(repeating: 7, count: 24))
        XCTAssertEqual(try read(frame.encoded()).get(), frame)
    }

    func testAMalformedFrameIsRefused() {
        let body = Data(repeating: 1, count: 16)
        let cases: [(String, Data)] = [
            ("a short header", header(0, 2, 2, 16).prefix(10)),
            ("a length over the cap", header(0, 720, 720, 720 * 720 * 4 + 4)),
            ("a length that isn't w×h×4", header(0, 2, 2, 12) + Data(count: 12)),
            ("a side over 720", header(0, 721, 1, 721 * 4) + Data(count: 721 * 4)),
            ("a zero side", header(0, 0, 4, 0)),
            ("fewer bytes than announced", header(0, 2, 2, 16) + body.prefix(15)),
            ("bytes after the body", header(0, 2, 2, 16) + body + Data([0])),
            ("a failure with sides", header(5, 2, 2, 0)),
        ]
        for (what, bytes) in cases {
            XCTAssertEqual(read(bytes), .failure(.malformed), what)
        }
    }

    func testASilentWorkerIsATimeout() {
        let started = Date()
        XCTAssertEqual(read(Data(), keepOpen: true, within: 0.3), .failure(.timeout))
        XCTAssertLessThan(Date().timeIntervalSince(started), 1)
        XCTAssertEqual(read(header(0, 2, 2, 16), keepOpen: true, within: 0.3), .failure(.timeout),
                       "a header, then silence")
    }
}
