import BrookCore
import XCTest
@testable import Brook

private func person(_ id: String, _ handle: String, _ name: String) -> FfiMember {
    FfiMember(id: id, handle: handle, displayName: name, role: nil)
}

final class ChannelTitleTests: XCTestCase {
    func testANamedChannelKeepsItsName() {
        let row = ChannelRow(id: "c", name: "general", members: [person("me", "me", "Me"), person("b", "bob", "Bob")])
        XCTAssertEqual(ChannelTitle.of(row, me: "me"), "general")
    }

    func testADMIsTheOtherPersonsDisplayName() {
        let row = ChannelRow(id: "c", name: nil, members: [person("me", "me", "Me"), person("b", "bob", "Bob R")])
        XCTAssertEqual(ChannelTitle.of(row, me: "me"), "Bob R")
        XCTAssertEqual(ChannelTitle.of(row, me: "b"), "Me", "the other side sees the first member")
    }

    func testABlankDisplayNameFallsBackToTheHandle() {
        let row = ChannelRow(id: "c", name: nil, members: [person("me", "me", "Me"), person("b", "bob", "")])
        XCTAssertEqual(ChannelTitle.of(row, me: "me"), "bob")
    }

    func testAnEmptyNameCountsAsNoNameAndYourOwnDMUsesYou() {
        let row = ChannelRow(id: "c", name: "", members: [person("me", "me", "Me")])
        XCTAssertEqual(ChannelTitle.of(row, me: "me"), "Me")
    }

    func testNoMembersKnownStillSaysDirectMessage() {
        XCTAssertEqual(ChannelTitle.of(ChannelRow(id: "c", name: nil, members: []), me: "me"), "Direct message")
        XCTAssertEqual(ChannelTitle.of(ChannelRow(id: "c", name: nil, members: [person("b", "bob", "Bob")]), me: nil), "Bob")
    }
}
