import BrookCore
import SwiftUI

/// Which conversation sheet is up.
enum ConversationSheet: String, Identifiable {
    case newMessage, newChannel, browse, addMember, rename
    var id: String { rawValue }
}

/// A channel action that asks first.
enum ManageConfirm: Identifiable {
    case archive(Bool), delete
    var id: String {
        switch self {
        case let .archive(on): on ? "archive" : "unarchive"
        case .delete: "delete"
        }
    }
}

struct StartConversationSheet: View {
    @State var model: StartConversationModel
    let onOpened: (FfiChannel) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("New Message").font(.title2)
            Form { TextField("Their handle", text: $model.handle, prompt: Text("@handle")) }
            if let error = model.error {
                Text(error).foregroundStyle(.red)
            } else if !model.handle.isEmpty, let problem = model.problem {
                Text(problem).font(.callout).foregroundStyle(.secondary)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                Button("Start") { Task { await model.submit() } }
                    .keyboardShortcut(.defaultAction).disabled(!model.canSubmit)
            }
        }
        .padding(20).frame(width: 380)
        .onChange(of: model.opened) { _, channel in
            if let channel { onOpened(channel); dismiss() }
        }
    }
}

struct NewChannelSheet: View {
    @State var model: NewChannelModel
    let onCreated: (FfiChannel) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("New Channel").font(.title2)
            Form {
                TextField("Name", text: $model.name)
                TextField("Topic", text: $model.topic, prompt: Text("Optional"))
                Toggle("Public: anyone can find and join it", isOn: $model.isPublic)
            }
            if let error = model.error {
                Text(error).foregroundStyle(.red)
            } else if !model.name.isEmpty, let problem = model.problem {
                Text(problem).font(.callout).foregroundStyle(.secondary)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                Button("Create") { Task { await model.submit() } }
                    .keyboardShortcut(.defaultAction).disabled(!model.canSubmit)
            }
        }
        .padding(20).frame(width: 420)
        .onChange(of: model.created) { _, channel in
            if let channel { onCreated(channel); dismiss() }
        }
    }
}

struct BrowseChannelsSheet: View {
    @State var model: PublicChannelsModel
    let onJoined: (FfiChannel) -> Void
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Browse Channels").font(.title2)
            if !model.loaded {
                ProgressView().frame(maxWidth: .infinity)
            } else if model.channels.isEmpty, model.error == nil {
                Text("No channels to join.").foregroundStyle(.secondary)
            } else {
                List(model.channels, id: \.id) { channel in
                    HStack {
                        VStack(alignment: .leading) {
                            Text(channel.name ?? "Channel")
                            if let topic = channel.topic, !topic.isEmpty {
                                Text(topic).font(.caption).foregroundStyle(.secondary)
                            }
                        }
                        Spacer()
                        Button("Join") { Task { await model.join(channel.id) } }
                            .disabled(model.busy != nil)
                    }
                }
                .frame(minHeight: 180)
            }
            if let error = model.error { Text(error).foregroundStyle(.red) }
            HStack { Spacer(); Button("Done") { dismiss() }.keyboardShortcut(.defaultAction) }
        }
        .padding(20).frame(width: 420)
        .task { await model.load() }
        .onChange(of: model.joined) { _, channel in
            if let channel { onJoined(channel); dismiss() }
        }
    }
}

struct AddMemberSheet: View {
    let model: ChannelManagementModel
    let title: String
    @State private var handle = ""
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Add Member to \(title)").font(.title2)
            Form { TextField("Their handle", text: $handle, prompt: Text("@handle")) }
            if let error = model.error { Text(error).foregroundStyle(.red) }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                Button("Add") { Task { await model.addMember(handle) } }
                    .keyboardShortcut(.defaultAction)
                    .disabled(Handle.clean(handle).isEmpty || model.busy)
            }
        }
        .padding(20).frame(width: 380)
        .onChange(of: model.done) { _, done in if done { dismiss() } }
    }
}

struct RenameSheet: View {
    let model: ChannelManagementModel
    @State private var name: String
    @State private var topic: String
    @Environment(\.dismiss) private var dismiss

    init(model: ChannelManagementModel) {
        self.model = model
        _name = State(initialValue: model.channel.name ?? "")
        _topic = State(initialValue: model.channel.topic ?? "")
    }

    private var problem: String? {
        ChannelManagementModel.renameProblem(name: name, topic: topic)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Rename Channel").font(.title2)
            Form {
                TextField("Name", text: $name)
                TextField("Topic", text: $topic, prompt: Text("Optional"))
            }
            if let error = model.error {
                Text(error).foregroundStyle(.red)
            } else if let problem { Text(problem).font(.callout).foregroundStyle(.secondary) }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                Button("Save") { Task { await model.rename(name: name, topic: topic) } }
                    .keyboardShortcut(.defaultAction).disabled(problem != nil || model.busy)
            }
        }
        .padding(20).frame(width: 420)
        .onChange(of: model.done) { _, done in if done { dismiss() } }
    }
}

/// The conversation sheets, the archive/delete confirmation and the failure alert, kept out of
/// `SignedInView.body` (which is already too big for the type-checker).
struct ConversationPresentation: ViewModifier {
    let client: any FfiBrookClientProtocol
    let user: FfiUser
    @Binding var sheet: ConversationSheet?
    @Binding var managing: ChannelManagementModel?
    @Binding var confirming: ManageConfirm?
    @Binding var manageError: String?
    let onOpen: (FfiChannel) -> Void

    private var title: String {
        guard let confirming, let name = managing?.channel.name else { return "" }
        switch confirming {
        case let .archive(on): return on ? "Archive \(name)?" : "Unarchive \(name)?"
        case .delete: return "Delete \(name)?"
        }
    }

    /// An archive or delete the user confirmed: its error, if any, in an alert.
    private func run(_ act: @escaping (ChannelManagementModel) async -> Void) {
        guard let model = managing, !model.busy else { return }
        Task {
            await act(model)
            manageError = model.error
            // Only this model's: a sheet opened meanwhile holds another.
            if managing === model { managing = nil }
        }
    }

    func body(content: Content) -> some View {
        content
            .sheet(item: $sheet) { which in sheetContent(which) }
            .confirmationDialog(
                title, isPresented: Binding(get: { confirming != nil }, set: { if !$0 { confirming = nil } }),
                presenting: confirming
            ) { action in
                switch action {
                case let .archive(on): Button(on ? "Archive" : "Unarchive") { run { await $0.setArchived(on) } }
                case .delete: Button("Delete", role: .destructive) { run { await $0.delete() } }
                }
            } message: { action in
                if case .delete = action { Text("This deletes its messages for everyone. It can't be undone.") }
            }
            .alert("Couldn't do that", isPresented: Binding(get: { manageError != nil }, set: { if !$0 { manageError = nil } })) {
                Button("OK") {}
            } message: { Text(manageError ?? "") }
    }

    @ViewBuilder private func sheetContent(_ which: ConversationSheet) -> some View {
        if let conversations = client as? any ConversationClient {
            switch which {
            case .newMessage:
                StartConversationSheet(model: StartConversationModel(myHandle: user.handle, client: conversations),
                                       onOpened: onOpen)
            case .newChannel:
                NewChannelSheet(model: NewChannelModel(client: conversations), onCreated: onOpen)
            case .browse:
                BrowseChannelsSheet(model: PublicChannelsModel(client: conversations), onJoined: onOpen)
            case .addMember:
                if let managing { AddMemberSheet(model: managing, title: managing.channel.name ?? "channel") }
            case .rename:
                if let managing { RenameSheet(model: managing) }
            }
        }
    }
}
