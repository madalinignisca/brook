import BrookCore
import XCTest

@testable import Brook

/// A message is read only when it can be seen: the app active (#145's note, as GTK #177).
@MainActor
final class ReadWhenActiveTests: XCTestCase {
    private final class Active: @unchecked Sendable { var value = true }

    private func settle() async { for _ in 0 ..< 10 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) } }

    func testInTheBackgroundAMessageIsOwedAndReadOnceActive() async {
        let chat = FakeChat()
        let active = Active()
        active.value = false
        let t = TimelineModel(channelId: "c", client: chat, isActive: { active.value })
        t.apply(.messageNew(message: msg("m1", "hi")))
        await settle()
        XCTAssertTrue(chat.read.isEmpty, "read while the app was in the background")
        XCTAssertTrue(t.readOwed)
        active.value = true
        t.appBecameActive()
        await settle()
        XCTAssertEqual(chat.read, ["m1"])
        XCTAssertFalse(t.readOwed)
    }

    func testInFrontItsReadAtOnce() async {
        let chat = FakeChat()
        let t = TimelineModel(channelId: "c", client: chat, isActive: { true })
        t.apply(.messageNew(message: msg("m1", "hi")))
        await settle()
        XCTAssertEqual(chat.read, ["m1"])
        XCTAssertFalse(t.readOwed)
    }

    func testTheOpenChannelsBadgeShowsWhileItsUnseen() async {
        let client = FakeRealtime(channels: [channel("c", "general")])
        let model = ChannelsModel(client: client)
        await model.start()
        model.openChannel = "c"
        let t = TimelineModel(channelId: "c", client: FakeChat(), isActive: { false })
        model.timeline = t
        model.handle(.messageNew(message: msg("m1", "hi")))
        let row = ChannelRow(id: "c", name: "general", unread: 1)
        XCTAssertEqual(model.unread(row), 1, "an unseen message in the open channel had no badge")
        t.appBecameActive()
        XCTAssertNil(model.unread(row))
    }
}
