// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import Foundation
import Observation

/// What the About window shows about the server: its version and where its source code is
/// (AGPL section 13 asks a network service to offer that to the people using it). All the
/// logic lives here, with no UI and no network of its own, so BrookTests can cover it with an
/// injected `fetch`.
@MainActor
@Observable
final class AboutModel {
    enum Line: Equatable {
        case noServer
        case loading
        /// `sourceText` is core's string unchanged: it is what the view shows. `sourceURL` is
        /// only for opening it. Showing `sourceURL.absoluteString` instead would let
        /// Foundation re-parse and re-serialise the text, which could change what the user
        /// reads (core's form is the one the spec promises, e.g. an IDN host in punycode).
        case loaded(version: String, sourceText: String, sourceURL: URL)
        /// Carries nothing on purpose: a failure must never show a link, neither a cached
        /// one nor the upstream project's. A link that is not this server's own source would
        /// misstate where the running code comes from (spec section 4.4).
        case failed
    }

    enum Target: Equatable {
        case none
        case invalid
        case server(String)
    }

    enum Text {
        static let noServer = "No server yet."
        static let loading = "Fetching…"
        static let failed = "Couldn't fetch this server's source link."
        static let versionLabel = "Server version"
        static let sourceLabel = "Server source"
    }

    typealias Fetch = @Sendable (_ address: String, _ allowInsecureHttp: Bool) async throws -> FfiServerInfo

    static let live: Fetch = { try await serverInfo(baseUrl: $0, allowInsecureHttp: $1) }

    /// Which server About asks, in this order: the signed-in one; else what the sign-in screen's
    /// field holds. Pure, so the order is tested without a store.
    static func target(signedInServer: String?, typed: String, remembered: String?) -> Target {
        if let signedInServer { return .server(signedInServer) }
        let trimmed = typed.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed.isEmpty { return .none }
        // A fresh install: the field still holds the prefill (Settings.serverPrefill) and no
        // sign-in ever succeeded. That is not a server the user chose, so ask nothing. Once
        // `remembered` is set, a typed https://localhost is a real choice and is fetched.
        if remembered == nil, trimmed == Settings.fallbackServer { return .none }
        // The sign-in screen's own check, so About never contacts an address sign-in refuses.
        switch ServerAddress.parse(trimmed) {
        case .failure: return .invalid
        case .success: return .server(trimmed)
        }
    }

    private(set) var line: Line = .noServer
    /// Bumped by every `open()`; the view refreshes whenever it changes (`.task(id:)`).
    private(set) var opens = 0

    private var generation = 0
    private let allowInsecureHTTP: @MainActor () -> Bool
    private let target: @MainActor () -> Target
    private let fetch: Fetch

    /// The closures are main-actor: they read the store and the form, which live there.
    /// `target` is a closure so each refresh reads the live store and form (built in
    /// BrookApp.init()); tests pass a fixed one.
    init(
        allowInsecureHTTP: @escaping @MainActor () -> Bool, target: @escaping @MainActor () -> Target,
        fetch: @escaping Fetch = AboutModel.live
    ) {
        self.allowInsecureHTTP = allowInsecureHTTP
        self.target = target
        self.fetch = fetch
    }

    func open() { opens += 1 }

    /// Asks the server again. Every open fetches: a cached answer could name a server the user
    /// no longer uses.
    func refresh() async {
        generation += 1
        let mine = generation
        let address: String
        switch target() {
        case .none:
            line = .noServer
            return
        case .invalid:
            line = .failed
            return
        case let .server(a):
            address = a
        }
        line = .loading
        do {
            let info = try await fetch(address, allowInsecureHTTP())
            // A newer refresh owns `line` now. Checked before EVERY write after the await,
            // the catch below included: `.task(id:)` cancels an older run, whose fetch then
            // throws, and that late error must not overwrite the newer run's `.loaded`.
            guard generation == mine else { return }
            if let url = URL(string: info.sourceUrl) {
                line = .loaded(version: info.version, sourceText: info.sourceUrl, sourceURL: url)
            } else {
                line = .failed
            }
        } catch {
            guard generation == mine else { return }
            line = .failed
        }
    }
}
