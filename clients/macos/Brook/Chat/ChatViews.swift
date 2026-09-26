import AppKit
import BrookCore
import SwiftUI

/// A channel's conversation: its messages, and the box to write in.
struct ChatView: View {
    let channelId: String
    let me: String
    @State var timeline: TimelineModel
    @State var composer: ComposerModel
    @State var saves: SaveModel
    /// Unsent messages (with local data, #62).
    let pending: PendingModel?

    init(channelId: String, me: String, client: any ChatClient, timeline: TimelineModel,
         pending: PendingModel? = nil) {
        self.channelId = channelId
        self.me = me
        self.pending = pending
        _timeline = State(initialValue: timeline)
        let composer = ComposerModel(
            channelId: channelId, client: client, onMessage: { [weak timeline] in
                timeline?.merge([$0])
            })
        composer.pending = pending
        pending?.timeline = timeline
        _composer = State(initialValue: composer)
        _saves = State(initialValue: SaveModel(client: client))
    }

    var body: some View {
        VStack(spacing: 0) {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 10) {
                        if timeline.atStart {
                            Text("This is the start of the conversation.")
                                .font(.caption).foregroundStyle(.secondary)
                        } else if !timeline.messages.isEmpty {
                            ProgressView().controlSize(.small)
                                .onAppear { Task { await timeline.loadOlder() } }
                        }
                        ForEach(timeline.messages, id: \.id) { message in
                            MessageRow(message: message, author: timeline.authorName(message),
                                       mine: message.authorId == me, saves: saves, composer: composer)
                                .id(message.id)
                        }
                        if let pending {
                            ForEach(pending.visible, id: \.clientId) { unsent in
                                PendingRow(message: unsent, pending: pending)
                            }
                        }
                    }
                    .padding(12)
                }
                .onChange(of: timeline.messages.last?.id) { _, newest in
                    if let newest { proxy.scrollTo(newest, anchor: .bottom) }
                }
            }
            if let error = timeline.visibleError {
                Text(error).foregroundStyle(.red).font(.caption).padding(.horizontal)
            }
            Divider()
            ComposerView(composer: composer)
        }
        // Files dropped anywhere on the conversation join the next message (not while
        // editing, and only with this Mac's storage).
        .dropDestination(for: URL.self) { urls, _ in
            guard composer.canAttach else { return false }
            composer.attach(urls.filter(\.isFileURL))
            return true
        }
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
    let saves: SaveModel
    let composer: ComposerModel

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Text(author).bold()
                Text(Self.time(message.createdAt)).font(.caption).foregroundStyle(.secondary)
                if message.editedAt != nil, !message.deleted {
                    Text("edited").font(.caption).foregroundStyle(.secondary)
                }
            }
            if let quote = message.replyTo, !message.deleted {
                Text("↳ Replying to \(Self.excerpt(quote))")
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
                AttachmentRow(file: file, saves: saves)
            }
        }
        .contextMenu {
            if !message.deleted {
                Button("Reply") { composer.reply(to: message) }
                if mine {
                    Button("Edit") { composer.edit(message) }
                    Button("Delete", role: .destructive) {
                        Task { await composer.delete(message) }
                    }
                }
            }
        }
    }

    /// A quote's line, from its state rather than its text.
    static func excerpt(_ quote: FfiReplyExcerpt) -> String {
        if quote.deleted { return "a deleted message" }
        let flat = quote.body.split(whereSeparator: \.isWhitespace).joined(separator: " ")
        if flat.isEmpty { return quote.attachments > 0 ? "a file" : "a message" }
        return String(flat.prefix(80))
    }

    static func time(_ iso: String) -> String {
        let parser = ISO8601DateFormatter()
        parser.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let date = parser.date(from: iso) ?? ISO8601DateFormatter().date(from: iso)
        guard let date else { return "" }
        return date.formatted(date: .omitted, time: .shortened)
    }
}

struct AttachmentRow: View {
    let file: FfiFileInfo
    let saves: SaveModel

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: Self.icon(file.contentType))
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
        }
        .padding(8)
        .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))
        .frame(maxWidth: 420, alignment: .leading)
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

    static func icon(_ type: String) -> String {
        if type.hasPrefix("image/") { return "photo" }
        if type.hasPrefix("video/") { return "film" }
        if type.hasPrefix("audio/") { return "waveform" }
        if type == "application/pdf" { return "doc.richtext" }
        return "doc"
    }
}

struct ComposerView: View {
    @Bindable var composer: ComposerModel

    var body: some View {
        VStack(alignment: .leading, spacing: 4) {
            if let reply = composer.replyingTo {
                banner("Replying to \(reply.authorDisplayName ?? "a message")")
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
                    .onSubmit { Task { await composer.send() } }
                Button(composer.editing == nil ? "Send" : "Save") {
                    Task { await composer.send() }
                }
                .disabled(!composer.canSend)
                .keyboardShortcut(.return, modifiers: .command)
            }
        }
        .padding(10)
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
