import BrookCore
import Foundation
import Observation

/// The account calls the password sheets need (the Rust client; a fake in tests).
protocol AccountClient: AnyObject, Sendable {
    func changePassword(current: String, new: String, signOutOtherDevices: Bool) async throws -> Bool?
    func adminResetPassword(userId: String, adminPassword: String, new: String) async throws
    func listUsers() async throws -> [FfiUserSummary]
    func me() async throws -> FfiMe
    func updateProfile(displayName: String?, statusText: String?) async throws -> FfiMe
    func totpEnroll(password: String) async throws -> FfiTotpEnrollment
    func totpActivate(code: String) async throws -> [String]
    func totpDisable(password: String, factor: FfiSecondFactor) async throws
    func totpRegenerateRecoveryCodes(password: String, factor: FfiSecondFactor) async throws -> [String]
    func adminResetTotp(userId: String, adminPassword: String) async throws
    func createUser(handle: String, displayName: String, password: String, adminPassword: String) async throws -> FfiUserSummary
}

extension FfiBrookClient: AccountClient {}

/// The server's password policy, mirrored only to answer early (the server decides).
enum PasswordPolicy {
    static let minLength = 8
    static let maxLength = 256

    /// Why `new`/`confirm` can't be sent, or nil.
    static func problem(new: String, confirm: String) -> String? {
        // Code points, as the server counts (`count` is characters: "é" typed as e + accent is
        // one character but two code points).
        let length = new.unicodeScalars.count
        if length < minLength { return "The new password needs at least \(minLength) characters." }
        if length > maxLength { return "The new password can have at most \(maxLength) characters." }
        if !same(new, confirm) { return "The new passwords don't match." }
        return nil
    }

    /// Byte equality, as the server's hash sees it. Swift's `==` treats canonically equivalent
    /// text as equal, so a confirmation typed differently would pass here and not sign in.
    static func same(_ a: String, _ b: String) -> Bool {
        a.utf8.elementsEqual(b.utf8)
    }
}

/// Error text shared by both sheets. `wrongPassword` differs: the current password for a
/// change, the admin's own password for a reset.
enum AccountMessage {
    static let tooManyAttempts = "Too many attempts. Wait a few minutes and try again."
    static let unreachable = "Couldn't reach the server. Try again."
    static let signedOut = "You're signed out. Sign in again."
    static let refused = "The server refused the new password (8 to 256 characters)."
    static let unexpected = "Something went wrong. Try again."

    /// `noAnswer` is for calls that may have committed without a usable answer reaching us (the
    /// password calls): no answer, or a 200 whose body did not parse. There, "couldn't reach the
    /// server, try again" would be wrong.
    static func text(
        for error: Error, wrongPassword: String, forbidden: String = unexpected,
        noAnswer: String? = nil
    ) -> String {
        guard let error = error as? LoginError else { return unexpected }
        switch error {
        case let .Api(code, _):
            switch code {
            case "auth.invalid_credentials": return wrongPassword
            case "validation": return refused
            case "auth.rate_limited": return tooManyAttempts
            case "authz.forbidden": return forbidden
            case "not_found": return "That user no longer exists."
            default: return unexpected
            }
        case .Network, .Timeout, .Disconnected: return noAnswer ?? unreachable
        case .UnexpectedResponse: return noAnswer ?? unexpected
        case .NotAuthenticated: return signedOut
        default: return unexpected
        }
    }
}

/// Change Password sheet: validation, the request, and what the fields hold when.
@MainActor
@Observable
final class ChangePasswordModel {
    var current = ""
    var new = ""
    var confirm = ""
    /// "Sign out of other devices": on by default, and again each time the sheet opens.
    var signOutOtherDevices = true
    private(set) var busy = false
    private(set) var error: String?
    /// What happened, once it has (worded from the server's answer).
    private(set) var done: String?

    static let signedOthersOut = "Password changed. Your other devices are signed out."
    static let keptOthersSignedIn = "Password changed. Your other devices stay signed in."
    static let olderServer = "Password changed. Your other devices will be signed out within 15 minutes."
    static let wrongCurrent = "The current password is wrong."
    static let sameAsCurrent = "The new password is the same as the current one."
    static let noAnswer =
        "No clear answer came back, so the change may have gone through. If you're signed out, sign in with the new password."

    private let client: any AccountClient

    init(client: any AccountClient) {
        self.client = client
    }

    /// Why it can't be sent yet, or nil (shown as the user types; the button stays disabled).
    var problem: String? {
        if current.isEmpty { return "Enter your current password." }
        if PasswordPolicy.same(new, current), !new.isEmpty { return Self.sameAsCurrent }
        return PasswordPolicy.problem(new: new, confirm: confirm)
    }

    func submit() async {
        guard problem == nil, !busy else { return }
        busy = true
        error = nil
        defer { busy = false }
        do {
            // Worded from what the server says it did, not from the box: an older server ignores
            // the box and answers nil (it signs the others out when their access tokens expire).
            let signedOut = try await client.changePassword(
                current: current, new: new, signOutOtherDevices: signOutOtherDevices)
            clear()
            switch signedOut {
            case true?: done = Self.signedOthersOut
            case false?: done = Self.keptOthersSignedIn
            case nil: done = Self.olderServer
            }
        } catch {
            // Fields stay: a typo can be fixed without retyping everything.
            self.error = AccountMessage.text(
                for: error, wrongPassword: Self.wrongCurrent, noAnswer: Self.noAnswer)
        }
    }

    /// When the sheet closes (and after success): nothing keeps the passwords.
    func clear() {
        current = ""
        new = ""
        confirm = ""
        signOutOtherDevices = true
        done = nil
    }
}

/// Reset a User's Password sheet (admins): pick a member, re-enter your own password.
@MainActor
@Observable
final class AdminResetModel {
    private(set) var users: [FfiUserSummary] = []
    var selectedId: String?
    var adminPassword = ""
    var new = ""
    var confirm = ""
    private(set) var busy = false
    private(set) var error: String?
    private(set) var done: String?

    static let wrongAdmin = "Your own password is wrong."
    static let noAnswer = "No clear answer came back, so the reset may have gone through. Trying again is safe."
    static let adminTarget = "Admins change their own passwords."

    private let client: any AccountClient
    private let selfId: String

    init(client: any AccountClient, selfId: String) {
        self.client = client
        self.selfId = selfId
    }

    /// Everyone but admins (the server refuses admin targets) and the caller.
    func load() async {
        do {
            users = try await client.listUsers().filter { $0.id != selfId && $0.globalRole != "admin" }
        } catch {
            self.error = AccountMessage.text(for: error, wrongPassword: Self.wrongAdmin)
        }
    }

    var problem: String? {
        if selectedId == nil { return "Choose a user." }
        if adminPassword.isEmpty { return "Enter your own password." }
        return PasswordPolicy.problem(new: new, confirm: confirm)
    }

    func submit() async {
        guard problem == nil, !busy, let id = selectedId else { return }
        busy = true
        error = nil
        defer { busy = false }
        do {
            try await client.adminResetPassword(userId: id, adminPassword: adminPassword, new: new)
            let who = users.first { $0.id == id }?.displayName ?? "The user"
            clear()
            done = "\(who)'s password is set. They're signed out everywhere."
        } catch {
            self.error = AccountMessage.text(
                for: error, wrongPassword: Self.wrongAdmin, forbidden: Self.adminTarget,
                noAnswer: Self.noAnswer)
        }
    }

    func clear() {
        adminPassword = ""
        new = ""
        confirm = ""
    }
}

/// The server's limits for a new account's handle and display name, mirrored only to answer early.
enum NewUserPolicy {
    static let handleRange = 2 ... 64
    static let nameRange = 1 ... 64

    /// The handle as it is sent (trimmed, no leading `@`; case kept) and the name as sent (trimmed).
    static func sent(handle: String, displayName: String) -> (handle: String, displayName: String) {
        (Handle.clean(handle), displayName.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    /// Why this handle/name can't be sent, or nil.
    static func problem(handle: String, displayName: String) -> String? {
        let (handle, name) = sent(handle: handle, displayName: displayName)
        // ASCII only, spelled out: `\w` and character classes would admit other scripts.
        let allowed = handle.unicodeScalars.allSatisfy {
            ("A" ... "Z").contains($0) || ("a" ... "z").contains($0) || ("0" ... "9").contains($0)
                || $0 == "_" || $0 == "." || $0 == "-"
        }
        if !handleRange.contains(handle.unicodeScalars.count) || !allowed {
            return "A handle is 2 to 64 letters, digits, dots, dashes or underscores."
        }
        // Code points, as the server counts.
        if !nameRange.contains(name.unicodeScalars.count) {
            return "A display name is 1 to 64 characters."
        }
        return nil
    }
}

/// A random password a person can read out or retype: no 0 O 1 l I o.
enum PasswordGenerator {
    static let alphabet = Array("ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnpqrstuvwxyz23456789")
    static let length = 16

    /// `Int.random(in:using:)` draws uniformly (it rejects, it does not take a modulo), so no
    /// character is likelier than another.
    static func make<G: RandomNumberGenerator>(using generator: inout G) -> String {
        String((0 ..< length).map { _ in alphabet[Int.random(in: 0 ..< alphabet.count, using: &generator)] })
    }

    static func make() -> String {
        var system = SystemRandomNumberGenerator()
        return make(using: &system)
    }
}

/// Whether the Account menu offers Add User… (the server decides; this only hides the entry).
enum AddUserMenu {
    static func isVisible(globalRole: String) -> Bool { globalRole == "admin" }
}

/// Add User sheet (admins): a new member's handle, name and first password; the admin's own
/// password is asked again.
@MainActor
@Observable
final class AddUserModel {
    var handle = ""
    var displayName = ""
    /// Editing it after `generate()` hides the generated text and empties Confirm.
    var password = "" {
        didSet {
            if let shown = generated, !PasswordPolicy.same(shown, password) {
                generated = nil
                confirm = ""
            }
        }
    }
    var confirm = ""
    var adminPassword = ""
    /// The generated password, shown once while the Password field still holds it.
    private(set) var generated: String?
    private(set) var busy = false
    private(set) var error: String?
    private(set) var done: String?
    /// The handle of the latest try that got no usable answer: it may have been created.
    private var lastNoAnswerHandle: String?

    static let taken = "That handle is taken. Handles are case-sensitive, and a disabled account keeps its handle."
    static let probablyCreated =
        "Your previous try got no answer and probably created it, with the password you entered."
    static let notAllowed =
        "Not allowed: your account may no longer be an admin, or your sign-in expired. Try again."
    static let limits =
        "The server refused it: a handle is 2 to 64 letters, digits, dots, dashes or underscores; a name 1 to 64 characters; a password 8 to 256."
    static let noAnswer =
        "No clear answer came back, so the account may have been created. Try again; if it says the handle is taken, it was."

    private let client: any AccountClient
    private let generator: () -> String

    init(client: any AccountClient, generator: @escaping () -> String = { PasswordGenerator.make() }) {
        self.client = client
        self.generator = generator
    }

    /// Why it can't be sent yet, or nil (shown as the user types; the button stays disabled).
    var problem: String? {
        if let problem = NewUserPolicy.problem(handle: handle, displayName: displayName) { return problem }
        if let problem = PasswordPolicy.problem(new: password, confirm: confirm) { return problem }
        if adminPassword.isEmpty { return "Enter your own password." }
        return nil
    }

    func generate() {
        let fresh = generator()
        password = fresh // may hide an older generated text; set below
        confirm = fresh
        generated = fresh
    }

    func submit() async {
        guard problem == nil, !busy else { return }
        let (handle, name) = NewUserPolicy.sent(handle: handle, displayName: displayName)
        busy = true
        error = nil
        defer { busy = false }
        do {
            _ = try await client.createUser(
                handle: handle, displayName: name, password: password, adminPassword: adminPassword)
            clear()
            lastNoAnswerHandle = nil
            done = "\(handle) was added. Give them the password; they can change it under Change Password."
        } catch {
            // Fields stay: a typo can be fixed without retyping everything.
            self.error = text(for: error, handle: handle)
        }
    }

    private func text(for error: Error, handle: String) -> String {
        switch error as? LoginError {
        case let .Api(code, _)?:
            switch code {
            case "conflict":
                return lastNoAnswerHandle == handle ? Self.probablyCreated : Self.taken
            case "auth.invalid_credentials": return AdminResetModel.wrongAdmin
            case "authz.forbidden": return Self.notAllowed
            case "validation": return Self.limits
            case "auth.rate_limited": return AccountMessage.tooManyAttempts
            case "http_5xx":
                lastNoAnswerHandle = handle
                return Self.noAnswer
            default: return AccountMessage.unexpected
            }
        case .Network?, .Timeout?, .Disconnected?, .UnexpectedResponse?:
            lastNoAnswerHandle = handle
            return Self.noAnswer
        case .NotAuthenticated?: return AccountMessage.signedOut
        default: return AccountMessage.unexpected
        }
    }

    /// When the sheet closes (and after success): nothing keeps the passwords.
    func clear() {
        password = ""
        confirm = ""
        adminPassword = ""
        generated = nil
    }
}
