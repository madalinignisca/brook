// Brook KDE/Plasma client — Phase 0 login → home placeholder.
// Kirigami so the app follows the Plasma theme, accent, and dark/light.
import QtQuick
import QtQuick.Controls as Controls
import QtQuick.Layouts
import org.kde.kirigami as Kirigami
import dev.brook.kde

Kirigami.ApplicationWindow {
    id: root
    title: "Brook"
    width: Kirigami.Units.gridUnit * 24
    height: Kirigami.Units.gridUnit * 36
    minimumWidth: Kirigami.Units.gridUnit * 18
    minimumHeight: Kirigami.Units.gridUnit * 24

    LoginController {
        id: controller
    }

    pageStack.initialPage: controller.logged_in ? homePage : loginPage

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

    Component {
        id: homePage
        Kirigami.Page {
            title: "Brook"
            Kirigami.PlaceholderMessage {
                anchors.centerIn: parent
                width: parent.width - Kirigami.Units.gridUnit * 4
                icon.name: "user-identity"
                text: "Signed in as " + controller.display_name
                explanation: "Chat, calls and files will live here."
            }
        }
    }
}
