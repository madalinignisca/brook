import AppKit
import BrookCore
@testable import Brook
import SwiftUI
import XCTest

/// Renders the app's views off-screen to PNGs for review (no window, no screen control).
/// Runs only when BROOK_SCREENSHOTS=1; skipped in normal test runs. The test host is the
/// sandboxed app, so files land in its container's temporary directory (printed below).
@MainActor
final class ScreenshotRenderer: XCTestCase {
    private func render(_ view: some View, _ name: String, dark: Bool, to dir: URL) throws {
        // Off-screen there is no window to paint the background; supply the system one.
        let framed = view.frame(width: 420, height: 560).background(Color(nsColor: .windowBackgroundColor))
        let host = NSHostingView(rootView: framed)
        host.frame = NSRect(x: 0, y: 0, width: 420, height: 560)
        host.appearance = NSAppearance(named: dark ? .darkAqua : .aqua)
        host.layoutSubtreeIfNeeded()
        let rep = try XCTUnwrap(host.bitmapImageRepForCachingDisplay(in: host.bounds))
        host.cacheDisplay(in: host.bounds, to: rep)
        let png = try XCTUnwrap(rep.representation(using: .png, properties: [:]))
        try png.write(to: dir.appending(path: "\(name)-\(dark ? "dark" : "light").png"))
    }

    func testRenderScreens() async throws {
        guard ProcessInfo.processInfo.environment["BROOK_SCREENSHOTS"] == "1" else {
            throw XCTSkip("set BROOK_SCREENSHOTS=1 to render screenshots")
        }
        let dir = FileManager.default.temporaryDirectory.appending(path: "brook-screens")
        try? FileManager.default.removeItem(at: dir)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        print("BROOK_SCREENSHOTS_AT \(dir.path)")
        let defaults = try XCTUnwrap(UserDefaults(suiteName: "brook.screens.\(UUID().uuidString)"))

        func form(insecure: Bool = false, result: Result<LoginResult, LoginError> = .failure(.UnexpectedResponse)) -> LoginForm {
            let env = insecure ? ["BROOK_ALLOW_INSECURE_HTTP": "1"] : [:]
            let recorder = FactoryRecorder { FakeClient(result: result) }
            return LoginForm(store: SessionStore(settings: Settings(defaults: defaults, environment: env), makeClient: recorder.factory))
        }

        for dark in [false, true] {
            try render(LoginView(form: form()), "1-login-empty", dark: dark, to: dir)

            let invalid = form()
            invalid.server = "https://chat.example.com"
            invalid.handle = ""
            invalid.password = "secret"
            await invalid.submit()
            try render(LoginView(form: invalid), "2-login-missing-fields", dark: dark, to: dir)

            let wrong = form(insecure: true, result: .failure(.Api(code: "auth.invalid_credentials", message: "x")))
            wrong.server = "http://192.168.1.192:8080"
            wrong.handle = "alice"
            wrong.password = "wrong"
            await wrong.submit()
            try render(LoginView(form: wrong), "3-login-wrong-password-insecure", dark: dark, to: dir)

            try render(SignedInView(user: alice), "4-signed-in", dark: dark, to: dir)
        }
    }
}
