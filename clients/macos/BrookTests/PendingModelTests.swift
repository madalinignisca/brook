import BrookCore
import XCTest

@testable import Brook

func pendingMessage(_ id: String, _ state: FfiPendingState, body: String = "hi") -> FfiPendingMessage {
    FfiPendingMessage(clientId: id, channelId: "c", body: body, replyToId: nil, files: [], state: state)
}

/// Unsent bubbles (#62 spec item 2): GTK's texts and actions, and never a bubble beside the
/// message it became.
@MainActor
final class PendingModelTests: XCTestCase {
    func testEveryStateReadsAsGtkWithItsActions() {
        let cases: [(FfiPendingState, String, [PendingModel.Action])] = [
            (.pending, "Sending…", []),
            (.sending, "Sending…", []),
            (.accepted, "Sending…", []),
            (.failed(code: "http_500"), "Not sent", [.retry, .delete]),
            (.failed(code: "message.reply_target_gone"), "Not sent: the quoted message was deleted",
             [.sendWithoutQuote, .delete]),
            (.failed(code: "not_found"), "Not sent: you can't post here any more", [.retry, .delete]),
            (.failed(code: "authz.forbidden"), "Not sent: you can't post here any more", [.retry, .delete]),
            (.failed(code: "http_403"), "Not sent: you can't post here any more", [.retry, .delete]),
            (.failed(code: "http_404"), "Not sent: you can't post here any more", [.retry, .delete]),
        ]
        for (state, text, actions) in cases {
            let m = pendingMessage("q", state)
            XCTAssertEqual(PendingModel.text(m), text, "\(state)")
            XCTAssertEqual(PendingModel.actions(m), actions, "\(state)")
        }
    }

    func testABubbleGoesAsSoonAsItsMessageArrivesAndARereadDoesntBringItBack() async {
        let chat = FakeChat()
        chat.local = true
        chat.pendingReads = [[pendingMessage("q1", .sending)]]
        let timeline = TimelineModel(channelId: "c", client: chat)
        let pending = PendingModel(channelId: "c", client: chat)
        pending.timeline = timeline
        await pending.reload()
        XCTAssertEqual(pending.visible.map(\.clientId), ["q1"])
        var arrived = msg("m1", "hi")
        arrived.clientId = "q1"
        timeline.apply(.messageNew(message: arrived))
        XCTAssertTrue(pending.visible.isEmpty, "the bubble stayed beside its message")
        chat.pendingReads = [[pendingMessage("q1", .accepted)]] // the cache not caught up yet
        await pending.reload()
        XCTAssertTrue(pending.visible.isEmpty, "a re-read brought the bubble back")
    }

    func testActionsCallCoreThenReread() async {
        let chat = FakeChat()
        chat.local = true
        let pending = PendingModel(channelId: "c", client: chat)
        await pending.perform(.sendWithoutQuote, on: pendingMessage("q1", .failed(code: "message.reply_target_gone")))
        await pending.perform(.delete, on: pendingMessage("q2", .failed(code: "x")))
        XCTAssertEqual(chat.cacheCalls.withLock { $0 }, ["noquote:q1", "pending", "delete:q2", "pending"])
    }
}

/// Sending through the queue (#62 spec item 2).
@MainActor
final class ComposerQueueTests: XCTestCase {
    private func composer(_ chat: FakeChat) -> ComposerModel {
        ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
    }

    func testAQueuedSendClearsWithAFreshLowercaseIdAndRereadsTheBubbles() async {
        let chat = FakeChat()
        chat.local = true
        let c = composer(chat)
        let pending = PendingModel(channelId: "c", client: chat)
        c.pending = pending
        c.text = "hello"
        await c.send()
        c.text = "again"
        await c.send()
        let ids = chat.queued.withLock { $0 }.map { String($0.split(separator: "|")[0]) }
        XCTAssertEqual(ids.count, 2)
        XCTAssertNotEqual(ids[0], ids[1])
        XCTAssertEqual(ids[0], ids[0].lowercased())
        XCTAssertNotNil(UUID(uuidString: ids[0]))
        XCTAssertEqual(c.text, "")
        XCTAssertTrue(chat.sent.withLock { $0 }.isEmpty, "sent directly with local data")
        XCTAssertEqual(chat.cacheCalls.withLock { $0 }.filter { $0 == "pending" }.count, 2)
    }

    func testAFailedQueueKeepsItsIdOnlyForTheSameMessage() async {
        let chat = FakeChat()
        chat.local = true
        chat.queueFailure = LoginError.Api(code: "outbox.store_failed", message: "")
        let c = composer(chat)
        let reply = msg("q", "quoted")
        c.reply(to: reply)
        c.text = "hello"
        await c.send()
        XCTAssertEqual(c.text, "hello", "the text came back")
        XCTAssertEqual(c.replyingTo?.id, "q")
        await c.send() // the same message again
        c.cancel() // the quote dropped
        c.text = "hello"
        await c.send()
        let rows = chat.queued.withLock { $0 }.map { $0.split(separator: "|").map(String.init) }
        XCTAssertEqual(rows[0][0], rows[1][0], "a retry of the same message got a new id")
        XCTAssertNotEqual(rows[1][0], rows[2][0], "a changed quote reused the id")
        XCTAssertEqual(rows[2][2], "-")
    }

    func testWithoutLocalDataItSendsDirectlyAsBefore() async {
        let chat = FakeChat()
        let c = composer(chat)
        c.text = "hello"
        await c.send()
        XCTAssertEqual(chat.sent.withLock { $0 }, ["hello|-"])
        XCTAssertTrue(chat.queued.withLock { $0 }.isEmpty)
    }
}
