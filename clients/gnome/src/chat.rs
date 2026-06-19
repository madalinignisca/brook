//! The post-login chat view: a sidebar of channels/DMs, a message list, and a
//! composer — over `brook-core`. Networking runs on the Tokio runtime; results
//! are applied on the GTK main loop (await a runtime `JoinHandle` inside
//! `spawn_future_local`). Realtime `message.new` events are consumed from the
//! core's broadcast channel on the main loop and appended live.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use brook_core::{BrookClient, Channel, Message, ReactionSummary, ServerEvent};
use gtk::glib;
use tokio::runtime::Handle;

/// Quick-react emoji offered by the per-message reaction picker.
const QUICK_EMOJI: [&str; 6] = [
    "\u{1f44d}",
    "\u{2764}",
    "\u{1f602}",
    "\u{1f389}",
    "\u{1f440}",
    "\u{1f64f}",
];

/// Shared UI state captured by the various callbacks.
///
/// TODO(Phase 1b): callbacks capture `Rc<Chat>` strongly while `Chat` owns the
/// widgets, forming a reference cycle (flagged in review). Harmless for the
/// single-window session lifetime, but should move to weak captures to honor the
/// no-cycles contract in `main.rs`.
struct Chat {
    client: Arc<BrookClient>,
    runtime: Handle,
    me: Rc<RefCell<Option<String>>>,
    is_admin: Rc<RefCell<bool>>,
    current: Rc<RefCell<Option<String>>>,
    channel_list: gtk::ListBox,
    channels: Rc<RefCell<Vec<Channel>>>,
    /// Unread badge label per sidebar row, parallel to `channels`.
    badges: Rc<RefCell<Vec<gtk::Label>>>,
    /// message id -> its widgets, for live edit/delete of the open channel.
    message_rows: Rc<RefCell<HashMap<String, MessageWidgets>>>,
    /// The message id currently being replied to (quote-reply), if any.
    replying_to: Rc<RefCell<Option<String>>>,
    /// The reply banner shown above the composer while replying.
    reply_bar: gtk::Revealer,
    reply_label: gtk::Label,
    message_list: gtk::ListBox,
    message_scroll: gtk::ScrolledWindow,
    title: adw::WindowTitle,
    composer: gtk::Entry,
    send_button: gtk::Button,
}

/// The widgets of a rendered message we may mutate after an edit/delete/reaction.
#[derive(Clone)]
struct MessageWidgets {
    row: gtk::ListBoxRow,
    body: gtk::Label,
    edited: gtk::Label,
    /// The channel this message is in (for toggling reactions on it).
    channel_id: String,
    /// Container holding the reaction chips (rebuilt on each reaction change).
    reactions_box: gtk::Box,
    /// Current reaction tallies, kept in sync from `reaction.update` events.
    reactions: Rc<RefCell<Vec<ReactionSummary>>>,
}

/// Build the chat view. `is_admin` controls whether channel creation is offered.
pub fn build(client: Arc<BrookClient>, runtime: Handle, is_admin: bool) -> gtk::Widget {
    let channel_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(["navigation-sidebar"])
        .build();

    let message_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["background"])
        .build();
    let message_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&message_list)
        .build();

    let composer = gtk::Entry::builder()
        .placeholder_text("Message…")
        .hexpand(true)
        .sensitive(false)
        .build();
    let send_button = gtk::Button::builder()
        .icon_name("paper-plane-symbolic")
        .sensitive(false)
        .css_classes(["suggested-action"])
        .build();

    let title = adw::WindowTitle::new("Brook", "Pick a conversation");

    // Reply banner (revealed above the composer while quoting a message).
    let reply_label = gtk::Label::builder()
        .xalign(0.0)
        .hexpand(true)
        .wrap(false)
        .css_classes(["caption", "dim-label"])
        .build();
    let reply_cancel = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .has_frame(false)
        .tooltip_text("Cancel reply")
        .build();
    let reply_inner = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_start(12)
        .margin_end(6)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    reply_inner.append(&reply_label);
    reply_inner.append(&reply_cancel);
    let reply_bar = gtk::Revealer::builder()
        .child(&reply_inner)
        .reveal_child(false)
        .build();

    let chat = Rc::new(Chat {
        client,
        runtime,
        me: Rc::new(RefCell::new(None)),
        is_admin: Rc::new(RefCell::new(is_admin)),
        current: Rc::new(RefCell::new(None)),
        channel_list: channel_list.clone(),
        channels: Rc::new(RefCell::new(Vec::new())),
        badges: Rc::new(RefCell::new(Vec::new())),
        message_rows: Rc::new(RefCell::new(HashMap::new())),
        replying_to: Rc::new(RefCell::new(None)),
        reply_bar: reply_bar.clone(),
        reply_label: reply_label.clone(),
        message_list: message_list.clone(),
        message_scroll: message_scroll.clone(),
        title: title.clone(),
        composer: composer.clone(),
        send_button: send_button.clone(),
    });

    // --- sidebar ---
    let add_button = gtk::MenuButton::builder()
        .icon_name("list-add-symbolic")
        .tooltip_text("New conversation")
        .popover(&new_conversation_popover(&chat))
        .build();
    let sidebar_header = adw::HeaderBar::builder()
        .show_end_title_buttons(false)
        .build();
    sidebar_header.pack_start(&add_button);
    sidebar_header.set_title_widget(Some(&adw::WindowTitle::new("Brook", "")));

    let sidebar_scroll = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&channel_list)
        .build();
    let sidebar = adw::ToolbarView::new();
    sidebar.add_top_bar(&sidebar_header);
    sidebar.set_content(Some(&sidebar_scroll));

    // --- content ---
    let content_header = adw::HeaderBar::new();
    content_header.set_title_widget(Some(&title));
    let add_member_button = gtk::MenuButton::builder()
        .icon_name("contact-new-symbolic")
        .tooltip_text("Add member to this channel")
        .popover(&add_member_popover(&chat))
        .build();
    content_header.pack_end(&add_member_button);

    let composer_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    composer_row.append(&composer);
    composer_row.append(&send_button);

    let content_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content_box.append(&message_scroll);
    content_box.append(&reply_bar);
    content_box.append(&composer_row);

    reply_cancel.connect_clicked({
        let chat = chat.clone();
        move |_| set_reply(&chat, None)
    });

    let content = adw::ToolbarView::new();
    content.add_top_bar(&content_header);
    content.set_content(Some(&content_box));

    let split = adw::OverlaySplitView::builder()
        .sidebar(&sidebar)
        .content(&content)
        .min_sidebar_width(240.0)
        .max_sidebar_width(360.0)
        .build();

    // --- wiring ---
    channel_list.connect_row_selected({
        let chat = chat.clone();
        move |_, row| {
            if let Some(row) = row {
                let idx = row.index() as usize;
                let id = chat.channels.borrow().get(idx).map(|c| c.id.clone());
                if let Some(id) = id {
                    select_channel(&chat, &id);
                }
            }
        }
    });

    let do_send: Rc<dyn Fn()> = Rc::new({
        let chat = chat.clone();
        move || send_current(&chat)
    });
    composer.connect_activate({
        let do_send = do_send.clone();
        move |_| (do_send)()
    });
    send_button.connect_clicked({
        let do_send = do_send.clone();
        move |_| (do_send)()
    });

    // Resolve our user id, load channels, open the realtime stream.
    bootstrap(&chat);

    split.upcast()
}

/// Load the current user id, the channel list, and start realtime.
fn bootstrap(chat: &Rc<Chat>) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        // These run on the Tokio runtime, not the GLib executor: `start_realtime`
        // calls `tokio::spawn` internally and would panic off-runtime.
        let id = chat
            .runtime
            .spawn({
                let client = chat.client.clone();
                async move { client.current_user_id().await }
            })
            .await
            .ok()
            .flatten();
        if let Some(id) = id {
            *chat.me.borrow_mut() = Some(id);
        }
        let _ = chat
            .runtime
            .spawn({
                let client = chat.client.clone();
                async move { client.start_realtime().await }
            })
            .await;

        spawn_event_loop(&chat);
        refresh_channels(&chat, None);
    });
}

/// Consume realtime events on the GTK main loop, appending live messages.
fn spawn_event_loop(chat: &Rc<Chat>) {
    let chat = chat.clone();
    let mut events = chat.client.events();
    glib::spawn_future_local(async move {
        loop {
            match events.recv().await {
                Ok(ServerEvent::MessageNew(message)) => {
                    let is_current = chat
                        .current
                        .borrow()
                        .as_deref()
                        .is_some_and(|c| c == message.channel_id);
                    if is_current {
                        append_message(&chat, &message);
                        mark_read(&chat, message.channel_id.clone(), Some(message.id.clone()));
                    } else {
                        // Bump the unread badge for the channel that received it.
                        let idx = chat
                            .channels
                            .borrow()
                            .iter()
                            .position(|c| c.id == message.channel_id);
                        if let Some(idx) = idx {
                            chat.channels.borrow_mut()[idx].unread_count += 1;
                            update_badge(&chat, idx);
                        }
                        // Desktop notification — only when we know who we are and
                        // it's someone else (don't notify our own messages, and
                        // don't guess if our identity isn't resolved yet).
                        let me = chat.me.borrow().clone().unwrap_or_default();
                        if !me.is_empty() && message.author_id != me {
                            let author = message
                                .author_display_name
                                .clone()
                                .or_else(|| message.author_handle.clone())
                                .unwrap_or_else(|| "Someone".to_string());
                            let title = chat
                                .channels
                                .borrow()
                                .iter()
                                .find(|c| c.id == message.channel_id)
                                .map(|c| c.title(&me))
                                .unwrap_or_else(|| "Brook".to_string());
                            notify(
                                &message.channel_id,
                                &title,
                                &format!("{author}: {}", message.body),
                            );
                        }
                    }
                }
                Ok(ServerEvent::MessageUpdate(message)) => {
                    // Update the row in place if the edited message is on screen.
                    let widgets = chat.message_rows.borrow().get(&message.id).cloned();
                    if let Some(widgets) = widgets {
                        widgets.body.set_label(&message.body);
                        widgets.edited.set_visible(true);
                    }
                }
                Ok(ServerEvent::MessageDelete { message_id, .. }) => {
                    let removed = chat.message_rows.borrow_mut().remove(&message_id);
                    if let Some(widgets) = removed {
                        chat.message_list.remove(&widgets.row);
                    }
                    // If we were replying to this message, the reply target is gone.
                    if chat.replying_to.borrow().as_deref() == Some(message_id.as_str()) {
                        set_reply(&chat, None);
                    }
                }
                Ok(ServerEvent::ReactionUpdate {
                    message_id,
                    emoji,
                    user_id,
                    added,
                    count,
                    ..
                }) => {
                    apply_reaction(&chat, &message_id, &emoji, &user_id, added, count);
                }
                Ok(ServerEvent::ChannelUpdate(_)) => {
                    // Added to / removed from a channel, or metadata changed:
                    // reload the sidebar so it reflects the change live.
                    refresh_channels(&chat, None);
                }
                Ok(ServerEvent::Ready) => {}
                Ok(_) => {} // future event kinds — ignored
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => break, // sender gone
            }
        }
    });
}

/// (Re)load the sidebar channel list. `select` optionally selects a channel id.
fn refresh_channels(chat: &Rc<Chat>, select: Option<String>) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            async move { client.list_channels().await }
        });
        let Ok(Ok(channels)) = handle.await else {
            return;
        };

        while let Some(row) = chat.channel_list.row_at_index(0) {
            chat.channel_list.remove(&row);
        }
        chat.badges.borrow_mut().clear();
        let me = chat.me.borrow().clone().unwrap_or_default();
        for channel in &channels {
            let (row, badge) =
                channel_row(&channel.title(&me), channel.is_dm(), channel.unread_count);
            chat.channel_list.append(&row);
            chat.badges.borrow_mut().push(badge);
        }
        *chat.channels.borrow_mut() = channels;

        if let Some(id) = select {
            let idx = chat.channels.borrow().iter().position(|c| c.id == id);
            if let Some(idx) = idx {
                if let Some(row) = chat.channel_list.row_at_index(idx as i32) {
                    chat.channel_list.select_row(Some(&row));
                }
            }
        }
    });
}

/// Load and render a channel's history, and enable the composer.
fn select_channel(chat: &Rc<Chat>, channel_id: &str) {
    *chat.current.borrow_mut() = Some(channel_id.to_string());
    let me = chat.me.borrow().clone().unwrap_or_default();
    if let Some(channel) = chat.channels.borrow().iter().find(|c| c.id == channel_id) {
        chat.title.set_title(&channel.title(&me));
        chat.title.set_subtitle(if channel.is_dm() {
            "Direct message"
        } else {
            "Channel"
        });
    }
    chat.composer.set_sensitive(true);
    chat.send_button.set_sensitive(true);

    // Opening a channel reads it: clear its unread badge locally and tell the
    // server. Compute idx in its own statement so the immutable borrow is dropped
    // before borrow_mut (an inline `if let` scrutinee would hold it and panic).
    let idx = chat
        .channels
        .borrow()
        .iter()
        .position(|c| c.id == channel_id);
    if let Some(idx) = idx {
        chat.channels.borrow_mut()[idx].unread_count = 0;
        update_badge(chat, idx);
    }
    mark_read(chat, channel_id.to_string(), None);

    // A pending reply targets a message in the channel we're leaving — drop it.
    set_reply(chat, None);
    // Clear now, before the await, so live messages that arrive while history is
    // loading are appended to a fresh list rather than wiped by a late clear.
    chat.message_rows.borrow_mut().clear();
    while let Some(row) = chat.message_list.row_at_index(0) {
        chat.message_list.remove(&row);
    }

    let chat = chat.clone();
    let channel_id = channel_id.to_string();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            let channel_id = channel_id.clone();
            async move { client.channel_history(&channel_id, None).await }
        });
        let Ok(Ok(messages)) = handle.await else {
            return;
        };
        // Only render if the user hasn't switched channels meanwhile.
        if chat.current.borrow().as_deref() != Some(channel_id.as_str()) {
            return;
        }
        for message in &messages {
            append_message(&chat, message);
        }
    });
}

/// Send the composer's text into the current channel (the WS echo renders it).
fn send_current(chat: &Rc<Chat>) {
    let body = chat.composer.text().to_string();
    let Some(channel_id) = chat.current.borrow().clone() else {
        return;
    };
    if body.trim().is_empty() {
        return;
    }
    chat.composer.set_text("");
    let reply_to = chat.replying_to.borrow().clone();
    set_reply(chat, None);

    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            async move {
                client
                    .send_message(&channel_id, &body, reply_to.as_deref())
                    .await
            }
        });
        if let Ok(Err(err)) = handle.await {
            tracing::warn!(%err, "failed to send message");
        }
    });
}

/// Append a message row and scroll to the bottom.
fn append_message(chat: &Rc<Chat>, message: &Message) {
    let author = message
        .author_display_name
        .clone()
        .or_else(|| message.author_handle.clone())
        .unwrap_or_else(|| "Unknown".to_string());

    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(12)
        .margin_end(12)
        .build();

    // Header: author + "edited" marker + (for our own messages) an actions menu.
    let header = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .build();
    let author_label = gtk::Label::builder()
        .label(&author)
        .xalign(0.0)
        .hexpand(true)
        .css_classes(["caption", "dim-label"])
        .build();
    let edited_label = gtk::Label::builder()
        .label("edited")
        .css_classes(["caption", "dim-label"])
        .visible(message.edited_at.is_some())
        .build();
    header.append(&author_label);
    header.append(&edited_label);

    let is_own = chat
        .me
        .borrow()
        .as_deref()
        .is_some_and(|me| me == message.author_id);
    header.append(&message_actions_button(chat, message, is_own));

    let body_label = gtk::Label::builder()
        .label(&message.body)
        .xalign(0.0)
        .wrap(true)
        .selectable(true)
        .build();
    row.append(&header);
    // Quoted-reply preview above the body, if this message is a reply.
    if let Some(reply) = &message.reply_to {
        let who = reply
            .author_display_name
            .clone()
            .or_else(|| reply.author_handle.clone())
            .unwrap_or_else(|| "Unknown".to_string());
        let quote = gtk::Label::builder()
            .label(format!("\u{21b3} {who}: {}", reply.body))
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build();
        row.append(&quote);
    }
    row.append(&body_label);

    // Reactions row: chips + a quick-react picker.
    let reactions_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .build();
    let reactions_wrapper = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(4)
        .margin_top(2)
        .build();
    reactions_wrapper.append(&reactions_box);
    reactions_wrapper.append(&react_button(chat, &message.channel_id, &message.id));
    row.append(&reactions_wrapper);

    let list_row = gtk::ListBoxRow::builder()
        .activatable(false)
        .child(&row)
        .build();
    // De-dupe: if this id is already on screen (history + WS echo can overlap),
    // drop the old row so edit/delete only ever tracks one.
    if let Some(old) = chat.message_rows.borrow_mut().remove(&message.id) {
        chat.message_list.remove(&old.row);
    }
    chat.message_list.append(&list_row);
    chat.message_rows.borrow_mut().insert(
        message.id.clone(),
        MessageWidgets {
            row: list_row,
            body: body_label,
            edited: edited_label,
            channel_id: message.channel_id.clone(),
            reactions_box,
            reactions: Rc::new(RefCell::new(message.reactions.clone())),
        },
    );
    render_reactions(chat, &message.id);

    // Scroll to bottom after layout settles.
    let adj = chat.message_scroll.vadjustment();
    glib::idle_add_local_once(move || adj.set_value(adj.upper()));
}

/// Rebuild a message's reaction chips from its tracked tallies.
fn render_reactions(chat: &Rc<Chat>, message_id: &str) {
    let Some(mw) = chat.message_rows.borrow().get(message_id).cloned() else {
        return;
    };
    while let Some(child) = mw.reactions_box.first_child() {
        mw.reactions_box.remove(&child);
    }
    let channel_id = mw.channel_id.clone();
    for summary in mw.reactions.borrow().iter() {
        let chip = gtk::Button::builder()
            .label(format!("{} {}", summary.emoji, summary.count))
            .has_frame(false)
            .build();
        if summary.me {
            chip.add_css_class("suggested-action");
        }
        chip.connect_clicked({
            let chat = chat.clone();
            let channel_id = channel_id.clone();
            let message_id = message_id.to_string();
            let emoji = summary.emoji.clone();
            move |_| toggle_reaction(&chat, channel_id.clone(), message_id.clone(), emoji.clone())
        });
        mw.reactions_box.append(&chip);
    }
}

/// A "react" menu button offering the quick-react emoji.
fn react_button(chat: &Rc<Chat>, channel_id: &str, message_id: &str) -> gtk::MenuButton {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .margin_top(2)
        .margin_bottom(2)
        .margin_start(2)
        .margin_end(2)
        .build();
    let popover = gtk::Popover::builder().build();
    for emoji in QUICK_EMOJI {
        let button = gtk::Button::builder().label(emoji).has_frame(false).build();
        button.connect_clicked({
            let chat = chat.clone();
            let popover = popover.clone();
            let channel_id = channel_id.to_string();
            let message_id = message_id.to_string();
            move |_| {
                popover.popdown();
                toggle_reaction(
                    &chat,
                    channel_id.clone(),
                    message_id.clone(),
                    emoji.to_string(),
                );
            }
        });
        row.append(&button);
    }
    popover.set_child(Some(&row));
    gtk::MenuButton::builder()
        .icon_name("face-smile-symbolic")
        .has_frame(false)
        .popover(&popover)
        .tooltip_text("Add reaction")
        .build()
}

/// Toggle a reaction on the server; the WS `reaction.update` echo re-renders.
fn toggle_reaction(chat: &Rc<Chat>, channel_id: String, message_id: String, emoji: String) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let result = chat
            .runtime
            .spawn({
                let client = chat.client.clone();
                async move {
                    client
                        .toggle_reaction(&channel_id, &message_id, &emoji)
                        .await
                }
            })
            .await;
        if let Ok(Err(err)) = result {
            tracing::warn!(%err, "failed to toggle reaction");
        }
    });
}

/// Apply an incremental `reaction.update` to a message's tallies, then re-render.
fn apply_reaction(
    chat: &Rc<Chat>,
    message_id: &str,
    emoji: &str,
    user_id: &str,
    added: bool,
    count: i64,
) {
    let me = chat.me.borrow().clone().unwrap_or_default();
    let is_me = !me.is_empty() && user_id == me;
    let Some(mw) = chat.message_rows.borrow().get(message_id).cloned() else {
        return;
    };
    {
        let mut reactions = mw.reactions.borrow_mut();
        if let Some(existing) = reactions.iter_mut().find(|r| r.emoji == emoji) {
            existing.count = count;
            if is_me {
                existing.me = added;
            }
        } else if count > 0 {
            reactions.push(ReactionSummary {
                emoji: emoji.to_string(),
                count,
                me: is_me && added,
            });
        }
        reactions.retain(|r| r.count > 0);
    }
    render_reactions(chat, message_id);
}

/// A flat "⋯" menu: Reply (any message) plus Edit / Delete for our own.
fn message_actions_button(chat: &Rc<Chat>, message: &Message, is_own: bool) -> gtk::MenuButton {
    let menu = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();
    let popover = gtk::Popover::builder().build();

    let reply = gtk::Button::builder()
        .label("Reply")
        .has_frame(false)
        .build();
    menu.append(&reply);
    reply.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        let message_id = message.id.clone();
        let label = message
            .author_display_name
            .clone()
            .or_else(|| message.author_handle.clone())
            .unwrap_or_else(|| "message".to_string());
        move |_| {
            popover.popdown();
            set_reply(&chat, Some((message_id.clone(), label.clone())));
        }
    });

    if !is_own {
        popover.set_child(Some(&menu));
        return gtk::MenuButton::builder()
            .icon_name("view-more-symbolic")
            .has_frame(false)
            .popover(&popover)
            .tooltip_text("Message actions")
            .build();
    }

    let edit = gtk::Button::builder()
        .label("Edit")
        .has_frame(false)
        .build();
    let delete = gtk::Button::builder()
        .label("Delete")
        .has_frame(false)
        .css_classes(["error"])
        .build();
    menu.append(&edit);
    menu.append(&delete);
    popover.set_child(Some(&menu));

    edit.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        let channel_id = message.channel_id.clone();
        let message_id = message.id.clone();
        move |_| {
            popover.popdown();
            // Read the CURRENT body — a prior live edit may have changed it, so the
            // captured original would revert it.
            let current = chat
                .message_rows
                .borrow()
                .get(&message_id)
                .map(|w| w.body.label().to_string())
                .unwrap_or_default();
            edit_message_dialog(&chat, channel_id.clone(), message_id.clone(), current);
        }
    });
    delete.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        let channel_id = message.channel_id.clone();
        let message_id = message.id.clone();
        move |_| {
            popover.popdown();
            delete_message_confirm(&chat, channel_id.clone(), message_id.clone());
        }
    });

    gtk::MenuButton::builder()
        .icon_name("view-more-symbolic")
        .has_frame(false)
        .popover(&popover)
        .tooltip_text("Message actions")
        .build()
}

/// Edit dialog: prefilled entry → `edit_message` (the WS `message.update` re-renders).
fn edit_message_dialog(chat: &Rc<Chat>, channel_id: String, message_id: String, current: String) {
    let entry = gtk::Entry::builder().text(&current).hexpand(true).build();
    let dialog = adw::AlertDialog::builder()
        .heading("Edit message")
        .extra_child(&entry)
        .build();
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("save", "Save");
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.connect_response(None, {
        let chat = chat.clone();
        move |_, response| {
            if response != "save" {
                return;
            }
            let body = entry.text().to_string();
            if body.trim().is_empty() {
                return;
            }
            let chat = chat.clone();
            let channel_id = channel_id.clone();
            let message_id = message_id.clone();
            glib::spawn_future_local(async move {
                let result = chat
                    .runtime
                    .spawn({
                        let client = chat.client.clone();
                        async move { client.edit_message(&channel_id, &message_id, &body).await }
                    })
                    .await;
                if let Ok(Err(err)) = result {
                    tracing::warn!(%err, "failed to edit message");
                }
            });
        }
    });
    dialog.present(Some(&chat.message_list));
}

/// Delete confirmation → `delete_message` (the WS `message.delete` removes the row).
fn delete_message_confirm(chat: &Rc<Chat>, channel_id: String, message_id: String) {
    let dialog = adw::AlertDialog::new(Some("Delete message?"), Some("This can't be undone."));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("delete", "Delete");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.connect_response(None, {
        let chat = chat.clone();
        move |_, response| {
            if response != "delete" {
                return;
            }
            let chat = chat.clone();
            let channel_id = channel_id.clone();
            let message_id = message_id.clone();
            glib::spawn_future_local(async move {
                let result = chat
                    .runtime
                    .spawn({
                        let client = chat.client.clone();
                        async move { client.delete_message(&channel_id, &message_id).await }
                    })
                    .await;
                if let Ok(Err(err)) = result {
                    tracing::warn!(%err, "failed to delete message");
                }
            });
        }
    });
    dialog.present(Some(&chat.message_list));
}

/// A sidebar row; returns the row and its (initially-styled) unread badge label.
fn channel_row(title: &str, is_dm: bool, unread: i64) -> (gtk::ListBoxRow, gtk::Label) {
    let row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(8)
        .margin_top(8)
        .margin_bottom(8)
        .margin_start(8)
        .margin_end(8)
        .build();
    let icon = gtk::Image::from_icon_name(if is_dm {
        "avatar-default-symbolic"
    } else {
        "user-available-symbolic"
    });
    let label = gtk::Label::builder()
        .label(title)
        .xalign(0.0)
        .hexpand(true)
        .build();
    let badge = gtk::Label::builder()
        .label(unread.to_string())
        .css_classes(["caption-heading", "accent"])
        .visible(unread > 0)
        .build();
    row.append(&icon);
    row.append(&label);
    row.append(&badge);
    (gtk::ListBoxRow::builder().child(&row).build(), badge)
}

/// Refresh a single row's badge from the channel's current `unread_count`.
fn update_badge(chat: &Rc<Chat>, idx: usize) {
    let count = chat.channels.borrow().get(idx).map(|c| c.unread_count);
    if let (Some(count), Some(badge)) = (count, chat.badges.borrow().get(idx)) {
        badge.set_label(&count.to_string());
        badge.set_visible(count > 0);
    }
}

/// Show a desktop notification via the GApplication (`org.gtk.Notifications`).
///
/// GNOME Shell drops the raw freedesktop `Notify` from a registered GApplication
/// (it expects GTK notifications, tied to the installed `.desktop`), so we use the
/// native gio path. `id` lets a channel's later notification replace its earlier
/// one. Runs on the GLib main thread (where the event loop already is).
fn notify(id: &str, summary: &str, body: &str) {
    let Some(app) = gtk::gio::Application::default() else {
        tracing::warn!("no default GApplication; cannot send notification");
        return;
    };
    let notification = gtk::gio::Notification::new(summary);
    notification.set_body(Some(body));
    app.send_notification(Some(id), &notification);
}

/// Enter (`Some`) or leave (`None`) reply mode; toggles the banner above composer.
fn set_reply(chat: &Rc<Chat>, target: Option<(String, String)>) {
    match target {
        Some((id, label)) => {
            *chat.replying_to.borrow_mut() = Some(id);
            chat.reply_label.set_label(&format!("Replying to {label}"));
            chat.reply_bar.set_reveal_child(true);
            chat.composer.grab_focus();
        }
        None => {
            *chat.replying_to.borrow_mut() = None;
            chat.reply_bar.set_reveal_child(false);
        }
    }
}

/// Mark a channel read (up to `message_id`, or its latest) on the server.
fn mark_read(chat: &Rc<Chat>, channel_id: String, message_id: Option<String>) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let _ = chat
            .runtime
            .spawn({
                let client = chat.client.clone();
                async move { client.mark_read(&channel_id, message_id.as_deref()).await }
            })
            .await;
    });
}

/// The "+" popover: open a DM by handle, or (admins) create a channel.
fn new_conversation_popover(chat: &Rc<Chat>) -> gtk::Popover {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    // New DM
    let dm_entry = gtk::Entry::builder().placeholder_text("handle").build();
    let dm_button = gtk::Button::with_label("Open DM");
    column.append(
        &gtk::Label::builder()
            .label("New direct message")
            .xalign(0.0)
            .build(),
    );
    column.append(&dm_entry);
    column.append(&dm_button);

    let popover = gtk::Popover::builder().child(&column).build();

    dm_button.connect_clicked({
        let chat = chat.clone();
        let dm_entry = dm_entry.clone();
        let popover = popover.clone();
        move |_| {
            let handle = dm_entry.text().trim().to_string();
            if handle.is_empty() {
                return;
            }
            dm_entry.set_text("");
            popover.popdown();
            open_dm(&chat, handle);
        }
    });

    if *chat.is_admin.borrow() {
        let name_entry = gtk::Entry::builder().placeholder_text("name").build();
        let create_button = gtk::Button::with_label("Create channel");
        column.append(&gtk::Separator::new(gtk::Orientation::Horizontal));
        column.append(
            &gtk::Label::builder()
                .label("New channel (admin)")
                .xalign(0.0)
                .build(),
        );
        column.append(&name_entry);
        column.append(&create_button);

        create_button.connect_clicked({
            let chat = chat.clone();
            let name_entry = name_entry.clone();
            let popover = popover.clone();
            move |_| {
                let name = name_entry.text().trim().to_string();
                if name.is_empty() {
                    return;
                }
                name_entry.set_text("");
                popover.popdown();
                create_channel(&chat, name);
            }
        });
    }

    popover
}

fn open_dm(chat: &Rc<Chat>, handle: String) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let join = chat.runtime.spawn({
            let client = chat.client.clone();
            async move { client.open_dm(&handle).await }
        });
        if let Ok(Ok(channel)) = join.await {
            refresh_channels(&chat, Some(channel.id));
        }
    });
}

fn create_channel(chat: &Rc<Chat>, name: String) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let join = chat.runtime.spawn({
            let client = chat.client.clone();
            async move { client.create_channel(&name, None).await }
        });
        if let Ok(Ok(channel)) = join.await {
            refresh_channels(&chat, Some(channel.id));
        }
    });
}

/// Popover to add a member (by handle) to the currently-selected channel.
fn add_member_popover(chat: &Rc<Chat>) -> gtk::Popover {
    let entry = gtk::Entry::builder().placeholder_text("handle").build();
    let button = gtk::Button::with_label("Add to channel");
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    column.append(
        &gtk::Label::builder()
            .label("Add member")
            .xalign(0.0)
            .build(),
    );
    column.append(&entry);
    column.append(&button);
    let popover = gtk::Popover::builder().child(&column).build();

    button.connect_clicked({
        let chat = chat.clone();
        let entry = entry.clone();
        let popover = popover.clone();
        move |_| {
            let handle = entry.text().trim().to_string();
            let Some(channel_id) = chat.current.borrow().clone() else {
                return;
            };
            if handle.is_empty() {
                return;
            }
            entry.set_text("");
            popover.popdown();
            let chat = chat.clone();
            glib::spawn_future_local(async move {
                let join = chat.runtime.spawn({
                    let client = chat.client.clone();
                    async move { client.add_member(&channel_id, &handle).await }
                });
                // The server fans out channel.update; the new member's client
                // refreshes itself. Reload ours too so the member count updates.
                if let Ok(Ok(())) = join.await {
                    refresh_channels(&chat, None);
                }
            });
        }
    });
    popover
}
