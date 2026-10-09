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
    /// The visible area in content coordinates, from the scroll geometry. `nearTop` (within one
    /// screen of the top, `ScrollToLatest.isNearTop`) is derived from it. The older page is asked for
    /// when `nearTop` turns true, not by a view appearing: a view that stays on screen while a page
    /// lands never appears again, and asking from `.task(id:)` chained through every page.
    @State private var visible = CGRect.zero
    /// The user is dragging, or the view is decelerating or animating a scroll.
    @State private var scrolling = false
    private var nearTop: Bool {
        ScrollToLatest.isNearTop(visibleMinY: visible.minY, viewportHeight: visible.height)
    }
    /// The running older-page ask, so leaving the screen can cancel it.
    @State private var olderTask: Task<Void, Never>?
    /// What the scroll geometry reports, in one value so `old` and `new` are one consistent pair.
    private struct Metrics: Equatable {
        let content: CGFloat
        let visible: CGRect
    }
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
            .onScrollGeometryChange(for: Metrics.self) { geometry in
                Metrics(content: geometry.contentSize.height, visible: geometry.visibleRect)
            } action: { old, new in
                visible = new.visible
                // Only the turn from far to near asks: one page per arrival at the top.
                let was = ScrollToLatest.isNearTop(visibleMinY: old.visible.minY, viewportHeight: old.visible.height)
                let now = ScrollToLatest.isNearTop(visibleMinY: new.visible.minY, viewportHeight: new.visible.height)
                if !was, now { askOlder(proxy) }
                // Taller content while at the bottom (a reaction on the last row, say) keeps the
                // bottom in view. "At the bottom" is judged on the geometry BEFORE this change, which
                // is `old`: the new geometry already counts the growth as scrolled away.
                let distance = old.content - old.visible.maxY
                if landed, ScrollToLatest.pinsToBottom(oldDistanceFromBottom: distance, oldContentHeight: old.content,
                                                       newContentHeight: new.content, scrolling: scrolling) {
                    restore(Self.bottomId, proxy, anchor: .bottom)
                }
            }
            // A short conversation is near the top from the start, so no turn ever happens: ask once,
            // when the first landing is done. One ask, never repeated by itself.
            .onChange(of: landed) { if nearTop { askOlder(proxy) } }
            // Not while the user is dragging or the view is decelerating or animating (see
            // `ScrollToLatest.pinsToBottom`).
            .onScrollPhaseChange { _, phase in scrolling = phase != .idle }
            // The loader can come back with nothing asking: a failed older page or newest page set
            // `olderFailed`/`headFailed`, a later successful re-read cleared it, and the user is still
            // at the top, so no turn from far to near happens. Ask when it turns true again.
            .onChange(of: timeline.offersOlder && !timeline.olderFailed && !timeline.headFailed) { _, ready in
                if ready, nearTop { askOlder(proxy) }
            }
            // The first page renders at offset 0 until `.task` below lands the view (it waits for
            // `load()`, which includes a read-receipt round trip). Go to the bottom as soon as the
            // page is there.
            .onChange(of: timeline.messages.isEmpty) { _, empty in
                if !empty, !landed { proxy.scrollTo(Self.bottomId, anchor: .bottom) }
            }
            .onChange(of: timeline.messages.last?.id) {
                // Before the first landing `.task` below decides where the view goes (a live message
                // can arrive before the first page and must not count as the landing).
                guard landed, let newest = timeline.messages.last else { return }
                // At the bottom a new message is followed; scrolled up only the user's own moves the
                // view (spec decision 3).
                let mine = newest.authorId == session.me
                if ScrollToLatest.follows(away: away, mine: mine) {
                    proxy.scrollTo(Self.bottomId, anchor: .bottom)
                }
            }
            // A re-read replaced the shown history: the old position means nothing, go to the newest.
            .onChange(of: timeline.replaced) {
                proxy.scrollTo(Self.bottomId, anchor: .bottom)
            }
            // The first landing: `start()` returns once `load()` has merged the newest page, so
            // the bottom exists now (a live message arriving before the page does not count).
            // Rows below the screen are not measured yet (the stack is lazy), so one scroll can stop
            // short: scroll, give layout a moment, scroll again, and only then let older pages load.
            // (`.defaultScrollAnchor(.bottom, for: .sizeChanges)` was not tried: it also pins to the
            // bottom when the user is reading further up.)
            .task {
                await session.start()
                proxy.scrollTo(Self.bottomId, anchor: .bottom)
                try? await Task.sleep(for: .milliseconds(100))
                proxy.scrollTo(Self.bottomId, anchor: .bottom)
                landed = true
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
        .onDisappear {
            olderTask?.cancel()
            session.stop()
        }
        // A plain call, not inside a `Task`: the session takes its gap anchor synchronously, so
        // nothing can be merged between the scene change and the anchor.
        .onChange(of: scenePhase) { old, new in session.sceneChanged(from: old, to: new) }
        // The channel left the list (deleted, or this user was removed): back to the list.
        .onChange(of: session.isRemoved, initial: true) { _, removed in
            if removed { dismiss() }
        }
    }

    /// Ask for one older page, unless one is already being asked for or the first landing has not
    /// happened. Called when the top comes near, not on every scroll: one page per arrival at the top.
    /// `retry` is the Retry button: the one ask allowed while the last page has failed.
    private func askOlder(_ proxy: ScrollViewProxy, retry: Bool = false) {
        guard landed, olderTask == nil, timeline.offersOlder, retry || !timeline.olderFailed else { return }
        olderTask = Task {
            let again = await loadOlderKeepingPlace(proxy)
            olderTask = nil
            // The user moved while the page was on its way and is at the top now: no turn from far
            // to near will happen, so ask as if they had just arrived. One call, not a loop: each
            // further ask needs a page that landed while they were moving again.
            if again { askOlder(proxy) }
        }
    }

    /// Reading position when an older page lands at the top: the new rows are inserted above the
    /// view, which stays at the same offset, so the user would see the new top rows. Note the first
    /// row and where the view was at the ask, and once the page is in, scroll that row back to the
    /// top. Only if the user has not moved since the ask (`stayedPut`), or the scroll would pull them
    /// back. No automatic re-ask: after a good restore the old first row is a page below the top,
    /// so `nearTop` is false and the next page needs the user to scroll up again.
    /// Returns true when a page landed, the user had moved (so nothing was restored), and the view
    /// is near the top now: the caller asks once more.
    @discardableResult
    private func loadOlderKeepingPlace(_ proxy: ScrollViewProxy) async -> Bool {
        guard let first = timeline.messages.first?.id else { return false }
        let askedAt = visible.minY
        await timeline.loadOlder()
        // Nothing was added (the start, or a failure): nothing to put back.
        guard timeline.messages.first?.id != first, !Task.isCancelled else { return false }
        guard ScrollToLatest.stayedPut(askedAt: askedAt, now: visible.minY) else { return nearTop }
        // After layout: the new rows are not measured in the same turn they are merged, and a
        // scroll then can land on nothing.
        await Task.yield()
        restore(first, proxy)
        // Check by geometry, and scroll once more if the first try did not take.
        try? await Task.sleep(for: .milliseconds(50))
        if nearTop, !Task.isCancelled { restore(first, proxy) }
        return false
    }

    private func restore(_ id: String, _ proxy: ScrollViewProxy, anchor: UnitPoint = .top) {
        var none = Transaction()
        none.disablesAnimations = true
        withTransaction(none) { proxy.scrollTo(id, anchor: anchor) }
    }

    /// Above the first message: the start of the conversation, the older-page loader, or its retry.
    @ViewBuilder private func top(_ proxy: ScrollViewProxy) -> some View {
        if timeline.atStart {
            Text("This is the start of the conversation.")
                .font(.caption).foregroundStyle(.secondary)
        } else if timeline.offersOlder {
            if timeline.olderFailed {
                Button("Couldn't load older messages. Retry") { askOlder(proxy, retry: true) }
                    .font(.caption)
            } else {
                // Asked for by `askOlder`, when the top comes near (see `nearTop`); this only shows
                // that a page is on its way.
                ProgressView().controlSize(.small)
                    .frame(maxWidth: .infinity)
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
