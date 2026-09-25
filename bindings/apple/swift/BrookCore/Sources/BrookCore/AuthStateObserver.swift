import BrookCoreGenerated
import Synchronization

/// Observes a client's authentication state; the base of the app's observable store.
///
/// Latest state wins (see the Rust `AuthStateListener` docs): intermediate states may be
/// skipped, order never regresses, the final state always arrives. `onChange` is called
/// on a Rust runtime thread, never the main thread.
///
/// Rust's `Subscription.cancel()` may still deliver one late callback. This type closes
/// that gap: after `cancel()` returns, `onChange` is never called again.
public final class AuthStateObserver: Sendable {
    private let forwarder: Forwarder
    private let subscription: Subscription

    /// `onChange` must return quickly and must not synchronously wait on the main thread:
    /// `cancel()` waits for an in-flight `onChange` to finish. Hop to the main actor with
    /// `Task { @MainActor in … }`, not `DispatchQueue.main.sync`.
    public init(client: FfiBrookClient, onChange: @escaping @Sendable (FfiAuthState) -> Void) {
        let forwarder = Forwarder(onChange)
        self.forwarder = forwarder
        // Rust holds the forwarder, not `self`, so there is no retain cycle and `deinit` runs.
        subscription = client.subscribe(listener: forwarder)
    }

    public func cancel() {
        forwarder.cancel() // first: from here on, nothing reaches `onChange`
        subscription.cancel()
    }

    deinit { cancel() }
}

/// The object Rust calls. Holds the handler and the cancelled flag under one lock, so a
/// callback either completes before `cancel()` returns or is dropped.
final class Forwarder: AuthStateListener, Sendable {
    private let state: Mutex<(@Sendable (FfiAuthState) -> Void)?>

    init(_ onChange: @escaping @Sendable (FfiAuthState) -> Void) {
        state = Mutex(onChange)
    }

    func onState(state newState: FfiAuthState) {
        state.withLock { handler in handler?(newState) }
    }

    func cancel() {
        state.withLock { handler in handler = nil }
    }
}
