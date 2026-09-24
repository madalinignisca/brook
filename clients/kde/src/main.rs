//! Brook KDE/Plasma client — Qt6 + Kirigami over the shared Rust core.
//!
//! Phase 1: a Kirigami login page → chat (channels/DMs, messages). The
//! `LoginController` and `ChatController` QObjects live in [`login`]/[`chat`];
//! the UI is QML (see `qml/Main.qml`).

pub mod app;
pub mod chat;
pub mod login;

use cxx_qt_lib::{QGuiApplication, QQmlApplicationEngine, QUrl};

/// Surface core logs (e.g. the WebSocket lifecycle) on stderr, filtered by
/// `RUST_LOG` (default `info`). The WebSocket libraries are always capped at
/// `info`: at trace they dump whole frames, which carry access tokens.
fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let mut filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    for directive in ["tungstenite=info", "tokio_tungstenite=info"] {
        filter = filter.add_directive(directive.parse().expect("static directive"));
    }
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn main() {
    init_logging();

    let mut app = QGuiApplication::new();
    let mut engine = QQmlApplicationEngine::new();

    if let Some(engine) = engine.as_mut() {
        engine.load(&QUrl::from("qrc:/qt/qml/dev/brook/kde/qml/Main.qml"));
    }

    if let Some(app) = app.as_mut() {
        app.exec();
    }
}
