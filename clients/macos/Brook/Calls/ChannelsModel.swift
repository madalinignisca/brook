import BrookCore
import Foundation
import Observation

/// The signed-in home: channels, which have a live call, and whether joining is possible yet.
/// The list comes from the network, or from this device's cache when the network fails
/// (offline mid-session, #62), with the cache's unread counts either way.
@MainActor
@Observable
final class ChannelsModel {
    private(set) var channels: [ChannelRow] = []
    /// Set by the server's `Ready`: until then a join would hit `Disconnected`.
    private(set) var ready = false
    /// channel id → participants in its live call.
    private(set) var liveCalls: [String: UInt32] = [:]
    private(set) var error: String?
    /// The open conversation, which takes the message events.
    var timeline: TimelineModel?
    /// The open channel: its unread badge stays 0 whatever the cache counts.
    var openChannel: String? {
        didSet { if let id = openChannel { setUnread(id, 0) } }
    }
    /// Closed by the cache (the user was removed): the view clears its selection.
    private(set) var closed: String?

    private let client: any FfiBrookClientProtocol
    private var events: Subscription?
    /// Channels the cache said this user was removed from: filtered out of every later read,
    /// so an older list landing after the removal can't bring one back.
    private var removed: Set<String> = []
    /// Each list read's number: only the newest one to finish applies.
    private var generation = 0

    init(client: any FfiBrookClientProtocol) {
        self.client = client
    }

    private var offline: (any OfflineClient)? { client as? any OfflineClient }

    /// Subscribe to events BEFORE starting realtime, so the first `Ready` (and any call already
    /// live) is never missed; then list channels. Each step on its own: a realtime failure
    /// doesn't skip the list.
    func start() async {
        guard events == nil else { return }
        events = client.subscribeEvents(listener: EventBridge(self))
        do { try await client.startRealtime() } catch {}
        await reloadList()
    }

    func stop() {
        events?.cancel()
        events = nil
    }

    /// The network list, with unread counts from the cache; else the cached list; else the
    /// error. Only the newest read applies.
    func reloadList() async {
        generation += 1
        let mine = generation
        var rows: [ChannelRow]?
        if let list = try? await client.listChannels() {
            let unread = await cachedUnread()
            rows = list.filter { !$0.archived }.map { ChannelRow($0, unread: unread[$0.id] ?? 0) }
        } else if let cached = try? await offline?.cachedChannels() {
            rows = cached.filter { !$0.archived }.map(ChannelRow.init)
        }
        guard mine == generation else { return } // a newer read started meanwhile
        guard let rows else {
            if channels.isEmpty { error = "Couldn't load channels." }
            return
        }
        error = nil
        channels = rows.filter { !removed.contains($0.id) }.map { row in
            var row = row
            if row.id == openChannel { row.unread = 0 }
            return row
        }
    }

    /// The cache's unread counts (empty without local data).
    private func cachedUnread() async -> [String: Int64] {
        guard let cached = try? await offline?.cachedChannels() else { return [:] }
        return Dictionary(cached.map { ($0.id, $0.unreadCount) }, uniquingKeysWith: { a, _ in a })
    }

    // ---- The cache's notices (through `CacheFeed`) ----

    /// Rows changed: refresh the unread badges (the open channel's stays 0).
    func cacheChannelsChanged() async {
        let unread = await cachedUnread()
        guard !unread.isEmpty else { return }
        for i in channels.indices where channels[i].id != openChannel {
            if let n = unread[channels[i].id] { channels[i].unread = n }
        }
    }

    /// Removed from these channels: gone from the list, and the open one closes.
    func cacheRemoved(_ ids: [String]) {
        removed.formUnion(ids)
        channels.removeAll { removed.contains($0.id) }
        if let open = openChannel, ids.contains(open) {
            closed = open
            timeline = nil
        }
    }

    private func setUnread(_ id: String, _ n: Int64) {
        if let i = channels.firstIndex(where: { $0.id == id }) { channels[i].unread = n }
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

    func canJoin(_ channel: ChannelRow) -> Bool { ready }

    func title(_ channel: ChannelRow) -> String { channel.name ?? "Direct message" }

    /// "● Call · N" in the sidebar, or nil when no call is live.
    func badge(_ channel: ChannelRow) -> String? {
        liveCalls[channel.id].map { "● Call · \($0)" }
    }

    /// The unread count, when there is one (the open channel's is always 0).
    func unread(_ channel: ChannelRow) -> Int64? {
        channel.unread > 0 && channel.id != openChannel ? channel.unread : nil
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
