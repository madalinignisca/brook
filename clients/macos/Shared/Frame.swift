import Darwin
import Foundation

/// The worker's reply on its stdout (spec §1): a 20-byte header, `code u32 | width u32 |
/// height u32 | length u64`, little-endian, then exactly `length` bytes, then EOF.
struct Frame: Equatable {
    var code: UInt32
    var width: UInt32
    var height: UInt32
    var body: Data

    static let headerSize = 20

    func encoded() -> Data {
        var out = Data(capacity: Self.headerSize + body.count)
        for v in [code, width, height] { withUnsafeBytes(of: v.littleEndian) { out.append(contentsOf: $0) } }
        withUnsafeBytes(of: UInt64(body.count).littleEndian) { out.append(contentsOf: $0) }
        out.append(body)
        return out
    }

    /// The body length a header may announce: sides over 0 and at most `maxSide`, and
    /// exactly `width × height × 4` bytes (overflow-checked). A failure carries nothing
    /// (all zeros), except that in Debug builds its body may hold a test report (text).
    static func checkedLength(code: UInt32, width: UInt32, height: UInt32, length: UInt64) -> Int? {
        let largest = UInt64(PreviewCaps.maxSide * PreviewCaps.maxSide * 4)
        guard length <= largest else { return nil }
        if code != UInt32(ReplyCode.ok.rawValue) {
            guard width == 0, height == 0 else { return nil }
            #if DEBUG
            return Int(length)
            #else
            return length == 0 ? 0 : nil
            #endif
        }
        guard width > 0, height > 0, width <= PreviewCaps.maxSide, height <= PreviewCaps.maxSide
        else { return nil }
        let (row, o1) = UInt64(width).multipliedReportingOverflow(by: 4)
        let (all, o2) = row.multipliedReportingOverflow(by: UInt64(height))
        guard !o1, !o2, all == length else { return nil }
        return Int(length)
    }

    enum ReadError: Error, Equatable {
        case timeout
        /// Short, over the cap, inconsistent, or with bytes after the body.
        case malformed
    }

    /// Read one frame from `fd` before `deadline` (a `DispatchTime`), checking the header
    /// before reading the body, and requiring EOF right after it.
    static func read(fd: Int32, deadline: DispatchTime) -> Result<Frame, ReadError> {
        func readExactly(_ n: Int) -> Result<Data, ReadError> {
            var out = Data(count: n)
            var got = 0
            while got < n {
                switch waitReadable(fd, deadline) {
                case .timeout: return .failure(.timeout)
                case .ready: break
                }
                let r = out.withUnsafeMutableBytes { Darwin.read(fd, $0.baseAddress! + got, n - got) }
                if r < 0 && errno == EINTR { continue }
                if r <= 0 { return .failure(.malformed) } // EOF (or an error) before the end
                got += r
            }
            return .success(out)
        }
        let header: Data
        switch readExactly(headerSize) {
        case .failure(let e): return .failure(e)
        case .success(let h): header = h
        }
        func u32(_ at: Int) -> UInt32 {
            header.subdata(in: at ..< at + 4).withUnsafeBytes { UInt32(littleEndian: $0.loadUnaligned(as: UInt32.self)) }
        }
        let length = header.subdata(in: 12 ..< 20).withUnsafeBytes {
            UInt64(littleEndian: $0.loadUnaligned(as: UInt64.self))
        }
        let (code, width, height) = (u32(0), u32(4), u32(8))
        guard let n = checkedLength(code: code, width: width, height: height, length: length) else {
            return .failure(.malformed)
        }
        let body: Data
        switch n == 0 ? .success(Data()) : readExactly(n) {
        case .failure(let e): return .failure(e)
        case .success(let b): body = b
        }
        // Nothing may follow the body.
        switch waitReadable(fd, deadline) {
        case .timeout: return .failure(.timeout)
        case .ready: break
        }
        var extra: UInt8 = 0
        let r = Darwin.read(fd, &extra, 1)
        guard r == 0 else { return .failure(.malformed) }
        return .success(Frame(code: code, width: width, height: height, body: body))
    }

    private enum Readiness { case ready, timeout }

    private static func waitReadable(_ fd: Int32, _ deadline: DispatchTime) -> Readiness {
        while true {
            let now = DispatchTime.now()
            if now >= deadline { return .timeout }
            let ms = Int32(clamping: (deadline.uptimeNanoseconds - now.uptimeNanoseconds) / 1_000_000 + 1)
            var p = pollfd(fd: fd, events: Int16(POLLIN), revents: 0)
            let r = poll(&p, 1, ms)
            if r < 0 && errno == EINTR { continue }
            if r == 0 { return .timeout }
            return .ready // readable, hung up or errored: the read says which
        }
    }
}
