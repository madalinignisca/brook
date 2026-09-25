import AppKit
import Foundation

/// App quit during a call: `.terminateLater`, leave and wait for the engine to close (capture
/// provably stopped), then reply to AppKit exactly once, on completion or after the bound.
@MainActor
final class QuitCoordinator {
    /// Leaves the live call and waits for the engine to close; nil when no call is live.
    var leaveActiveCall: (() async -> Void)?
    private let timeout: Duration
    private let reply: @MainActor (Bool) -> Void
    private var pending = false

    init(
        timeout: Duration = .seconds(5),
        reply: @escaping @MainActor (Bool) -> Void = { NSApp.reply(toApplicationShouldTerminate: $0) }
    ) {
        self.timeout = timeout
        self.reply = reply
    }

    func shouldTerminate() -> NSApplication.TerminateReply {
        guard let leave = leaveActiveCall else { return .terminateNow }
        guard !pending else { return .terminateLater }  // a second ⌘Q while waiting
        pending = true
        var replied = false
        let finish: @MainActor () -> Void = { [reply] in
            guard !replied else { return }
            replied = true
            reply(true)
        }
        Task { @MainActor in
            await leave()
            finish()
        }
        let timeout = timeout
        Task { @MainActor in
            try? await Task.sleep(for: timeout)
            finish()
        }
        return .terminateLater
    }
}
