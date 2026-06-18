//! Brook KDE/Plasma client — Qt6 + Kirigami over the shared Rust core.
//!
//! Phase 0: a Kirigami login page that authenticates via `brook-core` and
//! switches to a placeholder home page on success. The `LoginController`
//! QObject lives in [`login`]; the UI is QML (see `qml/Main.qml`).

pub mod login;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QUrl};

fn main() {
    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();

    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from("qrc:/qt/qml/dev/brook/kde/qml/Main.qml"));
    }

    if let Some(app) = app.as_mut() {
        app.exec();
    }
}
