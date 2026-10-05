import BrookCore
import Foundation
import XCTest

@testable import Brook

/// A seeded source (SplitMix64), to drive the generator deterministically. A source that only
/// ever returns zeros would never be accepted by an unbiased draw, so this one is well mixed.
struct SeededRandom: RandomNumberGenerator {
    var state: UInt64
    mutating func next() -> UInt64 {
        state &+= 0x9E37_79B9_7F4A_7C15
        var z = state
        z = (z ^ (z >> 30)) &* 0xBF58_476D_1CE4_E5B9
        z = (z ^ (z >> 27)) &* 0x94D0_49BB_1331_11EB
        return z ^ (z >> 31)
    }
}

final class NewUserPolicyTests: XCTestCase {
    func testHandleBoundaries() {
        func problem(_ h: String) -> String? { NewUserPolicy.problem(handle: h, displayName: "Ann") }
        XCTAssertNotNil(problem(""))
        XCTAssertNotNil(problem("a"), "1 char accepted")
        XCTAssertNil(problem("ab"))
        XCTAssertNil(problem(String(repeating: "a", count: 64)))
        XCTAssertNotNil(problem(String(repeating: "a", count: 65)), "65 chars accepted")
        XCTAssertNotNil(problem("ann smith"))
        XCTAssertNotNil(problem("an\u{00E9}"), "non-ASCII accepted")
        XCTAssertNil(problem("  @Ann_1.x-y "), "cleaned handle, case as typed")
        XCTAssertNotNil(problem("a\nb"))
    }

    func testDisplayNameBoundariesInScalars() {
        func problem(_ n: String) -> String? { NewUserPolicy.problem(handle: "ann", displayName: n) }
        XCTAssertNotNil(problem(""))
        XCTAssertNotNil(problem("   "), "blank name accepted")
        XCTAssertNil(problem("A"))
        XCTAssertNil(problem(String(repeating: "a", count: 64)))
        XCTAssertNotNil(problem(String(repeating: "a", count: 65)))
        XCTAssertNil(problem("  " + String(repeating: "a", count: 64) + "  "), "limit counted before trimming")
        // 33 characters of two scalars each: 66 scalars, 33 characters.
        XCTAssertNotNil(problem(String(repeating: "e\u{0301}", count: 33)), "counted as characters")
    }
}

final class PasswordGeneratorTests: XCTestCase {
    func testAlphabetHasNoLookAlikes() {
        let chars = Array(PasswordGenerator.alphabet)
        XCTAssertEqual(chars.count, 56)
        XCTAssertEqual(Set(chars).count, 56, "duplicates bias the choice")
        for bad in "0O1lIo" { XCTAssertFalse(chars.contains(bad), "\(bad) is a look-alike") }
        XCTAssertTrue(chars.allSatisfy { $0.isASCII && ($0.isLetter || $0.isNumber) })
    }

    func testSixteenCharactersFromTheAlphabetAndTheSourceIsUsed() {
        var one = SeededRandom(state: 1), again = SeededRandom(state: 1), two = SeededRandom(state: 2)
        let a = PasswordGenerator.make(using: &one)
        XCTAssertEqual(a.count, 16)
        XCTAssertTrue(a.allSatisfy { PasswordGenerator.alphabet.contains($0) })
        XCTAssertEqual(a, PasswordGenerator.make(using: &again), "not a function of the source")
        XCTAssertNotEqual(a, PasswordGenerator.make(using: &two), "the injected source is ignored")
        XCTAssertEqual(PasswordGenerator.make().count, 16)
    }

    /// Every alphabet position can come out, evenly enough that a modulo of 56 over 2^64 values
    /// is not what is used on a raw word (no bias is visible only statistically, so this checks
    /// reach and spread, and that the draw is the stdlib's uniform one).
    func testEveryCharacterIsReachable() {
        var rng = SystemRandomNumberGenerator()
        var seen = Set<Character>()
        for _ in 0 ..< 200 { seen.formUnion(PasswordGenerator.make(using: &rng)) }
        XCTAssertEqual(seen.count, 56)
    }
}

@MainActor
final class AddUserModelTests: XCTestCase {
    private func filled(_ account: FakeAccount, generator: @escaping () -> String = { "unused" }) -> AddUserModel {
        let model = AddUserModel(client: account, generator: generator)
        model.handle = " @Ann.Lee "
        model.displayName = "  Ann Lee "
        model.password = "new-pass-2"
        model.confirm = "new-pass-2"
        model.adminPassword = "admin-pass-9"
        return model
    }

    func testProblemPerField() {
        let model = AddUserModel(client: FakeAccount(), generator: { "x" })
        XCTAssertNotNil(model.problem)
        model.handle = "ann"
        XCTAssertEqual(model.problem, NewUserPolicy.problem(handle: "ann", displayName: ""))
        model.displayName = "Ann"
        model.password = String(repeating: "p", count: 7)
        model.confirm = model.password
        XCTAssertEqual(model.problem, "The new password needs at least 8 characters.")
        model.password = String(repeating: "p", count: 8)
        model.confirm = model.password
        XCTAssertEqual(model.problem, "Enter your own password.")
        model.adminPassword = "x"
        XCTAssertNil(model.problem)
        model.password = String(repeating: "p", count: 256)
        model.confirm = model.password
        XCTAssertNil(model.problem)
        model.password = String(repeating: "p", count: 257)
        model.confirm = model.password
        XCTAssertEqual(model.problem, "The new password can have at most 256 characters.")
        model.password = "new-pass-2"
        model.confirm = "new-pass-3"
        XCTAssertEqual(model.problem, "The new passwords don't match.")
    }

    func testGenerateFillsBothAndMeetsThePolicy() {
        let model = AddUserModel(client: FakeAccount())
        model.generate()
        XCTAssertEqual(model.password.count, 16)
        XCTAssertEqual(model.confirm, model.password, "Confirm left empty")
        XCTAssertEqual(model.generated, model.password)
        XCTAssertNil(PasswordPolicy.problem(new: model.password, confirm: model.confirm))
    }

    func testEditingAfterGenerateHidesItAndEmptiesConfirm() {
        let model = AddUserModel(client: FakeAccount(), generator: { "Abcdefgh23456789" })
        model.generate()
        XCTAssertNotNil(model.generated)
        model.password = "Abcdefgh2345678"
        XCTAssertNil(model.generated, "generated text still shown after an edit")
        XCTAssertEqual(model.confirm, "")
        // Setting the same value again (the field redrawing) is not an edit.
        model.generate()
        model.password = "Abcdefgh23456789"
        XCTAssertNotNil(model.generated)
        XCTAssertEqual(model.confirm, "Abcdefgh23456789")
    }

    func testSuccessSendsExactlyShowsTheMessageAndClearsEverySecret() async {
        let account = FakeAccount()
        let model = filled(account, generator: { "Gen-erated-pass1" })
        model.generate()
        model.adminPassword = "admin-pass-9"
        await model.submit()
        XCTAssertEqual(account.calls.withLock { $0 }, ["add:Ann.Lee|Ann Lee|Gen-erated-pass1|admin-pass-9"])
        XCTAssertEqual(model.done, "Ann.Lee was added. Give them the password; they can change it under Change Password.")
        XCTAssertNil(model.error)
        XCTAssertEqual(model.password, "")
        XCTAssertEqual(model.confirm, "")
        XCTAssertEqual(model.adminPassword, "")
        XCTAssertNil(model.generated)
        XCTAssertFalse(model.busy)
    }

    func testThePasswordsAreNotSwapped() async {
        let account = FakeAccount()
        let model = filled(account)
        await model.submit()
        XCTAssertEqual(account.calls.withLock { $0 }, ["add:Ann.Lee|Ann Lee|new-pass-2|admin-pass-9"])
    }

    func testFailureTexts() async {
        func api(_ code: String) -> LoginError { .Api(code: code, message: "server words") }
        let cases: [(LoginError, String)] = [
            (api("conflict"), AddUserModel.taken),
            (api("auth.invalid_credentials"), AdminResetModel.wrongAdmin),
            (api("authz.forbidden"), AddUserModel.notAllowed),
            (api("validation"), AddUserModel.limits),
            (api("auth.rate_limited"), AccountMessage.tooManyAttempts),
            (.Network(message: "x"), AddUserModel.noAnswer),
            (.Timeout, AddUserModel.noAnswer),
            (.Disconnected, AddUserModel.noAnswer),
            (.UnexpectedResponse, AddUserModel.noAnswer),
            (api("http_5xx"), AddUserModel.noAnswer),
            (.NotAuthenticated, AccountMessage.signedOut),
            (api("something_else"), AccountMessage.unexpected),
            (.InsecureServerUrl, AccountMessage.unexpected),
        ]
        for (failure, text) in cases {
            let account = FakeAccount()
            account.failure = failure
            let model = filled(account)
            await model.submit()
            XCTAssertEqual(model.error, text, "\(failure)")
            XCTAssertNil(model.done)
            XCTAssertEqual(model.password, "new-pass-2", "fields must stay after a failure")
            XCTAssertEqual(model.adminPassword, "admin-pass-9")
            XCTAssertEqual(model.handle, " @Ann.Lee ")
            XCTAssertFalse(model.busy)
        }
    }

    func testTheTextsAreDistinctAndNeverCarryServerWords() {
        let texts = [AddUserModel.taken, AddUserModel.probablyCreated, AdminResetModel.wrongAdmin,
                     AddUserModel.notAllowed, AddUserModel.limits, AddUserModel.noAnswer]
        XCTAssertEqual(Set(texts).count, texts.count)
        XCTAssertNotEqual(AddUserModel.notAllowed, AdminResetModel.wrongAdmin)
    }

    func testConflictAfterANoAnswerTryOnTheSameHandleSaysItProbablyExists() async {
        let account = FakeAccount()
        account.createScript = [.Timeout, .Api(code: "conflict", message: "x")]
        let model = filled(account)
        await model.submit()
        XCTAssertEqual(model.error, AddUserModel.noAnswer)
        await model.submit()
        XCTAssertEqual(model.error, AddUserModel.probablyCreated)
    }

    func testAHttp5xxIsANoAnswerTryToo() async {
        let account = FakeAccount()
        account.createScript = [.Api(code: "http_5xx", message: "x"), .Api(code: "conflict", message: "x")]
        let model = filled(account)
        await model.submit()
        await model.submit()
        XCTAssertEqual(model.error, AddUserModel.probablyCreated)
    }

    func testConflictForAnotherHandleIsTheOrdinaryText() async {
        let account = FakeAccount()
        account.createScript = [.Timeout, .Api(code: "conflict", message: "x")]
        let model = filled(account)
        await model.submit()
        model.handle = "bob"
        await model.submit()
        XCTAssertEqual(model.error, AddUserModel.taken)
    }

    func testAnOrdinaryConflictWithoutAnEarlierTryIsTaken() async {
        let account = FakeAccount()
        account.failure = .Api(code: "conflict", message: "x")
        let model = filled(account)
        await model.submit()
        XCTAssertEqual(model.error, AddUserModel.taken)
    }

    func testASecondSubmitWhileOneIsInFlightIsANoOp() async {
        let account = FakeAccount()
        account.gate = Gate()
        let model = filled(account)
        let first = Task { await model.submit() }
        try? await Task.sleep(for: .milliseconds(50))
        XCTAssertTrue(model.busy)
        // Opens the gate soon, so a second call that wrongly goes through fails the count
        // below instead of hanging on the gate.
        let opener = Task { try? await Task.sleep(for: .milliseconds(200)); account.gate?.open() }
        await model.submit()
        await opener.value
        await first.value
        XCTAssertEqual(account.calls.withLock { $0.count }, 1)
        XCTAssertFalse(model.busy)
    }

    func testAnInvalidFormSendsNothing() async {
        let account = FakeAccount()
        let model = filled(account)
        model.adminPassword = ""
        await model.submit()
        XCTAssertTrue(account.calls.withLock { $0.isEmpty })
    }

    /// Without Generate nothing else empties Confirm, so clear() must do it itself.
    func testClearEmptiesTypedSecretsToo() {
        let model = filled(FakeAccount())
        model.clear()
        XCTAssertEqual(model.password, "")
        XCTAssertEqual(model.confirm, "")
        XCTAssertEqual(model.adminPassword, "")
    }

    func testClearEmptiesEverySecret() {
        let model = filled(FakeAccount(), generator: { "Gen-erated-pass1" })
        model.generate()
        model.clear()
        XCTAssertEqual(model.password, "")
        XCTAssertEqual(model.confirm, "")
        XCTAssertEqual(model.adminPassword, "")
        XCTAssertNil(model.generated)
    }
}

final class AddUserMenuTests: XCTestCase {
    func testOnlyAdminsSeeTheEntry() {
        XCTAssertTrue(AddUserMenu.isVisible(globalRole: "admin"))
        XCTAssertFalse(AddUserMenu.isVisible(globalRole: "member"))
        XCTAssertFalse(AddUserMenu.isVisible(globalRole: ""))
    }
}
