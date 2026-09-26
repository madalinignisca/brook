import BrookCore
import SwiftUI

struct SignedInView: View {
    let user: FfiUser
    let client: any FfiBrookClientProtocol
    let calls: CallCenter
    let signOut: () -> Void
    /// Set after a sign-in with a recovery code: warn when few are left.
    let recoveryCodesLeft: UInt32?
    @State private var channels: ChannelsModel
    @State private var totpEnabled: Bool?
    @State private var settingUpTotp = false
    @State private var secondFactorAction: SecondFactorModel.Action?
    @State private var resettingTotp = false
    @State private var selection: String?
    @State private var changingPassword = false
    @State private var resettingPassword = false
    /// The open channel's conversation (made when the selection changes, never in `body`).
    @State private var timeline: TimelineModel?

    init(
        user: FfiUser, client: any FfiBrookClientProtocol, calls: CallCenter,
        signOut: @escaping () -> Void, recoveryCodesLeft: UInt32? = nil
    ) {
        self.recoveryCodesLeft = recoveryCodesLeft
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
                    if let unread = channels.unread(channel) {
                        Text("\(unread)").font(.caption.bold()).monospacedDigit()
                            .padding(.horizontal, 6).padding(.vertical, 1)
                            .background(.tint.opacity(0.2), in: Capsule())
                            .accessibilityLabel("\(unread) unread")
                    }
                }
            }
            .navigationSplitViewColumnWidth(min: 200, ideal: 240)
        } detail: {
            if let channel = channels.channels.first(where: { $0.id == selection }),
               let chat = client as? any ChatClient, let timeline, timeline.channelId == channel.id {
                ChatView(channelId: channel.id, me: user.id, client: chat, timeline: timeline)
                    .id(channel.id)  // a new conversation per channel
                    .navigationTitle(channels.title(channel))
                    .toolbar {
                        ToolbarItem {
                            Button {
                                openWindow(id: "call")
                                Task {
                                    await calls.join(channelId: channel.id, name: channels.title(channel),
                                                     client: client)
                                }
                            } label: {
                                Label(channels.badge(channel) ?? "Join Call",
                                      systemImage: "phone.fill")
                            }
                            .disabled(!channels.canJoin(channel) || calls.call != nil
                                || calls.joining)
                            .help(calls.joinError ?? (channels.ready ? "Join the call" : "Connecting…"))
                        }
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
        .onChange(of: selection, initial: true) { _, channelId in openTimeline(channelId) }
        .onChange(of: channels.closed) { _, closed in
            if let closed, selection == closed { selection = nil } // removed from it (#62)
        }
        .toolbar {
            ToolbarItem {
                Menu {
                    Button("Change Password…") { changingPassword = true }
                    if user.globalRole == "admin" {
                        Button("Reset a User's Password…") { resettingPassword = true }
                    }
                    Divider()
                    if totpEnabled == true {
                        Button("New Recovery Codes…") { secondFactorAction = .newCodes }
                        Button("Turn Off Two-Factor Sign-In…") { secondFactorAction = .turnOff }
                    } else if totpEnabled == false {
                        Button("Turn On Two-Factor Sign-In…") { settingUpTotp = true }
                    }
                    if user.globalRole == "admin" {
                        Button("Reset a User's Two-Factor Sign-In…") { resettingTotp = true }
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
        .sheet(isPresented: $settingUpTotp, onDismiss: refreshTotp) {
            if let account = client as? any AccountClient { TwoFactorSetupSheet(client: account) }
        }
        .sheet(item: $secondFactorAction, onDismiss: refreshTotp) { action in
            if let account = client as? any AccountClient { SecondFactorSheet(client: account, action: action) }
        }
        .sheet(isPresented: $resettingTotp) {
            if let account = client as? any AccountClient {
                AdminTotpResetSheet(client: account, selfId: user.id)
            }
        }
        .task { refreshTotp() }
        .safeAreaInset(edge: .top) {
            if let left = recoveryCodesLeft, left <= 2, totpEnabled == true {
                HStack {
                    Label("You have \(left) recovery code\(left == 1 ? "" : "s") left.",
                          systemImage: "exclamationmark.triangle")
                    Spacer()
                    Button("New Recovery Codes…") { secondFactorAction = .newCodes }
                }
                .padding(8)
                .background(.orange.opacity(0.15))
            }
        }
        .sheet(isPresented: $resettingPassword) {
            if let account = client as? any AccountClient {
                AdminResetSheet(client: account, selfId: user.id)
            }
        }
    }
}

extension SignedInView {
    /// A new conversation for the selected channel, handed to the channel list, which
    /// forwards it the message events (the previous one stops receiving them).
    fileprivate func openTimeline(_ channelId: String?) {
        channels.openChannel = channelId
        guard let channelId, let chat = client as? any ChatClient else {
            timeline = nil
            channels.timeline = nil
            return
        }
        let model = TimelineModel(channelId: channelId, client: chat)
        timeline = model
        channels.timeline = model
    }

    /// Whether two-factor sign-in is on decides which menu items show.
    fileprivate func refreshTotp() {
        guard let account = client as? any AccountClient else { return }
        Task { totpEnabled = (try? await account.me())?.totpEnabled }
    }
}

extension SecondFactorModel.Action: Identifiable {
    var id: Self { self }
}
