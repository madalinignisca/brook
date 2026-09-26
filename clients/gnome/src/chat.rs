//! The post-login chat view: a sidebar of channels/DMs, a message list, and a
//! composer — over `brook-core`. Networking runs on the Tokio runtime; results
//! are applied on the GTK main loop (await a runtime `JoinHandle` inside
//! `spawn_future_local`). Realtime `message.new` events are consumed from the
//! core's broadcast channel on the main loop and appended live.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use brook_core::{
    BrookClient, CacheEvent, Channel, Deleted, Message, PendingMessage, PendingState,
    ReactionSummary, ServerEvent,
};
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
    /// Channel settings menu (rename/archive/delete); shown for managed channels.
    channel_settings: gtk::MenuButton,
    /// "X is typing…" indicator above the composer + its auto-clear timeout.
    typing_label: gtk::Label,
    typing_timeout: Rc<RefCell<Option<glib::SourceId>>>,
    /// Last time we sent a typing signal (to throttle to ~once per few seconds).
    last_typing: Rc<RefCell<Option<std::time::Instant>>>,
    message_list: gtk::ListBox,
    message_scroll: gtk::ScrolledWindow,
    title: adw::WindowTitle,
    composer: gtk::Entry,
    send_button: gtk::Button,
    /// Start/join the open channel's call; its label follows `channel.call`.
    call_button: gtk::Button,
    /// channel id -> participants in its ongoing call (from `channel.call`).
    active_calls: Rc<RefCell<HashMap<String, u32>>>,
    /// The open call window, if any (one call at a time).
    call_window: Rc<RefCell<Option<glib::WeakRef<adw::Window>>>>,
    /// Sign Out in the main menu: set by the app shell (it knows the login view).
    /// `true` also erases this device's data for the user ("Remove this device's data").
    sign_out: Rc<dyn Fn(bool)>,
    /// The open channel's queued (not yet sent) messages, drawn below the history.
    pending_rows: Rc<RefCell<Vec<gtk::ListBoxRow>>>,
    /// `client_id`s of messages already on screen: their pending bubble is dropped.
    shown_client_ids: Rc<RefCell<std::collections::HashSet<String>>>,
    /// "You're offline" above the messages, from the cache's state.
    offline_banner: adw::Banner,
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
    /// Shown as a tombstone (the message was deleted).
    deleted: Rc<Cell<bool>>,
    /// The message carries files (its text may then be empty).
    has_files: bool,
    /// The message text as sent (markdown, not the rendered markup), for editing.
    source: Rc<RefCell<String>>,
    /// Hidden once the message is a tombstone: actions, quote, files, reactions.
    extras: Vec<gtk::Widget>,
    /// The quoted message's id, its author and the quote line, if this is a reply.
    quote: Option<(String, String, gtk::Label)>,
}

/// Build the chat view. `is_admin` controls whether channel creation is offered.
pub fn build(
    client: Arc<BrookClient>,
    runtime: Handle,
    is_admin: bool,
    sign_out: Rc<dyn Fn(bool)>,
) -> gtk::Widget {
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

    // Bare button now (popover wired after `chat` exists); shown per channel.
    let channel_settings = gtk::MenuButton::builder()
        .icon_name("emblem-system-symbolic")
        .tooltip_text("Channel settings")
        .visible(false)
        .build();

    let typing_label = gtk::Label::builder()
        .xalign(0.0)
        .visible(false)
        .margin_start(12)
        .css_classes(["caption", "dim-label"])
        .build();

    let call_button = gtk::Button::builder()
        .icon_name("call-start-symbolic")
        .tooltip_text("Start a call")
        .sensitive(false)
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
        channel_settings: channel_settings.clone(),
        typing_label: typing_label.clone(),
        typing_timeout: Rc::new(RefCell::new(None)),
        last_typing: Rc::new(RefCell::new(None)),
        message_list: message_list.clone(),
        message_scroll: message_scroll.clone(),
        title: title.clone(),
        composer: composer.clone(),
        send_button: send_button.clone(),
        call_button: call_button.clone(),
        active_calls: Rc::default(),
        call_window: Rc::default(),
        sign_out,
        pending_rows: Rc::default(),
        shown_client_ids: Rc::default(),
        offline_banner: adw::Banner::builder()
            .title("You're offline. Showing saved messages.")
            .revealed(false)
            .build(),
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
    let search_button = gtk::Button::builder()
        .icon_name("system-search-symbolic")
        .tooltip_text("Search messages")
        .build();
    sidebar_header.pack_end(&search_button);
    let main_menu = gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Main menu")
        .popover(&main_menu_popover(&chat))
        .build();
    sidebar_header.pack_end(&main_menu);
    search_button.connect_clicked({
        let chat = chat.clone();
        move |_| search_dialog(&chat)
    });

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
    channel_settings.set_popover(Some(&channel_settings_popover(&chat)));
    content_header.pack_end(&channel_settings);
    content_header.pack_start(&call_button);
    call_button.connect_clicked({
        let chat = chat.clone();
        move |button| open_call(&chat, button)
    });

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
    content_box.append(&chat.offline_banner);
    content_box.append(&message_scroll);
    content_box.append(&typing_label);
    content_box.append(&reply_bar);
    content_box.append(&composer_row);

    composer.connect_changed({
        let chat = chat.clone();
        move |entry| {
            if !entry.text().is_empty() {
                maybe_send_typing(&chat);
            }
        }
    });

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
        spawn_cache_loop(&chat);
        watch_offline(&chat);
        refresh_channels(&chat, None);
    });
}

/// Consume realtime events on the GTK main loop, appending live messages.
fn spawn_event_loop(chat: &Rc<Chat>) {
    let chat = chat.clone();
    let mut events = chat.client.events();
    glib::spawn_future_local(async move {
        loop {
            let event = events.recv().await;
            // The view was torn down (signed out mid-session): stop, or a
            // rebuilt view on the same client would double-handle every event
            // (duplicate notifications).
            if chat.message_list.root().is_none() {
                break;
            }
            match event {
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
                            let mentioned =
                                message.mention_everyone || message.mentions.contains(&me);
                            let body = if mentioned {
                                format!("{author} mentioned you: {}", message.body)
                            } else {
                                format!("{author}: {}", message.body)
                            };
                            notify(&message.channel_id, &title, &body);
                        }
                    }
                }
                Ok(ServerEvent::MessageUpdate(message)) => {
                    // Update the row in place if the edited message is on screen.
                    let widgets = chat.message_rows.borrow().get(&message.id).cloned();
                    if let Some(widgets) = widgets {
                        widgets.body.set_markup(&markdown_to_pango(&message.body));
                        widgets.source.replace(message.body.clone());
                        // An edit can add or clear a file message's caption (the server refuses
                        // a blank edit on a message without files), so follow the new text.
                        widgets.body.set_visible(!message.body.trim().is_empty());
                        widgets.edited.set_visible(true);
                    }
                }
                Ok(ServerEvent::MessageDelete { message_id, .. }) => {
                    // The row stays as a tombstone, as history and the cache show it.
                    let shown = chat.message_rows.borrow().get(&message_id).cloned();
                    if let Some(widgets) = shown {
                        show_deleted(&widgets);
                    }
                    mark_quotes_deleted(&chat, &message_id);
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
                Ok(ServerEvent::ChannelDelete { channel_id }) => {
                    // If the open channel was deleted, clear the conversation view.
                    if chat.current.borrow().as_deref() == Some(channel_id.as_str()) {
                        *chat.current.borrow_mut() = None;
                        chat.message_rows.borrow_mut().clear();
                        while let Some(row) = chat.message_list.row_at_index(0) {
                            chat.message_list.remove(&row);
                        }
                        chat.title.set_title("Brook");
                        chat.title.set_subtitle("Pick a conversation");
                        chat.composer.set_sensitive(false);
                        chat.send_button.set_sensitive(false);
                        chat.channel_settings.set_visible(false);
                        chat.call_button.set_sensitive(false);
                    }
                    refresh_channels(&chat, None);
                }
                Ok(ServerEvent::Typing {
                    channel_id,
                    user_id,
                    display_name,
                }) => {
                    let me = chat.me.borrow().clone().unwrap_or_default();
                    let is_current = chat.current.borrow().as_deref() == Some(channel_id.as_str());
                    if is_current && user_id != me {
                        show_typing(&chat, &display_name);
                    }
                }
                Ok(ServerEvent::ChannelUpdate(_)) => {
                    // Added to / removed from a channel, or metadata changed:
                    // reload the sidebar so it reflects the change live.
                    refresh_channels(&chat, None);
                }
                Ok(ServerEvent::ChannelCall {
                    channel_id,
                    call_id,
                    participant_count,
                    ..
                }) => {
                    {
                        let mut calls = chat.active_calls.borrow_mut();
                        match call_id {
                            Some(_) if participant_count > 0 => {
                                calls.insert(channel_id, participant_count);
                            }
                            _ => {
                                calls.remove(&channel_id);
                            }
                        }
                    }
                    refresh_call_button(&chat);
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
        // The network is freshest (server-side unread counts); offline, the cache
        // answers with the last-synced list and locally computed unread counts.
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            async move {
                match client.list_channels().await {
                    Ok(channels) => Ok(channels),
                    Err(err) => client.cached_channels().await.map_err(|_| err),
                }
            }
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

        // Re-apply chrome for the open channel so a live rename/archive shows now.
        if let Some(current) = chat.current.borrow().clone() {
            apply_channel_chrome(&chat, &current);
        }

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
/// Apply the title, subtitle, settings-button visibility, and composer
/// sensitivity for `channel_id` from the current channel list (re-applied on
/// reload so a live archive/rename of the open channel takes effect immediately).
fn apply_channel_chrome(chat: &Rc<Chat>, channel_id: &str) {
    let me = chat.me.borrow().clone().unwrap_or_default();
    let meta = chat
        .channels
        .borrow()
        .iter()
        .find(|c| c.id == channel_id)
        .map(|c| (c.is_dm(), c.archived, c.title(&me)));
    let Some((is_dm, archived, title)) = meta else {
        return;
    };
    chat.title.set_title(&title);
    chat.title.set_subtitle(if is_dm {
        "Direct message"
    } else if archived {
        "Channel · archived"
    } else {
        "Channel"
    });
    chat.channel_settings
        .set_visible(!is_dm && *chat.is_admin.borrow());
    chat.composer.set_sensitive(!archived);
    chat.send_button.set_sensitive(!archived);
    // Archived channels refuse call.join.
    chat.call_button.set_sensitive(!archived);
    refresh_call_button(chat);
}

/// "Start a call" vs "Join call (N)" for the open channel.
fn refresh_call_button(chat: &Rc<Chat>) {
    let current = chat.current.borrow().clone();
    let count = current
        .as_ref()
        .and_then(|c| chat.active_calls.borrow().get(c).copied());
    match count {
        Some(n) => {
            chat.call_button.set_tooltip_text(Some(&format!(
                "Join call ({n} {})",
                if n == 1 { "person" } else { "people" }
            )));
            chat.call_button.add_css_class("success");
        }
        None => {
            chat.call_button.set_tooltip_text(Some("Start a call"));
            chat.call_button.remove_css_class("success");
        }
    }
}

/// Start or join the open channel's call in its own window (one at a time).
fn open_call(chat: &Rc<Chat>, button: &gtk::Button) {
    if let Some(window) = chat.call_window.borrow().as_ref().and_then(|w| w.upgrade()) {
        window.present();
        return;
    }
    let Some(channel_id) = chat.current.borrow().clone() else {
        return;
    };
    let title = chat.title.title().to_string();
    let parent = button.root().and_downcast::<gtk::Window>();
    let window = crate::call::open_call(
        parent.as_ref(),
        chat.client.clone(),
        chat.runtime.clone(),
        channel_id,
        &title,
    );
    *chat.call_window.borrow_mut() = Some(window.downgrade());
}

fn select_channel(chat: &Rc<Chat>, channel_id: &str) {
    *chat.current.borrow_mut() = Some(channel_id.to_string());
    apply_channel_chrome(chat, channel_id);

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
    clear_typing(chat);
    // Clear now, before the await, so live messages that arrive while history is
    // loading are appended to a fresh list rather than wiped by a late clear.
    chat.message_rows.borrow_mut().clear();
    chat.pending_rows.borrow_mut().clear();
    chat.shown_client_ids.borrow_mut().clear();
    while let Some(row) = chat.message_list.row_at_index(0) {
        chat.message_list.remove(&row);
    }

    let chat = chat.clone();
    let channel_id = channel_id.to_string();
    glib::spawn_future_local(async move {
        let is_current =
            |chat: &Rc<Chat>| chat.current.borrow().as_deref() == Some(channel_id.as_str());
        // Saved messages first (instant, and all there is offline), newest first
        // from the cache, drawn oldest first. A page the cache can't prove complete
        // fetches the newest into the cache; the cache event then fills it in.
        let cached = chat.runtime.spawn({
            let client = chat.client.clone();
            let channel_id = channel_id.clone();
            async move {
                let mut page = client.cached_messages(&channel_id, None, 50).await?;
                if page.needs_network && client.load_head(&channel_id, 50).await.is_ok() {
                    // Re-read: the head fetch filled the cache (drawing the stale page
                    // first would leave older rows arriving after newer ones).
                    page = client.cached_messages(&channel_id, None, 50).await?;
                }
                Ok::<_, brook_core::Error>(page.messages)
            }
        });
        if let Ok(Ok(mut messages)) = cached.await {
            if !is_current(&chat) {
                return;
            }
            messages.reverse();
            for message in &messages {
                append_message(&chat, message);
            }
        }
        // Then the network, as before (duplicates replace their cached row).
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            let channel_id = channel_id.clone();
            async move { client.channel_history(&channel_id, None).await }
        });
        if let Ok(Ok(messages)) = handle.await {
            if !is_current(&chat) {
                return;
            }
            for message in &messages {
                if !chat.message_rows.borrow().contains_key(&message.id) {
                    append_message(&chat, message);
                }
            }
        }
        if is_current(&chat) {
            render_pending(&chat);
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
    // "Replying to …", kept to restore the reply if the send fails.
    let reply_label = chat
        .reply_label
        .label()
        .strip_prefix("Replying to ")
        .map(str::to_string)
        .unwrap_or_default();
    set_reply(chat, None);

    let chat = chat.clone();
    glib::spawn_future_local(async move {
        // Through the outbox when offline storage is on (replies too): saved before
        // this returns, sent in order, shown as a "sending" bubble until it arrives.
        // Without local storage it sends directly, online.
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            let (body, reply_to) = (body.clone(), reply_to.clone());
            async move {
                match client
                    .send_queued(&channel_id, &body, reply_to.clone(), None)
                    .await
                {
                    Ok(_) => return Ok(true),
                    Err(brook_core::Error::Api { code, .. }) if code == "local.unavailable" => {}
                    Err(err) => return Err(err),
                }
                client
                    .send_message(&channel_id, &body, reply_to.as_deref())
                    .await
                    .map(|_| false)
            }
        });
        // A panicked or cancelled task is a failure too: nothing was confirmed sent.
        match handle
            .await
            .unwrap_or(Err(brook_core::Error::UnexpectedResponse))
        {
            Ok(true) => render_pending(&chat),
            Ok(false) => {}
            Err(err) => {
                tracing::warn!(%err, "failed to send message");
                // Nothing was sent: give the text (and the reply) back, unless the
                // user already started typing something new, and say why.
                if chat.composer.text().is_empty() {
                    chat.composer.set_text(&body);
                    chat.composer.set_position(-1);
                    if let Some(id) = reply_to {
                        set_reply(&chat, Some((id, reply_label)));
                    }
                }
                show_send_error(&chat, &send_error_text(&err));
            }
        }
    });
}

/// Convert a markdown message body to Pango markup (bold / italic / inline code /
/// code block / link / strikethrough). Text is escaped; raw HTML is dropped.
fn markdown_to_pango(text: &str) -> String {
    use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    let mut out = String::new();
    let mut in_code = false;
    for event in Parser::new_ext(text, options) {
        match event {
            Event::Start(Tag::Strong) => out.push_str("<b>"),
            Event::Start(Tag::Emphasis) => out.push_str("<i>"),
            Event::Start(Tag::Strikethrough) => out.push_str("<s>"),
            Event::Start(Tag::CodeBlock(_)) => {
                in_code = true;
                out.push_str("<tt>");
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                out.push_str("<a href=\"");
                out.push_str(glib::markup_escape_text(&dest_url).as_str());
                out.push_str("\">");
            }
            Event::Start(Tag::Item) => out.push_str("\u{2022} "),
            Event::End(TagEnd::Strong) => out.push_str("</b>"),
            Event::End(TagEnd::Emphasis) => out.push_str("</i>"),
            Event::End(TagEnd::Strikethrough) => out.push_str("</s>"),
            Event::End(TagEnd::CodeBlock) => {
                in_code = false;
                out.push_str("</tt>");
            }
            Event::End(TagEnd::Link) => out.push_str("</a>"),
            // Blank line between paragraphs; newline after each list item.
            Event::End(TagEnd::Paragraph) => out.push_str("\n\n"),
            Event::End(TagEnd::Item) => out.push('\n'),
            Event::Text(t) if in_code => out.push_str(glib::markup_escape_text(&t).as_str()),
            // Highlight @mentions in normal text (not inside code).
            Event::Text(t) => push_with_mentions(&mut out, &t, |m| {
                format!(
                    "<span foreground=\"#3584e4\" weight=\"bold\">{}</span>",
                    glib::markup_escape_text(m)
                )
            }),
            Event::Code(t) => {
                out.push_str("<tt>");
                out.push_str(glib::markup_escape_text(&t).as_str());
                out.push_str("</tt>");
            }
            Event::SoftBreak | Event::HardBreak => out.push('\n'),
            _ => {}
        }
    }
    out.trim().to_string()
}

/// Push `text` into `out`, escaping plain runs and wrapping `@mention` tokens with
/// `wrap` (which receives the raw token and returns escaped, marked-up output).
fn push_with_mentions(out: &mut String, text: &str, wrap: impl Fn(&str) -> String) {
    let chars: Vec<char> = text.chars().collect();
    // Handles may contain '.'/'-'; highlight is cosmetic so we don't trim trailing
    // punctuation (the server resolves notifications precisely).
    let is_word = |c: char| c.is_alphanumeric() || c == '_' || c == '.' || c == '-';
    let mut i = 0;
    let mut plain_start = 0;
    while i < chars.len() {
        let boundary = i == 0 || !(is_word(chars[i - 1]) || chars[i - 1] == '@');
        if chars[i] == '@' && boundary && chars.get(i + 1).is_some_and(|c| is_word(*c)) {
            let plain: String = chars[plain_start..i].iter().collect();
            out.push_str(glib::markup_escape_text(&plain).as_str());
            let start = i;
            i += 1;
            while i < chars.len() && is_word(chars[i]) {
                i += 1;
            }
            let token: String = chars[start..i].iter().collect();
            out.push_str(&wrap(&token));
            plain_start = i;
        } else {
            i += 1;
        }
    }
    let plain: String = chars[plain_start..].iter().collect();
    out.push_str(glib::markup_escape_text(&plain).as_str());
}

/// Whether a link URI is safe to hand to the system opener (no `file:`, `smb:`,
/// `javascript:`, etc. — only web + mail).
fn is_safe_link(uri: &str) -> bool {
    let lower = uri.to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://") || lower.starts_with("mailto:")
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
    let actions = message_actions_button(chat, message, is_own);
    header.append(&actions);
    let mut extras: Vec<gtk::Widget> = vec![actions.upcast(), edited_label.clone().upcast()];

    let body_label = gtk::Label::builder()
        .label(markdown_to_pango(&message.body))
        .use_markup(true)
        .xalign(0.0)
        .wrap(true)
        .selectable(true)
        .build();
    body_label.connect_activate_link(|_, uri| {
        if is_safe_link(uri) {
            let _ =
                gtk::gio::AppInfo::launch_default_for_uri(uri, gtk::gio::AppLaunchContext::NONE);
        } else {
            tracing::warn!(%uri, "refusing to open link with an unsafe scheme");
        }
        glib::Propagation::Stop
    });
    row.append(&header);
    // Quoted-reply preview above the body, if this message is a reply.
    let mut quote_widgets = None;
    if let Some(reply) = &message.reply_to {
        let who = reply
            .author_display_name
            .clone()
            .or_else(|| reply.author_handle.clone())
            .unwrap_or_else(|| "Unknown".to_string());
        let quote = gtk::Label::builder()
            .label(quote_text(
                &who,
                &reply.body,
                reply.deleted,
                reply.attachments,
            ))
            .xalign(0.0)
            .wrap(true)
            .css_classes(["caption", "dim-label"])
            .build();
        row.append(&quote);
        extras.push(quote.clone().upcast());
        quote_widgets = Some((reply.id.clone(), who, quote));
    }
    // A file sent without a caption has no text line (the server allows an empty body
    // when files are attached).
    body_label.set_visible(!(message.body.trim().is_empty() && !message.attachments.is_empty()));
    row.append(&body_label);
    // Attached files (a tombstone has none): shown, and saved only on request.
    for file in &message.attachments {
        let file_row =
            crate::attachments::attachment_row(file, chat.client.clone(), chat.runtime.clone());
        row.append(&file_row);
        extras.push(file_row.upcast());
    }

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
    extras.push(reactions_wrapper.upcast());

    let list_row = gtk::ListBoxRow::builder()
        .activatable(false)
        .child(&row)
        .build();
    // De-dupe: if this id is already on screen (history + WS echo can overlap),
    // drop the old row so edit/delete only ever tracks one.
    if let Some(old) = chat.message_rows.borrow_mut().remove(&message.id) {
        chat.message_list.remove(&old.row);
    }
    // In time order whatever the source (cache, history page, live event): ids are
    // UUIDv7, lowercase, so string order is time order. Queued bubbles stay after them.
    let position = insert_position(chat.message_rows.borrow().keys(), &message.id);
    chat.message_list.insert(&list_row, position as i32);
    if let Some(cid) = &message.client_id {
        chat.shown_client_ids.borrow_mut().insert(cid.clone());
        drop_pending_bubble(chat, cid);
    }
    keep_pending_last(chat);
    let widgets = MessageWidgets {
        row: list_row,
        body: body_label,
        edited: edited_label,
        channel_id: message.channel_id.clone(),
        reactions_box,
        reactions: Rc::new(RefCell::new(message.reactions.clone())),
        deleted: Rc::new(Cell::new(false)),
        has_files: !message.attachments.is_empty(),
        source: Rc::new(RefCell::new(message.body.clone())),
        extras,
        quote: quote_widgets,
    };
    if message.is_deleted() {
        show_deleted(&widgets);
    }
    chat.message_rows
        .borrow_mut()
        .insert(message.id.clone(), widgets);
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
                .map(|w| (w.source.borrow().clone(), w.has_files))
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
/// `current` is the text as sent and whether the message has files: a file message's
/// caption may be cleared, a text-only message can't be emptied.
fn edit_message_dialog(
    chat: &Rc<Chat>,
    channel_id: String,
    message_id: String,
    current: (String, bool),
) {
    let (current, has_files) = current;
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
            if body.trim().is_empty() && !has_files {
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

/// The channel-settings menu: rename / archive / unarchive / delete (on `current`).
/// The sidebar's main menu: account settings.
fn main_menu_popover(chat: &Rc<Chat>) -> gtk::Popover {
    let popover = gtk::Popover::new();
    let change_password = gtk::Button::builder()
        .label("Change Password…")
        .has_frame(false)
        .build();
    let menu = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();
    let two_factor = gtk::Button::builder()
        .label("Two-Factor Sign-In…")
        .has_frame(false)
        .build();
    let sign_out = gtk::Button::builder()
        .label("Sign Out")
        .has_frame(false)
        .build();
    menu.append(&change_password);
    menu.append(&two_factor);
    menu.append(&sign_out);
    two_factor.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            crate::totp_ui::settings_dialog(
                &chat.message_list,
                chat.client.clone(),
                chat.runtime.clone(),
            );
        }
    });
    popover.set_child(Some(&menu));
    sign_out.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            sign_out_dialog(&chat);
        }
    });
    change_password.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            crate::account::change_password_dialog(
                &chat.message_list,
                chat.client.clone(),
                chat.runtime.clone(),
            );
        }
    });
    popover
}

fn channel_settings_popover(chat: &Rc<Chat>) -> gtk::Popover {
    let menu = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(4)
        .margin_end(4)
        .build();
    let popover = gtk::Popover::builder().build();
    let rename = gtk::Button::builder()
        .label("Rename…")
        .has_frame(false)
        .build();
    let archive = gtk::Button::builder()
        .label("Archive")
        .has_frame(false)
        .build();
    let unarchive = gtk::Button::builder()
        .label("Unarchive")
        .has_frame(false)
        .build();
    let delete = gtk::Button::builder()
        .label("Delete channel")
        .has_frame(false)
        .css_classes(["error"])
        .build();
    menu.append(&rename);
    menu.append(&archive);
    menu.append(&unarchive);
    menu.append(&delete);
    popover.set_child(Some(&menu));

    rename.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            rename_channel_dialog(&chat);
        }
    });
    archive.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            update_channel_async(&chat, None, Some(true));
        }
    });
    unarchive.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            update_channel_async(&chat, None, Some(false));
        }
    });
    delete.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            delete_channel_confirm(&chat);
        }
    });
    popover
}

/// PATCH the current channel (rename/archive). The `channel.update` echo refreshes.
fn update_channel_async(chat: &Rc<Chat>, name: Option<String>, archived: Option<bool>) {
    let Some(channel_id) = chat.current.borrow().clone() else {
        return;
    };
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let result = chat
            .runtime
            .spawn({
                let client = chat.client.clone();
                async move {
                    client
                        .update_channel(&channel_id, name.as_deref(), None, archived)
                        .await
                }
            })
            .await;
        if let Ok(Err(err)) = result {
            tracing::warn!(%err, "failed to update channel");
        }
    });
}

/// Rename dialog, prefilled with the current channel's name.
fn rename_channel_dialog(chat: &Rc<Chat>) {
    let Some(channel_id) = chat.current.borrow().clone() else {
        return;
    };
    let me = chat.me.borrow().clone().unwrap_or_default();
    let current_name = chat
        .channels
        .borrow()
        .iter()
        .find(|c| c.id == channel_id)
        .map(|c| c.title(&me))
        .unwrap_or_default();
    let entry = gtk::Entry::builder()
        .text(&current_name)
        .hexpand(true)
        .build();
    let dialog = adw::AlertDialog::builder()
        .heading("Rename channel")
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
            let name = entry.text().to_string();
            if !name.trim().is_empty() {
                update_channel_async(&chat, Some(name), None);
            }
        }
    });
    dialog.present(Some(&chat.message_list));
}

/// Confirm + delete the current channel (the `channel.delete` echo removes it).
fn delete_channel_confirm(chat: &Rc<Chat>) {
    let Some(channel_id) = chat.current.borrow().clone() else {
        return;
    };
    let dialog = adw::AlertDialog::new(
        Some("Delete channel?"),
        Some("This permanently deletes the channel and its messages."),
    );
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
            glib::spawn_future_local(async move {
                let result = chat
                    .runtime
                    .spawn({
                        let client = chat.client.clone();
                        async move { client.delete_channel(&channel_id).await }
                    })
                    .await;
                if let Ok(Err(err)) = result {
                    tracing::warn!(%err, "failed to delete channel");
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

/// Tell the server we're typing in the current channel, throttled to ~once / 3s.
fn maybe_send_typing(chat: &Rc<Chat>) {
    let Some(channel_id) = chat.current.borrow().clone() else {
        return;
    };
    let now = std::time::Instant::now();
    let should_send = {
        let mut last = chat.last_typing.borrow_mut();
        if last.is_none_or(|t| now.duration_since(t).as_secs() >= 3) {
            *last = Some(now);
            true
        } else {
            false
        }
    };
    if should_send {
        let chat = chat.clone();
        glib::spawn_future_local(async move {
            let _ = chat
                .runtime
                .spawn({
                    let client = chat.client.clone();
                    async move { client.send_typing(&channel_id).await }
                })
                .await;
        });
    }
}

/// Show "<name> is typing…" and (re)start the 4s auto-clear.
fn show_typing(chat: &Rc<Chat>, name: &str) {
    chat.typing_label
        .set_label(&format!("{name} is typing\u{2026}"));
    chat.typing_label.set_visible(true);
    if let Some(id) = chat.typing_timeout.borrow_mut().take() {
        id.remove();
    }
    let chat2 = chat.clone();
    let id = glib::timeout_add_seconds_local(4, move || {
        chat2.typing_label.set_visible(false);
        *chat2.typing_timeout.borrow_mut() = None;
        glib::ControlFlow::Break
    });
    *chat.typing_timeout.borrow_mut() = Some(id);
}

/// Clear any typing indicator (e.g. on channel switch).
fn clear_typing(chat: &Rc<Chat>) {
    chat.typing_label.set_visible(false);
    if let Some(id) = chat.typing_timeout.borrow_mut().take() {
        id.remove();
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

    // Browse + self-join public channels (any user).
    let browse_button = gtk::Button::with_label("Browse public channels");
    column.append(&browse_button);

    let popover = gtk::Popover::builder().child(&column).build();

    browse_button.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            browse_public_channels(&chat);
        }
    });

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
        let public_check = gtk::CheckButton::with_label("Public (anyone can join)");
        column.append(&name_entry);
        column.append(&public_check);
        column.append(&create_button);

        create_button.connect_clicked({
            let chat = chat.clone();
            let name_entry = name_entry.clone();
            let public_check = public_check.clone();
            let popover = popover.clone();
            move |_| {
                let name = name_entry.text().trim().to_string();
                if name.is_empty() {
                    return;
                }
                let public = public_check.is_active();
                name_entry.set_text("");
                public_check.set_active(false);
                popover.popdown();
                create_channel(&chat, name, public);
            }
        });
    }

    popover
}

/// Fetch + present the public channels, each with a Join button.
fn browse_public_channels(chat: &Rc<Chat>) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            async move { client.list_public_channels().await }
        });
        let Ok(Ok(channels)) = handle.await else {
            return;
        };
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        let dialog = adw::AlertDialog::builder()
            .heading("Public channels")
            .build();
        if channels.is_empty() {
            list.append(&gtk::Label::new(Some("No public channels to join.")));
        }
        for channel in channels {
            let row = gtk::Box::builder()
                .orientation(gtk::Orientation::Horizontal)
                .spacing(6)
                .build();
            let name = channel
                .name
                .clone()
                .unwrap_or_else(|| "channel".to_string());
            row.append(
                &gtk::Label::builder()
                    .label(&name)
                    .hexpand(true)
                    .xalign(0.0)
                    .build(),
            );
            let join = gtk::Button::with_label("Join");
            join.connect_clicked({
                let chat = chat.clone();
                let dialog = dialog.clone();
                let channel_id = channel.id.clone();
                move |btn| {
                    btn.set_sensitive(false);
                    join_public(&chat, channel_id.clone());
                    dialog.close();
                }
            });
            row.append(&join);
            list.append(&row);
        }
        dialog.set_extra_child(Some(&list));
        dialog.add_response("close", "Close");
        dialog.present(Some(&chat.message_list));
    });
}

fn join_public(chat: &Rc<Chat>, channel_id: String) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            async move { client.join_channel(&channel_id).await }
        });
        if let Ok(Ok(channel)) = handle.await {
            refresh_channels(&chat, Some(channel.id));
        }
    });
}

/// A search dialog: type a term, see matching messages, click one to jump to it.
fn search_dialog(chat: &Rc<Chat>) {
    let entry = gtk::SearchEntry::builder().hexpand(true).build();
    let results = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(4)
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .min_content_height(280)
        .min_content_width(360)
        .child(&results)
        .build();
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(8)
        .build();
    column.append(&entry);
    column.append(&scroller);
    let dialog = adw::AlertDialog::builder()
        .heading("Search messages")
        .extra_child(&column)
        .build();
    dialog.add_response("close", "Close");

    // Generation guard: an older, slower search must not overwrite newer results.
    let generation = Rc::new(std::cell::Cell::new(0u64));
    // Weak so the entry's closure doesn't form a cycle that leaks the dialog.
    let dialog_weak = dialog.downgrade();
    entry.connect_activate({
        let chat = chat.clone();
        let results = results.clone();
        let generation = generation.clone();
        move |entry| {
            let query = entry.text().trim().to_string();
            if query.is_empty() {
                return;
            }
            let Some(dialog) = dialog_weak.upgrade() else {
                return;
            };
            run_search(&chat, query, results.clone(), dialog, generation.clone());
        }
    });
    dialog.present(Some(&chat.message_list));
    entry.grab_focus();
}

/// Run a search and populate the results box; each result jumps to its channel.
fn run_search(
    chat: &Rc<Chat>,
    query: String,
    results: gtk::Box,
    dialog: adw::AlertDialog,
    generation: Rc<std::cell::Cell<u64>>,
) {
    let this_gen = generation.get().wrapping_add(1);
    generation.set(this_gen);
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            async move { client.search_messages(&query).await }
        });
        let Ok(Ok(messages)) = handle.await else {
            return;
        };
        // A newer search has been issued since — drop these stale results.
        if generation.get() != this_gen {
            return;
        }
        while let Some(child) = results.first_child() {
            results.remove(&child);
        }
        if messages.is_empty() {
            results.append(&gtk::Label::new(Some("No matches.")));
            return;
        }
        let me = chat.me.borrow().clone().unwrap_or_default();
        for message in messages {
            let channel_name = chat
                .channels
                .borrow()
                .iter()
                .find(|c| c.id == message.channel_id)
                .map(|c| c.title(&me))
                .unwrap_or_else(|| "channel".to_string());
            let author = message
                .author_display_name
                .clone()
                .or_else(|| message.author_handle.clone())
                .unwrap_or_else(|| "?".to_string());
            let button = gtk::Button::builder()
                .label(format!("{channel_name} · {author}: {}", message.body))
                .has_frame(false)
                .build();
            if let Some(label) = button.child().and_downcast::<gtk::Label>() {
                label.set_xalign(0.0);
                label.set_wrap(true);
            }
            let dialog_weak = dialog.downgrade();
            button.connect_clicked({
                let chat = chat.clone();
                let channel_id = message.channel_id.clone();
                move |_| {
                    if let Some(dialog) = dialog_weak.upgrade() {
                        dialog.close();
                    }
                    jump_to_channel(&chat, &channel_id);
                }
            });
            results.append(&button);
        }
    });
}

/// Select a channel's sidebar row (which opens it via `connect_row_selected`).
fn jump_to_channel(chat: &Rc<Chat>, channel_id: &str) {
    let idx = chat
        .channels
        .borrow()
        .iter()
        .position(|c| c.id == channel_id);
    if let Some(idx) = idx {
        if let Some(row) = chat.channel_list.row_at_index(idx as i32) {
            chat.channel_list.select_row(Some(&row));
        }
    }
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

fn create_channel(chat: &Rc<Chat>, name: String, public: bool) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let join = chat.runtime.spawn({
            let client = chat.client.clone();
            async move {
                if public {
                    client.create_public_channel(&name).await
                } else {
                    client.create_channel(&name, None).await
                }
            }
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

// ------------------------------------------------------------------ offline (#62)

/// The words under a queued message.
fn pending_text(state: &PendingState) -> String {
    match state {
        PendingState::Pending | PendingState::Sending | PendingState::Accepted => "Sending…".into(),
        PendingState::Failed { code } => match code.as_str() {
            "not_found" | "authz.forbidden" | "http_403" | "http_404" => {
                "Not sent: you can't post here any more".into()
            }
            "message.reply_target_gone" => "Not sent: the quoted message was deleted".into(),
            _ => "Not sent".into(),
        },
    }
}

/// Re-read the open channel's queue and draw it below the history.
fn render_pending(chat: &Rc<Chat>) {
    let Some(channel_id) = chat.current.borrow().clone() else {
        return;
    };
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            let channel_id = channel_id.clone();
            async move { client.pending_messages(&channel_id).await }
        });
        let pending = match handle.await {
            Ok(Ok(pending)) => pending,
            _ => Vec::new(), // no local storage: nothing is ever queued
        };
        if chat.current.borrow().as_deref() != Some(channel_id.as_str()) {
            return;
        }
        for row in chat.pending_rows.borrow_mut().drain(..) {
            chat.message_list.remove(&row);
        }
        for item in pending {
            if chat.shown_client_ids.borrow().contains(&item.client_id) {
                continue; // already in the history (between the ack's two steps)
            }
            let row = pending_row(&chat, &item);
            chat.message_list.append(&row);
            chat.pending_rows.borrow_mut().push(row);
        }
    });
}

fn pending_row(chat: &Rc<Chat>, item: &PendingMessage) -> gtk::ListBoxRow {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_top(4)
        .margin_bottom(4)
        .margin_start(12)
        .margin_end(12)
        .opacity(0.6)
        .build();
    if let Some(target) = &item.reply_to_id {
        // The quoted message as shown in this channel, if it's on screen.
        let quoted = chat.message_rows.borrow().get(target).map(|w| Quoted {
            text: w.body.text().to_string(),
            deleted: w.deleted.get(),
            has_files: w.has_files,
        });
        let excerpt = reply_excerpt(quoted.as_ref());
        column.append(
            &gtk::Label::builder()
                .label(format!("\u{21b3} Replying to {excerpt}"))
                .xalign(0.0)
                .ellipsize(gtk::pango::EllipsizeMode::End)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
    }
    column.append(
        &gtk::Label::builder()
            .label(&item.body)
            .xalign(0.0)
            .wrap(true)
            .build(),
    );
    let footer = gtk::Box::builder().spacing(6).build();
    let failed = matches!(item.state, PendingState::Failed { .. });
    footer.append(
        &gtk::Label::builder()
            .label(pending_text(&item.state))
            .css_classes(if failed {
                vec!["caption", "error"]
            } else {
                vec!["caption", "dim-label"]
            })
            .build(),
    );
    if failed {
        column.set_opacity(1.0);
        // A reply whose quote is gone can't succeed as is: offer it as a plain message,
        // in place (same position in the queue), instead of a Retry that fails again.
        let quote_gone = matches!(
            &item.state,
            PendingState::Failed { code } if code == "message.reply_target_gone"
        );
        let retry = gtk::Button::builder()
            .label(if quote_gone {
                "Send without the quote"
            } else {
                "Retry"
            })
            .css_classes(["flat"])
            .build();
        let delete = gtk::Button::builder()
            .label("Delete")
            .css_classes(["flat"])
            .build();
        footer.append(&retry);
        footer.append(&delete);
        let cid = item.client_id.clone();
        retry.connect_clicked({
            let (chat, cid) = (chat.clone(), cid.clone());
            move |_| {
                let (client, cid) = (chat.client.clone(), cid.clone());
                chat.runtime.spawn(async move {
                    if quote_gone {
                        client.retry_without_reply(&cid).await
                    } else {
                        client.retry_send(&cid).await
                    }
                });
            }
        });
        delete.connect_clicked({
            let chat = chat.clone();
            move |_| {
                let (client, cid) = (chat.client.clone(), cid.clone());
                let handle = chat
                    .runtime
                    .spawn(async move { client.delete_pending(&cid).await });
                let chat = chat.clone();
                glib::spawn_future_local(async move {
                    // AlreadySent: it did go out; the history shows it.
                    if let Ok(Ok(Deleted::Removed | Deleted::AlreadySent | Deleted::NotFound)) =
                        handle.await
                    {
                        render_pending(&chat);
                    }
                });
            }
        });
    }
    column.append(&footer);
    gtk::ListBoxRow::builder()
        .activatable(false)
        .child(&column)
        .build()
}

/// A message with this `client_id` arrived: its bubble goes (the next re-read agrees).
fn drop_pending_bubble(chat: &Rc<Chat>, _client_id: &str) {
    // Bubbles don't carry their id; a re-read redraws the queue without it.
    if !chat.pending_rows.borrow().is_empty() {
        render_pending(chat);
    }
}

/// Keep queued bubbles below the history when a new message lands.
fn keep_pending_last(chat: &Rc<Chat>) {
    let rows = chat.pending_rows.borrow().clone();
    for row in &rows {
        chat.message_list.remove(row);
        chat.message_list.append(row);
    }
}

/// Change notices from the cache and outbox, on the GTK loop.
fn spawn_cache_loop(chat: &Rc<Chat>) {
    let chat = chat.clone();
    let mut events = chat.client.cache_events();
    glib::spawn_future_local(async move {
        use tokio::sync::broadcast::error::RecvError;
        loop {
            let event = events.recv().await;
            let current = chat.current.borrow().clone();
            match event {
                Ok(CacheEvent::Outbox(channel)) => {
                    if current.as_deref() == Some(channel.as_str()) {
                        render_pending(&chat);
                    }
                }
                Ok(CacheEvent::Channels(ids)) => {
                    // A catch-up (/sync) delivered rows no live event showed: fill the
                    // open channel in, and refresh unread badges from the cache.
                    if let Some(open) = current.filter(|c| ids.contains(c)) {
                        fill_from_cache(&chat, open);
                    }
                    badges_from_cache(&chat);
                }
                Ok(CacheEvent::Removed(_) | CacheEvent::Reset) | Err(RecvError::Lagged(_)) => {
                    refresh_channels(&chat, None);
                    report_outbox_lost(&chat);
                }
                Ok(CacheEvent::OutboxLost) => report_outbox_lost(&chat),
                Ok(_) => {}
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// Append cached messages of the open channel that aren't on screen yet.
fn fill_from_cache(chat: &Rc<Chat>, channel_id: String) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            let channel_id = channel_id.clone();
            async move { client.cached_messages(&channel_id, None, 50).await }
        });
        let Ok(Ok(page)) = handle.await else { return };
        if chat.current.borrow().as_deref() != Some(channel_id.as_str()) {
            return;
        }
        for message in page.messages.iter().rev() {
            let shown = chat.message_rows.borrow().get(&message.id).cloned();
            match shown {
                None => append_message(&chat, message),
                // Deleted while on screen, and no live event said so.
                Some(widgets) if message.is_deleted() && !widgets.deleted.get() => {
                    show_deleted(&widgets);
                    mark_quotes_deleted(&chat, &message.id);
                }
                Some(_) => {}
            }
        }
    });
}

/// Unread badges from the cache (after a catch-up that no live event announced).
fn badges_from_cache(chat: &Rc<Chat>) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            async move { client.cached_channels().await }
        });
        let Ok(Ok(cached)) = handle.await else { return };
        let current = chat.current.borrow().clone();
        let updates: Vec<(usize, i64)> = chat
            .channels
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let fresh = cached.iter().find(|f| f.id == c.id)?;
                // The open channel is being read: its badge stays clear.
                let unread = if current.as_deref() == Some(c.id.as_str()) {
                    0
                } else {
                    fresh.unread_count
                };
                (unread != c.unread_count).then_some((i, unread))
            })
            .collect();
        for (i, unread) in updates {
            chat.channels.borrow_mut()[i].unread_count = unread;
            update_badge(&chat, i);
        }
    });
}

/// The offline banner, from the cache's state (every few seconds), and the one-time
/// clean-up of other accounts' saved data once this user's storage is open.
fn watch_offline(chat: &Rc<Chat>) {
    // Core's state feed (#113): it follows sign-ins and switches by itself and resets
    // to the default on sign-out, so the banner never shows a previous user's state.
    let mut state = chat.client.subscribe_cache_state();
    let chat_weak = Rc::downgrade(chat);
    glib::spawn_future_local(async move {
        let mut checked_others = false;
        loop {
            let current = state.borrow_and_update().clone();
            let Some(chat) = chat_weak.upgrade() else {
                break;
            };
            if chat.offline_banner.root().is_none() {
                break; // signed out: the view is gone
            }
            chat.offline_banner.set_revealed(current.offline);
            // The first completed sync means this user's storage is open: now is the
            // time to clear another account's saved data (#46 §8).
            if current.last_synced.is_some() && !checked_others {
                checked_others = true;
                wipe_other_accounts(&chat);
            }
            drop(chat);
            if state.changed().await.is_err() {
                break;
            }
        }
    });
    report_outbox_lost(chat);
}

/// Unsent messages this device couldn't keep (their storage key was lost): say so once
/// and acknowledge exactly that loss, so a newer one is still reported.
fn report_outbox_lost(chat: &Rc<Chat>) {
    thread_local! {
        // One alert at a time: a second notice before the first is dismissed adds none.
        static SHOWING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
    if SHOWING.with(std::cell::Cell::get) {
        return;
    }
    let Some(n) = chat.client.outbox_lost() else {
        return;
    };
    SHOWING.with(|s| s.set(true));
    let alert = adw::AlertDialog::new(
        Some("Unsent Messages Lost"),
        Some("Some unsent messages on this device couldn't be recovered."),
    );
    alert.add_response("ok", "OK");
    alert.connect_response(None, {
        let chat = Rc::downgrade(chat);
        move |_, _| {
            let Some(chat) = chat.upgrade() else { return };
            chat.client.acknowledge_outbox_lost(n);
            SHOWING.with(|s| s.set(false));
            // A newer loss that came in while this alert was up is reported now, not
            // at the next event.
            report_outbox_lost(&chat);
        }
    });
    alert.present(Some(&chat.message_list));
}

/// A different account used this device before: its saved data goes (#46 §8).
fn wipe_other_accounts(chat: &Rc<Chat>) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            async move {
                let others = client.other_local_users().await?;
                if !others.is_empty() {
                    client.wipe_other_local_users().await?;
                }
                Ok::<_, brook_core::Error>(others.len())
            }
        });
        if let Ok(Ok(n)) = handle.await {
            if n > 0 {
                let alert = adw::AlertDialog::new(
                    Some("Saved Data Removed"),
                    Some("Another account's saved messages were removed from this device."),
                );
                alert.add_response("ok", "OK");
                alert.present(Some(&chat.message_list));
            }
        }
    });
}

/// Sign Out, with "Remove this device's data" (ticked by default, #46 §8) and a warning
/// when queued messages would be lost with it.
fn sign_out_dialog(chat: &Rc<Chat>) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let unsent = chat
            .runtime
            .spawn({
                let client = chat.client.clone();
                async move { client.unsent_count().await }
            })
            .await
            .unwrap_or(0);
        let remove = gtk::CheckButton::builder()
            .label("Remove this device's data")
            .active(true)
            .build();
        let body = sign_out_body(unsent, true);
        let dialog = adw::AlertDialog::new(Some("Sign Out?"), Some(&body));
        dialog.set_extra_child(Some(&remove));
        remove.connect_toggled({
            let dialog = dialog.clone();
            move |check| dialog.set_body(&sign_out_body(unsent, check.is_active()))
        });
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("sign-out", "Sign Out");
        dialog.set_response_appearance("sign-out", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.connect_response(None, {
            let chat = chat.clone();
            move |_, response| {
                if response == "sign-out" {
                    (chat.sign_out)(remove.is_active());
                }
            }
        });
        dialog.present(Some(&chat.message_list));
    });
}

/// What signing out does to this device's data, in words.
fn sign_out_body(unsent: u64, remove: bool) -> String {
    let mut text = if remove {
        String::from("Saved messages and files are removed from this device.")
    } else {
        String::from("Saved messages stay on this device for your next sign-in.")
    };
    if remove && unsent > 0 {
        let what = if unsent == 1 {
            "1 message hasn't"
        } else {
            "messages haven't"
        };
        let count = if unsent == 1 {
            String::new()
        } else {
            format!("{unsent} ")
        };
        text.push_str(&format!(" {count}{what} been sent and will be deleted."));
    }
    text
}

#[cfg(test)]
mod offline_tests {
    use super::*;

    #[test]
    fn sign_out_warns_only_when_unsent_messages_would_go() {
        assert!(sign_out_body(0, true).contains("removed from this device"));
        assert!(!sign_out_body(0, true).contains("deleted"));
        assert!(sign_out_body(1, true).contains("1 message hasn't been sent"));
        assert!(sign_out_body(3, true).contains("3 messages haven't been sent"));
        assert!(
            !sign_out_body(3, false).contains("deleted"),
            "kept messages aren't lost"
        );
    }

    #[test]
    fn pending_states_read_plainly() {
        assert_eq!(pending_text(&PendingState::Sending), "Sending…");
        assert!(pending_text(&PendingState::Failed {
            code: "not_found".into()
        })
        .contains("can't post here"));
        assert_eq!(
            pending_text(&PendingState::Failed {
                code: "http_500".into()
            }),
            "Not sent"
        );
        assert!(pending_text(&PendingState::Failed {
            code: "message.reply_target_gone".into()
        })
        .contains("quoted message was deleted"));
    }
}

/// Why a send failed, briefly (shown above the message box).
fn send_error_text(err: &brook_core::Error) -> String {
    match err {
        brook_core::Error::Http(_)
        | brook_core::Error::Timeout
        | brook_core::Error::Disconnected => {
            "Couldn't send: you're offline. Your message is back in the box.".into()
        }
        brook_core::Error::NotAuthenticated => "Couldn't send: you were signed out.".into(),
        brook_core::Error::Api { code, .. } if code == "outbox.would_overtake" => {
            "Wait for your earlier messages to send, then try again.".into()
        }
        _ => "Couldn't send. Your message is back in the box.".into(),
    }
}

/// Show a send error in the typing line for a few seconds.
fn show_send_error(chat: &Rc<Chat>, text: &str) {
    chat.typing_label.set_text(text);
    chat.typing_label.add_css_class("error");
    chat.typing_label.set_visible(true);
    let label = chat.typing_label.downgrade();
    glib::timeout_add_seconds_local_once(6, move || {
        if let Some(label) = label.upgrade() {
            label.remove_css_class("error");
            label.set_visible(false);
        }
    });
}

#[cfg(test)]
mod send_error_tests {
    use super::*;

    #[test]
    fn a_failed_send_says_the_text_is_back() {
        assert!(send_error_text(&brook_core::Error::Timeout).contains("back in the box"));
        let overtake = brook_core::Error::Api {
            code: "outbox.would_overtake".into(),
            message: String::new(),
        };
        assert!(send_error_text(&overtake).contains("earlier messages"));
    }
}

/// Where a message goes among those shown: after every older one. Message ids are
/// UUIDv7 in lowercase hex, so their string order is their time order.
fn insert_position<'a>(shown: impl Iterator<Item = &'a String>, id: &str) -> usize {
    shown.filter(|other| other.as_str() < id).count()
}

#[cfg(test)]
mod order_tests {
    use super::insert_position;

    #[test]
    fn an_older_message_goes_before_newer_ones() {
        let shown: Vec<String> = [
            "0190a000-0000-7000-8000-000000000003",
            "0190a000-0000-7000-8000-000000000005",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        // A page from load_head arriving after the newest is already on screen.
        assert_eq!(
            insert_position(shown.iter(), "0190a000-0000-7000-8000-000000000001"),
            0
        );
        assert_eq!(
            insert_position(shown.iter(), "0190a000-0000-7000-8000-000000000004"),
            1
        );
        assert_eq!(
            insert_position(shown.iter(), "0190a000-0000-7000-8000-000000000009"),
            2
        );
    }
}

/// The quoted message on a queued reply's bubble: one line, at most 80 characters.
fn reply_excerpt(quoted: Option<&Quoted>) -> String {
    let Some(quoted) = quoted else {
        return "an earlier message".into();
    };
    if quoted.deleted {
        return "a deleted message".into();
    }
    let flat = quoted.text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() && quoted.has_files {
        "a file".into() // sent without a caption
    } else {
        flat.chars().take(80).collect()
    }
}

/// A quoted message as it is shown on screen.
struct Quoted {
    text: String,
    deleted: bool,
    has_files: bool,
}

/// Show a row's text line as a tombstone.
fn show_deleted(widgets: &MessageWidgets) {
    widgets.body.set_markup("<i>Message deleted</i>");
    widgets.body.add_css_class("dim-label");
    widgets.body.set_visible(true);
    for extra in &widgets.extras {
        extra.set_visible(false);
    }
    widgets.source.replace(String::new());
    widgets.deleted.set(true);
}

/// Replies on screen that quote a message just deleted say so (only the target itself
/// gets `message.delete`; the next history or sync read carries the server's flag).
fn mark_quotes_deleted(chat: &Rc<Chat>, target: &str) {
    for widgets in chat.message_rows.borrow().values() {
        if let Some((id, who, label)) = &widgets.quote {
            if id == target {
                label.set_label(&quote_text(who, "", true, 0));
            }
        }
    }
}

/// The quote line above a reply, from the server's excerpt flags (not its text).
fn quote_text(who: &str, body: &str, deleted: bool, files: u32) -> String {
    let what = if deleted {
        "a deleted message".to_string()
    } else if !body.trim().is_empty() {
        body.to_string()
    } else {
        match files {
            0 => "an empty message".to_string(),
            1 => "a file".to_string(),
            n => format!("{n} files"),
        }
    };
    format!("\u{21b3} {who}: {what}")
}

#[cfg(test)]
mod reply_excerpt_tests {
    use super::{quote_text, reply_excerpt, Quoted};

    fn quoted(text: &str, deleted: bool, has_files: bool) -> Quoted {
        Quoted {
            text: text.into(),
            deleted,
            has_files,
        }
    }

    #[test]
    fn a_quote_is_one_line_and_a_tombstone_says_so() {
        let excerpt = |q: Quoted| reply_excerpt(Some(&q));
        assert_eq!(
            excerpt(quoted("first line\nsecond  line", false, false)),
            "first line second line"
        );
        assert_eq!(excerpt(quoted("", true, false)), "a deleted message");
        assert_eq!(excerpt(quoted("", false, true)), "a file");
        assert_eq!(excerpt(quoted("see this", false, true)), "see this");
        assert_eq!(reply_excerpt(None), "an earlier message");
        assert_eq!(
            excerpt(quoted(&"x".repeat(200), false, false))
                .chars()
                .count(),
            80
        );
    }

    #[test]
    fn a_sent_quote_follows_the_server_flags() {
        let q = |body, deleted, files| quote_text("Ana", body, deleted, files);
        assert_eq!(q("hello", false, 0), "\u{21b3} Ana: hello");
        assert_eq!(q("(deleted)", true, 0), "\u{21b3} Ana: a deleted message");
        assert_eq!(q("", false, 1), "\u{21b3} Ana: a file");
        assert_eq!(q(" ", false, 3), "\u{21b3} Ana: 3 files");
        assert_eq!(q("see this", false, 2), "\u{21b3} Ana: see this");
    }
}
