import BrookCore
import Synchronization
import XCTest

@testable import Brook

private let t0 = Date(timeIntervalSince1970: 1_000_000)

final class TypingStateTests: XCTestCase {
    func testTheLineNamesOneTwoOrMany() {
        var s = TypingState()
        XCTAssertNil(s.line(now: t0))
        s.note(userId: "a", name: "Ann", at: t0)
        XCTAssertEqual(s.line(now: t0), "Ann is typing…")
        s.note(userId: "b", name: "Bob", at: t0)
        XCTAssertEqual(s.line(now: t0), "Ann and Bob are typing…")
        s.note(userId: "c", name: "Cy", at: t0)
        XCTAssertEqual(s.line(now: t0), "Several people are typing…")
    }

    func testAnEntryExpiresFourSecondsAfterItsLastEvent() {
        var s = TypingState()
        s.note(userId: "a", name: "Ann", at: t0)
        XCTAssertNotNil(s.line(now: t0.addingTimeInterval(3.9)))
        XCTAssertNil(s.line(now: t0.addingTimeInterval(4)))
        s.note(userId: "a", name: "Ann", at: t0.addingTimeInterval(3)) // a fresh event restarts it
        XCTAssertNotNil(s.line(now: t0.addingTimeInterval(6)))
    }

    func testOnlyTheLiveOnesAreCounted() {
        var s = TypingState()
        s.note(userId: "a", name: "Ann", at: t0)
        s.note(userId: "b", name: "Bob", at: t0.addingTimeInterval(3))
        XCTAssertEqual(s.line(now: t0.addingTimeInterval(5)), "Bob is typing…", "Ann's has expired")
    }

    func testAMessageFromThemClearsIt() {
        var s = TypingState()
        s.note(userId: "a", name: "Ann", at: t0)
        s.clear(userId: "a", at: t0)
        XCTAssertNil(s.line(now: t0))
    }

    func testANoticeRightAfterTheirMessageIsTheOneSentJustBeforeItAndIsIgnored() {
        var s = TypingState()
        s.clear(userId: "a", at: t0) // their message arrived
        s.note(userId: "a", name: "Ann", at: t0.addingTimeInterval(0.5)) // the late notice
        XCTAssertNil(s.line(now: t0.addingTimeInterval(0.5)))
        s.note(userId: "a", name: "Ann", at: t0.addingTimeInterval(2)) // typing the next one
        XCTAssertNotNil(s.line(now: t0.addingTimeInterval(2)))
    }

    func testAMessageFromSomeoneElseLeavesTheOthersTyping() {
        var s = TypingState()
        s.note(userId: "a", name: "Ann", at: t0)
        s.note(userId: "b", name: "Bob", at: t0)
        s.clear(userId: "b", at: t0)
        XCTAssertEqual(s.line(now: t0), "Ann is typing…")
    }
}

@MainActor
final class TimelineTypingTests: XCTestCase {
    private func timeline(now: @escaping () -> Date) -> TimelineModel {
        TimelineModel(channelId: "c", client: FakeChat(), me: "me", now: now)
    }

    func testSomeoneElsesTypingShowsAndYourOwnAndOtherChannelsDont() {
        let t = timeline { t0 }
        t.apply(.typing(channelId: "c", userId: "bob", displayName: "Bob"))
        t.apply(.typing(channelId: "c", userId: "me", displayName: "Me"))
        t.apply(.typing(channelId: "other", userId: "ann", displayName: "Ann"))
        XCTAssertEqual(t.typing.line(now: t0), "Bob is typing…")
    }

    func testTheirMessageClearsIt() {
        let t = timeline { t0 }
        t.apply(.typing(channelId: "c", userId: "u", displayName: "U"))
        XCTAssertNotNil(t.typing.line(now: t0))
        t.apply(.messageNew(message: msg("m1", "hi"))) // msg() is from "u"
        XCTAssertNil(t.typing.line(now: t0))
    }
}

@MainActor
final class TypingSenderTests: XCTestCase {
    /// A settable clock (Mutex is noncopyable, so a small class holds it).
    private final class Clock: @unchecked Sendable {
        private let value = Mutex(t0)
        var now: Date {
            get { value.withLock { $0 } }
            set { value.withLock { $0 = newValue } }
        }
    }

    private func sender(_ chat: FakeChat, _ clock: Clock) -> TypingSender {
        TypingSender(channelId: "c", client: chat, now: { clock.now })
    }

    private func settle() async { for _ in 0..<20 { await Task.yield() } }

    func testAtMostOneEveryThreeSeconds() async {
        let chat = FakeChat()
        let clock = Clock()
        let s = sender(chat, clock)
        s.draftChanged("h")
        s.draftChanged("he")
        clock.now = t0.addingTimeInterval(2.9)
        s.draftChanged("hel")
        await settle()
        XCTAssertEqual(chat.typed.withLock { $0 }, ["c"], "throttled to one")
        clock.now = t0.addingTimeInterval(3)
        s.draftChanged("hell")
        await settle()
        XCTAssertEqual(chat.typed.withLock { $0 }, ["c", "c"])
    }

    func testAnEmptyDraftSendsNothing() async {
        let chat = FakeChat()
        let s = sender(chat, Clock())
        s.draftChanged("")
        s.draftChanged("   \n")
        await settle()
        XCTAssertEqual(chat.typed.withLock { $0 }, [])
    }

    func testChoosingEditSendsNothing() async {
        let chat = FakeChat()
        let composer = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        composer.edit(msg("m1", "my old text"))
        await settle()
        XCTAssertEqual(chat.typed.withLock { $0 }, [], "filling the field to edit isn't typing")
    }

    func testTheComposerTellsTheServerWhileTyping() async {
        let chat = FakeChat()
        let composer = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        composer.text = "hi"
        await settle()
        XCTAssertEqual(chat.typed.withLock { $0 }, ["c"])
    }
}

private final class FakeSearch: SearchClient, @unchecked Sendable {
    let queries = Mutex<[String]>([])
    var answer: [FfiMessage] = []
    var fails = false
    var gate: Gate?
    /// Holds only the first search (the later ones answer at once).
    var firstGate: Gate?
    var answers: [String: [FfiMessage]] = [:]
    private let calls = Mutex(0)
    func searchMessages(query: String) async throws -> [FfiMessage] {
        let n = calls.withLock { c -> Int in c += 1; return c }
        queries.withLock { $0.append(query) }
        if n == 1 { await firstGate?.wait() }
        await gate?.wait()
        if let special = answers[query] { return special }
        if fails { throw LoginError.Timeout }
        return answer
    }
}

@MainActor
final class SearchModelTests: XCTestCase {
    func testAnEmptyQuerySendsNothingAndClears() async {
        let client = FakeSearch()
        let model = SearchModel(client: client)
        model.query = "   "
        await model.submit()
        XCTAssertEqual(client.queries.withLock { $0 }, [])
        XCTAssertEqual(model.state, .idle)
    }

    func testResultsShowTheChannelAuthorAndAnExcerpt() async {
        let client = FakeSearch()
        var m = msg("m1", "  hello   there\nworld ", channel: "c7")
        m.authorDisplayName = "Ann"
        client.answer = [m]
        let model = SearchModel(client: client)
        model.query = " hello "
        await model.submit()
        XCTAssertEqual(client.queries.withLock { $0 }, ["hello"], "trimmed")
        guard case let .results(hits) = model.state else { return XCTFail("\(model.state)") }
        XCTAssertEqual(hits, [SearchHit(m)])
        XCTAssertEqual(hits[0].channelId, "c7")
        XCTAssertEqual(hits[0].author, "Ann")
        XCTAssertEqual(hits[0].excerpt, "hello there world")
        XCTAssertTrue(model.isShowing)
    }

    func testNoResultsAndAFailureHaveTheirStates() async {
        let client = FakeSearch()
        let model = SearchModel(client: client)
        model.query = "x"
        await model.submit()
        XCTAssertEqual(model.state, .none)
        client.fails = true
        await model.submit()
        XCTAssertEqual(model.state, .failed)
        XCTAssertEqual(SearchModel.offlineText, "Search needs a connection.")
    }

    func testAnOlderAnswerNeverReplacesANewerSearchOrAClear() async {
        let client = FakeSearch()
        let gate = Gate()
        client.gate = gate
        client.answer = [msg("m1", "old")]
        let model = SearchModel(client: client)
        model.query = "old"
        let first = Task { await model.submit() }
        while client.queries.withLock({ $0.isEmpty }) { await Task.yield() }
        model.clear() // the user cleared the field while it ran
        gate.open()
        await first.value
        XCTAssertEqual(model.state, .idle, "a stale answer came back after the clear")
    }

    func testAQueryOverTheServersLimitIsRefusedLocallyAndIsNotAConnectionError() async {
        let client = FakeSearch()
        let model = SearchModel(client: client)
        model.query = String(repeating: "a", count: 129)
        await model.submit()
        XCTAssertEqual(model.state, .tooLong)
        XCTAssertEqual(client.queries.withLock { $0 }, [], "nothing sent")
        model.query = String(repeating: "a", count: 128)
        await model.submit()
        XCTAssertEqual(client.queries.withLock { $0 }.count, 1, "128 is allowed")
    }

    func testTheResultsAreCappedAtFiftyWhenThereMayBeMore() async {
        let client = FakeSearch()
        client.answer = (0..<50).map { msg("m\($0)", "hit") }
        let model = SearchModel(client: client)
        model.query = "hit"
        await model.submit()
        XCTAssertTrue(model.capped)
        client.answer = [msg("only", "hit")]
        await model.submit()
        XCTAssertFalse(model.capped)
    }

    func testAnOlderSearchFinishingAfterANewerOneDoesNotReplaceIt() async {
        let client = FakeSearch()
        let gate = Gate()
        client.firstGate = gate
        client.answers = ["old": [msg("old1", "old")], "new": [msg("new1", "new")]]
        let model = SearchModel(client: client)
        model.query = "old"
        let first = Task { await model.submit() }
        while client.queries.withLock({ $0.isEmpty }) { await Task.yield() }
        model.query = "new"
        await model.submit() // the newer search resolves first
        gate.open()
        await first.value // then the older one finishes
        guard case let .results(hits) = model.state else { return XCTFail("\(model.state)") }
        XCTAssertEqual(hits.map(\.id), ["new1"], "the older answer replaced the newer results")
    }

    /// Editing the field while a search runs: its answer must not land under the new text.
    func testEditingTheQueryWhileASearchRunsDropsItsAnswer() async {
        let client = FakeSearch()
        let gate = Gate()
        client.gate = gate
        client.answer = [msg("m1", "old")]
        let model = SearchModel(client: client)
        model.query = "old"
        let first = Task { await model.submit() }
        while client.queries.withLock({ $0.isEmpty }) { await Task.yield() }
        model.query = "new" // typed on while it ran
        gate.open()
        await first.value
        XCTAssertEqual(model.state, .idle, "the old answer landed under the new query")
    }

    func testEditingTheQueryHidesResultsForTheOldOne() async {
        let client = FakeSearch()
        client.answer = [msg("m1", "hi")]
        let model = SearchModel(client: client)
        model.query = "hi"
        await model.submit()
        XCTAssertTrue(model.isShowing)
        model.query = "hit"
        XCTAssertFalse(model.isShowing)
    }

    func testClearResetsTheQueryAndHidesTheResults() async {
        let client = FakeSearch()
        client.answer = [msg("m1", "hi")]
        let model = SearchModel(client: client)
        model.query = "hi"
        await model.submit()
        model.clear()
        XCTAssertEqual(model.query, "")
        XCTAssertFalse(model.isShowing)
    }
}
