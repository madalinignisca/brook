import BrookCore
import Foundation
import Synchronization
import XCTest

@testable import Brook

final class FakeAccount: AccountClient, @unchecked Sendable {
    let calls = Mutex<[String]>([])
    var failure: LoginError?
    var users: [FfiUserSummary] = []
    /// What the server says it did about other devices; by default, what was asked.
    var outcome: Bool?? = .none

    func changePassword(current: String, new: String, signOutOtherDevices: Bool) async throws -> Bool? {
        calls.withLock { $0.append("change:\(current)>\(new):\(signOutOtherDevices ? "out" : "keep")") }
        if let failure { throw failure }
        return outcome ?? signOutOtherDevices
    }
    func adminResetPassword(userId: String, adminPassword: String, new: String) async throws {
        calls.withLock { $0.append("reset:\(userId):\(adminPassword)>\(new)") }
        if let failure { throw failure }
    }
    func listUsers() async throws -> [FfiUserSummary] { users }
}

func user(_ id: String, _ role: String = "member") -> FfiUserSummary {
    FfiUserSummary(id: id, handle: id, displayName: id.capitalized, globalRole: role)
}

@MainActor
final class ChangePasswordModelTests: XCTestCase {
    func testValidationBeforeSending() {
        let model = ChangePasswordModel(client: FakeAccount())
        XCTAssertNotNil(model.problem, "empty form sendable")
        model.current = "old-pass-1"
        model.new = "short"
        model.confirm = "short"
        XCTAssertEqual(model.problem, "The new password needs at least 8 characters.")
        model.new = "new-pass-2"
        model.confirm = "new-pass-3"
        XCTAssertEqual(model.problem, "The new passwords don't match.")
        model.new = "old-pass-1"
        model.confirm = "old-pass-1"
        XCTAssertEqual(model.problem, ChangePasswordModel.sameAsCurrent)
        model.new = String(repeating: "x", count: 257)
        model.confirm = model.new
        XCTAssertEqual(model.problem, "The new password can have at most 256 characters.")
        model.new = "new-pass-2"
        model.confirm = "new-pass-2"
        XCTAssertNil(model.problem)
    }

    /// The server hashes the bytes and counts code points; Swift's `==` and `count` see
    /// canonically equivalent text as equal and count grapheme clusters.
    func testPasswordsAreComparedAsTheServerSeesThem() {
        let composed = "abcdefgh\u{00E9}", decomposed = "abcdefgh\u{0065}\u{0301}"
        let model = ChangePasswordModel(client: FakeAccount())
        model.current = "old-pass-1"
        model.new = composed
        model.confirm = decomposed
        XCTAssertEqual(model.problem, "The new passwords don't match.", "different bytes confirmed")
        model.current = composed
        model.new = decomposed
        model.confirm = decomposed
        XCTAssertNil(model.problem, "different bytes called the same as the current one")
        model.current = "old-pass-1"
        model.new = String(repeating: "e\u{0301}", count: 4) // 8 code points, 4 characters
        model.confirm = model.new
        XCTAssertNil(model.problem, "a password the server accepts refused as too short")
        model.new = String(repeating: "\u{1F1F7}\u{1F1F4}", count: 129) // 258 code points
        model.confirm = model.new
        XCTAssertEqual(model.problem, "The new password can have at most 256 characters.")
    }

    func testSuccessSendsExactlyAndClearsTheFields() async {
        let account = FakeAccount()
        let model = ChangePasswordModel(client: account)
        model.current = "old-pass-1"
        model.new = "new-pass-2"
        model.confirm = "new-pass-2"
        await model.submit()
        XCTAssertEqual(account.calls.withLock { $0 }, ["change:old-pass-1>new-pass-2:out"])
        XCTAssertEqual(model.done, ChangePasswordModel.signedOthersOut)
        XCTAssertEqual([model.current, model.new, model.confirm], ["", "", ""], "passwords kept")
    }

    /// "Sign out of other devices" starts checked, is sent as set, says what happened, and is
    /// checked again when the sheet is next opened.
    func testSignOutOtherDevicesIsOnByDefaultAndSentAsSet() async {
        let account = FakeAccount()
        let model = ChangePasswordModel(client: account)
        XCTAssertTrue(model.signOutOtherDevices, "not checked by default")
        model.current = "old-pass-1"
        model.new = "new-pass-2"
        model.confirm = "new-pass-2"
        model.signOutOtherDevices = false
        await model.submit()
        XCTAssertEqual(account.calls.withLock { $0 }, ["change:old-pass-1>new-pass-2:keep"])
        XCTAssertEqual(model.done, ChangePasswordModel.keptOthersSignedIn)
        model.clear()
        XCTAssertTrue(model.signOutOtherDevices, "left unchecked for the next change")
        XCTAssertNil(model.done)
    }

    /// The confirmation says what the server did, not what the box asked: an older server
    /// (nil) is worded as its 15-minute behaviour, and a server's own answer wins over the box.
    func testTheConfirmationFollowsTheServersAnswer() async {
        let cases: [(box: Bool, answer: Bool?, text: String)] = [
            (true, nil, ChangePasswordModel.olderServer),
            (false, nil, ChangePasswordModel.olderServer),
            (true, false, ChangePasswordModel.keptOthersSignedIn),
            (false, true, ChangePasswordModel.signedOthersOut),
        ]
        for (box, answer, text) in cases {
            let account = FakeAccount()
            account.outcome = .some(answer)
            let model = ChangePasswordModel(client: account)
            model.current = "old-pass-1"
            model.new = "new-pass-2"
            model.confirm = "new-pass-2"
            model.signOutOtherDevices = box
            await model.submit()
            XCTAssertEqual(model.done, text, "box \(box), server answered \(String(describing: answer))")
        }
    }

    func testRefusalKeepsTheFieldsAndSaysWhy() async {
        let cases: [(LoginError, String)] = [
            (.Api(code: "auth.invalid_credentials", message: "x"), ChangePasswordModel.wrongCurrent),
            (.Api(code: "validation", message: "x"), AccountMessage.refused),
            (.Api(code: "auth.rate_limited", message: "x"), AccountMessage.tooManyAttempts),
            (.Network(message: "x"), AccountMessage.unreachable),
            (.NotAuthenticated, AccountMessage.signedOut),
        ]
        for (failure, message) in cases {
            let account = FakeAccount()
            account.failure = failure
            let model = ChangePasswordModel(client: account)
            model.current = "old-pass-1"
            model.new = "new-pass-2"
            model.confirm = "new-pass-2"
            await model.submit()
            XCTAssertEqual(model.error, message, "\(failure)")
            XCTAssertNil(model.done)
            XCTAssertEqual(model.current, "old-pass-1", "fields cleared after a refusal")
        }
    }

    func testNothingSentWhileInvalid() async {
        let account = FakeAccount()
        let model = ChangePasswordModel(client: account)
        model.current = "old-pass-1"
        model.new = "short"
        model.confirm = "short"
        await model.submit()
        XCTAssertEqual(account.calls.withLock { $0 }, [])
    }
}

@MainActor
final class AdminResetModelTests: XCTestCase {
    func testTheListLeavesOutAdminsAndOneself() async {
        let account = FakeAccount()
        account.users = [user("me", "admin"), user("other-admin", "admin"), user("bob"), user("carol")]
        let model = AdminResetModel(client: account, selfId: "me")
        await model.load()
        XCTAssertEqual(model.users.map(\.id), ["bob", "carol"])
    }

    func testResetSendsTheAdminPasswordAndClears() async {
        let account = FakeAccount()
        account.users = [user("bob")]
        let model = AdminResetModel(client: account, selfId: "me")
        await model.load()
        XCTAssertEqual(model.problem, "Choose a user.")
        model.selectedId = "bob"
        XCTAssertEqual(model.problem, "Enter your own password.")
        model.adminPassword = "admin-pw"
        model.new = "bobs-new-pass"
        model.confirm = "bobs-new-pass"
        XCTAssertNil(model.problem)
        await model.submit()
        XCTAssertEqual(account.calls.withLock { $0 }, ["reset:bob:admin-pw>bobs-new-pass"])
        XCTAssertNotNil(model.done)
        XCTAssertEqual([model.adminPassword, model.new, model.confirm], ["", "", ""])
    }

    func testWrongAdminPasswordAndAdminTargetAreSaidPlainly() async {
        for (code, message) in [
            ("auth.invalid_credentials", AdminResetModel.wrongAdmin),
            ("authz.forbidden", AdminResetModel.adminTarget),
        ] {
            let account = FakeAccount()
            account.users = [user("bob")]
            account.failure = .Api(code: code, message: "x")
            let model = AdminResetModel(client: account, selfId: "me")
            await model.load()
            model.selectedId = "bob"
            model.adminPassword = "admin-pw"
            model.new = "bobs-new-pass"
            model.confirm = "bobs-new-pass"
            await model.submit()
            XCTAssertEqual(model.error, message)
            XCTAssertEqual(model.adminPassword, "admin-pw", "fields cleared after a refusal")
        }
    }
}
