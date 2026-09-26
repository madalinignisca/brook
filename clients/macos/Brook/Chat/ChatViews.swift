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

    init(channelId: String, me: String, client: any ChatClient, timeline: TimelineModel) {
        self.channelId = channelId
        self.me = me
        _timeline = State(initialValue: timeline)
        _composer = State(initialValue: ComposerModel(
            channelId: channelId, client: client, onMessage: { [weak timeline] in
                timeline?.merge([$0])
            }))
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
        .task {
            saves.start()
            await timeline.load()
        }
        .onDisappear { saves.stop() }
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
            HStack(alignment: .bottom) {
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

    private func banner(_ text: String) -> some View {
        HStack {
            Text(text).font(.caption).foregroundStyle(.secondary)
            Spacer()
            Button("Cancel") { composer.cancel() }.buttonStyle(.borderless).font(.caption)
        }
    }
}
