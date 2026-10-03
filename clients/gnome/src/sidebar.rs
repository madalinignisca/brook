//! The sidebar's state: what each conversation's last activity is, and when this device last
//! opened it. The rules (labels, the order, when a message re-sorts) are core's
//! (`brook_core::sidebar`, the same as the Mac's, see
//! `docs/superpowers/specs/2026-10-01-sidebar-order-spec.md`); this holds the per-device state
//! they need and applies them to GTK's channel list.
//!
//! Two keys per conversation: `activity` (the newest message id seen, from the cache or live) and
//! `sorted` (the value at the last re-sort). A live message is compared against `sorted`, not
//! `activity`, so a cache notice that already advanced `activity` can't swallow its later
//! `message.new`.

use std::collections::{HashMap, HashSet};

use brook_core::{
    activity_moves, conversation_label, sidebar_order, sort_key, Channel, SidebarEntry,
};

/// Per-conversation activity and open order, for this device.
pub struct SidebarState {
    activity: HashMap<String, String>,
    sorted: HashMap<String, String>,
    /// When this device last opened a conversation: higher is later. Saved per account.
    opened: HashMap<String, i64>,
    counter: i64,
    /// Conversations opened whose history back-fill hasn't been seen yet: opening one loads its
    /// old messages into the cache, which raises its key without being new activity, and that
    /// must not move its row once you've switched away. A channel leaves when a cache notice has
    /// moved its key (consumed, without re-sorting for it) or a live message for it arrives; a
    /// re-sort does not clear it, because the fetch can finish after any number of re-sorts.
    awaiting: HashSet<String>,
}

impl Default for SidebarState {
    fn default() -> Self {
        Self::new(HashMap::new())
    }
}

impl SidebarState {
    /// With the opened ranks saved for this account: the counter continues above them.
    pub fn new(saved: HashMap<String, i64>) -> Self {
        Self {
            activity: HashMap::new(),
            sorted: HashMap::new(),
            counter: saved.values().copied().max().unwrap_or(0) + 1,
            opened: saved,
            awaiting: HashSet::new(),
        }
    }

    /// Move a conversation's activity key forward (never back). Whether it moved.
    fn advance(&mut self, id: &str, message_id: &str) -> bool {
        if activity_moves(self.activity.get(id).map(String::as_str), message_id) {
            self.activity.insert(id.to_string(), message_id.to_string());
            true
        } else {
            false
        }
    }

    /// A list load: take the cache's last ids (a conversation with none learns nothing). The
    /// list is re-sorted by the caller afterwards.
    pub fn learn(&mut self, id: &str, last: Option<&str>) {
        if let Some(last) = last {
            self.advance(id, last);
        }
    }

    /// A live message: whether it re-sorts the list (it's newer than what the list was last
    /// sorted by). A message for a conversation that isn't listed yet is kept, and applies when
    /// its row arrives.
    pub fn live(&mut self, id: &str, message_id: &str) -> bool {
        self.advance(id, message_id);
        self.awaiting.remove(id);
        activity_moves(self.sorted.get(id).map(String::as_str), message_id)
    }

    /// A cache notice with each conversation's newest cached id: whether it re-sorts the list.
    /// Only a key moved forward for a conversation that is neither the open one nor one opened
    /// whose back-fill hasn't been seen counts; those are consumed instead.
    pub fn notice(&mut self, cached: &[(String, Option<String>)], open: Option<&str>) -> bool {
        let mut resort = false;
        for (id, last) in cached {
            let Some(last) = last else { continue };
            if !self.advance(id, last) {
                continue;
            }
            let back_fill = self.awaiting.remove(id);
            if open == Some(id.as_str()) || back_fill {
                continue;
            }
            resort = true;
        }
        resort
    }

    /// This device opened a conversation. A tie-break at the next sort: opening never re-sorts
    /// by itself.
    pub fn opened_now(&mut self, id: &str) {
        self.opened.insert(id.to_string(), self.counter);
        self.counter += 1;
        self.awaiting.insert(id.to_string());
    }

    /// The opened ranks, to save.
    pub fn opened_ranks(&self) -> &HashMap<String, i64> {
        &self.opened
    }

    /// Forget ranks of conversations that are gone: only from the network's list, and only when
    /// it isn't empty (an empty offline list must not erase them).
    pub fn prune(&mut self, listed: &[&str]) -> bool {
        if listed.is_empty() {
            return false;
        }
        let before = self.opened.len();
        self.opened.retain(|id, _| listed.contains(&id.as_str()));
        self.opened.len() != before
    }

    /// The conversation ids in sidebar order, which becomes what the list was sorted by. The
    /// name key is core's `sort_key` (usernames off), so "Show usernames" can never change it.
    pub fn order(&mut self, channels: &[Channel], me: &str) -> Vec<String> {
        let entries: Vec<SidebarEntry> = channels
            .iter()
            .map(|c| SidebarEntry {
                id: c.id.clone(),
                kind: c.kind.clone(),
                last_message_id: self.activity.get(&c.id).cloned(),
                opened: self.opened.get(&c.id).copied(),
                sort_key: sort_key(&c.kind, c.name.as_deref(), &c.members, me),
            })
            .collect();
        self.sorted = self.activity.clone();
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

/// The labels of `channels` in the order given (the preference toggling relabels the rows where
/// they are; it never sorts).
pub fn labels_in_order(channels: &[Channel], me: &str, show_usernames: bool) -> Vec<String> {
    channels
        .iter()
        .map(|c| label(c, me, show_usernames))
        .collect()
}

/// Whether the list must be redrawn after a sort: the rows are drawn from `chat.channels` by
/// position, so a changed order with no redraw would make a click open the wrong conversation,
/// and an unchanged one needs none (a redraw destroys every row, with its focus and any press
/// in progress).
pub fn order_changed(before: &[String], after: &[String]) -> bool {
    before != after
}

/// Where keyboard focus belongs after a redraw: the row of the conversation that had it (by id,
/// since its position may have moved), if it is still listed.
pub fn focus_target(focused_id: Option<&str>, channels: &[Channel]) -> Option<usize> {
    let id = focused_id?;
    channels.iter().position(|c| c.id == id)
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

    fn ids(state: &mut SidebarState) -> Vec<String> {
        state.order(&list(), "me")
    }

    fn cached(items: &[(&str, &str)]) -> Vec<(String, Option<String>)> {
        items
            .iter()
            .map(|(id, last)| (id.to_string(), Some(last.to_string())))
            .collect()
    }

    #[test]
    fn channels_come_first_each_section_by_last_activity_then_name() {
        let mut state = SidebarState::default();
        // Nothing seen yet: channels by name (case-insensitive), then people by name.
        assert_eq!(ids(&mut state), ["alpha", "zeta", "dm1", "dm2"]);
        state.learn("zeta", Some("0002"));
        state.learn("dm2", Some("0009"));
        state.learn("dm1", Some("0003"));
        state.learn("alpha", None);
        assert_eq!(ids(&mut state), ["zeta", "alpha", "dm2", "dm1"]);
    }

    #[test]
    fn a_live_message_re_sorts_once_and_an_older_or_repeated_one_doesnt() {
        let mut state = SidebarState::default();
        state.learn("alpha", Some("0005"));
        ids(&mut state);
        assert!(state.live("zeta", "0006"), "first activity moves it");
        ids(&mut state); // the caller re-sorts
        assert!(!state.live("zeta", "0006"), "the same message again");
        assert!(!state.live("zeta", "0004"), "an older one");
        assert!(state.live("zeta", "0007"));
        assert_eq!(ids(&mut state)[..2], ["zeta", "alpha"]);
        // The cache never lowers what a live message showed.
        state.learn("zeta", Some("0001"));
        assert_eq!(ids(&mut state)[..2], ["zeta", "alpha"]);
    }

    #[test]
    fn a_cache_notice_that_advanced_the_open_channel_doesnt_swallow_its_live_message() {
        // The open conversation's back-fill raised its key; its next live message still re-sorts.
        let mut state = SidebarState::default();
        ids(&mut state);
        state.opened_now("zeta");
        assert!(
            !state.notice(&cached(&[("zeta", "0006")]), Some("zeta")),
            "the open one"
        );
        // The same message, heard live after the notice already moved the key, is still news.
        assert!(
            state.live("zeta", "0006"),
            "compared with what was last sorted"
        );
    }

    #[test]
    fn a_back_fill_finishing_after_you_switch_away_doesnt_move_the_row() {
        // Open A, switch to B, then A's history notice arrives: A must not jump.
        let mut state = SidebarState::default();
        ids(&mut state);
        state.opened_now("alpha");
        state.opened_now("zeta"); // switched to B
        assert!(
            !state.notice(&cached(&[("alpha", "0005")]), Some("zeta")),
            "consumed"
        );
        // The next notice for A is real activity.
        assert!(state.notice(&cached(&[("alpha", "0006")]), Some("zeta")));
    }

    #[test]
    fn a_re_sort_doesnt_clear_the_awaiting_back_fill() {
        let mut state = SidebarState::default();
        state.opened_now("alpha");
        state.opened_now("zeta");
        ids(&mut state); // a re-sort for some other reason
        assert!(
            !state.notice(&cached(&[("alpha", "0005")]), Some("zeta")),
            "still awaited"
        );
    }

    #[test]
    fn a_live_message_ends_the_wait_so_a_later_notice_is_real_news() {
        let mut state = SidebarState::default();
        state.opened_now("alpha");
        state.opened_now("zeta");
        assert!(state.live("alpha", "0005"));
        ids(&mut state);
        assert!(state.notice(&cached(&[("alpha", "0006")]), Some("zeta")));
    }

    #[test]
    fn a_notice_for_a_channel_not_awaited_re_sorts_and_an_unmoved_key_doesnt() {
        let mut state = SidebarState::default();
        state.learn("alpha", Some("0005"));
        assert!(
            !state.notice(&cached(&[("alpha", "0005")]), None),
            "not moved"
        );
        assert!(state.notice(&cached(&[("alpha", "0006")]), None));
        assert!(
            !state.notice(&[("alpha".into(), None)], None),
            "nothing cached says nothing"
        );
    }

    #[test]
    fn opening_breaks_a_tie_but_doesnt_re_sort_by_itself() {
        let mut state = SidebarState::default();
        state.learn("alpha", Some("0005"));
        state.learn("zeta", Some("0005"));
        assert_eq!(ids(&mut state)[..2], ["alpha", "zeta"], "by name");
        state.opened_now("zeta");
        // Nothing re-sorted until asked; then the rank applies.
        assert_eq!(
            ids(&mut state)[..2],
            ["zeta", "alpha"],
            "opened later first"
        );
    }

    #[test]
    fn saved_ranks_continue_above_and_apply_after_a_restart() {
        let saved = HashMap::from([("alpha".to_string(), 7), ("zeta".to_string(), 3)]);
        let mut state = SidebarState::new(saved);
        state.learn("alpha", Some("0005"));
        state.learn("zeta", Some("0005"));
        assert_eq!(
            ids(&mut state)[..2],
            ["alpha", "zeta"],
            "alpha was opened later"
        );
        state.opened_now("zeta");
        assert_eq!(
            state.opened_ranks()["zeta"],
            8,
            "the counter starts at max(saved) + 1"
        );
        assert_eq!(ids(&mut state)[..2], ["zeta", "alpha"]);
        assert_eq!(SidebarState::new(HashMap::new()).counter, 1);
    }

    #[test]
    fn pruning_forgets_gone_conversations_but_never_from_an_empty_list() {
        let saved = HashMap::from([("alpha".to_string(), 2), ("gone".to_string(), 1)]);
        let mut state = SidebarState::new(saved);
        assert!(!state.prune(&[]), "an empty (offline) list erases nothing");
        assert_eq!(state.opened_ranks().len(), 2);
        assert!(state.prune(&["alpha", "zeta"]));
        assert_eq!(state.opened_ranks().len(), 1);
        assert!(state.opened_ranks().contains_key("alpha"));
    }

    #[test]
    fn a_message_for_an_unlisted_channel_is_kept_until_its_row_arrives() {
        let mut state = SidebarState::default();
        state.learn("alpha", Some("0002"));
        assert!(state.live("ghost", "0009"));
        let order = state.order(&list(), "me");
        assert_eq!(order[..2], ["alpha", "zeta"], "ghost isn't listed yet");
        let mut with_ghost = list();
        with_ghost.push(channel("ghost", "channel", Some("Ghost"), &[]));
        assert_eq!(
            state.order(&with_ghost, "me")[0],
            "ghost",
            "its key applies now"
        );
    }

    fn people() -> Vec<Channel> {
        // Display-name order (Amy, Zed) differs from handle order (aaa=Zed, zzz=Amy).
        vec![
            channel("dz", "dm", None, &[("me", "me", "Me"), ("z", "aaa", "Zed")]),
            channel("da", "dm", None, &[("me", "me", "Me"), ("a", "zzz", "Amy")]),
        ]
    }

    #[test]
    fn show_usernames_relabels_where_the_rows_are_and_never_sorts() {
        let rows = people();
        assert_eq!(
            labels_in_order(&rows, "me", false),
            ["Zed", "Amy"],
            "the rows' own order"
        );
        assert_eq!(labels_in_order(&rows, "me", true), ["@aaa", "@zzz"]);
        // The order itself is by name whatever the preference: Amy, then Zed.
        let mut state = SidebarState::default();
        assert_eq!(state.order(&rows, "me"), ["da", "dz"]);
        // Handle order would have put Zed ("aaa") first.
        let by_handle: Vec<_> = {
            let mut v = labels_in_order(&rows, "me", true);
            v.sort();
            v
        };
        assert_eq!(by_handle, ["@aaa", "@zzz"]);
    }

    #[test]
    fn labels_follow_the_preference_and_fall_back_to_the_handle_for_a_blank_name() {
        let list = list();
        assert_eq!(label(&list[1], "me", false), "#Zeta");
        assert_eq!(label(&list[0], "me", false), "Ann");
        assert_eq!(label(&list[0], "me", true), "@ann");
        let blank = channel("d", "dm", None, &[("me", "me", "Me"), ("b", "bob", "  ")]);
        assert_eq!(label(&blank, "me", false), "@bob");
    }

    #[test]
    fn a_changed_order_needs_a_redraw_and_an_unchanged_one_doesnt() {
        let a = ["x".to_string(), "y".to_string()];
        let b = ["y".to_string(), "x".to_string()];
        assert!(
            order_changed(&a, &b),
            "rows are drawn by position: they must follow"
        );
        assert!(
            !order_changed(&a, &a),
            "nothing moved: leave the rows (and their focus) alone"
        );
        assert!(order_changed(&a, &a[..1]));
    }

    #[test]
    fn focus_follows_the_conversation_to_its_new_row() {
        let rows = list();
        assert_eq!(focus_target(Some("alpha"), &rows), Some(2));
        let moved = arranged(list(), &["alpha".into(), "zeta".into()]);
        assert_eq!(
            focus_target(Some("alpha"), &moved),
            Some(0),
            "by id, not position"
        );
        assert_eq!(focus_target(Some("gone"), &rows), None);
        assert_eq!(focus_target(None, &rows), None, "focus wasn't in the list");
    }

    #[test]
    fn arranged_follows_the_order_and_leaves_unnamed_ones_last() {
        let sorted = arranged(list(), &["dm2".into(), "alpha".into()]);
        let ids: Vec<&str> = sorted.iter().map(|c| c.id.as_str()).collect();
        assert_eq!(ids[..2], ["dm2", "alpha"]);
        assert_eq!(ids.len(), 4);
    }
}
