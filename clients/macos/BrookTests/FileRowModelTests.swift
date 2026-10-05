import BrookCore
import CoreGraphics
import Synchronization
import XCTest

@testable import Brook

/// The attachment row: Open, keep offline, previews (spec 2026-09-26-mac-file-row §4).
@MainActor
final class FileRowModelTests: XCTestCase {
    private func info(_ type: String = "application/pdf", size: UInt64 = 1000) -> FfiFileInfo {
        FfiFileInfo(id: "f1", filename: "report.pdf", originalName: "Report.pdf", size: size,
                    contentType: type, sha256: "ab")
    }

    private final class Opened: @unchecked Sendable { var urls: [URL] = []; var result = true }
    private final class Decoded: @unchecked Sendable {
        var calls = 0; var image: CGImage?; var aliveSeen: Bool?
        /// The request's own liveness check, as the decoder's queue asks it before starting.
        var alive: (@Sendable () -> Bool)?
        /// Held until opened: the decode finishes when the test says.
        var gate: Gate?
    }

    /// A row on its own defaults suite, never the real ones (the test host is Brook.app).
    /// `previews`: the stored setting, nil for none stored (the default).
    private func model(_ chat: FakeChat, file: FfiFileInfo? = nil, exists: Bool = true, expensive: Bool = false,
                       opened: Opened = Opened(), decoded: Decoded = Decoded(),
                       previews: Bool? = true, decoderOff: Flag = Flag()) -> FileRowModel {
        let suite = "brook.tests.filerow.\(UUID().uuidString)"
        let defaults = UserDefaults(suiteName: suite)!
        addTeardownBlock { defaults.removePersistentDomain(forName: suite) }
        if let previews { defaults.set(previews, forKey: Settings.showImagePreviewsKey) }
        let m = FileRowModel(file: file ?? info(), client: chat,
                             opener: { opened.urls.append($0); return opened.result },
                             exists: { _ in exists }, expensive: { expensive },
                             decode: { _, alive in
                                 decoded.calls += 1
                                 decoded.alive = alive
                                 // As the decoder's queue does: from another thread.
                                 decoded.aliveSeen = await Task.detached { alive() }.value
                                 await decoded.gate?.wait()
                                 return decoded.image
                             },
                             defaults: defaults, decoderOff: { decoderOff.value })
        m.onScreen = true
        return m
    }

    private func local() -> FakeChat {
        let chat = FakeChat()
        chat.local = true
        return chat
    }

    private static let image: CGImage = {
        let ctx = CGContext(data: nil, width: 2, height: 2, bitsPerComponent: 8, bytesPerRow: 8,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!,
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
        return ctx.makeImage()!
    }()

    private func imageBytes() -> FfiImagePreview {
        FfiImagePreview(kind: .png, width: 2, height: 2, bytes: Data([1, 2, 3]))
    }

    // ---- Open ----

    func testOpenOpensExactlyThePathCoreReturned() async {
        let chat = local()
        let opened = Opened()
        let m = model(chat, opened: opened)
        await m.reloadKeep()
        await m.open()
        XCTAssertEqual(opened.urls, [URL(fileURLWithPath: "/private/brook/open/abc/report.pdf")])
        XCTAssertNil(m.message)
    }

    func testOpenErrorsSayWhatToDo() async {
        let cases: [(String, String)] = [
            ("file.open_refused", "Can't be opened from Brook. Save it instead"),
            ("file.unknown", "Not available yet. Try again in a moment"),
            ("file.gone", "No longer available."),
        ]
        for (code, text) in cases {
            let chat = local()
            chat.openResult = .failure(LoginError.Api(code: code, message: ""))
            let m = model(chat)
            await m.reloadKeep()
            await m.open()
            XCTAssertEqual(m.message, text, code)
            XCTAssertEqual(m.gone, code == "file.gone", code)
        }
    }

    func testAGoneCopyAndNoAppAreSaid() async {
        let chat = local()
        let gone = model(chat, exists: false)
        await gone.reloadKeep()
        await gone.open()
        XCTAssertEqual(gone.message, "Not available yet. Try again in a moment")
        let opened = Opened()
        opened.result = false
        let noApp = model(local(), opened: opened)
        await noApp.reloadKeep()
        await noApp.open()
        XCTAssertEqual(noApp.message, "No app opens this. Save it instead")
    }

    func testWithoutLocalDataThereIsNoOpenNoToggleNoPreview() async {
        let chat = FakeChat() // every call: local.unavailable
        let opened = Opened()
        let m = model(chat, file: info("image/png"), opened: opened)
        await m.reloadKeep()
        XCTAssertFalse(m.hasLocalData)
        await m.open()
        await m.startPreview()
        XCTAssertTrue(opened.urls.isEmpty)
        XCTAssertFalse(chat.cacheCalls.withLock { $0 }.contains("open"), "Open asked core without local data")
        guard case .none = m.preview else { return XCTFail("a preview without local data") }
    }

    // ---- Keep available offline ----

    func testTheToggleShowsTheCachesThreeStates() {
        XCTAssertEqual(FileRowModel.view(.notCached), .off)
        XCTAssertEqual(FileRowModel.view(.cached), .off)
        XCTAssertEqual(FileRowModel.view(.pinned(cached: false, done: 0, size: 9, transfer: 4)), .fetching(4))
        XCTAssertEqual(FileRowModel.view(.pinned(cached: true, done: 9, size: 9, transfer: nil)), .kept)
    }

    func testOneToggleAtATimeThenTheCachesWord() async {
        let chat = local()
        let gate = Gate()
        chat.pinGate = gate
        chat.states = [.notCached, .pinned(cached: false, done: 0, size: 9, transfer: nil)]
        let m = model(chat)
        await m.reloadKeep()
        let first = Task { await m.toggleKeep() }
        for _ in 0 ..< 20 { await Task.yield() }
        XCTAssertTrue(m.keepBusy)
        await m.toggleKeep() // a second click while busy: ignored
        gate.open()
        await first.value
        XCTAssertEqual(chat.pins.withLock { $0 }, ["pin"])
        XCTAssertEqual(m.keep, .fetching(nil))
        XCTAssertFalse(m.keepBusy)
    }

    func testAFailedToggleSaysWhyAndTakesTheCachesStateNotAGuess() async {
        let chat = local()
        chat.pinFailure = LoginError.Api(code: "file.unknown", message: "")
        // Meanwhile the file became kept (another device's pin landing, a Files event).
        chat.states = [.notCached, .pinned(cached: true, done: 9, size: 9, transfer: nil)]
        let m = model(chat)
        await m.reloadKeep()
        await m.toggleKeep()
        XCTAssertEqual(m.message, "Not available yet. Try again in a moment")
        XCTAssertEqual(m.keep, .kept, "the toggle was set back over a newer state")
    }

    func testAnOlderReadNeverOverwritesANewerOne() async {
        let chat = local()
        let gate = Gate()
        chat.stateGate = gate
        chat.states = [.notCached, .pinned(cached: true, done: 9, size: 9, transfer: nil)]
        let m = model(chat)
        let older = Task { await m.reloadKeep() } // held, answers notCached
        for _ in 0 ..< 20 { await Task.yield() }
        await m.reloadKeep() // newer: kept
        gate.open()
        await older.value
        XCTAssertEqual(m.keep, .kept)
    }

    func testAFilesEventRereadsRegisteredRowsAndGoneRowsDropOut() async {
        let chat = local()
        chat.states = [.notCached, .pinned(cached: true, done: 9, size: 9, transfer: nil)]
        let feed = CacheFeed(client: chat)
        let m = model(chat)
        feed.register(m)
        await m.reloadKeep()
        feed.handle(.files(ids: ["f1"]))
        for _ in 0 ..< 20 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        XCTAssertEqual(m.keep, .kept)
        var gone: FileRowModel? = model(chat)
        feed.register(gone!)
        XCTAssertEqual(feed.registeredRows, 2)
        gone = nil
        XCTAssertEqual(feed.registeredRows, 1, "a row that went stayed registered")
        _ = gone
    }

    // ---- Previews ----

    func testOnlyDeclaredImagesUpTo16MiBArePreviewable() {
        let chat = local()
        XCTAssertTrue(model(chat, file: info("image/png")).previewable)
        XCTAssertTrue(model(chat, file: info("image/jpeg; charset=binary")).previewable)
        XCTAssertFalse(model(chat, file: info("application/pdf")).previewable)
        XCTAssertFalse(model(chat, file: info("image/svg+xml")).previewable)
        XCTAssertFalse(model(chat, file: info("image/png", size: previewMaxBytes() + 1)).previewable)
    }

    func testSmallImagesPreviewByThemselvesAndLargerOnesWaitForAClick() async {
        let decoded = Decoded()
        decoded.image = Self.image
        let chat = local()
        chat.previewResult = .success(imageBytes())
        let small = model(chat, file: info("image/png", size: 1000), decoded: decoded)
        await small.reloadKeep()
        await small.startPreview()
        guard case .shown = small.preview else { return XCTFail("a small image didn't preview") }
        let large = model(chat, file: info("image/png", size: FileRowModel.autoMaxBytes + 1), decoded: decoded)
        await large.reloadKeep()
        await large.startPreview()
        guard case .offer = large.preview else { return XCTFail("a large image fetched by itself") }
        await large.showPreview()
        guard case .shown = large.preview else { return XCTFail("Show preview didn't show it") }
    }

    func testOnAnExpensiveConnectionOnlyAFileAlreadyHerePreviewsByItself() async {
        let decoded = Decoded()
        decoded.image = Self.image
        let chat = local()
        chat.previewResult = .success(imageBytes())
        chat.states = [.notCached]
        let away = model(chat, file: info("image/png"), expensive: true, decoded: decoded)
        await away.reloadKeep()
        await away.startPreview()
        guard case .offer = away.preview else { return XCTFail("fetched on an expensive connection") }
        chat.states = [.cached]
        let here = model(chat, file: info("image/png"), expensive: true, decoded: decoded)
        await here.reloadKeep()
        await here.startPreview()
        guard case .shown = here.preview else { return XCTFail("a cached image didn't preview") }
    }

    func testAnyFailureMeansNoPreview() async {
        let chat = local() // previewFile refuses
        let decoded = Decoded()
        let m = model(chat, file: info("image/png"), decoded: decoded)
        await m.reloadKeep()
        await m.startPreview()
        guard case .none = m.preview else { return XCTFail("a refused file showed something") }
        XCTAssertEqual(decoded.calls, 0)
        chat.previewResult = .success(imageBytes()) // bytes, but the decoder gives nothing
        let n = model(chat, file: info("image/png"), decoded: decoded)
        await n.reloadKeep()
        await n.startPreview()
        guard case .none = n.preview else { return XCTFail("a failed decode showed something") }
    }

    func testTheDecodersAliveCheckIsSafeOffTheMainThreadAndFollowsTheRow() async {
        let decoded = Decoded()
        decoded.image = Self.image
        let chat = local()
        chat.previewResult = .success(imageBytes())
        let m = model(chat, file: info("image/png"), decoded: decoded)
        await m.reloadKeep()
        await m.showPreview()
        XCTAssertEqual(decoded.aliveSeen, true)
        m.onScreen = false
        await m.showPreview() // an off-screen row asks for nothing
        XCTAssertEqual(decoded.calls, 1)
        XCTAssertFalse(chat.cacheCalls.withLock { $0 }.filter { $0 == "preview" }.count > 1)
    }

    // ---- The decisions, over every input ----

    func testCanPreviewNeedsADeclaredImageSmallEnoughLocalDataAndALiveDecoder() {
        let ok = (type: "image/png", size: UInt64(1000))
        func can(_ type: String = ok.type, size: UInt64 = ok.size, local: Bool = true, off: Bool = false) -> Bool {
            FileRowModel.canPreview(type: type, size: size, hasLocalData: local, decoderOff: off)
        }
        XCTAssertTrue(can())
        for type in ["image/png", "image/jpeg", "image/gif", "image/webp", "IMAGE/PNG; x=y"] { XCTAssertTrue(can(type), type) }
        for type in ["application/pdf", "image/svg+xml", "text/plain", ""] { XCTAssertFalse(can(type), type) }
        XCTAssertTrue(can(size: previewMaxBytes()))
        XCTAssertFalse(can(size: previewMaxBytes() + 1))
        XCTAssertFalse(can(local: false))
        XCTAssertFalse(can(off: true))
    }

    func testShouldAutoPreviewIsTheSettingAndCanPreviewAndSmallAndNotExpensiveUnlessHere() {
        let small = FileRowModel.autoMaxBytes
        // setting, size, type, local, decoderOff, expensive, here -> expected
        func auto(setting: Bool = true, size: UInt64 = small, type: String = "image/png", local: Bool = true,
                  off: Bool = false, expensive: Bool = false, here: Bool = false) -> Bool {
            FileRowModel.shouldAutoPreview(setting: setting, size: size, type: type, hasLocalData: local,
                                           decoderOff: off, expensive: expensive, here: here)
        }
        XCTAssertTrue(auto())
        XCTAssertFalse(auto(setting: false))
        XCTAssertFalse(auto(setting: false, here: true))
        XCTAssertFalse(auto(size: small + 1))
        XCTAssertFalse(auto(type: "application/pdf"))
        XCTAssertFalse(auto(local: false))
        XCTAssertFalse(auto(off: true))
        XCTAssertFalse(auto(expensive: true))
        XCTAssertTrue(auto(expensive: true, here: true))
        XCTAssertTrue(auto(here: true))
    }

    // ---- The "Show image previews" setting ----

    private func started(_ chat: FakeChat, _ type: String = "image/png", size: UInt64 = 1000, previews: Bool? = true,
                         decoded: Decoded = Decoded(), decoderOff: Flag = Flag(), expensive: Bool = false) async -> FileRowModel {
        let m = model(chat, file: info(type, size: size), expensive: expensive, decoded: decoded,
                      previews: previews, decoderOff: decoderOff)
        await m.reloadKeep()
        await m.startPreview()
        return m
    }

    private func previewCalls(_ chat: FakeChat) -> Int { chat.cacheCalls.withLock { $0 }.filter { $0 == "preview" }.count }

    func testWithTheSettingOffNothingIsFetchedOrDecodedUntilShowPreviewIsClicked() async {
        let decoded = Decoded()
        decoded.image = Self.image
        let chat = local()
        chat.previewResult = .success(imageBytes())
        chat.states = [.notCached, .notCached, .cached] // reloadKeep reads one; a second read would take another
        let m = await started(chat, previews: false, decoded: decoded)
        guard case .offer = m.preview else { return XCTFail("no button with the setting off") }
        XCTAssertEqual(previewCalls(chat), 0)
        XCTAssertEqual(decoded.calls, 0)
        XCTAssertEqual(chat.states.count, 2, "the cache was asked about a preview that wasn't wanted")
        await m.showPreview()
        guard case .shown = m.preview else { return XCTFail("the click didn't show it") }
        XCTAssertEqual(previewCalls(chat), 1)
        XCTAssertEqual(decoded.calls, 1)
    }

    func testAbsentSettingMeansOffAndAStoredOneIsReadWithoutBeingToldAnything() async {
        let decoded = Decoded()
        decoded.image = Self.image
        let chat = local()
        chat.previewResult = .success(imageBytes())
        let absent = await started(chat, previews: nil, decoded: decoded)
        guard case .offer = absent.preview else { return XCTFail("previews on by default") }
        let on = await started(chat, previews: true, decoded: decoded)
        guard case .shown = on.preview else { return XCTFail("a stored on was ignored") }
        let off = await started(chat, previews: false, decoded: decoded)
        guard case .offer = off.preview else { return XCTFail("a stored off was ignored") }
    }

    func testWithTheSettingOnTheRuleIsAsBefore() async {
        let decoded = Decoded()
        decoded.image = Self.image
        let chat = local()
        chat.previewResult = .success(imageBytes())
        chat.states = [.notCached]
        let large = await started(chat, size: FileRowModel.autoMaxBytes + 1, decoded: decoded)
        guard case .offer = large.preview else { return XCTFail("a large image fetched by itself") }
        let atLimit = await started(chat, size: FileRowModel.autoMaxBytes, decoded: decoded)
        guard case .shown = atLimit.preview else { return XCTFail("4 MiB didn't preview") }
        let away = await started(chat, decoded: decoded, expensive: true)
        guard case .offer = away.preview else { return XCTFail("fetched on an expensive connection") }
        chat.states = [.cached]
        let here = await started(chat, decoded: decoded, expensive: true)
        guard case .shown = here.preview else { return XCTFail("a cached image didn't preview") }
    }

    func testNoButtonWhereAPreviewCannotWork() async {
        for setting in [false, true] {
            let chat = local()
            chat.previewResult = .success(imageBytes())
            let decoded = Decoded()
            for (type, size) in [("application/pdf", UInt64(1000)), ("image/svg+xml", 1000), ("image/png", previewMaxBytes() + 1)] {
                let m = await started(chat, type, size: size, previews: setting, decoded: decoded)
                guard case .none = m.preview else { return XCTFail("\(type) \(size) offered, setting \(setting)") }
            }
            let dead = Flag()
            dead.set(true)
            let off = await started(chat, previews: setting, decoded: decoded, decoderOff: dead)
            guard case .none = off.preview else { return XCTFail("a dead decoder offered, setting \(setting)") }
            let noData = model(FakeChat(), file: info("image/png"), previews: setting)
            await noData.reloadKeep()
            await noData.startPreview()
            guard case .none = noData.preview else { return XCTFail("no local data offered, setting \(setting)") }
            XCTAssertEqual(previewCalls(chat), 0)
            XCTAssertEqual(decoded.calls, 0)
        }
    }

    func testADecoderThatDiesAfterTheButtonWasOfferedFetchesNothing() async {
        let chat = local()
        chat.previewResult = .success(imageBytes())
        let dead = Flag()
        let decoded = Decoded()
        let m = await started(chat, previews: false, decoded: decoded, decoderOff: dead)
        guard case .offer = m.preview else { return XCTFail("no button") }
        dead.set(true)
        await m.showPreview()
        guard case .none = m.preview else { return XCTFail("the button stayed for a dead decoder") }
        XCTAssertEqual(previewCalls(chat), 0)
    }

    func testTurningOffPutsEveryShownRowBackToAButtonAtOnceAndFetchesNothing() async {
        let decoded = Decoded()
        decoded.image = Self.image
        let chat = local()
        chat.previewResult = .success(imageBytes())
        let m = await started(chat, decoded: decoded)
        guard case .shown = m.preview else { return XCTFail("not shown to begin with") }
        await m.previewSetting(false)
        guard case .offer = m.preview else { return XCTFail("a shown row stayed shown") }
        XCTAssertEqual(previewCalls(chat), 1, "turning off fetched again")
        await m.previewSetting(false) // unchanged: nothing
        guard case .offer = m.preview else { return XCTFail("a repeat changed the row") }
    }

    func testAnImageThatArrivesAfterTurningOffIsDropped() async {
        // Held in the decoder.
        let decoded = Decoded()
        decoded.image = Self.image
        let gate = Gate()
        decoded.gate = gate
        let chat = local()
        chat.previewResult = .success(imageBytes())
        let m = model(chat, file: info("image/png"), decoded: decoded)
        await m.reloadKeep()
        let loading = Task { await m.startPreview() }
        for _ in 0 ..< 40 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        guard case .loading = m.preview else { return XCTFail("not decoding yet") }
        await m.previewSetting(false)
        guard case .offer = m.preview else { return XCTFail("a loading row stayed loading") }
        gate.open()
        await loading.value
        guard case .offer = m.preview else { return XCTFail("a late image was shown after turning off") }

        // Held in the fetch.
        let fetchGate = Gate()
        let chat2 = local()
        chat2.previewResult = .success(imageBytes())
        chat2.previewGate = fetchGate
        let decoded2 = Decoded()
        decoded2.image = Self.image
        let n = model(chat2, file: info("image/png"), decoded: decoded2)
        await n.reloadKeep()
        let fetching = Task { await n.startPreview() }
        for _ in 0 ..< 40 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        guard case .loading = n.preview else { return XCTFail("not fetching yet") }
        await n.previewSetting(false)
        fetchGate.open()
        await fetching.value
        guard case .offer = n.preview else { return XCTFail("a late fetch changed the row after turning off") }
        XCTAssertEqual(decoded2.calls, 0, "a fetch that arrived after turning off was decoded")
    }

    /// A decode waiting in the decoder's queue asks `alive` before it starts: turning previews off
    /// must end that request, not only hide its result.
    func testTurningOffEndsTheDecodeRequestSoQueuedWorkIsDropped() async {
        let decoded = Decoded()
        decoded.image = Self.image
        let gate = Gate()
        decoded.gate = gate
        let chat = local()
        chat.previewResult = .success(imageBytes())
        let m = model(chat, file: info("image/png"), decoded: decoded)
        await m.reloadKeep()
        let loading = Task { await m.startPreview() }
        for _ in 0 ..< 40 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        XCTAssertEqual(decoded.alive?(), true, "wanted while previews are on")
        await m.previewSetting(false)
        XCTAssertEqual(decoded.alive?(), false, "a queued decode would still start with previews off")
        gate.open()
        await loading.value
    }

    func testTurningOffWhileTheCacheIsAskedLeavesTheButton() async {
        let chat = local()
        chat.states = [.notCached]
        let lookup = Gate()
        let m = model(chat, file: info("image/png"))
        await m.reloadKeep()
        chat.stateGate = lookup // after reloadKeep: that call must not take the gate
        let starting = Task { await m.startPreview() }
        for _ in 0 ..< 40 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        await m.previewSetting(false)
        lookup.open()
        await starting.value
        guard case .offer = m.preview else { return XCTFail("no button after turning off during the lookup") }
        XCTAssertFalse(chat.cacheCalls.withLock { $0 }.contains("preview"), "something was fetched with previews off")
    }

    func testADecoderThatDiesDuringTheLookupOffersNoButton() async {
        let chat = local()
        chat.states = [.notCached]
        let lookup = Gate()
        let off = Flag()
        let m = model(chat, file: info("image/png"), decoderOff: off)
        await m.reloadKeep()
        chat.stateGate = lookup // after reloadKeep: that call must not take the gate
        let starting = Task { await m.startPreview() }
        for _ in 0 ..< 40 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        off.set(true)
        lookup.open()
        await starting.value
        guard case .none = m.preview else { return XCTFail("a button for a dead decoder") }
    }

    func testAFetchThatFailsAfterTurningOffLeavesTheButton() async {
        let gate = Gate()
        let chat = local()
        chat.previewResult = .failure(LoginError.Network(message: "offline"))
        chat.previewGate = gate
        let m = model(chat, file: info("image/png"))
        await m.reloadKeep()
        let fetching = Task { await m.startPreview() }
        for _ in 0 ..< 40 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        guard case .loading = m.preview else { return XCTFail("not fetching yet") }
        await m.previewSetting(false)
        gate.open()
        await fetching.value
        guard case .offer = m.preview else { return XCTFail("a failed fetch took the button away after turning off") }
    }

    func testANetworkFailureOnTheClickKeepsTheButtonAndARefusalDoesNot() async {
        let net = local()
        net.previewResult = .failure(LoginError.Network(message: "offline"))
        let a = await started(net, previews: false)
        await a.showPreview()
        guard case .offer = a.preview else { return XCTFail("a network blip took the button away") }

        let refused = local()
        refused.previewResult = .failure(LoginError.Api(code: "file.preview_refused", message: ""))
        let b = await started(refused, previews: false)
        await b.showPreview()
        guard case .none = b.preview else { return XCTFail("a refused preview kept its button") }
    }

    func testASecondClickWhileOneRunsStartsNoSecondFetch() async {
        let gate = Gate()
        let chat = local()
        chat.previewResult = .success(imageBytes())
        chat.previewGate = gate
        let decoded = Decoded()
        decoded.image = Self.image
        let m = model(chat, file: info("image/png"), decoded: decoded, previews: false)
        await m.reloadKeep()
        await m.startPreview()
        guard case .offer = m.preview else { return XCTFail("no button") }
        let first = Task { await m.showPreview() }
        for _ in 0 ..< 40 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        guard case .loading = m.preview else { return XCTFail("not fetching yet") }
        // The second click, while the first waits in the fetch: it must return at once (run apart, so a
        // missing guard fails the count below instead of waiting on the fetch's gate for good).
        let second = Task { await m.showPreview() }
        for _ in 0 ..< 40 { await Task.yield(); try? await Task.sleep(for: .milliseconds(5)) }
        XCTAssertEqual(previewCalls(chat), 1, "two fetches for one row")
        gate.open()
        await first.value
        await second.value
        XCTAssertEqual(chat.cacheCalls.withLock { $0 }.filter { $0 == "preview" }.count, 1, "two fetches for one row")
    }

    func testTurningOnDecidesEveryButtonAsOnOpeningAndLeavesShownRowsAlone() async {
        let decoded = Decoded()
        decoded.image = Self.image
        let chat = local()
        chat.previewResult = .success(imageBytes())
        chat.states = [.notCached]
        let small = await started(chat, previews: false, decoded: decoded)
        let large = await started(chat, size: FileRowModel.autoMaxBytes + 1, previews: false, decoded: decoded)
        let away = await started(chat, previews: false, decoded: decoded, expensive: true)
        // A preview the user asked for while previews were off: it must stay when they are turned on.
        let shown = await started(chat, previews: false, decoded: decoded)
        await shown.showPreview()
        guard case .shown = shown.preview else { return XCTFail("the clicked preview did not show") }
        XCTAssertEqual(previewCalls(chat), 1)
        for m in [small, large, away] {
            guard case .offer = m.preview else { return XCTFail("not a button before") }
            await m.previewSetting(true)
        }
        guard case .shown = small.preview else { return XCTFail("a small image stayed a button") }
        guard case .offer = large.preview else { return XCTFail("a large image was fetched by turning on") }
        guard case .offer = away.preview else { return XCTFail("fetched on an expensive connection") }
        await shown.previewSetting(true) // off to on, with a preview already shown
        guard case .shown = shown.preview else { return XCTFail("a shown row changed") }
        XCTAssertEqual(previewCalls(chat), 2, "only the small one was fetched, the shown one not again")
    }

    func testTheSettingNeverPinsUnpinsOpensOrDeletes() async {
        let chat = local()
        chat.previewResult = .success(imageBytes())
        let decoded = Decoded()
        decoded.image = Self.image
        let m = await started(chat, previews: false, decoded: decoded)
        await m.previewSetting(true)
        await m.previewSetting(false)
        await m.showPreview()
        XCTAssertTrue(chat.pins.withLock { $0 }.isEmpty)
        XCTAssertFalse(chat.cacheCalls.withLock { $0 }.contains("open"))
        XCTAssertEqual(m.keep, .off)
    }
}
