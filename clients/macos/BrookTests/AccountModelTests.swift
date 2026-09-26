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

    // TOTP management: each call recorded; `failure` (when set) is thrown by all of them.
    var codes = (1 ... 10).map { String(format: "abcd-%04d", $0) }
    func me() async throws -> FfiMe {
        FfiMe(user: FfiUser(id: "me", handle: "me", displayName: "Me", globalRole: "member", statusText: nil),
              totpEnabled: false, recoveryCodesLeft: nil)
    }
    /// Tests only: the factor as sent (the type itself never renders its code).
    static func plain(_ factor: FfiSecondFactor) -> String {
        switch factor {
        case let .code(code): "code:\(code)"
        case let .recovery(code): "recovery:\(code)"
        }
    }
    var gate: Gate?
    func totpEnroll(password: String) async throws -> FfiTotpEnrollment {
        calls.withLock { $0.append("enroll:\(password)") }
        await gate?.wait()
        if let failure { throw failure }
        return FakeEnrollment()
    }
    func totpActivate(code: String) async throws -> [String] {
        calls.withLock { $0.append("activate:\(code)") }
        if let failure { throw failure }
        return codes
    }
    func totpDisable(password: String, factor: FfiSecondFactor) async throws {
        calls.withLock { $0.append("disable:\(password):\(Self.plain(factor))") }
        if let failure { throw failure }
    }
    func totpRegenerateRecoveryCodes(password: String, factor: FfiSecondFactor) async throws -> [String] {
        calls.withLock { $0.append("regenerate:\(password):\(Self.plain(factor))") }
        await gate?.wait()
        if let failure { throw failure }
        return codes
    }
    func adminResetTotp(userId: String, adminPassword: String) async throws {
        calls.withLock { $0.append("totp-reset:\(userId):\(adminPassword)") }
        if let failure { throw failure }
    }
}

final class FakeEnrollment: FfiTotpEnrollment, @unchecked Sendable {
    init() { super.init(noHandle: NoHandle()) }
    required init(unsafeFromHandle _: UInt64) { fatalError("never lifted from Rust") }
    override func otpauthUri() -> String {
        "otpauth://totp/Brook:me?secret=JBSWY3DPEHPK3PXPJBSWY3DP&issuer=Brook&algorithm=SHA1&digits=6&period=30"
    }
    override func expiresIn() -> UInt64 { 600 }
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
            // No answer: the server may have committed, so never a plain "try again".
            (.Network(message: "x"), ChangePasswordModel.noAnswer),
            (.Timeout, ChangePasswordModel.noAnswer),
            // A 200 whose body did not parse: the server committed.
            (.UnexpectedResponse, ChangePasswordModel.noAnswer),
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

    /// No answer to a reset: it may have gone through, and repeating it is harmless.
    func testAResetWithoutAnAnswerSaysItMayHaveGoneThrough() async {
        let account = FakeAccount()
        account.users = [user("bob")]
        let model = AdminResetModel(client: account, selfId: "me")
        await model.load()
        account.failure = .Timeout
        model.selectedId = "bob"
        model.adminPassword = "admin-pw"
        model.new = "bobs-new-pass"
        model.confirm = "bobs-new-pass"
        await model.submit()
        XCTAssertEqual(model.error, AdminResetModel.noAnswer)
    }
}

@MainActor
final class TwoFactorModelTests: XCTestCase {
    func testTheManualKeyIsTheSecretInGroupsOfFour() {
        XCTAssertEqual(
            TwoFactorSetupModel.key(from: FakeEnrollment().otpauthUri()),
            "JBSW Y3DP EHPK 3PXP JBSW Y3DP")
        XCTAssertNil(TwoFactorSetupModel.key(from: "https://example.com"))
    }

    func testTheQRCodeRendersLocally() {
        XCTAssertNotNil(QRCode.image(for: FakeEnrollment().otpauthUri(), size: 200))
    }

    /// Password → scan → code → recovery codes → done; the password and the secret are gone
    /// as soon as their step is over.
    func testSetupWalksTheStepsAndDropsSecretsAsItGoes() async {
        let account = FakeAccount()
        let model = TwoFactorSetupModel(client: account)
        model.password = "pw"
        await model.start()
        XCTAssertEqual(model.step, .scan)
        XCTAssertEqual(model.password, "", "password kept after enrolment")
        XCTAssertNotNil(model.uri)
        model.code = "123 456"
        await model.activate()
        XCTAssertEqual(account.calls.withLock { $0 }, ["enroll:pw", "activate:123456"])
        XCTAssertEqual(model.step, .codes(account.codes))
        XCTAssertNil(model.uri, "the secret kept after activation")
        model.finish()
        XCTAssertEqual(model.step, .codes(account.codes), "finished without confirming the codes were saved")
        model.savedCodes = true
        model.finish()
        XCTAssertEqual(model.step, .done)
        XCTAssertEqual(model.note, TwoFactorSetupModel.othersSignedOut)
    }

    func testSetupErrorsAreSaidPlainly() async {
        let account = FakeAccount()
        let model = TwoFactorSetupModel(client: account)
        account.failure = .Api(code: "auth.invalid_credentials", message: "x")
        model.password = "wrong"
        await model.start()
        XCTAssertEqual(model.error, TwoFactorSetupModel.wrongPassword)
        XCTAssertEqual(model.step, .password)

        account.failure = nil
        model.password = "pw"
        await model.start()
        account.failure = .Api(code: "auth.invalid_code", message: "x")
        model.code = "000000"
        await model.activate()
        XCTAssertEqual(model.error, SessionStore.Message.wrongCode)
        XCTAssertEqual(model.step, .scan)

        account.failure = .Api(code: "auth.totp_enrollment_expired", message: "x")
        model.code = "123456"
        await model.activate()
        XCTAssertEqual(model.error, TwoFactorSetupModel.enrollmentExpired)
        XCTAssertEqual(model.step, .password, "an expired enrolment must start over with a new QR code")
        XCTAssertNil(model.uri)
    }

    func testClosingTheSheetDropsEverything() async {
        let model = TwoFactorSetupModel(client: FakeAccount())
        model.password = "pw"
        await model.start()
        model.code = "12"
        model.clear()
        XCTAssertNil(model.uri)
        XCTAssertEqual([model.password, model.code], ["", ""])
        XCTAssertEqual(model.step, .password)
    }

    func testTurnOffAndNewCodesSendTheRightSecondFactor() async {
        let account = FakeAccount()
        let off = SecondFactorModel(client: account, action: .turnOff)
        off.password = "pw"
        off.code = "123 456"
        await off.submit()
        off.useRecovery = true
        off.password = "pw"
        off.code = " abcd-0001 "
        let fresh = SecondFactorModel(client: account, action: .newCodes)
        fresh.password = "pw"
        fresh.useRecovery = true
        fresh.code = "abcd-0002"
        await fresh.submit()
        XCTAssertEqual(account.calls.withLock { $0 }, [
            "disable:pw:code:123456",
            "regenerate:pw:recovery:abcd-0002",
        ])
        XCTAssertTrue(off.done)
        XCTAssertEqual(fresh.newCodes, account.codes)
        XCTAssertEqual([fresh.password, fresh.code], ["", ""], "secrets kept after success")
    }

    func testAdminTwoFactorResetSendsTheAdminPassword() async {
        let account = FakeAccount()
        account.users = [user("bob")]
        let model = AdminTotpResetModel(client: account, selfId: "me")
        await model.load()
        model.selectedId = "bob"
        model.adminPassword = "admin-pw"
        await model.submit()
        XCTAssertEqual(account.calls.withLock { $0 }, ["totp-reset:bob:admin-pw"])
        XCTAssertNotNil(model.done)
        XCTAssertEqual(model.adminPassword, "")
    }

    // MARK: review round 1

    func testClosingAfterNewCodesDropsThem() async {
        let model = SecondFactorModel(client: FakeAccount(), action: .newCodes)
        model.password = "pw"
        model.code = "123456"
        await model.submit()
        XCTAssertNotNil(model.newCodes)
        model.dismissed()
        XCTAssertNil(model.newCodes, "recovery codes kept after the sheet closed")
    }

    /// Closing the setup sheet while enrolment is in flight: the late answer doesn't bring the
    /// secret (or a step) back into the closed model.
    func testALateEnrolmentAfterClosingIsIgnored() async {
        let account = FakeAccount()
        account.gate = Gate()
        let model = TwoFactorSetupModel(client: account)
        model.password = "pw"
        let started = Task { await model.start() }
        try? await Task.sleep(for: .milliseconds(50))
        model.clear()
        account.gate?.open()
        await started.value
        XCTAssertNil(model.uri, "the secret came back after closing")
        XCTAssertEqual(model.step, .password)
    }

    /// The wording follows what was sent, not the toggle as it is when the answer arrives.
    func testTheRefusalIsWordedForWhatWasSent() async {
        let account = FakeAccount()
        account.gate = Gate()
        account.failure = .Api(code: "auth.invalid_code", message: "x")
        let model = SecondFactorModel(client: account, action: .newCodes)
        model.password = "pw"
        model.useRecovery = true
        model.code = "abcd-0001"
        let sent = Task { await model.submit() }
        try? await Task.sleep(for: .milliseconds(50))
        model.useRecovery = false // switched while the recovery code is being checked
        account.gate?.open()
        await sent.value
        XCTAssertEqual(model.error, SessionStore.Message.wrongRecoveryCode)
    }
}
