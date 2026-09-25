import BrookCore
import Foundation
import Observation
import Synchronization

/// The app's single owner of the Rust core: sign in, sign out, and follow core when it loses
/// the session by itself (a remote sign-out: a password change elsewhere, an admin reset).
///
/// Every sign-in is an *attempt* with its own id. Core's auth events for an attempt arrive on
/// one ordered stream, numbered as they arrive, consumed by one main-actor task. Core's
/// subscription keeps only the latest value (a quick `LoggedIn` then `LoggedOut` can arrive as
/// just `LoggedOut`), so the login's completion reads core's state directly and settles every
/// event numbered before it; any `LoggedOut` after that is a remote sign-out. Anything that
/// arrives for an attempt that is no longer current (a dropped client's late event, a stale
/// login result) is ignored, and the first transition out of signed-in wins.
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
        static let signedOut = "You're signed out. Sign in again."
    }

    typealias ClientFactory = (_ server: String, _ allowInsecureHttp: Bool) throws -> FfiBrookClient

    private(set) var phase: Phase = .signedOut(error: nil)
    let settings: Settings
    private let makeClient: ClientFactory
    /// Kept while signed in: later phases talk to the server through it.
    private(set) var client: FfiBrookClient?
    /// The current attempt; bumping it ends the previous one.
    private var attempt = 0
    private var observer: AuthStateObserver?
    private var events: Task<Void, Never>?
    /// Events numbered up to this were settled by the login's completion (it read core's state).
    @ObservationIgnored private var settled = Int.max
    @ObservationIgnored private var delivered: DeliveryCount?

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
        attempt += 1
        let mine = attempt
        do {
            let client = try makeClient(address, settings.allowInsecureHTTP)
            follow(client, attempt: mine)
            let result = try await client.login(handle: handle, password: password)
            guard mine == attempt else { return true } // ended meanwhile (a sign-out won)
            switch result {
            case let .loggedIn(session):
                // Whatever the (lossy) stream delivered so far, core's state now is the truth.
                settled = delivered?.value ?? 0
                if case .loggedOut = client.authState() {
                    end()
                    phase = .signedOut(error: Message.signedOut)
                    return true
                }
                self.client = client
                settings.saveLastGoodServer(address)
                phase = .signedIn(session.user)
            }
        } catch let error as LoginError {
            guard mine == attempt else { return true }
            end()
            phase = .signedOut(error: Self.message(for: error, address: address))
        } catch {
            guard mine == attempt else { return true }
            end()
            phase = .signedOut(error: Message.unexpected)
        }
        return true
    }

    /// Account → Sign Out. The attempt ends first, so a remote `LoggedOut` arriving after it
    /// changes nothing (the user just chose to sign out; no message needed).
    func signOut() {
        guard case .signedIn = phase, let client else { return }
        end()
        phase = .signedOut(error: nil)
        Task { await client.logout() } // core revokes the token; the client goes after
    }

    /// Core's auth events for `attempt`, in order, on the main actor.
    private func follow(_ client: FfiBrookClient, attempt mine: Int) {
        let (stream, sink) = AsyncStream.makeStream(of: (Int, FfiAuthState).self)
        let counter = DeliveryCount()
        delivered = counter
        settled = Int.max // nothing counts until the login completes
        observer = AuthStateObserver(client: client) { state in
            counter.next { sink.yield(($0, state)) } // numbered and yielded under one lock
        }
        events = Task { [weak self] in
            for await (n, state) in stream {
                guard let self, mine == self.attempt else { return }
                // Before or at the completion's settle point: already accounted for.
                guard n > self.settled, case .loggedOut = state else { continue }
                // Delivery order says nothing about when core published it (a fresh client's
                // initial LoggedOut can be delivered late): only core's state now decides.
                guard case .loggedOut = client.authState() else { continue }
                self.end()
                self.phase = .signedOut(error: Message.signedOut)
                return
            }
        }
    }

    /// End the current attempt: nothing that arrives for it applies any more.
    private func end() {
        attempt += 1
        observer?.cancel()
        observer = nil
        events?.cancel()
        events = nil
        delivered = nil
        settled = Int.max
        client = nil
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
        case .NotAuthenticated: Message.signedOut
        case .Disconnected, .Timeout: Message.unreachable
        case .CallEnded, .Busy, .TooLarge: Message.unexpected
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

/// Numbers auth events in delivery order; the number and the yield happen under one lock, so
/// the stream's order and the numbers agree.
final class DeliveryCount: Sendable {
    private let count = Mutex(0)
    var value: Int { count.withLock { $0 } }
    func next(_ deliver: (Int) -> Void) {
        count.withLock { n in
            n += 1
            deliver(n)
        }
    }
}
