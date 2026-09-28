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
    /// Closed by the cache or the server (the user left or was removed): the view clears its
    /// selection.
    private(set) var closed: String?

    private let client: any FfiBrookClientProtocol
    private var events: Subscription?
    /// This user's id (nil until known: nothing counts or notifies before).
    private let me: String?
    private let notifier: (any Notifying)?
    private let isActive: @MainActor () -> Bool
    /// Channels the cache said this user was removed from, with the newest read number at
    /// that moment: a read that started before the removal can't bring one back, and a later
    /// read (re-added since) can.
    private var removedAt: [String: Int] = [:]
    /// Each list read's number: only the newest one to finish applies.
    private var generation = 0

    init(client: any FfiBrookClientProtocol, me: String? = nil, notifier: (any Notifying)? = nil,
         isActive: @escaping @MainActor () -> Bool = { AppActivity.isActive }) {
        self.client = client
        self.me = me
        self.notifier = notifier
        self.isActive = isActive
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
            let counts = await cachedCounts()
            rows = list.filter { !$0.archived }.map {
                ChannelRow($0, unread: counts[$0.id]?.unread ?? 0, mentions: counts[$0.id]?.mentions)
            }
        } else if let cached = try? await offline?.cachedChannels() {
            rows = cached.filter { !$0.archived }.map(ChannelRow.init)
        }
        guard mine == generation else { return } // a newer read started meanwhile
        guard let rows else {
            if channels.isEmpty { error = "Couldn't load channels." }
            return
        }
        error = nil
        channels = rows.filter { removedAt[$0.id].map { mine > $0 } ?? true }.map { row in
            var row = row
            if row.id == openChannel { (row.unread, row.unreadMentions) = (0, 0) }
            return row
        }
    }

    /// The cache's unread and unread-mention counts (empty without local data).
    private func cachedCounts() async -> [String: (unread: Int64, mentions: Int64)] {
        guard let cached = try? await offline?.cachedChannels() else { return [:] }
        return Dictionary(cached.map { ($0.id, (unread: $0.unreadCount, mentions: $0.unreadMentions)) },
                          uniquingKeysWith: { a, _ in a })
    }

    // ---- The cache's notices (through `CacheFeed`) ----

    /// Rows changed: refresh the unread badges (the open channel's stays 0).
    func cacheChannelsChanged() async {
        let counts = await cachedCounts()
        guard !counts.isEmpty else { return }
        for i in channels.indices where channels[i].id != openChannel {
            if let n = counts[channels[i].id] { (channels[i].unread, channels[i].unreadMentions) = n }
        }
    }

    /// Removed from these channels: gone from the list, and the open one closes.
    func cacheRemoved(_ ids: [String]) {
        for id in ids { removedAt[id] = generation }
        channels.removeAll { ids.contains($0.id) }
        if let open = openChannel, ids.contains(open) {
            closed = open
            timeline = nil
        }
    }

    private func setUnread(_ id: String, _ n: Int64) {
        if let i = channels.firstIndex(where: { $0.id == id }) {
            channels[i].unread = n
            if n == 0 { channels[i].unreadMentions = 0 } // read: its mentions are too
        }
    }

    func handle(_ event: FfiServerEvent) {
        switch event {
        case .ready:
            ready = true
        case let .channelCall(channelId, callId, count):
            liveCalls[channelId] = callId != nil && count > 0 ? count : nil
        case let .messageNew(message):
            timeline?.apply(event)  // the open conversation's
            arrived(message)
        case .messageUpdate, .messageDelete, .resync:
            timeline?.apply(event)
        case let .channelDelete(channelId):
            cacheRemoved([channelId]) // left, removed or deleted: as the cache's removal
        case let .channelUpdate(channel):
            channelChanged(channel)
        }
    }

    /// A channel's new state: replace its row, keeping the unread count. One not in the
    /// list (added to it, or re-added after a removal) is never inserted from the event:
    /// the list is re-read, so the server's word decides, and a channel just left can't
    /// come back from a late update.
    private func channelChanged(_ channel: FfiChannel) {
        guard let i = channels.firstIndex(where: { $0.id == channel.id }) else {
            Task { await reloadList() }
            return
        }
        if channel.archived {
            channels.remove(at: i) // as a list read drops archived channels
            if channel.id == openChannel { // and an open one closes, as a removal does
                closed = channel.id
                timeline = nil
            }
            return
        }
        // An update's per-user counts are 0 (never a count): the row keeps its own.
        channels[i] = ChannelRow(channel, unread: channels[i].unread, mentions: channels[i].unreadMentions)
    }

    /// A live message that isn't being read: its channel's badge rises, and it notifies.
    private func arrived(_ message: FfiMessage) {
        guard NotificationPlanner.counts(message, me: me, openChannel: openChannel, appActive: isActive()),
              let me, let i = channels.firstIndex(where: { $0.id == message.channelId })
        else { return }
        channels[i].unread += 1
        if NotificationPlanner.mentions(message, me: me) { channels[i].unreadMentions += 1 }
        notifier?.post(channelId: message.channelId, title: title(channels[i]),
                       body: NotificationPlanner.body(message, me: me))
    }

    func canJoin(_ channel: ChannelRow) -> Bool { ready }

    /// The pending offer to make this user an owner of `channel` (#190), while `me` is known.
    func offerToMe(_ channel: ChannelRow) -> FfiOwnerOffer? {
        guard let me, !me.isEmpty else { return nil }
        return channel.ownerOffers.first { $0.userId == me }
    }

    func title(_ channel: ChannelRow) -> String { channel.name ?? "Direct message" }

    /// "● Call · N" in the sidebar, or nil when no call is live.
    func badge(_ channel: ChannelRow) -> String? {
        liveCalls[channel.id].map { "● Call · \($0)" }
    }

    /// The unread mentions, when there are any, by the same rule as `unread`.
    func mentions(_ channel: ChannelRow) -> Int64? {
        let seen = channel.id == openChannel && timeline?.readOwed != true
        return channel.unreadMentions > 0 && !seen ? channel.unreadMentions : nil
    }

    /// The unread count, when there is one (the open channel's is always 0).
    func unread(_ channel: ChannelRow) -> Int64? {
        // The open channel's count shows only while what arrived there isn't seen yet.
        let seen = channel.id == openChannel && timeline?.readOwed != true
        return channel.unread > 0 && !seen ? channel.unread : nil
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
