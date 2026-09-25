import XCTest

@testable import BrookCore

/// The offline API through the real Rust client, without a server or a Keychain: local data
/// is off until `enableLocalData`, so reads say `local.unavailable`, and the state feed
/// delivers its default at once.
final class OfflineAPITests: XCTestCase {
    private final class StateSink: CacheStateListener, @unchecked Sendable {
        let delivered: XCTestExpectation
        private(set) var last: FfiCacheState?
        init(_ delivered: XCTestExpectation) { self.delivered = delivered }
        func onCacheState(state: FfiCacheState) {
            last = state
            delivered.fulfill()
        }
    }

    private final class EventSink: CacheEventListener, @unchecked Sendable {
        func onCacheEvent(event: FfiCacheEvent) {}
    }

    func testTheStateFeedStartsAtTheDefault() throws {
        let client = try FfiBrookClient(baseUrl: "https://brook.invalid", allowInsecureHttp: false)
        let sink = StateSink(expectation(description: "current state delivered"))
        let sub = client.subscribeCacheState(listener: sink)
        wait(for: [sink.delivered], timeout: 5)
        XCTAssertEqual(sink.last, FfiCacheState(syncing: false, lastSyncedUnixMs: nil, offline: false))
        sub.cancel()
        client.subscribeCacheEvents(listener: EventSink()).cancel()
    }

    func testReadsSayLocalDataIsUnavailableUntilEnabled() async throws {
        let client = try FfiBrookClient(baseUrl: "https://brook.invalid", allowInsecureHttp: false)
        XCTAssertNil(client.outboxLost())
        do {
            _ = try await client.cachedChannels()
            XCTFail("cached channels answered with local data off")
        } catch LoginError.Api(let code, _) {
            XCTAssertEqual(code, "local.unavailable")
        }
        do {
            _ = try await client.sendQueued(channelId: "c", body: "hi", replyToId: nil, clientId: UUID().uuidString)
            XCTFail("queued a message with local data off")
        } catch LoginError.Api(let code, _) {
            XCTAssertEqual(code, "local.unavailable")
        }
        let unsent = await client.unsentCount()
        XCTAssertEqual(unsent, 0)
    }
}
