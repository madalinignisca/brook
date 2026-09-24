import BrookCore
import Foundation
import Observation

/// The app's single owner of the Rust core. Phase 0: sign in, show who is signed in.
///
/// The phase is driven only by `login`'s result: nothing else can change the session yet,
/// and the result carries the typed `LoginError` the message needs. An auth-state observer
/// arrives with session restore / logout, when state can change outside a call.
@MainActor
@Observable
final class SessionStore {
    enum Phase: Equatable {
        case signedOut(error: String?)
        case signingIn
        case signedIn(FfiUser)
    }

    enum Message {
        static let missingFields = "Enter your handle and password."
        static let invalidAddress = "That server address isn't valid."
        static let notJustAnAddress = "Enter just the server address, like https://chat.example.com"
        static let wrongCredentials = "Wrong handle or password."
        static let unreachable = "Couldn't reach the server. Check the address."
        static let unreachableLAN = "Couldn't reach the server. If macOS asked to allow local network access, allow it and try again."
        static let insecure = "The server address must start with https://"
        static let unexpected = "The server sent an unexpected response."
    }

    typealias ClientFactory = (_ server: String, _ allowInsecureHttp: Bool) throws -> FfiBrookClient

    private(set) var phase: Phase = .signedOut(error: nil)
    let settings: Settings
    private let makeClient: ClientFactory
    /// Kept while signed in: later phases talk to the server through it.
    private var client: FfiBrookClient?

    init(settings: Settings = Settings(), makeClient: @escaping ClientFactory = SessionStore.liveClient) {
        self.settings = settings
        self.makeClient = makeClient
    }

    nonisolated static func liveClient(server: String, allowInsecureHttp: Bool) throws -> FfiBrookClient {
        try FfiBrookClient(baseUrl: server, allowInsecureHttp: allowInsecureHttp)
    }

    /// The password is used exactly as typed: the server hashes it verbatim.
    /// Returns whether a login was actually attempted (false: rejected locally or ignored).
    @discardableResult
    func signIn(server: String, handle: String, password: String) async -> Bool {
        if case .signingIn = phase { return false }
        let handle = handle.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !handle.isEmpty, !password.isEmpty else {
            phase = .signedOut(error: Message.missingFields)
            return false
        }
        let address: String
        switch ServerAddress.parse(server) {
        case let .success(parsed): address = parsed
        case .failure(.invalid): phase = .signedOut(error: Message.invalidAddress); return false
        case .failure(.notJustAnAddress): phase = .signedOut(error: Message.notJustAnAddress); return false
        }

        phase = .signingIn
        do {
            let client = try makeClient(address, settings.allowInsecureHTTP)
            switch try await client.login(handle: handle, password: password) {
            case let .loggedIn(session):
                self.client = client
                settings.saveLastGoodServer(address)
                phase = .signedIn(session.user)
            }
        } catch let error as LoginError {
            phase = .signedOut(error: Self.message(for: error, address: address))
        } catch {
            phase = .signedOut(error: Message.unexpected)
        }
        return true
    }

    static func message(for error: LoginError, address: String) -> String {
        switch error {
        case let .Api(code, message):
            code == "auth.invalid_credentials" ? Message.wrongCredentials : message
        case .Network:
            isLocalNetwork(address) ? Message.unreachableLAN : Message.unreachable
        case .InsecureServerUrl: Message.insecure
        case .InvalidServerUrl: Message.invalidAddress
        case .UnexpectedResponse: Message.unexpected
        }
    }

    /// Private IPv4 ranges, link-local, and local-only names: where macOS's Local Network
    /// permission applies and the first connection can fail before the user answers.
    static func isLocalNetwork(_ address: String) -> Bool {
        guard let host = URLComponents(string: address)?.host?.lowercased() else { return false }
        if host.hasSuffix(".local") || host.hasSuffix(".lan") || host.hasSuffix(".home.arpa") { return true }
        let octets = host.split(separator: ".").compactMap { UInt8($0) }
        guard octets.count == 4 else { return host.hasPrefix("fe80:") || host.hasPrefix("fd") }
        switch (octets[0], octets[1]) {
        case (10, _), (192, 168), (169, 254): return true
        case (172, 16 ... 31): return true
        default: return false
        }
    }
}
