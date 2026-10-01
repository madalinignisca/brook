//! The sidebar's state: what each conversation's last activity is, and when this device last
//! opened it. The rules (labels, the order, when a message re-sorts) are core's
//! (`brook_core::sidebar`); this holds the per-device state they need and applies them to GTK's
//! channel list, so every client orders conversations the same way.

use std::collections::HashMap;

use brook_core::{activity_moves, conversation_label, sidebar_order, Channel, SidebarEntry};

/// Per-conversation activity and open order, for this device and this run.
#[derive(Default)]
pub struct SidebarState {
    /// The newest message id seen per conversation (cached, or live since).
    last: HashMap<String, String>,
    /// When this device last opened a conversation: higher is later.
    opened: HashMap<String, i64>,
    counter: i64,
}

impl SidebarState {
    /// Take a conversation's last message id from the cache, keeping the newer of it and what
    /// a live message already showed.
    /// Whether that changed what the list is sorted by.
    pub fn seed(&mut self, id: &str, last: Option<&str>) -> bool {
        match last {
            Some(last) => self.bump(id, last),
            None => false,
        }
    }

    /// A message arrived: whether it re-sorts the list (it's newer than what the list was
    /// last sorted by).
    pub fn bump(&mut self, id: &str, message_id: &str) -> bool {
        if activity_moves(self.last.get(id).map(String::as_str), message_id) {
            self.last.insert(id.to_string(), message_id.to_string());
            true
        } else {
            false
        }
    }

    /// This device opened a conversation (a tie-break at the next sort: opening never
    /// re-sorts by itself).
    pub fn opened_now(&mut self, id: &str) {
        self.counter += 1;
        self.opened.insert(id.to_string(), self.counter);
    }

    /// The conversation ids in sidebar order. The name key is the label with usernames off,
    /// so the "Show usernames" preference can never change the order.
    pub fn order(&self, channels: &[Channel], me: &str) -> Vec<String> {
        let entries: Vec<SidebarEntry> = channels
            .iter()
            .map(|c| SidebarEntry {
                id: c.id.clone(),
                kind: c.kind.clone(),
                last_message_id: self.last.get(&c.id).cloned(),
                opened: self.opened.get(&c.id).copied(),
                sort_key: label(c, me, false).to_lowercase(),
            })
            .collect();
        sidebar_order(&entries)
    }
}

/// What a conversation is called in the sidebar: `#name`, or the person (display name, or
/// `@handle` with "Show usernames").
pub fn label(channel: &Channel, me: &str, show_usernames: bool) -> String {
    conversation_label(
        &channel.kind,
        channel.name.as_deref(),
        &channel.members,
        me,
        show_usernames,
    )
}

/// `channels` reordered as `order` says (an id it doesn't name keeps its relative place at
/// the end).
pub fn arranged(mut channels: Vec<Channel>, order: &[String]) -> Vec<Channel> {
    let rank: HashMap<&str, usize> = order
        .iter()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    channels.sort_by_key(|c| rank.get(c.id.as_str()).copied().unwrap_or(usize::MAX));
    channels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn channel(
        id: &str,
        kind: &str,
        name: Option<&str>,
        members: &[(&str, &str, &str)],
    ) -> Channel {
        let members: Vec<serde_json::Value> = members
            .iter()
            .map(|(id, handle, name)| {
                serde_json::json!({"id": id, "handle": handle, "display_name": name})
            })
            .collect();
        serde_json::from_value(serde_json::json!({
            "id": id, "kind": kind, "name": name, "topic": null,
            "created_by": "u", "created_at": "2026-10-01T00:00:00Z", "members": members
        }))
        .unwrap()
    }

    fn list() -> Vec<Channel> {
        vec![
            channel(
                "dm1",
                "dm",
                None,
                &[("me", "me", "Me"), ("a", "ann", "Ann")],
            ),
            channel("zeta", "channel", Some("Zeta"), &[]),
            channel("alpha", "channel", Some("alpha"), &[]),
            channel(
                "dm2",
                "dm",
                None,
                &[("me", "me", "Me"), ("b", "bob", "Bob")],
            ),
        ]
    }

    #[test]
    fn channels_come_first_each_section_by_last_activity_then_name() {
        let mut state = SidebarState::default();
        let order = |s: &SidebarState| s.order(&list(), "me");
        // Nothing seen yet: channels by name (case-insensitive), then people by name.
        assert_eq!(order(&state), ["alpha", "zeta", "dm1", "dm2"]);
        state.seed("zeta", Some("0002"));
        state.seed("dm2", Some("0009"));
        state.seed("dm1", Some("0003"));
        assert_eq!(order(&state), ["zeta", "alpha", "dm2", "dm1"]);
    }

    #[test]
    fn a_newer_message_moves_a_conversation_and_an_older_one_doesnt() {
        let mut state = SidebarState::default();
        state.seed("alpha", Some("0005"));
        assert!(state.bump("zeta", "0006"), "first activity moves it");
        assert!(!state.bump("zeta", "0006"), "the same message again");
        assert!(!state.bump("zeta", "0004"), "an older one");
        assert!(!state.seed("zeta", None), "nothing cached says nothing");
        assert!(
            state.seed("dm1", Some("0002")),
            "a cached id is new activity"
        );
        assert!(!state.seed("dm1", Some("0002")), "but only once");
        assert!(state.bump("zeta", "0007"));
        assert_eq!(state.order(&list(), "me")[..2], ["zeta", "alpha"]);
        // The cache never lowers what a live message showed.
        state.seed("zeta", Some("0001"));
        assert_eq!(state.order(&list(), "me")[..2], ["zeta", "alpha"]);
    }

    #[test]
    fn opening_breaks_a_tie_but_doesnt_reorder_by_itself() {
        let mut state = SidebarState::default();
        state.seed("alpha", Some("0005"));
        state.seed("zeta", Some("0005"));
        assert_eq!(
            state.order(&list(), "me")[..2],
            ["alpha", "zeta"],
            "by name"
        );
        state.opened_now("zeta");
        assert_eq!(
            state.order(&list(), "me")[..2],
            ["zeta", "alpha"],
            "opened later first"
        );
    }

    #[test]
    fn the_username_preference_changes_labels_but_never_the_order() {
        let list = list();
        assert_eq!(label(&list[1], "me", false), "#Zeta");
        assert_eq!(label(&list[0], "me", false), "Ann");
        assert_eq!(label(&list[0], "me", true), "@ann");
        let state = SidebarState::default();
        // The same ids either way: the order is computed with the preference off.
        assert_eq!(state.order(&list, "me"), ["alpha", "zeta", "dm1", "dm2"]);
    }

    #[test]
    fn arranged_follows_the_order_and_leaves_unnamed_ones_last() {
        let sorted = arranged(list(), &["dm2".into(), "alpha".into()]);
        let ids: Vec<&str> = sorted.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids[..2], ["dm2", "alpha"]);
        assert_eq!(ids.len(), 4);
    }
}
