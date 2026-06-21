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
        #[qproperty(bool, admin)]
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
        /// Create a public (self-joinable) channel, then reload channels.
        #[qinvokable]
        fn create_public_channel(self: Pin<&mut Self>, name: &QString);
        /// Rename a channel (admin/owner); the `channel.update` echo refreshes.
        #[qinvokable]
        fn rename_channel(self: Pin<&mut Self>, channel_id: &QString, name: &QString);
        /// Archive or unarchive a channel (admin/owner).
        #[qinvokable]
        fn set_archived(self: Pin<&mut Self>, channel_id: &QString, archived: bool);
        /// Delete a channel (admin/owner); the `channel.delete` echo removes it.
        #[qinvokable]
        fn delete_channel(self: Pin<&mut Self>, channel_id: &QString);
        /// Fetch public channels (emits `public_channels_loaded`).
        #[qinvokable]
        fn browse_public(self: Pin<&mut Self>);
        /// Self-join a public channel, then reload channels.
        #[qinvokable]
        fn join_channel(self: Pin<&mut Self>, channel_id: &QString);
        /// Search message bodies (emits `search_results_loaded`).
        #[qinvokable]
        fn search(self: Pin<&mut Self>, query: &QString);
        /// Render a markdown body to safe HTML (drops raw HTML + images) for display.
        #[qinvokable]
        fn render_markdown(self: Pin<&mut Self>, text: &QString) -> QString;
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
        /// Toggle the caller's emoji reaction on a message.
        #[qinvokable]
        fn toggle_reaction(
            self: Pin<&mut Self>,
            channel_id: &QString,
            message_id: &QString,
            emoji: &QString,
        );
        /// Signal that we're typing in a channel (ephemeral; debounce on the caller).
        #[qinvokable]
        fn typing(self: Pin<&mut Self>, channel_id: &QString);
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
        #[qsignal]
        fn reaction_updated(self: Pin<&mut Self>, json: QString);
        #[qsignal]
        fn channel_deleted(self: Pin<&mut Self>, channel_id: QString);
        #[qsignal]
        fn public_channels_loaded(self: Pin<&mut Self>, json: QString);
        #[qsignal]
        fn search_results_loaded(self: Pin<&mut Self>, json: QString);
        #[qsignal]
        fn typing_received(self: Pin<&mut Self>, channel_id: QString, display_name: QString);
    }

    impl cxx_qt::Threading for ChatController {}
}

#[derive(Default)]
pub struct ChatControllerRust {
    my_id: QString,
    admin: bool,
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
            let admin = client.is_admin().await;
            let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
                this.as_mut().set_admin(admin);
            });
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
                    Ok(ServerEvent::ReactionUpdate {
                        channel_id,
                        message_id,
                        emoji,
                        user_id,
                        added,
                        count,
                    }) => {
                        let json = serde_json::json!({
                            "channel_id": channel_id,
                            "message_id": message_id,
                            "emoji": emoji,
                            "user_id": user_id,
                            "added": added,
                            "count": count,
                        })
                        .to_string();
                        let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
                            this.as_mut().reaction_updated(QString::from(json.as_str()));
                        });
                    }
                    Ok(ServerEvent::ChannelDelete { channel_id }) => {
                        let cid = channel_id.clone();
                        let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
                            this.as_mut().channel_deleted(QString::from(cid.as_str()));
                        });
                        let client = client.clone();
                        let qt = qt.clone();
                        app::runtime().spawn(async move { emit_channels(&client, &qt).await });
                    }
                    Ok(ServerEvent::Typing {
                        channel_id,
                        display_name,
                        ..
                    }) => {
                        let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
                            this.as_mut().typing_received(
                                QString::from(channel_id.as_str()),
                                QString::from(display_name.as_str()),
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

    fn create_public_channel(self: Pin<&mut Self>, name: &QString) {
        let qt = self.qt_thread();
        let name = name.to_string();
        app::runtime().spawn(async move {
            let Some(client) = app::client().await else {
                return;
            };
            if client.create_public_channel(&name).await.is_ok() {
                emit_channels(&client, &qt).await;
            }
        });
    }

    fn rename_channel(self: Pin<&mut Self>, channel_id: &QString, name: &QString) {
        let channel_id = channel_id.to_string();
        let name = name.to_string();
        if name.trim().is_empty() {
            return;
        }
        app::runtime().spawn(async move {
            if let Some(client) = app::client().await {
                if let Err(err) = client
                    .update_channel(&channel_id, Some(&name), None, None)
                    .await
                {
                    tracing::warn!(%err, "rename_channel failed");
                }
            }
        });
    }

    fn set_archived(self: Pin<&mut Self>, channel_id: &QString, archived: bool) {
        let channel_id = channel_id.to_string();
        app::runtime().spawn(async move {
            if let Some(client) = app::client().await {
                if let Err(err) = client
                    .update_channel(&channel_id, None, None, Some(archived))
                    .await
                {
                    tracing::warn!(%err, "set_archived failed");
                }
            }
        });
    }

    fn delete_channel(self: Pin<&mut Self>, channel_id: &QString) {
        let channel_id = channel_id.to_string();
        app::runtime().spawn(async move {
            if let Some(client) = app::client().await {
                if let Err(err) = client.delete_channel(&channel_id).await {
                    tracing::warn!(%err, "delete_channel failed");
                }
            }
        });
    }

    fn browse_public(self: Pin<&mut Self>) {
        let qt = self.qt_thread();
        app::runtime().spawn(async move {
            let Some(client) = app::client().await else {
                return;
            };
            if let Ok(channels) = client.list_public_channels().await {
                let json = serde_json::to_string(&channels).unwrap_or_default();
                let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
                    this.as_mut()
                        .public_channels_loaded(QString::from(json.as_str()));
                });
            }
        });
    }

    fn join_channel(self: Pin<&mut Self>, channel_id: &QString) {
        let qt = self.qt_thread();
        let channel_id = channel_id.to_string();
        app::runtime().spawn(async move {
            let Some(client) = app::client().await else {
                return;
            };
            if client.join_channel(&channel_id).await.is_ok() {
                emit_channels(&client, &qt).await;
            }
        });
    }

    fn render_markdown(self: Pin<&mut Self>, text: &QString) -> QString {
        QString::from(markdown_to_html(&text.to_string()).as_str())
    }

    fn search(self: Pin<&mut Self>, query: &QString) {
        let qt = self.qt_thread();
        let query = query.to_string();
        if query.trim().is_empty() {
            return;
        }
        app::runtime().spawn(async move {
            let Some(client) = app::client().await else {
                return;
            };
            if let Ok(messages) = client.search_messages(&query).await {
                // "[]" (not "") on the unlikely serialize failure, so QML's
                // JSON.parse never throws on an empty string.
                let json = serde_json::to_string(&messages).unwrap_or_else(|_| "[]".to_string());
                let _ = qt.queue(move |mut this: Pin<&mut Controller>| {
                    this.as_mut()
                        .search_results_loaded(QString::from(json.as_str()));
                });
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

    fn toggle_reaction(
        self: Pin<&mut Self>,
        channel_id: &QString,
        message_id: &QString,
        emoji: &QString,
    ) {
        let channel_id = channel_id.to_string();
        let message_id = message_id.to_string();
        let emoji = emoji.to_string();
        app::runtime().spawn(async move {
            if let Some(client) = app::client().await {
                if let Err(err) = client
                    .toggle_reaction(&channel_id, &message_id, &emoji)
                    .await
                {
                    tracing::warn!(%err, "toggle_reaction failed");
                }
            }
        });
    }

    fn typing(self: Pin<&mut Self>, channel_id: &QString) {
        let channel_id = channel_id.to_string();
        app::runtime().spawn(async move {
            if let Some(client) = app::client().await {
                if let Err(err) = client.send_typing(&channel_id).await {
                    tracing::warn!(%err, "typing failed");
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

/// Render a markdown body to a safe HTML subset for Qt `Text.RichText`. Raw HTML
/// and images are dropped (untrusted bodies can't inject markup or load remote
/// resources); an image's alt text is kept.
fn markdown_to_html(text: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let mut out = String::new();
    let mut in_code = false;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Strong) => out.push_str("<b>"),
            Event::End(TagEnd::Strong) => out.push_str("</b>"),
            Event::Start(Tag::Emphasis) => out.push_str("<i>"),
            Event::End(TagEnd::Emphasis) => out.push_str("</i>"),
            Event::Start(Tag::Strikethrough) => out.push_str("<s>"),
            Event::End(TagEnd::Strikethrough) => out.push_str("</s>"),
            Event::Start(Tag::CodeBlock(_)) => {
                in_code = true;
                out.push_str("<pre>");
            }
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                out.push_str("</pre>");
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                out.push_str("<a href=\"");
                out.push_str(&html_escape(&dest_url));
                out.push_str("\">");
            }
            Event::End(TagEnd::Link) => out.push_str("</a>"),
            Event::Start(Tag::Item) => out.push_str("\u{2022} "),
            Event::End(TagEnd::Item) => out.push_str("<br>"),
            Event::End(TagEnd::Paragraph) => out.push_str("<br><br>"),
            Event::Code(t) => {
                out.push_str("<code>");
                out.push_str(&html_escape(&t));
                out.push_str("</code>");
            }
            Event::Text(t) if in_code => out.push_str(&html_escape(&t)),
            Event::Text(t) => push_html_with_mentions(&mut out, &t),
            Event::SoftBreak | Event::HardBreak => out.push_str("<br>"),
            // Raw HTML and images are dropped (they don't match any arm).
            _ => {}
        }
    }
    out.trim_end_matches("<br>").to_string()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Push text, escaping plain runs and wrapping `@mention` tokens in a styled span.
fn push_html_with_mentions(out: &mut String, text: &str) {
    let chars: Vec<char> = text.chars().collect();
    // Handles may contain '.'/'-'; the highlight is cosmetic (server resolves
    // notifications precisely), so we don't trim trailing punctuation.
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '.' || c == '-';
    let mut i = 0;
    let mut plain_start = 0;
    while i < chars.len() {
        let boundary = i == 0 || !(is_word(chars[i - 1]) || chars[i - 1] == '@');
        if chars[i] == '@' && boundary && chars.get(i + 1).is_some_and(|c| is_word(*c)) {
            let plain: String = chars[plain_start..i].iter().collect();
            out.push_str(&html_escape(&plain));
            let start = i;
            i += 1;
            while i < chars.len() && is_word(chars[i]) {
                i += 1;
            }
            let token: String = chars[start..i].iter().collect();
            out.push_str("<span style=\"color:#3584e4;font-weight:bold\">");
            out.push_str(&html_escape(&token));
            out.push_str("</span>");
            plain_start = i;
        } else {
            i += 1;
        }
    }
    let plain: String = chars[plain_start..].iter().collect();
    out.push_str(&html_escape(&plain));
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
