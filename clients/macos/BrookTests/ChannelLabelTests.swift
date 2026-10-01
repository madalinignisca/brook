import BrookCore
import XCTest
@testable import Brook

private func person(_ id: String, _ handle: String, _ name: String) -> FfiMember {
    FfiMember(id: id, handle: handle, displayName: name, role: nil)
}

private func dm(_ id: String, with other: FfiMember) -> FfiChannel {
    FfiChannel(id: id, kind: "dm", name: nil, archived: false, topic: nil, isPublic: false, unreadMentions: 0,
               members: [person("me", "me", "Me"), other], ownerOffers: [])
}

/// What a conversation is called in the sidebar, the window title and notifications (#219).
@MainActor
final class ChannelLabelTests: XCTestCase {
    private var defaults: UserDefaults!
    private var suite: String!

    override func setUp() {
        suite = "brook.tests.\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
    }

    private func started(_ channels: [FfiChannel]) async -> (FakeRealtime, ChannelsModel) {
        let client = FakeRealtime(channels: channels)
        let model = ChannelsModel(client: client, me: "me", isActive: { true }, defaults: defaults)
        await model.start()
        return (client, model)
    }

    func testAChannelReadsHashNameInTheSidebarAndTheTitle() async {
        let (_, model) = await started([channel("c1", "general")])
        XCTAssertEqual(model.channels[0].label, "#general")
        XCTAssertEqual(model.title(model.channels[0]), "#general")
    }

    func testADMReadsTheOtherPersonsDisplayNameAndTheHandleWhenShowingUsernames() async {
        let (_, model) = await started([dm("d1", with: person("b", "bob", "Bob R"))])
        XCTAssertEqual(model.title(model.channels[0]), "Bob R")
        model.showUsernames = true
        XCTAssertEqual(model.title(model.channels[0]), "@bob", "the title follows the preference too")
    }

    func testABlankDisplayNameReadsAtHandleEitherWay() async {
        let (_, model) = await started([dm("d1", with: person("b", "bob", "  "))])
        XCTAssertEqual(model.channels[0].label, "@bob")
        model.showUsernames = true
        XCTAssertEqual(model.channels[0].label, "@bob")
    }

    func testShowUsernamesIsOffOnAFreshInstall() async {
        XCTAssertFalse(Settings(defaults: defaults, environment: [:]).showUsernames)
        let (_, model) = await started([dm("d1", with: person("b", "bob", "Bob R"))])
        XCTAssertFalse(model.showUsernames)
        XCTAssertEqual(model.channels[0].label, "Bob R")
    }

    func testThePreferenceInDefaultsIsReadAtLaunch() async {
        defaults.set(true, forKey: Settings.showUsernamesKey)
        let (_, model) = await started([dm("d1", with: person("b", "bob", "Bob R"))])
        XCTAssertEqual(model.channels[0].label, "@bob")
    }

    /// Display-name order (Amy, Zed) differs from handle order (amy, zoe -> Zed first):
    /// flipping the preference relabels at once and leaves every row where it was.
    func testTogglingRelabelsAtOnceAndMovesNoRow() async {
        let (_, model) = await started([
            dm("d1", with: person("a", "zoe", "Amy")),
            dm("d2", with: person("z", "amy", "Zed")),
        ])
        XCTAssertEqual(model.channels.map(\.id), ["d1", "d2"])
        XCTAssertEqual(model.channels.map(\.label), ["Amy", "Zed"])
        model.showUsernames = true
        XCTAssertEqual(model.channels.map(\.label), ["@zoe", "@amy"])
        XCTAssertEqual(model.channels.map(\.id), ["d1", "d2"], "the preference moved a row")
    }

    func testAChannelUpdateRenameShowsAtOnce() async {
        let (client, model) = await started([channel("c1", "general")])
        client.deliver(.channelUpdate(channel: channel("c1", "renamed")))
        await drainMain()
        XCTAssertEqual(model.channels[0].label, "#renamed")
    }

    func testAnUpdatedDMKeepsTheCurrentPreference() async {
        let (client, model) = await started([dm("d1", with: person("b", "bob", "Bob R"))])
        model.showUsernames = true
        client.deliver(.channelUpdate(channel: dm("d1", with: person("b", "bob", "Robert"))))
        await drainMain()
        XCTAssertEqual(model.channels[0].label, "@bob")
        model.showUsernames = false
        XCTAssertEqual(model.channels[0].label, "Robert")
    }

    func testAnUnknownIdentityJoinsTheMembersAndNoMembersSaysDirectMessage() async {
        let client = FakeRealtime(channels: [dm("d1", with: person("b", "bob", "Bob")), 
                                             FfiChannel(id: "d2", kind: "dm", name: nil, archived: false, topic: nil,
                                                        isPublic: false, unreadMentions: 0, members: [], ownerOffers: [])])
        let model = ChannelsModel(client: client, me: nil, defaults: defaults)
        await model.start()
        XCTAssertEqual(model.channels.map(\.label).sorted(), ["Direct message", "Me, Bob"])
    }
}
