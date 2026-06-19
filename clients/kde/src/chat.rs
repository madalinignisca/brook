//! `ChatController`: bridges `brook-core` chat to QML.
//!
//! Channel/message data crosses to QML as JSON strings (parsed with `JSON.parse`)
//! rather than via a `QAbstractListModel`, keeping the bridge small. Networking
//! runs on the shared Tokio runtime; signals are emitted back on the Qt thread.

use core::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};

use brook_core::ServerEvent;
use cxx_qt::Threading;
use cxx_qt_lib::QString;
use tokio::sync::broadcast::error::RecvError;

use crate::app;

/// Guards the single realtime listener so `start()` can't spawn overlapping loops
/// if the chat page is recreated.
static STARTED: AtomicBool = AtomicBool::new(false);

#[cxx_qt::bridge]
pub mod qobject {
    unsafe extern "C++" {
        include!("cxx-qt-lib/qstring.h");
        type QString = cxx_qt_lib::QString;
    }

    extern "RustQt" {
        #[qobject]
        #[qml_element]
        #[qproperty(QString, my_id)]
        type ChatController = super::ChatControllerRust;

        /// Resolve identity, load channels, and open the realtime stream.
        #[qinvokable]
        fn start(self: Pin<&mut Self>);
        /// Reload the channel list.
        #[qinvokable]
        fn refresh(self: Pin<&mut Self>);
        /// Load a channel's history (emits `history_loaded`).
        #[qinvokable]
        fn select_channel(self: Pin<&mut Self>, channel_id: &QString);
        /// Send a message (optionally a quote-reply; empty `reply_to_id` = none).
        /// The WS echo renders it.
        #[qinvokable]
        fn send(self: Pin<&mut Self>, channel_id: &QString, body: &QString, reply_to_id: &QString);
        /// Open/find a 1:1 DM by handle, then reload channels.
        #[qinvokable]
        fn open_dm(self: Pin<&mut Self>, handle: &QString);
        /// Create a channel (server enforces admin), then reload channels.
        #[qinvokable]
        fn create_channel(self: Pin<&mut Self>, name: &QString);
        /// Add a member (by handle) to a channel, then reload channels.
        #[qinvokable]
        fn add_member(self: Pin<&mut Self>, channel_id: &QString, handle: &QString);
        /// Edit a message's body (author only); the WS echo re-renders it.
        #[qinvokable]
        fn edit_message(
            self: Pin<&mut Self>,
            channel_id: &QString,
            message_id: &QString,
            body: &QString,
        );
        /// Delete a message (author or admin); the WS echo removes it.
        #[qinvokable]
        fn delete_message(self: Pin<&mut Self>, channel_id: &QString, message_id: &QString);
        /// Mark a channel read up to its latest message.
        #[qinvokable]
        fn mark_read(self: Pin<&mut Self>, channel_id: &QString);
        /// Show a desktop notification (freedesktop D-Bus).
        #[qinvokable]
        fn notify(self: Pin<&mut Self>, summary: &QString, body: &QString);

        #[qsignal]
        fn channels_loaded(self: Pin<&mut Self>, json: QString);
        #[qsignal]
        fn history_loaded(self: Pin<&mut Self>, channel_id: QString, json: QString);
        #[qsignal]
        fn message_received(self: Pin<&mut Self>, json: QString);
        #[qsignal]
        fn message_updated(self: Pin<&mut Self>, json: QString);
        #[qsignal]
        fn message_deleted(self: Pin<&mut Self>, channel_id: QString, message_id: QString);
    }

    impl cxx_qt::Threading for ChatController {}
}

#[derive(Default)]
pub struct ChatControllerRust {
    my_id: QString,
}

type Controller = qobject::ChatController;

impl qobject::ChatController {
    fn start(self: Pin<&mut Self>) {
        if STARTED.swap(true, Ordering::SeqCst) {
            return; // already listening; don't spawn a second loop
        }
        let qt = self.qt_thread();
        app::runtime().spawn(async move {
            let Some(client) = app::client().await else {
                tracing::warn!("chat start before login (no client)");
                return;
            };

            if let Some(id) = client.current_user_id().await {
                let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
                    this.as_mut().set_my_id(QString::from(id.as_str()));
                });
            }
            // Subscribe BEFORE connecting so no events fired during connect are missed.
            let mut events = client.events();
            if let Err(err) = client.start_realtime().await {
                tracing::warn!(%err, "failed to start realtime");
            }
            emit_channels(&client, &qt).await;

            loop {
                match events.recv().await {
                    Ok(ServerEvent::MessageNew(message)) => {
                        let json = serde_json::to_string(&message).unwrap_or_default();
                        let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
                            this.as_mut().message_received(QString::from(json.as_str()));
                        });
                    }
                    Ok(ServerEvent::MessageUpdate(message)) => {
                        let json = serde_json::to_string(&message).unwrap_or_default();
                        let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
                            this.as_mut().message_updated(QString::from(json.as_str()));
                        });
                    }
                    Ok(ServerEvent::MessageDelete {
                        channel_id,
                        message_id,
                    }) => {
                        let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
                            this.as_mut().message_deleted(
                                QString::from(channel_id.as_str()),
                                QString::from(message_id.as_str()),
                            );
                        });
                    }
                    Ok(ServerEvent::ChannelUpdate(_)) => {
                        // Reload the list, but detached — don't block the event loop
                        // on an HTTP round-trip (would risk lagging/dropping events).
                        let client = client.clone();
                        let qt = qt.clone();
                        app::runtime().spawn(async move { emit_channels(&client, &qt).await });
                    }
                    Ok(ServerEvent::Ready) => {}
                    Ok(_) => {} // future event kinds — ignored
                    Err(RecvError::Lagged(_)) => continue,
                    Err(RecvError::Closed) => break,
                }
            }
        });
    }

    fn refresh(self: Pin<&mut Self>) {
        let qt = self.qt_thread();
        app::runtime().spawn(async move {
            if let Some(client) = app::client().await {
                emit_channels(&client, &qt).await;
            }
        });
    }

    fn select_channel(self: Pin<&mut Self>, channel_id: &QString) {
        let qt = self.qt_thread();
        let channel_id = channel_id.to_string();
        app::runtime().spawn(async move {
            let Some(client) = app::client().await else {
                return;
            };
            if let Ok(messages) = client.channel_history(&channel_id, None).await {
                let json = serde_json::to_string(&messages).unwrap_or_default();
                let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
                    this.as_mut().history_loaded(
                        QString::from(channel_id.as_str()),
                        QString::from(json.as_str()),
                    );
                });
            }
        });
    }

    fn send(self: Pin<&mut Self>, channel_id: &QString, body: &QString, reply_to_id: &QString) {
        let channel_id = channel_id.to_string();
        let body = body.to_string();
        let reply_to_id = reply_to_id.to_string();
        if body.trim().is_empty() {
            return;
        }
        let reply = (!reply_to_id.is_empty()).then_some(reply_to_id);
        app::runtime().spawn(async move {
            if let Some(client) = app::client().await {
                if let Err(err) = client
                    .send_message(&channel_id, &body, reply.as_deref())
                    .await
                {
                    tracing::warn!(%err, "failed to send message");
                }
            }
        });
    }

    fn open_dm(self: Pin<&mut Self>, handle: &QString) {
        let qt = self.qt_thread();
        let handle = handle.to_string();
        app::runtime().spawn(async move {
            let Some(client) = app::client().await else {
                return;
            };
            if client.open_dm(&handle).await.is_ok() {
                emit_channels(&client, &qt).await;
            }
        });
    }

    fn create_channel(self: Pin<&mut Self>, name: &QString) {
        let qt = self.qt_thread();
        let name = name.to_string();
        app::runtime().spawn(async move {
            let Some(client) = app::client().await else {
                return;
            };
            if client.create_channel(&name, None).await.is_ok() {
                emit_channels(&client, &qt).await;
            }
        });
    }

    fn add_member(self: Pin<&mut Self>, channel_id: &QString, handle: &QString) {
        let qt = self.qt_thread();
        let channel_id = channel_id.to_string();
        let handle = handle.to_string();
        if handle.trim().is_empty() {
            return;
        }
        app::runtime().spawn(async move {
            let Some(client) = app::client().await else {
                return;
            };
            if client.add_member(&channel_id, &handle).await.is_ok() {
                emit_channels(&client, &qt).await;
            }
        });
    }

    fn edit_message(
        self: Pin<&mut Self>,
        channel_id: &QString,
        message_id: &QString,
        body: &QString,
    ) {
        let channel_id = channel_id.to_string();
        let message_id = message_id.to_string();
        let body = body.to_string();
        if body.trim().is_empty() {
            return;
        }
        app::runtime().spawn(async move {
            if let Some(client) = app::client().await {
                if let Err(err) = client.edit_message(&channel_id, &message_id, &body).await {
                    tracing::warn!(%err, "edit_message failed");
                }
            }
        });
    }

    fn delete_message(self: Pin<&mut Self>, channel_id: &QString, message_id: &QString) {
        let channel_id = channel_id.to_string();
        let message_id = message_id.to_string();
        app::runtime().spawn(async move {
            if let Some(client) = app::client().await {
                if let Err(err) = client.delete_message(&channel_id, &message_id).await {
                    tracing::warn!(%err, "delete_message failed");
                }
            }
        });
    }

    fn mark_read(self: Pin<&mut Self>, channel_id: &QString) {
        let channel_id = channel_id.to_string();
        app::runtime().spawn(async move {
            if let Some(client) = app::client().await {
                if let Err(err) = client.mark_read(&channel_id, None).await {
                    tracing::warn!(%err, "mark_read failed");
                }
            }
        });
    }

    fn notify(self: Pin<&mut Self>, summary: &QString, body: &QString) {
        show_notification(summary.to_string(), body.to_string());
    }
}

/// Show a desktop notification on a plain OS thread (NOT tokio `spawn_blocking`:
/// notify-rust's blocking zbus can return Ok from a tokio blocking thread without
/// rendering; a standalone-style thread works).
fn show_notification(summary: String, body: String) {
    std::thread::spawn(move || {
        // Finite timeout: notify-rust's default (-1) is a persistent banner that
        // blocks later notifications until dismissed.
        if let Err(err) = notify_rust::Notification::new()
            .summary(&summary)
            .body(&body)
            .appname("Brook")
            .timeout(notify_rust::Timeout::Milliseconds(5000))
            .show()
        {
            tracing::warn!(%err, "desktop notification failed");
        }
    });
}

/// Fetch channels and emit them as JSON on the Qt thread.
async fn emit_channels(
    client: &std::sync::Arc<brook_core::BrookClient>,
    qt: &cxx_qt::CxxQtThread<Controller>,
) {
    if let Ok(channels) = client.list_channels().await {
        let json = serde_json::to_string(&channels).unwrap_or_default();
        let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
            this.as_mut().channels_loaded(QString::from(json.as_str()));
        });
    }
}
