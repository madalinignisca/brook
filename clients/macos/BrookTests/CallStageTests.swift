import XCTest

@testable import Brook

/// What fills the call window: a shared screen, or the tile the user pinned (a grid when neither).
final class CallStageTests: XCTestCase {
    private func tile(_ id: String, screen: Bool = false) -> CallModel.Tile {
        CallModel.Tile(id: id, name: id, isSelf: false, isScreen: screen, audio: true, video: true, track: nil)
    }

    func testASharedScreenTakesTheStageAndTheRestWaitInAStrip() {
        let split = CallStage.split([tile("a"), tile("p2.screen", screen: true), tile("b")], pinned: nil)
        XCTAssertEqual(split.stage?.id, "p2.screen")
        XCTAssertEqual(split.strip.map(\.id), ["a", "b"], "the strip keeps the order and drops the stage")
    }

    func testWithoutAScreenOrAPinThereIsNoStageAndTheGridKeepsEveryone() {
        let split = CallStage.split([tile("a"), tile("b")], pinned: nil)
        XCTAssertNil(split.stage)
        XCTAssertEqual(split.strip.map(\.id), ["a", "b"])
    }

    func testAPinBeatsTheScreenAndPinsAnyTile() {
        let tiles = [tile("a"), tile("p2.screen", screen: true), tile("b")]
        XCTAssertEqual(CallStage.split(tiles, pinned: "b").stage?.id, "b")
        XCTAssertEqual(CallStage.split(tiles, pinned: "b").strip.map(\.id), ["a", "p2.screen"],
                       "the screen is still shown, in the strip")
        XCTAssertEqual(CallStage.split([tile("a"), tile("b")], pinned: "a").stage?.id, "a", "a pin works with no screen")
    }

    func testAPinForATileThatLeftIsIgnored() {
        let tiles = [tile("a"), tile("p2.screen", screen: true)]
        XCTAssertEqual(CallStage.split(tiles, pinned: "gone").stage?.id, "p2.screen", "falls back to the screen")
        XCTAssertNil(CallStage.split([tile("a")], pinned: "gone").stage, "and to the grid")
    }

    /// The stored pin is dropped when its tile goes, so it cannot return with the same id later.
    func testAPinIsDroppedForGoodWhenItsTileLeaves() {
        let withScreen = [tile("a"), tile("p2.screen", screen: true)]
        XCTAssertEqual(CallStage.pruned("p2.screen", tiles: withScreen), "p2.screen", "kept while the tile is there")
        let gone = CallStage.pruned("p2.screen", tiles: [tile("a"), tile("p3.screen", screen: true)])
        XCTAssertNil(gone, "dropped when the tile is gone")
        // The same person shares again: the old pin must not displace the other share.
        let again = [tile("a"), tile("p3.screen", screen: true), tile("p2.screen", screen: true)]
        XCTAssertEqual(CallStage.split(again, pinned: gone).stage?.id, "p3.screen")
        XCTAssertNil(CallStage.pruned(nil, tiles: again))
    }

    func testSharesKeepTheirArrivalOrderAndANewShareGoesLast() {
        XCTAssertEqual(CallStage.arrivalOrder(previous: [], current: ["p3"]), ["p3"])
        XCTAssertEqual(CallStage.arrivalOrder(previous: ["p3"], current: ["p2", "p3"]), ["p3", "p2"],
                       "p2 is earlier in the roster but arrived second")
        XCTAssertEqual(CallStage.arrivalOrder(previous: ["p3", "p2"], current: ["p2"]), ["p2"], "p3 stopped sharing")
        XCTAssertEqual(CallStage.arrivalOrder(previous: ["p2"], current: ["p2", "p9", "p4"]), ["p2", "p4", "p9"],
                       "several in one update are ordered by id")
        XCTAssertEqual(CallStage.arrivalOrder(previous: ["p3"], current: ["p3", "p2"]).first, "p3", "the first stays first")
        // p3 stops (an update without it), then shares again: it goes after the share still running.
        let stopped = CallStage.arrivalOrder(previous: ["p3", "p2"], current: ["p2"])
        XCTAssertEqual(CallStage.arrivalOrder(previous: stopped, current: ["p2", "p3"]), ["p2", "p3"])
    }

    func testClickingPinsAndClickingTheStageReleases() {
        XCTAssertEqual(CallStage.toggled(nil, clicked: "b", onStage: "p2.screen"), "b", "a strip tile is pinned")
        XCTAssertNil(CallStage.toggled("b", clicked: "b", onStage: "b"), "the pinned stage releases")
        XCTAssertEqual(CallStage.toggled(nil, clicked: "p2.screen", onStage: "p2.screen"), "p2.screen",
                       "an automatic stage is pinned, not released")
        XCTAssertEqual(CallStage.toggled(nil, clicked: "a", onStage: nil), "a", "a grid tile goes on the stage")
    }
}
