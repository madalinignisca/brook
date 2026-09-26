import BrookCore
import Foundation
import Observation

/// Edit Profile: display name and status line (#183). Only what changed is sent.
@MainActor
@Observable
final class ProfileModel {
    /// The server's limits, counted as it counts them (code points, after trimming), so the
    /// sheet can say why before sending. The server still decides.
    static let nameLimit = 1...64
    static let statusLimit = 100
    static let refused = "That name or status can't be used. Avoid invisible or control characters."

    var name = ""
    var status = ""
    private(set) var loaded = false
    private(set) var busy = false
    private(set) var error: String?
    /// The profile the server answered with, once saved.
    private(set) var saved: FfiUser?

    private var loadedName = ""
    private var loadedStatus = ""
    private let client: any AccountClient

    init(client: any AccountClient) {
        self.client = client
    }

    func load() async {
        do {
            let me = try await client.me()
            set(me.user)
            loaded = true
        } catch {
            self.error = "Couldn't load your profile."
        }
    }

    private func set(_ user: FfiUser) {
        loadedName = user.displayName
        loadedStatus = user.statusText ?? ""
        name = loadedName
        status = loadedStatus
    }

    private static func trimmed(_ s: String) -> String { s.trimmingCharacters(in: .whitespacesAndNewlines) }

    /// What to send: nil for a field that's unchanged. A cleared status is "".
    var changes: (name: String?, status: String?) {
        let n = Self.trimmed(name), s = Self.trimmed(status)
        return (n == loadedName ? nil : n, s == loadedStatus ? nil : s)
    }

    var problem: String? {
        let n = Self.trimmed(name).unicodeScalars.count
        if n < Self.nameLimit.lowerBound { return "Enter a name." }
        if n > Self.nameLimit.upperBound { return "A name can be up to \(Self.nameLimit.upperBound) characters." }
        if Self.trimmed(status).unicodeScalars.count > Self.statusLimit {
            return "A status can be up to \(Self.statusLimit) characters."
        }
        return nil
    }

    var canSave: Bool {
        loaded && !busy && problem == nil && (changes.name != nil || changes.status != nil)
    }

    func save() async {
        guard canSave else { return }
        let (n, s) = changes
        busy = true
        error = nil
        defer { busy = false }
        do {
            let me = try await client.updateProfile(displayName: n, statusText: s)
            set(me.user)
            saved = me.user
        } catch let LoginError.Api(code, _) where code == "profile.invalid" {
            error = Self.refused
        } catch {
            self.error = AccountMessage.text(for: error, wrongPassword: AccountMessage.unexpected)
        }
    }
}
