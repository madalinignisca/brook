//! The post-login chat view: a sidebar of channels/DMs, a message list, and a
//! composer — over `brook-core`. Networking runs on the Tokio runtime; results
//! are applied on the GTK main loop (await a runtime `JoinHandle` inside
//! `spawn_future_local`). Realtime `message.new` events are consumed from the
//! core's broadcast channel on the main loop and appended live.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    // With the emoji-presentation selector, as the Mac and KDE send it: the server keeps the
    // string as sent, so a bare U+2764 would be a different reaction from theirs.
    "\u{2764}\u{fe0f}",
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
    /// Per-conversation activity and open order, for the sidebar's order.
    sidebar: Rc<RefCell<crate::sidebar::SidebarState>>,
    /// This account's saved opened ranks are loaded into `sidebar` (once the user is known).
    sidebar_loaded: Rc<Cell<bool>>,
    /// The user chose to remove this device's data: a late save mustn't bring their ranks back.
    ranks_forgotten: Rc<Cell<bool>>,
    /// This session was signed out by the user (either way): nothing it started may erase.
    ended: Rc<Cell<bool>>,
    /// "Show usernames" (a per-device preference): people are named `@handle`, not by name.
    show_usernames: Rc<Cell<bool>>,
    /// Set while the list is rebuilt, so removing and re-adding rows doesn't "select" them.
    rebuilding: Rc<Cell<bool>>,
    channels: Rc<RefCell<Vec<Channel>>>,
    /// Unread badge label per sidebar row, parallel to `channels`.
    badges: Rc<RefCell<Vec<Badge>>>,
    /// message id -> its widgets, for live edit/delete of the open channel.
    message_rows: Rc<RefCell<HashMap<String, MessageWidgets>>>,
    /// The newest reaction event applied per message and emoji, for the count and for my own
    /// flag (a late, older event is dropped, or applied to my flag alone).
    reaction_order: Rc<RefCell<ReactionOrder>>,
    /// The message id currently being replied to (quote-reply), if any.
    replying_to: Rc<RefCell<Option<String>>>,
    /// The reply banner shown above the composer while replying.
    reply_bar: gtk::Revealer,
    reply_label: gtk::Label,
    /// Channel settings menu (members, leave, and for owners and admins rename/archive/delete).
    channel_settings: gtk::MenuButton,
    /// What only an owner or an admin of the open channel is offered (see `show_management`).
    manage: Rc<RefCell<ManageWidgets>>,
    /// "X is typing…" indicator above the composer, who is in it, and the timer that
    /// refreshes it when the first of them expires.
    typing_label: gtk::Label,
    typing: Rc<RefCell<TypingState>>,
    /// While set and in the future, the line shows an error (a failed send or reaction) that
    /// typing must not overwrite or hide.
    error_hold: Rc<Cell<ErrorHold>>,
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
    /// Files waiting to go with the next message, drawn as chips in `staged_box`.
    staged: Rc<RefCell<Vec<crate::outgoing::Staged>>>,
    staged_box: gtk::Box,
    attach_button: gtk::Button,
    /// A send with files is being copied into the outbox: the composer waits for it.
    preparing: Rc<Cell<bool>>,
    /// Upload progress by transfer id, for the chips and the pending bubbles.
    progress: crate::outgoing::Progress,
    /// Transfer ids the pending bubbles show (forgotten when they're redrawn).
    pending_ids: Rc<RefCell<Vec<brook_core::TransferId>>>,
    /// The staged message's outbox id, made at its first Send and kept until it's queued
    /// or dropped: a second Send of the same message after an unclear failure can't make a
    /// second message, and a changed one (text, quote or files) gets a new id.
    draft_id: Rc<RefCell<Option<(Draft, String)>>>,
    /// The chip whose button cancelled the copy: that file leaves, the others stay.
    cancelled_chip: Rc<Cell<Option<brook_core::TransferId>>>,
    /// A text send that failed: what it was (channel, text, quoted message) and its outbox
    /// id. Sending exactly that again reuses the id, so it can never become two messages.
    text_draft: Rc<RefCell<Option<(Draft, String)>>>,
    /// This user's local stores answered a cached call: `unsent_count` can be trusted (it
    /// answers 0 while they're closed).
    local_open: Rc<Cell<bool>>,
    /// A message arrived in the open conversation while the window wasn't focused: it's
    /// marked read when the window is focused again, not before.
    read_owed: Rc<Cell<bool>>,
    /// Current names of authors whose profile changed this session (from `cached_users`),
    /// used for every row drawn afterwards too: stored message rows keep the old name.
    author_names: Rc<RefCell<HashMap<String, String>>>,
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
    has_files: Rc<Cell<bool>>,
    /// The attachment rows by file id, in `files_box` (a file deleted from the message
    /// leaves on `message.update`).
    files_box: gtk::Box,
    file_rows: Rc<RefCell<Vec<(String, gtk::Widget)>>>,
    /// The message text as sent (markdown, not the rendered markup), for editing.
    source: Rc<RefCell<String>>,
    /// Who wrote it, and the label showing their name (redrawn when their profile changes).
    author_id: String,
    author: gtk::Label,
    /// The author's handle and the name the message came with, for naming them by the
    /// "Show usernames" preference (a profile change updates the name in `author_names`).
    author_handle: String,
    author_fallback: String,
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
    let attach_button = gtk::Button::builder()
        .icon_name("mail-attachment-symbolic")
        .tooltip_text("Add files")
        .sensitive(false)
        .css_classes(["flat"])
        .build();
    let staged_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(2)
        .margin_start(6)
        .margin_end(6)
        .visible(false)
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
        sidebar: Rc::default(),
        sidebar_loaded: Rc::default(),
        ranks_forgotten: Rc::default(),
        ended: Rc::default(),
        show_usernames: Rc::new(Cell::new(crate::prefs::show_usernames())),
        rebuilding: Rc::default(),
        channels: Rc::new(RefCell::new(Vec::new())),
        badges: Rc::new(RefCell::new(Vec::new())),
        message_rows: Rc::new(RefCell::new(HashMap::new())),
        reaction_order: Rc::default(),
        replying_to: Rc::new(RefCell::new(None)),
        reply_bar: reply_bar.clone(),
        reply_label: reply_label.clone(),
        channel_settings: channel_settings.clone(),
        manage: Rc::default(),
        typing_label: typing_label.clone(),
        typing: Rc::default(),
        error_hold: Rc::default(),
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
        staged: Rc::default(),
        staged_box: staged_box.clone(),
        attach_button: attach_button.clone(),
        preparing: Rc::default(),
        progress: crate::outgoing::Progress::default(),
        pending_ids: Rc::default(),
        draft_id: Rc::default(),
        cancelled_chip: Rc::default(),
        text_draft: Rc::default(),
        local_open: Rc::default(),
        author_names: Rc::default(),
        read_owed: Rc::default(),
    });
    // Focusing the window again reads what arrived in the open conversation meanwhile.
    chat.message_list.connect_realize({
        let chat = Rc::downgrade(&chat);
        move |list| {
            let Some(window) = list.root().and_downcast::<gtk::Window>() else {
                return;
            };
            let chat = chat.clone();
            window.connect_is_active_notify(move |window| {
                if let Some(chat) = chat.upgrade() {
                    if window.is_active() {
                        read_what_arrived(&chat);
                    }
                }
            });
        }
    });
    chat.progress.listen(&chat.client);

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
        .visible(false)
        .build();
    content_header.pack_end(&add_member_button);
    chat.manage
        .borrow_mut()
        .whole
        .push(add_member_button.clone().upcast());
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
    composer_row.append(&attach_button);
    composer_row.append(&composer);
    composer_row.append(&send_button);

    let content_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content_box.append(&chat.offline_banner);
    content_box.append(&message_scroll);
    content_box.append(&typing_label);
    content_box.append(&reply_bar);
    content_box.append(&staged_box);
    content_box.append(&composer_row);

    // Files dropped anywhere on the conversation join the message being written, as if
    // picked with Add files (only while it can be written in).
    let drop = gtk::DropTarget::new(
        gtk::gdk::FileList::static_type(),
        gtk::gdk::DragAction::COPY,
    );
    drop.connect_drop({
        let chat = chat.clone();
        move |_, value, _, _| {
            if !chat.attach_button.is_sensitive() {
                return false; // no conversation open, archived, or a send is being prepared
            }
            let Ok(list) = value.get::<gtk::gdk::FileList>() else {
                return false;
            };
            stage_files(&chat, list.files());
            true
        }
    });
    content_box.add_controller(drop);

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
    // Two sections: channels, then people (the order itself is `rebuild_sidebar`'s).
    channel_list.set_header_func({
        let channels = chat.channels.clone();
        move |row, before| {
            let channels = channels.borrow();
            let is_dm = |r: &gtk::ListBoxRow| channels.get(r.index() as usize).map(Channel::is_dm);
            let (this, previous) = (is_dm(row), before.and_then(is_dm));
            let heading = match (previous, this) {
                (None, Some(false)) => Some("Channels"),
                (None, Some(true)) | (Some(false), Some(true)) => Some("People"),
                _ => None,
            };
            row.set_header(
                heading
                    .map(|text| {
                        gtk::Label::builder()
                            .label(text)
                            .xalign(0.0)
                            .margin_start(12)
                            .margin_top(8)
                            .css_classes(["caption-heading", "dim-label"])
                            .build()
                            .upcast::<gtk::Widget>()
                    })
                    .as_ref(),
            );
        }
    });
    channel_list.connect_row_selected({
        let chat = chat.clone();
        move |_, row| {
            if chat.rebuilding.get() {
                return;
            }
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
    attach_button.connect_clicked({
        let chat = chat.clone();
        move |_| add_files(&chat)
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
        // Before the event loop: replacing the state later would lose what a message heard in
        // between taught it.
        load_sidebar_ranks(&chat);
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
                        // They sent it: they're done typing.
                        clear_typing_of(&chat, &message.author_id);
                    }
                    // New activity moves its conversation up (re-sorted only for this, never
                    // on a plain redraw). The unread counts below are looked up after it.
                    if chat
                        .sidebar
                        .borrow_mut()
                        .live(&message.channel_id, &message.id)
                    {
                        resort_sidebar(&chat);
                    }
                    if is_current && window_focused(&chat) {
                        append_message(&chat, &message);
                        mark_read(&chat, message.channel_id.clone(), Some(message.id.clone()));
                    } else {
                        if is_current {
                            // Shown, but not seen yet: read once the window is focused.
                            append_message(&chat, &message);
                            chat.read_owed.set(true);
                        }
                        // Bump the unread badge for the channel that received it.
                        let idx = chat
                            .channels
                            .borrow()
                            .iter()
                            .position(|c| c.id == message.channel_id);
                        if let Some(idx) = idx {
                            let mentioned =
                                mentions_me(&message, chat.me.borrow().as_deref().unwrap_or(""));
                            let mut channels = chat.channels.borrow_mut();
                            channels[idx].unread_count += 1;
                            channels[idx].unread_mentions += i64::from(mentioned);
                            drop(channels);
                            update_badge(&chat, idx);
                        }
                        // Desktop notification — only when we know who we are and
                        // it's someone else (don't notify our own messages, and
                        // don't guess if our identity isn't resolved yet).
                        let me = chat.me.borrow().clone().unwrap_or_default();
                        if !me.is_empty() && message.author_id != me && !message.is_deleted() {
                            let author = if message.author_display_name.is_none()
                                && message.author_handle.is_none()
                            {
                                "Someone".to_string()
                            } else {
                                author_text(
                                    message.author_display_name.as_deref().unwrap_or_default(),
                                    message.author_handle.as_deref().unwrap_or_default(),
                                    chat.show_usernames.get(),
                                )
                            };
                            let title = chat
                                .channels
                                .borrow()
                                .iter()
                                .find(|c| c.id == message.channel_id)
                                .map(|c| crate::sidebar::label(c, &me, chat.show_usernames.get()))
                                .unwrap_or_else(|| "Brook".to_string());
                            let body = notification_body(&message, &me, &author);
                            notify(&message.channel_id, &title, &body);
                        }
                    }
                }
                Ok(ServerEvent::MessageUpdate(message)) => {
                    // Update the row in place if the edited message is on screen.
                    let widgets = chat.message_rows.borrow().get(&message.id).cloned();
                    if let Some(widgets) = widgets {
                        update_message(&chat, &widgets, &message);
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
                    seq,
                    ..
                }) => {
                    apply_reaction(&chat, &message_id, &emoji, &user_id, added, count, seq);
                }
                Ok(ServerEvent::ChannelDelete { channel_id }) => {
                    // If the open channel was deleted, clear the conversation view.
                    if chat.current.borrow().as_deref() == Some(channel_id.as_str()) {
                        *chat.current.borrow_mut() = None;
                        chat.message_rows.borrow_mut().clear();
                        chat.reaction_order.borrow_mut().clear();
                        while let Some(row) = chat.message_list.row_at_index(0) {
                            chat.message_list.remove(&row);
                        }
                        chat.title.set_title("Brook");
                        chat.title.set_subtitle("Pick a conversation");
                        chat.composer.set_sensitive(false);
                        chat.send_button.set_sensitive(false);
                        chat.attach_button.set_sensitive(false);
                        chat.channel_settings.set_visible(false);
                        show_management(&chat, ManageShown::default());
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
                        show_typing(&chat, &user_id, &display_name);
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
                Ok(ServerEvent::Ready) => {
                    // A (re)connect: the server's numbering may have restarted (a restore from
                    // a backup), so what was applied before says nothing about what follows.
                    chat.reaction_order.borrow_mut().clear();
                }
                Ok(_) => {} // future event kinds — ignored
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    // Events were missed: the order seen so far can't vouch for what follows.
                    chat.reaction_order.borrow_mut().clear();
                    continue;
                }
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
                    Ok(mut channels) => {
                        // The server doesn't say when a conversation was last used; the cache
                        // (when local data is on) does, for the sidebar's order.
                        if let Ok(cached) = client.cached_channels().await {
                            for channel in &mut channels {
                                channel.last_message_id = cached
                                    .iter()
                                    .find(|c| c.id == channel.id)
                                    .and_then(|c| c.last_message_id.clone());
                            }
                        }
                        Ok((channels, true))
                    }
                    Err(err) => client
                        .cached_channels()
                        .await
                        .map(|channels| (channels, false))
                        .map_err(|_| err),
                }
            }
        });
        let Ok(Ok((channels, from_network))) = handle.await else {
            return;
        };
        load_sidebar_ranks(&chat);
        {
            let mut sidebar = chat.sidebar.borrow_mut();
            for channel in &channels {
                sidebar.learn(&channel.id, channel.last_message_id.as_deref());
            }
            // Ranks of conversations that are gone go, but only on the network's list and only
            // when it isn't empty (an empty offline list must not erase them).
            if from_network {
                let listed: Vec<&str> = channels.iter().map(|c| c.id.as_str()).collect();
                if sidebar.prune(&listed) {
                    save_sidebar_ranks(&chat, &sidebar);
                }
            }
        }
        *chat.channels.borrow_mut() = channels;
        rebuild_sidebar(&chat);

        // Re-apply chrome for the open channel so a live rename/archive shows now.
        let current = chat.current.borrow().clone();
        if let Some(current) = current {
            apply_channel_chrome(&chat, &current);
        }
        // An offer made while it's open asks now; a withdrawn one stops asking.
        ask_about_ownership(&chat);

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

/// Order `chat.channels` as the sidebar's rules say (channels, then people; each by last use)
/// and redraw the list. The open conversation stays selected without being "opened" again.
fn rebuild_sidebar(chat: &Rc<Chat>) {
    sort_sidebar(chat);
    redraw_sidebar(chat);
}

/// Sort for new activity: redraw only if a row actually moves, so a message in the top
/// conversation doesn't destroy and rebuild every row (and with them focus, a press in
/// progress and the selection).
fn resort_sidebar(chat: &Rc<Chat>) {
    if sort_sidebar(chat) {
        redraw_sidebar(chat);
    }
}

/// Order `chat.channels` by the sidebar's rules. Whether the order changed.
fn sort_sidebar(chat: &Rc<Chat>) -> bool {
    let me = chat.me.borrow().clone().unwrap_or_default();
    let channels = std::mem::take(&mut *chat.channels.borrow_mut());
    let before: Vec<String> = channels.iter().map(|c| c.id.clone()).collect();
    let order = chat.sidebar.borrow_mut().order(&channels, &me);
    *chat.channels.borrow_mut() = crate::sidebar::arranged(channels, &order);
    crate::sidebar::order_changed(&before, &order)
}

/// Draw the list as `chat.channels` stands, in that order: no sorting. What a change of labels
/// ("Show usernames") needs, since a click or two since the last sort must not take effect then.
fn redraw_sidebar(chat: &Rc<Chat>) {
    let me = chat.me.borrow().clone().unwrap_or_default();
    let channels = chat.channels.borrow().clone();
    let labels = crate::sidebar::labels_in_order(&channels, &me, chat.show_usernames.get());
    // The conversation whose row has keyboard focus (removing the rows would drop it). Rows
    // carry their conversation's id as their widget name: `chat.channels` is already in the
    // new order here while the rows are still in the old one, so a position can't say.
    let focused_id = chat
        .channel_list
        .focus_child()
        .and_downcast::<gtk::ListBoxRow>()
        .map(|row| row.widget_name().to_string());

    chat.rebuilding.set(true);
    while let Some(row) = chat.channel_list.row_at_index(0) {
        chat.channel_list.remove(&row);
    }
    chat.badges.borrow_mut().clear();
    for (channel, label) in channels.iter().zip(&labels) {
        let (row, badge) = channel_row(
            label,
            channel.is_dm(),
            (channel.unread_count, channel.unread_mentions),
            channel.owner_offer_for(&me).is_some(),
        );
        row.set_widget_name(&channel.id);
        chat.channel_list.append(&row);
        chat.badges.borrow_mut().push(badge);
    }
    // Keep the open conversation selected (it keeps its place until activity moves it).
    let current = chat.current.borrow().clone();
    if let Some(idx) = current.and_then(|id| channels.iter().position(|c| c.id == id)) {
        if let Some(row) = chat.channel_list.row_at_index(idx as i32) {
            chat.channel_list.select_row(Some(&row));
        }
    }
    if let Some(idx) = crate::sidebar::focus_target(focused_id.as_deref(), &channels) {
        if let Some(row) = chat.channel_list.row_at_index(idx as i32) {
            row.grab_focus();
        }
    }
    chat.rebuilding.set(false);
    chat.channel_list.invalidate_headers();
}

/// Load this account's saved opened ranks into the sidebar state, once the user is known (and
/// before anything is learned, so the counter continues above them).
fn load_sidebar_ranks(chat: &Rc<Chat>) {
    let me = chat.me.borrow().clone().unwrap_or_default();
    if chat.sidebar_loaded.get() || me.is_empty() {
        return;
    }
    chat.sidebar_loaded.set(true);
    *chat.sidebar.borrow_mut() = crate::sidebar::SidebarState::new(crate::prefs::load_opened(&me));
}

/// Save the opened ranks for this account (synchronously, so there is no ordering race).
fn save_sidebar_ranks(chat: &Rc<Chat>, sidebar: &crate::sidebar::SidebarState) {
    let me = chat.me.borrow().clone().unwrap_or_default();
    if !crate::prefs::may_save_opened(chat.ranks_forgotten.get(), &me) {
        return;
    }
    crate::prefs::save_opened(&me, sidebar.opened_ranks());
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
        .map(|c| {
            let my_role = c
                .members
                .iter()
                .find(|m| m.id == me)
                .and_then(|m| m.role.clone());
            (
                c.is_dm(),
                c.archived,
                crate::sidebar::label(c, &me, chat.show_usernames.get()),
                my_role,
            )
        });
    let Some((is_dm, archived, title, my_role)) = meta else {
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
    // Every member of a channel may leave it; DMs can't be left.
    chat.channel_settings.set_visible(!is_dm);
    // Add member, rename, archive and delete: the owner or a global admin (as the server
    // allows it), never in a DM.
    show_management(
        chat,
        management_shown(is_dm, *chat.is_admin.borrow(), my_role.as_deref(), archived),
    );
    // While files are being copied the composer waits (a reload mustn't unlock it).
    let open = !archived && !chat.preparing.get();
    chat.composer.set_sensitive(open);
    chat.send_button.set_sensitive(open);
    chat.attach_button.set_sensitive(open);
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
    // Files picked for one conversation never go to another.
    if !chat.preparing.get() {
        clear_staged(chat);
    }
    let previous = chat.current.replace(Some(channel_id.to_string()));
    if previous.as_deref() != Some(channel_id) {
        forget_deferred_question();
    }
    load_sidebar_ranks(chat);
    {
        let mut sidebar = chat.sidebar.borrow_mut();
        sidebar.opened_now(channel_id);
        save_sidebar_ranks(chat, &sidebar);
    }
    apply_channel_chrome(chat, channel_id);
    ask_about_ownership(chat);

    // Opening a channel reads it: clear its unread badge locally and tell the
    // server. Compute idx in its own statement so the immutable borrow is dropped
    // before borrow_mut (an inline `if let` scrutinee would hold it and panic).
    let idx = chat
        .channels
        .borrow()
        .iter()
        .position(|c| c.id == channel_id);
    if let Some(idx) = idx {
        let mut channels = chat.channels.borrow_mut();
        (channels[idx].unread_count, channels[idx].unread_mentions) = (0, 0);
        drop(channels);
        update_badge(chat, idx);
    }
    mark_read(chat, channel_id.to_string(), None);

    // A pending reply targets a message in the channel we're leaving — drop it.
    set_reply(chat, None);
    clear_typing(chat);
    // Clear now, before the await, so live messages that arrive while history is
    // loading are appended to a fresh list rather than wiped by a late clear.
    chat.message_rows.borrow_mut().clear();
    chat.reaction_order.borrow_mut().clear();
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
    if chat.preparing.get() {
        return;
    }
    if !chat.staged.borrow().is_empty() {
        send_with_files(chat);
        return;
    }
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
    let client_id = draft_id(
        &mut chat.text_draft.borrow_mut(),
        Draft {
            channel: channel_id.clone(),
            body: body.clone(),
            reply_to: reply_to.clone(),
            files: Vec::new(),
        },
    );

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
                    .send_queued(&channel_id, &body, reply_to.clone(), Some(client_id))
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
            Ok(true) => {
                chat.text_draft.replace(None);
                render_pending(&chat);
            }
            Ok(false) => {
                chat.text_draft.replace(None);
            }
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

/// Pick files and put them under the message box (limits checked as they're added).
fn add_files(chat: &Rc<Chat>) {
    let window = chat.composer.root().and_downcast::<gtk::Window>();
    let chat = chat.clone();
    crate::outgoing::pick(window.as_ref(), move |files| stage_files(&chat, files));
}

/// Put files (picked, or dropped on the conversation) under the message box, checking the
/// limits as they're added.
fn stage_files(chat: &Rc<Chat>, files: Vec<gtk::gio::File>) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        for file in files {
            let staged = match crate::outgoing::describe(&file).await {
                Ok(staged) => staged,
                Err(crate::outgoing::NotStaged::NotAFile) => {
                    show_send_error(&chat, "Only files can be sent (not folders or devices).");
                    continue;
                }
                Err(crate::outgoing::NotStaged::Unreadable) => {
                    show_send_error(&chat, "That file couldn't be read.");
                    continue;
                }
            };
            let count = chat.staged.borrow().len();
            if let Some(why) = crate::outgoing::refusal(count, &staged.name, staged.size) {
                show_send_error(&chat, &why);
                continue;
            }
            chat.staged.borrow_mut().push(staged);
        }
        redraw_staged(&chat);
        chat.composer.grab_focus();
    });
}

/// Draw the staged files as chips above the message box.
fn redraw_staged(chat: &Rc<Chat>) {
    while let Some(child) = chat.staged_box.first_child() {
        chat.staged_box.remove(&child);
    }
    let staged = chat.staged.borrow().clone();
    for file in &staged {
        let tid = file.transfer_id;
        let chip = crate::outgoing::staged_chip(file, &chat.progress, {
            let chat = Rc::downgrade(chat);
            move || {
                let Some(chat) = chat.upgrade() else { return };
                if chat.preparing.get() {
                    // Mid-copy: stop the whole send (core's flags are per message, so
                    // nothing gets queued); this file then leaves, the others stay.
                    chat.cancelled_chip.set(Some(tid));
                    chat.client.cancel_transfer(tid);
                } else {
                    chat.staged.borrow_mut().retain(|f| f.transfer_id != tid);
                    chat.progress.forget([tid]);
                    redraw_staged(&chat);
                }
            }
        });
        chat.staged_box.append(&chip);
    }
    chat.staged_box.set_visible(!staged.is_empty());
}

fn clear_staged(chat: &Rc<Chat>) {
    chat.draft_id.replace(None);
    let ids: Vec<_> = chat
        .staged
        .borrow_mut()
        .drain(..)
        .map(|f| f.transfer_id)
        .collect();
    chat.progress.forget(ids);
    redraw_staged(chat);
}

/// Send the staged files with the composer's text (which may be empty). Core copies each
/// file before the call returns; meanwhile the chips show the copy and can cancel it.
fn send_with_files(chat: &Rc<Chat>) {
    let Some(channel_id) = chat.current.borrow().clone() else {
        return;
    };
    let body = chat.composer.text().to_string();
    let reply_to = chat.replying_to.borrow().clone();
    let files: Vec<_> = chat.staged.borrow().iter().map(|f| f.outgoing()).collect();
    let client_id = draft_id(
        &mut chat.draft_id.borrow_mut(),
        Draft {
            channel: channel_id.clone(),
            body: body.clone(),
            reply_to: reply_to.clone(),
            files: chat.staged.borrow().iter().map(|f| f.transfer_id).collect(),
        },
    );
    chat.cancelled_chip.set(None);
    set_preparing(chat, true);

    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            let body = body.clone();
            async move {
                client
                    .send_queued_with_files(&channel_id, &body, reply_to, Some(client_id), files)
                    .await
            }
        });
        let result = handle
            .await
            .unwrap_or(Err(brook_core::Error::UnexpectedResponse));
        set_preparing(&chat, false);
        match result {
            Ok(_) => {
                // Queued: the bubble takes over. Keep anything typed since.
                if chat.composer.text() == body {
                    chat.composer.set_text("");
                }
                set_reply(&chat, None);
                clear_staged(&chat);
                render_pending(&chat);
            }
            Err(err) => {
                // Nothing was queued: the files and text stay for another try, except a
                // file whose own button cancelled the copy.
                if let Some(tid) = chat.cancelled_chip.take() {
                    chat.staged.borrow_mut().retain(|f| f.transfer_id != tid);
                    chat.progress.forget([tid]);
                    if chat.staged.borrow().is_empty() {
                        chat.draft_id.replace(None);
                    }
                }
                redraw_staged(&chat);
                let code = match &err {
                    brook_core::Error::Api { code, .. } => code.as_str(),
                    _ => "",
                };
                if code != "transfer.cancelled" {
                    tracing::warn!(%err, "failed to queue files");
                    let text = crate::outgoing::send_error_text(code)
                        .map(str::to_string)
                        .unwrap_or_else(|| send_error_text(&err));
                    show_send_error(&chat, &text);
                }
            }
        }
    });
}

/// Lock the composer while files are copied (the chips' buttons then cancel).
fn set_preparing(chat: &Rc<Chat>, on: bool) {
    chat.preparing.set(on);
    if on {
        chat.composer.set_sensitive(false);
        chat.send_button.set_sensitive(false);
        chat.attach_button.set_sensitive(false);
    } else if let Some(id) = chat.current.borrow().clone() {
        apply_channel_chrome(chat, &id);
    }
}

/// What a text send is: an id is reused only for exactly the same one. Core answers a known
/// id with the stored message, so a retry that changed any of these (the quote included)
/// must be a new message, never the old one sent again.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Draft {
    channel: String,
    body: String,
    reply_to: Option<String>,
    /// The staged files, by their (stable, per staging) transfer ids; none for text.
    files: Vec<brook_core::TransferId>,
}

/// The outbox id for `this` send: the failed attempt's again when it's the same send, else a
/// new one (remembered until the send succeeds).
fn draft_id(draft: &mut Option<(Draft, String)>, this: Draft) -> String {
    if let Some((was, id)) = draft.as_ref() {
        if *was == this {
            return id.clone();
        }
    }
    let id = glib::uuid_string_random().to_string();
    *draft = Some((this, id.clone()));
    id
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
    // A name that changed since the message was stored wins.
    let current = chat.author_names.borrow().get(&message.author_id).cloned();
    let author = author_text(
        current
            .as_deref()
            .or(message.author_display_name.as_deref())
            .unwrap_or_default(),
        message.author_handle.as_deref().unwrap_or_default(),
        chat.show_usernames.get(),
    );

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
    if message.body.trim().is_empty() && message.attachments.is_empty() && !message.is_deleted() {
        // Every file was deleted from a message that had no text.
        body_label.set_markup("<i>Files removed</i>");
        body_label.add_css_class("dim-label");
    }
    row.append(&body_label);
    // Attached files (a tombstone has none): shown, and saved only on request.
    let files_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let file_rows = Rc::new(RefCell::new(Vec::new()));
    for file in &message.attachments {
        let file_row =
            crate::attachments::attachment_row(file, chat.client.clone(), chat.runtime.clone());
        files_box.append(&file_row);
        file_rows.borrow_mut().push((file.id.clone(), file_row));
    }
    row.append(&files_box);
    extras.push(files_box.clone().upcast());

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
        author_id: message.author_id.clone(),
        author: author_label.clone(),
        author_handle: message.author_handle.clone().unwrap_or_default(),
        author_fallback: message.author_display_name.clone().unwrap_or_default(),
        deleted: Rc::new(Cell::new(false)),
        has_files: Rc::new(Cell::new(!message.attachments.is_empty())),
        files_box,
        file_rows,
        source: Rc::new(RefCell::new(message.body.clone())),
        extras,
        quote: quote_widgets,
    };
    let me = chat.me.borrow().clone().unwrap_or_default();
    if mentions_me(message, &me) {
        widgets.row.add_css_class("mentions-me");
    }
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
            show_send_error(&chat, "Couldn't react. Try again.");
        }
    });
}

/// The newest `reaction.update` seq applied per message and emoji, kept apart for the count and
/// for my own flag. The server numbers changes in commit order, but events can arrive out of
/// order: a count is taken from an event only if it is the newest for its emoji, while my flag
/// is taken from my own events only if they are the newest of mine. So an older event of mine
/// still sets my chip when someone else's newer event carried the count (otherwise the chip
/// would show unselected and a click would toggle my reaction off).
#[derive(Default)]
struct ReactionOrder {
    counts: HashMap<(String, String), i64>,
    own: HashMap<(String, String), i64>,
}

/// What an event may change on a message's chips.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Fresh {
    /// The count is the newest seen.
    count: bool,
    /// My flag is the newest of my own events (false for other users' events).
    flag: bool,
}

impl ReactionOrder {
    /// Judge an event and remember it.
    fn judge(&mut self, message_id: &str, emoji: &str, seq: i64, mine: bool) -> Fresh {
        let key = (message_id.to_string(), emoji.to_string());
        let newest = |map: &mut HashMap<(String, String), i64>| {
            let last = map.entry(key.clone()).or_insert(i64::MIN);
            let fresh = seq > *last;
            if fresh {
                *last = seq;
            }
            fresh
        };
        Fresh {
            count: newest(&mut self.counts),
            flag: mine && newest(&mut self.own),
        }
    }

    fn clear(&mut self) {
        self.counts.clear();
        self.own.clear();
    }
}

/// A message's chips after an event. A fresh count replaces the emoji's total (a chip at 0 goes);
/// my flag follows my own fresh events. A stale count with a fresh flag changes only my flag, on
/// the chip that exists.
fn reactions_after(
    list: &mut Vec<ReactionSummary>,
    emoji: &str,
    count: i64,
    added: bool,
    mine: bool,
    fresh: Fresh,
) {
    let existing = list.iter().position(|r| r.emoji == emoji);
    if fresh.count {
        match existing {
            Some(i) => {
                list[i].count = count;
                if mine && fresh.flag {
                    list[i].me = added;
                }
            }
            None if count > 0 => list.push(ReactionSummary {
                emoji: emoji.to_string(),
                count,
                me: mine && fresh.flag && added,
            }),
            None => {}
        }
    } else if fresh.flag {
        if let Some(i) = existing {
            list[i].me = added;
        }
    }
    list.retain(|r| r.count > 0);
}

/// Apply an incremental `reaction.update` to a message's tallies, then re-render.
fn apply_reaction(
    chat: &Rc<Chat>,
    message_id: &str,
    emoji: &str,
    user_id: &str,
    added: bool,
    count: i64,
    seq: i64,
) {
    let me = chat.me.borrow().clone().unwrap_or_default();
    let mine = !me.is_empty() && user_id == me;
    // A message that isn't on screen: nothing to change, and nothing worth remembering.
    let Some(mw) = chat.message_rows.borrow().get(message_id).cloned() else {
        return;
    };
    let fresh = chat
        .reaction_order
        .borrow_mut()
        .judge(message_id, emoji, seq, mine);
    reactions_after(
        &mut mw.reactions.borrow_mut(),
        emoji,
        count,
        added,
        mine,
        fresh,
    );
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
                .map(|w| (w.source.borrow().clone(), w.has_files.get()))
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
    let edit_profile = gtk::Button::builder()
        .label("Edit Profile…")
        .has_frame(false)
        .build();
    edit_profile.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            edit_profile_dialog(&chat);
        }
    });
    menu.append(&edit_profile);
    // Per device: people are named `@handle` instead of by display name, at once.
    let show_usernames = gtk::CheckButton::builder()
        .label("Show usernames")
        .active(chat.show_usernames.get())
        .margin_start(8)
        .margin_end(8)
        .margin_top(4)
        .margin_bottom(4)
        .build();
    show_usernames.connect_toggled({
        let chat = chat.clone();
        move |check| {
            chat.show_usernames.set(check.is_active());
            crate::prefs::save_show_usernames(check.is_active());
            // Relabel the rows where they are: this never sorts.
            redraw_sidebar(&chat);
            relabel_authors(&chat);
            let current = chat.current.borrow().clone();
            if let Some(current) = current {
                apply_channel_chrome(&chat, &current);
            }
        }
    });
    menu.append(&show_usernames);
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
    let members = gtk::Button::builder()
        .label("Members…")
        .has_frame(false)
        .build();
    let leave = gtk::Button::builder()
        .label("Leave channel")
        .has_frame(false)
        .css_classes(["error"])
        .build();
    menu.append(&members);
    // Renaming, archiving and deleting are for owners and admins (shown per channel by
    // `show_management`); everyone can see who's in and leave.
    menu.append(&rename);
    menu.append(&archive);
    menu.append(&unarchive);
    menu.append(&delete);
    {
        let mut manage = chat.manage.borrow_mut();
        manage.whole.push(rename.clone().upcast());
        manage.whole.push(delete.clone().upcast());
        manage.archive = Some(archive.clone().upcast());
        manage.unarchive = Some(unarchive.clone().upcast());
    }
    menu.append(&leave);
    popover.set_child(Some(&menu));
    members.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            members_dialog(&chat);
        }
    });
    leave.connect_clicked({
        let chat = chat.clone();
        let popover = popover.clone();
        move |_| {
            popover.popdown();
            leave_channel_confirm(&chat);
        }
    });

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

/// Why leaving, removing or a profile change was refused, briefly.
fn membership_error_text(err: &brook_core::Error) -> String {
    match err {
        brook_core::Error::Api { code, .. } => match code.as_str() {
            "channel.last_owner" => {
                "The last owner can't leave. Delete the channel instead.".into()
            }
            "channel.dm" => "A direct message can't be left.".into(),
            "authz.forbidden" => "Only an admin or the channel's owner can do that.".into(),
            "not_found" => "That member isn't in this channel any more.".into(),
            "offer.not_found" => "That offer was already answered or withdrawn.".into(),
            "channel.already_owner" => "They're already an owner.".into(),
            "channel.not_member" => "They aren't a member of this channel.".into(),
            "profile.invalid" => {
                "That name or status can't be used (it's empty, too long, or has invisible characters)."
                    .into()
            }
            _ => "That didn't work. Try again.".into(),
        },
        brook_core::Error::NotAuthenticated => "You were signed out.".into(),
        _ => "Couldn't reach the server.".into(),
    }
}

/// A handle as typed: trimmed, without a leading `@`.
fn clean_handle(typed: &str) -> String {
    typed.trim().trim_start_matches('@').trim().to_string()
}

/// The thing the action was about is already gone (`not_found`): what it wanted, so nothing
/// to report. The `channel.update` or `channel.delete` on its way redraws.
fn already_gone(err: &brook_core::Error) -> bool {
    matches!(err, brook_core::Error::Api { code, .. } if code == "not_found")
}

/// The conversation actions that can fail with a message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConvAction {
    StartDm,
    CreateChannel,
    Join,
    Browse,
    AddMember,
    Update,
    Delete,
}

const TRY_AGAIN: &str = "That didn't work. Try again.";
const NO_ONE: &str = "No one has that handle.";
const BAD_NAME: &str = "That name or topic can't be used.";

impl ConvAction {
    /// The alert's heading, then what a refusal by the server's rules says (`forbidden`), what a
    /// rejected input says (`invalid`: an unknown handle, a name that doesn't fit) and what
    /// anything else that was answered says.
    fn texts(self) -> (&'static str, &'static str, &'static str, &'static str) {
        match self {
            ConvAction::StartDm => (
                "Couldn't Start the Conversation",
                "You can't message them.",
                NO_ONE,
                TRY_AGAIN,
            ),
            ConvAction::CreateChannel => (
                "Couldn't Create the Channel",
                "Only admins can create channels.",
                BAD_NAME,
                TRY_AGAIN,
            ),
            ConvAction::Join => (
                "Couldn't Join",
                "You can't join that channel.",
                TRY_AGAIN,
                TRY_AGAIN,
            ),
            ConvAction::Browse => (
                "Couldn't Load the Channels",
                "You can't browse channels.",
                TRY_AGAIN,
                TRY_AGAIN,
            ),
            ConvAction::AddMember => (
                "Couldn't Add Them",
                "Only an owner or admin can add members.",
                NO_ONE,
                TRY_AGAIN,
            ),
            ConvAction::Update => (
                "Couldn't Do That",
                "Only an owner or admin can do that.",
                BAD_NAME,
                TRY_AGAIN,
            ),
            ConvAction::Delete => (
                "Couldn't Delete",
                "Only an owner or admin can delete it.",
                TRY_AGAIN,
                TRY_AGAIN,
            ),
        }
    }

    /// The alert's heading and text for `err`. A call that wasn't answered says so; one answered
    /// in a shape the client didn't expect gets the generic text, since retrying may help.
    fn failure(self, err: &brook_core::Error) -> (&'static str, String) {
        let (heading, forbidden, invalid, fallback) = self.texts();
        let body = match err {
            brook_core::Error::Api { code, .. } => match code.as_str() {
                "authz.forbidden" => forbidden,
                "validation.error" => invalid,
                _ => fallback,
            },
            brook_core::Error::NotAuthenticated => "You were signed out.",
            brook_core::Error::UnexpectedResponse => fallback,
            _ => "Couldn't reach the server.",
        };
        (heading, body.to_string())
    }
}

fn show_conversation_failure(chat: &Rc<Chat>, action: ConvAction, err: &brook_core::Error) {
    let (heading, body) = action.failure(err);
    show_alert(chat, heading, &body);
}

fn show_alert(chat: &Rc<Chat>, heading: &str, body: &str) {
    let alert = adw::AlertDialog::new(Some(heading), Some(body));
    alert.add_response("ok", "OK");
    alert.present(Some(&chat.message_list));
}

/// Whether a profile edit is within the server's lengths (#183): a display name of 1 to 64
/// characters and a status line of at most 100, both trimmed. (Which characters are allowed
/// is the server's to judge; it says so if one isn't.)
fn profile_fits(name: &str, status: &str) -> bool {
    let n = name.trim().chars().count();
    (1..=64).contains(&n) && status.trim().chars().count() <= 100
}

/// The widgets only an owner or an admin of the open channel is offered.
#[derive(Default)]
struct ManageWidgets {
    /// Add member, Rename and Delete.
    whole: Vec<gtk::Widget>,
    /// Archive (an open channel) and Unarchive (an archived one).
    archive: Option<gtk::Widget>,
    unarchive: Option<gtk::Widget>,
}

/// Whether a viewer manages a channel (add members, rename, archive, delete): a global admin
/// or its owner, as the server allows it (`_require_channel_admin`).
fn may_manage(admin: bool, my_role: Option<&str>) -> bool {
    admin || my_role == Some("owner")
}

/// What of the manager-only widgets to show.
#[derive(Debug, Default, PartialEq, Eq)]
struct ManageShown {
    /// Add member, Rename and Delete.
    whole: bool,
    archive: bool,
    unarchive: bool,
}

/// What a viewer is offered in the open channel: nothing in a DM (the server refuses it, 422)
/// or without management rights; Archive for an open channel and Unarchive for an archived one.
fn management_shown(
    is_dm: bool,
    admin: bool,
    my_role: Option<&str>,
    archived: bool,
) -> ManageShown {
    let manage = !is_dm && may_manage(admin, my_role);
    ManageShown {
        whole: manage,
        archive: manage && !archived,
        unarchive: manage && archived,
    }
}

/// Show or hide what only managers are offered.
fn show_management(chat: &Rc<Chat>, shown: ManageShown) {
    let widgets = chat.manage.borrow();
    for widget in &widgets.whole {
        widget.set_visible(shown.whole);
    }
    if let Some(archive) = &widgets.archive {
        archive.set_visible(shown.archive);
    }
    if let Some(unarchive) = &widgets.unarchive {
        unarchive.set_visible(shown.unarchive);
    }
}

/// Whether a viewer is offered Remove on a member, as the server allows it (#183): never
/// themselves (that's Leave); a global admin removes anyone; a channel owner removes members
/// but not other owners.
fn may_remove(admin: bool, my_role: Option<&str>, their_role: Option<&str>, is_me: bool) -> bool {
    if is_me {
        return false;
    }
    admin || (my_role == Some("owner") && their_role != Some("owner"))
}

/// "Leave channel": confirm, then leave. The server's `channel.delete` to us closes it.
fn leave_channel_confirm(chat: &Rc<Chat>) {
    let Some(channel_id) = chat.current.borrow().clone() else {
        return;
    };
    let me = chat.me.borrow().clone().unwrap_or_default();
    // The last owner can't leave (the server refuses): say so before asking.
    let owners: Vec<String> = chat
        .channels
        .borrow()
        .iter()
        .find(|c| c.id == channel_id)
        .map(|c| {
            c.members
                .iter()
                .filter(|m| m.role.as_deref() == Some("owner"))
                .map(|m| m.id.clone())
                .collect()
        })
        .unwrap_or_default();
    if owners.len() == 1 && owners[0] == me {
        show_alert(
            chat,
            "You're the Last Owner",
            "The last owner can't leave. Delete the channel instead.",
        );
        return;
    }
    let title = chat
        .channels
        .borrow()
        .iter()
        .find(|c| c.id == channel_id)
        .map(|c| c.title(&me))
        .unwrap_or_default();
    let dialog = adw::AlertDialog::new(
        Some(&format!("Leave {title}?")),
        Some("You'll stop getting its messages. An owner or an admin can add you back."),
    );
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("leave", "Leave");
    dialog.set_response_appearance("leave", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.connect_response(None, {
        let chat = chat.clone();
        move |_, response| {
            if response != "leave" {
                return;
            }
            let chat = chat.clone();
            let channel_id = channel_id.clone();
            glib::spawn_future_local(async move {
                let result = chat
                    .runtime
                    .spawn({
                        let client = chat.client.clone();
                        async move { client.leave_channel(&channel_id).await }
                    })
                    .await
                    .unwrap_or(Err(brook_core::Error::UnexpectedResponse));
                if let Err(err) = result {
                    // Not a member any more: already out, which is what leaving wanted.
                    let already_out =
                        matches!(&err, brook_core::Error::Api { code, .. } if code == "not_found");
                    if !already_out {
                        show_alert(&chat, "Couldn't Leave", &membership_error_text(&err));
                    }
                }
            });
        }
    });
    dialog.present(Some(&chat.message_list));
}

/// Run a member action (offer, withdraw) from its button: insensitive while it runs, `done`
/// on success (the `channel.update` echo refreshes the lists), the reason on failure.
fn member_action<F, Fut>(
    chat: &Rc<Chat>,
    button: &gtk::Button,
    done: &'static str,
    heading: &'static str,
    action: F,
) where
    F: FnOnce(Arc<BrookClient>) -> Fut + Send + 'static,
    Fut: std::future::Future<Output = brook_core::Result<()>> + Send + 'static,
{
    button.set_sensitive(false);
    let (chat, button) = (chat.clone(), button.clone());
    glib::spawn_future_local(async move {
        let client = chat.client.clone();
        let result = chat
            .runtime
            .spawn(async move { action(client).await })
            .await
            .unwrap_or(Err(brook_core::Error::UnexpectedResponse));
        match result {
            Ok(()) => button.set_label(done),
            Err(err) => {
                button.set_sensitive(true);
                show_alert(&chat, heading, &membership_error_text(&err));
            }
        }
    });
}

/// Which offer a question is about: the channel, who offered and when. A new offer on the
/// same channel is a new question.
fn offer_key(channel_id: &str, offer: &brook_core::OwnerOffer) -> String {
    format!("{channel_id}|{}|{}", offer.offered_by, offer.created_at)
}

/// What the ownership question does now, given the one on screen (`open`), the one put off
/// until the channel is next opened (`deferred`) and the open channel's offer to this user
/// (`wanted`). Answers (close the one on screen, ask now). A withdrawn or answered offer
/// closes its question; a deferred one isn't asked again this opening.
fn ownership_question(
    open: Option<&str>,
    deferred: Option<&str>,
    wanted: Option<&str>,
) -> (bool, bool) {
    let close = open.is_some() && open != wanted;
    let asking = open.is_some() && !close;
    let ask = wanted.is_some() && !asking && deferred != wanted;
    (close, ask)
}

thread_local! {
    /// The ownership question on screen, by `offer_key`.
    static ASKING: RefCell<Option<(String, adw::AlertDialog)>> = const { RefCell::new(None) };
    /// The offer whose answer failed and was put off, until its channel is next opened.
    static DEFERRED: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Opening another channel ends any "Ask Me Later".
fn forget_deferred_question() {
    DEFERRED.with(|d| *d.borrow_mut() = None);
}

/// The open channel's offer to this user asks until it's answered: Accept or Decline, no
/// Escape. It closes itself once the offer is gone (withdrawn, or answered elsewhere). A
/// failed answer can be retried or put off until the channel is next opened, so being
/// offline can't trap anyone in it.
fn ask_about_ownership(chat: &Rc<Chat>) {
    let me = chat.me.borrow().clone().unwrap_or_default();
    let current = chat.current.borrow().clone();
    let offer = current.as_deref().and_then(|channel_id| {
        chat.channels
            .borrow()
            .iter()
            .find(|c| c.id == channel_id)
            .and_then(|c| {
                c.owner_offer_for(&me).map(|o| {
                    (
                        offer_key(channel_id, o),
                        channel_id.to_string(),
                        c.title(&me),
                        o.offered_by.clone(),
                        c.members.clone(),
                    )
                })
            })
    });
    let open = ASKING.with(|a| a.borrow().as_ref().map(|(k, _)| k.clone()));
    let deferred = DEFERRED.with(|d| d.borrow().clone());
    let (close, ask) = ownership_question(
        open.as_deref(),
        deferred.as_deref(),
        offer.as_ref().map(|o| o.0.as_str()),
    );
    if close {
        if let Some((_, dialog)) = ASKING.with(|a| a.borrow_mut().take()) {
            dialog.force_close();
        }
    }
    let (true, Some((key, channel_id, title, offered_by, members))) = (ask, offer) else {
        return;
    };
    let by = members
        .iter()
        .find(|m| m.id == offered_by)
        .map(|m| format!("{} (@{})", m.display_name, m.handle))
        .unwrap_or_else(|| "An owner".into());
    let dialog = adw::AlertDialog::new(
        Some("Become an Owner?"),
        Some(&format!(
            "{by} offered to make you an owner of {title}. Owners can rename it, remove members and offer ownership to others."
        )),
    );
    dialog.add_response("decline", "Decline");
    dialog.add_response("accept", "Accept");
    dialog.set_response_appearance("accept", adw::ResponseAppearance::Suggested);
    // No Escape, no close: only an answer (or the offer going away) ends it.
    dialog.set_can_close(false);
    dialog.connect_response(None, {
        let (chat, key) = (chat.clone(), key.clone());
        move |dialog, response| {
            let accept = match response {
                "accept" => true,
                "decline" => false,
                _ => return, // an attempt to dismiss: stays open
            };
            dialog.force_close();
            let (chat, channel_id, key) = (chat.clone(), channel_id.clone(), key.clone());
            glib::spawn_future_local(async move {
                let result = chat
                    .runtime
                    .spawn({
                        let (client, channel_id) = (chat.client.clone(), channel_id.clone());
                        async move {
                            if accept {
                                client.accept_ownership(&channel_id).await.map(|_| ())
                            } else {
                                client.decline_ownership(&channel_id).await
                            }
                        }
                    })
                    .await
                    .unwrap_or(Err(brook_core::Error::UnexpectedResponse));
                if let Err(err) = result {
                    // Withdrawn or answered elsewhere meanwhile: nothing left to answer.
                    let gone = matches!(&err, brook_core::Error::Api { code, .. } if code == "offer.not_found");
                    if !gone {
                        // Put off until asked again, so the refresh below doesn't re-ask.
                        DEFERRED.with(|d| *d.borrow_mut() = Some(key));
                        answer_failed(&chat, &membership_error_text(&err));
                    }
                }
                refresh_channels(&chat, None);
            });
        }
    });
    dialog.connect_closed({
        let key = key.clone();
        move |_| {
            ASKING.with(|a| {
                let mut a = a.borrow_mut();
                if a.as_ref().is_some_and(|(k, _)| *k == key) {
                    *a = None;
                }
            })
        }
    });
    ASKING.with(|a| *a.borrow_mut() = Some((key, dialog.clone())));
    dialog.present(Some(&chat.message_list));
}

/// An answer didn't go through: try again now, or be asked when the channel is next opened.
fn answer_failed(chat: &Rc<Chat>, body: &str) {
    let alert = adw::AlertDialog::new(Some("Couldn't Answer"), Some(body));
    alert.add_response("later", "Ask Me Later");
    alert.add_response("retry", "Try Again");
    alert.set_default_response(Some("retry"));
    alert.set_close_response("later");
    alert.connect_response(None, {
        let chat = chat.clone();
        move |_, response| {
            if response == "retry" {
                forget_deferred_question();
                ask_about_ownership(&chat);
            }
        }
    });
    alert.present(Some(&chat.message_list));
}

/// "Members": who's in the open channel; for a global admin, each can be removed.
fn members_dialog(chat: &Rc<Chat>) {
    let Some(channel_id) = chat.current.borrow().clone() else {
        return;
    };
    let me = chat.me.borrow().clone().unwrap_or_default();
    let members = chat
        .channels
        .borrow()
        .iter()
        .find(|c| c.id == channel_id)
        .map(|c| c.members.clone())
        .unwrap_or_default();
    // The server's rules (#183): an owner or an admin removes others; only an admin
    // removes an owner. The server decides anyway; this only offers what it would allow.
    let admin = *chat.is_admin.borrow();
    let my_role = members
        .iter()
        .find(|m| m.id == me)
        .and_then(|m| m.role.clone());
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .css_classes(["boxed-list"])
        .build();
    let offers: Vec<String> = chat
        .channels
        .borrow()
        .iter()
        .find(|c| c.id == channel_id)
        .map(|c| c.owner_offers.iter().map(|o| o.user_id.clone()).collect())
        .unwrap_or_default();
    // Offering ownership is for owners and admins, to members who aren't owners yet.
    let may_offer = admin || my_role.as_deref() == Some("owner");
    for member in members {
        let offered = offers.contains(&member.id);
        // The person as the preference names them; the other form goes beneath.
        let show_usernames = chat.show_usernames.get();
        let title = brook_core::person_label(&member.display_name, &member.handle, show_usernames);
        // With usernames on, the display name goes beneath, when there is one (a blank name
        // would leave an empty subtitle, or a bare " · owner").
        let other = if show_usernames {
            member.display_name.trim().to_string()
        } else {
            format!("@{}", member.handle)
        };
        let status = match member.role.as_deref() {
            Some("owner") => Some("owner"),
            _ if offered => Some("owner offered"),
            _ => None,
        };
        let subtitle = [Some(other.as_str()).filter(|o| !o.is_empty()), status]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join(" · ");
        let row = adw::ActionRow::builder()
            .title(glib::markup_escape_text(&title).as_str())
            .subtitle(glib::markup_escape_text(&subtitle).as_str())
            .build();
        if may_remove(
            admin,
            my_role.as_deref(),
            member.role.as_deref(),
            member.id == me,
        ) {
            let remove = gtk::Button::builder()
                .label("Remove")
                .valign(gtk::Align::Center)
                .css_classes(["flat", "error"])
                .build();
            remove.connect_clicked({
                let (chat, channel_id, member) = (chat.clone(), channel_id.clone(), member.clone());
                move |button| {
                    button.set_sensitive(false);
                    let (chat, channel_id, user_id) =
                        (chat.clone(), channel_id.clone(), member.id.clone());
                    let button = button.clone();
                    glib::spawn_future_local(async move {
                        let result = chat
                            .runtime
                            .spawn({
                                let client = chat.client.clone();
                                async move { client.remove_member(&channel_id, &user_id).await }
                            })
                            .await
                            .unwrap_or(Err(brook_core::Error::UnexpectedResponse));
                        match result {
                            // The channel.update echo refreshes the list; this row goes.
                            Ok(()) => button.set_label("Removed"),
                            // Already gone (removed elsewhere, or they left): what Remove
                            // wanted, so nothing to report (as on the Mac).
                            Err(brook_core::Error::Api { code, .. }) if code == "not_found" => {
                                button.set_label("Removed")
                            }
                            Err(err) => {
                                button.set_sensitive(true);
                                show_alert(&chat, "Couldn't Remove", &membership_error_text(&err));
                            }
                        }
                    });
                }
            });
            row.add_suffix(&remove);
        }
        if may_offer && member.id != me && member.role.as_deref() != Some("owner") {
            let (label, done, heading) = if offered {
                ("Withdraw", "Withdrawn", "Couldn't Withdraw")
            } else {
                ("Make owner…", "Offered", "Couldn't Offer")
            };
            let button = gtk::Button::builder()
                .label(label)
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            button.connect_clicked({
                let (chat, channel_id, member) = (chat.clone(), channel_id.clone(), member.clone());
                move |button| {
                    let (channel_id, member) = (channel_id.clone(), member.clone());
                    member_action(&chat, button, done, heading, move |client| async move {
                        if offered {
                            client
                                .withdraw_ownership_offer(&channel_id, &member.id)
                                .await
                        } else {
                            client
                                .offer_ownership(&channel_id, &member.handle)
                                .await
                                .map(|_| ())
                        }
                    });
                }
            });
            row.add_suffix(&button);
        }
        list.append(&row);
    }
    let scroller = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .min_content_height(120)
        .max_content_height(360)
        .propagate_natural_height(true)
        .child(&list)
        .build();
    let dialog = adw::AlertDialog::builder()
        .heading("Members")
        .extra_child(&scroller)
        .build();
    dialog.add_response("close", "Close");
    dialog.present(Some(&chat.message_list));
}

/// "Edit Profile": your display name and status line (the handle, which signs in, stays).
fn edit_profile_dialog(chat: &Rc<Chat>) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let me = chat
            .runtime
            .spawn({
                let client = chat.client.clone();
                async move { client.me().await }
            })
            .await;
        let Ok(Ok(me)) = me else {
            show_alert(
                &chat,
                "Couldn't Load Your Profile",
                "Try again in a moment.",
            );
            return;
        };
        let name = adw::EntryRow::builder()
            .title("Display name")
            .text(&me.user.display_name)
            .build();
        let status = adw::EntryRow::builder()
            .title("Status")
            .text(me.user.status_text.as_deref().unwrap_or(""))
            .build();
        let handle = adw::ActionRow::builder()
            .title("Handle")
            .subtitle(glib::markup_escape_text(&format!("@{}", me.user.handle)).as_str())
            .build();
        let list = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        list.append(&name);
        list.append(&status);
        list.append(&handle);
        let dialog = adw::AlertDialog::builder()
            .heading("Edit Profile")
            .extra_child(&list)
            .build();
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("save", "Save");
        dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("save"));
        // The server's lengths, checked as the user types: Save waits for a name that fits,
        // so a refusal never throws the edit away.
        let check: Rc<dyn Fn()> = Rc::new({
            let (dialog, name, status) = (dialog.clone(), name.clone(), status.clone());
            move || {
                let ok = profile_fits(&name.text(), &status.text());
                dialog.set_response_enabled("save", ok);
            }
        });
        name.connect_changed({
            let check = check.clone();
            move |_| check()
        });
        status.connect_changed({
            let check = check.clone();
            move |_| check()
        });
        check();
        let (old_name, old_status) = (
            me.user.display_name,
            me.user.status_text.unwrap_or_default(),
        );
        dialog.connect_response(None, {
            let chat = chat.clone();
            move |_, response| {
                if response != "save" {
                    return;
                }
                // Only what changed is sent (omitted fields stay).
                let (new_name, new_status) = (name.text().to_string(), status.text().to_string());
                let name_change = (new_name.trim() != old_name.trim()).then_some(new_name);
                let status_change = (new_status.trim() != old_status.trim()).then_some(new_status);
                if name_change.is_none() && status_change.is_none() {
                    return;
                }
                let chat = chat.clone();
                glib::spawn_future_local(async move {
                    let result = chat
                        .runtime
                        .spawn({
                            let client = chat.client.clone();
                            async move {
                                client
                                    .update_profile(
                                        name_change.as_deref(),
                                        status_change.as_deref(),
                                    )
                                    .await
                            }
                        })
                        .await
                        .unwrap_or(Err(brook_core::Error::UnexpectedResponse));
                    if let Err(err) = result {
                        show_alert(
                            &chat,
                            "Couldn't Save Your Profile",
                            &membership_error_text(&err),
                        );
                    }
                });
            }
        });
        dialog.present(Some(&chat.message_list));
    });
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
            if already_gone(&err) {
                // A missed event can leave a stale row: read the list again.
                refresh_channels(&chat, None);
            } else {
                show_conversation_failure(&chat, ConvAction::Update, &err);
            }
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
                    if already_gone(&err) {
                        refresh_channels(&chat, None);
                    } else {
                        show_conversation_failure(&chat, ConvAction::Delete, &err);
                    }
                }
            });
        }
    });
    dialog.present(Some(&chat.message_list));
}

/// A sidebar row; returns the row and its (initially-styled) unread badge label.
fn channel_row(
    title: &str,
    is_dm: bool,
    (unread, mentions): (i64, i64),
    offered: bool,
) -> (gtk::ListBoxRow, Badge) {
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
    let badge = Badge {
        mentions: gtk::Label::builder()
            .css_classes(["caption-heading", "mention-badge"])
            .build(),
        unread: gtk::Label::builder()
            .css_classes(["caption-heading", "accent"])
            .build(),
    };
    show_badge(&badge, unread, mentions);
    row.append(&icon);
    row.append(&label);
    // An ownership offer waits for this user here: highlighted until it's answered.
    if offered {
        label.add_css_class("heading");
        row.append(
            &gtk::Label::builder()
                .label("Owner offer")
                .css_classes(["caption", "accent"])
                .tooltip_text("You're offered ownership of this channel: open it to answer")
                .build(),
        );
    }
    row.append(&badge.mentions);
    row.append(&badge.unread);
    (gtk::ListBoxRow::builder().child(&row).build(), badge)
}

/// Refresh a single row's badge from the channel's current unread and mention counts.
fn update_badge(chat: &Rc<Chat>, idx: usize) {
    let counts = chat
        .channels
        .borrow()
        .get(idx)
        .map(|c| (c.unread_count, c.unread_mentions));
    if let (Some((unread, mentions)), Some(badge)) = (counts, chat.badges.borrow().get(idx)) {
        show_badge(badge, unread, mentions);
    }
}

/// A channel's counts in the sidebar: "@M" in a filled accent pill for unread messages
/// that mention you, beside the plain unread count (as on the Mac).
struct Badge {
    mentions: gtk::Label,
    unread: gtk::Label,
}

/// Show a channel's counts; each is hidden at zero.
fn show_badge(badge: &Badge, unread: i64, mentions: i64) {
    let (mention_text, unread_text) = badge_texts(unread, mentions);
    badge.mentions.set_visible(!mention_text.is_empty());
    badge.mentions.set_label(&mention_text);
    let tip = match mentions {
        m if m <= 0 => None,
        1 => Some("1 unread message mentions you".to_string()),
        m => Some(format!("{m} unread messages mention you")),
    };
    badge.mentions.set_tooltip_text(tip.as_deref());
    badge.unread.set_visible(!unread_text.is_empty());
    badge.unread.set_label(&unread_text);
}

/// The two badges' texts: "@M" for unread mentions and "N" for unread messages, "" for
/// none (never fewer unread than mentions: a count that lags shows the mentions).
fn badge_texts(unread: i64, mentions: i64) -> (String, String) {
    let mentions = mentions.max(0);
    let unread = unread.max(mentions);
    let text = |n: i64, prefix: &str| {
        if n > 0 {
            format!("{prefix}{n}")
        } else {
            String::new()
        }
    };
    (text(mentions, "@"), text(unread, ""))
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

/// How long an error keeps the typing line.
const ERROR_SHOWN: Duration = Duration::from_secs(6);

/// The typing line is shared with errors (a failed send or reaction). While an error is held,
/// typing and messages leave the line alone; when the hold ends, typing is drawn again.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ErrorHold {
    until: Option<Instant>,
}

impl ErrorHold {
    /// An error is shown now: it holds the line for `ERROR_SHOWN` (a newer one renews it).
    fn hold(&mut self, now: Instant) {
        self.until = Some(now + ERROR_SHOWN);
    }

    fn holding(&self, now: Instant) -> bool {
        self.until.is_some_and(|until| until > now)
    }

    /// An error's timer fired. Over: draw the line again. Wait: the hold lasts longer (a newer
    /// error renewed it, or GLib's loop clock ran ahead of ours and fired early), so the timer
    /// is set again for what's left; nothing is ever left holding with no timer.
    fn timer_fired(&mut self, now: Instant) -> HoldTimer {
        match self.until {
            Some(until) if until > now => HoldTimer::Wait(until - now),
            _ => {
                self.until = None;
                HoldTimer::Over
            }
        }
    }

    /// Another channel was opened: an error about the last one goes now.
    fn clear(&mut self) {
        self.until = None;
    }
}

/// What an error's timer should do when it fires.
#[derive(Debug, PartialEq, Eq)]
enum HoldTimer {
    Over,
    Wait(Duration),
}

/// What the typing line shows.
#[derive(Debug, PartialEq, Eq)]
enum LineShown {
    /// An error holds it: leave it alone.
    Error,
    Typing(String),
    Nothing,
}

/// The state half of opening another channel: nobody is typing there yet, and an error about the
/// last one goes.
fn switch_channel(typing: &mut TypingState, hold: &mut ErrorHold) {
    typing.reset();
    hold.clear();
}

fn line_shown(hold: &ErrorHold, typing: &TypingState, now: Instant) -> LineShown {
    if hold.holding(now) {
        return LineShown::Error;
    }
    typing
        .line(now)
        .map_or(LineShown::Nothing, LineShown::Typing)
}

/// Who is typing in the open channel, as events say. Each shows for `LIFETIME` after their
/// last event, or until a message from them arrives.
#[derive(Default)]
struct TypingState {
    seen: Vec<(String, String, Instant)>,
    last_message: HashMap<String, Instant>,
}

impl TypingState {
    const LIFETIME: Duration = Duration::from_secs(4);
    /// A typing notice this soon after their message is the one sent just before it (two
    /// requests, no ordering between them), not a new one.
    const AFTER_MESSAGE: Duration = Duration::from_secs(2);

    fn note(&mut self, user_id: &str, name: &str, at: Instant) {
        if let Some(sent) = self.last_message.get(user_id) {
            if at.saturating_duration_since(*sent) < Self::AFTER_MESSAGE {
                return;
            }
        }
        match self.seen.iter_mut().find(|(id, _, _)| id == user_id) {
            Some(entry) => (entry.1, entry.2) = (name.to_string(), at),
            None => self.seen.push((user_id.to_string(), name.to_string(), at)),
        }
    }

    /// A message from them: they're done.
    fn clear(&mut self, user_id: &str, at: Instant) {
        self.seen.retain(|(id, _, _)| id != user_id);
        self.last_message.insert(user_id.to_string(), at);
    }

    fn reset(&mut self) {
        *self = Self::default();
    }

    fn live(&self, now: Instant) -> impl Iterator<Item = &(String, String, Instant)> {
        self.seen
            .iter()
            .filter(move |(_, _, at)| now.saturating_duration_since(*at) < Self::LIFETIME)
    }

    /// "Ann is typing…", "Ann and Bob are typing…", "Several people are typing…"; none when
    /// nobody is.
    fn line(&self, now: Instant) -> Option<String> {
        let mut names: Vec<&str> = self.live(now).map(|(_, n, _)| n.as_str()).collect();
        names.sort_unstable();
        match names.as_slice() {
            [] => None,
            [one] => Some(format!("{one} is typing\u{2026}")),
            [a, b] => Some(format!("{a} and {b} are typing\u{2026}")),
            _ => Some("Several people are typing\u{2026}".to_string()),
        }
    }

    /// How long until the first of those shown expires.
    fn next_expiry(&self, now: Instant) -> Option<Duration> {
        self.live(now)
            .map(|(_, _, at)| Self::LIFETIME.saturating_sub(now.saturating_duration_since(*at)))
            .min()
    }
}

/// Note that `user_id` is typing, and redraw the line.
fn show_typing(chat: &Rc<Chat>, user_id: &str, name: &str) {
    chat.typing.borrow_mut().note(user_id, name, Instant::now());
    render_typing(chat);
}

/// A message from `user_id` arrived: they're no longer typing.
fn clear_typing_of(chat: &Rc<Chat>, user_id: &str) {
    chat.typing.borrow_mut().clear(user_id, Instant::now());
    render_typing(chat);
}

/// Draw who's typing, and wake up when the first of them expires.
fn render_typing(chat: &Rc<Chat>) {
    let now = Instant::now();
    let (shown, expiry) = {
        let state = chat.typing.borrow();
        (
            line_shown(&chat.error_hold.get(), &state, now),
            state.next_expiry(now),
        )
    };
    match shown {
        // An error is showing: it keeps the line until its own timer ends it, which draws this.
        LineShown::Error => return,
        LineShown::Typing(line) => {
            chat.typing_label.remove_css_class("error");
            chat.typing_label.set_label(&line);
            chat.typing_label.set_visible(true);
        }
        LineShown::Nothing => {
            chat.typing_label.remove_css_class("error");
            chat.typing_label.set_visible(false);
        }
    }
    if let Some(id) = chat.typing_timeout.borrow_mut().take() {
        id.remove();
    }
    if let Some(expiry) = expiry {
        let chat2 = chat.clone();
        let id = glib::timeout_add_local_once(expiry + Duration::from_millis(50), move || {
            // Fired: forget the id first, so nothing removes a source that's gone.
            *chat2.typing_timeout.borrow_mut() = None;
            render_typing(&chat2);
        });
        *chat.typing_timeout.borrow_mut() = Some(id);
    }
}

/// Clear any typing indicator (e.g. on channel switch).
fn clear_typing(chat: &Rc<Chat>) {
    // An error about the channel just left doesn't follow you into the next.
    let mut hold = chat.error_hold.get();
    switch_channel(&mut chat.typing.borrow_mut(), &mut hold);
    chat.error_hold.set(hold);
    chat.typing_label.remove_css_class("error");
    render_typing(chat);
}

/// Whether a message calls for this user's attention: someone else's, not deleted, naming
/// them or everyone. Mentions are stored with the message (#195), so cached rows and
/// history carry them as well as live ones.
fn mentions_me(message: &Message, me: &str) -> bool {
    !me.is_empty()
        && message.author_id != me
        && !message.is_deleted()
        && (message.mention_everyone || message.mentions.iter().any(|m| m == me))
}

/// A notification's text for someone else's message: what they wrote, "mentioned you" when
/// it names this user (or everyone), and "sent a file" for files with no text.
fn notification_body(message: &Message, me: &str, author: &str) -> String {
    if message.body.trim().is_empty() && !message.attachments.is_empty() {
        return format!("{author} sent a file");
    }
    let mentioned = message.mention_everyone || message.mentions.iter().any(|m| m == me);
    if mentioned {
        format!("{author} mentioned you: {}", message.body)
    } else {
        format!("{author}: {}", message.body)
    }
}

/// Whether the user can be looking at the open conversation: its window is focused.
fn window_focused(chat: &Rc<Chat>) -> bool {
    chat.message_list
        .root()
        .and_downcast::<gtk::Window>()
        .is_some_and(|w| w.is_active())
}

/// The window is focused again: what arrived in the open conversation meanwhile is read, and
/// its badge clears.
fn read_what_arrived(chat: &Rc<Chat>) {
    if !chat.read_owed.replace(false) {
        return;
    }
    let Some(current) = chat.current.borrow().clone() else {
        return;
    };
    mark_read(chat, current.clone(), None);
    let idx = chat.channels.borrow().iter().position(|c| c.id == current);
    if let Some(idx) = idx {
        let mut channels = chat.channels.borrow_mut();
        (channels[idx].unread_count, channels[idx].unread_mentions) = (0, 0);
        drop(channels);
        update_badge(chat, idx);
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
            let handle = clean_handle(&dm_entry.text());
            if handle.is_empty() {
                return;
            }
            popover.popdown();
            // The text stays until the server accepts the handle, so a typo is fixed, not retyped.
            let entry = dm_entry.clone();
            open_dm(&chat, handle, move || entry.set_text(""));
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
                popover.popdown();
                let (entry, check) = (name_entry.clone(), public_check.clone());
                create_channel(&chat, name, public, move || {
                    entry.set_text("");
                    check.set_active(false);
                });
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
        let channels = match handle.await {
            Ok(Ok(channels)) => channels,
            Ok(Err(err)) => {
                show_conversation_failure(&chat, ConvAction::Browse, &err);
                return;
            }
            Err(_) => return,
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
        match handle.await {
            Ok(Ok(channel)) => refresh_channels(&chat, Some(channel.id)),
            // Gone, archived or no longer public: retrying never helps.
            Ok(Err(err)) if already_gone(&err) => show_alert(
                &chat,
                "Couldn't Join",
                "That channel isn't open to join any more.",
            ),
            Ok(Err(err)) => show_conversation_failure(&chat, ConvAction::Join, &err),
            Err(_) => {}
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

fn open_dm(chat: &Rc<Chat>, handle: String, on_opened: impl FnOnce() + 'static) {
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let join = chat.runtime.spawn({
            let client = chat.client.clone();
            async move { client.open_dm(&handle).await }
        });
        match join.await {
            Ok(Ok(channel)) => {
                on_opened();
                refresh_channels(&chat, Some(channel.id));
            }
            Ok(Err(err)) => show_conversation_failure(&chat, ConvAction::StartDm, &err),
            Err(_) => {}
        }
    });
}

fn create_channel(
    chat: &Rc<Chat>,
    name: String,
    public: bool,
    on_created: impl FnOnce() + 'static,
) {
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
        match join.await {
            Ok(Ok(channel)) => {
                on_created();
                refresh_channels(&chat, Some(channel.id));
            }
            Ok(Err(err)) => show_conversation_failure(&chat, ConvAction::CreateChannel, &err),
            Err(_) => {}
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
            let handle = clean_handle(&entry.text());
            let Some(channel_id) = chat.current.borrow().clone() else {
                return;
            };
            if handle.is_empty() {
                return;
            }
            popover.popdown();
            let chat = chat.clone();
            let entry = entry.clone();
            glib::spawn_future_local(async move {
                let join = chat.runtime.spawn({
                    let client = chat.client.clone();
                    async move { client.add_member(&channel_id, &handle).await }
                });
                // The server fans out channel.update; the new member's client
                // refreshes itself. Reload ours too so the member count updates.
                match join.await {
                    Ok(Ok(())) => {
                        entry.set_text(""); // kept on a failure, so a typo isn't retyped
                        refresh_channels(&chat, None);
                    }
                    Ok(Err(err)) if already_gone(&err) => {
                        entry.set_text("");
                        refresh_channels(&chat, None);
                    }
                    Ok(Err(err)) => show_conversation_failure(&chat, ConvAction::AddMember, &err),
                    Err(_) => {}
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
            "transfer.cancelled" => "Cancelled".into(),
            "outbox.snapshot_damaged" => "Not sent: a file's saved copy is damaged".into(),
            c if c.starts_with("file.") || c == "outbox.duplicate_file" => {
                "Not sent: a file was refused".into()
            }
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
            Ok(Ok(pending)) => {
                chat.local_open.set(true);
                pending
            }
            _ => Vec::new(), // no local storage: nothing is ever queued
        };
        if chat.current.borrow().as_deref() != Some(channel_id.as_str()) {
            return;
        }
        for row in chat.pending_rows.borrow_mut().drain(..) {
            chat.message_list.remove(&row);
        }
        let old: Vec<_> = chat.pending_ids.borrow_mut().drain(..).collect();
        chat.progress.forget(old);
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
            has_files: w.has_files.get(),
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
            .visible(!item.body.trim().is_empty() || item.files.is_empty())
            .build(),
    );
    for file in &item.files {
        column.append(&crate::outgoing::pending_file_line(file, &chat.progress));
        chat.pending_ids.borrow_mut().push(file.transfer_id);
    }
    let footer = gtk::Box::builder().spacing(6).build();
    let failed = matches!(item.state, PendingState::Failed { .. });
    // Files still to upload: Cancel stops the message's sending (it fails; Retry resumes).
    if let Some(file) = item.files.iter().find(|f| !f.uploaded).filter(|_| !failed) {
        let cancel = gtk::Button::builder()
            .label("Cancel")
            .css_classes(["flat"])
            .build();
        let (client, tid) = (chat.client.clone(), file.transfer_id);
        cancel.connect_clicked(move |_| client.cancel_transfer(tid));
        footer.append(&cancel);
    }
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
                // Profiles changed: authors on screen take their current names.
                Ok(CacheEvent::Users(ids)) => redraw_authors(&chat, ids),
                // Cached files changed (fetched, kept, evicted, gone): rows showing them
                // re-read their state.
                Ok(CacheEvent::Files(ids)) => crate::attachments::refresh_rows(&ids),
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// How a message header names its author: the display name, or `@handle` with "Show usernames"
/// (and when there is no name). "Unknown" when the message carries neither.
fn author_text(display_name: &str, handle: &str, show_usernames: bool) -> String {
    if handle.is_empty() {
        // Nothing to name them by but the name (a message that carries no handle).
        let name = display_name.trim();
        return if name.is_empty() { "Unknown" } else { name }.to_string();
    }
    brook_core::person_label(display_name, handle, show_usernames)
}

/// The authors on screen, named by the current preference (after it changes).
fn relabel_authors(chat: &Rc<Chat>) {
    let names = chat.author_names.borrow();
    for widgets in chat.message_rows.borrow().values() {
        let name = names
            .get(&widgets.author_id)
            .map_or(widgets.author_fallback.as_str(), String::as_str);
        widgets.author.set_label(&author_text(
            name,
            &widgets.author_handle,
            chat.show_usernames.get(),
        ));
    }
}

/// Put the cache's current names on the authors shown among `ids` (message rows keep the
/// name they were stored with).
fn redraw_authors(chat: &Rc<Chat>, ids: Vec<String>) {
    let shown: Vec<String> = {
        let rows = chat.message_rows.borrow();
        ids.into_iter()
            .filter(|id| rows.values().any(|w| w.author_id == *id))
            .collect()
    };
    if shown.is_empty() {
        return;
    }
    let chat = chat.clone();
    glib::spawn_future_local(async move {
        let handle = chat.runtime.spawn({
            let client = chat.client.clone();
            async move { client.cached_users(&shown).await }
        });
        let Ok(Ok(users)) = handle.await else { return };
        let names: HashMap<String, String> = users
            .into_iter()
            // A blank name stays blank: `author_text` then names them `@handle`.
            .map(|u| (u.id, u.display_name))
            .collect();
        for widgets in chat.message_rows.borrow().values() {
            if let Some(name) = names.get(&widgets.author_id) {
                widgets.author.set_label(&author_text(
                    name,
                    &widgets.author_handle,
                    chat.show_usernames.get(),
                ));
            }
        }
        // Rows drawn later (an older page, a redraw after a reset) use them too.
        chat.author_names.borrow_mut().extend(names);
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
        chat.local_open.set(true);
        let current = chat.current.borrow().clone();
        let updates: Vec<(usize, (i64, i64))> = chat
            .channels
            .borrow()
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                let fresh = cached.iter().find(|f| f.id == c.id)?;
                // The open channel is being read: its badge stays clear, unless the window
                // is in the background (then what arrived there isn't read yet).
                let counts = if current.as_deref() == Some(c.id.as_str()) && !chat.read_owed.get() {
                    (0, 0)
                } else {
                    (fresh.unread_count, fresh.unread_mentions)
                };
                (counts != (c.unread_count, c.unread_mentions)).then_some((i, counts))
            })
            .collect();
        for (i, (unread, mentions)) in updates {
            let mut channels = chat.channels.borrow_mut();
            (channels[i].unread_count, channels[i].unread_mentions) = (unread, mentions);
            drop(channels);
            update_badge(&chat, i);
        }
        // What a catch-up brought is new activity too: re-sort once if it moved a conversation
        // that isn't the open one or one whose history back-fill is still awaited (opening a
        // conversation loads its old messages, which is not news and must not move its row,
        // not even after you've switched away).
        let moved = {
            let items: Vec<(String, Option<String>)> = cached
                .iter()
                .map(|f| (f.id.clone(), f.last_message_id.clone()))
                .collect();
            chat.sidebar.borrow_mut().notice(&items, current.as_deref())
        };
        if moved {
            resort_sidebar(&chat);
        }
    });
}

/// How long the cache must stay offline before the banner says so.
const OFFLINE_BANNER_DELAY: Duration = Duration::from_secs(3);

/// What the banner does when the cache's offline flag is read.
#[derive(Debug, PartialEq, Eq)]
enum BannerAction {
    /// Online: hide it at once, and any reveal still waiting is cancelled.
    Hide,
    /// Offline and not shown: start a timer, which must call `on_fire` with this number.
    Start(u64),
    /// Nothing to do (already shown, or a timer is already waiting).
    Keep,
}

/// The offline banner's bookkeeping, apart from GTK so it can be tested. Each started timer
/// has a number, and only the newest waiting one may reveal the banner, so a timer that is
/// stale (the flag went false and true again, or the loop ended) can't show it early even if
/// it still fires.
#[derive(Default)]
struct OfflineBanner {
    waiting: Option<u64>,
    last_timer: u64,
}

impl OfflineBanner {
    fn on_state(&mut self, offline: bool, revealed: bool) -> BannerAction {
        if !offline {
            self.waiting = None;
            return BannerAction::Hide;
        }
        if revealed || self.waiting.is_some() {
            return BannerAction::Keep;
        }
        self.last_timer += 1;
        self.waiting = Some(self.last_timer);
        BannerAction::Start(self.last_timer)
    }

    /// A timer fired: whether to reveal the banner now. Only the waiting timer may, and going
    /// online (or the watch ending) clears it, so "still offline" is implied.
    fn on_fire(&mut self, timer: u64) -> bool {
        if self.waiting != Some(timer) {
            return false;
        }
        self.waiting = None;
        true
    }

    /// The watch ended: nothing waiting may reveal anything.
    fn stop(&mut self) {
        self.waiting = None;
    }
}

/// Watch the cache's offline flag: show the offline banner `OFFLINE_BANNER_DELAY` after the
/// flag turns true if it hasn't gone false since (a flip back within the delay shows nothing;
/// the flag is a watch value, so an offline-online-offline flap shorter than one poll can show
/// the banner slightly early), and hide it at once when it goes false. Also the one-time
/// clean-up of other accounts' saved data once this user's storage is open.
fn watch_offline(chat: &Rc<Chat>) {
    // Core's state feed (#113): it follows sign-ins and switches by itself and resets
    // to the default on sign-out, so the banner never shows a previous user's state.
    let mut state = chat.client.subscribe_cache_state();
    let chat_weak = Rc::downgrade(chat);
    glib::spawn_future_local(async move {
        let mut checked_others = false;
        // The banner's bookkeeping (a flaky link flips the flag within seconds: not every flip
        // is shown), and the timer that reveals it once offline has lasted.
        let banner_state = Rc::new(RefCell::new(OfflineBanner::default()));
        let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
        loop {
            let current = state.borrow_and_update().clone();
            let Some(chat) = chat_weak.upgrade() else {
                break;
            };
            if chat.offline_banner.root().is_none() {
                break; // signed out: the view is gone
            }
            let action = banner_state
                .borrow_mut()
                .on_state(current.offline, chat.offline_banner.is_revealed());
            match action {
                BannerAction::Hide => {
                    if let Some(id) = pending.borrow_mut().take() {
                        id.remove();
                    }
                    chat.offline_banner.set_revealed(false);
                }
                BannerAction::Start(generation) => {
                    let banner = chat.offline_banner.downgrade();
                    let (banner_state, slot) = (banner_state.clone(), pending.clone());
                    let id = glib::timeout_add_local_once(OFFLINE_BANNER_DELAY, move || {
                        // Fired: forget the id first, so nothing removes a source that's gone.
                        *slot.borrow_mut() = None;
                        let show = banner_state.borrow_mut().on_fire(generation);
                        if let Some(banner) = banner.upgrade() {
                            if show {
                                banner.set_revealed(true);
                            }
                        }
                    });
                    *pending.borrow_mut() = Some(id);
                }
                BannerAction::Keep => {}
            }
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
        // The loop is over (signed out): no timer may fire on a banner that's gone.
        banner_state.borrow_mut().stop();
        let leftover = pending.borrow_mut().take();
        if let Some(id) = leftover {
            id.remove();
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
        // Their unsent counts are read before the wipe (#46 §8), so the notice can say what
        // went with it.
        let lookup = chat.runtime.spawn({
            let client = chat.client.clone();
            async move { client.other_local_users().await }
        });
        let Ok(Ok(others)) = lookup.await else {
            return;
        };
        if others.is_empty() {
            return;
        }
        // Their sidebar orders go on this thread (every writer of that file is here) and
        // before the wipe, which stops at the first error (#235).
        let ids: Vec<String> = others.iter().map(|o| o.user_id.clone()).collect();
        crate::prefs::forget_all_opened(&crate::prefs::others_to_forget(chat.ended.get(), &ids));
        if chat.ended.get() {
            return;
        }
        let wipe = chat.runtime.spawn({
            let client = chat.client.clone();
            async move { client.wipe_other_local_users().await }
        });
        if let Ok(Ok(())) = wipe.await {
            let unsent: Vec<Option<u64>> = others.iter().map(|o| o.unsent).collect();
            let alert = adw::AlertDialog::new(
                Some("Saved Data Removed"),
                Some(&others_removed_text(&unsent)),
            );
            alert.add_response("ok", "OK");
            alert.present(Some(&chat.message_list));
        }
    });
}

/// The notice after another account's saved data was removed: with its unsent messages when
/// they could be counted, and a "may have" when any couldn't.
fn others_removed_text(unsent: &[Option<u64>]) -> String {
    let base = "Another account's saved messages were removed from this device";
    if unsent.iter().any(Option::is_none) {
        return format!("{base}. They may have included unsent messages.");
    }
    match unsent.iter().flatten().sum::<u64>() {
        0 => format!("{base}."),
        1 => format!("{base}, including 1 unsent message."),
        n => format!("{base}, including {n} unsent messages."),
    }
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
        let options = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .build();
        options.append(&remove);
        options.append(
            &gtk::Label::builder()
                .label(lost_device_help(*chat.is_admin.borrow()))
                .wrap(true)
                .xalign(0.0)
                .css_classes(["caption", "dim-label"])
                .build(),
        );
        let known = chat.local_open.get();
        let body = sign_out_body(unsent, known, true);
        let dialog = adw::AlertDialog::new(Some("Sign Out?"), Some(&body));
        dialog.set_extra_child(Some(&options));
        remove.connect_toggled({
            let dialog = dialog.clone();
            move |check| dialog.set_body(&sign_out_body(unsent, known, check.is_active()))
        });
        dialog.add_response("cancel", "Cancel");
        dialog.add_response("sign-out", "Sign Out");
        dialog.set_response_appearance("sign-out", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.connect_response(None, {
            let chat = chat.clone();
            move |_, response| {
                if response == "sign-out" {
                    chat.ended.set(true);
                    // Even if erasing the rest fails: the user asked for it (#235).
                    let me = chat.me.borrow().clone().unwrap_or_default();
                    if crate::prefs::sign_out_forgets(remove.is_active(), &me) {
                        chat.ranks_forgotten.set(true);
                    }
                    (chat.sign_out)(remove.is_active());
                }
            }
        });
        dialog.present(Some(&chat.message_list));
    });
}

/// Under the checkbox (owner decision, #46 §8): a lost device's *account* access is cut off by
/// ending its sign-in. The data already saved on it is not: it is encrypted with that device's
/// own random key, held in that device's keyring, so it stays readable to whoever can sign in
/// to that computer. The text says both. An admin can't be reset by another admin (the server
/// refuses), so they're only told the password route.
fn lost_device_help(admin: bool) -> String {
    let switch = crate::account::SIGN_OUT_OTHERS_LABEL;
    let reset = if admin {
        ""
    } else {
        ", or ask an admin to reset your account"
    };
    format!(
        "If you lose a device, change your password with \"{switch}\" on{reset}. That ends its \
         sign-in, but messages already saved on it stay readable to anyone who can sign in to \
         that computer."
    )
}

/// What signing out does to this device's data, in words.
/// `known`: this user's stores answered a cached call, so `unsent` is a real count (it's 0
/// while they're closed, which isn't the same as none).
fn sign_out_body(unsent: u64, known: bool, remove: bool) -> String {
    let mut text = if remove {
        String::from("Saved messages and files are removed from this device.")
    } else {
        String::from("Saved messages stay on this device for your next sign-in.")
    };
    if remove && !known {
        text.push_str(" Unsent messages on this device may be deleted.");
    } else if remove && unsent > 0 {
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
    fn a_profile_fits_the_servers_lengths() {
        assert!(profile_fits("Ana", ""));
        assert!(!profile_fits("   ", ""), "a blank name");
        assert!(
            profile_fits(&"é".repeat(64), &"x".repeat(100)),
            "characters, not bytes"
        );
        assert!(!profile_fits(&"a".repeat(65), ""));
        assert!(!profile_fits("Ana", &"x".repeat(101)));
    }

    #[test]
    fn remove_is_offered_as_the_server_allows_it() {
        // Never yourself.
        assert!(!may_remove(true, Some("owner"), Some("owner"), true));
        // An admin removes anyone, owners included.
        assert!(may_remove(true, None, Some("owner"), false));
        assert!(may_remove(true, Some("member"), Some("member"), false));
        // An owner removes members, not other owners.
        assert!(may_remove(false, Some("owner"), Some("member"), false));
        assert!(may_remove(false, Some("owner"), None, false));
        assert!(!may_remove(false, Some("owner"), Some("owner"), false));
        // A member removes no one; unknown roles offer nothing.
        assert!(!may_remove(false, Some("member"), Some("member"), false));
        assert!(!may_remove(false, None, None, false));
    }

    #[test]
    fn membership_refusals_read_as_sentences() {
        let api = |code: &str| brook_core::Error::Api {
            code: code.into(),
            message: String::new(),
        };
        assert!(membership_error_text(&api("channel.last_owner")).contains("Delete the channel"));
        assert!(membership_error_text(&api("authz.forbidden")).contains("owner"));
        assert!(membership_error_text(&api("profile.invalid")).contains("invisible"));
        assert!(membership_error_text(&api("something.new")).contains("Try again"));
    }

    #[test]
    fn a_notification_says_what_arrived() {
        let mut m: Message = serde_json::from_value(serde_json::json!({
            "id": "m1", "channel_id": "c", "author_id": "u9", "body": "hello",
            "created_at": "2026-09-26T00:00:00Z"
        }))
        .unwrap();
        assert_eq!(notification_body(&m, "me", "Bo"), "Bo: hello");
        m.mentions = vec!["me".into()];
        assert_eq!(notification_body(&m, "me", "Bo"), "Bo mentioned you: hello");
        m.mentions.clear();
        m.mention_everyone = true;
        assert_eq!(notification_body(&m, "me", "Bo"), "Bo mentioned you: hello");
        m.body = String::new();
        m.attachments = vec![serde_json::from_value(serde_json::json!({
            "id": "f", "channel_id": "c", "uploader_id": "u9", "filename": "a.png",
            "original_name": "a.png", "size": 1, "content_type": "image/png",
            "status": "committed", "sha256": "x"
        }))
        .unwrap()];
        assert_eq!(notification_body(&m, "me", "Bo"), "Bo sent a file");
    }

    #[test]
    fn a_failed_text_keeps_its_outbox_id_for_the_retry() {
        let d = |channel: &str, body: &str, reply: Option<&str>| Draft {
            channel: channel.into(),
            body: body.into(),
            reply_to: reply.map(str::to_string),
            files: Vec::new(),
        };
        let mut draft = None;
        let first = draft_id(&mut draft, d("c1", "hello", None));
        assert_eq!(
            draft_id(&mut draft, d("c1", "hello", None)),
            first,
            "the same send again"
        );
        let edited = draft_id(&mut draft, d("c1", "hello!", None));
        assert_ne!(edited, first, "edited text is a new message");
        let other = draft_id(&mut draft, d("c2", "hello!", None));
        assert_ne!(other, edited, "another channel");
        // Core would answer a reused id with the stored (unquoted) message.
        let quoted = draft_id(&mut draft, d("c2", "hello!", Some("m1")));
        assert_ne!(quoted, other, "a changed quote is a new message");
        assert_ne!(draft_id(&mut draft, d("c2", "hello!", Some("m2"))), quoted);
        // A message with files: removing one after a failure is a new message too (core
        // would otherwise answer with the stored one, the removed file included).
        let (a, b) = (brook_core::TransferId::new(), brook_core::TransferId::new());
        let with = |files: Vec<brook_core::TransferId>| Draft {
            files,
            ..d("c1", "", None)
        };
        let both = draft_id(&mut draft, with(vec![a, b]));
        assert_eq!(
            draft_id(&mut draft, with(vec![a, b])),
            both,
            "the same files again"
        );
        assert_ne!(draft_id(&mut draft, with(vec![a])), both, "a file removed");
    }

    #[test]
    fn the_other_account_notice_says_what_went_with_it() {
        assert!(others_removed_text(&[Some(0)]).ends_with("this device."));
        assert!(others_removed_text(&[Some(1)]).contains("including 1 unsent message."));
        assert!(others_removed_text(&[Some(2), Some(1)]).contains("including 3 unsent messages"));
        assert!(others_removed_text(&[Some(2), None]).contains("may have included unsent"));
    }

    #[test]
    fn sign_out_warns_only_when_unsent_messages_would_go() {
        assert!(sign_out_body(0, true, true).contains("removed from this device"));
        assert!(!sign_out_body(0, true, true).contains("deleted"));
        assert!(sign_out_body(1, true, true).contains("1 message hasn't been sent"));
        assert!(sign_out_body(3, true, true).contains("3 messages haven't been sent"));
        assert!(
            !sign_out_body(3, true, false).contains("deleted"),
            "kept messages aren't lost"
        );
        // Stores not known to be open: a 0 isn't "none".
        assert!(sign_out_body(0, false, true).contains("may be deleted"));
        assert!(!sign_out_body(0, false, false).contains("deleted"));
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

/// Show an error in the typing line for a few seconds. Typing and messages arriving meanwhile
/// leave it alone; when it ends, whoever is typing is drawn again.
fn show_send_error(chat: &Rc<Chat>, text: &str) {
    chat.typing_label.set_text(text);
    chat.typing_label.add_css_class("error");
    chat.typing_label.set_visible(true);
    let mut hold = chat.error_hold.get();
    hold.hold(Instant::now());
    chat.error_hold.set(hold);
    arm_error_timer(chat, ERROR_SHOWN);
}

/// End the hold after `delay`, or set the timer again for what's left if it fired early.
fn arm_error_timer(chat: &Rc<Chat>, delay: Duration) {
    let chat = chat.clone();
    glib::timeout_add_local_once(delay, move || {
        let mut hold = chat.error_hold.get();
        let fired = hold.timer_fired(Instant::now());
        chat.error_hold.set(hold);
        match fired {
            HoldTimer::Over => {
                chat.typing_label.remove_css_class("error");
                render_typing(&chat);
            }
            HoldTimer::Wait(left) => arm_error_timer(&chat, left + Duration::from_millis(20)),
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

/// A `message.update`: an edit of the text, or a file deleted from the message (its
/// `attachments` only ever shrink; an edit never touches them).
fn update_message(chat: &Rc<Chat>, widgets: &MessageWidgets, message: &Message) {
    if widgets.deleted.get() {
        return; // a tombstone stays one
    }
    widgets.body.set_markup(&markdown_to_pango(&message.body));
    widgets.source.replace(message.body.clone());
    widgets.edited.set_visible(message.edited_at.is_some());
    // Rows of files still attached stay as they are (a Save in progress continues).
    let wanted: Vec<&str> = message.attachments.iter().map(|f| f.id.as_str()).collect();
    widgets.file_rows.borrow_mut().retain(|(id, row)| {
        let keep = wanted.contains(&id.as_str());
        if !keep {
            widgets.files_box.remove(row);
        }
        keep
    });
    for file in &message.attachments {
        let shown = widgets
            .file_rows
            .borrow()
            .iter()
            .any(|(id, _)| *id == file.id);
        if !shown {
            let row =
                crate::attachments::attachment_row(file, chat.client.clone(), chat.runtime.clone());
            widgets.files_box.append(&row);
            widgets.file_rows.borrow_mut().push((file.id.clone(), row));
        }
    }
    widgets.has_files.set(!message.attachments.is_empty());
    // The text line follows the text; a message left with neither text nor files says so.
    if message.body.trim().is_empty() && message.attachments.is_empty() {
        widgets.body.set_markup("<i>Files removed</i>");
        widgets.body.add_css_class("dim-label");
        widgets.body.set_visible(true);
    } else {
        widgets.body.remove_css_class("dim-label");
        widgets.body.set_visible(!message.body.trim().is_empty());
    }
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
    // A tombstone mentions nobody (the server drops its mentions too).
    widgets.row.remove_css_class("mentions-me");
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

#[cfg(test)]
mod ownership_question_tests {
    use super::{offer_key, ownership_question};

    #[test]
    fn an_offer_asks_once_and_its_withdrawal_closes_the_question() {
        // Nothing on screen, an offer: ask.
        assert_eq!(ownership_question(None, None, Some("c|o|t")), (false, true));
        // Already asking about it: leave it be.
        assert_eq!(
            ownership_question(Some("c|o|t"), None, Some("c|o|t")),
            (false, false)
        );
        // Withdrawn (or answered elsewhere, or another channel opened): close, don't ask.
        assert_eq!(ownership_question(Some("c|o|t"), None, None), (true, false));
        // A new offer replaced it: close the old question, ask the new one.
        assert_eq!(
            ownership_question(Some("c|o|t"), None, Some("c|o|t2")),
            (true, true)
        );
        // No offer, nothing on screen: nothing.
        assert_eq!(ownership_question(None, None, None), (false, false));
    }

    #[test]
    fn a_failed_answer_put_off_isnt_asked_again_until_a_new_offer() {
        assert_eq!(
            ownership_question(None, Some("c|o|t"), Some("c|o|t")),
            (false, false)
        );
        // A newer offer on the same channel still asks.
        assert_eq!(
            ownership_question(None, Some("c|o|t"), Some("c|o|t2")),
            (false, true)
        );
    }

    #[test]
    fn the_key_is_the_channel_the_offerer_and_when() {
        let offer = brook_core::OwnerOffer {
            user_id: "me".into(),
            offered_by: "own".into(),
            created_at: "2026-09-26T10:00:00Z".into(),
        };
        assert_eq!(offer_key("c1", &offer), "c1|own|2026-09-26T10:00:00Z");
    }
}

#[cfg(test)]
mod mention_tests {
    use super::{badge_texts, mentions_me};
    use brook_core::Message;

    fn message(author: &str, mentions: &[&str], everyone: bool) -> Message {
        serde_json::from_value(serde_json::json!({
            "id": "m1", "channel_id": "c", "author_id": author, "body": "hi",
            "created_at": "2026-09-26T10:00:00Z",
            "mentions": mentions, "mention_everyone": everyone
        }))
        .unwrap()
    }

    #[test]
    fn a_message_mentions_me_by_name_or_everyone_but_never_my_own() {
        assert!(mentions_me(&message("bo", &["me"], false), "me"));
        assert!(mentions_me(&message("bo", &[], true), "me"));
        assert!(!mentions_me(&message("bo", &["someone"], false), "me"));
        assert!(!mentions_me(&message("me", &["me"], true), "me"));
        // Identity not known yet: nothing is claimed.
        assert!(!mentions_me(&message("bo", &[], true), ""));
    }

    #[test]
    fn a_deleted_message_mentions_nobody() {
        let mut m = message("bo", &["me"], true);
        m.deleted_at = Some("2026-09-26T10:01:00Z".into());
        assert!(!mentions_me(&m, "me"));
    }

    #[test]
    fn the_badges_say_how_many_mention_you_and_how_many_are_unread() {
        assert_eq!(badge_texts(0, 0), (String::new(), String::new()));
        assert_eq!(badge_texts(3, 0), (String::new(), "3".into()));
        assert_eq!(badge_texts(3, 1), ("@1".into(), "3".into()));
        // A count that lags the mentions never shows fewer.
        assert_eq!(badge_texts(0, 2), ("@2".into(), "2".into()));
    }
}

#[cfg(test)]
mod lost_device_help_tests {
    use super::lost_device_help;
    use crate::account::SIGN_OUT_OTHERS_LABEL;

    #[test]
    fn the_sign_out_help_names_the_ways_to_cut_off_a_lost_device() {
        let member = lost_device_help(false);
        assert!(member.contains("change your password"));
        assert!(
            member.contains(SIGN_OUT_OTHERS_LABEL),
            "the switch's own label"
        );
        assert!(member.contains("ask an admin to reset your account"));
    }

    #[test]
    fn the_help_doesnt_promise_protection_for_data_already_on_the_device() {
        for admin in [false, true] {
            let help = lost_device_help(admin);
            assert!(help.contains("ends its sign-in, but"), "{help}");
            assert!(help.contains("stay readable"), "{help}");
        }
    }

    #[test]
    fn an_admin_is_only_told_the_password_route() {
        // The server refuses an admin resetting another admin (403).
        let admin = lost_device_help(true);
        assert!(admin.contains(SIGN_OUT_OTHERS_LABEL));
        assert!(!admin.contains("admin"));
    }
}

#[cfg(test)]
mod manage_tests {
    use super::{management_shown, may_manage, ManageShown, QUICK_EMOJI};

    #[test]
    fn an_admin_or_the_channels_owner_manages_it() {
        assert!(may_manage(true, None));
        assert!(may_manage(true, Some("member")));
        assert!(may_manage(false, Some("owner")));
        assert!(!may_manage(false, Some("member")));
        // Roles unknown (an older server) and not an admin: nothing is offered.
        assert!(!may_manage(false, None));
    }

    #[test]
    fn a_dm_offers_nothing_and_archive_follows_the_state() {
        // A DM: not even an admin or an owner (the server answers 422).
        assert_eq!(
            management_shown(true, true, Some("owner"), false),
            ManageShown::default()
        );
        // A plain member of a channel.
        assert_eq!(
            management_shown(false, false, Some("member"), false),
            ManageShown::default()
        );
        // An owner of an open channel: Archive, not Unarchive.
        assert_eq!(
            management_shown(false, false, Some("owner"), false),
            ManageShown {
                whole: true,
                archive: true,
                unarchive: false
            }
        );
        // An admin of an archived channel: Unarchive, not Archive.
        assert_eq!(
            management_shown(false, true, None, true),
            ManageShown {
                whole: true,
                archive: false,
                unarchive: true
            }
        );
    }

    #[test]
    fn the_quick_heart_is_the_emoji_form_the_mac_and_kde_send() {
        // The server keys reactions on the exact string: a bare U+2764 would be another one.
        assert_eq!(QUICK_EMOJI[1], "\u{2764}\u{fe0f}");
    }
}

#[cfg(test)]
mod typing_tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn the_line_names_one_two_or_several_and_expires() {
        let t0 = Instant::now();
        let mut state = TypingState::default();
        assert_eq!(state.line(t0), None);
        state.note("a", "Ann", t0);
        assert_eq!(state.line(t0).as_deref(), Some("Ann is typing\u{2026}"));
        state.note("b", "Bob", t0 + S);
        assert_eq!(
            state.line(t0 + S).as_deref(),
            Some("Ann and Bob are typing\u{2026}")
        );
        state.note("c", "Cy", t0 + S);
        assert_eq!(
            state.line(t0 + S).as_deref(),
            Some("Several people are typing\u{2026}")
        );
        // Ann's last event was 4 s ago: only Bob and Cy are left.
        assert_eq!(
            state.line(t0 + 4 * S).as_deref(),
            Some("Bob and Cy are typing\u{2026}")
        );
        assert_eq!(state.next_expiry(t0 + 4 * S), Some(S));
        assert_eq!(state.line(t0 + 6 * S), None);
    }

    #[test]
    fn the_next_expiry_is_the_earliest_not_the_latest() {
        let t0 = Instant::now();
        let mut state = TypingState::default();
        state.note("a", "Ann", t0);
        state.note("b", "Bob", t0 + 3 * S);
        // Ann expires in 2 s, Bob in 4: the refresh must come for Ann first.
        assert_eq!(state.next_expiry(t0 + 2 * S), Some(2 * S));
    }

    #[test]
    fn a_new_event_renews_the_same_person() {
        let t0 = Instant::now();
        let mut state = TypingState::default();
        state.note("a", "Ann", t0);
        state.note("a", "Ann", t0 + 3 * S);
        assert_eq!(
            state.line(t0 + 6 * S).as_deref(),
            Some("Ann is typing\u{2026}")
        );
        assert_eq!(state.live(t0 + 6 * S).count(), 1);
    }

    #[test]
    fn their_message_clears_them_and_ignores_the_notice_just_before_it() {
        let t0 = Instant::now();
        let mut state = TypingState::default();
        state.note("a", "Ann", t0);
        state.clear("a", t0 + S);
        assert_eq!(state.line(t0 + S), None);
        // The typing request that raced their message: dropped.
        state.note("a", "Ann", t0 + S + Duration::from_millis(500));
        assert_eq!(state.line(t0 + 2 * S), None);
        // A real one for their next message, after the window: shown.
        state.note("a", "Ann", t0 + 4 * S);
        assert_eq!(
            state.line(t0 + 4 * S).as_deref(),
            Some("Ann is typing\u{2026}")
        );
    }

    #[test]
    fn switching_channels_forgets_everyone() {
        let t0 = Instant::now();
        let mut state = TypingState::default();
        state.note("a", "Ann", t0);
        state.clear("b", t0);
        state.reset();
        assert_eq!(state.line(t0), None);
        state.note("b", "Bob", t0);
        assert!(state.line(t0).is_some(), "no stale last-message window");
    }
}

#[cfg(test)]
mod conversation_error_tests {
    use super::*;

    fn api(code: &str) -> brook_core::Error {
        brook_core::Error::Api {
            code: code.into(),
            message: String::new(),
        }
    }

    #[test]
    fn a_handle_is_trimmed_and_loses_its_at_sign() {
        assert_eq!(clean_handle("  @ana "), "ana");
        assert_eq!(clean_handle("ana"), "ana");
        assert_eq!(clean_handle("@@ana"), "ana");
        assert_eq!(clean_handle(" @ "), "");
    }

    #[test]
    fn what_already_happened_isnt_reported() {
        assert!(already_gone(&api("not_found")));
        assert!(!already_gone(&api("authz.forbidden")));
        assert!(!already_gone(&brook_core::Error::UnexpectedResponse));
    }

    const ALL: [ConvAction; 7] = [
        ConvAction::StartDm,
        ConvAction::CreateChannel,
        ConvAction::Join,
        ConvAction::Browse,
        ConvAction::AddMember,
        ConvAction::Update,
        ConvAction::Delete,
    ];

    fn body(action: ConvAction, code: &str) -> String {
        action.failure(&api(code)).1
    }

    #[test]
    fn each_action_says_why_in_its_own_words() {
        for action in ALL {
            let (heading, forbidden, invalid, fallback) = action.texts();
            assert!(heading.starts_with("Couldn't"), "{action:?}");
            assert_eq!(
                action.failure(&api("authz.forbidden")),
                (heading, forbidden.to_string())
            );
            assert_eq!(
                action.failure(&api("validation.error")),
                (heading, invalid.to_string())
            );
            assert_eq!(
                action.failure(&api("whatever")),
                (heading, fallback.to_string())
            );
        }
        // The specific ones, so swapped columns can't pass.
        assert_eq!(
            body(ConvAction::StartDm, "validation.error"),
            "No one has that handle."
        );
        assert_eq!(
            body(ConvAction::AddMember, "validation.error"),
            "No one has that handle."
        );
        assert_eq!(
            body(ConvAction::CreateChannel, "validation.error"),
            "That name or topic can't be used."
        );
        assert_eq!(
            body(ConvAction::Update, "validation.error"),
            "That name or topic can't be used."
        );
        assert_eq!(
            body(ConvAction::CreateChannel, "authz.forbidden"),
            "Only admins can create channels."
        );
        assert_eq!(
            body(ConvAction::Join, "authz.forbidden"),
            "You can't join that channel."
        );
        assert_eq!(
            body(ConvAction::Browse, "authz.forbidden"),
            "You can't browse channels."
        );
        assert_eq!(
            body(ConvAction::Join, "validation.error"),
            "That didn't work. Try again."
        );
        assert_eq!(
            body(ConvAction::Delete, "authz.forbidden"),
            "Only an owner or admin can delete it."
        );
        assert_eq!(
            body(ConvAction::AddMember, "authz.forbidden"),
            "Only an owner or admin can add members."
        );
    }

    #[test]
    fn an_unanswered_call_says_so_but_an_odd_answer_is_a_retry() {
        for action in ALL {
            assert_eq!(
                action.failure(&brook_core::Error::Timeout).1,
                "Couldn't reach the server."
            );
            assert_eq!(
                action.failure(&brook_core::Error::NotAuthenticated).1,
                "You were signed out."
            );
            assert_eq!(
                action.failure(&brook_core::Error::UnexpectedResponse).1,
                "That didn't work. Try again."
            );
        }
    }
}

#[cfg(test)]
mod error_hold_tests {
    use super::*;

    const S: Duration = Duration::from_secs(1);

    #[test]
    fn typing_noted_during_an_error_doesnt_show_until_the_hold_ends() {
        let t0 = Instant::now();
        let mut hold = ErrorHold::default();
        let mut typing = TypingState::default();
        hold.hold(t0);
        typing.note("a", "Ann", t0 + S);
        assert_eq!(line_shown(&hold, &typing, t0 + 2 * S), LineShown::Error);
        // The hold ends at 6 s; Ann's notice (1 s) has expired at 5 s, so a fresh one shows.
        typing.note("a", "Ann", t0 + 6 * S);
        assert_eq!(hold.timer_fired(t0 + 6 * S), HoldTimer::Over);
        assert_eq!(
            line_shown(&hold, &typing, t0 + 6 * S),
            LineShown::Typing("Ann is typing\u{2026}".into())
        );
    }

    #[test]
    fn a_new_error_renews_the_hold_so_the_first_timer_does_nothing() {
        let t0 = Instant::now();
        let mut hold = ErrorHold::default();
        hold.hold(t0);
        hold.hold(t0 + 3 * S);
        // The first error's timer fires at 6 s: the second is still held until 9 s.
        assert_eq!(hold.timer_fired(t0 + 6 * S), HoldTimer::Wait(3 * S));
        assert!(hold.holding(t0 + 8 * S));
        assert_eq!(hold.timer_fired(t0 + 9 * S), HoldTimer::Over);
        assert!(!hold.holding(t0 + 9 * S));
    }

    #[test]
    fn switching_channel_forgets_typing_and_drops_the_error() {
        let t0 = Instant::now();
        let mut hold = ErrorHold::default();
        let mut typing = TypingState::default();
        typing.note("a", "Ann", t0);
        hold.hold(t0);
        switch_channel(&mut typing, &mut hold);
        assert_eq!(line_shown(&hold, &typing, t0 + S), LineShown::Nothing);
        assert!(!hold.holding(t0 + S));
    }

    #[test]
    fn opening_another_channel_drops_the_error() {
        let t0 = Instant::now();
        let mut hold = ErrorHold::default();
        hold.hold(t0);
        hold.clear();
        assert!(!hold.holding(t0 + S));
        assert_eq!(
            line_shown(&hold, &TypingState::default(), t0 + S),
            LineShown::Nothing
        );
        // The timer still pending from that error finds nothing held and just redraws.
        assert_eq!(hold.timer_fired(t0 + 6 * S), HoldTimer::Over);
    }

    #[test]
    fn a_timer_that_fires_early_is_set_again_for_what_is_left() {
        // GLib's cached loop time can run slightly ahead of our clock: the timer fires before
        // the deadline. It must not leave the error holding with nothing to end it.
        let t0 = Instant::now();
        let mut hold = ErrorHold::default();
        hold.hold(t0);
        let early = t0 + ERROR_SHOWN - Duration::from_millis(5);
        assert_eq!(
            hold.timer_fired(early),
            HoldTimer::Wait(Duration::from_millis(5))
        );
        assert!(hold.holding(early), "still held");
        assert_eq!(hold.timer_fired(t0 + ERROR_SHOWN), HoldTimer::Over);
    }
}

#[cfg(test)]
mod author_text_tests {
    use super::author_text;

    #[test]
    fn an_author_is_named_by_the_preference_with_fallbacks() {
        assert_eq!(author_text("Ann", "ann", false), "Ann");
        assert_eq!(author_text("Ann", "ann", true), "@ann");
        assert_eq!(author_text("", "ann", false), "@ann", "no name: the handle");
        assert_eq!(author_text("  ", "ann", false), "@ann");
        assert_eq!(author_text("", "", false), "Unknown");
        assert_eq!(
            author_text("Ann", "", true),
            "Ann",
            "no handle: the name stands"
        );
    }
}

#[cfg(test)]
mod reaction_order_tests {
    use super::{reactions_after, Fresh, ReactionOrder, ReactionSummary};

    const UP: &str = "\u{1f44d}";
    const PARTY: &str = "\u{1f389}";

    fn chip(emoji: &str, count: i64, me: bool) -> ReactionSummary {
        ReactionSummary {
            emoji: emoji.into(),
            count,
            me,
        }
    }

    /// Run an event through the order and the chips, as `apply_reaction` does.
    fn apply(
        order: &mut ReactionOrder,
        list: &mut Vec<ReactionSummary>,
        (emoji, count, added, mine, seq): (&str, i64, bool, bool, i64),
    ) {
        let fresh = order.judge("m1", emoji, seq, mine);
        reactions_after(list, emoji, count, added, mine, fresh);
    }

    #[test]
    fn an_older_count_is_dropped_and_a_newer_one_applies() {
        let (mut order, mut list) = (ReactionOrder::default(), vec![]);
        apply(&mut order, &mut list, (UP, 3, true, false, 10));
        apply(&mut order, &mut list, (UP, 2, true, false, 9));
        assert_eq!(
            list,
            [chip(UP, 3, false)],
            "an older count must not come back"
        );
        apply(&mut order, &mut list, (UP, 1, false, false, 11));
        assert_eq!(list, [chip(UP, 1, false)]);
        apply(&mut order, &mut list, (UP, 1, false, false, 11));
        assert_eq!(list, [chip(UP, 1, false)], "the same event twice");
    }

    #[test]
    fn my_older_event_still_sets_my_flag_after_a_newer_event_of_someone_else() {
        // I react at seq 10, someone else at 11, and 11 arrives first.
        let (mut order, mut list) = (ReactionOrder::default(), vec![]);
        apply(&mut order, &mut list, (UP, 2, true, false, 11));
        assert_eq!(list, [chip(UP, 2, false)]);
        apply(&mut order, &mut list, (UP, 1, true, true, 10));
        assert_eq!(list, [chip(UP, 2, true)], "the count stays, my flag is set");
    }

    #[test]
    fn my_older_event_never_undoes_a_newer_one_of_mine() {
        let (mut order, mut list) = (ReactionOrder::default(), vec![]);
        apply(&mut order, &mut list, (UP, 1, true, true, 12)); // I add
        apply(&mut order, &mut list, (UP, 0, false, true, 11)); // an older remove of mine, late
        assert_eq!(list, [chip(UP, 1, true)]);
    }

    #[test]
    fn my_newer_remove_clears_my_flag_even_when_the_count_is_already_newer() {
        // Mirror of the add case: someone else's seq 12 set the count, then my remove at 11
        // arrives late. The count stays, and my flag goes (it is the newest of mine).
        let (mut order, mut list) = (ReactionOrder::default(), vec![]);
        apply(&mut order, &mut list, (UP, 2, true, true, 9)); // I added
        apply(&mut order, &mut list, (UP, 3, true, false, 12)); // someone else, newer
        assert_eq!(list, [chip(UP, 3, true)]);
        apply(&mut order, &mut list, (UP, 2, false, true, 11)); // my remove, late
        assert_eq!(list, [chip(UP, 3, false)], "count kept, my flag cleared");
    }

    #[test]
    fn my_older_add_never_undoes_a_newer_remove_of_mine() {
        let (mut order, mut list) = (ReactionOrder::default(), vec![]);
        apply(&mut order, &mut list, (UP, 2, true, false, 5));
        apply(&mut order, &mut list, (UP, 1, false, true, 12)); // I removed
        apply(&mut order, &mut list, (UP, 2, true, true, 11)); // my older add, late
        assert_eq!(list, [chip(UP, 1, false)]);
    }

    #[test]
    fn a_stale_flag_event_for_a_chip_that_is_gone_creates_nothing() {
        let (mut order, mut list) = (ReactionOrder::default(), vec![]);
        apply(&mut order, &mut list, (UP, 0, false, false, 20)); // the count went to 0
        apply(&mut order, &mut list, (UP, 1, true, true, 10)); // my older add arrives late
        assert!(list.is_empty());
    }

    #[test]
    fn the_order_is_per_message_and_per_emoji_and_forgotten_on_clear() {
        let mut order = ReactionOrder::default();
        assert_eq!(
            order.judge("m1", UP, 10, false),
            Fresh {
                count: true,
                flag: false
            }
        );
        assert!(order.judge("m1", PARTY, 5, false).count, "another emoji");
        assert!(order.judge("m2", UP, 1, false).count, "another message");
        assert!(!order.judge("m1", UP, 9, false).count);
        order.clear();
        assert_eq!(
            order.judge("m1", UP, 1, true),
            Fresh {
                count: true,
                flag: true
            },
            "after a reconnect or a channel switch the numbering starts over"
        );
    }
}

#[cfg(test)]
mod offline_banner_tests {
    use super::{BannerAction, OfflineBanner};

    #[test]
    fn going_offline_starts_one_timer_and_a_repeat_while_waiting_starts_none() {
        let mut b = OfflineBanner::default();
        assert_eq!(b.on_state(true, false), BannerAction::Start(1));
        assert_eq!(
            b.on_state(true, false),
            BannerAction::Keep,
            "no second timer"
        );
        assert!(b.on_fire(1));
    }

    #[test]
    fn a_flaky_flip_restarts_the_wait_and_the_first_timer_shows_nothing() {
        let mut b = OfflineBanner::default();
        assert_eq!(b.on_state(true, false), BannerAction::Start(1));
        assert_eq!(b.on_state(false, false), BannerAction::Hide);
        assert_eq!(
            b.on_state(true, false),
            BannerAction::Start(2),
            "a fresh wait"
        );
        // The first timer still fires (3 s after the first true): it must not reveal.
        assert!(!b.on_fire(1), "stale timer");
        assert!(b.on_fire(2));
    }

    #[test]
    fn a_timer_firing_after_the_flag_went_false_shows_nothing() {
        let mut b = OfflineBanner::default();
        assert_eq!(b.on_state(true, false), BannerAction::Start(1));
        assert_eq!(b.on_state(false, false), BannerAction::Hide);
        assert!(!b.on_fire(1));
    }

    #[test]
    fn an_already_shown_banner_starts_no_timer_and_a_stopped_watch_reveals_nothing() {
        let mut b = OfflineBanner::default();
        assert_eq!(b.on_state(true, true), BannerAction::Keep);
        assert_eq!(
            b.on_state(false, true),
            BannerAction::Hide,
            "back online hides it"
        );
        assert_eq!(b.on_state(true, false), BannerAction::Start(1));
        b.stop();
        assert!(!b.on_fire(1));
    }
}
