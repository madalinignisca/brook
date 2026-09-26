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
