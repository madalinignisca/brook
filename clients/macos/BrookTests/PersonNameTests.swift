import XCTest
@testable import Brook

/// How a person reads everywhere in the app (#238): one rule, so no surface can drift.
final class PersonNameTests: XCTestCase {
    func testLabelTable() {
        let rows: [(String?, String?, Bool, String)] = [
            ("Ann", "ann", false, "Ann"),
            ("Ann", "ann", true, "@ann"),
            (" Ann ", "ann", false, "Ann"),
            ("  ", "ann", false, "@ann"),
            (nil, "ann", false, "@ann"),
            ("Ann", nil, true, "Ann"),
            ("Ann", "", true, "Ann"),
            ("Ann", "  ", true, "Ann"),
            ("Ann", nil, false, "Ann"),
            (nil, nil, false, "Someone"),
            (nil, nil, true, "Someone"),
            ("", "", false, "Someone"),
            ("", "", true, "Someone"),
            (" ", nil, true, "Someone"),
        ]
        for (name, handle, on, want) in rows {
            XCTAssertEqual(PersonName.label(name, handle: handle, showUsernames: on), want,
                           "\(String(describing: name)) / \(String(describing: handle)) / \(on)")
        }
    }

    func testOtherTable() {
        let rows: [(String?, String?, Bool, String?)] = [
            ("Ann", "ann", false, "@ann"),
            ("Ann", "ann", true, "Ann"),
            (" Ann ", "ann", true, "Ann"),
            // The label is already "@ann": nothing new to add.
            ("", "ann", false, nil),
            ("  ", "ann", true, nil),
            (nil, "ann", true, nil),
            ("Ann", nil, false, nil),
            ("Ann", "", true, nil),
            (nil, nil, false, nil),
            // The name equals the label.
            ("@ann", "ann", false, nil),
        ]
        for (name, handle, on, want) in rows {
            XCTAssertEqual(PersonName.other(name, handle: handle, showUsernames: on), want,
                           "\(String(describing: name)) / \(String(describing: handle)) / \(on)")
        }
    }

    func testBothTable() {
        let rows: [(String?, String?, Bool, String)] = [
            ("Ann", "ann", false, "Ann (@ann)"),
            ("Ann", "ann", true, "@ann (Ann)"),
            ("", "ann", false, "@ann"),
            ("", "ann", true, "@ann"),
            ("Ann", nil, true, "Ann"),
            ("Ann", "", false, "Ann"),
            (nil, nil, false, "Someone"),
        ]
        for (name, handle, on, want) in rows {
            XCTAssertEqual(PersonName.both(name, handle: handle, showUsernames: on), want,
                           "\(String(describing: name)) / \(String(describing: handle)) / \(on)")
        }
    }
}
