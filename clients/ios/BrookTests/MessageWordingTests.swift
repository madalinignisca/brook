// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

@testable import Brook
import XCTest

/// No text the iOS app shows may name the Mac. The shared messages interpolate `ThisDevice`;
/// this fails if the iOS `ThisDevice` (or a message) slips back to Mac wording.
@MainActor
final class MessageWordingTests: XCTestCase {
    // Swift cannot list the static constants of a type, so each is named here by hand. A
    // `Message` constant added later is NOT checked until it is added to this list: add it.
    private let messages: [String] = [
        SessionStore.Message.missingFields,
        SessionStore.Message.invalidAddress,
        SessionStore.Message.notJustAnAddress,
        SessionStore.Message.wrongCredentials,
        SessionStore.Message.unreachable,
        SessionStore.Message.unreachableLAN,
        SessionStore.Message.insecure,
        SessionStore.Message.unexpected,
        SessionStore.Message.signedOut,
        SessionStore.Message.wrongCode,
        SessionStore.Message.wrongRecoveryCode,
        SessionStore.Message.codeStepExpired,
        SessionStore.Message.codeFormat,
        SessionStore.Message.recoveryFormat,
        SessionStore.Message.keychainUnavailable,
        SessionStore.Message.restoreOffline,
        SessionStore.Message.signOutIncomplete,
        SessionStore.Message.secondInstance,
        SessionStore.Message.removalIncomplete,
        SessionStore.Message.removalAndSignOutIncomplete,
    ]

    func testNoMessageNamesTheMac() throws {
        let store = SessionStore(
            settings: Settings(defaults: isolatedDefaults(), environment: ["BROOK_ALLOW_INSECURE_HTTP": "1"]),
            makeFeed: nil)
        let warning = try XCTUnwrap(LoginForm(store: store).insecureWarning)
        for text in messages + [warning] {
            XCTAssertFalse(text.contains("Mac"), text)
            XCTAssertFalse(text.contains("macOS"), text)
        }
    }

    /// The device-specific wording itself, so a blanked-out `ThisDevice` fails too.
    func testTheIPhoneWording() {
        XCTAssertTrue(SessionStore.Message.unreachableLAN.contains("If your iPhone asked to allow local network access"))
        XCTAssertTrue(SessionStore.Message.signOutIncomplete.hasPrefix("This iPhone couldn't forget your saved sign-in"))
        XCTAssertTrue(SessionStore.Message.removalIncomplete.contains("this iPhone's data"))
        XCTAssertTrue(SessionStore.Message.removalAndSignOutIncomplete.contains("this iPhone's data"))
        XCTAssertEqual(Settings.fallbackServer, "")
    }
}
