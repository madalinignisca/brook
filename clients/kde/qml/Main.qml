// Brook KDE/Plasma client — Phase 1: login → chat (channels/DMs, messages).
// Kirigami so the app follows the Plasma theme, accent, and dark/light.
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import dev.brook.kde

Kirigami.ApplicationWindow {
    id: root
    title: "Brook"
    width: Kirigami.Units.gridUnit * 48
    height: Kirigami.Units.gridUnit * 36
    minimumWidth: Kirigami.Units.gridUnit * 28
    minimumHeight: Kirigami.Units.gridUnit * 20

    LoginController {
        id: controller
    }
    ChatController {
        id: chat
    }

    pageStack.initialPage: controller.logged_in ? chatPage : loginPage

    // --- login ---
    Component {
        id: loginPage
        Kirigami.ScrollablePage {
            title: "Welcome to Brook"
            ColumnLayout {
                anchors.centerIn: parent
                width: Math.min(parent.width, Kirigami.Units.gridUnit * 20)
                spacing: Kirigami.Units.largeSpacing

                Kirigami.Heading {
                    text: "Sign in to your server"
                    level: 2
                    Layout.alignment: Qt.AlignHCenter
                }
                Kirigami.FormLayout {
                    Layout.fillWidth: true
                    Controls.TextField {
                        id: serverField
                        Kirigami.FormData.label: "Server"
                        text: "https://localhost"
                        enabled: !controller.busy
                    }
                    Controls.TextField {
                        id: handleField
                        Kirigami.FormData.label: "Handle"
                        enabled: !controller.busy
                        onAccepted: passwordField.forceActiveFocus()
                    }
                    Kirigami.PasswordField {
                        id: passwordField
                        Kirigami.FormData.label: "Password"
                        enabled: !controller.busy
                        onAccepted: controller.log_in(serverField.text, handleField.text, passwordField.text)
                    }
                }
                Controls.Button {
                    text: controller.busy ? "Signing in…" : "Log in"
                    enabled: !controller.busy
                    Layout.fillWidth: true
                    onClicked: controller.log_in(serverField.text, handleField.text, passwordField.text)
                }
                Kirigami.InlineMessage {
                    Layout.fillWidth: true
                    type: Kirigami.MessageType.Error
                    text: controller.error_text
                    visible: controller.error_text.length > 0
                }
            }
        }
    }

    // --- chat ---
    Component {
        id: chatPage
        Kirigami.Page {
            id: page
            padding: 0
            title: "Brook"

            property string currentChannel: ""
            property string currentKind: ""
            property bool currentArchived: false
            property string replyingTo: ""
            property string replyingToText: ""
            property string typingText: ""
            property double lastTyping: 0
            // The channel list as the server (or the cache) last sent it: members, roles
            // and ownership offers for the open channel's dialogs.
            property var channelsData: []
            property var currentMembers: []
            property var currentOffers: []
            property string myRole: ""
            // "Ask Me Later" after a failed answer: that offer isn't asked about again
            // until a channel is next opened. An answered one isn't asked about again.
            property string deferredOffer: ""
            property string answeredOffer: ""

            function maybeTyping() {
                if (composer.text.length > 0 && Date.now() - page.lastTyping > 3000) {
                    chat.typing(page.currentChannel);
                    page.lastTyping = Date.now();
                }
            }

            Timer {
                id: typingTimer
                interval: 4000
                onTriggered: page.typingText = ""
            }

            Component.onCompleted: chat.start()

            ListModel { id: channelsModel }
            ListModel { id: messagesModel }
            ListModel { id: publicModel }
            ListModel { id: searchModel }

            function openChannelById(cid) {
                for (var i = 0; i < channelsModel.count; i++) {
                    if (channelsModel.get(i).cid === cid) {
                        page.openChannel(i);
                        break;
                    }
                }
            }
            // Open the sidebar's channel at `i`: reading it clears its counts.
            function openChannel(i) {
                var row = channelsModel.get(i);
                if (row.cid !== page.currentChannel)
                    page.deferredOffer = ""; // a new opening asks again
                page.currentChannel = row.cid;
                page.currentKind = row.kind;
                page.currentArchived = row.archived;
                page.cancelReply(); // a pending reply targets the old channel
                page.typingText = "";
                messagesModel.clear(); // don't show the old channel while loading
                channelsModel.setProperty(i, "unread", 0);
                channelsModel.setProperty(i, "mentions", 0);
                chat.select_channel(row.cid);
                chat.mark_read(row.cid);
                page.syncCurrent();
            }
            function currentData() {
                for (var i = 0; i < page.channelsData.length; i++)
                    if (page.channelsData[i].id === page.currentChannel)
                        return page.channelsData[i];
                return null;
            }
            // The open channel's members, offers and your role, from the latest list.
            function syncCurrent() {
                var c = page.currentData();
                page.currentMembers = c && c.members ? c.members : [];
                var offers = [];
                if (c && c.owner_offers)
                    for (var i = 0; i < c.owner_offers.length; i++)
                        offers.push(c.owner_offers[i].user_id);
                page.currentOffers = offers;
                var role = "";
                for (var j = 0; j < page.currentMembers.length; j++)
                    if (page.currentMembers[j].id === chat.my_id)
                        role = page.currentMembers[j].role || "";
                page.myRole = role;
                page.syncOfferQuestion();
            }
            // Which offer to you a channel carries: channel, offerer and time (a new
            // offer is a new question). "" for none.
            function offerKeyFor(c) {
                if (!c || !c.owner_offers || !chat.my_id)
                    return "";
                for (var i = 0; i < c.owner_offers.length; i++) {
                    var o = c.owner_offers[i];
                    if (o.user_id === chat.my_id)
                        return c.id + "|" + o.offered_by + "|" + o.created_at;
                }
                return "";
            }
            // The ownership question follows the open channel's offer: it closes once the
            // offer is gone (withdrawn, answered elsewhere, another channel opened) and asks
            // about a new one, unless it was put off or already answered.
            function syncOfferQuestion() {
                var c = page.currentData();
                var wanted = page.offerKeyFor(c);
                if (offerDialog.showing && offerDialog.key !== wanted) {
                    offerDialog.showing = false;
                    offerDialog.close();
                }
                if (wanted === "" || offerDialog.showing || wanted === page.deferredOffer
                        || wanted === page.answeredOffer)
                    return;
                var by = "An owner";
                for (var i = 0; i < c.members.length; i++)
                    for (var j = 0; j < c.owner_offers.length; j++)
                        if (c.owner_offers[j].user_id === chat.my_id
                                && c.members[i].id === c.owner_offers[j].offered_by)
                            by = c.members[i].display_name + " (@" + c.members[i].handle + ")";
                offerDialog.key = wanted;
                offerDialog.cid = c.id;
                offerDialog.subtitle = by + " offered to make you an owner of " + page.channelTitle(c)
                    + ". Owners can rename it, remove members and offer ownership to others.";
                offerDialog.busy = false;
                offerDialog.failed = false;
                offerDialog.errorText = "";
                offerDialog.showing = true;
                offerDialog.open();
            }
            function isOwner(uid) {
                for (var i = 0; i < page.currentMembers.length; i++)
                    if (page.currentMembers[i].id === uid)
                        return page.currentMembers[i].role === "owner";
                return false;
            }
            function showAlert(heading, text) {
                alertDialog.title = heading;
                alertDialog.subtitle = text;
                alertDialog.open();
            }
            // "Leave": the last owner is told first (the server refuses them); else confirm.
            function askToLeave() {
                var owners = 0;
                for (var i = 0; i < page.currentMembers.length; i++)
                    if (page.currentMembers[i].role === "owner")
                        owners++;
                if (owners === 1 && page.myRole === "owner") {
                    page.showAlert("You're the Last Owner",
                                   "The last owner can't leave. Delete the channel instead.");
                    return;
                }
                leaveDialog.cid = page.currentChannel;
                leaveDialog.title = "Leave " + page.channelNameById(page.currentChannel) + "?";
                leaveDialog.open();
            }
            function channelNameById(cid) {
                for (var i = 0; i < channelsModel.count; i++)
                    if (channelsModel.get(i).cid === cid)
                        return channelsModel.get(i).label;
                return "channel";
            }

            function channelTitle(c) {
                if (c.name && c.name.length > 0)
                    return c.name;
                if (c.kind === "dm" && c.members) {
                    for (var i = 0; i < c.members.length; i++)
                        if (c.members[i].id !== chat.my_id)
                            return c.members[i].display_name;
                }
                return "Conversation";
            }
            function appendMessage(m) {
                messagesModel.append({
                    mid: m.id,
                    authorId: m.author_id,
                    author: m.author_display_name || m.author_handle || "Unknown",
                    body: m.body,
                    bodyHtml: chat.render_markdown(m.body),
                    edited: m.edited_at ? true : false,
                    // Names you or everyone (stored with the message, so history has it too).
                    mentioned: chat.mentions_me(JSON.stringify(m)),
                    replyAuthor: m.reply_to ? (m.reply_to.author_display_name || m.reply_to.author_handle || "Unknown") : "",
                    replyBody: m.reply_to ? m.reply_to.body : "",
                    // Stored as a JSON string: a nested JS array in a ListModel role
                    // gets wrapped in a nested ListModel, breaking modelData/length.
                    reactionsJson: JSON.stringify(m.reactions || [])
                });
            }
            readonly property var quickEmoji: ["👍", "❤️", "😂", "🎉", "👀", "🙏"]
            function applyReaction(r) {
                if (r.channel_id !== page.currentChannel)
                    return;
                for (var i = 0; i < messagesModel.count; i++) {
                    if (messagesModel.get(i).mid !== r.message_id)
                        continue;
                    var list = JSON.parse(messagesModel.get(i).reactionsJson);
                    var next = [];
                    var found = false;
                    for (var j = 0; j < list.length; j++) {
                        var item = { emoji: list[j].emoji, count: list[j].count, me: list[j].me };
                        if (item.emoji === r.emoji) {
                            found = true;
                            item.count = r.count;
                            if (r.user_id === chat.my_id)
                                item.me = r.added;
                        }
                        if (item.count > 0)
                            next.push(item);
                    }
                    if (!found && r.count > 0)
                        next.push({ emoji: r.emoji, count: r.count, me: (r.user_id === chat.my_id && r.added) });
                    messagesModel.setProperty(i, "reactionsJson", JSON.stringify(next));
                    break;
                }
            }
            function startReply(mid, author) {
                page.replyingTo = mid;
                page.replyingToText = "Replying to " + author;
                composer.forceActiveFocus();
            }
            function cancelReply() {
                page.replyingTo = "";
                page.replyingToText = "";
            }
            function sendMessage() {
                if (composer.text.trim().length === 0)
                    return;
                chat.send(page.currentChannel, composer.text, page.replyingTo);
                composer.text = "";
                page.cancelReply();
            }

            Connections {
                target: chat
                function onChannels_loaded(json) {
                    channelsModel.clear();
                    var arr = JSON.parse(json);
                    page.channelsData = arr;
                    for (var i = 0; i < arr.length; i++) {
                        var open = arr[i].id === page.currentChannel;
                        channelsModel.append({
                            cid: arr[i].id,
                            label: channelTitle(arr[i]),
                            // The open channel is being read: its counts stay clear.
                            unread: open ? 0 : (arr[i].unread_count || 0),
                            mentions: open ? 0 : (arr[i].unread_mentions || 0),
                            offered: page.offerKeyFor(arr[i]) !== "",
                            kind: arr[i].kind,
                            archived: arr[i].archived || false
                        });
                        // Re-sync the open channel's state so a live archive/rename
                        // updates the composer/header without reselecting.
                        if (arr[i].id === page.currentChannel) {
                            page.currentKind = arr[i].kind;
                            page.currentArchived = arr[i].archived || false;
                        }
                    }
                    page.syncCurrent();
                }
                function onChannel_deleted(cid) {
                    if (cid === page.currentChannel) {
                        page.currentChannel = "";
                        page.currentKind = "";
                        page.currentArchived = false;
                        messagesModel.clear();
                    }
                }
                function onPublic_channels_loaded(json) {
                    publicModel.clear();
                    var arr = JSON.parse(json);
                    for (var i = 0; i < arr.length; i++)
                        publicModel.append({ cid: arr[i].id, label: arr[i].name || "channel" });
                }
                function onSearch_results_loaded(json) {
                    searchModel.clear();
                    var arr = JSON.parse(json);
                    for (var i = 0; i < arr.length; i++) {
                        var who = arr[i].author_display_name || arr[i].author_handle || "?";
                        searchModel.append({
                            cid: arr[i].channel_id,
                            line: page.channelNameById(arr[i].channel_id) + " · " + who + ": " + arr[i].body
                        });
                    }
                }
                function onHistory_loaded(cid, json) {
                    if (cid !== page.currentChannel)
                        return;
                    messagesModel.clear();
                    var arr = JSON.parse(json);
                    for (var i = 0; i < arr.length; i++)
                        appendMessage(arr[i]);
                }
                function onMessage_received(json) {
                    var m = JSON.parse(json);
                    if (m.channel_id === page.currentChannel) {
                        appendMessage(m);
                        chat.mark_read(page.currentChannel);
                    } else {
                        // Bump the unread badge for the channel that received it.
                        var label = "Brook";
                        for (var i = 0; i < channelsModel.count; i++) {
                            if (channelsModel.get(i).cid === m.channel_id) {
                                channelsModel.setProperty(i, "unread", channelsModel.get(i).unread + 1);
                                if (chat.mentions_me(json))
                                    channelsModel.setProperty(i, "mentions", channelsModel.get(i).mentions + 1);
                                label = channelsModel.get(i).label;
                                break;
                            }
                        }
                        // Desktop notification — only when we know who we are and
                        // it's someone else (don't notify our own messages).
                        if (chat.my_id && m.author_id !== chat.my_id) {
                            var who = m.author_display_name || m.author_handle || "Someone";
                            var mentioned = m.mention_everyone || (m.mentions || []).indexOf(chat.my_id) >= 0;
                            chat.notify(label, mentioned ? (who + " mentioned you: " + m.body) : (who + ": " + m.body));
                        }
                    }
                }
                function onMessage_updated(json) {
                    var m = JSON.parse(json);
                    if (m.channel_id !== page.currentChannel)
                        return;
                    for (var i = 0; i < messagesModel.count; i++) {
                        if (messagesModel.get(i).mid === m.id) {
                            messagesModel.setProperty(i, "body", m.body);
                            messagesModel.setProperty(i, "bodyHtml", chat.render_markdown(m.body));
                            messagesModel.setProperty(i, "edited", true);
                            break;
                        }
                    }
                }
                function onMessage_deleted(cid, mid) {
                    if (cid !== page.currentChannel)
                        return;
                    if (page.replyingTo === mid)
                        page.cancelReply(); // the reply target is gone
                    for (var i = 0; i < messagesModel.count; i++) {
                        if (messagesModel.get(i).mid === mid) {
                            messagesModel.remove(i);
                            break;
                        }
                    }
                }
                function onReaction_updated(json) {
                    page.applyReaction(JSON.parse(json));
                }
                function onAction_failed(heading, text) {
                    page.showAlert(heading, text);
                }
                // An answer's result names the offer it answered: a late one for an offer
                // since replaced leaves the newer question alone.
                function onAction_done(action, tag) {
                    if (action === "answer" && offerDialog.showing && tag === offerDialog.key) {
                        page.answeredOffer = offerDialog.key;
                        offerDialog.showing = false;
                        offerDialog.close();
                    }
                }
                function onOwnership_answer_failed(key, text) {
                    if (!offerDialog.showing || key !== offerDialog.key)
                        return;
                    offerDialog.busy = false;
                    offerDialog.failed = true;
                    offerDialog.errorText = text;
                }
                function onProfile_loaded(json) {
                    var u = JSON.parse(json);
                    profileDialog.oldName = u.display_name || "";
                    profileDialog.oldStatus = u.status_text || "";
                    profileName.text = profileDialog.oldName;
                    profileStatus.text = profileDialog.oldStatus;
                    profileDialog.handle = u.handle || "";
                    profileDialog.open();
                }
                function onTyping_received(cid, name) {
                    if (cid !== page.currentChannel)
                        return;
                    page.typingText = name + " is typing…";
                    typingTimer.restart();
                }
            }

            RowLayout {
                anchors.fill: parent
                spacing: 0

                // sidebar
                ColumnLayout {
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 14
                    Layout.fillHeight: true
                    spacing: 0
                    RowLayout {
                        Layout.fillWidth: true
                        Layout.margins: Kirigami.Units.smallSpacing
                        Kirigami.Heading {
                            text: "Conversations"
                            level: 4
                            Layout.fillWidth: true
                        }
                        Controls.Button {
                            icon.name: "user-properties"
                            display: Controls.AbstractButton.IconOnly
                            text: "Edit profile"
                            onClicked: chat.load_profile()
                        }
                        Controls.Button {
                            icon.name: "edit-find"
                            display: Controls.AbstractButton.IconOnly
                            text: "Search messages"
                            onClicked: {
                                searchModel.clear();
                                searchField.text = "";
                                searchSheet.open();
                            }
                        }
                        Controls.Button {
                            icon.name: "list-add"
                            display: Controls.AbstractButton.IconOnly
                            text: "New conversation"
                            onClicked: newConvSheet.open()
                        }
                    }
                    Controls.ScrollView {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        ListView {
                            model: channelsModel
                            clip: true
                            delegate: Controls.ItemDelegate {
                                width: ListView.view ? ListView.view.width : implicitWidth
                                contentItem: RowLayout {
                                    Controls.Label {
                                        text: model.label
                                        elide: Text.ElideRight
                                        Layout.fillWidth: true
                                    }
                                    // An ownership offer waits for you here until answered.
                                    Controls.Label {
                                        text: "Owner offer"
                                        visible: model.offered
                                        color: Kirigami.Theme.highlightColor
                                        font: Kirigami.Theme.smallFont
                                    }
                                    // Unread messages that mention you: "@M", filled with
                                    // the accent, beside the unread count (as on GTK, Mac).
                                    Controls.Label {
                                        text: "@" + model.mentions
                                        visible: model.mentions > 0
                                        color: Kirigami.Theme.highlightedTextColor
                                        font.bold: true
                                        leftPadding: Kirigami.Units.smallSpacing
                                        rightPadding: Kirigami.Units.smallSpacing
                                        background: Rectangle {
                                            color: Kirigami.Theme.highlightColor
                                            radius: height / 2
                                        }
                                        Controls.ToolTip.visible: mentionHover.hovered
                                        Controls.ToolTip.text: model.mentions === 1
                                            ? "1 unread message mentions you"
                                            : model.mentions + " unread messages mention you"
                                        HoverHandler { id: mentionHover }
                                    }
                                    Controls.Label {
                                        text: model.unread
                                        visible: model.unread > 0
                                        color: Kirigami.Theme.highlightColor
                                        font.bold: true
                                    }
                                }
                                onClicked: page.openChannel(index)
                            }
                        }
                    }
                }

                Kirigami.Separator { Layout.fillHeight: true }

                // conversation
                ColumnLayout {
                    Layout.fillWidth: true
                    Layout.fillHeight: true
                    spacing: 0
                    Controls.ToolBar {
                        Layout.fillWidth: true
                        visible: page.currentChannel !== ""
                        RowLayout {
                            anchors.fill: parent
                            Item { Layout.fillWidth: true }
                            Controls.Button {
                                text: "Members"
                                icon.name: "system-users"
                                visible: page.currentKind !== "dm"
                                onClicked: membersSheet.open()
                            }
                            Controls.Button {
                                text: "Leave"
                                icon.name: "system-log-out"
                                visible: page.currentKind !== "dm"
                                onClicked: page.askToLeave()
                            }
                            Controls.Button {
                                text: "Add member"
                                icon.name: "contact-new"
                                visible: page.currentKind === "channel"
                                onClicked: addMemberSheet.open()
                            }
                            Controls.Button {
                                text: "Settings"
                                icon.name: "emblem-system"
                                visible: chat.admin && page.currentKind === "channel"
                                onClicked: channelMenu.open()
                                Controls.Menu {
                                    id: channelMenu
                                    Controls.MenuItem {
                                        text: "Rename…"
                                        onTriggered: {
                                            renameField.text = "";
                                            renameDialog.open();
                                        }
                                    }
                                    Controls.MenuItem {
                                        text: page.currentArchived ? "Unarchive" : "Archive"
                                        onTriggered: chat.set_archived(page.currentChannel, !page.currentArchived)
                                    }
                                    Controls.MenuItem {
                                        text: "Delete channel"
                                        onTriggered: deleteChannelDialog.open()
                                    }
                                }
                            }
                        }
                    }
                    Controls.ScrollView {
                        Layout.fillWidth: true
                        Layout.fillHeight: true
                        ListView {
                            id: messageView
                            model: messagesModel
                            clip: true
                            spacing: Kirigami.Units.smallSpacing
                            delegate: Item {
                                width: ListView.view ? ListView.view.width : implicitWidth
                                implicitHeight: msgDelegate.implicitHeight
                                // A message that mentions you is tinted with the accent.
                                Rectangle {
                                    anchors.fill: parent
                                    anchors.leftMargin: Kirigami.Units.smallSpacing
                                    anchors.rightMargin: Kirigami.Units.smallSpacing
                                    visible: model.mentioned
                                    radius: Kirigami.Units.smallSpacing
                                    color: Qt.rgba(Kirigami.Theme.highlightColor.r, Kirigami.Theme.highlightColor.g,
                                                   Kirigami.Theme.highlightColor.b, 0.12)
                                }
                            ColumnLayout {
                                id: msgDelegate
                                property string mmid: model.mid
                                // Read here: inside the Repeater below, `model` is the
                                // Repeater's own property, not this delegate's row.
                                property string reactions: model.reactionsJson
                                width: parent.width
                                spacing: 0
                                RowLayout {
                                    Layout.fillWidth: true
                                    Layout.leftMargin: Kirigami.Units.largeSpacing
                                    Layout.rightMargin: Kirigami.Units.largeSpacing
                                    Controls.Label {
                                        text: model.author
                                        opacity: 0.7
                                        font: Kirigami.Theme.smallFont
                                    }
                                    Controls.Label {
                                        text: "edited"
                                        visible: model.edited
                                        opacity: 0.5
                                        font: Kirigami.Theme.smallFont
                                    }
                                    Item { Layout.fillWidth: true }
                                    // Reply is available on any message.
                                    Controls.ToolButton {
                                        text: "Reply"
                                        display: Controls.AbstractButton.TextOnly
                                        font: Kirigami.Theme.smallFont
                                        onClicked: page.startReply(model.mid, model.author)
                                    }
                                    // Author-only actions for this message.
                                    Controls.ToolButton {
                                        text: "Edit"
                                        visible: chat.my_id && model.authorId === chat.my_id
                                        display: Controls.AbstractButton.TextOnly
                                        font: Kirigami.Theme.smallFont
                                        onClicked: {
                                            editDialog.cid = page.currentChannel;
                                            editDialog.mid = model.mid;
                                            editField.text = model.body;
                                            editDialog.open();
                                        }
                                    }
                                    Controls.ToolButton {
                                        text: "Delete"
                                        visible: chat.my_id && model.authorId === chat.my_id
                                        display: Controls.AbstractButton.TextOnly
                                        font: Kirigami.Theme.smallFont
                                        onClicked: {
                                            deleteDialog.cid = page.currentChannel;
                                            deleteDialog.mid = model.mid;
                                            deleteDialog.open();
                                        }
                                    }
                                }
                                // Quoted-reply preview above the body, if any.
                                Controls.Label {
                                    visible: model.replyBody !== ""
                                    text: "↳ " + model.replyAuthor + ": " + model.replyBody
                                    opacity: 0.6
                                    elide: Text.ElideRight
                                    font: Kirigami.Theme.smallFont
                                    Layout.fillWidth: true
                                    Layout.leftMargin: Kirigami.Units.largeSpacing
                                    Layout.rightMargin: Kirigami.Units.largeSpacing
                                }
                                Controls.Label {
                                    text: model.bodyHtml
                                    textFormat: Text.RichText
                                    wrapMode: Text.WordWrap
                                    // Only open web + mail links (no file:, smb:, …).
                                    onLinkActivated: (link) => {
                                        if (/^(https?:|mailto:)/i.test(link))
                                            Qt.openUrlExternally(link);
                                    }
                                    Layout.fillWidth: true
                                    Layout.leftMargin: Kirigami.Units.largeSpacing
                                    Layout.rightMargin: Kirigami.Units.largeSpacing
                                }
                                // Reaction chips + quick-react picker.
                                RowLayout {
                                    Layout.leftMargin: Kirigami.Units.largeSpacing
                                    spacing: Kirigami.Units.smallSpacing
                                    Repeater {
                                        model: JSON.parse(msgDelegate.reactions)
                                        delegate: Controls.Button {
                                            required property var modelData
                                            text: modelData.emoji + " " + modelData.count
                                            flat: true
                                            highlighted: modelData.me
                                            font: Kirigami.Theme.smallFont
                                            onClicked: chat.toggle_reaction(page.currentChannel, msgDelegate.mmid, modelData.emoji)
                                        }
                                    }
                                    Controls.ToolButton {
                                        text: "🙂 React"
                                        display: Controls.AbstractButton.TextOnly
                                        flat: true
                                        font: Kirigami.Theme.smallFont
                                        onClicked: emojiMenu.open()
                                        Controls.Menu {
                                            id: emojiMenu
                                            Repeater {
                                                model: page.quickEmoji
                                                delegate: Controls.MenuItem {
                                                    required property string modelData
                                                    text: modelData
                                                    onTriggered: chat.toggle_reaction(page.currentChannel, msgDelegate.mmid, modelData)
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                            }
                            onCountChanged: positionViewAtEnd()
                        }
                    }
                    Controls.Label {
                        text: page.typingText
                        textFormat: Text.PlainText // display_name is untrusted
                        visible: page.typingText !== ""
                        opacity: 0.7
                        font: Kirigami.Theme.smallFont
                        Layout.leftMargin: Kirigami.Units.largeSpacing
                    }
                    Kirigami.Separator { Layout.fillWidth: true }
                    // Reply banner — shown while quoting a message.
                    RowLayout {
                        Layout.fillWidth: true
                        Layout.leftMargin: Kirigami.Units.smallSpacing
                        Layout.rightMargin: Kirigami.Units.smallSpacing
                        visible: page.replyingTo !== ""
                        Controls.Label {
                            text: page.replyingToText
                            elide: Text.ElideRight
                            opacity: 0.7
                            font: Kirigami.Theme.smallFont
                            Layout.fillWidth: true
                        }
                        Controls.ToolButton {
                            icon.name: "window-close"
                            onClicked: page.cancelReply()
                        }
                    }
                    RowLayout {
                        Layout.fillWidth: true
                        Layout.margins: Kirigami.Units.smallSpacing
                        Controls.TextField {
                            id: composer
                            Layout.fillWidth: true
                            placeholderText: page.currentArchived ? "This channel is archived" : "Message…"
                            enabled: page.currentChannel !== "" && !page.currentArchived
                            onTextEdited: page.maybeTyping()
                            onAccepted: page.sendMessage()
                        }
                        Controls.Button {
                            text: "Send"
                            enabled: page.currentChannel !== "" && !page.currentArchived
                            onClicked: page.sendMessage()
                        }
                    }
                }
            }

            Kirigami.OverlaySheet {
                id: newConvSheet
                title: "New conversation"
                ColumnLayout {
                    spacing: Kirigami.Units.largeSpacing
                    Controls.Label { text: "Direct message" }
                    Controls.TextField {
                        id: dmField
                        Layout.fillWidth: true
                        placeholderText: "handle"
                    }
                    Controls.Button {
                        text: "Open DM"
                        onClicked: {
                            chat.open_dm(dmField.text);
                            dmField.text = "";
                            newConvSheet.close();
                        }
                    }
                    Kirigami.Separator { Layout.fillWidth: true }
                    Controls.Button {
                        text: "Browse public channels"
                        onClicked: {
                            chat.browse_public();
                            newConvSheet.close();
                            browseSheet.open();
                        }
                    }
                    Kirigami.Separator { Layout.fillWidth: true; visible: chat.admin }
                    Controls.Label { text: "New channel (admin only)"; visible: chat.admin }
                    Controls.TextField {
                        id: chanField
                        Layout.fillWidth: true
                        placeholderText: "name"
                        visible: chat.admin
                    }
                    Controls.CheckBox {
                        id: publicCheck
                        text: "Public (anyone can join)"
                        visible: chat.admin
                    }
                    Controls.Button {
                        text: "Create channel"
                        visible: chat.admin
                        onClicked: {
                            if (publicCheck.checked)
                                chat.create_public_channel(chanField.text);
                            else
                                chat.create_channel(chanField.text);
                            chanField.text = "";
                            publicCheck.checked = false;
                            newConvSheet.close();
                        }
                    }
                }
            }

            Kirigami.OverlaySheet {
                id: addMemberSheet
                title: "Add member"
                ColumnLayout {
                    spacing: Kirigami.Units.largeSpacing
                    Controls.Label { text: "Add a user to this channel by handle" }
                    Controls.TextField {
                        id: memberField
                        Layout.fillWidth: true
                        placeholderText: "handle"
                    }
                    Controls.Button {
                        text: "Add"
                        onClicked: {
                            chat.add_member(page.currentChannel, memberField.text);
                            memberField.text = "";
                            addMemberSheet.close();
                        }
                    }
                }
            }

            Kirigami.PromptDialog {
                id: editDialog
                property string cid: ""
                property string mid: ""
                title: "Edit message"
                standardButtons: Controls.Dialog.Ok | Controls.Dialog.Cancel
                onAccepted: {
                    if (editField.text.trim().length > 0)
                        chat.edit_message(editDialog.cid, editDialog.mid, editField.text);
                }
                Controls.TextField {
                    id: editField
                    Layout.fillWidth: true
                    onAccepted: editDialog.accept()
                }
            }

            Kirigami.PromptDialog {
                id: deleteDialog
                property string cid: ""
                property string mid: ""
                title: "Delete message?"
                subtitle: "This can't be undone."
                standardButtons: Controls.Dialog.Ok | Controls.Dialog.Cancel
                onAccepted: chat.delete_message(deleteDialog.cid, deleteDialog.mid)
            }

            Kirigami.PromptDialog {
                id: renameDialog
                title: "Rename channel"
                standardButtons: Controls.Dialog.Ok | Controls.Dialog.Cancel
                onAccepted: {
                    if (renameField.text.trim().length > 0)
                        chat.rename_channel(page.currentChannel, renameField.text);
                }
                Controls.TextField {
                    id: renameField
                    Layout.fillWidth: true
                    onAccepted: renameDialog.accept()
                }
            }

            Kirigami.PromptDialog {
                id: deleteChannelDialog
                title: "Delete channel?"
                subtitle: "This permanently deletes the channel and its messages."
                standardButtons: Controls.Dialog.Ok | Controls.Dialog.Cancel
                onAccepted: chat.delete_channel(page.currentChannel)
            }

            Kirigami.PromptDialog {
                id: alertDialog
                standardButtons: Controls.Dialog.Ok
            }

            Kirigami.PromptDialog {
                id: leaveDialog
                // The channel asked about, kept: the open one can change while this is up.
                property string cid: ""
                subtitle: "You'll stop getting its messages. An owner or an admin can add you back."
                standardButtons: Controls.Dialog.Ok | Controls.Dialog.Cancel
                onAccepted: chat.leave_channel(cid)
            }

            // Remove or offer ownership: confirmed first.
            Kirigami.PromptDialog {
                id: memberConfirm
                property string cid: ""
                property string action: ""
                property string uid: ""
                property string handle: ""
                standardButtons: Controls.Dialog.Ok | Controls.Dialog.Cancel
                onAccepted: {
                    if (action === "remove")
                        chat.remove_member(cid, uid);
                    else
                        chat.offer_ownership(cid, handle);
                }
            }

            Kirigami.OverlaySheet {
                id: membersSheet
                title: "Members"
                ColumnLayout {
                    spacing: Kirigami.Units.smallSpacing
                    Layout.preferredWidth: Kirigami.Units.gridUnit * 22
                    Repeater {
                        model: page.currentMembers
                        delegate: RowLayout {
                            required property var modelData
                            readonly property bool isMe: modelData.id === chat.my_id
                            readonly property bool offered: page.currentOffers.indexOf(modelData.id) >= 0
                            Layout.fillWidth: true
                            ColumnLayout {
                                Layout.fillWidth: true
                                spacing: 0
                                Controls.Label {
                                    text: modelData.display_name + (isMe ? " (you)" : "")
                                    textFormat: Text.PlainText
                                    elide: Text.ElideRight
                                    Layout.fillWidth: true
                                }
                                Controls.Label {
                                    text: "@" + modelData.handle
                                          + (modelData.role === "owner" ? " · owner"
                                             : (offered ? " · owner offered" : ""))
                                    textFormat: Text.PlainText
                                    opacity: 0.7
                                    font: Kirigami.Theme.smallFont
                                }
                            }
                            Controls.Button {
                                text: offered ? "Withdraw" : "Make owner…"
                                flat: true
                                visible: chat.may_offer(page.myRole, modelData.role || "", isMe)
                                onClicked: {
                                    if (offered) {
                                        chat.withdraw_ownership_offer(page.currentChannel, modelData.id);
                                    } else {
                                        memberConfirm.action = "offer";
                                        memberConfirm.handle = modelData.handle;
                                        memberConfirm.title = "Offer " + modelData.display_name + " ownership?";
                                        memberConfirm.subtitle = "They'll be asked when they next open this channel.";
                                        memberConfirm.cid = page.currentChannel;
                                        memberConfirm.open();
                                    }
                                }
                            }
                            Controls.Button {
                                text: "Remove"
                                flat: true
                                visible: chat.may_remove(page.myRole, modelData.role || "", isMe)
                                onClicked: {
                                    memberConfirm.action = "remove";
                                    memberConfirm.uid = modelData.id;
                                    memberConfirm.title = "Remove " + modelData.display_name + "?";
                                    memberConfirm.subtitle = "They'll stop getting this channel's messages.";
                                    memberConfirm.cid = page.currentChannel;
                                    memberConfirm.open();
                                }
                            }
                        }
                    }
                }
            }

            // The ownership question: Accept or Decline, nothing else, until an answer fails
            // (then "Ask Me Later" too, so being offline can't trap anyone in it).
            Kirigami.PromptDialog {
                id: offerDialog
                property string key: ""
                property string cid: ""
                property bool showing: false
                property bool busy: false
                property bool failed: false
                property string errorText: ""
                title: "Become an Owner?"
                showCloseButton: false
                closePolicy: Controls.Popup.NoAutoClose
                modal: true
                standardButtons: Controls.Dialog.NoButton
                onClosed: showing = false
                customFooterActions: [
                    Kirigami.Action {
                        text: "Ask Me Later"
                        visible: offerDialog.failed
                        onTriggered: {
                            page.deferredOffer = offerDialog.key;
                            offerDialog.showing = false;
                            offerDialog.close();
                        }
                    },
                    Kirigami.Action {
                        text: "Decline"
                        enabled: !offerDialog.busy
                        onTriggered: {
                            offerDialog.busy = true;
                            chat.answer_ownership(offerDialog.cid, false, offerDialog.key);
                        }
                    },
                    Kirigami.Action {
                        text: offerDialog.failed ? "Accept (Try Again)" : "Accept"
                        enabled: !offerDialog.busy
                        onTriggered: {
                            offerDialog.busy = true;
                            chat.answer_ownership(offerDialog.cid, true, offerDialog.key);
                        }
                    }
                ]
                Controls.Label {
                    visible: offerDialog.errorText !== ""
                    text: offerDialog.errorText
                    color: Kirigami.Theme.negativeTextColor
                    wrapMode: Text.WordWrap
                    Layout.fillWidth: true
                }
            }

            Kirigami.PromptDialog {
                id: profileDialog
                property string oldName: ""
                property string oldStatus: ""
                property string handle: ""
                title: "Edit Profile"
                subtitle: "Your handle, @" + handle + ", signs you in and stays the same."
                standardButtons: Controls.Dialog.Cancel
                // Save waits for a name that fits, so a refusal never throws the edit away.
                customFooterActions: [
                    Kirigami.Action {
                        text: "Save"
                        icon.name: "document-save"
                        enabled: chat.profile_fits(profileName.text, profileStatus.text)
                        onTriggered: {
                            chat.save_profile(profileDialog.oldName, profileDialog.oldStatus,
                                              profileName.text, profileStatus.text);
                            profileDialog.close();
                        }
                    }
                ]
                ColumnLayout {
                    Controls.TextField {
                        id: profileName
                        placeholderText: "Display name"
                        Layout.fillWidth: true
                    }
                    Controls.TextField {
                        id: profileStatus
                        placeholderText: "Status (optional)"
                        Layout.fillWidth: true
                    }
                }
            }

            Kirigami.OverlaySheet {
                id: searchSheet
                title: "Search messages"
                ColumnLayout {
                    spacing: Kirigami.Units.smallSpacing
                    Controls.TextField {
                        id: searchField
                        Layout.fillWidth: true
                        Layout.preferredWidth: Kirigami.Units.gridUnit * 20
                        placeholderText: "Search…"
                        onAccepted: chat.search(searchField.text)
                    }
                    Repeater {
                        model: searchModel
                        delegate: Controls.ItemDelegate {
                            required property string cid
                            required property string line
                            Layout.fillWidth: true
                            text: line
                            onClicked: {
                                page.openChannelById(cid);
                                searchSheet.close();
                            }
                        }
                    }
                    Controls.Label {
                        text: "Type a term and press Enter."
                        visible: searchModel.count === 0
                        opacity: 0.6
                    }
                }
            }

            Kirigami.OverlaySheet {
                id: browseSheet
                title: "Public channels"
                ColumnLayout {
                    spacing: Kirigami.Units.smallSpacing
                    Repeater {
                        model: publicModel
                        delegate: RowLayout {
                            required property string cid
                            required property string label
                            Layout.fillWidth: true
                            Controls.Label { text: label; Layout.fillWidth: true }
                            Controls.Button {
                                text: "Join"
                                onClicked: {
                                    chat.join_channel(cid);
                                    browseSheet.close();
                                }
                            }
                        }
                    }
                    Controls.Label {
                        text: "No public channels to join."
                        visible: publicModel.count === 0
                        opacity: 0.6
                    }
                }
            }
        }
    }
}
