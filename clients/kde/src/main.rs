//! Brook KDE/Plasma client — Qt6 + Kirigami over the shared Rust core.
//!
//! Phase 1: a Kirigami login page → chat (channels/DMs, messages). The
//! `LoginController` and `ChatController` QObjects live in [`login`]/[`chat`];
//! the UI is QML (see `qml/Main.qml`).

pub mod app;
pub mod chat;
pub mod login;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QUrl};

fn main() {
    // Surface core logs (e.g. the WebSocket lifecycle) on stderr.
    tracing_subscriber::fmt::init();

    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();

    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from("qrc:/qt/qml/dev/brook/kde/qml/Main.qml"));
    }

    if let Some(app) = app.as_mut() {
        app.exec();
    }
}
