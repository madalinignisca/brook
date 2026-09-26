import BrookCore
import Foundation
import Observation

/// One signed-in client's local-data notices (#62 spec items 3 to 5 and 7): the cache's
/// events and state, delivered in order on the main thread, fanned out to the models that
/// registered, plus the offline banner, lost unsent messages and other accounts' data.
@MainActor
@Observable
final class CacheFeed {
    /// The last sync couldn't reach the server: the banner shows.
    private(set) var offline = false {
        didSet { timeline?.offline = offline }
    }
    private(set) var lastSynced: Int64?

    /// What an alert says, one at a time: a loss of unsent messages (acknowledged exactly
    /// when dismissed), or the notice after other accounts' data went.
    enum Alert: Equatable, Identifiable {
        case lost(UInt64)
        case notice(String)

        /// Each alert is its own presentation (the next one re-presents).
        var id: String {
            switch self {
            case let .lost(n): "lost-\(n)"
            case let .notice(text): "notice-\(text)"
            }
        }

        var text: String {
            switch self {
            case .lost: CacheFeed.lostText
            case let .notice(text): text
            }
        }
    }

    /// Alerts waiting, the first on screen.
    private(set) var alerts: [Alert] = []
    var alert: Alert? { alerts.first }

    weak var channels: ChannelsModel?
    weak var timeline: TimelineModel? {
        didSet { timeline?.offline = offline }
    }
    weak var pending: PendingModel?

    private let client: any OfflineClient
    private var subscriptions: [Subscription] = []
    /// Other accounts' data: once per feed, retried at the next sync after a failure.
    private var cleanedUp = false
    private var cleaning = false
    /// Stopped (signed out): nothing late may queue an alert.
    private var stopped = false

    init(client: any OfflineClient) {
        self.client = client
    }

    /// Subscribes before local data is switched on: opening the stores can report a loss.
    func start() {
        guard subscriptions.isEmpty else { return }
        subscriptions = [
            client.subscribeCacheEvents(listener: CacheEventBridge(self)),
            client.subscribeCacheState(listener: CacheStateBridge(self)),
        ]
    }

    /// Signed out: nothing more arrives, and the banner and alerts reset.
    func stop() {
        stopped = true
        subscriptions.forEach { $0.cancel() }
        subscriptions = []
        offline = false
        lastSynced = nil
        alerts = []
    }

    func handle(_ event: FfiCacheEvent) {
        switch event {
        case let .channels(ids):
            Task {
                await channels?.cacheChannelsChanged()
                if let t = timeline, ids.contains(t.channelId) { await t.refill() }
            }
        case let .users(ids):
            Task { await timeline?.refreshAuthors(ids) }
        case let .removed(ids):
            channels?.cacheRemoved(ids)
            checkLost()
        case .reset: // also what a lagged feed arrives as
            Task {
                await channels?.reloadList()
                await timeline?.refill()
                await pending?.reload()
            }
            checkLost()
        case let .outbox(channelId):
            if let p = pending, p.channelId == channelId { Task { await p.reload() } }
        case .outboxLost:
            checkLost()
        case .files:
            break // the Mac's file cache UI comes after #79
        }
    }

    func state(_ state: FfiCacheState) {
        offline = state.offline
        lastSynced = state.lastSyncedUnixMs
        // Other accounts' data goes at this user's first completed sync (#46 §8).
        if lastSynced != nil, !cleanedUp, !cleaning {
            cleaning = true
            Task { await cleanUpOthers() }
        }
    }

    // ---- Lost unsent messages ----

    /// Queue a loss if there is one and none is queued.
    func checkLost() {
        guard !stopped, !alerts.contains(where: { if case .lost = $0 { true } else { false } }),
              let n = client.outboxLost()
        else { return }
        alerts.append(.lost(n))
    }

    /// The alert on screen was dismissed: a loss is acknowledged for exactly the one shown,
    /// then a newer one may queue, after this update (so the alert shows again).
    func dismiss() {
        guard !alerts.isEmpty else { return }
        if case let .lost(n) = alerts.removeFirst() {
            client.acknowledgeOutboxLost(n: n)
            checkLost() // a newer loss: a new alert id, so it presents again
        }
    }

    nonisolated static let lostText = "Some unsent messages on this Mac couldn't be recovered."

    // ---- Other accounts ----

    func cleanUpOthers() async {
        defer { cleaning = false }
        guard let others = try? await client.otherLocalUsers() else { return } // retried later
        guard !others.isEmpty else {
            cleanedUp = true
            return
        }
        guard (try? await client.wipeOtherLocalUsers()) != nil else { return } // retried later
        cleanedUp = true
        if !stopped { alerts.append(.notice(Self.noticeText(others))) }
    }

    /// Names their unsent messages (#46 §8: "wiped after their unsent count is surfaced").
    static func noticeText(_ others: [FfiLocalUser]) -> String {
        let base = "Another account's saved messages were removed from this Mac"
        if others.contains(where: { $0.unsent == nil }) {
            return base + ", which may have included unsent messages."
        }
        let unsent = others.reduce(UInt64(0)) { $0 + ($1.unsent ?? 0) }
        switch unsent {
        case 0: return base + "."
        case 1: return base + ", including 1 unsent message."
        default: return base + ", including \(unsent) unsent messages."
        }
    }
}

/// The cache's events, in order, on the main thread.
final class CacheEventBridge: CacheEventListener, @unchecked Sendable {
    private weak var feed: CacheFeed?
    init(_ feed: CacheFeed) { self.feed = feed }

    func onCacheEvent(event: FfiCacheEvent) {
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { self?.feed?.handle(event) }
        }
    }
}

/// The cache's state, in order, on the main thread.
final class CacheStateBridge: CacheStateListener, @unchecked Sendable {
    private weak var feed: CacheFeed?
    init(_ feed: CacheFeed) { self.feed = feed }

    func onCacheState(state: FfiCacheState) {
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { self?.feed?.state(state) }
        }
    }
}
