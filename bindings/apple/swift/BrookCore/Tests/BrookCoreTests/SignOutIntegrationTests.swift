import Foundation
import XCTest

@testable import BrookCore

/// Sign out against the real shared test server (spec 2026-09-25-sign-out, P4): the refresh
/// token the session held is refused by the server afterwards.
final class SignOutIntegrationTests: XCTestCase {
    func testSignOutRevokesTheRefreshTokenOnTheServer() async throws {
        let env = ProcessInfo.processInfo.environment
        guard let server = env["BROOK_TEST_SERVER"], let handle = env["BROOK_TEST_HANDLE"],
              let password = env["BROOK_TEST_PASSWORD"]
        else {
            let why = "BROOK_TEST_SERVER / BROOK_TEST_HANDLE / BROOK_TEST_PASSWORD not set"
            if env["BROOK_REQUIRE_ITEST"] == "1" { XCTFail(why) }
            throw XCTSkip(why)
        }
        let client = try FfiBrookClient(
            baseUrl: server, allowInsecureHttp: env["BROOK_TEST_ALLOW_INSECURE_HTTP"] == "1")
        // Credential checks share the server's per-IP bucket with the other suites: wait out
        // a 429 as the server asks, never count it as a result.
        var result: LoginResult?
        for _ in 0 ..< 12 where result == nil {
            do { result = try await client.login(handle: handle, password: password) } catch let LoginError.Api(code, _)
                where code == "auth.rate_limited" { try await Task.sleep(for: .seconds(7)) }
        }
        guard case let .loggedIn(session) = result else { return XCTFail("login did not sign in") }

        await client.logout()

        // Core revokes best-effort in its own task: give it time, then probe exactly once.
        // (A probe that succeeded would rotate the token and revoke it itself, so polling
        // until a 401 would pass even if sign-out revoked nothing.)
        try await Task.sleep(for: .seconds(2))
        var req = URLRequest(url: URL(string: server)!.appending(path: "api/v1/auth/refresh"))
        req.httpMethod = "POST"
        req.setValue("application/json", forHTTPHeaderField: "Content-Type")
        req.httpBody = try JSONSerialization.data(withJSONObject: ["refresh_token": session.refreshToken])
        let (_, response) = try await URLSession.shared.data(for: req)
        let status = (response as? HTTPURLResponse)?.statusCode ?? 0
        XCTAssertEqual(status, 401, "the signed-out refresh token still works on the server")
    }
}
