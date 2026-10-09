// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import Foundation
import XCTest

@testable import BrookCore

/// Reading a session back out of `MemorySlot` (declared in `ChatIntegrationTests.swift`).
///
/// Why: tokens no longer cross the FFI (`FfiSession` is `{ user }` only), yet two tests must
/// hold the client's own refresh token to check what the server does with it. Core writes the
/// session to the slot it was given, so the test gives it a `MemorySlot` and reads the token
/// back from the stored JSON, which is what the Keychain slot would hold in the app.
extension MemorySlot {
    /// Give `client` this slot. Call it BEFORE `login`: core writes the session only through
    /// a slot that is already set. The data directory is a fresh temp directory; `test`
    /// removes it when the test ends (pass or fail), so runs do not litter the temp folder.
    func attach(to client: FfiBrookClient, in test: XCTestCase) throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("brook-itest-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        test.addTeardownBlock { try? FileManager.default.removeItem(at: dir) }
        client.enablePersistence(slot: self, dataDir: dir.path)
    }

    /// The refresh token core stored at login (`{"user": ..., "refresh_token": ...}`, see
    /// `persist.rs`). It takes the one slot named `session:...` and fails unless there is
    /// exactly one, rather than rebuilding the name from the origin string.
    func storedRefreshToken(file: StaticString = #filePath, line: UInt = #line) throws -> String {
        let sessions = contents().filter { $0.key.hasPrefix("session:") }
        XCTAssertEqual(sessions.count, 1, "expected exactly one session slot: \(sessions.keys)",
                       file: file, line: line)
        let data = try XCTUnwrap(sessions.values.first, file: file, line: line)
        let json = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any],
                                 file: file, line: line)
        let token = try XCTUnwrap(json["refresh_token"] as? String, file: file, line: line)
        // An empty token would make later "this token is dead" checks pass for the wrong reason.
        XCTAssertFalse(token.isEmpty, "stored refresh token is empty", file: file, line: line)
        return token
    }
}
