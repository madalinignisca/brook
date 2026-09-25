import CryptoKit
import Foundation
import XCTest

@testable import BrookCore

/// TOTP against the real shared test server (spec 2026-09-25-totp-clients, plan P4). Codes are
/// computed here from the enrolment's secret (RFC 6238: HMAC-SHA1, 6 digits, 30 s), and no step
/// is ever reused: every code-accepting endpoint shares the server's replay guard. Throwaway
/// accounts only; credential checks share the server's per-IP bucket, so a 429 is waited out.
final class TotpIntegrationTests: XCTestCase {
    private struct Config {
        let server: URL
        let adminHandle: String
        let adminPassword: String
        let allowInsecureHttp: Bool
    }

    private struct Pair { let access: String; let refresh: String }
    private let password = "throwaway-totp-1"

    private func config() throws -> Config {
        let env = ProcessInfo.processInfo.environment
        guard let server = env["BROOK_TEST_SERVER"].flatMap(URL.init(string:)),
              let adminHandle = env["BROOK_TEST_ADMIN_HANDLE"],
              let adminPassword = env["BROOK_TEST_ADMIN_PASSWORD"]
        else {
            let why = "BROOK_TEST_SERVER / BROOK_TEST_ADMIN_HANDLE / BROOK_TEST_ADMIN_PASSWORD not set"
            if env["BROOK_REQUIRE_ITEST"] == "1" { XCTFail(why) }
            throw XCTSkip(why)
        }
        return Config(server: server, adminHandle: adminHandle, adminPassword: adminPassword,
                      allowInsecureHttp: env["BROOK_TEST_ALLOW_INSECURE_HTTP"] == "1")
    }

    // MARK: RFC 6238

    private static func base32(_ text: String) -> Data {
        let alphabet = Array("ABCDEFGHIJKLMNOPQRSTUVWXYZ234567")
        var bits = 0, value = 0
        var out = Data()
        for ch in text.uppercased() where ch != "=" {
            guard let index = alphabet.firstIndex(of: ch) else { continue }
            value = (value << 5) | index
            bits += 5
            if bits >= 8 {
                out.append(UInt8((value >> (bits - 8)) & 0xFF))
                bits -= 8
            }
        }
        return out
    }

    private static func secret(from uri: String) -> Data {
        let item = URLComponents(string: uri)?.queryItems?.first { $0.name == "secret" }?.value
        return base32(item ?? "")
    }

    private static func step(_ date: Date = Date()) -> UInt64 { UInt64(date.timeIntervalSince1970) / 30 }

    private static func code(_ secret: Data, step: UInt64) -> String {
        var counter = step.bigEndian
        let message = Data(bytes: &counter, count: 8)
        let mac = Array(HMAC<Insecure.SHA1>.authenticationCode(for: message, using: SymmetricKey(data: secret)))
        let offset = Int(mac[19] & 0x0F)
        let number = (UInt32(mac[offset] & 0x7F) << 24) | (UInt32(mac[offset + 1]) << 16)
            | (UInt32(mac[offset + 2]) << 8) | UInt32(mac[offset + 3])
        return String(format: "%06u", number % 1_000_000)
    }

    /// Waits until the step after `used`, and returns it: a code never reused.
    private func nextStep(after used: UInt64) async throws -> UInt64 {
        while Self.step() <= used { try await Task.sleep(for: .milliseconds(500)) }
        return Self.step()
    }

    // MARK: raw HTTP (another device, the admin), paced on 429

    private func send(_ cfg: Config, _ method: String, _ path: String, bearer: String? = nil,
                      json: [String: Any]? = nil) async throws -> (Int, [String: Any]) {
        var req = URLRequest(url: cfg.server.appending(path: "api/v1/\(path)"))
        req.httpMethod = method
        if let bearer { req.setValue("Bearer \(bearer)", forHTTPHeaderField: "Authorization") }
        if let json {
            req.setValue("application/json", forHTTPHeaderField: "Content-Type")
            req.httpBody = try JSONSerialization.data(withJSONObject: json)
        }
        for _ in 0 ..< 10 {
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
        throw XCTSkip("still rate limited: \(method) \(path)")
    }

    private func paced<T>(_ call: () async throws -> T) async throws -> T {
        for _ in 0 ..< 12 {
            do { return try await call() } catch let LoginError.Api(code, _) where code == "auth.rate_limited" {
                try await Task.sleep(for: .seconds(7))
            }
        }
        return try await call()
    }

    private func throwaway(_ cfg: Config) async throws -> (String, String) {
        let (s, admin) = try await send(cfg, "POST", "auth/login",
                                        json: ["handle": cfg.adminHandle, "password": cfg.adminPassword])
        XCTAssertEqual(s, 200, "admin login")
        let handle = "tf-" + UUID().uuidString.prefix(8).lowercased()
        let (status, body) = try await send(
            cfg, "POST", "auth/register", bearer: admin["access_token"] as? String,
            json: ["handle": handle, "display_name": "Throwaway \(handle)", "password": password])
        XCTAssertEqual(status, 201, "register \(handle)")
        return (handle, try XCTUnwrap(body["id"] as? String))
    }

    private func client(_ cfg: Config) throws -> FfiBrookClient {
        try FfiBrookClient(baseUrl: cfg.server.absoluteString, allowInsecureHttp: cfg.allowInsecureHttp)
    }

    private func challenge(_ c: FfiBrookClient, _ handle: String) async throws -> FfiTotpChallenge {
        let result = try await paced { try await c.login(handle: handle, password: self.password) }
        guard case let .totpRequired(ch) = result else { throw XCTSkip("expected the code step, got \(result)") }
        return ch
    }

    /// True only for the server's `auth.invalid_code`. A rate limit is waited out and retried;
    /// anything else (a success included) is reported, never read as "refused".
    private func isInvalidCode(_ call: () async throws -> UInt32?) async -> Bool {
        for _ in 0 ..< 12 {
            do {
                let left = try await call()
                XCTFail("accepted (codes left: \(String(describing: left)))")
                return false
            } catch let LoginError.Api(code, _) where code == "auth.rate_limited" {
                try? await Task.sleep(for: .seconds(7))
            } catch let LoginError.Api(code, _) {
                if code != "auth.invalid_code" { XCTFail("refused with \(code), not auth.invalid_code") }
                return code == "auth.invalid_code"
            } catch {
                XCTFail("failed with \(error)")
                return false
            }
        }
        XCTFail("still rate limited")
        return false
    }

    // MARK: the run

    func testTwoFactorSignInEndToEnd() async throws {
        let cfg = try config()
        let (handle, id) = try await throwaway(cfg)

        // Two devices signed in with the password: A (the app), B (raw tokens).
        let a = try client(cfg)
        guard case let .loggedIn(aSession) = try await paced({ try await a.login(handle: handle, password: self.password) })
        else { return XCTFail("password login") }
        let (_, bBody) = try await send(cfg, "POST", "auth/login", json: ["handle": handle, "password": password])
        let b = Pair(access: bBody["access_token"] as! String, refresh: bBody["refresh_token"] as! String)

        // 1. Enrol and activate on A: both devices' old tokens are cut off; A's new pair works.
        let enrollment = try await paced { try await a.totpEnroll(password: self.password) }
        let secret = Self.secret(from: enrollment.otpauthUri())
        var used = Self.step()
        let codes = try await paced { try await a.totpActivate(code: Self.code(secret, step: used)) }
        XCTAssertEqual(codes.count, 10)
        for (who, access) in [("B", b.access), ("A's old", aSession.accessToken)] {
            let (me, _) = try await send(cfg, "GET", "auth/me", bearer: access)
            XCTAssertEqual(me, 401, "\(who) access token survived activation")
        }
        for (who, refresh) in [("B", b.refresh), ("A's old", aSession.refreshToken)] {
            let (r, _) = try await send(cfg, "POST", "auth/refresh", json: ["refresh_token": refresh])
            XCTAssertEqual(r, 401, "\(who) refresh token survived activation")
        }
        let me = try await a.me()
        XCTAssertTrue(me.totpEnabled, "A's new pair doesn't work, or TOTP isn't on")

        // 2. Password + code. The code used at activation is replayed on a fresh challenge while
        //    still inside its window: refused. The next step's code on that challenge succeeds,
        //    so the refusal was the replay guard, not an expired or consumed challenge.
        let c = try client(cfg)
        let ch = try await challenge(c, handle)
        let replayed = await isInvalidCode { try await c.completeTotp(challenge: ch, code: Self.code(secret, step: used)) }
        XCTAssertTrue(replayed, "a code accepted at activation signed in again")
        used = try await nextStep(after: used)
        let left = try await paced { try await c.completeTotp(challenge: ch, code: Self.code(secret, step: used)) }
        XCTAssertNil(left)

        // 3. A recovery code signs in once.
        let d = try client(cfg)
        let dch = try await challenge(d, handle)
        let afterRecovery = try await paced { try await d.completeRecovery(challenge: dch, recoveryCode: codes[0]) }
        XCTAssertEqual(afterRecovery, 9)
        let e = try client(cfg)
        let ech = try await challenge(e, handle)
        let reused = await isInvalidCode { try await e.completeRecovery(challenge: ech, recoveryCode: codes[0]) }
        XCTAssertTrue(reused, "a recovery code worked twice")
        await e.cancelTotp(challenge: ech)

        // 4. New recovery codes: an old unused one is refused, a new one signs in.
        used = try await nextStep(after: used)
        let fresh = try await paced {
            try await c.totpRegenerateRecoveryCodes(password: self.password, factor: .code(code: Self.code(secret, step: used)))
        }
        XCTAssertEqual(fresh.count, 10)
        let f = try client(cfg)
        let fch = try await challenge(f, handle)
        let old = await isInvalidCode { try await f.completeRecovery(challenge: fch, recoveryCode: codes[1]) }
        XCTAssertTrue(old, "an old recovery code survived regeneration")
        let afterNew = try await paced { try await f.completeRecovery(challenge: fch, recoveryCode: fresh[0]) }
        XCTAssertEqual(afterNew, 9)

        // 5. Admin reset while TOTP is on, with a raw device's session open: it's cut off, and
        //    the password alone signs in again.
        let (_, hLogin) = try await send(cfg, "POST", "auth/login",
                                         json: ["handle": handle, "password": password, "supports_totp": true])
        let (hs, hBody) = try await send(cfg, "POST", "auth/totp",
                                         json: ["totp_token": hLogin["totp_token"] as? String ?? "", "recovery_code": fresh[1]])
        XCTAssertEqual(hs, 200, "raw device sign-in")
        let h = Pair(access: hBody["access_token"] as? String ?? "", refresh: hBody["refresh_token"] as? String ?? "")
        let admin = try client(cfg)
        _ = try await paced { try await admin.login(handle: cfg.adminHandle, password: cfg.adminPassword) }
        try await paced { try await admin.adminResetTotp(userId: id, adminPassword: cfg.adminPassword) }
        let (hMe, _) = try await send(cfg, "GET", "auth/me", bearer: h.access)
        XCTAssertEqual(hMe, 401, "the target's access token survived the reset")
        let (hRefresh, _) = try await send(cfg, "POST", "auth/refresh", json: ["refresh_token": h.refresh])
        XCTAssertEqual(hRefresh, 401, "the target's refresh token survived the reset")
        let g = try client(cfg)
        guard case .loggedIn = try await paced({ try await g.login(handle: handle, password: self.password) })
        else { return XCTFail("the password alone doesn't sign in after the reset") }

        // 6. Turning it off (a second throwaway account): password-only works again.
        let (other, _) = try await throwaway(cfg)
        let u = try client(cfg)
        _ = try await paced { try await u.login(handle: other, password: self.password) }
        let uSecret = Self.secret(from: try await paced { try await u.totpEnroll(password: self.password) }.otpauthUri())
        var uUsed = Self.step()
        _ = try await paced { try await u.totpActivate(code: Self.code(uSecret, step: uUsed)) }
        uUsed = try await nextStep(after: uUsed)
        try await paced { try await u.totpDisable(password: self.password, factor: .code(code: Self.code(uSecret, step: uUsed))) }
        let uMe = try await u.me()
        XCTAssertFalse(uMe.totpEnabled)
        let (plain, plainBody) = try await send(cfg, "POST", "auth/login",
                                                json: ["handle": other, "password": password, "supports_totp": true])
        XCTAssertEqual(plain, 200)
        XCTAssertNotNil(plainBody["access_token"], "still asked for a code after turning it off")
    }
}
