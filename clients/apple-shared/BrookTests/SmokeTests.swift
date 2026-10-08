// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import Foundation
import XCTest

/// Proves the app target links the Rust core and can call into it. Shared by the Mac and iOS
/// test targets, so it runs once per platform.
final class SmokeTests: XCTestCase {
    func testAppLinksAndCallsTheRustCore() throws {
        _ = try FfiBrookClient(baseUrl: "https://brook.invalid", allowInsecureHttp: false)
        XCTAssertThrowsError(
            try FfiBrookClient(baseUrl: "http://192.168.1.50", allowInsecureHttp: false)
        ) { error in
            XCTAssertEqual(error as? LoginError, .InsecureServerUrl)
        }
    }

    /// The synchronous test above never leaves the calling thread. A sign-in needs more: core's
    /// tokio runtime, an async future handed across UniFFI, and DNS. `.invalid` never resolves
    /// (RFC 6761), so no socket connects and no TLS handshake happens: this proves the runtime
    /// and the resolver, not TLS.
    func testAsyncLoginRunsOnTheRuntimeAndFailsOnTheNetwork() async throws {
        let client = try FfiBrookClient(baseUrl: "https://brook.invalid", allowInsecureHttp: false)
        do {
            _ = try await client.login(handle: "x", password: "y")
            XCTFail("login to brook.invalid must not succeed")
        } catch let error as LoginError {
            guard case .Network = error else { return XCTFail("expected .Network, got \(error)") }
        }
    }

    /// Core calls back into Swift during `restore`; this is the Rust-to-Swift direction of UniFFI
    /// (the other tests only go Swift-to-Rust). With nothing stored the fake returns nil, and
    /// core must answer `.notSignedIn` after asking for the `session:` slot.
    func testRestoreCallsBackIntoASwiftKeySlot() async throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("brook-smoke-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let slot = RecordingSlot()
        let client = try FfiBrookClient(baseUrl: "https://brook.invalid", allowInsecureHttp: false)
        client.enablePersistence(slot: slot, dataDir: dir.path)
        let outcome = await client.restore()

        XCTAssertEqual(outcome, .notSignedIn)
        XCTAssertTrue(
            slot.loaded.contains { $0.hasPrefix("session:") },
            "core never asked the Swift slot for a session: \(slot.loaded)"
        )
    }
}

/// A key slot that holds nothing and records which slots core asked for.
private final class RecordingSlot: FfiKeySlot, @unchecked Sendable {
    private let lock = NSLock()
    private var names: [String] = []
    var loaded: [String] { lock.withLock { names } }

    func load(slot: String) throws -> Data? {
        lock.withLock { names.append(slot) }
        return nil
    }
    func create(slot _: String, bytes _: Data) throws {}
    func replace(slot _: String, bytes _: Data) throws {}
    func delete(slot _: String) throws {}
}
