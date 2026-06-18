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

            Component.onCompleted: chat.start()

            ListModel { id: channelsModel }
            ListModel { id: messagesModel }

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
                    author: m.author_display_name || m.author_handle || "Unknown",
                    body: m.body
                });
            }
            function sendMessage() {
                if (composer.text.trim().length === 0)
                    return;
                chat.send(page.currentChannel, composer.text);
                composer.text = "";
            }

            Connections {
                target: chat
                function onChannels_loaded(json) {
                    channelsModel.clear();
                    var arr = JSON.parse(json);
                    for (var i = 0; i < arr.length; i++)
                        channelsModel.append({ cid: arr[i].id, label: channelTitle(arr[i]) });
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
                    if (m.channel_id === page.currentChannel)
                        appendMessage(m);
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
                                text: model.label
                                onClicked: {
                                    page.currentChannel = model.cid;
                                    messagesModel.clear(); // don't show the old channel while loading
                                    chat.select_channel(model.cid);
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
                                onClicked: addMemberSheet.open()
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
                                width: ListView.view ? ListView.view.width : implicitWidth
                                spacing: 0
                                Controls.Label {
                                    text: model.author
                                    opacity: 0.7
                                    font: Kirigami.Theme.smallFont
                                    Layout.leftMargin: Kirigami.Units.largeSpacing
                                }
                                Controls.Label {
                                    text: model.body
                                    wrapMode: Text.WordWrap
                                    Layout.fillWidth: true
                                    Layout.leftMargin: Kirigami.Units.largeSpacing
                                    Layout.rightMargin: Kirigami.Units.largeSpacing
                                }
                            }
                            onCountChanged: positionViewAtEnd()
                        }
                    }
                    Kirigami.Separator { Layout.fillWidth: true }
                    RowLayout {
                        Layout.fillWidth: true
                        Layout.margins: Kirigami.Units.smallSpacing
                        Controls.TextField {
                            id: composer
                            Layout.fillWidth: true
                            placeholderText: "Message…"
                            enabled: page.currentChannel !== ""
                            onAccepted: page.sendMessage()
                        }
                        Controls.Button {
                            text: "Send"
                            enabled: page.currentChannel !== ""
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
                    Controls.Label { text: "New channel (admin only)" }
                    Controls.TextField {
                        id: chanField
                        Layout.fillWidth: true
                        placeholderText: "name"
                    }
                    Controls.Button {
                        text: "Create channel"
                        onClicked: {
                            chat.create_channel(chanField.text);
                            chanField.text = "";
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
        }
    }
}
