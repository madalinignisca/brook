// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import AppKit
import Foundation
import Observation

/// The ring's sound (a fake in tests).
@MainActor
protocol Ringer: AnyObject {
    func start()
    func stop()
}

/// A system sound, repeated every few seconds while ringing; silent while the setting is off.
@MainActor
final class SystemRinger: Ringer {
    private var timer: Timer?
    private let defaults: UserDefaults

    init(defaults: UserDefaults = .standard) { self.defaults = defaults }

    func start() {
        stop()
        guard defaults.object(forKey: Settings.ringForCallsKey) as? Bool ?? true else { return }
        let play = { NSSound(named: "Submarine")?.play() }
        _ = play()
        timer = Timer.scheduledTimer(withTimeInterval: 3, repeats: true) { _ in _ = play() }
    }

    func stop() {
        timer?.invalidate()
        timer = nil
    }
}

/// When a DM call rings on this Mac (spec 2026-10-07-mac-incoming-call). One rule, re-checked on
/// every announcement and whenever what it depends on changes: a DM, a live call with one person in
/// it, this Mac not in or joining that channel's call, and that call not finished here (declined,
/// timed out, answered, or seen with both people in it).
@MainActor
@Observable
final class IncomingCalls {
    struct Ringing: Equatable {
        let channelId: String
        let callId: String
    }

    private(set) var ringing: Ringing?

    /// The last announcement per channel, kept so a call announced before the channel list is
    /// known still rings once the list says it is a DM.
    private var announced: [String: (callId: String, count: UInt32)] = [:]
    private var finished: Set<String> = []
    private var timeout: Task<Void, Never>?

    /// Whether a channel is a DM; nil while the list doesn't have it yet.
    var isDM: (String) -> Bool? = { _ in nil }
    /// The channel of this Mac's call, live or being joined: read at every check, never cached.
    var localChannel: () -> String? = { nil }
    /// Posts (true) or removes (false) the background notification for a channel.
    var alert: (_ channelId: String, _ on: Bool) -> Void = { _, _ in }

    private let ringer: any Ringer
    private let limit: Duration

    init(ringer: any Ringer, limit: Duration = .seconds(45)) {
        self.ringer = ringer
        self.limit = limit
    }

    /// A `channel.call` announcement.
    func observe(channelId: String, callId: String?, count: UInt32) {
        if let callId, count > 0 {
            announced[channelId] = (callId, count)
        } else {
            announced[channelId] = nil
        }
        reevaluate()
    }

    /// Something the rule reads changed (the channel list loaded, a join began).
    func reevaluate() {
        let local = localChannel()
        // Both people in: answered (here or elsewhere), so it stays answered, including when this Mac
        // leaves and the caller is alone in it again.
        for (_, call) in announced where call.count >= 2 { finished.insert(call.callId) }

        if let current = ringing, !rings(current.channelId, current.callId, local: local) {
            end(finishing: current)
        }
        guard ringing == nil else { return }
        for (channelId, call) in announced.sorted(by: { $0.key < $1.key })
        where rings(channelId, call.callId, local: local) {
            begin(Ringing(channelId: channelId, callId: call.callId))
            return
        }
    }

    private func rings(_ channelId: String, _ callId: String, local: String?) -> Bool {
        guard let call = announced[channelId], call.callId == callId, call.count == 1,
              isDM(channelId) == true, local != channelId, !finished.contains(callId)
        else { return false }
        return true
    }

    /// This Mac only: the caller isn't told.
    func decline() {
        if let ringing { end(finishing: ringing) }
    }

    /// Stops the ring and gives the call to join.
    func answer() -> Ringing? {
        guard let ringing else { return nil }
        end(finishing: ringing)
        return ringing
    }

    /// The session ended: no sound, timer or notification outlives it.
    func stop() {
        if let ringing { end(finishing: ringing) }
        announced = [:]
    }

    private func begin(_ r: Ringing) {
        ringing = r
        ringer.start()
        alert(r.channelId, true)
        let limit = limit
        timeout = Task { [weak self] in
            try? await Task.sleep(for: limit)
            guard !Task.isCancelled, let self, self.ringing == r else { return }
            self.end(finishing: r) // a missed ring: the sidebar's chip stays
        }
    }

    private func end(finishing r: Ringing) {
        finished.insert(r.callId)
        timeout?.cancel()
        timeout = nil
        ringing = nil
        ringer.stop()
        alert(r.channelId, false)
    }
}
