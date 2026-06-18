use cxx_qt_build::{CxxQtBuilder, QmlModule};

fn main() {
    // The QML files are registered under qrc:/qt/qml/dev/brook/kde/ ; the bridge
    // file (src/login.rs) registers the LoginController QML type into the module.
    CxxQtBuilder::new_qml_module(QmlModule::new("dev.brook.kde").qml_files(["qml/Main.qml"]))
        .files(["src/login.rs", "src/chat.rs"])
        .build();
}
