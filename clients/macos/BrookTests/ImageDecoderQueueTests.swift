import XCTest

@testable import Brook

/// The client's queue (previews spec §4): two in flight, the newest waiting request next, and
/// requests whose row has gone dropped before they start.
final class ImageDecoderQueueTests: XCTestCase {
    /// A fake decoder whose requests finish only when released, recording the order they start.
    final class Gate: @unchecked Sendable {
        private let lock = NSLock()
        private var started: [UInt8] = []
        private var waiters: [CheckedContinuation<Void, Never>] = []

        func request(_ bytes: Data, _ kind: ImageKindCode) async -> (Int, Int, Int, Data)? {
            await withCheckedContinuation { c in
                lock.withLock {
                    started.append(bytes.first ?? 0)
                    waiters.append(c)
                }
            }
            return (ReplyCode.refused.rawValue, 0, 0, Data())
        }

        var order: [UInt8] { lock.withLock { started } }
        func releaseOne() { lock.withLock { waiters.isEmpty ? nil : waiters.removeFirst() }?.resume() }
    }

    private func settle() async { for _ in 0 ..< 20 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) } }

    func testTwoRunTheNewestWaitingIsNextAndGoneRowsAreDropped() async {
        let gate = Gate()
        let decoder = ImageDecoder(send: { await gate.request($0, $1) })
        var tasks: [Task<Void, Never>] = []
        for id: UInt8 in 1 ... 5 {
            let alive: ImageDecoder.Alive = { id != 3 } // row 3 scrolls away
            tasks.append(Task { [decoder] in
                _ = await decoder.thumbnail(bytes: Data([id]), kind: .png, header: (1, 1), alive: alive)
            })
            await settle()
        }
        XCTAssertEqual(gate.order, [1, 2], "two in flight")
        gate.releaseOne()
        await settle()
        XCTAssertEqual(gate.order, [1, 2, 5], "the newest waiting goes next")
        gate.releaseOne()
        await settle()
        XCTAssertEqual(gate.order, [1, 2, 5, 4], "3's row is gone: skipped")
        gate.releaseOne(); gate.releaseOne()
        for t in tasks { await t.value }
        XCTAssertEqual(gate.order, [1, 2, 5, 4])
    }
}
