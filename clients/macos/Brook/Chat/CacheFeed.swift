import BrookCore
import Foundation
import Observation

/// The offline banner's bookkeeping, apart from the UI so it can be tested. Each started
/// timer has a number and only the newest waiting one may reveal the banner, so a timer
/// that is stale (the flag went false and true again, or the feed stopped) can't show it
/// early even if it still fires.
struct OfflineBannerGate {
    enum Action: Equatable {
        /// Online: hide at once; any reveal still waiting is cancelled.
        case hide
        /// Offline and not shown: start a timer that calls `onFire` with this number.
        case start(UInt64)
        /// Nothing to do (already shown, or a timer is already waiting).
        case keep
    }

    private var waiting: UInt64?
    private var lastTimer: UInt64 = 0

    mutating func onState(offline: Bool, shown: Bool) -> Action {
        guard offline else {
            waiting = nil
            return .hide
        }
        if shown || waiting != nil { return .keep }
        lastTimer += 1
        waiting = lastTimer
        return .start(lastTimer)
    }

    /// A timer fired: whether to show the banner now. Only the waiting timer may, and going
    /// online (or stopping) clears it, so "still offline" is implied.
    mutating func onFire(_ timer: UInt64) -> Bool {
        guard waiting == timer else { return false }
        waiting = nil
        return true
    }

    /// The feed stopped: nothing waiting may reveal anything.
    mutating func stop() { waiting = nil }
}

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

    /// The banner on screen: `offline` once it has lasted `bannerDelay` (a flaky link flips
    /// the flag within seconds, and not every flip is worth showing). Other readers of
    /// `offline` keep the raw flag.
    private(set) var showsOfflineBanner = false
    @ObservationIgnored private var gate = OfflineBannerGate()
    @ObservationIgnored private var bannerTimer: Task<Void, Never>?
    @ObservationIgnored private let bannerDelay: Duration

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

    /// Attachment rows on screen, by file id (held weakly; gone rows drop out).
    private var fileRows: [String: [WeakRow]] = [:]
    private struct WeakRow { weak var model: FileRowModel? }

    func register(_ row: FileRowModel) {
        purgeRows()
        fileRows[row.file.id, default: []].append(WeakRow(model: row))
    }

    private func purgeRows() {
        fileRows = fileRows.compactMapValues { rows in
            let live = rows.filter { $0.model != nil }
            return live.isEmpty ? nil : live
        }
    }

    /// Rows registered and still alive (tests).
    var registeredRows: Int { purgeRows(); return fileRows.values.reduce(0) { $0 + $1.count } }

    private let client: any OfflineClient
    private let defaults: UserDefaults
    private var subscriptions: [Subscription] = []
    /// Other accounts' data: once per feed, retried at the next sync after a failure.
    private var cleanedUp = false
    private var cleaning = false
    /// Stopped (signed out): nothing late may queue an alert.
    private var stopped = false

    init(client: any OfflineClient, bannerDelay: Duration = .seconds(3), defaults: UserDefaults = .standard) {
        self.client = client
        self.defaults = defaults
        self.bannerDelay = bannerDelay
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
        gate.stop()
        bannerTimer?.cancel()
        bannerTimer = nil
        showsOfflineBanner = false
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
        case let .files(ids):
            purgeRows()
            for id in ids {
                for row in fileRows[id] ?? [] {
                    if let model = row.model { Task { await model.reloadKeep() } }
                }
            }
        }
    }

    func state(_ state: FfiCacheState) {
        offline = state.offline
        updateBanner()
        lastSynced = state.lastSyncedUnixMs
        // Other accounts' data goes at this user's first completed sync (#46 §8).
        if lastSynced != nil, !cleanedUp, !cleaning {
            cleaning = true
            Task { await cleanUpOthers() }
        }
    }

    private func updateBanner() {
        guard !stopped else { return } // a late state must not start a wait after sign-out
        switch gate.onState(offline: offline, shown: showsOfflineBanner) {
        case .hide:
            bannerTimer?.cancel()
            bannerTimer = nil
            showsOfflineBanner = false
        case let .start(timer):
            bannerTimer = Task { [weak self, bannerDelay] in
                try? await Task.sleep(for: bannerDelay)
                guard !Task.isCancelled, let self else { return }
                bannerTimer = nil
                if gate.onFire(timer) { showsOfflineBanner = true }
            }
        case .keep:
            break
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
        // Before the wipe, whatever its outcome: the core wipes one account at a time and stops
        // at the first error, so a retry no longer lists the ones already wiped. The ranks are
        // derived, so losing those of an account that is then kept costs nothing.
        for other in others { ChannelsModel.eraseRanks(for: other.userId, defaults: defaults) }
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
