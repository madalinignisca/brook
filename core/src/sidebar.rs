// SPDX-FileCopyrightText: 2026 Madalin Ignisca and Brook contributors
// SPDX-License-Identifier: AGPL-3.0-or-later

//! How a conversation is labelled and ordered in a client's sidebar. Pure rules, no I/O, so
//! every client lists conversations the same way; the state they need (the opened rank, the
//! preference) stays in each client.
//!
//! ```
//! use brook_core::{activity_moves, sidebar_order, SidebarEntry};
//!
//! let entry = |id: &str, kind: &str, last: Option<&str>| SidebarEntry {
//!     id: id.into(),
//!     kind: kind.into(),
//!     last_message_id: last.map(str::to_string),
//!     opened: None,
//!     sort_key: id.into(),
//! };
//! let order = sidebar_order(&[
//!     entry("dm", "dm", Some("9")),
//!     entry("old", "channel", Some("1")),
//!     entry("new", "channel", Some("2")),
//! ]);
//! assert_eq!(order, ["new", "old", "dm"]);
//! assert!(activity_moves(Some("1"), "2"));
//! ```

use std::cmp::Ordering;

use crate::chat::ChannelMember;

/// What a person is called: `@handle` when `show_usernames`, else the display name, with
/// `@handle` standing in when that is blank.
pub fn person_label(display_name: &str, handle: &str, show_usernames: bool) -> String {
    let name = display_name.trim();
    if show_usernames || name.is_empty() {
        format!("@{handle}")
    } else {
        name.to_string()
    }
}

/// What a conversation is called. Never empty, so the name key that orders it is always
/// defined.
pub fn conversation_label(
    kind: &str,
    name: Option<&str>,
    members: &[ChannelMember],
    me: &str,
    show_usernames: bool,
) -> String {
    if let Some(name) = name.filter(|n| !n.is_empty()) {
        return format!("#{name}");
    }
    let label = |m: &ChannelMember| person_label(&m.display_name, &m.handle, show_usernames);
    // An empty `me` picks nobody: the members are joined rather than guessing one.
    if kind == "dm" && !me.is_empty() {
        if let Some(other) = members.iter().find(|m| m.id != me) {
            return label(other);
        }
    }
    let all = members.iter().map(label).collect::<Vec<_>>();
    if all.is_empty() {
        "Direct message".to_string()
    } else {
        all.join(", ")
    }
}

/// The name key a `SidebarEntry` is ordered by: the lowercase `conversation_label` with
/// usernames off, whatever the preference, so toggling it never reorders the list.
pub fn sort_key(kind: &str, name: Option<&str>, members: &[ChannelMember], me: &str) -> String {
    conversation_label(kind, name, members, me, false).to_lowercase()
}

/// One conversation as the sidebar order sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SidebarEntry {
    pub id: String,
    /// `"dm"` or anything else (a channel).
    pub kind: String,
    /// The newest message id cached or seen live (UUIDv7, so it sorts by time).
    pub last_message_id: Option<String>,
    /// When this device last opened it: higher is later.
    pub opened: Option<i64>,
    /// The lowercase `conversation_label` with `show_usernames` off, so the preference can
    /// never change the order.
    pub sort_key: String,
}

/// Conversation ids in sidebar order: channels, then DMs; within each, the newest message
/// first, then the most recently opened, then by name key, then by id. Missing keys sort last.
pub fn sidebar_order(entries: &[SidebarEntry]) -> Vec<String> {
    // Descending with `None` last (a plain `.reverse()` would put it first).
    fn newest_first<T: Ord>(a: &Option<T>, b: &Option<T>) -> Ordering {
        match (a, b) {
            (Some(a), Some(b)) => b.cmp(a),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
    }
    let mut sorted: Vec<&SidebarEntry> = entries.iter().collect();
    sorted.sort_by(|a, b| {
        (a.kind == "dm")
            .cmp(&(b.kind == "dm"))
            .then_with(|| newest_first(&a.last_message_id, &b.last_message_id))
            .then_with(|| newest_first(&a.opened, &b.opened))
            .then_with(|| a.sort_key.cmp(&b.sort_key))
            .then_with(|| a.id.cmp(&b.id))
    });
    sorted.into_iter().map(|e| e.id.clone()).collect()
}

/// Whether a message re-sorts the list: strictly newer than the key it was last sorted by.
pub fn activity_moves(current: Option<&str>, message_id: &str) -> bool {
    current.is_none_or(|c| message_id > c)
}
