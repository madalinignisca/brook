import BrookCore
import SwiftUI

struct SignedInView: View {
    let user: FfiUser
    let client: any FfiBrookClientProtocol
    let calls: CallCenter
    let signOut: () -> Void
    @State private var channels: ChannelsModel
    @State private var selection: String?
    @State private var changingPassword = false
    @State private var resettingPassword = false

    init(user: FfiUser, client: any FfiBrookClientProtocol, calls: CallCenter, signOut: @escaping () -> Void) {
        self.user = user
        self.client = client
        self.calls = calls
        self.signOut = signOut
        _channels = State(initialValue: ChannelsModel(client: client))
    }
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        NavigationSplitView {
            List(channels.channels, id: \.id, selection: $selection) { channel in
                HStack {
                    Text(channels.title(channel))
                    Spacer()
                    if let badge = channels.badge(channel) {
                        Text(badge).font(.caption).foregroundStyle(.green)
                    }
                }
            }
            .navigationSplitViewColumnWidth(min: 200, ideal: 240)
        } detail: {
            if let channel = channels.channels.first(where: { $0.id == selection }) {
                VStack(spacing: 16) {
                    Text(channels.title(channel)).font(.title2)
                    if let badge = channels.badge(channel) { Text(badge).foregroundStyle(.green) }
                    Button {
                        openWindow(id: "call")
                        Task { await calls.join(channel, name: channels.title(channel), client: client) }
                    } label: {
                        Label("Join call", systemImage: "phone.fill")
                    }
                    .disabled(!channels.canJoin(channel) || calls.call != nil || calls.joining)
                    if !channels.ready { Text("Connecting…").foregroundStyle(.secondary) }
                    if let error = calls.joinError { Text(error).foregroundStyle(.red) }
                }
            } else {
                ContentUnavailableView {
                    Label("Signed in as \(user.displayName)", systemImage: "person.crop.circle.badge.checkmark")
                } description: {
                    Text(channels.error ?? "Choose a channel.")
                }
            }
        }
        .task { await channels.start() }
        .toolbar {
            ToolbarItem {
                Menu {
                    Button("Change Password…") { changingPassword = true }
                    if user.globalRole == "admin" {
                        Button("Reset a User's Password…") { resettingPassword = true }
                    }
                    Divider()
                    Button("Sign Out", action: signOut)
                } label: {
                    Label("Account", systemImage: "person.crop.circle")
                }
            }
        }
        .sheet(isPresented: $changingPassword) {
            if let account = client as? any AccountClient {
                ChangePasswordSheet(client: account)
            }
        }
        .sheet(isPresented: $resettingPassword) {
            if let account = client as? any AccountClient {
                AdminResetSheet(client: account, selfId: user.id)
            }
        }
    }
}
