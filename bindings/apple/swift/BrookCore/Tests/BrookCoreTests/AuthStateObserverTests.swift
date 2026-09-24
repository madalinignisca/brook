import XCTest

@testable import BrookCore

final class AuthStateObserverTests: XCTestCase {
    /// Rust may deliver one late callback after `Subscription.cancel()`; Swift must drop it.
    func testForwarderDropsStatesDeliveredAfterCancel() {
        let seen = StateLog()
        let forwarder = Forwarder { state in seen.append(state) }

        forwarder.onState(state: .loggedOut)
        forwarder.cancel()
        forwarder.onState(state: .authenticating) // the late one

        XCTAssertEqual(seen.all, [.loggedOut])
    }

    /// End to end through the real Rust subscription, without a server: a fresh client is
    /// logged out, and that current state is delivered on subscribe.
    func testObserverReceivesCurrentStateFromRust() throws {
        let client = try FfiBrookClient(baseUrl: "https://brook.invalid", allowInsecureHttp: false)
        let delivered = expectation(description: "current state delivered")
        let observer = AuthStateObserver(client: client) { state in
            if state == .loggedOut { delivered.fulfill() }
        }
        wait(for: [delivered], timeout: 5)
        observer.cancel()
    }
}
