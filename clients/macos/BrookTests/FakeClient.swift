import BrookCore
import Foundation
import Synchronization

/// Test double for the Rust-backed client, via UniFFI's `init(noHandle:)` hook. Both methods
/// the app can reach are overridden: inherited ones would call Rust with handle 0.
final class FakeClient: FfiBrookClient, @unchecked Sendable {
    struct Call: Equatable {
        let handle: String
        let password: String
    }

    private struct State {
        var calls: [Call] = []
        var gate: [CheckedContinuation<Void, Never>] = []
        var gateOpen: Bool
        var listener: AuthStateListener?
        var logouts = 0
        var coreState: FfiAuthState = .loggedIn(user: alice)
        var totpCalls: [String] = []
        var totpResult: Result<UInt32?, LoginError> = .success(nil)
        var cancels = 0
        var persistence: [String] = []
        var restoreOutcome: FfiRestoreOutcome = .notSignedIn
        var restores = 0
        var signOutComplete = true
        var logoutGated = false
        var logoutWaiters: [CheckedContinuation<Void, Never>] = []
    }

    private let result: Result<LoginResult, LoginError>
    private let state: Mutex<State>

    /// `gated`: `login` and `restore` suspend until `release()` — lets a test observe the in-flight state.
    init(result: Result<LoginResult, LoginError>, gated: Bool = false) {
        self.result = result
        state = Mutex(State(gateOpen: !gated))
        super.init(noHandle: NoHandle())
    }

    /// Only Rust hands out handles; a fake never comes from Rust.
    required init(unsafeFromHandle _: UInt64) {
        fatalError("FakeClient is never lifted from Rust")
    }

    var calls: [Call] { state.withLock { $0.calls } }

    func release() {
        let waiting = state.withLock { s -> [CheckedContinuation<Void, Never>] in
            s.gateOpen = true
            defer { s.gate = [] }
            return s.gate
        }
        waiting.forEach { $0.resume() }
    }

    override func login(handle: String, password: String) async throws -> LoginResult {
        let mustWait = state.withLock { s -> Bool in
            s.calls.append(Call(handle: handle, password: password))
            return !s.gateOpen
        }
        if mustWait { await waitForGate() }
        return try result.get()
    }

    /// Suspends (outside the lock) until `release()` when the fake is gated.
    private func waitForGate() async {
        await withCheckedContinuation { cont in
            let openAlready = state.withLock { s -> Bool in
                if s.gateOpen { return true }
                s.gate.append(cont)
                return false
            }
            if openAlready { cont.resume() }
        }
    }

    /// Keeps the listener so a test can deliver core's auth states in any order.
    override func subscribe(listener: AuthStateListener) -> Subscription {
        state.withLock { $0.listener = listener }
        return FakeSubscription()
    }

    /// Deliver an auth state as core would (on a background thread in production).
    func emit(_ auth: FfiAuthState) {
        let listener = state.withLock { $0.listener }
        listener?.onState(state: auth)
    }

    var logouts: Int { state.withLock { $0.logouts } }

    /// What core's state is right now (`authState()`), independent of what was delivered: the
    /// real subscription keeps only the latest value and can skip states.
    func setCoreState(_ auth: FfiAuthState) { state.withLock { $0.coreState = auth } }

    override func authState() -> FfiAuthState { state.withLock { $0.coreState } }

    // MARK: TOTP second step

    var totpCalls: [String] { state.withLock { $0.totpCalls } }
    var cancels: Int { state.withLock { $0.cancels } }
    func setTotpResult(_ r: Result<UInt32?, LoginError>) { state.withLock { $0.totpResult = r } }

    override func completeTotp(challenge _: FfiTotpChallenge, code: String) async throws -> UInt32? {
        try state.withLock { s in
            s.totpCalls.append("code:\(code)")
            if case .success = s.totpResult { s.coreState = .loggedIn(user: alice) }
            return try s.totpResult.get()
        }
    }

    override func completeRecovery(challenge _: FfiTotpChallenge, recoveryCode: String) async throws -> UInt32? {
        try state.withLock { s in
            s.totpCalls.append("recovery:\(recoveryCode)")
            if case .success = s.totpResult { s.coreState = .loggedIn(user: alice) }
            return try s.totpResult.get()
        }
    }

    override func cancelTotp(challenge _: FfiTotpChallenge) async {
        state.withLock { $0.cancels += 1 }
    }

    override func logout() async {
        await withCheckedContinuation { cont in
            let go = state.withLock { s -> Bool in
                s.logouts += 1
                if s.logoutGated { s.logoutWaiters.append(cont); return false }
                return true
            }
            if go { cont.resume() }
        }
    }

    /// `logout` suspends until `releaseLogouts()` (a sign-out whose result comes late).
    func gateLogouts() { state.withLock { $0.logoutGated = true } }
    func releaseLogouts() {
        let waiting = state.withLock { s -> [CheckedContinuation<Void, Never>] in
            s.logoutGated = false
            defer { s.logoutWaiters = [] }
            return s.logoutWaiters
        }
        waiting.forEach { $0.resume() }
    }

    // MARK: Staying signed in

    /// The data directories persistence was enabled with, in order.
    var persistence: [String] { state.withLock { $0.persistence } }
    var restores: Int { state.withLock { $0.restores } }
    func setRestore(_ outcome: FfiRestoreOutcome) { state.withLock { $0.restoreOutcome = outcome } }
    func setSignOutComplete(_ complete: Bool) { state.withLock { $0.signOutComplete = complete } }

    override func enablePersistence(slot _: FfiKeySlot, dataDir: String) {
        state.withLock { $0.persistence.append(dataDir) }
    }

    /// Persistence must already be on (core restores nothing otherwise).
    override func restore() async -> FfiRestoreOutcome {
        await waitForGate()
        return state.withLock { s in
            s.restores += 1
            guard !s.persistence.isEmpty else { return .notSignedIn }
            if case let .loggedIn(user) = s.restoreOutcome { s.coreState = .loggedIn(user: user) }
            return s.restoreOutcome
        }
    }

    override func signOutComplete() -> Bool { state.withLock { $0.signOutComplete } }

    // MARK: Local data (#62): none of it reaches Rust; each call is recorded in order.

    let localCalls = Mutex<[String]>([])
    let enableResult = Mutex(true)
    let enableGate = Gate()
    let enableGated = Mutex(false)
    let forgetGate = Gate()
    let forgetGated = Mutex(false)
    let forgetFails = Mutex(false)
    let lost = Mutex<UInt64?>(nil)
    private func note(_ call: String) { localCalls.withLock { $0.append(call) } }

    override func subscribeCacheEvents(listener _: CacheEventListener) -> Subscription {
        note("subscribeCacheEvents")
        return FakeSubscription()
    }
    override func subscribeCacheState(listener _: CacheStateListener) -> Subscription {
        note("subscribeCacheState")
        return FakeSubscription()
    }
    override func enableLocalData(slot _: FfiKeySlot, dataDir _: String) async -> Bool {
        note("enable")
        if enableGated.withLock({ $0 }) { await enableGate.wait() }
        note("enabled")
        return enableResult.withLock { $0 }
    }
    override func outboxLost() -> UInt64? {
        note("outboxLost")
        return lost.withLock { $0 }
    }
    override func acknowledgeOutboxLost(n: UInt64) { note("ack:\(n)") }
    override func signOutAndForget() async throws {
        note("forget")
        if forgetGated.withLock({ $0 }) { await forgetGate.wait() }
        note("forgot")
        if forgetFails.withLock({ $0 }) { throw LoginError.Api(code: "local.store", message: "") }
    }
    override func cachedChannels() async throws -> [FfiCachedChannel] { [] }
    override func unsentCount() async -> UInt64 { 0 }
    override func otherLocalUsers() async throws -> [FfiLocalUser] { [] }
    override func wipeOtherLocalUsers() async throws { note("wipe") }
    let closeGate = Gate()
    let closeGated = Mutex(false)
    override func closeLocalData() async {
        note("close")
        if closeGated.withLock({ $0 }) { await closeGate.wait() }
        note("closed")
    }
}

/// Records what the store asked the factory for, and hands out a prepared client.
final class FactoryRecorder: @unchecked Sendable {
    struct Request: Equatable {
        let server: String
        let allowInsecureHttp: Bool
    }

    private let requests = Mutex<[Request]>([])
    private let make: @Sendable () throws -> FfiBrookClient

    init(_ make: @escaping @Sendable () throws -> FfiBrookClient) { self.make = make }

    var all: [Request] { requests.withLock { $0 } }

    func factory(server: String, allowInsecureHttp: Bool) throws -> FfiBrookClient {
        requests.withLock { $0.append(Request(server: server, allowInsecureHttp: allowInsecureHttp)) }
        return try make()
    }
}

let alice = FfiUser(id: "u1", handle: "alice", displayName: "Alice", globalRole: "admin", statusText: nil)
let aliceSession = FfiSession(accessToken: "a", refreshToken: "r", user: alice)

/// A challenge with no Rust side (the store only passes it back to the client).
final class FakeChallenge: FfiTotpChallenge, @unchecked Sendable {
    init() { super.init(noHandle: NoHandle()) }
    required init(unsafeFromHandle _: UInt64) { fatalError("never lifted from Rust") }
    override func secondsLeft() -> UInt64 { 300 }
}

/// A key slot that is never called (the store only hands it to the client).
final class UnusedSlot: FfiKeySlot {
    func load(slot _: String) throws -> Data? { fatalError("unused") }
    func create(slot _: String, bytes _: Data) throws { fatalError("unused") }
    func replace(slot _: String, bytes _: Data) throws { fatalError("unused") }
    func delete(slot _: String) throws { fatalError("unused") }
}
