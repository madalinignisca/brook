import Synchronization

@testable import BrookCore

/// Thread-safe record of observed states; a class so escaping `@Sendable` closures can
/// share it (a `Mutex` is noncopyable and cannot be captured directly).
final class StateLog: Sendable {
    private let states = Mutex<[FfiAuthState]>([])

    func append(_ state: FfiAuthState) { states.withLock { $0.append(state) } }
    var all: [FfiAuthState] { states.withLock { $0 } }
}
