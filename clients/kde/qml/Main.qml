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

            Component.onCompleted: chat.start()

            ListModel { id: channelsModel }
            ListModel { id: messagesModel }
            ListModel { id: publicModel }
            ListModel { id: searchModel }

            function openChannelById(cid) {
                for (var i = 0; i < channelsModel.count; i++) {
                    if (channelsModel.get(i).cid === cid) {
                        page.currentChannel = cid;
                        page.currentKind = channelsModel.get(i).kind;
                        page.currentArchived = channelsModel.get(i).archived;
                        page.cancelReply();
                        messagesModel.clear();
                        channelsModel.setProperty(i, "unread", 0);
                        chat.select_channel(cid);
                        chat.mark_read(cid);
                        break;
                    }
                }
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
                    edited: m.edited_at ? true : false,
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
                    for (var i = 0; i < arr.length; i++) {
                        channelsModel.append({
                            cid: arr[i].id,
                            label: channelTitle(arr[i]),
                            unread: arr[i].unread_count || 0,
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
                                label = channelsModel.get(i).label;
                                break;
                            }
                        }
                        // Desktop notification — only when we know who we are and
                        // it's someone else (don't notify our own messages).
                        if (chat.my_id && m.author_id !== chat.my_id) {
                            var who = m.author_display_name || m.author_handle || "Someone";
                            chat.notify(label, who + ": " + m.body);
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
                                    Controls.Label {
                                        text: model.unread
                                        visible: model.unread > 0
                                        color: Kirigami.Theme.highlightColor
                                        font.bold: true
                                    }
                                }
                                onClicked: {
                                    page.currentChannel = model.cid;
                                    page.currentKind = model.kind;
                                    page.currentArchived = model.archived;
                                    page.cancelReply(); // a pending reply targets the old channel
                                    messagesModel.clear(); // don't show the old channel while loading
                                    channelsModel.setProperty(index, "unread", 0);
                                    chat.select_channel(model.cid);
                                    chat.mark_read(model.cid);
                                }
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
                            delegate: ColumnLayout {
                                id: msgDelegate
                                property string mmid: model.mid
                                width: ListView.view ? ListView.view.width : implicitWidth
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
                                    text: model.body
                                    wrapMode: Text.WordWrap
                                    Layout.fillWidth: true
                                    Layout.leftMargin: Kirigami.Units.largeSpacing
                                    Layout.rightMargin: Kirigami.Units.largeSpacing
                                }
                                // Reaction chips + quick-react picker.
                                RowLayout {
                                    Layout.leftMargin: Kirigami.Units.largeSpacing
                                    spacing: Kirigami.Units.smallSpacing
                                    Repeater {
                                        model: JSON.parse(model.reactionsJson)
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
                            onCountChanged: positionViewAtEnd()
                        }
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
