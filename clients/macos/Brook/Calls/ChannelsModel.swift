import BrookCore
import Foundation
import Observation

/// The signed-in home: channels, which have a live call, and whether joining is possible yet.
@MainActor
@Observable
final class ChannelsModel {
    private(set) var channels: [FfiChannel] = []
    /// Set by the server's `Ready`: until then a join would hit `Disconnected`.
    private(set) var ready = false
    /// channel id → participants in its live call.
    private(set) var liveCalls: [String: UInt32] = [:]
    private(set) var error: String?
    /// The open conversation, which takes the message events.
    var timeline: TimelineModel?

    private let client: any FfiBrookClientProtocol
    private var events: Subscription?

    init(client: any FfiBrookClientProtocol) {
        self.client = client
    }

    /// Subscribe to events BEFORE starting realtime, so the first `Ready` (and any call already
    /// live) is never missed; then list channels.
    func start() async {
        guard events == nil else { return }
        events = client.subscribeEvents(listener: EventBridge(self))
        do {
            try await client.startRealtime()
            channels = try await client.listChannels().filter { !$0.archived }
        } catch {
            self.error = "Couldn't load channels."
        }
    }

    func stop() {
        events?.cancel()
        events = nil
    }

    func handle(_ event: FfiServerEvent) {
        switch event {
        case .ready:
            ready = true
        case let .channelCall(channelId, callId, count):
            liveCalls[channelId] = callId != nil && count > 0 ? count : nil
        case .messageNew, .messageUpdate, .messageDelete, .resync:
            timeline?.apply(event)  // the open conversation's
        }
    }

    func canJoin(_ channel: FfiChannel) -> Bool { ready }

    func title(_ channel: FfiChannel) -> String { channel.name ?? "Direct message" }

    /// "● Call · N" in the sidebar, or nil when no call is live.
    func badge(_ channel: FfiChannel) -> String? {
        liveCalls[channel.id].map { "● Call · \($0)" }
    }
}

/// Delivers core's events to the model on the main thread, in order (a Task per event would
/// not keep the order).
final class EventBridge: ServerEventListener, @unchecked Sendable {
    private weak var model: ChannelsModel?
    init(_ model: ChannelsModel) { self.model = model }

    func onEvent(event: FfiServerEvent) {
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { self?.model?.handle(event) }
        }
    }
}
