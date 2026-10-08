// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

import BrookCore
import SwiftUI

struct SignedInView: View {
    let user: FfiUser
    let client: any FfiBrookClientProtocol
    let calls: CallCenter
    let signOut: () -> Void
    /// Local data (#62): the cache's notices, whether Sign Out offers removal, and the sign-out
    /// that removes or keeps it.
    let feed: CacheFeed?
    let offersRemoval: Bool
    let signOutChoosing: (_ removeData: Bool) -> Void
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
    @State private var addingUser = false
    /// The open channel's conversation (made when the selection changes, never in `body`).
    @State private var timeline: TimelineModel?
    @State private var pending: PendingModel?
    @State private var signingOut = false
    @State private var editingProfile = false
    /// The name `updateProfile` answered with: the session's stored user keeps the old one
    /// until the next sign-in (#184).
    @State private var shownName: String?
    @State private var leaving: ChannelRow?
    @State private var showingMembers = false
    /// Message search (online), when this client can.
    @State private var search: SearchModel?
    /// Starting conversations and managing the open channel (spec 2026-09-30-mac-conversations).
    @State private var conversationSheet: ConversationSheet?
    @State private var managing: ChannelManagementModel?
    @State private var confirming: ManageConfirm?
    @State private var manageError: String?
    /// The open channel's ownership question (#190): when it shows, and its answer's state
    /// (kept here so an error and "Ask Me Later" survive redraws). Keyed by channel and offer.
    @State private var prompt = OfferPrompt()
    @State private var answering: OfferAnswerModel?
    @State private var answeringFor: String?

    init(
        user: FfiUser, client: any FfiBrookClientProtocol, calls: CallCenter,
        signOut: @escaping () -> Void, recoveryCodesLeft: UInt32? = nil,
        feed: CacheFeed? = nil, offersRemoval: Bool = false,
        signOutChoosing: @escaping (_ removeData: Bool) -> Void = { _ in }
    ) {
        self.recoveryCodesLeft = recoveryCodesLeft
        self.feed = feed
        self.offersRemoval = offersRemoval
        self.signOutChoosing = signOutChoosing
        self.user = user
        self.client = client
        self.calls = calls
        self.signOut = signOut
        let channelsModel = ChannelsModel(client: client, me: user.id, notifier: MacNotifier.shared)
        _channels = State(initialValue: channelsModel)
        // Hits are listed only for channels the list has (a hit elsewhere would open an empty pane).
        _search = State(initialValue: (client as? any SearchClient).map { search in
            SearchModel(client: search, known: { id in channelsModel.channels.contains { $0.id == id } })
        })
    }
    @Environment(\.openWindow) private var openWindow
    /// The Settings window's preference: pushed into the model, which relabels the rows.
    @AppStorage(Settings.showUsernamesKey) private var showUsernames = false

    var body: some View {
        NavigationSplitView {
            List(channels.channels, id: \.id, selection: $selection) { channel in
                HStack {
                    Text(channels.title(channel))
                    if channel.archived {
                        Text("archived").font(.caption).foregroundStyle(.secondary)
                    }
                    Spacer()
                    if let badge = channels.badge(channel) {
                        Text(badge).font(.caption).foregroundStyle(.green)
                    }
                    if channels.offerToMe(channel) != nil {
                        Text("Owner?").font(.caption.bold()).foregroundStyle(.tint)
                            .help("You've been offered ownership of this channel")
                    }
                    if let mentions = channels.mentions(channel) {
                        Text("@\(mentions)").font(.caption.bold()).monospacedDigit()
                            .padding(.horizontal, 5).padding(.vertical, 1)
                            .foregroundStyle(.white).background(.tint, in: Capsule())
                            .accessibilityLabel("\(mentions) unread mention\(mentions == 1 ? "" : "s")")
                    }
                    if let unread = channels.unread(channel) {
                        Text("\(unread)").font(.caption.bold()).monospacedDigit()
                            .padding(.horizontal, 6).padding(.vertical, 1)
                            .background(.tint.opacity(0.2), in: Capsule())
                            .accessibilityLabel("\(unread) unread")
                    }
                }
                .contextMenu {
                    if channel.canLeave, client is any MembershipClient {
                        Button("Leave Channel…") { leaving = channel }
                    }
                }
            }
            .modifier(SearchPresentation(
                model: search, title: { id in channels.channels.first { $0.id == id }.map(channels.title) ?? "a channel" },
                onOpen: { id in selection = id; search?.clear() }))
            .navigationSplitViewColumnWidth(min: 200, ideal: 240)
        } detail: {
            if let channel = channels.channels.first(where: { $0.id == selection }),
               let chat = client as? any ChatClient, let timeline, timeline.channelId == channel.id {
                ChatView(channelId: channel.id, me: user.id, client: chat, timeline: timeline,
                         pending: pending, feed: feed, archived: channel.archived)
                    .id(channel.id)  // a new conversation per channel
                    .navigationTitle(channels.title(channel))
                    .toolbar {
                        ToolbarItem {
                            Button { join(channel) } label: {
                                Label(channels.badge(channel) ?? "Join Call",
                                      systemImage: "phone.fill")
                            }
                            .disabled(channel.archived || !channels.canJoin(channel) || calls.call != nil
                                || calls.joining)
                            .help(calls.joinError ?? (channels.ready ? "Join the call" : "Connecting…"))
                        }
                        if channel.canLeave, powers(channel).canManage, client is any ConversationClient {
                            ToolbarItem {
                                Menu {
                                    Button("Add Member…") { manage(channel, sheet: .addMember) }
                                        .disabled(managing?.busy == true)
                                    Button("Rename…") { manage(channel, sheet: .rename) }
                                        .disabled(managing?.busy == true)
                                    Divider()
                                    Button(channel.archived ? "Unarchive…" : "Archive…") {
                                        managing = managementModel(channel)
                                        confirming = .archive(!channel.archived)
                                    }
                                    .disabled(managing?.busy == true)
                                    Button("Delete…", role: .destructive) {
                                        managing = managementModel(channel)
                                        confirming = .delete
                                    }
                                    .disabled(managing?.busy == true)
                                } label: {
                                    Label("Channel", systemImage: "ellipsis.circle")
                                }
                                .help("Manage this channel")
                            }
                        }
                        if channel.canLeave, let membership = client as? any MembershipClient {
                            ToolbarItem {
                                Button { showingMembers = true } label: {
                                    Label("Members", systemImage: "person.2")
                                }
                                .help("Members")
                                .popover(isPresented: $showingMembers) {
                                    MembersView(title: channels.title(channel), powers: powers(channel),
                                                model: MembersModel(channelId: channel.id, client: membership))
                                }
                            }
                        }
                    }
            } else {
                ContentUnavailableView {
                    let name = shownName ?? user.displayName // raw name: input to the label
                    let me = PersonName.label(name, handle: user.handle, showUsernames: showUsernames)
                    Label("Signed in as \(me)", systemImage: "person.crop.circle.badge.checkmark")
                } description: {
                    Text(channels.error ?? "Choose a channel.")
                }
            }
        }
        .task {
            MacNotifier.shared.onOpen = { selection = $0 } // a clicked notification opens its channel
            wireIncomingCalls()
            await channels.start()
        }
        .onDisappear { channels.stop() } // the session ended: no late read may save its ranks
        // The feed arrives once local data is switched on, after this view appears.
        .onChange(of: feed.map(ObjectIdentifier.init), initial: true) { _, _ in registerWithFeed() }
        // This Mac joining by any route ends a ring for that call (the rule also reads it directly).
        .onChange(of: calls.channelId) { _, _ in channels.incoming.reevaluate() }
        .safeAreaInset(edge: .top) {
            if let ringing = channels.incoming.ringing,
               let channel = channels.channels.first(where: { $0.id == ringing.channelId }) {
                IncomingCallBanner(caller: channels.title(channel),
                                   // Answering means leaving the current call first: the ring isn't
                                   // consumed by an Answer that can't join.
                                   busy: calls.call != nil || calls.joining || !channels.canJoin(channel),
                                   answer: { if channels.incoming.answer() != nil { join(channel) } },
                                   decline: { channels.incoming.decline() })
            }
        }
        .safeAreaInset(edge: .top) {
            if feed?.showsOfflineBanner == true {
                Label("Offline: showing messages saved on this Mac", systemImage: "wifi.slash")
                    .font(.callout).frame(maxWidth: .infinity).padding(6)
                    .background(.yellow.opacity(0.2))
            }
        }
        .sheet(isPresented: $signingOut) {
            if let offline = client as? any OfflineClient {
                SignOutSheet(model: SignOutModel(client: offline), signOut: signOutChoosing)
            }
        }
        // One alert at a time (a loss, or the other-accounts notice), in the order they came.
        .alert(item: Binding(get: { feed?.alert }, set: { if $0 == nil { feed?.dismiss() } })) { alert in
            Alert(title: Text(alert.text))
        }
        .onChange(of: selection, initial: true) { _, channelId in
            openTimeline(channelId)
            prompt.opened()
            syncAnswering()
        }
        .onChange(of: showUsernames, initial: true) { _, on in channels.showUsernames = on }
        .onChange(of: channels.channels) { _, _ in syncAnswering() } // an offer came or went
        .sheet(isPresented: Binding(
            get: {
                prompt.shows(channelId: selection, offer: selectedOffer?.offer,
                             answered: answering?.done ?? true)
            },
            // Not a deferral: the sheet can't be dismissed, so SwiftUI only closes it itself,
            // once the offer is gone or answered. "Ask Me Later" is the one way to put it off.
            set: { _ in }
        )) {
            if let answering {
                OfferAnswerSheet(model: answering) {
                    if let selection, let offer = selectedOffer?.offer { prompt.later(selection, offer) }
                }
            }
        }
        .onChange(of: channels.closed) { _, closed in
            if let closed, selection == closed { selection = nil } // removed from it (#62)
            // A sheet or confirmation for a channel that's gone would act on nothing (a
            // `not_found` reads as "done").
            if let closed {
                (managing, conversationSheet, confirming) = ManagementRules.channelClosed(
                    closed, managing: managing, sheet: conversationSheet, confirming: confirming)
            }
        }
        .toolbar {
            if client is any ConversationClient {
                ToolbarItem {
                    Menu {
                        Button("New Message…") { conversationSheet = .newMessage }
                        if user.globalRole == "admin" {
                            Button("New Channel…") { conversationSheet = .newChannel }
                        }
                        Button("Browse Channels…") { conversationSheet = .browse }
                    } label: {
                        Label("New", systemImage: "square.and.pencil")
                    }
                    .help("Start a conversation")
                }
            }
            ToolbarItem {
                Menu {
                    Button("Edit Profile…") { editingProfile = true }
                    Button("Change Password…") { changingPassword = true }
                    if AddUserMenu.isVisible(globalRole: user.globalRole) {
                        Button("Add User…") { addingUser = true }
                    }
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
                    Button("Sign Out") {
                        if offersRemoval { signingOut = true } else { signOut() }
                    }
                } label: {
                    Label("Account", systemImage: "person.crop.circle")
                }
            }
        }
        .modifier(ConversationPresentation(
            client: client, user: user, sheet: $conversationSheet, managing: $managing,
            confirming: $confirming, manageError: $manageError, onOpen: open))
        .sheet(isPresented: $editingProfile) {
            if let account = client as? any AccountClient {
                ProfileSheet(client: account) { shownName = $0.displayName } // raw name: kept with the user's handle
            }
        }
        .sheet(item: $leaving) { row in
            if let membership = client as? any MembershipClient {
                LeaveSheet(model: LeaveModel(channelId: row.id, title: channels.title(row),
                                             powers: powers(row), client: membership))
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
        .sheet(isPresented: $addingUser) {
            if let account = client as? any AccountClient {
                AddUserSheet(client: account)
            }
        }
        .sheet(isPresented: $resettingPassword) {
            if let account = client as? any AccountClient {
                AdminResetSheet(client: account, selfId: user.id)
            }
        }
        // Outermost, so every sheet, popover and dialog above reads the preference.
        .modifier(FollowsShowUsernames())
    }
}

extension SignedInView {
    /// The open channel and the pending offer to this user on it.
    fileprivate var selectedOffer: (row: ChannelRow, offer: FfiOwnerOffer)? {
        guard let row = channels.channels.first(where: { $0.id == selection }),
              let offer = channels.offerToMe(row) else { return nil }
        return (row, offer)
    }

    /// A question model for the open channel's offer, new when the channel or the offer
    /// changes; none without an offer.
    fileprivate func syncAnswering() {
        guard let (row, offer) = selectedOffer, let membership = client as? any MembershipClient else {
            answering = nil
            answeringFor = nil
            return
        }
        let key = "\(row.id)|\(offer.createdAt)"
        guard key != answeringFor else { return }
        answeringFor = key
        answering = OfferAnswerModel(
            channelId: row.id, title: channels.title(row),
            offeredBy: OfferAnswerModel.offerer(of: offer, in: row.members), client: membership)
    }

    /// The toolbar's Join and the incoming call's Answer: the call window, then the join.
    private func join(_ channel: ChannelRow) {
        guard !channel.archived, channels.canJoin(channel), calls.call == nil, !calls.joining else { return }
        openWindow(id: "call")
        Task {
            // A participant carries a name only: the handles come from here.
            let handles = Dictionary(
                channel.members.map { ($0.id, $0.handle) }, uniquingKeysWith: { first, _ in first })
            await calls.join(channelId: channel.id, name: channels.title(channel),
                             handles: handles, client: client)
        }
    }

    /// The ring reads this Mac's call directly (never through a view update), and notifies only
    /// while the app is in the background.
    private func wireIncomingCalls() {
        let calls = calls
        let channels = channels
        channels.incoming.localChannel = { calls.channelId }
        channels.incoming.alert = { id, on in
            let notice = MacNotifier.callId(id)
            guard on else { return MacNotifier.shared.remove(id: notice) }
            guard !AppActivity.isActive, let channel = channels.channels.first(where: { $0.id == id }) else { return }
            MacNotifier.shared.post(id: notice, channelId: id, title: channels.title(channel), body: "is calling you")
        }
        channels.incoming.reevaluate()
    }

    /// Select a channel just created, joined or opened, once the list has it.
    fileprivate func open(_ channel: FfiChannel) {
        Task { if await channels.reveal(channel.id) { selection = channel.id } }
    }

    fileprivate func managementModel(_ channel: ChannelRow) -> ChannelManagementModel? {
        (client as? any ConversationClient).map {
            ChannelManagementModel(channel: channel, powers: powers(channel), client: $0)
        }
    }

    fileprivate func manage(_ channel: ChannelRow, sheet: ConversationSheet) {
        managing = managementModel(channel)
        conversationSheet = sheet
    }

    fileprivate func powers(_ channel: ChannelRow) -> ChannelPowers {
        ChannelPowers(channel, me: user.id, isAdmin: user.globalRole == "admin")
    }

    /// A new conversation for the selected channel, handed to the channel list, which
    /// forwards it the message events (the previous one stops receiving them).
    fileprivate func openTimeline(_ channelId: String?) {
        channels.openChannel = channelId
        if let channelId { MacNotifier.shared.remove(channelId: channelId) } // read now
        guard let channelId, let chat = client as? any ChatClient else {
            timeline = nil
            channels.timeline = nil
            pending = nil
            feed?.timeline = nil
            feed?.pending = nil
            return
        }
        let model = TimelineModel(
            channelId: channelId, client: chat, me: user.id,
            members: { [channels] in channels.channels.first { $0.id == channelId }?.members ?? [] })
        timeline = model
        channels.timeline = model
        pending = (client as? any OfflineClient).map { PendingModel(channelId: channelId, client: $0) }
        feed?.timeline = model
        feed?.pending = pending
    }

    /// The cache's notices reach the list and the open conversation.
    fileprivate func registerWithFeed() {
        feed?.channels = channels
        feed?.timeline = timeline
        feed?.pending = pending
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

/// Across the top of the main window while a DM call rings here (#286).
struct IncomingCallBanner: View {
    let caller: String
    let busy: Bool
    let answer: () -> Void
    let decline: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "phone.arrow.down.left.fill").foregroundStyle(.green)
                .symbolEffect(.pulse)
            Text("\(caller) is calling").font(.headline).lineLimit(1)
            Spacer()
            Button("Decline", role: .destructive, action: decline)
                .help("Stop ringing on this Mac (the caller isn't told)")
            Button("Answer", action: answer)
                .buttonStyle(.borderedProminent).tint(.green) // no Return shortcut: Return sends messages
                .disabled(busy)
                .help(busy ? "Leave your current call to answer" : "Join the call")
        }
        .padding(.horizontal, 14).padding(.vertical, 10)
        .background(.green.opacity(0.15))
        .accessibilityElement(children: .contain)
        .accessibilityLabel("\(caller) is calling")
    }
}
