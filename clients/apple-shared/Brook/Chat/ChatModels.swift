// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import Foundation
import Observation
import UniformTypeIdentifiers

/// What the chat views need from the client (a fake in tests).
protocol ChatClient: AnyObject, Sendable {
    func channelHistory(channelId: String, before: String?) async throws -> [FfiMessage]
    func sendMessage(channelId: String, body: String, replyToId: String?,
                     clientId: String?) async throws -> FfiMessage
    func editMessage(channelId: String, messageId: String, body: String) async throws -> FfiMessage
    func deleteMessage(channelId: String, messageId: String) async throws
    func markRead(channelId: String, messageId: String?) async throws
    func sendTyping(channelId: String) async throws
    func toggleReaction(channelId: String, messageId: String, emoji: String) async throws -> [FfiReaction]
    func downloadFile(transferId: UInt64, fileId: String, sha256: String, size: UInt64,
                      destination: String) async throws
    func cancelTransfer(transferId: UInt64)
    func subscribeTransfers(listener: TransferListener) -> Subscription
}

extension FfiBrookClient: ChatClient {}

/// One channel's messages, oldest at the top. Keyed by id and kept in id order (ids are
/// UUIDv7: time order). With local data, pages come from this device's cache first (#62),
/// then the network; whatever order pages and live events land in, the merge gives the same
/// result (see `merge`).
@MainActor
@Observable
final class TimelineModel {
    let channelId: String
    private(set) var messages: [FfiMessage] = []
    private(set) var loading = false
    /// No older page: the start of the channel is on screen.
    private(set) var atStart = false
    /// The last older page failed: the view offers a retry instead of a spinner. It stays until
    /// tapped, or until the newest page loads again (then the loader asks by itself): a spinner that
    /// retried on every change is what looped.
    private(set) var olderFailed = false
    /// An older page is being asked for: another ask (the Retry button's new spinner appearing, say)
    /// joins it instead of waiting to start one more.
    private var olderInFlight = false
    /// The newest page failed to load: the network is not asked for an older page behind it (it would
    /// end at an empty "start of the conversation" over history that never loaded); cached pages are.
    private(set) var headFailed = false
    /// The network's error. Hidden (`visibleError`) while offline with cached messages shown.
    private(set) var error: String?
    /// Set from the cache's state feed: the last sync couldn't reach the server.
    var offline = false {
        // The connection is back: a head load that failed is tried again.
        didSet { if oldValue, !offline { retryFailedHead() } }
    }
    /// Current (name, handle) of authors (a `Users` notice): cached rows keep what they were
    /// stored with. Never a finished label, so Show usernames relabels without a reload.
    private(set) var authors: [String: (name: String, handle: String)] = [:]

    /// iOS only (see `init`): every newest-page fetch also closes a gap, and `.ready` re-reads.
    private let rereadOnReady: Bool
    /// Up to 150 older messages (3 pages of `pageSize`) are fetched to find where a re-read meets
    /// what is shown. Past that, replacing the shown history is cheaper than paging further.
    static let rereadPageBacks = 3
    /// How many times a re-read replaced the shown history. The view scrolls to the newest message
    /// when it changes, and `loadOlder` drops an older page that started before the change: that
    /// page belongs to history that is no longer shown.
    private(set) var replaced = 0
    /// The newest message that was shown when a re-read was asked for, and has not been filled up
    /// to yet. The gap check compares the re-read's pages against this, not against whatever is
    /// newest when the fetch finally runs: a live message merged in between (a `message.new`
    /// right after `ready`, or the user's own send) is newer than the gap and would hide it.
    private var gapAnchor: String?

    /// Ids deleted, including ones not shown yet: every later copy stays a tombstone.
    private var deleted: Set<String> = []
    /// Messages here came from the cache.
    private var fromCache = false

    private let client: any ChatClient
    static let pageSize: UInt32 = 50
    /// Whether the user can be looking at it: the app is active (#145's note, as GTK #177).
    private let isActive: @MainActor () -> Bool
    /// Messages arrived while the app was in the background: read when it's active again.
    private(set) var readOwed = false
    /// Who's typing here (never this user: `me` is filtered out).
    private(set) var typing = TypingState()
    private let now: () -> Date
    /// This user's id: a reaction event of theirs sets their own flag.
    let me: String
    /// Why the last reaction didn't go through: cleared by the next one, or by itself after
    /// `errorLifetime`.
    private(set) var reactionError: String?
    private var reactionErrorSerial = 0
    private let errorLifetime: Duration
    /// Messages whose reaction is being toggled: a second tap on the same message meanwhile sends
    /// nothing (answers then arrive in the order they were asked, and can't drop each other's).
    private var reacting: Set<String> = []
    /// Live reaction events seen per message: a toggle's answer is a snapshot, and is used only
    /// if none arrived while it was in flight (the events carry the newer counts).
    private var reactionEvents: [String: Int] = [:]
    /// The last server `seq` applied per message and emoji, so a late, older event is dropped.
    private var reactionSeqs: [String: Int64] = [:]
    /// The same, for events that are mine: they decide the "I reacted" flag on their own.
    private var reactionMineSeqs: [String: Int64] = [:]

    func clearReactionError() {
        reactionErrorSerial += 1
        reactionError = nil
    }

    /// Show a failure, and take it down after `errorLifetime` unless a newer one replaced it.
    private func failReaction() {
        reactionErrorSerial += 1
        let serial = reactionErrorSerial
        reactionError = "Couldn't react. Try again."
        let lifetime = errorLifetime
        Task { [weak self] in
            try? await Task.sleep(for: lifetime)
            guard let self, reactionErrorSerial == serial else { return }
            reactionError = nil
        }
    }

    /// The open channel's members (a data source, not the flag): a typing notice carries only a
    /// name, and the handle comes from here.
    private let members: @MainActor () -> [FfiMember]

    /// `rereadOnReady` is iOS only, the twin of `ChannelsModel.rereadOnReconnect`. iOS suspends the
    /// socket in the background and keeps no cache, so events sent meanwhile are lost; a re-read of
    /// the newest page (on every `.ready` and on return to the foreground) is the only way to show
    /// them. The Mac's `CacheFeed` fills that hole, so it leaves this off, and there a `.ready` only
    /// retries a failed head, as before.
    init(channelId: String, client: any ChatClient, me: String = "",
         members: @escaping @MainActor () -> [FfiMember] = { [] }, now: @escaping () -> Date = Date.init,
         errorLifetime: Duration = .seconds(5),
         isActive: @escaping @MainActor () -> Bool = { AppActivity.isActive },
         rereadOnReady: Bool = false) {
        self.channelId = channelId
        self.client = client
        self.rereadOnReady = rereadOnReady
        self.me = me
        self.members = members
        self.now = now
        self.errorLifetime = errorLifetime
        self.isActive = isActive
    }

    /// Toggle your reaction. The answer is the message's whole summary from your side, and replaces
    /// its row's unless a reaction event for the message arrived meanwhile: the answer is then
    /// older than what the events say (your own echo included), so it's left out.
    func toggleReaction(_ message: FfiMessage, emoji: String) async {
        guard !message.deleted, reacting.insert(message.id).inserted else { return }
        defer { reacting.remove(message.id) }
        clearReactionError()
        let seen = reactionEvents[message.id, default: 0]
        do {
            let summary = try await client.toggleReaction(
                channelId: channelId, messageId: message.id, emoji: emoji)
            if reactionEvents[message.id, default: 0] == seen,
               let i = messages.firstIndex(where: { $0.id == message.id }) {
                messages[i].reactions = summary
            }
        } catch {
            failReaction()
        }
    }

    /// The app became active: what arrived meanwhile is read now.
    func appBecameActive() {
        guard readOwed, let newest = messages.last else { return }
        readOwed = false
        Task { try? await client.markRead(channelId: channelId, messageId: newest.id) }
    }

    private var cache: (any OfflineClient)? { client as? any OfflineClient }

    /// What the view shows: no network error while offline with cached messages on screen.
    var visibleError: String? { offline && fromCache ? nil : error }

    /// "Ann is typing…" and so on. The handle comes from the channel's members, then from a
    /// refreshed author, then from a message of theirs here; with none the name shows.
    func typingLine(now: Date, showUsernames: Bool) -> String? {
        let members = members()
        return typing.line(now: now) { userId, name in
            let handle = members.first { $0.id == userId }?.handle
                ?? authors[userId]?.handle
                ?? messages.last { $0.authorId == userId }?.authorHandle
            return PersonName.label(name, handle: handle, showUsernames: showUsernames)
        }
    }

    /// The name to show for a message's author.
    func authorName(_ message: FfiMessage, showUsernames: Bool) -> String {
        if let a = authors[message.authorId] {
            return PersonName.label(a.name, handle: a.handle, showUsernames: showUsernames)
        }
        return PersonName.label(
            message.authorDisplayName, handle: message.authorHandle, showUsernames: showUsernames) // raw name: input to the label
    }

    /// The cached head first (and, when the cache can't vouch for it, a load and a re-read),
    /// then the network's newest page; then mark read.
    func load() async {
        await readCache(before: nil, loadIfIncomplete: true)
        await fetchHead() // returns when the page, and any resync queued behind it, is merged
        if let newest = messages.last {
            try? await client.markRead(channelId: channelId, messageId: newest.id)
        }
    }

    /// The page before the oldest shown (scrolled to the top): the cache's, loading it when
    /// the cache can't vouch for it; the network's when there's no local data.
    func loadOlder() async {
        // A load already running (the newest page of a new conversation): wait for it and then
        // decide. Giving up here left the loader spinning for good, since it asks once, when it
        // appears. Only the newest page's load is waited for: asks for older pages join each other.
        // (`loading` covers the network fetches; a cached head's own load, in `readCache`, is not
        // waited for: an older read then runs beside it and still ends at the start or a page.)
        while loading || headDrain != nil, !olderInFlight, !Task.isCancelled {
            try? await Task.sleep(for: .milliseconds(25))
        }
        guard !Task.isCancelled, !olderInFlight, !atStart, !loading, !messages.isEmpty else { return }
        olderInFlight = true
        defer { olderInFlight = false }
        olderFailed = false
        // A re-read can replace the shown history (`replaced`) while this waits on the cache or on
        // a head fetch, or while the older request is in the air. The oldest message noted below
        // then belongs to history that is gone, and so does the answer. So: note `replaced` together
        // with the oldest message (no suspension between the two), check it again just before the
        // request and when the answer lands, and on a change start over from the new oldest message.
        // Starting over, not giving up: the loader's spinner asks once, when it appears, and a dropped
        // ask would leave it spinning for good. A replace needs a whole re-read, so this cannot spin.
        while !Task.isCancelled {
            guard !atStart, let oldest = messages.first else { return }
            let replacedAtStart = replaced
            loading = true // one page at a time
            let fromCache = await readCache(before: oldest.id, loadIfIncomplete: true)
            loading = false
            if fromCache { return }
            // A newest-page fetch may have started meanwhile (a returning connection, a resync): its
            // outcome decides whether an older page may be asked for, so it is waited for.
            if let drain = headDrain { await drain.value }
            // The last look before the request. Nothing suspends between this check and the call
            // inside `fetch`, so no replace can land in between.
            if replaced != replacedAtStart { continue }
            // Cached history is paged whatever the network does, but behind a newest page that failed the
            // network is not asked for older ones (an empty answer would read as the start of the
            // conversation): the Retry button instead, so no spinner is left that nothing will end.
            if headFailed {
                olderFailed = true
                return
            }
            if await fetch(before: oldest.id, replacedAtAsk: replacedAtStart) { continue }
            return
        }
    }

    /// The loader above the first message: shown while an older page can be asked for. Behind a failed
    /// newest page only cached history can be, so without any it stays hidden (the error shows).
    var offersOlder: Bool { !atStart && !messages.isEmpty && (!headFailed || fromCache) }

    /// Re-read the cached head (the cache changed for this channel, or was reset).
    func refill() async {
        await readCache(before: nil, loadIfIncomplete: false)
    }

    /// Current names for these authors, from the cache.
    func refreshAuthors(_ ids: [String]) async {
        guard let users = try? await cache?.cachedUsers(ids: ids) else { return }
        for u in users { authors[u.id] = (u.displayName, u.handle) } // raw name: stored with its handle
    }

    /// One cached page, merged: true if the cache answered (so the network isn't needed for
    /// paging). An incomplete page is loaded and read again; an empty, complete page before
    /// `before` is the start of the channel.
    @discardableResult
    private func readCache(before: String?, loadIfIncomplete: Bool) async -> Bool {
        guard let cache else { return false }
        do {
            var page = try await cache.cachedMessages(channelId: channelId, before: before, limit: Self.pageSize)
            if page.needsNetwork, loadIfIncomplete {
                if before == nil {
                    try? await cache.loadHead(channelId: channelId, limit: Self.pageSize)
                } else {
                    try? await cache.loadOlder(channelId: channelId, limit: Self.pageSize)
                }
                page = try await cache.cachedMessages(channelId: channelId, before: before, limit: Self.pageSize)
                // Still nothing the cache can vouch for (the load failed, or brought
                // nothing): the network path, which pages or shows its error, instead of
                // paging getting stuck.
                if page.needsNetwork, page.messages.isEmpty { return false }
            }
            if before != nil, page.messages.isEmpty, !page.needsNetwork { atStart = true }
            if !page.messages.isEmpty { fromCache = true }
            merge(page.messages)
            return true
        } catch {
            return false // no local data (yet): the network path
        }
    }

    /// One page from the network, merged. Returns true when this was an older page whose answer
    /// was dropped because a re-read replaced the history after the ask (`replacedAtAsk`): the
    /// caller starts over. The head fetch (`before == nil`) never drops its answer.
    @discardableResult
    private func fetch(before: String?, replacedAtAsk: Int = 0) async -> Bool {
        loading = true
        defer { loading = false }
        do {
            let page = try await client.channelHistory(channelId: channelId, before: before)
            // The answer belongs to history that was replaced while it was in the air: merging it
            // would put old messages above the new ones, and `atStart` would describe the wrong end.
            if before != nil, replaced != replacedAtAsk { return true }
            if page.isEmpty, before != nil { atStart = true }
            // The newest page is back: an older page is asked for again, and a Retry left from the time
            // it was held back is not shown.
            if before == nil { headFailed = false; olderFailed = false }
            merge(page)
            error = nil
        } catch {
            // A failure of a dropped ask says nothing about the new history either.
            if before != nil, replaced != replacedAtAsk { return true }
            self.error = "Couldn't load messages."
            if before != nil { olderFailed = true } else { headFailed = true }
        }
        return false
    }

    /// The newest page for a re-read (`rereadOnReady`), closing the gap behind it. Runs inside the
    /// head drain, so it is one fetch at a time like any head fetch. Steps:
    /// 1. Take the anchor (the newest message shown when the re-read was asked for, see
    ///    `noteGapAnchor`) and clear it, so a trigger during this turn notes a fresh one for the
    ///    turn queued behind it. No anchor was noted for a first load or a failed head's retry; the
    ///    newest shown message is then the right point.
    /// 2. Fetch the newest page. It "meets" the shown messages when there is no anchor, it is
    ///    empty, or its oldest id is at or below the anchor (ids are UUIDv7: id order is time order).
    /// 3. Not met: page back with `before:` the oldest fetched id, up to `rereadPageBacks` times. An
    ///    empty page-back is the start of the channel: everything since is fetched, so that meets.
    /// 4. Met: merge. Not met: replace (see below).
    /// Any request failing leaves everything as it was: merging part of a fetch would leave a hole
    /// in the middle of the conversation. The anchor goes back, so the next try (the next `ready`
    /// or foreground) still compares against the oldest point that may have a gap.
    private func fetchHeadFillingGap() async {
        loading = true
        defer { loading = false }
        let anchor = gapAnchor ?? messages.last?.id
        gapAnchor = nil
        do {
            var fetched = try await client.channelHistory(channelId: channelId, before: nil)
            var met = Self.meets(fetched, anchor: anchor)
            var pageBacks = 0
            while !met, pageBacks < Self.rereadPageBacks, let oldest = fetched.map(\.id).min() {
                pageBacks += 1
                let older = try await client.channelHistory(channelId: channelId, before: oldest)
                fetched = older + fetched
                met = older.isEmpty || Self.meets(older, anchor: anchor)
            }
            if met {
                merge(fetched)
            } else {
                // Still no meeting point: the shown history is too far behind to join. Keep what was
                // fetched, plus the shown messages newer than the newest fetched one: live messages
                // and the user's own sends merged while the page-backs ran are in `messages` and
                // not in `fetched`. Dropping them would lose a message the user just saw. `deleted`
                // is kept, so a message deleted earlier stays a tombstone in `merge`.
                let newest = fetched.map(\.id).max() ?? ""
                messages = messages.filter { $0.id > newest }
                atStart = false // the start of the channel is no longer the oldest shown message's
                olderFailed = false
                replaced += 1
                merge(fetched)
            }
            headFailed = false
            olderFailed = false
            error = nil
        } catch {
            // The older of this turn's anchor and any fresh one a trigger noted meanwhile.
            gapAnchor = [anchor, gapAnchor].compactMap { $0 }.min()
            self.error = "Couldn't load messages."
            headFailed = true
        }
    }

    /// A page meets the shown messages when nothing is owed (no anchor), it is empty (nothing
    /// newer exists), or it reaches back to the anchor.
    private static func meets(_ page: [FfiMessage], anchor: String?) -> Bool {
        guard let anchor, let oldest = page.map(\.id).min() else { return true }
        return oldest <= anchor
    }

    /// Remember the newest shown message as the point a coming re-read must reach back to. Every
    /// trigger calls this synchronously, before anything else can be merged. Triggers before the
    /// next head fetch starts share the first (oldest) anchor: `gapAnchor` is only set when empty.
    private func noteGapAnchor() {
        if gapAnchor == nil { gapAnchor = messages.last?.id }
    }

    /// iOS: read the newest page again (a `ready`, or the app coming back to the front), closing any
    /// gap. The anchor is taken here, synchronously, and not when the fetch runs. Returns the task so
    /// a caller can wait for the result. It marks the newest message read when it brought a newer
    /// one (owed while the app is not active). It leaves the reaction marks alone: with the socket
    /// still up no event was missed (`.ready` clears them, see `apply`).
    @discardableResult
    func reread() -> Task<Void, Never> {
        noteGapAnchor()
        let newestBefore = messages.last?.id
        return Task {
            await fetchHead()
            if error == nil, messages.last?.id != newestBefore { await markNewestRead() }
        }
    }

    /// The running chain of head fetches, until the last one queued behind it ends.
    private var headDrain: Task<Void, Never>?
    private var headRefetchPending = false

    /// The newest page, returning once it (and any fetch queued behind it) has landed. One
    /// head fetch runs at a time: a request that arrives during one (a resync, say) is not
    /// run beside it, and is not dropped either, since it may know of newer state than the
    /// fetch in flight: one more fetch follows, however many arrived. The request waits for
    /// that chain too, so a caller that reads the result (`load()`) sees the merged page.
    private func fetchHead() async {
        if let drain = headDrain {
            headRefetchPending = true
            await drain.value
            return
        }
        await startHeadDrain().value
    }

    /// Takes the one-at-a-time slot (synchronously, so two callers can't both start).
    private func startHeadDrain() -> Task<Void, Never> {
        let drain = Task { [self] in
            repeat {
                headRefetchPending = false
                if rereadOnReady { await fetchHeadFillingGap() } else { await fetch(before: nil) }
            } while headRefetchPending
            headDrain = nil
        }
        headDrain = drain
        return drain
    }

    /// The newest message is read: now if the app is active, else when it becomes so.
    private func markNewestRead() async {
        guard let newest = messages.last else { return }
        if isActive() {
            try? await client.markRead(channelId: channelId, messageId: newest.id)
        } else {
            readOwed = true // shown, not seen yet
        }
    }

    /// Retry the head load when a signal says the connection is back (`.ready`, or offline
    /// going false), if there is an error to clear. The error may also come from a failed
    /// older page; the retry then loads the head, which succeeding clears it. A retry that
    /// fails sets the error again and waits for the next signal: no timer, no loop. The
    /// slot is taken here, before the task runs, so two signals arriving together start one
    /// fetch. A retry that succeeds reads the channel as `load()` would have.
    private func retryFailedHead() {
        guard error != nil, headDrain == nil else { return }
        let drain = startHeadDrain()
        Task {
            await drain.value
            if error == nil { await markNewestRead() }
        }
    }

    /// A live event, if it's this channel's.
    func apply(_ event: FfiServerEvent) {
        switch event {
        case let .typing(channel, userId, name):
            guard channel == channelId, userId != me else { return }
            typing.note(userId: userId, name: name, at: now())
        case let .messageNew(message), let .messageUpdate(message):
            guard message.channelId == channelId else { return }
            if case .messageNew = event { typing.clear(userId: message.authorId, at: now()) }
            merge([message])
            if case .messageNew = event {
                if isActive() {
                    Task { try? await client.markRead(channelId: channelId, messageId: message.id) }
                } else {
                    readOwed = true // shown, not seen yet
                }
            }
        case let .messageDelete(channel, messageId):
            guard channel == channelId else { return }
            deleted.insert(messageId) // even before its page lands
            if let i = messages.firstIndex(where: { $0.id == messageId }) {
                messages[i] = Self.tombstone(messages[i])
            }
        case .resync:
            // A reconnect (or a restored server) may number events from lower values again.
            reactionSeqs = [:]
            reactionMineSeqs = [:]
            // With the flag, the head fetch below also closes a gap, and needs its anchor now: the
            // binding delivers the events it kept right after `Resync` (bindings/apple/src/client.rs),
            // so they are merged before the fetch runs, and the newest shown message by then is newer
            // than the events that were dropped.
            if rereadOnReady { noteGapAnchor() }
            Task { await fetchHead() }
        case let .reactionUpdate(channel, messageId, emoji, userId, added, count, seq):
            guard channel == channelId, let i = messages.firstIndex(where: { $0.id == messageId }) else { return }
            // The server numbers changes in commit order. The count and my own flag are ordered
            // apart: an older event must not put an older count back, but my own older event can
            // still be the newest word on whether *I* reacted (someone else's event carried the
            // newer count and not my flag).
            let key = "\(messageId)\u{0}\(emoji)"
            let byMe = !me.isEmpty && userId == me
            let countFresh = seq > (reactionSeqs[key] ?? Int64.min)
            let meFresh = byMe && seq > (reactionMineSeqs[key] ?? Int64.min)
            guard countFresh || meFresh else { return }
            if countFresh { reactionSeqs[key] = seq }
            if meFresh { reactionMineSeqs[key] = seq }
            reactionEvents[messageId, default: 0] += 1
            if countFresh {
                messages[i].reactions = ReactionRules.applying(
                    emoji: emoji, count: count, added: added, byMe: meFresh, to: messages[i].reactions)
            } else if let j = messages[i].reactions.firstIndex(where: { $0.emoji == emoji }) {
                let r = messages[i].reactions[j]
                messages[i].reactions[j] = FfiReaction(emoji: r.emoji, count: r.count, me: added)
            }
        case .ready:
            if rereadOnReady {
                // PROTOCOL.md section 2, `reaction.update`: on every reconnect the order marks are
                // forgotten before the refetch, since a restored server may number from lower values
                // again. Only here: a foreground re-read (`reread()`) has the socket still up.
                reactionSeqs = [:]
                reactionMineSeqs = [:]
                reread()
            } else {
                retryFailedHead() // every (re)connect
            }
        case .channelCall, .channelUpdate, .channelDelete:
            break
        }
    }

    /// Insert or replace by id, keeping id order, so arrival order doesn't matter:
    /// - a deleted id stays deleted (a late copy can't bring it back);
    /// - the body and its edited mark change only for a newer `editedAt` (a missing one is
    ///   older than any), so a stale page never undoes an edit;
    /// - every other field (names, the quoted excerpt, files) takes the incoming copy.
    func merge(_ incoming: [FfiMessage]) {
        guard !incoming.isEmpty else { return }
        var byId = Dictionary(messages.map { ($0.id, $0) }, uniquingKeysWith: { _, new in new })
        for var m in incoming {
            if m.deleted { deleted.insert(m.id) }
            if deleted.contains(m.id) {
                byId[m.id] = Self.tombstone(m)
                continue
            }
            if let old = byId[m.id], !Self.isNewer(m.editedAt, than: old.editedAt) {
                m.body = old.body
                m.editedAt = old.editedAt
            }
            byId[m.id] = m
        }
        messages = byId.values.sorted { $0.id < $1.id }
    }

    /// Whether `incoming`'s body replaces `shown`'s: a later edit (nil: never edited, older
    /// than any edit), or neither ever edited (the incoming copy, as for every other field).
    static func isNewer(_ incoming: String?, than shown: String?) -> Bool {
        switch (incoming, shown) {
        case (nil, nil): true
        case (nil, .some): false
        case (.some, nil): true
        case let (.some(a), .some(b)):
            if let da = parseTime(a), let db = parseTime(b) { da > db } else { a > b }
        }
    }

    /// The server's RFC 3339 times, with or without fractional seconds (it drops zero ones,
    /// so they don't order as text: "…:00.5Z" < "…:00Z").
    private static func parseTime(_ s: String) -> Date? {
        fractional.date(from: s) ?? whole.date(from: s)
    }

    // Formatters are costly to make; these are only read (thread-safe for reading).
    nonisolated(unsafe) private static let fractional: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()
    nonisolated(unsafe) private static let whole: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime]
        return f
    }()

    private static func tombstone(_ m: FfiMessage) -> FfiMessage {
        var gone = m
        gone.body = ""
        gone.deleted = true
        gone.attachments = []
        return gone
    }
}

/// `ChannelsModel` holds the open conversation as an `OpenTimeline` (see its doc); the real
/// model already has both members, so adopting the protocol needs no code.
extension TimelineModel: OpenTimeline {}

/// The message box: text, what it replies to, or the message being edited.
@MainActor
@Observable
final class ComposerModel {
    var text = "" {
        // Not while editing (choosing Edit fills the field without the user typing), and not when
        // the app puts a failed message's text back (`restore`).
        didSet {
            if text != oldValue, editing == nil, !restoring { typing.draftChanged(text) }
            if text != oldValue { forgetDraftUnlessOwnWrite() }
        }
    }
    private(set) var replyingTo: FfiMessage? {
        didSet { if replyingTo?.id != oldValue?.id { forgetDraftUnlessOwnWrite() } }
    }
    private(set) var editing: FfiMessage?
    private(set) var sending = false
    private(set) var error: String?

    private let channelId: String
    private let client: any ChatClient
    /// Tells the server you're typing (throttled).
    private let typing: TypingSender
    /// The app is putting a failed message's text back: that isn't typing.
    private var restoring = false

    private func restore(_ typed: String) {
        restoring = true
        text = typed
        restoring = false
    }
    /// Where a sent or edited message goes (the timeline, before the live event arrives).
    private let onMessage: (FfiMessage) -> Void
    /// The channel's unsent bubbles, re-read after a message is queued.
    weak var pending: PendingModel?

    // ---- Files for the next message (#66, spec 2026-09-26-mac-send-files) ----

    /// Staged files, each holding its security-scoped access until removed or sent.
    // No draft reset here: every staged file gets a new transfer id, so a changed set can never
    // match the draft's `files` in `draftId` (a removed file does not come back with its id).
    private(set) var staged: [StagedFile] = []
    /// The files are being copied into the outbox: nothing staged may change meanwhile.
    private(set) var preparing = false
    /// Core said there's no local data: files can't be sent (text still can). Cleared by the
    /// next queued send that works (the stores may open after the first try).
    private(set) var filesUnavailable = false
    var fileAccess: any FileAccess = SystemFileAccess()

    /// Attach and drops are open: local data possible, not preparing, not editing.
    var canAttach: Bool {
        !readOnly && client is any OfflineClient && !filesUnavailable && !preparing && editing == nil
    }

    /// An archived channel: nothing can be written to it (the view also replaces the composer).
    var readOnly = false

    func attach(_ urls: [URL]) {
        guard canAttach else { return }
        for url in urls {
            switch Staging.stage(url, already: staged, access: fileAccess) {
            case let .success(file): staged.append(file)
            case let .failure(refusal): if let text = refusal.text { error = text }
            }
        }
    }

    /// How a dropped file is copied out of its provider (a test passes its own). The Mac app sets
    /// its own importer (pasted images need AppKit); iOS has no drop or paste
    /// screen yet, so this plain default is never called there. It cannot be the Mac's closure:
    /// that needs AppKit, which shared code must not import.
    var importer: (NSItemProvider) async -> DropImport.Outcome = { await DropImport.copy($0) }

    /// Files dropped on the conversation: each is copied at once (see `DropImport`), then staged
    /// like a picked one. Whatever isn't staged leaves no copy behind.
    func attach(dropped providers: [NSItemProvider]) async {
        guard canAttach else { return }
        for provider in providers {
            // Before the copy: a full message needs no further 100 MiB copies to say so.
            guard staged.count < Int(maxFilesPerMessage()) else { error = StagingRefusal.tooMany.text; break }
            switch await importer(provider) {
            case let .refused(refusal): if let text = refusal.text { error = text }
            case let .copied(url, dir, source):
                // The composer may have moved on during the copy (a send began, an edit started).
                guard canAttach else {
                    try? FileManager.default.removeItem(at: dir)
                    error = "\(url.lastPathComponent) wasn't attached."
                    continue
                }
                switch Staging.stage(url, already: staged, access: fileAccess, ownedDir: dir, source: source) {
                case let .success(file): staged.append(file)
                case let .failure(refusal): if let text = refusal.text { error = text }
                }
            }
        }
    }

    /// Staged files not sent when the composer goes (a channel left, a draft abandoned): their
    /// copies and access end with it.
    isolated deinit { staged.forEach { $0.release() } }

    func remove(_ file: StagedFile) {
        guard !preparing else { return } // its access is in use by the enqueue
        staged.removeAll { $0 === file }
        file.release()
    }
    /// The id of a send that failed, and what it was sent with. A failed send may still have
    /// arrived (the network can drop after the server stored it), so pressing Send again on the
    /// restored, unchanged box reuses the id and the server (or core's outbox) keeps one copy.
    /// It is reused ONLY for that. As soon as the user changes the text, the quote or the files,
    /// clears the box, or starts an edit or a reply, the draft is dropped (`forgetDraft...`
    /// below). Otherwise a later, different message that happens to match ("ok" typed again, or
    /// "hello" re-sent after editing the delivered "hello" to "goodbye") would carry the old id,
    /// and the server would return the stored message and silently lose the new one (as GTK, #162).
    private var draft: (body: String, reply: String?, files: [UInt64], id: String)?
    /// True while the composer itself writes the box (clearing it on send, putting a failed
    /// message back): those writes must not count as the user changing the message.
    private var ownWrite = false

    private func forgetDraftUnlessOwnWrite() {
        if !ownWrite { draft = nil }
    }

    private func writingTheBoxItself(_ change: () -> Void) {
        ownWrite = true
        change()
        ownWrite = false
    }

    init(channelId: String, client: any ChatClient, onMessage: @escaping (FfiMessage) -> Void) {
        self.channelId = channelId
        self.client = client
        typing = TypingSender(channelId: channelId, client: client)
        self.onMessage = onMessage
    }

    func reply(to message: FfiMessage) {
        guard !readOnly else { return }
        draft = nil
        editing = nil
        replyingTo = message
    }

    func edit(_ message: FfiMessage) {
        guard !readOnly else { return }
        draft = nil // a delivered message being edited must not lend its id to a later send
        replyingTo = nil
        editing = message
        text = message.body
    }

    func cancel() {
        draft = nil
        replyingTo = nil
        if editing != nil { text = "" }
        editing = nil
    }

    /// A message needs text or files (#126), so an edit may clear the caption of a message
    /// that has files, but not empty a text-only one.
    var canSend: Bool {
        guard !sending, !preparing else { return false }
        let empty = text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        return !empty || editing?.attachments.isEmpty == false || (editing == nil && !staged.isEmpty)
    }

    /// Sends (or saves an edit). The box clears at once; on failure the text and the reply
    /// come back, with why. A network failure may still have delivered it, so it says so
    /// instead of "not sent". Sending the same text and quote again is safe: the direct send
    /// reuses the failed attempt's `client_id` (the draft's), and the server returns the
    /// message it already stored instead of making a second one (PROTOCOL.md §1).
    func send() async {
        guard canSend, !readOnly else { return }
        if editing == nil, !staged.isEmpty {
            await sendWithFiles()
            return
        }
        let (typed, reply, editing) = (text, replyingTo, self.editing)
        // A blank caption goes out as no caption, not as spaces.
        let body = typed.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? "" : typed
        writingTheBoxItself { // clearing the box is not the user changing the message
            text = ""
            replyingTo = nil
        }
        self.editing = nil
        sending = true
        defer { sending = false }
        // One id for this message, for the queue and for the direct send below: if the
        // network drops after the server stored it, the retry carries the same id and the
        // server answers with the stored message. An edit has no id (`editMessage` takes none).
        let id = editing == nil ? draftId(body: body, reply: reply?.id, files: []) : nil
        if let id, let cache = client as? any OfflineClient {
            do {
                _ = try await cache.sendQueued(channelId: channelId, body: body, replyToId: reply?.id,
                                               clientId: id)
                filesUnavailable = false // the queue works: this device's storage is there now
                draft = nil
                error = nil
                await pending?.reload()
                return
            } catch where error.isLocalUnavailable {
                // No local data (yet): sent directly below, as before. The draft stays: if this
                // send fails after the server stored it, sending again must reuse the id.
            } catch {
                if text.isEmpty { // unless something new was typed meanwhile
                    writingTheBoxItself {
                        restore(typed)
                        replyingTo = reply
                    }
                }
                self.error = Self.explainQueued(error)
                return
            }
        }
        do {
            let message: FfiMessage
            if let editing {
                message = try await client.editMessage(channelId: channelId, messageId: editing.id,
                                                       body: body)
            } else {
                message = try await client.sendMessage(channelId: channelId, body: body,
                                                       replyToId: reply?.id, clientId: id)
                // Delivered: the next message, even with the same text, is a new one and needs
                // a new id, or the server would hand back this one and drop the new one.
                draft = nil
                // A retry can come back as a tombstone: the message arrived the first time and
                // was deleted before this answer. It is gone, not a live message to show.
                if message.deleted {
                    error = nil
                    return
                }
            }
            error = nil
            onMessage(message)
        } catch {
            if text.isEmpty {  // unless something new was typed meanwhile
                writingTheBoxItself {
                    restore(typed)
                    replyingTo = reply
                    self.editing = editing
                }
            }
            self.error = Self.explain(error)
        }
    }

    /// The failed draft's id for the same message (text, quote and staged files), else a new
    /// lowercase one.
    private func draftId(body: String, reply: String?, files: [UInt64]) -> String {
        if let draft, draft.body == body, draft.reply == reply, draft.files == files { return draft.id }
        let id = UUID().uuidString.lowercased()
        draft = (body, reply, files, id)
        return id
    }

    /// A message with files: copied into the outbox off the main thread (large files), with
    /// everything locked meanwhile; on success sent and cleared, on error all kept, with why.
    private func sendWithFiles() async {
        guard let cache = client as? any OfflineClient else { return }
        let files = staged
        let (typed, reply) = (text, replyingTo)
        let body = typed.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty ? "" : typed
        let id = draftId(body: body, reply: reply?.id, files: files.map(\.transferId))
        let outgoing = files.map(\.outgoing)
        let channel = channelId
        preparing = true
        let result: Result<FfiSendReceipt, Error> = await Task.detached {
            do {
                return .success(try await cache.sendQueuedWithFiles(
                    channelId: channel, body: body, replyToId: reply?.id, clientId: id, files: outgoing))
            } catch {
                return .failure(error)
            }
        }.value
        preparing = false
        switch result {
        case .success:
            files.forEach { $0.release() } // snapshotted: their access is no longer needed
            staged = []
            if text == typed { text = "" }
            replyingTo = nil
            draft = nil
            error = nil
            await pending?.reload()
        case let .failure(failure):
            if failure.isLocalUnavailable { filesUnavailable = true }
            error = Self.explainFiles(failure)
        }
    }

    /// A send with files that core refused, as GTK's `send_error_text`.
    static func explainFiles(_ error: Error) -> String {
        guard case let .Api(code, _) = error as? LoginError else { return explain(error) }
        switch code {
        case "outbox.too_many_files": return "Too many files for one message."
        case "outbox.file_too_large": return "A file is too large to send."
        case "outbox.empty_file": return "An empty file can't be sent."
        case "outbox.empty_message": return "Write something or add a file."
        case "outbox.file_unreadable": return "A file couldn't be read. Is it still there?"
        case "outbox.store": return "Couldn't prepare the files. Is the disk full?"
        case "local.unavailable": return "Sending files needs this \(ThisDevice.name)'s storage, which isn't available yet."
        default: return explain(error)
        }
    }

    /// A queued send that failed before it was saved (nothing was queued).
    static func explainQueued(_ error: Error) -> String {
        switch error as? LoginError {
        case let .Api(code, _) where code == "outbox.empty_message": "Write something first."
        case .NotAuthenticated: "You were signed out."
        default: "Couldn't save the message to send."
        }
    }

    func delete(_ message: FfiMessage) async {
        draft = nil // the failed send may be the one deleted: its id would now return a tombstone
        do {
            try await client.deleteMessage(channelId: channelId, messageId: message.id)
        } catch {
            self.error = "Couldn't delete the message."
        }
    }

    static func explain(_ error: Error) -> String {
        switch error as? LoginError {
        case .Network, .Timeout, .Disconnected:
            "It may not have been sent: check the conversation before sending again."
        case let .Api(code, _) where code == "message.reply_target_gone":
            "The message you replied to was deleted."
        case .NotAuthenticated:
            "You were signed out."
        default:
            "Couldn't send."
        }
    }
}

/// Saving attachments: per file, its progress or its error.
@MainActor
@Observable
final class SaveModel {
    enum State: Equatable {
        case saving(done: UInt64, total: UInt64)
        case saved
        case failed(String)
    }

    private(set) var states: [String: State] = [:]
    private var transfers: [String: UInt64] = [:]  // file id → transfer id
    private let client: any ChatClient
    private var subscription: Subscription?

    init(client: any ChatClient) {
        self.client = client
    }

    func start() {
        guard subscription == nil else { return }
        subscription = client.subscribeTransfers(listener: TransferBridge(self))
    }

    func stop() {
        subscription?.cancel()
        subscription = nil
    }

    func save(_ file: FfiFileInfo, to destination: URL) async {
        guard let sha = file.sha256 else {
            states[file.id] = .failed("This file isn't ready yet.")
            return
        }
        let transfer = UInt64.random(in: 1 ... UInt64.max)
        transfers[file.id] = transfer
        states[file.id] = .saving(done: 0, total: file.size)
        // Into a temporary file first, then moved over the destination: saving over an
        // existing file ("Replace") must not destroy it if the download then fails.
        let temp = FileManager.default.temporaryDirectory
            .appending(path: "brook-save-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: temp) }
        do {
            try await client.downloadFile(transferId: transfer, fileId: file.id, sha256: sha,
                                          size: file.size, destination: temp.path)
            if FileManager.default.fileExists(atPath: destination.path) {
                _ = try FileManager.default.replaceItemAt(destination, withItemAt: temp)
            } else {
                try FileManager.default.moveItem(at: temp, to: destination)
            }
            states[file.id] = .saved
        } catch let LoginError.Api(code, _) where code == "file.gone" {
            states[file.id] = .failed("No longer available.")
        } catch let LoginError.Api(code, _) where code == "transfer.cancelled" {
            states[file.id] = nil
        } catch {
            states[file.id] = .failed("Couldn't save the file.")
        }
        transfers[file.id] = nil
    }

    func cancel(_ file: FfiFileInfo) {
        if let transfer = transfers[file.id] { client.cancelTransfer(transferId: transfer) }
    }

    func progress(_ event: FfiTransferEvent) {
        guard let file = transfers.first(where: { $0.value == event.transferId })?.key,
              case .saving = states[file] else { return }
        states[file] = .saving(done: event.done, total: event.total)
    }
}

/// Delivers transfer progress to the model on the main thread, in order.
final class TransferBridge: TransferListener, @unchecked Sendable {
    private weak var model: SaveModel?
    init(_ model: SaveModel) { self.model = model }

    func onTransfer(event: FfiTransferEvent) {
        DispatchQueue.main.async { [weak self] in
            MainActor.assumeIsolated { self?.model?.progress(event) }
        }
    }

    func onResync() {}  // a save's end state comes from its own call, not from events
}

/// How a live reaction event changes a message's chips. Absolute: the event carries the emoji's
/// new total, so applying the same event twice changes nothing. (Two different events for one
/// emoji can still arrive out of order; the server doesn't order them, so a wrong count lasts
/// until the next event, a re-read, or the cache's sync.)
enum ReactionRules {
    static func applying(emoji: String, count: Int64, added: Bool, byMe: Bool,
                         to list: [FfiReaction]) -> [FfiReaction] {
        var list = list
        let i = list.firstIndex { $0.emoji == emoji }
        let me = byMe ? added : (i.map { list[$0].me } ?? false)
        if count <= 0 {
            if let i { list.remove(at: i) }
        } else if let i {
            list[i] = FfiReaction(emoji: emoji, count: count, me: me)
        } else {
            list.append(FfiReaction(emoji: emoji, count: count, me: me))
        }
        return list
    }

    /// The quick set (as GTK's).
    static let quick = ["\u{1F44D}", "\u{2764}\u{FE0F}", "\u{1F602}", "\u{1F389}", "\u{1F440}", "\u{1F64F}"]
}
