import BrookCore
import Synchronization
import XCTest

@testable import Brook

final class FakeConversations: ConversationClient, @unchecked Sendable {
    let calls = Mutex<[String]>([])
    var failure: LoginError?
    var gate: Gate?
    var listed: [FfiChannel] = []
    var answer = channel("c9", "made")

    private func record(_ s: String) async throws {
        calls.withLock { $0.append(s) }
        await gate?.wait()
        if let failure { throw failure }
    }
    func openDm(handle: String) async throws -> FfiChannel { try await record("dm:\(handle)"); return answer }
    func createChannel(name: String, topic: String?, isPublic: Bool) async throws -> FfiChannel {
        try await record("create:\(name)|\(topic ?? "-")|\(isPublic)"); return answer
    }
    func listPublicChannels() async throws -> [FfiChannel] { try await record("list"); return listed }
    func joinChannel(channelId: String) async throws -> FfiChannel { try await record("join:\(channelId)"); return answer }
    func addMember(channelId: String, handle: String) async throws { try await record("add:\(channelId):\(handle)") }
    func updateChannel(channelId: String, name: String?, topic: String?, archived: Bool?) async throws -> FfiChannel {
        try await record("update:\(channelId)|\(name ?? "-")|\(topic ?? "-")|\(archived.map(String.init) ?? "-")")
        return answer
    }
    func deleteChannel(channelId: String) async throws { try await record("delete:\(channelId)") }
}

private func api(_ code: String) -> LoginError { .Api(code: code, message: "") }

@MainActor
final class StartConversationTests: XCTestCase {
    func testTheHandleIsCleanedAndOpensTheDm() async {
        let client = FakeConversations()
        let model = StartConversationModel(myHandle: "me", client: client)
        model.handle = "  @@bob "
        await model.submit()
        XCTAssertEqual(client.calls.withLock { $0 }, ["dm:bob"])
        XCTAssertEqual(model.opened?.id, "c9")
        XCTAssertNil(model.error)
    }

    func testNothingIsSentForAnEmptyOrYourOwnHandle() async {
        let client = FakeConversations()
        let model = StartConversationModel(myHandle: "Me", client: client)
        model.handle = "  @ "
        XCTAssertEqual(model.problem, "Enter their handle.")
        await model.submit()
        model.handle = "@me"
        XCTAssertEqual(model.problem, "That's you.", "compared without case")
        await model.submit()
        XCTAssertEqual(client.calls.withLock { $0 }, [])
    }

    func testAnUnknownHandleAndOtherFailuresHaveTheirTexts() async {
        for (code, text) in [("validation.error", Handle.noOne), ("not_found", Handle.noOne),
                             ("boom", "Couldn't start the conversation. Try again.")] {
            let client = FakeConversations()
            client.failure = api(code)
            let model = StartConversationModel(myHandle: "me", client: client)
            model.handle = "bob"
            await model.submit()
            XCTAssertEqual(model.error, text, code)
            XCTAssertNil(model.opened, code)
        }
    }

    func testASecondTapWhileRunningSendsNothing() async {
        let client = FakeConversations()
        let gate = Gate()
        client.gate = gate
        let model = StartConversationModel(myHandle: "me", client: client)
        model.handle = "bob"
        let first = Task { await model.submit() }
        while !model.busy { await Task.yield() }
        await model.submit()
        gate.open()
        await first.value
        XCTAssertEqual(client.calls.withLock { $0 }, ["dm:bob"])
    }
}

@MainActor
final class NewChannelTests: XCTestCase {
    func testTheCallCarriesTheTrimmedNameTopicAndPublic() async {
        let client = FakeConversations()
        let model = NewChannelModel(client: client)
        model.name = "  general "
        model.topic = " all hands "
        model.isPublic = true
        await model.submit()
        XCTAssertEqual(client.calls.withLock { $0 }, ["create:general|all hands|true"])
        XCTAssertEqual(model.created?.id, "c9")
    }

    func testAnEmptyTopicIsNotSent() async {
        let client = FakeConversations()
        let model = NewChannelModel(client: client)
        model.name = "x"
        model.topic = "   "
        await model.submit()
        XCTAssertEqual(client.calls.withLock { $0 }, ["create:x|-|false"])
    }

    func testTheServersLimits() {
        let model = NewChannelModel(client: FakeConversations())
        XCTAssertEqual(model.problem, "Enter a name.")
        model.name = String(repeating: "a", count: 128)
        XCTAssertNil(model.problem)
        model.name = String(repeating: "a", count: 129)
        XCTAssertEqual(model.problem, "A name can be up to 128 characters.")
        model.name = "ok"
        model.topic = String(repeating: "t", count: 513)
        XCTAssertEqual(model.problem, "A topic can be up to 512 characters.")
        XCTAssertFalse(model.canSubmit)
    }

    func testRefusalsHaveTheirTextsAndOneCallAtATime() async {
        let client = FakeConversations()
        client.failure = api("authz.forbidden")
        let model = NewChannelModel(client: client)
        model.name = "x"
        await model.submit()
        XCTAssertEqual(model.error, "Only admins can create channels.")
        client.failure = api("boom")
        await model.submit()
        XCTAssertEqual(model.error, "Couldn't create the channel. Try again.")

        let gated = FakeConversations()
        let gate = Gate()
        gated.gate = gate
        let other = NewChannelModel(client: gated)
        other.name = "y"
        let first = Task { await other.submit() }
        while !other.busy { await Task.yield() }
        await other.submit()
        gate.open()
        await first.value
        XCTAssertEqual(gated.calls.withLock { $0 }.count, 1)
    }
}

@MainActor
final class PublicChannelsTests: XCTestCase {
    func testArchivedChannelsAreNotOffered() async {
        let client = FakeConversations()
        var old = channel("c2", "old")
        old.archived = true
        client.listed = [channel("c1", "open"), old]
        let model = PublicChannelsModel(client: client)
        await model.load()
        XCTAssertEqual(model.channels.map(\.id), ["c1"])
        XCTAssertTrue(model.loaded)
    }

    func testJoiningSelectsItAndRemovesItFromTheList() async {
        let client = FakeConversations()
        client.listed = [channel("c1", "a"), channel("c2", "b")]
        client.answer = channel("c2", "b")
        let model = PublicChannelsModel(client: client)
        await model.load()
        await model.join("c2")
        XCTAssertEqual(model.joined?.id, "c2")
        XCTAssertEqual(model.channels.map(\.id), ["c1"])
    }

    func testAFailedJoinKeepsTheRowAndSaysSo() async {
        let client = FakeConversations()
        client.listed = [channel("c1", "a")]
        let model = PublicChannelsModel(client: client)
        await model.load()
        client.failure = api("boom")
        await model.join("c1")
        XCTAssertEqual(model.error, "Couldn't join. Try again.")
        XCTAssertEqual(model.channels.map(\.id), ["c1"])
        XCTAssertNil(model.joined)
    }
}

@MainActor
final class ChannelManagementTests: XCTestCase {
    private func model(_ client: FakeConversations, kind: String = "channel", admin: Bool = false,
                       owner: Bool = true) -> ChannelManagementModel {
        let role: String? = owner ? "owner" : "member"
        let r = ChannelRow(id: "c1", kind: kind, name: "general",
                           members: [FfiMember(id: "me", handle: "me", displayName: "Me", role: role)],
                           topic: "old topic")
        return ChannelManagementModel(channel: r, powers: ChannelPowers(r, me: "me", isAdmin: admin), client: client)
    }

    func testOnlyOwnersAndAdminsManageAChannelAndNeverADm() async {
        XCTAssertTrue(model(FakeConversations()).canManage)
        XCTAssertFalse(model(FakeConversations(), owner: false).canManage)
        XCTAssertTrue(model(FakeConversations(), admin: true, owner: false).canManage)
        let client = FakeConversations()
        let dm = model(client, kind: "dm", admin: true)
        XCTAssertFalse(dm.canManage)
        await dm.delete()
        await dm.setArchived(true)
        await dm.addMember("bob")
        XCTAssertEqual(client.calls.withLock { $0 }, [])
    }

    func testEachActionSendsItsCall() async {
        let cases: [(String, (ChannelManagementModel) async -> Void)] = [
            ("add:c1:bob", { await $0.addMember(" @bob ") }),
            ("update:c1|new name|-|-", { await $0.rename(name: " new name ", topic: "old topic") }),
            ("update:c1|-|new topic|-", { await $0.rename(name: "general", topic: "new topic") }),
            ("update:c1|-|-|true", { await $0.setArchived(true) }),
            ("update:c1|-|-|false", { await $0.setArchived(false) }),
            ("delete:c1", { await $0.delete() }),
        ]
        for (call, act) in cases {
            let client = FakeConversations()
            let m = model(client)
            await act(m)
            XCTAssertEqual(client.calls.withLock { $0 }, [call], call)
            XCTAssertTrue(m.done, call)
        }
    }

    func testAnEmptyOrTooLongRenameSendsNothing() async {
        let client = FakeConversations()
        let m = model(client)
        await m.rename(name: "  ", topic: "")
        await m.rename(name: String(repeating: "a", count: 129), topic: "")
        await m.rename(name: "ok", topic: String(repeating: "t", count: 513))
        XCTAssertEqual(client.calls.withLock { $0 }, [])
    }

    func testTheRenameLimitsAreTheServers() {
        XCTAssertEqual(ChannelManagementModel.renameProblem(name: "", topic: ""), "Enter a name.")
        XCTAssertNil(ChannelManagementModel.renameProblem(name: String(repeating: "a", count: 128),
                                                          topic: String(repeating: "t", count: 512)))
        XCTAssertEqual(ChannelManagementModel.renameProblem(name: "ok", topic: String(repeating: "t", count: 513)),
                       "A topic can be up to 512 characters.")
    }

    func testErrorsAndAlreadyGone() async {
        for (code, text) in [("validation.error", Handle.noOne), ("authz.forbidden", "Only an owner or admin can add members."),
                             ("boom", "Couldn't do that. Try again.")] {
            let client = FakeConversations()
            client.failure = api(code)
            let m = model(client)
            await m.addMember("bob")
            XCTAssertEqual(m.error, text, code)
            XCTAssertFalse(m.done, code)
        }
        let gone = FakeConversations()
        gone.failure = api("not_found")
        let m = model(gone)
        await m.delete()
        XCTAssertTrue(m.done)
        XCTAssertNil(m.error)
    }

    func testOneActionAtATime() async {
        let client = FakeConversations()
        let gate = Gate()
        client.gate = gate
        let m = model(client)
        let first = Task { await m.delete() }
        while !m.busy { await Task.yield() }
        await m.setArchived(true)
        gate.open()
        await first.value
        XCTAssertEqual(client.calls.withLock { $0 }, ["delete:c1"])
    }
}

@MainActor
final class RevealTests: XCTestCase {
    func testAChannelTheListHasIsRevealed() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        let model = ChannelsModel(client: client)
        await model.start()
        client.channels = [channel("c1", "general"), channel("c2", "new")]
        let revealed = await model.reveal("c2")
        XCTAssertTrue(revealed)
    }

    /// Not in the first read, in the second: revealed (a `reveal` that read once would miss it).
    func testAChannelThatAppearsOnTheSecondReadIsRevealed() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        client.readQueue.withLock {
            $0 = [[channel("c1", "general")], // start
                  [channel("c1", "general")], // reveal, first read: not there yet
                  [channel("c1", "general"), channel("c2", "new")]] // second read
        }
        let model = ChannelsModel(client: client)
        await model.start()
        let revealed = await model.reveal("c2")
        XCTAssertTrue(revealed)
    }

    func testRevealReadsTheListOnlyTwiceBeforeGivingUp() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        let model = ChannelsModel(client: client)
        await model.start()
        let before = client.order.withLock { $0.filter { $0 == "list" }.count }
        let revealed = await model.reveal("nope")
        XCTAssertFalse(revealed)
        XCTAssertEqual(client.order.withLock { $0.filter { $0 == "list" }.count } - before, 2)
    }

    func testAChannelTheListNeverGetsIsNotRevealed() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        let model = ChannelsModel(client: client)
        await model.start()
        let revealed = await model.reveal("nope")
        XCTAssertFalse(revealed)
    }
}

@MainActor
final class ArchivedChannelTests: XCTestCase {
    func testAnArchivedChannelsComposerWritesNothing() async {
        let chat = FakeChat()
        let composer = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        composer.readOnly = true
        composer.reply(to: msg("m1", "hi"))
        XCTAssertNil(composer.replyingTo)
        composer.edit(msg("m1", "hi"))
        XCTAssertNil(composer.editing)
        XCTAssertEqual(composer.text, "", "Edit filled the field of a read-only channel")
        composer.text = "typed anyway"
        await composer.send()
        XCTAssertEqual(chat.sent.withLock { $0 }, [], "a message went into an archived channel")
        XCTAssertFalse(composer.canAttach)
    }

    func testUnarchivingLetsItWriteAgain() async {
        let chat = FakeChat()
        let composer = ComposerModel(channelId: "c", client: chat, onMessage: { _ in })
        composer.readOnly = true
        composer.readOnly = false
        composer.text = "hello"
        await composer.send()
        XCTAssertEqual(chat.sent.withLock { $0 }, ["hello|-"])
    }
}

@MainActor
final class ManagementRulesTests: XCTestCase {
    private func model(_ id: String) -> ChannelManagementModel {
        let row = ChannelRow(id: id, name: id, members: [FfiMember(id: "me", handle: "me", displayName: "Me", role: "owner")])
        return ChannelManagementModel(channel: row, powers: ChannelPowers(row, me: "me", isAdmin: false),
                                      client: FakeConversations())
    }

    func testAChannelGoingAwayClosesItsOwnSheetsAndConfirmation() {
        let m = model("c1")
        for sheet in [ConversationSheet.addMember, .rename] {
            let out = ManagementRules.channelClosed("c1", managing: m, sheet: sheet, confirming: .delete)
            XCTAssertNil(out.managing)
            XCTAssertNil(out.sheet, "\(sheet)")
            XCTAssertNil(out.confirming)
        }
    }

    /// The finished model is still held after an Add Member sheet is cancelled; a New Message sheet
    /// opened since must not be dismissed (it holds what was typed).
    func testAnUnrelatedSheetSurvivesTheChannelGoingAway() {
        let m = model("c1")
        for sheet in [ConversationSheet.newMessage, .newChannel, .browse] {
            let out = ManagementRules.channelClosed("c1", managing: m, sheet: sheet, confirming: nil)
            XCTAssertEqual(out.sheet, sheet, "an unrelated sheet was dismissed")
            XCTAssertNil(out.managing)
        }
    }

    func testAnotherChannelGoingAwayChangesNothing() {
        let m = model("c1")
        let out = ManagementRules.channelClosed("c2", managing: m, sheet: .rename, confirming: .delete)
        XCTAssertTrue(out.managing === m)
        XCTAssertEqual(out.sheet, .rename)
        XCTAssertEqual(out.confirming?.id, ManageConfirm.delete.id)
    }

    func testAFinishedCallOnlyDropsItsOwnModel() {
        let old = model("c1"), newer = model("c1")
        XCTAssertNil(ManagementRules.finished(old, managing: old))
        XCTAssertTrue(ManagementRules.finished(old, managing: newer) === newer, "it dropped a newer model")
        XCTAssertNil(ManagementRules.finished(old, managing: nil))
    }
}

@MainActor
final class RevealSupersededTests: XCTestCase {
    /// Reads superseded by newer ones apply nothing: two of them must not use up the attempts, so
    /// the third, which applies, finds the channel (the original two-read loop gave up here).
    func testSupersededReadsAreNotMisses() async {
        let client = FakeRealtime(channels: [channel("c1", "general"), channel("c2", "new")])
        let model = ChannelsModel(client: client)
        var reads = 0
        let revealed = await model.reveal("c2") {
            reads += 1
            return reads < 3 ? false : await model.reloadList() // the first two were superseded
        }
        XCTAssertTrue(revealed)
        XCTAssertEqual(reads, 3)
    }

    func testOnlyAppliedReadsThatLackTheChannelAreMisses() async {
        let client = FakeRealtime(channels: [channel("c1", "general")])
        let model = ChannelsModel(client: client)
        var reads = 0
        let revealed = await model.reveal("nope") { reads += 1; return await model.reloadList() }
        XCTAssertFalse(revealed)
        XCTAssertEqual(reads, 2, "two applied reads without it is the end")
    }

    func testItStopsAfterFourReadsEvenIfAllAreSuperseded() async {
        let model = ChannelsModel(client: FakeRealtime(channels: []))
        var reads = 0
        let revealed = await model.reveal("x") { reads += 1; return false }
        XCTAssertFalse(revealed)
        XCTAssertEqual(reads, 4)
    }
}
