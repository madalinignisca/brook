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
        var gate: CheckedContinuation<Void, Never>?
        var gateOpen: Bool
    }

    private let result: Result<LoginResult, LoginError>
    private let state: Mutex<State>

    /// `gated`: `login` suspends until `release()` — lets a test observe the in-flight state.
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
        let waiting = state.withLock { s -> CheckedContinuation<Void, Never>? in
            s.gateOpen = true
            defer { s.gate = nil }
            return s.gate
        }
        waiting?.resume()
    }

    override func login(handle: String, password: String) async throws -> LoginResult {
        let mustWait = state.withLock { s -> Bool in
            s.calls.append(Call(handle: handle, password: password))
            return !s.gateOpen
        }
        if mustWait {
            // Suspend outside the lock; `release()` resumes us.
            await withCheckedContinuation { cont in
                let openAlready = state.withLock { s -> Bool in
                    if s.gateOpen { return true }
                    s.gate = cont
                    return false
                }
                if openAlready { cont.resume() }
            }
        }
        return try result.get()
    }

    override func subscribe(listener _: AuthStateListener) -> Subscription {
        Subscription(noHandle: Subscription.NoHandle())
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

let alice = FfiUser(id: "u1", handle: "alice", displayName: "Alice", globalRole: "admin")
let aliceSession = FfiSession(accessToken: "a", refreshToken: "r", user: alice)
