// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI

/// Builds the conversation's session once, then shows the conversation. Pushed by the list.
struct ConversationHost: View {
    let channelId: String
    let channels: ChannelsModel
    let client: any ChatClient
    let me: String
    /// Built once, in `.task`; building it in `init` would run again on every re-render of the
    /// parent. While it is nil the body must still show a real view (the `ProgressView`): `.task`
    /// is applied to the view's children, and a `Group` with no child has none. On a real iPhone a
    /// `.task` on an empty `Group` never ran (#335, see `SignedInHome`), and the simulator does not
    /// show it. Every lifecycle modifier in this file sits on a view that always exists.
    @State private var session: ConversationSession?

    var body: some View {
        Group {
            if let session {
                ConversationView(session: session)
            } else {
                ProgressView()
            }
        }
        // The guard matters: the task runs again when the child changes from the ProgressView to
        // ConversationView, and a second session would replace the first, which `stop()` would
        // then never reach.
        .task {
            if session == nil {
                session = ConversationSession(channelId: channelId, channels: channels, client: client, me: me)
            }
        }
    }
}

/// One conversation, read-only for now: the messages oldest at the top, older pages loading as you
/// scroll up, new ones arriving live. The view forwards to `ConversationSession`.
struct ConversationView: View {
    let session: ConversationSession
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.dismiss) private var dismiss
    /// Scrolled up by more than a little: new messages from others do not move the view, and the
    /// jump-to-latest button shows.
    @State private var away = false
    /// The first page has been scrolled to the bottom. Until then the older-page loader stays
    /// hidden: it sits at the top, and a page it loaded before the first landing would push the
    /// view away from the newest message.
    @State private var landed = false
    /// The older-page loader is on screen right now.
    @State private var loaderVisible = false
    private static let bottomId = "bottom"
    private var timeline: TimelineModel { session.timeline }

    var body: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 12) {
                    if landed { top(proxy) }
                    ForEach(timeline.messages, id: \.id) { message in
                        MessageRowView(
                            message: message,
                            author: timeline.authorName(message, showUsernames: session.channels.showUsernames),
                            me: session.me)
                            .id(message.id)
                    }
                    // Scrolling to the last row itself would leave it flush against the bottom edge;
                    // scrolling to this spacer keeps the gap (the Mac does the same, #285).
                    Color.clear.frame(height: ScrollToLatest.gap).id(Self.bottomId)
                }
                .scrollTargetLayout()
                .padding(.horizontal)
            }
            // Opens at the newest message.
            .defaultScrollAnchor(.bottom, for: .initialOffset)
            // `visibleRect` is the part of the content on screen, in content coordinates, so it
            // already leaves out the safe-area insets. `contentOffset` does not: it is negative by
            // the top inset at rest, which made "away" true even at the very bottom.
            .onScrollGeometryChange(for: Bool.self) { geometry in
                ScrollToLatest.isAway(contentHeight: geometry.contentSize.height,
                                      offset: geometry.visibleRect.minY,
                                      viewportHeight: geometry.visibleRect.height)
            } action: { _, isAway in
                away = isAway
            }
            .onChange(of: timeline.messages.last?.id) { old, _ in
                guard let newest = timeline.messages.last else { return }
                if old == nil {
                    // The first page. Rows below the screen are not measured yet (the stack is lazy),
                    // so one scroll can stop short: scroll, let layout settle, scroll again, and only
                    // then let the older-page loader appear.
                    proxy.scrollTo(Self.bottomId, anchor: .bottom)
                    Task {
                        try? await Task.sleep(for: .milliseconds(150))
                        proxy.scrollTo(Self.bottomId, anchor: .bottom)
                        landed = true
                    }
                    return
                }
                // The first page always goes to the bottom; later, the follow rule decides: at the
                // bottom a new message is followed, scrolled up only the user's own moves the view.
                let mine = newest.authorId == session.me
                if ScrollToLatest.follows(away: away, mine: mine) {
                    proxy.scrollTo(Self.bottomId, anchor: .bottom)
                }
            }
            // A re-read replaced the shown history: the old position means nothing, go to the newest.
            .onChange(of: timeline.replaced) {
                proxy.scrollTo(Self.bottomId, anchor: .bottom)
            }
            .overlay(alignment: .bottomTrailing) {
                if away {
                    Button {
                        withAnimation { proxy.scrollTo(Self.bottomId, anchor: .bottom) }
                    } label: {
                        Image(systemName: "chevron.down").font(.body.weight(.semibold))
                            .frame(width: 36, height: 36)
                            .background(.ultraThinMaterial, in: Circle())
                    }
                    .buttonStyle(.plain)
                    .padding(12)
                    .accessibilityLabel("Jump to the latest message")
                    .transition(.opacity)
                }
            }
            .animation(.easeInOut(duration: 0.15), value: away)
        }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if let error = timeline.visibleError {
                Text(error).foregroundStyle(.red).font(.caption)
                    .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal).padding(.vertical, 4)
                    .background(.bar)
            }
        }
        .navigationTitle(session.title)
        .navigationBarTitleDisplayMode(.inline)
        // The lifecycle modifiers sit here, on the scroll view, which always exists (see
        // `ConversationHost` for why that matters).
        .task { await session.start() }
        .onDisappear { session.stop() }
        // A plain call, not inside a `Task`: the session takes its gap anchor synchronously, so
        // nothing can be merged between the scene change and the anchor.
        .onChange(of: scenePhase) { old, new in session.sceneChanged(from: old, to: new) }
        // The channel left the list (deleted, or this user was removed): back to the list.
        .onChange(of: session.isRemoved, initial: true) { _, removed in
            if removed { dismiss() }
        }
    }

    /// Reading position when an older page lands at the top: the new rows would push the view, so
    /// note the first row, and put it back at the top once they are in (the plan's fallback; no
    /// transaction without animation, so the page appears without a visible move).
    private func loadOlderKeepingPlace(_ proxy: ScrollViewProxy) async {
        let first = timeline.messages.first?.id
        await timeline.loadOlder()
        // Nothing was added (the start, or a failure): nothing to put back.
        guard let first, timeline.messages.first?.id != first else { return }
        var none = Transaction()
        none.disablesAnimations = true
        withTransaction(none) { proxy.scrollTo(first, anchor: .top) }
    }

    /// Above the first message: the start of the conversation, the older-page loader, or its retry.
    @ViewBuilder private func top(_ proxy: ScrollViewProxy) -> some View {
        if timeline.atStart {
            Text("This is the start of the conversation.")
                .font(.caption).foregroundStyle(.secondary)
        } else if timeline.offersOlder {
            if timeline.olderFailed {
                Button("Couldn't load older messages. Retry") { Task { await loadOlderKeepingPlace(proxy) } }
                    .font(.caption)
            } else {
                // Asks once, when it appears: the model keeps a spinner from ever being left
                // without an answer (see `loadOlder`).
                ProgressView().controlSize(.small)
                    .frame(maxWidth: .infinity)
                    .onAppear { loaderVisible = true }
                    .onDisappear { loaderVisible = false }
                    // `.onAppear` alone asks once: when a page is short enough that the loader is
                    // still on screen after it lands, it never appears again and the next page is
                    // never asked for. So the ask is tied to the first row (it changes with every
                    // page) and repeated while the loader is visible. The wait lets the scroll back
                    // to the reading place settle first: if that took the loader off screen, stop.
                    .task(id: timeline.messages.first?.id) {
                        try? await Task.sleep(for: .milliseconds(150))
                        guard loaderVisible, !Task.isCancelled else { return }
                        await loadOlderKeepingPlace(proxy)
                    }
            }
        }
    }
}

/// One message, drawn for iOS from the timeline's data and the shared text helpers. Read-only:
/// reactions and files show but have no actions yet.
private struct MessageRowView: View {
    let message: FfiMessage
    /// The author's current name, already through `PersonName` (Show usernames).
    let author: String
    let me: String

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(author).bold()
                Text(MessageText.time(message.createdAt)).font(.caption).foregroundStyle(.secondary)
                if message.editedAt != nil, !message.deleted {
                    Text("edited").font(.caption).foregroundStyle(.secondary)
                }
            }
            if let quote = message.replyTo, !message.deleted {
                Text("↳ Replying to \(MessageText.excerpt(quote))")
                    .font(.caption).foregroundStyle(.secondary).lineLimit(1)
            }
            if message.deleted {
                Text("Message deleted").italic().foregroundStyle(.secondary)
            } else if !message.body.isEmpty {
                // Long-press gives the system's Copy.
                Text(message.body).textSelection(.enabled)
            } else if message.attachments.isEmpty {
                Text("Files removed").italic().foregroundStyle(.secondary)
            }
            ForEach(message.attachments, id: \.id) { file in
                HStack(spacing: 8) {
                    Image(systemName: MessageText.fileIcon(file.contentType))
                    VStack(alignment: .leading, spacing: 0) {
                        Text(file.originalName).lineLimit(1)
                        Text(ByteCountFormatter.string(fromByteCount: Int64(file.size), countStyle: .file))
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
            }
            if !message.deleted, !message.reactions.isEmpty {
                HStack(spacing: 8) {
                    ForEach(message.reactions, id: \.emoji) { reaction in
                        Text("\(reaction.emoji) \(reaction.count)").font(.callout)
                            .foregroundStyle(reaction.me ? Color.accentColor : .secondary)
                            .accessibilityLabel("\(reaction.emoji), \(reaction.count)\(reaction.me ? ", including you" : "")")
                    }
                }
            }
        }
        // A message that mentions you is tinted, as on the Mac.
        .padding(.vertical, 2).padding(.horizontal, 6)
        .background(NotificationPlanner.mentions(message, me: me) ? Color.accentColor.opacity(0.12) : .clear,
                    in: RoundedRectangle(cornerRadius: 6))
        .frame(maxWidth: .infinity, alignment: .leading)
    }
}
