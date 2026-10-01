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
        didSet {
            guard let id = openChannel else { return }
            setUnread(id, 0)
            // A click never re-sorts: the rank counts at the next re-sort (the history
            // back-fill that opening starts raises this conversation's own key, not the order).
            if id != oldValue { markOpened(id) }
        }
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
    private let defaults: UserDefaults
    /// The Settings window's "Show usernames", pushed in by the view: flipping it relabels the
    /// rows in place and never moves one (the order ignores labels' form).
    var showUsernames: Bool {
        didSet { if showUsernames != oldValue { channels = channels.map(makeRow) } }
    }
    /// Channels the cache said this user was removed from, with the newest read number at
    /// that moment: a read that started before the removal can't bring one back, and a later
    /// read (re-added since) can.
    private var removedAt: [String: Int] = [:]
    /// Each list read's number: only the newest one to finish applies.
    private var generation = 0

    // ---- The sidebar order (#219): channels, then DMs; each by newest message, then by what
    // this device opened last, then by name. The keys live here, not in the rows, so a reload
    // or a `channel.update` that replaces a row can't lose them. ----

    /// channel id → the newest message id seen, from the cache or live. Only moves forward.
    private var activity: [String: String] = [:]
    /// `activity` as of the last re-sort: a live message moves the list only if it is newer than
    /// this, so a cache notice that already raised `activity` can't swallow it.
    private var sorted: [String: String] = [:]
    /// channel id → when this device opened it (higher is later), saved per account.
    private var opened: [String: Int] = [:]
    private var nextRank = 1
    /// Opened, and the history back-fill that opening starts has not been seen yet: it raises
    /// the channel's own key without moving it, even once other conversations were opened or
    /// the list re-sorted meanwhile. A channel leaves when a notice has moved its key (the
    /// back-fill, consumed) or when a live message for it arrives; a re-sort does not clear it.
    private var awaitingBackfill: Set<String> = []

    init(client: any FfiBrookClientProtocol, me: String? = nil, notifier: (any Notifying)? = nil,
         isActive: @escaping @MainActor () -> Bool = { AppActivity.isActive },
         defaults: UserDefaults = .standard) {
        self.client = client
        self.defaults = defaults
        showUsernames = Settings(defaults: defaults).showUsernames
        self.me = me
        self.notifier = notifier
        self.isActive = isActive
        if let key = Self.ranksKey(me) {
            opened = (defaults.dictionary(forKey: key) as? [String: Int]) ?? [:]
            nextRank = (opened.values.max() ?? 0) + 1
        }
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
    @discardableResult
    func reloadList() async -> Bool {
        generation += 1
        let mine = generation
        var rows: [ChannelRow]?
        var keys: [String: String] = [:]
        var fromNetwork = false
        if let list = try? await client.listChannels() {
            let counts = await cachedCounts()
            rows = list.map {
                ChannelRow($0, unread: counts[$0.id]?.unread ?? 0, mentions: counts[$0.id]?.mentions)
            }
            keys = counts.compactMapValues(\.last)
            fromNetwork = true
        } else if let cached = try? await offline?.cachedChannels() {
            rows = cached.map(ChannelRow.init)
            keys = Dictionary(cached.compactMap { c in c.lastMessageId.map { (c.id, $0) } }, uniquingKeysWith: max)
        }
        guard mine == generation else { return false } // a newer read started meanwhile
        guard let rows else {
            if channels.isEmpty { error = "Couldn't load channels." }
            return true
        }
        error = nil
        channels = rows.filter { removedAt[$0.id].map { mine > $0 } ?? true }.map { row in
            var row = makeRow(row)
            if row.id == openChannel { (row.unread, row.unreadMentions) = (0, 0) }
            return row
        }
        // After the generation check, so a superseded read leaves the keys alone. `max`: the
        // cache may be behind what a live message already told us.
        raiseKeys(keys)
        // Only a network list says a channel is gone: an offline read (even an empty cache)
        // must not erase the saved ranks.
        let present = Set(channels.map(\.id))
        if fromNetwork, !channels.isEmpty, opened.keys.contains(where: { !present.contains($0) }) {
            opened = opened.filter { present.contains($0.key) }
            saveRanks()
        }
        resort()
        return true
    }

    /// Move keys forward only; the ids that moved.
    @discardableResult
    private func raiseKeys(_ keys: [String: String]) -> [String] {
        var moved: [String] = []
        for (id, key) in keys where key > (activity[id] ?? "") {
            activity[id] = key
            moved.append(id)
        }
        return moved
    }

    private func resort() {
        let entries = channels.map {
            FfiSidebarEntry(id: $0.id, kind: $0.kind, lastMessageId: activity[$0.id],
                            opened: opened[$0.id].map(Int64.init), sortKey: $0.nameKey)
        }
        let rank = Dictionary(sidebarOrder(entries: entries).enumerated().map { ($1, $0) }, uniquingKeysWith: { a, _ in a })
        channels.sort { (rank[$0.id] ?? 0) < (rank[$1.id] ?? 0) }
        sorted = activity
    }

    private static func ranksKey(_ me: String?) -> String? {
        guard let me, !me.isEmpty else { return nil } // nothing is saved for nobody
        return "ChannelOpenedRanks.\(me)"
    }

    private func markOpened(_ id: String) {
        opened[id] = nextRank
        nextRank += 1
        awaitingBackfill.insert(id)
        saveRanks()
    }

    /// On the main actor, synchronously: no write can land after a later one.
    private func saveRanks() {
        if let key = Self.ranksKey(me) { defaults.set(opened, forKey: key) }
    }

    /// The cache's unread and unread-mention counts (empty without local data).
    private func cachedCounts() async -> [String: (unread: Int64, mentions: Int64, last: String?)] {
        guard let cached = try? await offline?.cachedChannels() else { return [:] }
        return Dictionary(cached.map { ($0.id, (unread: $0.unreadCount, mentions: $0.unreadMentions, last: $0.lastMessageId)) },
                          uniquingKeysWith: { a, _ in a })
    }

    // ---- The cache's notices (through `CacheFeed`) ----

    /// Rows changed: refresh the unread badges (the open channel's stays 0).
    func cacheChannelsChanged() async {
        let counts = await cachedCounts()
        guard !counts.isEmpty else { return }
        for i in channels.indices where channels[i].id != openChannel {
            if let n = counts[channels[i].id] { (channels[i].unread, channels[i].unreadMentions) = (n.unread, n.mentions) }
        }
        let moved = raiseKeys(counts.compactMapValues(\.last))
        // The open conversation, and ones opened whose back-fill has not been seen, raise their
        // own key while their history back-fills: that is not news, and moving the row under
        // the pointer would be a jump.
        let listed = Set(channels.map(\.id))
        let news = moved.contains { listed.contains($0) && $0 != openChannel && !awaitingBackfill.contains($0) }
        awaitingBackfill.subtract(moved) // the notice that moved a key is the back-fill, consumed
        if news { resort() }
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
            ordered(message)
            arrived(message)
        case .messageUpdate, .messageDelete, .resync, .typing, .reactionUpdate:
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
        // An update's per-user counts are 0 (never a count): the row keeps its own.
        channels[i] = makeRow(ChannelRow(channel, unread: channels[i].unread, mentions: channels[i].unreadMentions))
    }

    /// A live message re-sorts the list when it is newer than the key the list was last sorted
    /// by. A channel not in the list yet keeps the key for when its row arrives.
    private func ordered(_ message: FfiMessage) {
        let id = message.channelId
        let moves = activityMoves(current: sorted[id], messageId: message.id)
        raiseKeys([id: message.id])
        awaitingBackfill.remove(id) // news of its own: any later notice is the cache catching up
        if moves, channels.contains(where: { $0.id == id }) { resort() }
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

    /// Re-read the list and say whether `id` is in it, so the caller can select a channel it just
    /// created, joined or opened (the row may not have arrived through an event yet). One more
    /// read if the first didn't have it; a channel the list never gets is never selected.
    func reveal(_ id: String) async -> Bool {
        await reveal(id) { await self.reloadList() }
    }

    /// `reveal` with the read it makes (tests give it reads that get superseded).
    func reveal(_ id: String, read: () async -> Bool) async -> Bool {
        var misses = 0
        // A read superseded by a newer one (an event's, say) applies nothing: it isn't a miss,
        // and the loop reads again (a few times at most).
        for _ in 0..<4 {
            let applied = await read()
            if channels.contains(where: { $0.id == id }) { return true }
            if applied { misses += 1 }
            if misses == 2 { break }
        }
        return false
    }

    func canJoin(_ channel: ChannelRow) -> Bool { ready }

    /// The pending offer to make this user an owner of `channel` (#190), while `me` is known.
    func offerToMe(_ channel: ChannelRow) -> FfiOwnerOffer? {
        guard let me, !me.isEmpty else { return nil }
        return channel.ownerOffers.first { $0.userId == me }
    }

    /// The one place a row gets its label and name key, so a list load, a `channel.update` and
    /// a toggle of the preference all agree (and `body` never calls into core per row).
    private func makeRow(_ row: ChannelRow) -> ChannelRow {
        var row = row
        func label(_ show: Bool) -> String {
            conversationLabel(kind: row.kind, name: row.name, members: row.members, me: me ?? "", showUsernames: show)
        }
        row.label = label(showUsernames)
        row.nameKey = label(false).lowercased()
        return row
    }

    /// What the sidebar, window title and notifications call a conversation: `#name` for a
    /// channel, the other person for a DM (`@handle` with Show usernames on).
    func title(_ channel: ChannelRow) -> String { channel.label }

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
