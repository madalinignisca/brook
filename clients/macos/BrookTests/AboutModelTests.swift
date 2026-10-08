// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
@testable import Brook
import Synchronization
import XCTest

/// Records what the model asked `fetch` for and answers by call number (0 for the first), so a
/// test can let one call wait on a `FetchGate` while a later one answers at once.
private final class FakeFetch: @unchecked Sendable {
    typealias Handler = @Sendable (_ call: Int) async throws -> FfiServerInfo

    struct Call: Equatable {
        let address: String
        let allowInsecureHttp: Bool
    }

    private let recorded = Mutex<[Call]>([])
    private let handler: Handler

    init(_ handler: @escaping Handler) { self.handler = handler }

    var calls: [Call] { recorded.withLock { $0 } }

    var fetch: AboutModel.Fetch {
        { [self] address, insecure in
            let n = recorded.withLock { calls -> Int in
                calls.append(Call(address: address, allowInsecureHttp: insecure))
                return calls.count - 1
            }
            return try await handler(n)
        }
    }
}

/// Holds a fake fetch until the test lets it go (open before wait is fine).
private final class FetchGate: @unchecked Sendable {
    private let state = Mutex<(open: Bool, waiters: [CheckedContinuation<Void, Never>])>((false, []))

    func wait() async {
        await withCheckedContinuation { c in
            let resume = state.withLock { s -> Bool in
                if s.open { return true }
                s.waiters.append(c)
                return false
            }
            if resume { c.resume() }
        }
    }

    func release() {
        let waiting = state.withLock { s -> [CheckedContinuation<Void, Never>] in
            s.open = true
            defer { s.waiters = [] }
            return s.waiters
        }
        for c in waiting { c.resume() }
    }
}

private func info(_ version: String = "1.2.3", _ source: String = "https://git.example.com/brook") -> FfiServerInfo {
    FfiServerInfo(version: version, sourceUrl: source)
}

@MainActor
final class AboutModelTests: XCTestCase {
    private func model(
        _ fake: FakeFetch, target: AboutModel.Target = .server("https://chat.example.com"),
        insecure: Bool = false
    ) -> AboutModel {
        AboutModel(allowInsecureHTTP: { insecure }, target: { target }, fetch: fake.fetch)
    }

    /// Waits (bounded) until the fake has seen `count` calls.
    private func waitForCalls(_ fake: FakeFetch, _ count: Int) async {
        for _ in 0 ..< 500 where fake.calls.count < count { try? await Task.sleep(for: .milliseconds(2)) }
    }

    // MARK: target

    func testSignedInServerWinsOverTheTypedField() {
        let t = AboutModel.target(signedInServer: "https://a.example", typed: "https://b.example", remembered: nil)
        XCTAssertEqual(t, .server("https://a.example"))
    }

    func testEmptyOrWhitespaceFieldIsNone() {
        for typed in ["", "   \n"] {
            XCTAssertEqual(AboutModel.target(signedInServer: nil, typed: typed, remembered: "https://x.example"), .none)
        }
    }

    func testAddressSignInRefusesIsInvalid() {
        XCTAssertEqual(
            AboutModel.target(signedInServer: nil, typed: "https://u:p@x.example", remembered: nil), .invalid)
    }

    func testValidTypedAddressIsTheServerTrimmed() {
        XCTAssertEqual(
            AboutModel.target(signedInServer: nil, typed: " https://chat.example.com ", remembered: nil),
            .server("https://chat.example.com"))
    }

    // Mutant: drop the fallback comparison -> the first case is .server, red.
    func testFreshInstallPrefillIsNoServer() {
        XCTAssertEqual(
            AboutModel.target(signedInServer: nil, typed: Settings.fallbackServer, remembered: nil), .none)
        XCTAssertEqual(
            AboutModel.target(signedInServer: nil, typed: " https://localhost ", remembered: nil), .none)
        XCTAssertEqual(
            AboutModel.target(
                signedInServer: nil, typed: Settings.fallbackServer, remembered: "https://chat.example.com"),
            .server("https://localhost"))
    }

    // MARK: refresh

    func testLoadedKeepsCoresTextAndParsesTheURL() async {
        let source = "https://xn--bcher-kva.example/brook"
        let fake = FakeFetch { _ in info("1.2.3", source) }
        let model = model(fake)
        await model.refresh()
        XCTAssertEqual(
            model.line, .loaded(version: "1.2.3", sourceText: source, sourceURL: URL(string: source)!))
        XCTAssertEqual(fake.calls, [.init(address: "https://chat.example.com", allowInsecureHttp: false)])
    }

    // Mutant: on failure set .loaded with the upstream URL -> red.
    func testUnreachableShowsNoLinkAndNoFallback() async {
        let errors: [LoginError] = [.Network(message: "x"), .UnexpectedResponse, .InsecureServerUrl]
        for error in errors {
            let fake = FakeFetch { _ in throw error }
            let model = model(fake)
            await model.refresh()
            XCTAssertEqual(model.line, .failed, "\(error)")
        }
    }

    func testNoServerAsksNothing() async {
        let fake = FakeFetch { _ in info() }
        let model = model(fake, target: .none)
        await model.refresh()
        XCTAssertEqual(model.line, .noServer)
        XCTAssertTrue(fake.calls.isEmpty)
    }

    func testInvalidTargetFailsWithoutAsking() async {
        let fake = FakeFetch { _ in info() }
        let model = model(fake, target: .invalid)
        await model.refresh()
        XCTAssertEqual(model.line, .failed)
        XCTAssertTrue(fake.calls.isEmpty)
    }

    func testInsecureFlagIsPassedThrough() async {
        let fake = FakeFetch { _ in info() }
        let model = model(fake, insecure: true)
        await model.refresh()
        XCTAssertEqual(fake.calls.map(\.allowInsecureHttp), [true])
    }

    // Mutant: drop the generation check -> A's late answer wins, red.
    func testNewestOpenWins() async {
        let gate = FetchGate()
        let fake = FakeFetch { n in
            if n == 0 { await gate.wait(); return info("old", "https://old.example/src") }
            return info("new", "https://new.example/src")
        }
        let model = model(fake)
        let a = Task { await model.refresh() }
        await waitForCalls(fake, 1)
        await model.refresh()
        gate.release()
        await a.value
        XCTAssertEqual(
            model.line,
            .loaded(version: "new", sourceText: "https://new.example/src", sourceURL: URL(string: "https://new.example/src")!))
    }

    // Mutant: no generation check in the catch -> line == .failed, red.
    func testALateFailureDoesNotWin() async {
        let gate = FetchGate()
        let fake = FakeFetch { n in
            if n == 0 { await gate.wait(); throw CancellationError() }
            return info("new", "https://new.example/src")
        }
        let model = model(fake)
        let a = Task { await model.refresh() }
        await waitForCalls(fake, 1)
        await model.refresh()
        gate.release()
        await a.value
        XCTAssertEqual(
            model.line,
            .loaded(version: "new", sourceText: "https://new.example/src", sourceURL: URL(string: "https://new.example/src")!))
    }

    func testEveryRefreshFetchesAgain() async {
        let fake = FakeFetch { _ in info() }
        let model = model(fake)
        await model.refresh()
        await model.refresh()
        XCTAssertEqual(fake.calls.count, 2)
    }

    func testOpenBumpsTheRefreshKey() {
        let model = model(FakeFetch { _ in info() })
        XCTAssertEqual(model.opens, 0)
        model.open()
        XCTAssertEqual(model.opens, 1)
    }
}
