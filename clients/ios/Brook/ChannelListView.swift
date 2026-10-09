// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI

/// "You have N recovery codes left." after a sign-in with a recovery code, when 2 or fewer
/// remain (the Mac's wording and threshold). Nil otherwise.
func recoveryCodesWarning(left: UInt32?) -> String? {
    guard let left, left <= 2 else { return nil }
    return "You have \(left) recovery code\(left == 1 ? "" : "s") left."
}

/// The signed-in home: the channels and DMs in the model's order, kept live by the model's
/// event stream. Plain system List; a row pushes its conversation (`ConversationHost`).
struct ChannelListView: View {
    let store: SessionStore
    let session: SignedInSession
    /// The conversation's client: the same signed-in client the list uses.
    let client: any ChatClient
    @Environment(\.scenePhase) private var scenePhase
    @State private var confirmingSignOut = false
    private var channels: ChannelsModel { session.channels }

    var body: some View {
        NavigationStack {
            content
                // The path carries only the channel id, so a row leaving the list cannot leave a
                // stale value on the stack; the conversation looks its row up itself.
                .navigationDestination(for: String.self) { id in
                    ConversationHost(channelId: id, channels: channels, client: client, me: session.me)
                }
                .navigationTitle("Brook")
                .toolbar {
                    ToolbarItem(placement: .topBarTrailing) {
                        Button("Sign Out") { confirmingSignOut = true }
                    }
                }
                .confirmationDialog("Sign out of Brook?", isPresented: $confirmingSignOut, titleVisibility: .visible) {
                    Button("Sign Out", role: .destructive) { store.signOut() }
                    Button("Cancel", role: .cancel) {}
                } message: {
                    // Nothing for the common case: a plain confirmation. The notice appears
                    // only when this launch cannot reach the saved sign-in (see `signOutNotice`).
                    if let notice = store.signOutNotice { Text(notice) }
                }
        }
        .task { await session.start() }
        .onDisappear { session.stop() }
        .onChange(of: scenePhase) { old, new in
            Task { await session.sceneChanged(from: old, to: new) }
        }
    }

    @ViewBuilder private var content: some View {
        if let error = channels.error, channels.channels.isEmpty {
            // The error takes the list's place. It sits in a List so pull to refresh still
            // works, which is how a failed first load is retried.
            List { Text(error).foregroundStyle(.secondary) }
                .refreshable { await channels.reloadList() }
        } else {
            List {
                if let warning = recoveryCodesWarning(left: store.recoveryCodesLeft) {
                    Label(warning, systemImage: "exclamationmark.triangle").foregroundStyle(.orange)
                }
                ForEach(channels.channels) { row in
                    NavigationLink(value: row.id) {
                        ChannelRowView(channels: channels, row: row)
                    }
                }
            }
            .refreshable { await channels.reloadList() }
        }
    }
}

private struct ChannelRowView: View {
    let channels: ChannelsModel
    let row: ChannelRow

    var body: some View {
        // One VoiceOver stop per row: title, then call and mentions, each with its own label.
        // No unread count here, on purpose: the server sends `unread_count` in `GET /channels`,
        // but the Apple binding's `FfiChannel` drops it (`bindings/apple/src/types.rs`), so only
        // `unread_mentions` can be shown. The Mac's count comes from its local cache, which iOS
        // does not have. Showing a count is its own issue (spec 2026-10-09-ios-conversation, §3).
        HStack {
            Text(channels.title(row))
            Spacer()
            if let count = channels.liveCalls[row.id] {
                // Spoken as a sentence; the visible glyph and number alone mean nothing to VoiceOver.
                Label("\(count)", systemImage: "phone.fill")
                    .foregroundStyle(.green)
                    .accessibilityElement(children: .ignore)
                    .accessibilityLabel("Call in progress, \(count) participant\(count == 1 ? "" : "s")")
            }
            if let mentions = channels.mentions(row) {
                Text("@\(mentions)").font(.caption.bold()).foregroundStyle(.red)
                    .accessibilityLabel("\(mentions) unread mention\(mentions == 1 ? "" : "s")")
            }
        }
        .accessibilityElement(children: .combine)
    }
}
