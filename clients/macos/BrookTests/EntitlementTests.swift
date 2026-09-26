import Security
import XCTest

/// The app's own sandbox, read from the running app (the tests are hosted in it).
final class EntitlementTests: XCTestCase {
    private func entitlement(_ key: String) -> Any? {
        guard let task = SecTaskCreateFromSelf(nil) else { return nil }
        return SecTaskCopyValueForEntitlement(task, key as CFString, nil)
    }

    /// Save… (NSSavePanel) and Attach (NSOpenPanel) need it: without it a sandboxed app
    /// can't present either panel, and a chosen file couldn't be written or read.
    func testTheUserMayChooseFilesToSaveAndToAttach() {
        XCTAssertEqual(entitlement("com.apple.security.files.user-selected.read-write") as? Bool, true)
    }

    /// Nothing wider than what the user chooses: no general file or folder access.
    func testNoWiderFileAccess() {
        for key in ["com.apple.security.files.downloads.read-write",
                    "com.apple.security.files.all",
                    "com.apple.security.temporary-exception.files.home-relative-path.read-write"] {
            XCTAssertNil(entitlement(key), key)
        }
    }
}
