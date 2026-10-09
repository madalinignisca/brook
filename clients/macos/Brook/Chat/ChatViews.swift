// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import AppKit
import BrookCore
import SwiftUI
import UniformTypeIdentifiers

/// A channel's conversation: its messages, and the box to write in.
struct ChatView: View {
    let channelId: String
    let me: String
    @State var timeline: TimelineModel
    @State var composer: ComposerModel
    @State var saves: SaveModel
    /// Unsent messages (with local data, #62).
    let pending: PendingModel?
    /// The cache's notices (a file's state changing reaches its row).
    let feed: CacheFeed?
    /// An archived channel is read-only.
    let archived: Bool
    private let client: any ChatClient
    @Environment(\.showUsernames) private var showUsernames
    /// Scrolled away from the newest message: offers the jump back (#284).
    @State private var awayFromLatest = false
    private static let bottomId = "bottom"

    init(channelId: String, me: String, client: any ChatClient, timeline: TimelineModel,
         pending: PendingModel? = nil, feed: CacheFeed? = nil, archived: Bool = false) {
        self.channelId = channelId
        self.archived = archived
        self.me = me
        self.pending = pending
        self.feed = feed
        self.client = client
        _timeline = State(initialValue: timeline)
        _composer = State(initialValue: Self.makeComposer(channelId: channelId, client: client,
                                                          timeline: timeline, pending: pending))
        _saves = State(initialValue: SaveModel(client: client))
    }

    /// The composer this view ships with. A static function so a test can build the very composer
    /// the Mac uses: the `importer` line is the one place that makes a pasted image a PNG file,
    /// and a test of a hand-built composer would not notice if it were dropped.
    static func makeComposer(channelId: String, client: any ChatClient, timeline: TimelineModel,
                             pending: PendingModel?) -> ComposerModel {
        let composer = ComposerModel(
            channelId: channelId, client: client, onMessage: { [weak timeline] in
                timeline?.merge([$0])
            })
        composer.pending = pending
        pending?.timeline = timeline
        composer.importer = ComposerModel.macImporter
        return composer
    }

    var body: some View {
        VStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        if timeline.atStart {
                            Text("This is the start of the conversation.")
                                .font(.caption).foregroundStyle(.secondary)
                        } else if timeline.offersOlder {
                            if timeline.olderFailed {
                                Button("Couldn't load older messages. Retry") { Task { await timeline.loadOlder() } }
                                    .buttonStyle(.link).font(.caption)
                            } else {
                                ProgressView().controlSize(.small)
                                    .onAppear { Task { await timeline.loadOlder() } }
                            }
                        }
                        ForEach(timeline.messages, id: \.id) { message in
                            MessageRow(message: message, author: timeline.authorName(message, showUsernames: showUsernames),
                                       mine: message.authorId == me, me: me, saves: saves, composer: composer,
                                       onReact: { emoji in Task { await timeline.toggleReaction(message, emoji: emoji) } },
                                       makeRow: makeRow)
                                .id(message.id)
                        }
                        if let pending {
                            ForEach(pending.visible, id: \.clientId) { unsent in
                                PendingRow(message: unsent, pending: pending)
                            }
                        }
                        // Scrolling to the last row itself would leave it flush against the composer;
                        // scrolling to this spacer keeps the gap (#285).
                        Color.clear.frame(height: ScrollToLatest.gap).id(Self.bottomId)
                    }
                    .padding([.horizontal, .top], 12)
                }
                .onScrollGeometryChange(for: Bool.self) { geometry in
                    ScrollToLatest.isAway(contentHeight: geometry.contentSize.height,
                                          offset: geometry.contentOffset.y,
                                          viewportHeight: geometry.containerSize.height)
                } action: { _, away in
                    awayFromLatest = away
                }
                .onChange(of: timeline.messages.last?.id) { _, newest in
                    if newest != nil { proxy.scrollTo(Self.bottomId, anchor: .bottom) }
                }
                .overlay(alignment: .bottomTrailing) {
                    if awayFromLatest {
                        Button {
                            withAnimation { proxy.scrollTo(Self.bottomId, anchor: .bottom) }
                        } label: {
                            Image(systemName: "chevron.down").font(.body.weight(.semibold))
                                .frame(width: 32, height: 32)
                                .background(.ultraThinMaterial, in: Circle())
                        }
                        .buttonStyle(.plain)
                        .opacity(0.8)
                        .padding(12)
                        .help("Jump to the latest message")
                        .accessibilityLabel("Jump to the latest message")
                        .transition(.opacity)
                    }
                }
                .animation(.easeInOut(duration: 0.15), value: awayFromLatest)
            }
            if let error = timeline.visibleError {
                Text(error).foregroundStyle(.red).font(.caption).padding(.horizontal)
            }
            // Re-evaluated every second, so "typing…" expires by itself.
            TimelineView(.periodic(from: .now, by: 1)) { context in
                if let line = timeline.typingLine(now: context.date, showUsernames: showUsernames) {
                    Text(line).font(.callout).foregroundStyle(.secondary)
                        .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 12)
                }
            }
            Divider()
            if let reactionError = timeline.reactionError {
                Text(reactionError).font(.callout).foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading).padding(.horizontal, 12)
                    // Gone by itself after a few seconds, or by the next reaction (the model's).
            }
            if archived {
                Text("This channel is archived. An owner or admin can unarchive it.")
                    .font(.callout).foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity).padding(10)
            } else {
                ComposerView(composer: composer)
            }
        }
        // Files dropped anywhere on the conversation join the next message (not while
        // editing, and only with this Mac's storage).
        .onDrop(of: [.fileURL], isTargeted: nil) { providers in
            guard composer.canAttach else { return false }
            Task { await composer.attach(dropped: providers) }
            return true
        }
        // Archived: nothing writes (the composer is replaced, and Reply, Edit and dropped files are off).
        .onChange(of: archived, initial: true) { _, archived in composer.readOnly = archived }
        .task {
            saves.start()
            pending?.startProgress()
            // Unsent bubbles don't wait for the network history.
            async let bubbles: Void = pending?.reload() ?? ()
            await timeline.load()
            await bubbles
        }
        .onDisappear {
            saves.stop()
            pending?.stopProgress()
        }
        // Back in front: what arrived in this conversation meanwhile is read now.
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            timeline.appBecameActive()
        }
    }
}

extension ChatView {
    /// An attachment's Open, keep-offline and preview model, registered for the cache's
    /// notices; nil without the offline client (tests of other views).
    func makeRow(_ file: FfiFileInfo) -> FileRowModel? {
        guard let offline = client as? any OfflineClient else { return nil }
        let row = FileRowModel(file: file, client: offline)
        feed?.register(row)
        return row
    }
}

/// An unsent message: dimmed while it's on its way, with its actions once it failed.
struct PendingRow: View {
    let message: FfiPendingMessage
    let pending: PendingModel

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if !message.body.isEmpty { Text(message.body).foregroundStyle(.secondary) }
            ForEach(message.files, id: \.transferId) { file in
                Label(pending.fileLine(file), systemImage: "doc").font(.caption).foregroundStyle(.secondary)
            }
            HStack(spacing: 8) {
                Text(PendingModel.text(message)).font(.caption).foregroundStyle(.secondary)
                ForEach(PendingModel.actions(message), id: \.self) { action in
                    Button(Self.title(action), role: action == .delete ? .destructive : nil) {
                        Task { await pending.perform(action, on: message) }
                    }
                    .buttonStyle(.link).font(.caption)
                }
            }
        }
        .opacity(PendingModel.actions(message).isEmpty ? 0.6 : 1)
    }

    static func title(_ action: PendingModel.Action) -> String {
        switch action {
        case .retry: "Retry"
        case .sendWithoutQuote: "Send without the quote"
        case .delete: "Delete"
        case .cancel: "Cancel"
        }
    }
}

struct MessageRow: View {
    let message: FfiMessage
    /// The author's current name (a rename reaches cached rows through the timeline).
    let author: String
    let mine: Bool
    /// This user's id (a message that mentions them is tinted).
    var me: String = ""
    let saves: SaveModel
    let composer: ComposerModel
    /// Toggle this user's reaction with an emoji.
    var onReact: (String) -> Void = { _ in }
    var makeRow: (FfiFileInfo) -> FileRowModel? = { _ in nil }

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
                Text(message.body).textSelection(.enabled)
            } else if message.attachments.isEmpty {
                Text("Files removed").italic().foregroundStyle(.secondary)
            }
            ForEach(message.attachments, id: \.id) { file in
                AttachmentRow(file: file, saves: saves, makeRow: makeRow)
            }
            if !message.deleted, !message.reactions.isEmpty {
                HStack(spacing: 4) {
                    ForEach(message.reactions, id: \.emoji) { reaction in
                        Button { onReact(reaction.emoji) } label: {
                            Text("\(reaction.emoji) \(reaction.count)").font(.callout)
                        }
                        .buttonStyle(.bordered)
                        .tint(reaction.me ? .accentColor : .secondary)
                        .accessibilityLabel("\(reaction.emoji), \(reaction.count)\(reaction.me ? ", including you" : "")")
                    }
                }
            }
        }
        // A message that mentions you is tinted.
        .padding(.vertical, 2).padding(.horizontal, 6)
        .background(NotificationPlanner.mentions(message, me: me) ? Color.accentColor.opacity(0.12) : .clear,
                    in: RoundedRectangle(cornerRadius: 6))
        .contextMenu {
            if !message.deleted {
                if !composer.readOnly {
                    Menu("React") {
                        ForEach(ReactionRules.quick, id: \.self) { emoji in
                            Button(emoji) { onReact(emoji) }
                        }
                    }
                    Button("Reply") { composer.reply(to: message) }
                }
                if mine {
                    if !composer.readOnly { Button("Edit") { composer.edit(message) } }
                    Button("Delete", role: .destructive) {
                        Task { await composer.delete(message) }
                    }
                }
            }
        }
    }
}

struct AttachmentRow: View {
    let file: FfiFileInfo
    let saves: SaveModel
    /// Open, keep offline and the preview (with local data; else nil: Save only).
    @State private var row: FileRowModel?
    let makeRow: (FfiFileInfo) -> FileRowModel?
    /// Settings' "Show image previews"; every row follows it at once (the key is shared).
    @AppStorage(Settings.showImagePreviewsKey) private var showPreviews = Settings.showImagePreviewsDefault

    init(file: FfiFileInfo, saves: SaveModel, makeRow: @escaping (FfiFileInfo) -> FileRowModel? = { _ in nil }) {
        self.file = file
        self.saves = saves
        self.makeRow = makeRow
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            if let row { previewView(row) }
            line
            if let message = row?.message {
                Text(message).font(.caption).foregroundStyle(.secondary)
            }
        }
        .task {
            if row == nil { row = makeRow(file) }
            guard let row else { return }
            row.onScreen = true
            await row.reloadKeep()
            // A row that was off screen during a change catches up before it decides.
            await row.previewSetting(showPreviews)
            await row.startPreview()
        }
        .onChange(of: showPreviews) { _, on in
            Task { await row?.previewSetting(on) }
        }
        .onDisappear { row?.onScreen = false }
    }

    @ViewBuilder
    private func previewView(_ row: FileRowModel) -> some View {
        switch row.preview {
        case let .shown(image):
            Image(decorative: image, scale: 2)
                .resizable().scaledToFit()
                .frame(maxWidth: 360, maxHeight: 240, alignment: .leading)
                .onTapGesture { Task { await row.open() } }
                .accessibilityLabel("Preview of \(file.originalName). Opens the file.")
        case .offer:
            Button("Show preview") { Task { await row.showPreview() } }.font(.caption)
        case .loading:
            ProgressView().controlSize(.small)
        case .none:
            EmptyView()
        }
    }

    private var line: some View {
        HStack(spacing: 8) {
            Image(systemName: MessageText.fileIcon(file.contentType))
            VStack(alignment: .leading, spacing: 0) {
                Text(file.originalName).lineLimit(1)
                Text(ByteCountFormatter.string(fromByteCount: Int64(file.size), countStyle: .file))
                    .font(.caption).foregroundStyle(.secondary)
            }
            Spacer()
            switch saves.states[file.id] {
            case let .saving(done, total):
                ProgressView(value: Double(done), total: Double(max(total, 1)))
                    .frame(width: 80)
                Button("Cancel") { saves.cancel(file) }
            case .saved:
                Text("Saved").font(.caption).foregroundStyle(.secondary)
                saveButton
            case let .failed(why):
                Text(why).font(.caption).foregroundStyle(.red)
                saveButton
            case nil:
                saveButton
            }
            if let row, row.hasLocalData, !row.gone {
                Button("Open") { Task { await row.open() } }.disabled(row.opening)
                Toggle(isOn: Binding(get: { row.keep != .off }, set: { _ in Task { await row.toggleKeep() } })) {
                    Image(systemName: "arrow.down.circle")
                }
                .toggleStyle(.button).disabled(row.keepBusy)
                .help(Self.keepHelp(row.keep))
                .accessibilityLabel("Keep available offline")
            }
        }
        .padding(8)
        .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))
        .frame(maxWidth: 420, alignment: .leading)
    }

    static func keepHelp(_ keep: FileRowModel.Keep) -> String {
        switch keep {
        case .off: "Keep available offline"
        case .fetching: "Downloading for offline"
        case .kept: "Available offline"
        }
    }

    private var saveButton: some View {
        Button("Save…") {
            let panel = NSSavePanel()
            panel.nameFieldStringValue = file.filename  // the server's safe name
            if panel.runModal() == .OK, let url = panel.url {
                Task { await saves.save(file, to: url) }
            }
        }
    }
}

struct ComposerView: View {
    @Bindable var composer: ComposerModel
    @Environment(\.showUsernames) private var showUsernames
    @FocusState private var inputFocused: Bool

    /// "Replying to <who>"; a message whose author has neither a name nor a handle is "a message".
    static func replyBanner(_ m: FfiMessage, showUsernames: Bool) -> String {
        let blank = [m.authorDisplayName, m.authorHandle].allSatisfy { // raw name: tested for blankness only
            ($0 ?? "").trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
        }
        if blank { return "Replying to a message" }
        return "Replying to " + PersonName.label(
            m.authorDisplayName, handle: m.authorHandle, showUsernames: showUsernames) // raw name: input to the label
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let reply = composer.replyingTo {
                banner(Self.replyBanner(reply, showUsernames: showUsernames))
            } else if composer.editing != nil {
                banner("Editing your message")
            }
            if let error = composer.error {
                Text(error).font(.caption).foregroundStyle(.red)
            }
            if !composer.staged.isEmpty {
                ScrollView(.horizontal) {
                    HStack {
                        ForEach(composer.staged) { file in
                            HStack(spacing: 4) {
                                Image(systemName: "doc")
                                Text(file.name).lineLimit(1)
                                Text(ByteCountFormatter.string(fromByteCount: Int64(file.size), countStyle: .binary))
                                    .foregroundStyle(.secondary)
                                Button { composer.remove(file) } label: { Image(systemName: "xmark.circle.fill") }
                                    .buttonStyle(.borderless).disabled(composer.preparing)
                                    .accessibilityLabel("Remove \(file.name)")
                            }
                            .font(.caption).padding(.horizontal, 6).padding(.vertical, 3)
                            .background(.quaternary, in: Capsule())
                        }
                    }
                }
            }
            if composer.preparing {
                Label("Preparing files…", systemImage: "hourglass").font(.caption).foregroundStyle(.secondary)
            }
            HStack(alignment: .bottom) {
                Button { attach() } label: { Image(systemName: "paperclip") }
                    .buttonStyle(.borderless)
                    .disabled(!composer.canAttach)
                    .help(composer.canAttach ? "Attach files" : "Sending files needs this Mac's storage")
                    .accessibilityLabel("Attach files")
                TextField("Message", text: $composer.text, axis: .vertical)
                    .lineLimit(1 ... 6)
                    .textFieldStyle(.roundedBorder)
                    .focused($inputFocused)
                    // The field takes Cmd+V itself (and disables Paste for an image), so SwiftUI's paste
                    // command never fires there: the key is caught before it reaches the field.
                    .onKeyboardPaste(active: inputFocused) { paste() }
                    .onSubmit { Task { await composer.send() } }
                Button(composer.editing == nil ? "Send" : "Save") {
                    Task { await composer.send() }
                }
                .disabled(!composer.canSend)
                .keyboardShortcut(.return, modifiers: .command)
            }
        }
        .padding(10)
        // Opening a conversation (the chat view is new per channel), or choosing Reply or Edit, leaves the
        // cursor in the box (#283). Not on later changes: typing elsewhere is never interrupted.
        .onAppear { inputFocused = true }
        .onChange(of: composer.replyingTo?.id) { _, id in if id != nil { inputFocused = true } }
        .onChange(of: composer.editing?.id) { _, id in if id != nil { inputFocused = true } }
    }

    /// Cmd+V with files or an image on the clipboard: they join the message (true). Anything else is
    /// left to the field's own paste (false).
    private func paste() -> Bool {
        let pasteboard = NSPasteboard.general
        let types = (pasteboard.types ?? []).compactMap { UTType($0.rawValue) }
        guard PasteImport.decide(types, canAttach: composer.canAttach) == .stage else { return false }
        let providers = PasteImport.providers(from: pasteboard)
        guard !providers.isEmpty else { return false }
        Task { await composer.attach(dropped: providers) }
        return true
    }

    /// Files only, several at once.
    private func attach() {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseDirectories = false
        panel.canChooseFiles = true
        panel.treatsFilePackagesAsDirectories = false
        guard panel.runModal() == .OK else { return }
        composer.attach(panel.urls)
    }

    private func banner(_ text: String) -> some View {
        HStack {
            Text(text).font(.caption).foregroundStyle(.secondary)
            Spacer()
            Button("Cancel") { composer.cancel() }.buttonStyle(.borderless).font(.caption)
        }
    }
}

extension View {
    /// Cmd+V while `active`: `handle` returns true if it took the paste, which then goes no further.
    func onKeyboardPaste(active: Bool, _ handle: @escaping () -> Bool) -> some View {
        modifier(KeyboardPaste(active: active, handle: handle))
    }
}

private struct KeyboardPaste: ViewModifier {
    let active: Bool
    let handle: () -> Bool
    @State private var monitor: Any?
    /// What the monitor reads: its closure outlives the view value it was made from, so `active` is
    /// copied here whenever it changes.
    @State private var state = Active()

    private final class Active { var value = false }

    func body(content: Content) -> some View {
        content
            .onChange(of: active, initial: true) { _, now in state.value = now }
            .onAppear {
                let state = state
                monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
                    let plainCommand = event.modifierFlags.intersection(.deviceIndependentFlagsMask) == .command
                    guard state.value, plainCommand, event.charactersIgnoringModifiers == "v" else { return event }
                    return handle() ? nil : event
                }
            }
            .onDisappear {
                if let monitor { NSEvent.removeMonitor(monitor) }
                monitor = nil
            }
    }
}
