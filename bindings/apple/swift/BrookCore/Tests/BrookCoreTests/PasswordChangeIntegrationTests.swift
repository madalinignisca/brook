import Foundation
import XCTest

@testable import BrookCore

/// Password change and admin reset against the real shared test server (spec
/// 2026-09-25-password-change-clients-design.md, P4 and §9). Every run registers its own
/// throwaway accounts through the admin, so the shared accounts other suites depend on are never
/// touched; a run that stops midway leaves only throwaway accounts behind.
///
/// "Device B" is raw HTTP and a raw WebSocket, so what the server does to another device is
/// observed directly: its old access token refused at once, its open socket closed with
/// `session_revoked`, well inside the 15-minute access-token lifetime.
///
/// Every credential check (login, refresh, register, password change) takes a token from the
/// server's per-IP bucket (10, refilled at 10/minute), and on the compose test server every LAN
/// client shares one IP. So a 429 is waited out as the server asks, never counted as a result,
/// and the suite spends about 20 checks per run.
final class PasswordChangeIntegrationTests: XCTestCase {
    private struct Config {
        let server: URL
        let adminHandle: String
        let adminPassword: String
        let allowInsecureHttp: Bool
    }

    private struct Pair { let access: String; let refresh: String }

    private let oldPassword = "throwaway-old-1"
    private let newPassword = "throwaway-new-2"

    private func config() throws -> Config {
        let env = ProcessInfo.processInfo.environment
        guard let server = env["BROOK_TEST_SERVER"].flatMap(URL.init(string:)),
              let adminHandle = env["BROOK_TEST_ADMIN_HANDLE"],
              let adminPassword = env["BROOK_TEST_ADMIN_PASSWORD"]
        else {
            let why = "BROOK_TEST_SERVER / BROOK_TEST_ADMIN_HANDLE / BROOK_TEST_ADMIN_PASSWORD not set"
            if env["BROOK_REQUIRE_ITEST"] == "1" {
                XCTFail(why)
                throw XCTSkip("failed above: \(why)")
            }
            throw XCTSkip(why)
        }
        return Config(
            server: server, adminHandle: adminHandle, adminPassword: adminPassword,
            allowInsecureHttp: env["BROOK_TEST_ALLOW_INSECURE_HTTP"] == "1")
    }

    // MARK: raw HTTP (device B, the admin's registration)

    private func api(_ cfg: Config, _ path: String) -> URL {
        cfg.server.appending(path: "api/v1/\(path)")
    }

    private func send(
        _ cfg: Config, _ method: String, _ path: String, bearer: String? = nil, json: [String: Any]? = nil
    ) async throws -> (Int, [String: Any]) {
        var req = URLRequest(url: api(cfg, path))
        req.httpMethod = method
        if let bearer { req.setValue("Bearer \(bearer)", forHTTPHeaderField: "Authorization") }
        if let json {
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try JSONSerialization.data(withJSONObject: json)
        }
        for _ in 0..<8 {
            let (body, response) = try await URLSession.shared.data(for: req)
            let http = response as? HTTPURLResponse
            if http?.statusCode == 429 {
                let wait = http?.value(forHTTPHeaderField: "Retry-After").flatMap(Int.init) ?? 10
                try await Task.sleep(for: .seconds(min(max(wait, 1), 70)))
                continue
            }
            let object = (try? JSONSerialization.jsonObject(with: body)) as? [String: Any] ?? [:]
            return (http?.statusCode ?? 0, object)
        }
        throw XCTSkip("still rate limited after 8 waits: \(method) \(path)")
    }

    /// A bindings call, retried while the server says to wait (the shared LAN bucket).
    private func paced<T>(_ call: () async throws -> T) async throws -> T {
        for _ in 0..<12 {
            do { return try await call() } catch let LoginError.Api(code, _) where code == "auth.rate_limited" {
                try await Task.sleep(for: .seconds(7))
            }
        }
        return try await call()
    }

    private var adminAccess: String?

    /// The admin's access token, logged in once per test case.

    private func login(_ cfg: Config, _ handle: String, _ password: String) async throws -> Pair? {
        let (status, body) = try await send(
            cfg, "POST", "auth/login", json: ["handle": handle, "password": password])
        guard status == 200, let a = body["access_token"] as? String,
              let r = body["refresh_token"] as? String
        else { return nil }
        return Pair(access: a, refresh: r)
    }

    private func admin(_ cfg: Config) async throws -> String {
        if let adminAccess { return adminAccess }
        let pair = try await login(cfg, cfg.adminHandle, cfg.adminPassword)
        adminAccess = try XCTUnwrap(pair, "admin login failed").access
        return adminAccess!
    }

    /// A new member account, created by the admin; returns (handle, id).
    private func throwaway(_ cfg: Config) async throws -> (String, String) {
        let adminToken = try await admin(cfg)
        let handle = "pc-" + UUID().uuidString.prefix(8).lowercased()
        let (status, body) = try await send(
            cfg, "POST", "auth/register", bearer: adminToken,
            json: ["handle": handle, "display_name": "Throwaway \(handle)", "password": oldPassword])
        XCTAssertEqual(status, 201, "register \(handle)")
        return (handle, try XCTUnwrap(body["id"] as? String))
    }

    // MARK: raw WebSocket (device B's open socket)

    /// An authenticated socket (the server has answered `ready`).
    private func openSocket(_ cfg: Config, access: String) async throws -> URLSessionWebSocketTask {
        // At the root, not under /api/v1.
        var comps = URLComponents(url: cfg.server.appending(path: "ws"), resolvingAgainstBaseURL: false)!
        comps.scheme = comps.scheme == "https" ? "wss" : "ws"
        let task = URLSession.shared.webSocketTask(with: comps.url!)
        task.resume()
        let auth = try JSONSerialization.data(withJSONObject: ["type": "auth", "data": ["access_token": access]])
        try await task.send(.string(String(decoding: auth, as: UTF8.self)))
        while true {
            guard case let .string(text) = try await task.receive() else { continue }
            if text.contains("\"ready\"") { return task }
        }
    }

    /// The close the server sent within `timeout`, as (code, reason), or nil if still open.
    private func closeOf(_ task: URLSessionWebSocketTask, within timeout: TimeInterval) async -> (Int, String)? {
        let reader = Task { while true { _ = try await task.receive() } }
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline, task.closeCode == .invalid {
            try? await Task.sleep(for: .milliseconds(50))
        }
        reader.cancel()
        guard task.closeCode != .invalid else { return nil }
        let reason = task.closeReason.map { String(decoding: $0, as: UTF8.self) } ?? ""
        return (task.closeCode.rawValue, reason)
    }

    // MARK: device A (the app's client, through the bindings)

    private func deviceA(_ cfg: Config, _ handle: String) async throws -> FfiBrookClient {
        let client = try FfiBrookClient(
            baseUrl: cfg.server.absoluteString, allowInsecureHttp: cfg.allowInsecureHttp)
        let result = try await paced { try await client.login(handle: handle, password: self.oldPassword) }
        guard case .loggedIn = result else { throw XCTSkip("device A login: \(result)") }
        return client
    }

    // MARK: tests

    /// Checked (the default): the other device is cut off at once on REST and on its open
    /// socket, its refresh is dead, this device keeps working, and only the new password signs in.
    func testSigningOutOtherDevicesCutsThemOffAtOnce() async throws {
        let cfg = try config()
        let (handle, _) = try await throwaway(cfg)
        let a = try await deviceA(cfg, handle)
        let bPair = try await login(cfg, handle, oldPassword)
        let b = try XCTUnwrap(bPair)
        let socket = try await openSocket(cfg, access: b.access)

        let outcome = try await paced {
            try await a.changePassword(current: self.oldPassword, new: self.newPassword, signOutOtherDevices: true)
        }
        XCTAssertEqual(outcome, true, "the server did not say it signed the others out")

        let (me, _) = try await send(cfg, "GET", "auth/me", bearer: b.access)
        XCTAssertEqual(me, 401, "the other device's access token still works")
        let closed = await closeOf(socket, within: 5)
        XCTAssertEqual(closed?.0, 1008, "the other device's socket stayed open")
        XCTAssertEqual(closed?.1, "session_revoked")
        let (refresh, _) = try await send(cfg, "POST", "auth/refresh", json: ["refresh_token": b.refresh])
        XCTAssertEqual(refresh, 401, "the other device can still refresh")

        _ = try await a.listChannels() // this device is still signed in
        let signedIn = try await login(cfg, handle, newPassword)
        XCTAssertNotNil(signedIn, "the new password does not sign in")
        let old = try await login(cfg, handle, oldPassword) // one deliberate failure per run
        XCTAssertNil(old, "the old password still signs in")
    }

    /// Unchecked: the password changes and the other device keeps its access token, its open
    /// socket and its refresh.
    func testKeepingOtherDevicesSignedIn() async throws {
        let cfg = try config()
        let (handle, _) = try await throwaway(cfg)
        let a = try await deviceA(cfg, handle)
        let bPair = try await login(cfg, handle, oldPassword)
        let b = try XCTUnwrap(bPair)
        let socket = try await openSocket(cfg, access: b.access)

        let outcome = try await paced {
            try await a.changePassword(current: self.oldPassword, new: self.newPassword, signOutOtherDevices: false)
        }
        XCTAssertEqual(outcome, false)

        let (me, _) = try await send(cfg, "GET", "auth/me", bearer: b.access)
        XCTAssertEqual(me, 200, "the other device was cut off")
        let closed = await closeOf(socket, within: 2)
        XCTAssertNil(closed.map { "\($0.0) \($0.1)" }, "the other device's socket was closed")
        socket.cancel(with: .normalClosure, reason: nil)
        let (refresh, _) = try await send(cfg, "POST", "auth/refresh", json: ["refresh_token": b.refresh])
        XCTAssertEqual(refresh, 200, "the other device lost its refresh")
        _ = try await a.listChannels()
        let signedIn = try await login(cfg, handle, newPassword)
        XCTAssertNotNil(signedIn)
    }

    /// Admin reset: the target is cut off everywhere at once, and only the new password works.
    func testAdminResetCutsTheTargetOffAtOnce() async throws {
        let cfg = try config()
        let (handle, id) = try await throwaway(cfg)
        let targetPair = try await login(cfg, handle, oldPassword)
        let target = try XCTUnwrap(targetPair)
        let socket = try await openSocket(cfg, access: target.access)
        let admin = try FfiBrookClient(
            baseUrl: cfg.server.absoluteString, allowInsecureHttp: cfg.allowInsecureHttp)
        _ = try await paced { try await admin.login(handle: cfg.adminHandle, password: cfg.adminPassword) }

        try await paced {
            try await admin.adminResetPassword(userId: id, adminPassword: cfg.adminPassword, new: self.newPassword)
        }

        let (me, _) = try await send(cfg, "GET", "auth/me", bearer: target.access)
        XCTAssertEqual(me, 401, "the target's access token still works")
        let closed = await closeOf(socket, within: 5)
        XCTAssertEqual(closed?.0, 1008, "the target's socket stayed open")
        XCTAssertEqual(closed?.1, "session_revoked")
        let (refresh, _) = try await send(
            cfg, "POST", "auth/refresh", json: ["refresh_token": target.refresh])
        XCTAssertEqual(refresh, 401)
        let signedIn = try await login(cfg, handle, newPassword)
        XCTAssertNotNil(signedIn, "the new password does not sign in")
        _ = try await admin.listUsers() // the admin's own session is untouched
    }
}
