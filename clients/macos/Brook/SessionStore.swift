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
        /// At launch: signing in with the stored session ("Signing in…", no form).
        case restoring
        case signingIn
        case signedIn(FfiUser)
        /// The password was right; the account's TOTP code (or a recovery code) comes next.
        case needsCode(error: String?)
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
        static let wrongCode = "Wrong or already-used code. Wait for the next one."
        static let wrongRecoveryCode = "That recovery code is wrong or already used. Try another one."
        static let codeStepExpired = "That took too long. Enter your password again."
        static let codeFormat = "Enter the 6-digit code from your authenticator app."
        static let recoveryFormat = "Enter one of your recovery codes."
        static let keychainUnavailable = "Your saved sign-in couldn't be read (the keychain may be locked). Sign in again."
        static let restoreOffline = "Couldn't reach the server to resume your session. It's kept for next time; you can also sign in again."
        static let signOutIncomplete = "This Mac couldn't forget your saved sign-in, so Brook may sign you in again at the next launch. Sign in and out again to retry."
        static let secondInstance = "Brook is already open. This window won't remember your sign-in."
        static let removalIncomplete = "Brook couldn't remove all of this Mac's data. Sign in and out again to retry."
        static let removalAndSignOutIncomplete = "Brook couldn't remove all of this Mac's data, and may sign you in again at the next launch. Sign in and out again to retry."
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

    private let persistence: SessionPersistence
    /// The launch restore runs at most once per process.
    @ObservationIgnored private var restoreStarted = false

    init(
        settings: Settings = Settings(), persistence: SessionPersistence = .off,
        makeClient: @escaping ClientFactory = SessionStore.liveClient,
        localDataWait: Duration = .seconds(30)
    ) {
        self.localDataWait = localDataWait
        self.settings = settings
        self.persistence = persistence
        self.makeClient = makeClient
        switch persistence {
        // Start on "Signing in…" rather than flash the form the restore may replace.
        case .on where settings.lastGoodServer != nil: phase = .restoring
        case .secondInstance: phase = .signedOut(error: Message.secondInstance)
        case .on, .off: break
        }
    }

    /// Every client this store makes keeps its session only when persistence is on.
    private func client(for address: String) throws -> FfiBrookClient {
        let client = try makeClient(address, settings.allowInsecureHTTP)
        if case let .on(slot, dataDir) = persistence { client.enablePersistence(slot: slot, dataDir: dataDir) }
        return client
    }

    /// At launch, with the last server: sign in with the stored session. It is an attempt
    /// like a sign-in, so a sign-out meanwhile wins and its late result is ignored.
    func restoreAtLaunch() async {
        guard phase == .restoring, !restoreStarted, let address = settings.lastGoodServer else { return }
        restoreStarted = true
        attempt += 1
        let mine = attempt
        let client: FfiBrookClient
        do { client = try self.client(for: address) } catch {
            end()
            phase = .signedOut(error: nil)
            return
        }
        follow(client, attempt: mine)
        let outcome = await client.restore()
        guard mine == attempt else { return }
        switch outcome {
        case let .loggedIn(user): finishSignIn(client, user: user, address: address)
        case .notSignedIn: end(); phase = .signedOut(error: nil)
        case .unavailable: end(); phase = .signedOut(error: Message.keychainUnavailable)
        case .offline: end(); phase = .signedOut(error: Message.restoreOffline)
        case .superseded: end(); phase = .signedOut(error: nil) // core's newer attempt isn't ours
        }
    }

    nonisolated static func liveClient(server: String, allowInsecureHttp: Bool) throws -> FfiBrookClient {
        try FfiBrookClient(baseUrl: server, allowInsecureHttp: allowInsecureHttp)
    }

    /// The password is used exactly as typed: the server hashes it verbatim.
    /// Returns whether a login was actually attempted (false: rejected locally or ignored).
    @discardableResult
    func signIn(server: String, handle: String, password: String) async -> Bool {
        if case .signingIn = phase { return false }
        if case .restoring = phase { return false }
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
            let client = try client(for: address)
            follow(client, attempt: mine)
            let result = try await client.login(handle: handle, password: password)
            guard mine == attempt else { return true } // ended meanwhile (a sign-out won)
            switch result {
            case let .loggedIn(session):
                finishSignIn(client, user: session.user, address: address)
            case let .totpRequired(challenge):
                // The password was right; nothing is signed in until the code step succeeds.
                pending = PendingCode(client: client, challenge: challenge, address: address, attempt: mine)
                phase = .needsCode(error: nil)
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

    /// Set when a sign-out couldn't make the stored session unusable; shown until a sign-in.
    private(set) var signOutWarning: String?

    // ---- This device's local data (#62) ----

    enum LocalData: Equatable {
        /// No persistence (no Keychain group yet, #79), or signed out: online only.
        case off
        /// Being switched on for the signed-in client (stores opening).
        case enabling
        case on
        /// `enableLocalData` answered false: online only.
        case failed
    }

    private(set) var localData: LocalData = .off
    /// The signed-in client's cache notices (nil without local data).
    private(set) var feed: CacheFeed?
    /// The last sign-out (it may still be erasing) and the last enable (it may still be
    /// opening stores): the next sign-in's enable waits for both, since the stores directory
    /// has no lock.
    @ObservationIgnored private var enableTask: Task<Void, Never>?
    /// Every sign-out and enable that hasn't finished yet, whoever started it: each new
    /// enable waits for all of them, not only the latest (a wait that timed out passes its
    /// unfinished tasks on).
    @ObservationIgnored private var unsettled: [UUID: Task<Void, Never>] = [:]

    /// Start `body` as a task that stays in `unsettled` until it ends.
    private func tracked(_ body: @escaping @MainActor () async -> Void) -> Task<Void, Never> {
        let id = UUID()
        let task = Task { [weak self] in
            await body()
            self?.unsettled[id] = nil
        }
        unsettled[id] = task // before the task can run: it's on this actor
        return task
    }
    /// The longest a sign-in waits for them; after that it stays online-only.
    private let localDataWait: Duration

    /// Sign Out offers "Remove this device's data" (switched on, or switching on).
    var offersRemoval: Bool { localData == .on || localData == .enabling }
    /// Completed sign-ins, counted: a sign-out's late result applies only if none came after.
    @ObservationIgnored private var signIns = 0

    /// After a sign-in with a recovery code: how many are left (the app warns when few).
    private(set) var recoveryCodesLeft: UInt32?
    /// A code is being checked (the button stays disabled).
    private(set) var codeBusy = false

    /// The code step: the client that proved the password, and its challenge.
    private struct PendingCode {
        let client: FfiBrookClient
        let challenge: FfiTotpChallenge
        let address: String
        let attempt: Int
    }

    @ObservationIgnored private var pending: PendingCode?

    /// Signed in: from here on core's state is the truth (the stream may skip states).
    private func finishSignIn(_ client: FfiBrookClient, user: FfiUser, address: String) {
        settled = delivered?.value ?? 0
        if case .loggedOut = client.authState() {
            end()
            phase = .signedOut(error: Message.signedOut)
            return
        }
        self.client = client
        signIns += 1
        signOutWarning = nil // the new sign-in replaced the stored copy
        settings.saveLastGoodServer(address)
        phase = .signedIn(user)
        if case let .on(slot, dataDir) = persistence { startLocalData(client, slot: slot, dataDir: dataDir) }
    }

    /// Local data for this signed-in client: after the previous sign-out and enable (bounded),
    /// subscribe, switch on, then read losses. Dropped if the attempt moved on meanwhile.
    private func startLocalData(_ client: FfiBrookClient, slot: FfiKeySlot, dataDir: String) {
        let mine = attempt
        let previous = Array(unsettled.values)
        let limit = localDataWait
        localData = .enabling
        enableTask = tracked { [weak self] in
            let settled = await Self.waitAll(previous, upTo: limit)
            // A sign-out or a newer sign-in meanwhile decides first: nothing to log.
            guard let self, mine == self.attempt, !Task.isCancelled else { return }
            guard settled else {
                Self.log("the previous sign-out hasn't finished: staying online only")
                self.localData = .off
                return
            }
            let feed = CacheFeed(client: client)
            feed.start() // before enabling: opening the stores can report a loss
            self.feed = feed
            let ok = await client.enableLocalData(slot: slot, dataDir: dataDir)
            guard mine == self.attempt, !Task.isCancelled else {
                feed.stop()
                if self.feed === feed { self.feed = nil }
                return
            }
            if ok {
                self.localData = .on
                feed.checkLost()
            } else {
                Self.log("this device's data couldn't be opened: online only")
                feed.stop()
                if self.feed === feed { self.feed = nil }
                self.localData = .failed
            }
        }
    }

    /// Whether every task ended within `limit`: resumes on whichever comes first, and never
    /// joins a task that doesn't end.
    nonisolated static func waitAll(_ tasks: [Task<Void, Never>], upTo limit: Duration) async -> Bool {
        if tasks.isEmpty { return true }
        return await withCheckedContinuation { (done: CheckedContinuation<Bool, Never>) in
            let once = ResumeOnce(done)
            Task {
                for task in tasks { await task.value }
                once.resume(true)
            }
            Task {
                try? await Task.sleep(for: limit)
                once.resume(false)
            }
        }
    }

    nonisolated static func log(_ message: String) {
        NSLog("Brook local data: %@", message)
    }

    /// The 6-digit code (spaces allowed, as pasted from "123 456").
    func submitCode(_ code: String) async {
        let digits = code.filter { !$0.isWhitespace }
        guard digits.count == 6, digits.allSatisfy({ $0.isASCII && $0.isNumber }) else {
            if case .needsCode = phase { phase = .needsCode(error: Message.codeFormat) }
            return
        }
        await complete(recovery: false) { try await $0.completeTotp(challenge: $1, code: digits) }
    }

    /// A recovery code instead of the 6-digit code.
    func submitRecovery(_ code: String) async {
        let trimmed = code.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else {
            if case .needsCode = phase { phase = .needsCode(error: Message.recoveryFormat) }
            return
        }
        await complete(recovery: true) { try await $0.completeRecovery(challenge: $1, recoveryCode: trimmed) }
    }

    /// Back to the password: this challenge ends (core refuses it from now on).
    func back() {
        guard case .needsCode = phase, let pending else { return }
        end()
        phase = .signedOut(error: nil)
        Task { await pending.client.cancelTotp(challenge: pending.challenge) }
    }

    private func complete(
        recovery: Bool, _ call: (FfiBrookClient, FfiTotpChallenge) async throws -> UInt32?
    ) async {
        guard case .needsCode = phase, let pending, !codeBusy else { return }
        codeBusy = true
        defer { codeBusy = false }
        do {
            let left = try await call(pending.client, pending.challenge)
            guard pending.attempt == attempt else { return }
            self.pending = nil
            recoveryCodesLeft = left
            guard case let .loggedIn(user) = pending.client.authState() else {
                end()
                phase = .signedOut(error: Message.signedOut)
                return
            }
            finishSignIn(pending.client, user: user, address: pending.address)
        } catch LoginError.ChallengeSuperseded {
            return // Back, a newer attempt or a sign-out already decided what shows
        } catch let LoginError.Api(code, _) where code == "auth.invalid_code" {
            guard pending.attempt == attempt else { return }
            phase = .needsCode(error: recovery ? Message.wrongRecoveryCode : Message.wrongCode)
        } catch let LoginError.Api(code, _) where code == "auth.totp_expired" {
            guard pending.attempt == attempt else { return }
            end()
            phase = .signedOut(error: Message.codeStepExpired)
        } catch let error as LoginError {
            guard pending.attempt == attempt else { return }
            phase = .needsCode(error: Self.message(for: error, address: pending.address))
        } catch {
            guard pending.attempt == attempt else { return }
            phase = .needsCode(error: Message.unexpected)
        }
    }

    /// Account → Sign Out. The attempt ends first, so a remote `LoggedOut` arriving after it
    /// changes nothing (the user just chose to sign out; no message needed).
    func signOut() { signOut(removeData: false) }

    /// Sign out, removing this device's data or keeping it (the sheet's choice). Removal waits
    /// for an enable still opening the stores, so what it opens goes too. The next sign-in's
    /// enable waits for this task.
    func signOut(removeData: Bool) {
        guard case .signedIn = phase, let client else { return }
        let enabling = enableTask
        end()
        phase = .signedOut(error: nil)
        let before = signIns
        _ = tracked { [weak self] in
            // An enable still opening the stores finishes first, whichever way this goes.
            await enabling?.value
            var removalFailed = false
            if removeData {
                do { try await client.signOutAndForget() } catch { removalFailed = true }
            } else {
                await client.logout() // core forgets the stored copy, then revokes (best effort)
            }
            // A sign-in completed since replaced the stored copy: then it's moot. Its own
            // value, not the form's error: typing into the form meanwhile must not hide it.
            guard let self, before == self.signIns else { return }
            // Both the keychain delete and the fence failed: the next launch could sign in
            // again. Checked whether or not the removal worked.
            let incomplete = !client.signOutComplete()
            switch (removalFailed, incomplete) {
            case (true, true): self.signOutWarning = Message.removalAndSignOutIncomplete
            case (true, false): self.signOutWarning = Message.removalIncomplete
            case (false, true): self.signOutWarning = Message.signOutIncomplete
            case (false, false): break
            }
        }
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
        pending = nil
        recoveryCodesLeft = nil
        client = nil
        enableTask?.cancel() // its handle stays: the next enable waits for it to finish
        feed?.stop()
        feed = nil
        localData = .off
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
        case .ChallengeSuperseded: Message.unexpected
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

/// A continuation resumed once, by whichever comes first.
final class ResumeOnce<T: Sendable>: @unchecked Sendable {
    private let lock = NSLock()
    private var continuation: CheckedContinuation<T, Never>?
    init(_ continuation: CheckedContinuation<T, Never>) { self.continuation = continuation }
    func resume(_ value: T) {
        let c = lock.withLock { () -> CheckedContinuation<T, Never>? in
            defer { continuation = nil }
            return continuation
        }
        c?.resume(returning: value)
    }
}
