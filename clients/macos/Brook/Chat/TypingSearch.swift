import BrookCore
import Foundation
import Observation

// Typing and search (spec 2026-09-30-mac-timeline-extras, piece B).

/// Who's typing in the open channel: each is shown for 4 seconds after their last event, and
/// until a message from them arrives.
struct TypingState {
    static let lifetime: TimeInterval = 4

    /// A typing notice that arrives this soon after their message is the notice that was sent
    /// just before it (two requests, no ordering between them): not a new one. (A genuine first
    /// notice for their next message can be dropped by it, and the sender's 3 s throttle then
    /// delays the next; the line appears up to about 3 s late, a fair price for never showing a
    /// stale one.)
    static let afterMessage: TimeInterval = 2

    private var seen: [String: (name: String, at: Date)] = [:]
    private var lastMessage: [String: Date] = [:]

    mutating func note(userId: String, name: String, at: Date) {
        if let sent = lastMessage[userId], at.timeIntervalSince(sent) < Self.afterMessage { return }
        seen[userId] = (name, at)
    }

    /// A message from them: they're done typing.
    mutating func clear(userId: String, at: Date) {
        seen[userId] = nil
        lastMessage[userId] = at
    }

    /// "Ann is typing…", "Ann and Bob are typing…", "Several people are typing…"; nil when nobody.
    /// `label` says how each (user id, name as sent) reads, so a toggle relabels at once.
    func line(now: Date, label: (_ userId: String, _ name: String) -> String) -> String? {
        let names = seen.filter { now.timeIntervalSince($0.value.at) < Self.lifetime }
            .map { label($0.key, $0.value.name) }.sorted()
        switch names.count {
        case 0: return nil
        case 1: return "\(names[0]) is typing…"
        case 2: return "\(names[0]) and \(names[1]) are typing…"
        default: return "Several people are typing…"
        }
    }
}

/// Tells the server you're typing: at most once every 3 seconds, and never for an empty draft.
/// A failure is ignored (it's a courtesy, not a message).
@MainActor
final class TypingSender {
    static let interval: TimeInterval = 3

    private let channelId: String
    private let client: any ChatClient
    private let now: () -> Date
    private var last: Date?

    init(channelId: String, client: any ChatClient, now: @escaping () -> Date = Date.init) {
        self.channelId = channelId
        self.client = client
        self.now = now
    }

    func draftChanged(_ text: String) {
        guard !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return }
        let at = now()
        if let last, at.timeIntervalSince(last) < Self.interval { return }
        last = at
        let (client, channelId) = (client, channelId)
        Task { try? await client.sendTyping(channelId: channelId) }
    }
}

protocol SearchClient: AnyObject, Sendable {
    func searchMessages(query: String) async throws -> [FfiMessage]
}

extension FfiBrookClient: SearchClient {}

/// One found message, as the results list shows it.
struct SearchHit: Identifiable, Equatable {
    let id: String
    let channelId: String
    /// Kept as sent, never a finished label: the list relabels when drawn.
    let authorName: String?
    let authorHandle: String?
    let excerpt: String

    init(_ message: FfiMessage) {
        id = message.id
        channelId = message.channelId
        authorName = message.authorDisplayName // raw name: stored with its handle
        authorHandle = message.authorHandle
        let flat = message.body.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        excerpt = String(flat.prefix(100))
    }

    func author(showUsernames: Bool) -> String {
        PersonName.label(authorName, handle: authorHandle, showUsernames: showUsernames)
    }
}

/// Search the server's messages (online only, like GTK): Return searches, choosing a result
/// opens its channel.
@MainActor
@Observable
final class SearchModel {
    enum State: Equatable {
        case idle
        case searching
        case results([SearchHit])
        case none
        case failed
        /// The server refuses a query over its limit (422), which isn't "no connection".
        case tooLong
    }

    /// The server's query limit, and how many matches it returns (no paging past them).
    static let queryLimit = 128
    static let resultCap = 50

    static let offlineText = "Search needs a connection."

    /// Editing it drops whatever was in flight or showing: results under a different query would
    /// mislead, and an older answer must not land on it. Return starts the next search.
    var query = "" {
        didSet {
            guard query != oldValue else { return }
            generation += 1
            if state != .idle { state = .idle }
            capped = false
        }
    }
    private(set) var state: State = .idle
    /// The server returned as many as it will: there may be older matches it won't show.
    private(set) var capped = false
    private var generation = 0
    private let client: any SearchClient
    /// Whether the list has this channel: a hit in one it doesn't would open an empty pane, so it
    /// isn't listed (and if none are, the search reads as "No messages found.").
    private let known: (String) -> Bool

    init(client: any SearchClient, known: @escaping (String) -> Bool = { _ in true }) {
        self.client = client
        self.known = known
    }

    /// The results list replaces the channel list while a search is showing.
    var isShowing: Bool { state != .idle }


    func submit() async {
        let q = query.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !q.isEmpty else { return clear() }
        guard q.unicodeScalars.count <= Self.queryLimit else {
            generation += 1
            state = .tooLong
            return
        }
        generation += 1
        let mine = generation
        state = .searching
        do {
            let found = try await client.searchMessages(query: q)
            guard mine == generation else { return } // a newer search (or a clear) came meanwhile
            let hits = found.filter { known($0.channelId) }.map(SearchHit.init)
            capped = found.count >= Self.resultCap // the server's page, before any filtering
            state = hits.isEmpty ? .none : .results(hits)
        } catch {
            guard mine == generation else { return }
            state = .failed
        }
    }

    func clear() {
        generation += 1
        query = ""
        state = .idle
        capped = false
    }
}
