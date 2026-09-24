import Foundation
import XCTest

@testable import BrookCore

/// Against the real shared test server. Configured by `itest.sh` from the gitignored
/// `.itest.env`. With `BROOK_REQUIRE_ITEST=1` a missing configuration FAILS instead of
/// skipping, so "no server" can never read as "passed".
final class LoginIntegrationTests: XCTestCase {
    private struct Config {
        let server: String
        let handle: String
        let password: String
        let allowInsecureHttp: Bool
    }

    private func config() throws -> Config {
        let env = ProcessInfo.processInfo.environment
        guard let server = env["BROOK_TEST_SERVER"], let handle = env["BROOK_TEST_HANDLE"],
              let password = env["BROOK_TEST_PASSWORD"]
        else {
            let why = "BROOK_TEST_SERVER / BROOK_TEST_HANDLE / BROOK_TEST_PASSWORD not set"
            if env["BROOK_REQUIRE_ITEST"] == "1" {
                XCTFail(why)
                throw XCTSkip("failed above: \(why)") // stop the test body; the failure stands
            }
            throw XCTSkip(why)
        }
        return Config(
            server: server, handle: handle, password: password,
            allowInsecureHttp: env["BROOK_TEST_ALLOW_INSECURE_HTTP"] == "1"
        )
    }

    /// Waits until `predicate` holds for the latest observed state.
    private func waitForState(
        _ states: StateLog, timeout: TimeInterval = 10,
        _ predicate: (FfiAuthState) -> Bool
    ) -> [FfiAuthState] {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            let snapshot = states.all
            if let last = snapshot.last, predicate(last) { return snapshot }
            Thread.sleep(forTimeInterval: 0.05)
        }
        return states.all
    }

    private func rank(_ s: FfiAuthState) -> Int {
        switch s {
        case .loggedOut: 0
        case .authenticating: 1
        case .loggedIn, .failed: 2
        }
    }

    func testLoginReturnsWorkingTokensAndEndsLoggedIn() async throws {
        let cfg = try config()
        let client = try FfiBrookClient(baseUrl: cfg.server, allowInsecureHttp: cfg.allowInsecureHttp)
        let states = StateLog()
        let observer = AuthStateObserver(client: client) { s in states.append(s) }
        defer { observer.cancel() }

        let result = try await client.login(handle: cfg.handle, password: cfg.password)
        guard case let .loggedIn(session) = result else { return XCTFail("unexpected \(result)") }

        XCTAssertEqual(session.user.handle, cfg.handle)
        XCTAssertFalse(session.accessToken.isEmpty)
        XCTAssertFalse(session.refreshToken.isEmpty)
        XCTAssertNotEqual(session.accessToken, session.refreshToken)

        // The token Swift received must be the one the server accepts as an access token.
        var me = URLRequest(url: URL(string: cfg.server)!.appending(path: "api/v1/auth/me"))
        me.setValue("Bearer \(session.accessToken)", forHTTPHeaderField: "Authorization")
        let (body, response) = try await URLSession.shared.data(for: me)
        XCTAssertEqual((response as? HTTPURLResponse)?.statusCode, 200)
        let meJSON = try JSONSerialization.jsonObject(with: body) as? [String: Any]
        XCTAssertEqual(meJSON?["handle"] as? String, cfg.handle)

        let observed = waitForState(states) { if case .loggedIn = $0 { true } else { false } }
        guard case let .loggedIn(user)? = observed.last else {
            return XCTFail("final state is not loggedIn: \(observed)")
        }
        XCTAssertEqual(user.handle, cfg.handle)
        XCTAssertTrue(zip(observed, observed.dropFirst()).allSatisfy { rank($0) <= rank($1) },
                      "order regressed: \(observed)")
    }

    func testWrongPasswordIsRejectedWithItsCodeAndEndsFailed() async throws {
        let cfg = try config()
        let client = try FfiBrookClient(baseUrl: cfg.server, allowInsecureHttp: cfg.allowInsecureHttp)
        let states = StateLog()
        let observer = AuthStateObserver(client: client) { s in states.append(s) }
        defer { observer.cancel() }

        do {
            _ = try await client.login(handle: cfg.handle, password: cfg.password + "-wrong")
            XCTFail("login with a wrong password succeeded")
        } catch let LoginError.Api(code, _) {
            XCTAssertEqual(code, "auth.invalid_credentials")
        }

        let observed = waitForState(states) { if case .failed = $0 { true } else { false } }
        guard case .failed? = observed.last else {
            return XCTFail("final state is not failed: \(observed)")
        }
    }
}
