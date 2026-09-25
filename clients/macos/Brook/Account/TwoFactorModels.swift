import AppKit
import BrookCore
import CoreImage.CIFilterBuiltins
import Foundation
import Observation

/// Turning two-factor sign-in on: password → scan the QR code → first code → recovery codes.
/// The password is dropped once enrolment starts, the secret (the URI) once activation succeeds,
/// and everything when the sheet closes.
@MainActor
@Observable
final class TwoFactorSetupModel {
    enum Step: Equatable {
        case password
        case scan
        case codes([String])
        case done
    }

    var password = ""
    var code = ""
    /// The user confirms the recovery codes are saved before the sheet can finish.
    var savedCodes = false
    private(set) var step: Step = .password
    /// The enrolment's URI (QR code and manual key). It carries the secret: kept only while
    /// the scan step is showing.
    private(set) var uri: String?
    private(set) var busy = false
    private(set) var error: String?
    /// What to tell the user once it's on.
    private(set) var note: String?

    static let wrongPassword = "The password is wrong."
    static let enrollmentExpired = "That setup expired. Start again to get a new QR code."
    static let alreadyOn = "Two-factor sign-in is already on."
    static let othersSignedOut = "Two-factor sign-in is on. Your other devices are signed out."

    private let client: any AccountClient
    /// Bumped when the sheet closes: an answer that arrives later changes nothing.
    @ObservationIgnored private var epoch = 0

    init(client: any AccountClient) {
        self.client = client
    }

    /// The manual-entry key: the URI's secret in groups of four.
    static func key(from uri: String) -> String? {
        guard let components = URLComponents(string: uri), components.scheme == "otpauth",
              let secret = components.queryItems?.first(where: { $0.name == "secret" })?.value,
              !secret.isEmpty
        else { return nil }
        return stride(from: 0, to: secret.count, by: 4).map { start -> String in
            let from = secret.index(secret.startIndex, offsetBy: start)
            let to = secret.index(from, offsetBy: min(4, secret.count - start))
            return String(secret[from ..< to])
        }.joined(separator: " ")
    }

    var key: String? { uri.flatMap(Self.key(from:)) }

    func start() async {
        guard step == .password, !password.isEmpty, !busy else { return }
        busy = true
        error = nil
        let mine = epoch
        defer { if mine == epoch { busy = false } }
        do {
            let enrollment = try await client.totpEnroll(password: password)
            guard mine == epoch else { return } // closed meanwhile
            password = ""
            uri = enrollment.otpauthUri()
            step = .scan
        } catch {
            guard mine == epoch else { return }
            self.error = message(for: error)
        }
    }

    func activate() async {
        guard step == .scan, !busy else { return }
        let digits = code.filter { !$0.isWhitespace }
        code = ""
        guard digits.count == 6, digits.allSatisfy({ $0.isASCII && $0.isNumber }) else {
            error = SessionStore.Message.codeFormat
            return
        }
        busy = true
        error = nil
        let mine = epoch
        defer { if mine == epoch { busy = false } }
        do {
            let codes = try await client.totpActivate(code: digits)
            guard mine == epoch else { return } // closed meanwhile: the codes aren't kept
            uri = nil
            step = .codes(codes)
        } catch let LoginError.Api(code, _) where code == "auth.totp_enrollment_expired" {
            guard mine == epoch else { return }
            uri = nil
            step = .password
            error = Self.enrollmentExpired
        } catch {
            guard mine == epoch else { return }
            self.error = message(for: error)
        }
    }

    /// Only after the user confirms the recovery codes are saved.
    func finish() {
        guard case .codes = step, savedCodes else { return }
        step = .done
        note = Self.othersSignedOut
    }

    /// The sheet closed: nothing secret is kept, and nothing in flight comes back.
    func clear() {
        epoch += 1
        busy = false
        password = ""
        code = ""
        uri = nil
        savedCodes = false
        error = nil
        step = .password
    }

    private func message(for error: Error) -> String {
        if case let LoginError.Api(code, _)? = error as? LoginError {
            switch code {
            case "auth.invalid_code": return SessionStore.Message.wrongCode
            case "conflict": return Self.alreadyOn
            default: break
            }
        }
        return AccountMessage.text(for: error, wrongPassword: Self.wrongPassword)
    }
}

/// Turning two-factor sign-in off, or replacing the recovery codes: the password and a second
/// factor (a current code, or a recovery code).
@MainActor
@Observable
final class SecondFactorModel {
    enum Action { case turnOff, newCodes }

    var password = ""
    var code = ""
    var useRecovery = false
    private(set) var busy = false
    private(set) var error: String?
    private(set) var done = false
    /// For `.newCodes`: the fresh set, shown once.
    private(set) var newCodes: [String]?
    let action: Action
    private let client: any AccountClient
    @ObservationIgnored private var epoch = 0

    init(client: any AccountClient, action: Action) {
        self.client = client
        self.action = action
    }

    var problem: String? {
        if password.isEmpty { return "Enter your password." }
        if code.trimmingCharacters(in: .whitespaces).isEmpty {
            return useRecovery ? SessionStore.Message.recoveryFormat : SessionStore.Message.codeFormat
        }
        return nil
    }

    func submit() async {
        guard problem == nil, !busy else { return }
        let recovery = useRecovery // the wording follows what was sent
        let mine = epoch
        let factor: FfiSecondFactor = recovery
            ? .recovery(code: code.trimmingCharacters(in: .whitespacesAndNewlines))
            : .code(code: code.filter { !$0.isWhitespace })
        busy = true
        error = nil
        defer { if mine == epoch { busy = false } }
        do {
            var fresh: [String]?
            switch action {
            case .turnOff: try await client.totpDisable(password: password, factor: factor)
            case .newCodes: fresh = try await client.totpRegenerateRecoveryCodes(password: password, factor: factor)
            }
            guard mine == epoch else { return } // closed meanwhile: the codes aren't kept
            newCodes = fresh
            clear()
            done = true
        } catch let LoginError.Api(code, _) where code == "auth.invalid_code" {
            guard mine == epoch else { return }
            self.code = ""
            error = recovery ? SessionStore.Message.wrongRecoveryCode : SessionStore.Message.wrongCode
        } catch {
            guard mine == epoch else { return }
            self.error = AccountMessage.text(for: error, wrongPassword: TwoFactorSetupModel.wrongPassword)
        }
    }

    func clear() {
        password = ""
        code = ""
    }

    /// The sheet closed: the new codes go too, and nothing in flight comes back.
    func dismissed() {
        epoch += 1
        busy = false
        clear()
        newCodes = nil
        error = nil
    }
}

/// Admin: turn off another member's two-factor sign-in (their authenticator is lost).
@MainActor
@Observable
final class AdminTotpResetModel {
    var selectedId: String?
    var adminPassword = ""
    private(set) var users: [FfiUserSummary] = []
    private(set) var busy = false
    private(set) var error: String?
    private(set) var done: String?
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
            self.error = AccountMessage.text(for: error, wrongPassword: AdminResetModel.wrongAdmin)
        }
    }

    var problem: String? {
        if selectedId == nil { return "Choose a user." }
        if adminPassword.isEmpty { return "Enter your own password." }
        return nil
    }

    func submit() async {
        guard problem == nil, !busy, let id = selectedId else { return }
        busy = true
        error = nil
        defer { busy = false }
        do {
            try await client.adminResetTotp(userId: id, adminPassword: adminPassword)
            let who = users.first { $0.id == id }?.displayName ?? "The user"
            adminPassword = ""
            done = "\(who)'s two-factor sign-in is off. They're signed out everywhere."
        } catch {
            self.error = AccountMessage.text(
                for: error, wrongPassword: AdminResetModel.wrongAdmin, forbidden: AdminResetModel.adminTarget)
        }
    }

    func clear() { adminPassword = "" }
}

/// A QR code rendered on the device (the secret never goes to an image service).
enum QRCode {
    static func image(for text: String, size: CGFloat) -> NSImage? {
        let filter = CIFilter.qrCodeGenerator()
        filter.message = Data(text.utf8)
        filter.correctionLevel = "M"
        guard let output = filter.outputImage, output.extent.width > 0 else { return nil }
        let scale = size / output.extent.width
        let scaled = output.transformed(by: CGAffineTransform(scaleX: scale, y: scale))
        let rep = NSCIImageRep(ciImage: scaled)
        let image = NSImage(size: rep.size)
        image.addRepresentation(rep)
        return image
    }
}
